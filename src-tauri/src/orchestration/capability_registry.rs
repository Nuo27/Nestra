//! Capability registry — the queryable `(endpoint, model) → abilities` index
//! the router consults.
//!
//! Distinct from the user-edited `model_abilities` on `provider_endpoint`
//! (which is the *input*): this module builds the *resolved* `model_catalog`
//! by layering, per the existing `model_abilities` contract
//! (`model_abilities.rs:13`, `:429-431`):
//!
//! ```text
//! models.dev cache  <  bundled corrections  <  per-endpoint user overrides
//! ```
//!
//! The output is one `model_catalog` row per `(endpoint_id, model_id)` with
//! the merged [`ModelAbilities`] serialized to JSON. The router and the
//! `/orchestration` Model-catalog card both read it.
//!
//! Build is idempotent and cheap enough to run on demand (a `rebuild` command
//! + lazily on first catalog read after endpoint edits). It never mutates the
//! input `provider_endpoint` rows.

use std::collections::HashMap;

use rusqlite::Connection;

use crate::config_writer::{ModelsConfig, ProviderKind};
use crate::db;
use crate::error::{AppError, AppResult};
use crate::model_abilities::{self, ModelAbilities};

use super::identity::CapabilityReq;
use super::store::{self, ModelCatalogRow};

/// Rebuild the entire `model_catalog` from the live `provider_endpoint` rows
/// + the models.dev ability cache. Wipes and repopulates `model_catalog`
/// atomically (delete per-endpoint, re-insert). Safe to call repeatedly.
pub fn rebuild(conn: &Connection) -> AppResult<usize> {
    // Build the merged ability index once (models.dev cache + corrections).
    // Per-endpoint user overrides are layered per endpoint below.
    let base_index = merged_index(conn)?;

    let endpoints = db::list_endpoints(conn)?;
    let mut total = 0usize;
    for ep in &endpoints {
        total += rebuild_one(conn, &base_index, ep)?;
    }
    Ok(total)
}

/// Rebuild the catalog rows for ONE endpoint — called after a models-save so
/// a default-model change is visible to the next gateway request without a
/// full rebuild. `0` when the endpoint is gone or has no models.
pub fn rebuild_endpoint(conn: &Connection, endpoint_id: &str) -> AppResult<usize> {
    let base_index = merged_index(conn)?;
    let Some(ep) = db::get_endpoint(conn, endpoint_id)? else {
        return Ok(0);
    };
    rebuild_one(conn, &base_index, &ep)
}

/// models.dev cache < bundled corrections. Corrections carry the per-model
/// `api` dialect (which wire a model is officially served on) and context
/// fixes — without this merge the catalog would lack `api` entirely and
/// wire selection would silently fall back to the endpoint protocol.
fn merged_index(conn: &Connection) -> AppResult<HashMap<String, crate::model_abilities::ModelAbilities>> {
    Ok(crate::model_abilities::merge_into(
        crate::model_abilities::load_index(conn)?,
        crate::model_abilities::load_corrections(),
    ))
}

fn rebuild_one(
    conn: &Connection,
    base_index: &std::collections::HashMap<String, ModelAbilities>,
    ep: &db::EndpointRow,
) -> AppResult<usize> {
    // Parse BEFORE deleting: an unparseable `models_json` must error out, not
    // wipe the endpoint's catalog rows. (The old order deleted-then-parsed,
    // so a parse failure committed an EMPTY catalog — routing silently lost
    // every model for that endpoint.)
    if let Some(json) = ep.models_json.as_deref() {
        if serde_json::from_str::<serde_json::Value>(json).is_err() {
            return Err(AppError::Internal(format!(
                "endpoint '{}': models_json is not valid JSON",
                ep.id
            )));
        }
    }
    let model_ids = model_ids_from(kind_from_protocols(&ep.protocols), ep.models_json.as_deref());
    if model_ids.is_empty() {
        // A genuinely empty model list still clears stale rows — but that
        // only happens on a *parsable* config with no models, which is a
        // user-visible state, not a parse failure.
        let tx = conn.unchecked_transaction()?;
        store::delete_model_catalog_for_endpoint(&tx, &ep.id)?;
        tx.commit()?;
        return Ok(0);
    }

    // Wipe this endpoint's stale catalog rows, then re-insert fresh ones —
    // all in ONE transaction. Previously each delete + upsert was its own
    // auto-commit (N+1 fsyncs under synchronous=FULL); now it's a single
    // commit regardless of model count.
    let tx = conn.unchecked_transaction()?;
    store::delete_model_catalog_for_endpoint(&tx, &ep.id)?;

    // Per-endpoint user overrides (highest-priority layer).
    let user_overrides = model_abilities::parse_overrides(ep.model_abilities_json.as_deref());

    let mut total = 0usize;
    for mid in &model_ids {
        let merged = resolve_abilities(base_index, &user_overrides, mid);
        let abilities_json = serde_json::to_string(&merged)?;
        store::upsert_model_catalog(
            &tx,
            &ModelCatalogRow {
                endpoint_id: ep.id.clone(),
                model_id: mid.clone(),
                abilities_json,
            },
        )?;
        total += 1;
    }
    tx.commit()?;
    Ok(total)
}

/// Resolve the merged [`ModelAbilities`] for one model id on one endpoint.
///
/// Layering (low → high): models.dev cache + corrections → per-endpoint user
/// override. Matches the contract documented at `model_abilities.rs:429-431`
/// and the merge used by the OpenCode config writer
/// (`commands.rs::build_switch_context`).
fn resolve_abilities(
    base_index: &HashMap<String, ModelAbilities>,
    user_overrides: &HashMap<String, ModelAbilities>,
    model_id: &str,
) -> ModelAbilities {
    let mut merged = model_abilities::abilities_for(base_index, model_id).unwrap_or_default();
    if let Some(ovr) = user_overrides.get(model_id) {
        // User override wins field-by-field (only set fields override).
        merged = model_abilities::merge_field_overrides(merged, ovr.clone());
    }
    merged
}

/// Extract the distinct model id list an endpoint exposes, mirroring
/// `commands::parse_models` + `ModelsConfig::ids()` without taking a
/// dependency on the commands module. Returns the deduped, order-preserving
/// list of non-empty ids.
fn model_ids_from(kind: ProviderKind, models_json: Option<&str>) -> Vec<String> {
    let Some(json) = models_json else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let default_str = v
        .get("default")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let available: Vec<String> = v
        .get("available")
        .and_then(|a| a.as_array())
        .map(|arr| arr.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let cfg = match kind {
        ProviderKind::Anthropic => {
            let pick = |tier: &str| -> String {
                v.get(tier)
                    .and_then(|s| s.as_str())
                    .map(String::from)
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| default_str.clone())
            };
            ModelsConfig::Anthropic {
                default: default_str.clone(),
                haiku: pick("haiku"),
                sonnet: pick("sonnet"),
                opus: pick("opus"),
            }
        }
        _ => ModelsConfig::Openai {
            default: default_str,
            available,
        },
    };
    let mut ids = cfg.ids();
    // Real Anthropic-shape providers (and dual-protocol gateways like
    // opencode-go, whose anthropic row carries an `available` list) serve
    // more than the tier slots. Index the full `available` set too, or the
    // catalog only knows default+tiers and capability routing can never
    // pick the rest. Empty strings are dropped (a garbage `""` id would
    // otherwise land as a `model_id=""` catalog row).
    if let Some(arr) = v.get("available").and_then(|a| a.as_array()) {
        for m in arr.iter().filter_map(|x| x.as_str()).filter(|s| !s.is_empty()) {
            if !ids.iter().any(|i| i == m) {
                ids.push(m.to_string());
            }
        }
    }
    ids.retain(|i| !i.is_empty());
    ids
}

/// Infer the endpoint's primary protocol from its `endpoint_protocol` rows.
/// The catalog indexes models under the endpoint's *native* protocol; an
/// endpoint with an `anthropic` protocol row is Anthropic-tiered, everything
/// else is free-form OpenAI-shape. Falls back to `Custom` when no rows exist
/// (which yields a free-form list — the safe default).
fn kind_from_protocols(protocols: &[db::ProtocolEntry]) -> ProviderKind {
    for p in protocols {
        match p.protocol.as_str() {
            "anthropic" => return ProviderKind::Anthropic,
            "openai-comp" => return ProviderKind::Openai,
            "response-api" => return ProviderKind::Responses,
            "custom" => return ProviderKind::Custom,
            _ => {}
        }
    }
    ProviderKind::Custom
}

// ---- capability matching (consumed by the router) ------------------------

/// `true` when `abilities` satisfies `req`. A `None` ability is treated as
/// "unknown, don't filter on it" — the router prefers to over-include rather
/// than reject a model for a capability we have no data on. `context_floor`
/// is satisfied when the model's reported context window is `>= floor` (or
/// unreported, which we treat as eligible to avoid over-filtering).
pub fn satisfies(req: &CapabilityReq, abilities: &ModelAbilities) -> bool {
    if req.reasoning && !abilities.reasoning.unwrap_or(false) {
        // reasoning required but model reports false → reject. Unknown (None)
        // is treated as "might support" → eligible.
        if abilities.reasoning == Some(false) {
            return false;
        }
    }
    if req.tool_call && abilities.tool_call == Some(false) {
        return false;
    }
    if req.vision {
        // Vision = image input modality. Reject only when modalities are
        // reported AND exclude image. Unreported → eligible.
        if let Some(mods) = &abilities.modalities {
            if !mods.input.iter().any(|m| {
                matches!(m, model_abilities::Modality::Image)
            }) {
                return false;
            }
        }
    }
    if let Some(floor) = req.context_floor {
        if let Some(limit) = &abilities.limit {
            if limit.context < floor {
                return false;
            }
        }
        // No limit reported → treat as eligible (don't filter on unknown).
    }
    true
}

/// All `(endpoint_id, model_id)` pairs whose merged abilities satisfy `req`.
/// Built from the `model_catalog` table (so call [`rebuild`] first if
/// endpoints changed). Returns endpoint-then-model id pairs the router ranks.
pub fn eligible_models(
    conn: &Connection,
    req: &CapabilityReq,
) -> AppResult<Vec<(String, String, ModelAbilities)>> {
    let endpoints = db::list_endpoints(conn)?;
    let mut out = Vec::new();
    for ep in &endpoints {
        for row in store::list_model_catalog(conn, &ep.id)? {
            let abilities: ModelAbilities =
                serde_json::from_str(&row.abilities_json).unwrap_or_default();
            if satisfies(req, &abilities) {
                out.push((ep.id.clone(), row.model_id, abilities));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_writer::ProviderKind;
    use crate::model_abilities::{ModelAbilities, ModelLimit, Modality, Modalities};
    use crate::schema;

    fn abilities_with(context: u64) -> ModelAbilities {
        ModelAbilities {
            reasoning: Some(true),
            tool_call: Some(true),
            attachment: None,
            temperature: None,
            limit: Some(ModelLimit {
                context,
                output: 8192,
                input: None,
            }),
            modalities: None,
            api: None,
        }
    }

    #[test]
    fn satisfies_filters_on_explicit_false_only() {
        let a = abilities_with(200_000);
        // Required reasoning + tool_call, model has both → ok.
        assert!(satisfies(
            &CapabilityReq {
                reasoning: true,
                tool_call: true,
                vision: false,
                context_floor: Some(100_000),
            },
            &a
        ));
        // Context floor too high → reject.
        assert!(!satisfies(
            &CapabilityReq {
                reasoning: true,
                tool_call: true,
                vision: false,
                context_floor: Some(500_000),
            },
            &a
        ));
        // Reasoning required, model reports false → reject.
        let no_reason = ModelAbilities {
            reasoning: Some(false),
            ..a.clone()
        };
        assert!(!satisfies(
            &CapabilityReq {
                reasoning: true,
                tool_call: false,
                vision: false,
                context_floor: None,
            },
            &no_reason
        ));
    }

    #[test]
    fn unknown_capability_does_not_filter() {
        // A model with NO ability data (all None) must be eligible for any
        // request — we over-include rather than reject on missing data.
        let unknown = ModelAbilities {
            reasoning: None,
            tool_call: None,
            attachment: None,
            temperature: None,
            limit: None,
            modalities: None,
            api: None,
        };
        assert!(satisfies(
            &CapabilityReq {
                reasoning: true,
                tool_call: true,
                vision: true,
                context_floor: Some(1_000_000),
            },
            &unknown
        ));
    }

    #[test]
    fn vision_checks_image_modality() {
        let with_image = ModelAbilities {
            modalities: Some(Modalities {
                input: vec![Modality::Text, Modality::Image],
                output: vec![Modality::Text],
            }),
            ..abilities_with(200_000)
        };
        let text_only = ModelAbilities {
            modalities: Some(Modalities {
                input: vec![Modality::Text],
                output: vec![Modality::Text],
            }),
            ..abilities_with(200_000)
        };
        let vision_req = CapabilityReq {
            reasoning: false,
            tool_call: false,
            vision: true,
            context_floor: None,
        };
        assert!(satisfies(&vision_req, &with_image));
        assert!(!satisfies(&vision_req, &text_only));
    }

    #[test]
    fn model_ids_from_anthropic_dedupes_tiers() {
        let json = r#"{"default":"sonnet","haiku":"haiku","sonnet":"sonnet","opus":"opus","available":[]}"#;
        let ids = model_ids_from(ProviderKind::Anthropic, Some(json));
        // ModelsConfig::Anthropic ids() dedupes; order is default first.
        assert!(ids.contains(&"sonnet".to_string()));
        assert!(ids.contains(&"haiku".to_string()));
        assert!(ids.contains(&"opus".to_string()));
        // No duplicates.
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len());
    }

    #[test]
    fn model_ids_from_anthropic_unions_available() {
        // opencode-go's anthropic row: tiers empty (fall back to default),
        // full list lives in `available` — the catalog must index all of
        // them, not just default+tiers (regression: catalog held 1 of 25).
        let json = r#"{"default":"deepseek-v4-flash","haiku":"","opus":"","sonnet":"",
            "available":["deepseek-v4-flash","grok-4.5","kimi-k3","qwen3.8-max"]}"#;
        let ids = model_ids_from(ProviderKind::Anthropic, Some(json));
        assert_eq!(ids.len(), 4);
        for m in ["deepseek-v4-flash", "grok-4.5", "kimi-k3", "qwen3.8-max"] {
            assert!(ids.contains(&m.to_string()), "missing {m}");
        }
        // Tier models already covered by available are not duplicated.
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len());
    }

    #[test]
    fn model_ids_from_openai_uses_available_plus_default() {
        let json = r#"{"default":"gpt-4o","available":["gpt-4o","gpt-4o-mini"]}"#;
        let ids = model_ids_from(ProviderKind::Openai, Some(json));
        assert_eq!(ids, vec!["gpt-4o".to_string(), "gpt-4o-mini".to_string()]);
    }

    #[test]
    fn rebuild_populates_catalog_from_endpoints() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        schema::build_v1(&conn).unwrap();
        // Seed an endpoint with a model + protocol.
        conn.execute(
            "INSERT INTO provider_endpoint (id, kind, display_name, has_api_key, status, models_json)
             VALUES ('ep-1','custom','Main',0,'unvalidated',
                     '{\"default\":\"m-1\",\"available\":[\"m-1\",\"m-2\"]}')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO endpoint_protocol (endpoint_id, protocol, base_url)
             VALUES ('ep-1','openai-comp','https://x')",
            [],
        )
        .unwrap();

        let n = rebuild(&conn).unwrap();
        assert_eq!(n, 2, "two distinct models should be cataloged");

        let rows = store::list_model_catalog(&conn, "ep-1").unwrap();
        let ids: Vec<_> = rows.iter().map(|r| r.model_id.as_str()).collect();
        assert!(ids.contains(&"m-1"));
        assert!(ids.contains(&"m-2"));
        // abilities_json is valid ModelAbilities JSON (defaulted, all-None
        // since no models.dev match for fake ids).
        for r in &rows {
            let parsed: ModelAbilities = serde_json::from_str(&r.abilities_json).unwrap();
            assert_eq!(parsed, ModelAbilities::default());
        }
    }

    #[test]
    fn rebuild_respects_user_overrides() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        schema::build_v1(&conn).unwrap();
        conn.execute(
            "INSERT INTO provider_endpoint (id, kind, display_name, has_api_key, status,
                                            models_json, model_abilities_json)
             VALUES ('ep-1','custom','Main',0,'unvalidated',
                     '{\"default\":\"m-1\"}',
                     '{\"m-1\":{\"reasoning\":true}}')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO endpoint_protocol (endpoint_id, protocol, base_url)
             VALUES ('ep-1','openai-comp','https://x')",
            [],
        )
        .unwrap();

        rebuild(&conn).unwrap();
        let rows = store::list_model_catalog(&conn, "ep-1").unwrap();
        let a: ModelAbilities = serde_json::from_str(&rows[0].abilities_json).unwrap();
        assert_eq!(a.reasoning, Some(true), "user override must land in catalog");
    }
}
