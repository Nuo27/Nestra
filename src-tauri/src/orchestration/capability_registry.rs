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

/// models.dev cache < pi.dev catalogs < bundled corrections < per-endpoint
/// user overrides. Corrections carry the per-model `api` dialect (which wire
/// a model is officially served on) and context fixes; pi.dev carries
/// richer limits for providers models.dev under-reports (opencode-go's own
/// /models is ids-only). Without this merge the catalog would lack `api`
/// entirely for models.dev-absent providers and wire selection would
/// silently fall back to the endpoint protocol.
pub(crate) fn merged_index(conn: &Connection) -> AppResult<HashMap<String, crate::model_abilities::ModelAbilities>> {
    Ok(crate::model_abilities::merge_into_tail(
        crate::model_abilities::merge_into_tail(
            crate::model_abilities::load_index(conn)?,
            crate::model_abilities::load_pi_index(conn)?,
        ),
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

/// Derive the request's capability requirements from its JSON body, keyed to
/// the inbound wire (Smart Gateway fix 2 — the capability routing stage
/// existed and filtered nothing because no handler ever set
/// `TaskContext::required_capabilities`).
///
/// Conservative by design: a flag flips to `true` only on a PRESENT
/// structural signal (non-empty `tools`/`functions`, an image block, a
/// `thinking` config); a text-only request stays all-`false`, which filters
/// nothing — the same over-include-on-unknown policy as [`satisfies`]. A body
/// that isn't a JSON object yields the default (never a routing rejection).
/// `context_floor` is deferred (it needs tokenization).
pub fn derive_capability_req(body: &[u8], hint: ProviderKind) -> CapabilityReq {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return CapabilityReq::default();
    };
    let Some(obj) = v.as_object() else {
        return CapabilityReq::default();
    };
    let mut req = CapabilityReq::default();
    // Tool/function declarations ride both wire dialects under these keys.
    req.tool_call = ["tools", "functions"]
        .iter()
        .any(|k| obj.get(*k).and_then(|t| t.as_array()).is_some_and(|a| !a.is_empty()));
    let image_in_messages = |msgs: &serde_json::Value| -> bool {
        msgs.as_array().is_some_and(|ms| {
            ms.iter().any(|m| {
                m.get("content")
                    .and_then(|c| c.as_array())
                    .is_some_and(|blocks| {
                        blocks.iter().any(|b| {
                            let t = b.get("type").and_then(|t| t.as_str());
                            // Anthropic content block / OpenAI content part.
                            t == Some("image") || t == Some("image_url") || t == Some("input_image")
                        })
                    })
            })
        })
    };
    match hint {
        ProviderKind::Anthropic => {
            // A top-level thinking config (carrying budget_tokens) asks for a
            // reasoning-capable model. `null` counts as absent.
            req.reasoning = obj.get("thinking").is_some_and(|t| !t.is_null());
            req.vision = image_in_messages(obj.get("messages").unwrap_or(&serde_json::Value::Null));
        }
        // OpenAI chat shape (the Responses inbound was removed; a Responses
        // dialect body would reuse the same `input_image` part check).
        _ => {
            req.vision = image_in_messages(obj.get("messages").unwrap_or(&serde_json::Value::Null));
        }
    }
    req
}

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

#[cfg(test)]
mod tests;
