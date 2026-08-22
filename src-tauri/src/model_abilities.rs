//! Model ability data sourced from [Models.dev](https://models.dev) — the
//! same open model database OpenCode itself reads.
//!
//! OpenCode's per-model config accepts capability booleans (`reasoning`,
//! `tool_call`, `attachment`, `temperature`) and a `limit` object. Nestra
//! used to emit only `{ "name": "<id>" }`, so OpenCode never learned a
//! model could reason. This module gathers the authoritative abilities
//! from `https://models.dev/models.json`, caches them in `setting_kv`
//! (7-day TTL), and exposes lookups the OpenCode adapter turns into
//! per-model entry fields.
//!
//! Hard rule: **only the OpenCode adapter consumes this.** Other writers
//! stay untouched. Unmatched/offline models fall back to name-only.

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::db;
use crate::error::AppResult;

/// `setting_kv` key holding the cached models.dev payload + fetch time.
const CACHE_KEY: &str = "models_dev_cache";
const ENDPOINT: &str = "https://models.dev/models.json";
/// Refresh window. A weekly pull is plenty — model capability data is
/// near-static and the cache is best-effort enrichment, never load-bearing.
const TTL_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// The subset of models.dev/OpenCode fields Nestra knows how to emit.
/// `Option` everywhere so we only write keys models.dev actually reported —
/// never invent defaults. Both Serialize (for the OpenCode config writer) and
/// Deserialize (for the user-override JSON blob) are required.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelAbilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<ModelLimit>,
    /// Input/output modalities (text/image/video/audio/pdf). Sourced from
    /// models.dev's `modalities` field, with bundled corrections overriding
    /// vendor-stale entries (e.g. MiniMax-M3). The capability disclosure
    /// renders this read-only; the merge layer keeps it consistent with the
    /// other fields so a future editor can plug in without schema work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modalities: Option<Modalities>,
    /// The protocol dialect this model is officially served on (e.g.
    /// `"anthropic"` / `"openai-comp"` / `"response-api"`). `None` = no per-model
    /// protocol info — the model follows the endpoint's protocol. Used by
    /// the Direct-mode writer to filter which models are written for a
    /// given agent protocol (e.g. Claude Code + opencode-go only gets the
    /// models that actually speak Anthropic). Sourced from the bundled
    /// corrections map (provider-verified, like the context limits).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelLimit {
    pub context: u64,
    pub output: u64,
    /// Optional input-token cap (OpenCode schema accepts it; models.dev
    /// rarely reports it). Carried through transparently when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<u64>,
}

/// Input/output modality matrix on a model entry. OpenCode's config schema
/// allows the same enum on both sides (`text|audio|image|video|pdf`); we
/// parse, merge, and re-emit it verbatim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Modalities {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input: Vec<Modality>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output: Vec<Modality>,
}

/// One modality token. The serde rename keeps the wire format lowercase to
/// match OpenCode's schema enum (`text`, `image`, `video`, `audio`, `pdf`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Modality {
    Text,
    Audio,
    Image,
    Video,
    Pdf,
}

/// Normalize a model id for matching: lowercase, trim whitespace + a
/// leading `models/` prefix, drop bracket/paren markers (`[1M]`, `(beta)`)
/// and a trailing `-YYYYMMDD` snapshot date. Mirrors cc-switch's
/// `resolve_image_input_capability` normalizer.
pub(crate) fn normalize(id: &str) -> String {
    let mut s = id.trim().to_lowercase();
    if let Some(rest) = s.strip_prefix("models/") {
        s = rest.to_string();
    }
    // Cut at the FIRST bracket/paren: the marker content is a suffix
    // (`claude-sonnet-4-5[1M]`, `MiniMax-M3[1m]`, `grok-4(beta)`) and must
    // be DROPPED, not kept — the old filter removed only the bracket chars,
    // leaving `claude-sonnet-4-51m`, which never matched the generated id
    // and silently disabled ability routing for every bracketed model.
    if let Some(idx) = s.find(['[', '(']) {
        s.truncate(idx);
    }
    while s.ends_with(char::is_whitespace) {
        s.pop();
    }
    // Strip a trailing snapshot date like "-20250929" (dash + 8 digits).
    let bytes = s.as_bytes();
    if bytes.len() >= 9 {
        let tail = &bytes[bytes.len() - 9..];
        if tail[0] == b'-' && tail[1..].iter().all(|b| b.is_ascii_digit()) {
            s.truncate(s.len() - 9);
        }
    }
    s.trim().to_string()
}

/// Pull the ability fields out of one models.dev entry. Returns `None`
/// when the entry carries none of the fields we emit (nothing to write).
fn parse_entry(v: &serde_json::Value) -> Option<ModelAbilities> {
    let reasoning = v.get("reasoning").and_then(|x| x.as_bool());
    let tool_call = v.get("tool_call").and_then(|x| x.as_bool());
    let attachment = v.get("attachment").and_then(|x| x.as_bool());
    let temperature = v.get("temperature").and_then(|x| x.as_bool());
    let limit = v
        .get("limit")
        .and_then(|l| {
            let ctx = l.get("context").and_then(|c| c.as_u64());
            let out = l.get("output").and_then(|o| o.as_u64());
            ctx.zip(out)
        })
        .map(|(context, output)| {
            let input = v
                .get("limit")
                .and_then(|l| l.get("input"))
                .and_then(|i| i.as_u64());
            ModelLimit { context, output, input }
        });
    let modalities = v
        .get("modalities")
        .and_then(parse_modalities);
    let api = v.get("api").and_then(|x| x.as_str()).map(String::from);
    if reasoning.is_none()
        && tool_call.is_none()
        && attachment.is_none()
        && temperature.is_none()
        && limit.is_none()
        && modalities.is_none()
        && api.is_none()
    {
        return None;
    }
    Some(ModelAbilities { reasoning, tool_call, attachment, temperature, limit, modalities, api })
}

/// Parse models.dev's `modalities: { input: [...], output: [...] }` shape
/// into the typed form. Drops any unknown tokens silently — the OpenCode
/// schema enum is closed (`text|audio|image|video|pdf`), and we'd rather
/// lose an exotic value than fail the whole entry.
fn parse_modalities(v: &serde_json::Value) -> Option<Modalities> {
    let obj = v.as_object()?;
    let input = parse_modality_array(obj.get("input"));
    let output = parse_modality_array(obj.get("output"));
    if input.is_empty() && output.is_empty() {
        return None;
    }
    Some(Modalities { input, output })
}

fn parse_modality_array(v: Option<&serde_json::Value>) -> Vec<Modality> {
    v.and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| item.as_str())
                .filter_map(|s| serde_json::from_value::<Modality>(serde_json::Value::String(s.into())).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Build a `normalized_id -> abilities` index from the models.dev payload.
/// The payload is a flat object keyed by `lab/model` (e.g. `openai/gpt-4o`).
pub fn build_index(json: &serde_json::Value) -> HashMap<String, ModelAbilities> {
    let mut map = HashMap::new();
    let Some(obj) = json.as_object() else { return map };
    for (_raw_id, v) in obj {
        // `parse_entry` only READS — pass `v` by reference instead of
        // cloning every entry's Value (thousands of records).
        let Some(abilities) = parse_entry(v) else { continue };
        // Key by the normalized `id` field (models.dev includes `id`
        // inside each entry; fall back to the object key).
        let key = v
            .get("id")
            .and_then(|i| i.as_str())
            .map(normalize)
            .unwrap_or_else(|| normalize(_raw_id));
        map.insert(key, abilities);
    }
    map
}

/// Resolve abilities for a single provider model id.
///
/// Match order:
/// 1. normalized exact hit on the index key;
/// 2. tail-segment match — compare the last `/`-segment of the query
///    against the last segment of each index key (handles
///    `gpt-4o` ↔ `openai/gpt-4o`);
/// 3. `None`.
///
/// Tail matches that describe the SAME model from different sources are
/// merged field-by-field (e.g. `opencode-go/minimax-m3` carries the `api`
/// dialect while `minimax/MiniMax-M3` carries the context fix — the merged
/// result has both). Two sources that CONTRADICT on a field (both set,
/// different values) are genuinely ambiguous → `None` rather than guessing.
pub fn abilities_for(index: &HashMap<String, ModelAbilities>, model_id: &str) -> Option<ModelAbilities> {
    let needle = normalize(model_id);
    if needle.is_empty() {
        return None;
    }
    if let Some(a) = index.get(&needle) {
        return Some(a.clone());
    }
    let needle_tail = needle.rsplit('/').next().unwrap_or(&needle);
    if needle_tail.is_empty() {
        return None;
    }
    // Collect tail matches and merge compatible sources; a direct field
    // contradiction makes the resolution ambiguous.
    let mut merged: Option<ModelAbilities> = None;
    for (k, v) in index.iter() {
        if k.rsplit('/').next().unwrap_or(k) != needle_tail {
            continue;
        }
        match &merged {
            None => merged = Some(v.clone()),
            Some(cur) => {
                if field_conflicts(cur, v) {
                    return None;
                }
                merged = Some(merge_field_overrides(cur.clone(), v.clone()));
            }
        }
    }
    merged
}

/// `true` when two ability sets disagree on any field both report.
/// `true` when two ability sets disagree on any field both report. Boolean
/// fields and `api` are atomic; limit/modalities are compared PER-FIELD so a
/// partial overlap (one source reports only `context`, the other only
/// `output`) MERGES instead of being treated as a conflict — the old
/// whole-struct `!=` comparison threw away mergeable data whenever two
/// sources covered different subsets of the same limit.
fn field_conflicts(a: &ModelAbilities, b: &ModelAbilities) -> bool {
    let conflict = |x: Option<bool>, y: Option<bool>| matches!((x, y), (Some(l), Some(r)) if l != r);
    conflict(a.reasoning, b.reasoning)
        || conflict(a.tool_call, b.tool_call)
        || conflict(a.attachment, b.attachment)
        || conflict(a.temperature, b.temperature)
        || match (&a.api, &b.api) {
            (Some(l), Some(r)) if l != r => true,
            _ => false,
        }
        || match (&a.limit, &b.limit) {
            (Some(l), Some(r)) => {
                (l.context != r.context)
                    || (l.output != r.output)
                    || match (l.input, r.input) {
                        (Some(li), Some(ri)) => li != ri,
                        _ => false,
                    }
            }
            _ => false,
        }
        || match (&a.modalities, &b.modalities) {
            (Some(l), Some(r)) => {
                // Partial overlap merges; conflict only when a shared
                // dimension disagrees.
                let input_conflict = !l.input.is_empty()
                    && !r.input.is_empty()
                    && l.input != r.input;
                let output_conflict = !l.output.is_empty()
                    && !r.output.is_empty()
                    && l.output != r.output;
                input_conflict || output_conflict
            }
            _ => false,
        }
}

/// Subset the global index down to just the ids a provider actually uses,
/// keyed by the provider's own model id (so the writer can look up by the
/// exact id it writes).
pub fn subset_for(
    index: &HashMap<String, ModelAbilities>,
    ids: &[String],
) -> HashMap<String, ModelAbilities> {
    let mut out = HashMap::new();
    for id in ids {
        if out.contains_key(id) {
            continue;
        }
        if let Some(a) = abilities_for(index, id) {
            out.insert(id.clone(), a);
        }
    }
    out
}

/// The ordered `(key, value)` pairs the OpenCode writer appends to a model
/// entry, in OpenCode schema order. Only fields the abilities object
/// actually carries — absent fields are omitted entirely.
pub fn to_model_entry_fields(a: &ModelAbilities) -> Vec<(String, serde_json::Value)> {
    let mut fields = Vec::new();
    if let Some(b) = a.reasoning {
        fields.push(("reasoning".into(), serde_json::Value::Bool(b)));
    }
    if let Some(b) = a.tool_call {
        fields.push(("tool_call".into(), serde_json::Value::Bool(b)));
    }
    if let Some(b) = a.attachment {
        fields.push(("attachment".into(), serde_json::Value::Bool(b)));
    }
    if let Some(b) = a.temperature {
        fields.push(("temperature".into(), serde_json::Value::Bool(b)));
    }
    if let Some(l) = &a.limit {
        let mut lim = serde_json::Map::new();
        lim.insert("context".into(), serde_json::Value::Number(l.context.into()));
        lim.insert("output".into(), serde_json::Value::Number(l.output.into()));
        if let Some(inp) = l.input {
            lim.insert("input".into(), serde_json::Value::Number(inp.into()));
        }
        fields.push(("limit".into(), serde_json::Value::Object(lim)));
    }
    if let Some(m) = &a.modalities {
        let mut mods = serde_json::Map::new();
        if !m.input.is_empty() {
            mods.insert(
                "input".into(),
                serde_json::Value::Array(
                    m.input.iter().map(|x| serde_json::to_value(*x).unwrap_or(serde_json::Value::Null)).collect(),
                ),
            );
        }
        if !m.output.is_empty() {
            mods.insert(
                "output".into(),
                serde_json::Value::Array(
                    m.output.iter().map(|x| serde_json::to_value(*x).unwrap_or(serde_json::Value::Null)).collect(),
                ),
            );
        }
        if !mods.is_empty() {
            fields.push(("modalities".into(), serde_json::Value::Object(mods)));
        }
    }
    fields
}

/// Marker Claude Code recognises to mean "this model has a 1M-token context
/// window". Appended to model ids written into `ANTHROPIC_*_MODEL` env vars so
/// Claude Code stops defaulting to 200k for models it doesn't natively know
/// (third-party Anthropic-protocol endpoints like z.ai / MiniMax / GLM-4.5+).
/// Matches cc-switch's `claude_desktop_config::ONE_M_CONTEXT_MARKER`.
pub const ONE_M_CONTEXT_MARKER: &str = "[1m]";
/// Token count at and above which the [`ONE_M_CONTEXT_MARKER`] gets appended.
/// Mirrors Claude Code's own threshold — anything below is treated as 200k.
pub const ONE_M_CONTEXT_TOKENS: u64 = 1_000_000;

/// Returns `model_id` with a `[1m]` suffix appended when abilities indicate a
/// 1M-or-larger context window. No-op when abilities are absent, when the
/// `limit` field isn't reported, or when the window is below the threshold.
/// Idempotent: an id that already ends with `[1m]` (case-insensitive) is
/// returned unchanged, so re-running the helper on the same id never stacks
/// markers.
///
/// Only the Claude Code writer consumes this — Pi / OpenCode use their own
/// config schemas and don't read `ANTHROPIC_*_MODEL` env vars. Callers that
/// don't target Claude Code should leave the model id bare.
pub fn claude_code_model_id(model_id: &str, abilities: Option<&ModelAbilities>) -> String {
    if model_id.is_empty() {
        return String::new();
    }
    let needs = abilities
        .and_then(|a| a.limit.as_ref())
        .is_some_and(|l| l.context >= ONE_M_CONTEXT_TOKENS);
    let already = model_id
        .to_ascii_lowercase()
        .ends_with(ONE_M_CONTEXT_MARKER);
    if needs && !already {
        format!("{model_id}{ONE_M_CONTEXT_MARKER}")
    } else {
        model_id.to_string()
    }
}

/// Layer an override on top of a default ability. The override only wins on
/// fields it explicitly sets; unset fields inherit from `default`. `None`
/// fields in the default stay `None` unless the override carries a value
/// (so a user can fill in a missing models.dev entry without first having to
/// find the truth elsewhere).
pub fn merge_field_overrides(
    default: ModelAbilities,
    override_: ModelAbilities,
) -> ModelAbilities {
    ModelAbilities {
        reasoning: override_.reasoning.or(default.reasoning),
        tool_call: override_.tool_call.or(default.tool_call),
        attachment: override_.attachment.or(default.attachment),
        temperature: override_.temperature.or(default.temperature),
        limit: match (default.limit, override_.limit) {
            (_, Some(lim)) => Some(lim),
            (d, None) => d,
        },
        modalities: override_.modalities.or(default.modalities),
        api: override_.api.or(default.api),
    }
}


/// Merge a per-endpoint override map (`{"<model_id>": ModelAbilities}`) on
/// top of the models.dev defaults. The output key set is the union of the
/// two maps. Keys present only in `overrides` are emitted (with whatever
/// fields the user set); keys only in `defaults` pass through; collisions
/// resolve field-by-field via [`merge_field_overrides`].
pub fn merge_into(
    defaults: HashMap<String, ModelAbilities>,
    overrides: HashMap<String, ModelAbilities>,
) -> HashMap<String, ModelAbilities> {
    let mut out: HashMap<String, ModelAbilities> = defaults;
    for (id, ov) in overrides {
        match out.remove(&id) {
            Some(def) => {
                out.insert(id, merge_field_overrides(def, ov));
            }
            None => {
                out.insert(id, ov);
            }
        }
    }
    out
}

/// Parse the persisted JSON overrides blob (`{"<id>": { ... }}`) into a
/// `HashMap<String, ModelAbilities>`. Tolerant of `None` / empty input /
/// per-row parse errors — any individual row that fails to parse is
/// silently skipped rather than aborting the switch. The OpenCode writer
/// already handles a missing entry by emitting bare `{ "name": ... }`, so
/// "best-effort" is the correct failure mode.
pub fn parse_overrides(json: Option<&str>) -> HashMap<String, ModelAbilities> {
    let Some(s) = json else {
        return HashMap::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(s) else {
        return HashMap::new();
    };
    let Some(obj) = v.as_object() else {
        return HashMap::new();
    };
    let mut out: HashMap<String, ModelAbilities> = HashMap::new();
    for (id, row) in obj {
        let Ok(parsed) = serde_json::from_value::<ModelAbilities>(row.clone()) else {
            continue;
        };
        out.insert(id.clone(), parsed);
    }
    out
}

/// Bundled vendor-authoritative corrections, embedded into the binary at
/// compile time via `include_str!`. Keys use the same `lab/model` form as
/// models.dev (e.g. `minimax/MiniMax-M3`); values are merged on top of the
/// models.dev cache (this layer wins) and below per-endpoint user overrides
/// (user wins). Add a model here only when models.dev is provably wrong AND
/// the vendor's own docs publish the correct value — every entry is a
/// maintenance burden.
///
/// Layering (lowest → highest priority):
///   models.dev cache  <  `load_corrections()`  <  user overrides
///
/// Built once per process via `OnceLock` — the source is `include_str!`,
/// so it cannot change at runtime.
const CORRECTIONS_JSON: &str = include_str!("model_abilities_corrections.json");

pub fn load_corrections() -> HashMap<String, ModelAbilities> {
    static CACHE: std::sync::OnceLock<HashMap<String, ModelAbilities>> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| {
            // Bundled corrections are load-bearing (the MiniMax-M3 context
            // fix etc.) — a malformed bundle must fail LOUDLY at first use,
            // not silently ship an empty correction map.
            let v = serde_json::from_str(CORRECTIONS_JSON).expect("bundled corrections JSON is valid");
            build_index(&v)
        })
        .clone()
}

/// Refresh the cache from models.dev if it's older than [`TTL_MS`], or
/// absent — unless `force` is set, which skips the TTL check (used by the
/// explicit "Fetch models" button: a brand-new model may already be listed
/// upstream while the local cache is still inside its 7-day window).
/// **Network failure is never fatal** — on error the existing
/// cache (if any) is kept and this returns `Ok(())`.
/// NOTE: the fetch runs while
/// the caller holds the DB lock (the write targets the same connection) —
/// a slow network could stall other DB commands. Mitigations: the TTL check
/// makes this rare, a process-wide fetch-dedupe prevents concurrent call
/// sites from stacking fetches, and the timeout bounds the worst case.
pub fn refresh(conn: &rusqlite::Connection, force: bool) -> AppResult<()> {
    if !force {
        if let Some(cached) = db::get_setting(conn, CACHE_KEY)? {
            let fetched_at = cached
                .get("fetched_at")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let now = chrono::Utc::now().timestamp_millis();
            if now - fetched_at < TTL_MS {
                return Ok(()); // fresh enough
            }
        }
    }
    // Dedupe: several call sites may enter within the same window — only one
    // of them performs the network fetch; the rest see `true` and skip.
    use std::sync::atomic::{AtomicBool, Ordering};
    static FETCHING: AtomicBool = AtomicBool::new(false);
    if FETCHING.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    struct FetchGuard;
    impl Drop for FetchGuard {
        fn drop(&mut self) {
            FETCHING.store(false, Ordering::SeqCst);
        }
    }
    let _guard = FetchGuard;

    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .build();
    let body = match agent.get(ENDPOINT).call() {
        Ok(resp) => resp.into_string().unwrap_or_default(),
        Err(e) => {
            tracing::warn!(error = %e, "models.dev fetch failed; keeping existing cache");
            return Ok(());
        }
    };
    let json: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "models.dev payload unparseable; keeping existing cache");
            return Ok(());
        }
    };
    if !json.is_object() {
        return Ok(());
    }
    let cache = serde_json::json!({
        "fetched_at": chrono::Utc::now().timestamp_millis(),
        "json": json,
    });
    db::set_setting(conn, CACHE_KEY, &cache)?;
    Ok(())
}

/// Load the index from whatever is in cache (no network). Empty map when
/// no cache exists — callers then emit name-only entries.
pub fn load_index(conn: &rusqlite::Connection) -> AppResult<HashMap<String, ModelAbilities>> {
    let Some(cached) = db::get_setting(conn, CACHE_KEY)? else {
        return Ok(HashMap::new());
    };
    let json = cached.get("json").cloned().unwrap_or(serde_json::Value::Null);
    Ok(build_index(&json))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ab(reasoning: bool, tool_call: bool, attachment: bool) -> ModelAbilities {
        ModelAbilities {
            reasoning: Some(reasoning),
            tool_call: Some(tool_call),
            attachment: Some(attachment),
            temperature: None,
            limit: Some(ModelLimit { context: 200_000, output: 8_000, input: None }),
            modalities: None,
            api: None,
        }
    }

    #[test]
    fn normalize_strips_prefix_markers_and_date() {
        assert_eq!(normalize("models/gpt-4o"), "gpt-4o");
        assert_eq!(normalize(" GPT-4o "), "gpt-4o");
        // Bracket content is DROPPED, not kept: the old behavior produced
        // "claude-sonnet-4-51m" which never matched the generated id.
        assert_eq!(normalize("claude-sonnet-4-5[1M]"), "claude-sonnet-4-5");
        assert_eq!(normalize("minimax-m3[1m]"), "minimax-m3");
        assert_eq!(normalize("grok-4(beta)"), "grok-4");
        assert_eq!(normalize("claude-sonnet-4-5-20250929"), "claude-sonnet-4-5");
        assert_eq!(normalize("openai/gpt-4o"), "openai/gpt-4o");
    }

    #[test]
    fn bracket_marker_ids_match_generated_ids() {
        // The generated catalog id (with bracket suffix) must resolve to the
        // same normalized key as the plain id — this is what makes ability
        // routing actually work for bracketed models.
        let idx = build_index(&serde_json::json!({
            "minimax/minimax-m3[1m]": {
                "id": "minimax/minimax-m3[1m]",
                "reasoning": true,
                "tool_call": true,
                "attachment": false,
                "limit": { "context": 200000, "output": 64000 }
            }
        }));
        assert_eq!(normalize("minimax-m3[1m]"), normalize("minimax-m3"));
        let a = abilities_for(&idx, "MiniMax-M3[1M]").expect("bracketed lookup");
        assert_eq!(a.reasoning, Some(true), "reasoning survives the bracket");
        let b = abilities_for(&idx, "minimax-m3").expect("plain lookup");
        assert_eq!(a.limit, b.limit);
    }

    #[test]
    fn partial_limit_overlap_merges_not_conflicts() {
        // Same context/output, but `input` reported by only ONE source —
        // the old whole-struct `!=` treated the None-vs-Some as a conflict
        // and dropped the mergeable data.
        let a = ModelAbilities {
            limit: Some(ModelLimit { context: 128000, output: 16384, input: Some(100000) }),
            ..Default::default()
        };
        let b = ModelAbilities {
            limit: Some(ModelLimit { context: 128000, output: 16384, input: None }),
            ..Default::default()
        };
        assert!(
            !field_conflicts(&a, &b),
            "input None-vs-Some is mergeable, not a conflict"
        );
        let conflict_a = ModelAbilities {
            limit: Some(ModelLimit { context: 128000, output: 0, input: None }),
            ..Default::default()
        };
        let conflict_b = ModelAbilities {
            limit: Some(ModelLimit { context: 64000, output: 0, input: None }),
            ..Default::default()
        };
        assert!(
            field_conflicts(&conflict_a, &conflict_b),
            "same limit field with different values IS a conflict"
        );
    }

    #[test]
    fn build_index_keys_by_normalized_id() {
        let payload = serde_json::json!({
            "openai/gpt-4o": {
                "id": "openai/gpt-4o",
                "reasoning": false,
                "tool_call": true,
                "attachment": true,
                "limit": { "context": 128000, "output": 16384 }
            },
            "anthropic/claude-sonnet-4-5": {
                "id": "anthropic/claude-sonnet-4-5",
                "reasoning": true,
                "tool_call": true,
                "attachment": true
            }
        });
        let idx = build_index(&payload);
        assert!(idx.contains_key("openai/gpt-4o"));
        assert!(idx.contains_key("anthropic/claude-sonnet-4-5"));
    }

    #[test]
    fn abilities_for_exact_match() {
        let payload = serde_json::json!({
            "openai/gpt-4o": { "id": "openai/gpt-4o", "reasoning": false, "tool_call": true, "attachment": true }
        });
        let idx = build_index(&payload);
        let a = abilities_for(&idx, "openai/gpt-4o").unwrap();
        assert_eq!(a.reasoning, Some(false));
        assert_eq!(a.tool_call, Some(true));
    }

    #[test]
    fn abilities_for_tail_segment_matches_bare_id() {
        let payload = serde_json::json!({
            "openai/gpt-4o": { "id": "openai/gpt-4o", "reasoning": false, "tool_call": true, "attachment": true }
        });
        let idx = build_index(&payload);
        // Provider lists the bare id; we match on the tail segment.
        assert!(abilities_for(&idx, "gpt-4o").is_some());
        assert!(abilities_for(&idx, "models/gpt-4o").is_some());

        // Snapshot date (YYYYMMDD) stripped, then tail-segment matches.
        let payload2 = serde_json::json!({
            "anthropic/claude-sonnet-4-5": { "id": "anthropic/claude-sonnet-4-5", "reasoning": true, "tool_call": true, "attachment": true }
        });
        let idx2 = build_index(&payload2);
        assert!(abilities_for(&idx2, "claude-sonnet-4-5-20250929").is_some());
    }

    #[test]
    fn abilities_for_ambiguous_tail_returns_none() {
        // Two distinct labs shipping the same tail segment → ambiguous.
        let payload = serde_json::json!({
            "openai/gemini": { "id": "openai/gemini", "reasoning": true, "tool_call": true, "attachment": true },
            "google/gemini": { "id": "google/gemini", "reasoning": false, "tool_call": false, "attachment": false }
        });
        let idx = build_index(&payload);
        assert!(abilities_for(&idx, "gemini").is_none());
    }

    #[test]
    fn abilities_for_aliased_tail_resolves_when_abilities_match() {
        // Two index entries, same tail, identical abilities → not ambiguous.
        let payload = serde_json::json!({
            "openai/gpt-4o": { "id": "openai/gpt-4o", "reasoning": false, "tool_call": true, "attachment": true },
            "mirror/gpt-4o": { "id": "mirror/gpt-4o", "reasoning": false, "tool_call": true, "attachment": true }
        });
        let idx = build_index(&payload);
        assert!(abilities_for(&idx, "gpt-4o").is_some());
    }

    #[test]
    fn abilities_for_unmatched_returns_none() {
        let idx: HashMap<String, ModelAbilities> = HashMap::new();
        assert!(abilities_for(&idx, "does-not-exist").is_none());
    }

    #[test]
    fn subset_for_dedupes_and_keeps_matches_only() {
        let payload = serde_json::json!({
            "openai/gpt-4o": { "id": "openai/gpt-4o", "reasoning": false, "tool_call": true, "attachment": true }
        });
        let idx = build_index(&payload);
        let ids = vec!["gpt-4o".to_string(), "gpt-4o".to_string(), "nope".to_string()];
        let sub = subset_for(&idx, &ids);
        assert_eq!(sub.len(), 1);
        assert!(sub.contains_key("gpt-4o"));
    }

    #[test]
    fn to_model_entry_fields_emits_only_present_keys() {
        // ab(true, true, false): reasoning + tool_call present, attachment
        // present (false), limit present. temperature absent.
        let a = ab(true, true, false);
        let fields = to_model_entry_fields(&a);
        let keys: Vec<&str> = fields.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["reasoning", "tool_call", "attachment", "limit"]);

        // Only reasoning present (bool) + attachment (bool false) → two keys.
        let a2 = ModelAbilities {
            reasoning: Some(true),
            tool_call: None,
            attachment: Some(false),
            temperature: None,
            limit: None,
            modalities: None,
            api: None,
        };
        let f2 = to_model_entry_fields(&a2);
        let keys2: Vec<&str> = f2.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys2, vec!["reasoning", "attachment"]);
    }

    fn at(context: u64) -> ModelAbilities {
        ModelAbilities {
            reasoning: None,
            tool_call: None,
            attachment: None,
            temperature: None,
            limit: Some(ModelLimit { context, output: 8_000, input: None }),
            modalities: None,
            api: None,
        }
    }

    #[test]
    fn claude_code_model_id_passes_through_when_abilities_absent() {
        assert_eq!(claude_code_model_id("MiniMax-M3", None), "MiniMax-M3");
    }

    #[test]
    fn claude_code_model_id_passes_through_when_limit_absent() {
        let a = ModelAbilities {
            reasoning: Some(true),
            tool_call: None,
            attachment: None,
            temperature: None,
            limit: None,
            modalities: None,
       api: None,
        };
        assert_eq!(claude_code_model_id("glm-5.2", Some(&a)), "glm-5.2");
    }

    #[test]
    fn claude_code_model_id_passes_through_below_1m_threshold() {
        // 200_000 is the default Anthropic context — no suffix needed.
        assert_eq!(claude_code_model_id("claude-opus", Some(&at(200_000))), "claude-opus");
        // Just under the threshold — still no suffix.
        assert_eq!(claude_code_model_id("foo", Some(&at(999_999))), "foo");
    }

    #[test]
    fn claude_code_model_id_appends_marker_at_threshold_and_above() {
        assert_eq!(claude_code_model_id("MiniMax-M3", Some(&at(1_000_000))), "MiniMax-M3[1m]");
        assert_eq!(claude_code_model_id("glm-5.2", Some(&at(2_000_000))), "glm-5.2[1m]");
    }

    #[test]
    fn claude_code_model_id_is_idempotent_against_existing_marker() {
        let a = at(1_000_000);
        // Lowercase marker already present.
        assert_eq!(claude_code_model_id("foo[1m]", Some(&a)), "foo[1m]");
        // Uppercase M — case-insensitive match.
        assert_eq!(claude_code_model_id("foo[1M]", Some(&a)), "foo[1M]");
        // Mixed case.
        assert_eq!(claude_code_model_id("foo[1m]", Some(&a)), "foo[1m]");
    }

    #[test]
    fn claude_code_model_id_handles_empty_input() {
        let a = at(1_000_000);
        assert_eq!(claude_code_model_id("", None), "");
        assert_eq!(claude_code_model_id("", Some(&a)), "");
    }

    fn full(
        reasoning: bool,
        tool_call: bool,
        attachment: bool,
        temperature: bool,
        ctx: u64,
        out: u64,
    ) -> ModelAbilities {
        ModelAbilities {
            reasoning: Some(reasoning),
            tool_call: Some(tool_call),
            attachment: Some(attachment),
            temperature: Some(temperature),
            limit: Some(ModelLimit { context: ctx, output: out, input: None }),
            modalities: None,
            api: None,
        }
    }

    #[test]
    fn merge_field_overrides_wins_only_on_set_fields() {
        let def = full(true, true, true, false, 200_000, 8_000);
        let ov = ModelAbilities {
            reasoning: Some(false), // override: flip off
            tool_call: None,        // inherit
            attachment: Some(true), // explicit (matches default — same result)
            temperature: Some(true),
            limit: None,
            modalities: None,
       api: None,
        };
        let merged = merge_field_overrides(def, ov);
        assert_eq!(merged.reasoning, Some(false));
        assert_eq!(merged.tool_call, Some(true), "tool_call inherits from default");
        assert_eq!(merged.attachment, Some(true));
        assert_eq!(merged.temperature, Some(true));
        assert_eq!(merged.limit.as_ref().unwrap().context, 200_000);
    }

    #[test]
    fn merge_field_overrides_fills_gaps_when_default_is_empty() {
        // No default data for this model id; the override fully populates.
        let def = ModelAbilities {
            reasoning: None,
            tool_call: None,
            attachment: None,
            temperature: None,
            limit: None,
            modalities: None,
       api: None,
        };
        let ov = full(true, true, true, false, 100_000, 4_000);
        let merged = merge_field_overrides(def, ov);
        assert_eq!(merged.reasoning, Some(true));
        assert_eq!(merged.tool_call, Some(true));
        assert!(merged.limit.is_some());
    }

    #[test]
    fn merge_into_unions_keys_and_resolves_collisions() {
        let mut defaults = HashMap::new();
        defaults.insert("a".into(), full(true, true, true, false, 200_000, 8_000));
        defaults.insert("b-only-default".into(), full(false, true, false, false, 1, 1));

        let mut overrides = HashMap::new();
        // Collision: override flips reasoning off but inherits everything else.
        overrides.insert(
            "a".into(),
            ModelAbilities {
                reasoning: Some(false),
                tool_call: None,
                attachment: None,
                temperature: None,
                limit: None,
                modalities: None,
           api: None,
        },
        );
        // New model id the cache doesn't know about.
        overrides.insert("custom".into(), full(true, false, false, true, 4096, 1024));

        let out = merge_into(defaults, overrides);
        assert_eq!(out.len(), 3, "default-only key + collision + new key");

        let a = out.get("a").unwrap();
        assert_eq!(a.reasoning, Some(false), "override wins on collision");
        assert_eq!(a.tool_call, Some(true), "default inherits on collision");
        assert_eq!(a.limit.as_ref().unwrap().context, 200_000);

        assert!(out.contains_key("b-only-default"));
        assert!(out.contains_key("custom"));
    }

    #[test]
    fn parse_overrides_handles_none_empty_and_malformed() {
        assert!(parse_overrides(None).is_empty());
        assert!(parse_overrides(Some("")).is_empty());
        assert!(parse_overrides(Some("not json")).is_empty());
        // Non-object root (e.g. an array) is also rejected.
        assert!(parse_overrides(Some("[1,2,3]")).is_empty());
        // A bad row is skipped, a good one survives — partial JSON is best-effort.
        let mixed = r#"{
            "ok": {"reasoning": true},
            "broken": "not an abilities object"
        }"#;
        let parsed = parse_overrides(Some(mixed));
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed.get("ok").unwrap().reasoning, Some(true));
    }

    #[test]
    fn load_corrections_overrides_minimax_m3_context_to_1m() {
        // Vendor-authoritative correction: models.dev reports 512000 for
        // MiniMax-M3, MiniMax's own docs publish 1,000,000. The bundled
        // corrections file must win over the cache value.
        let corrections = load_corrections();
        let a = abilities_for(&corrections, "MiniMax-M3")
            .expect("MiniMax-M3 should match a correction entry");
        assert_eq!(a.limit.as_ref().unwrap().context, 1_000_000, "context must be 1M per MiniMax docs");
        assert_eq!(a.limit.as_ref().unwrap().output, 128_000);
    }

    #[test]
    fn corrections_cover_opencode_go_ox_alpha_free_from_bare_id() {
        // models.dev doesn't list ox-alpha at all and OpenCode Zen's /models
        // is ids-only, so the bundled correction is the only ability source.
        // The endpoint's fetched id is bare ("ox-alpha-free") — it must
        // tail-match the "opencode-go/ox-alpha-free" key. Limit figures are
        // the vendor's own (mirrored on OpenRouter as stealth/ox-alpha).
        let corrections = load_corrections();
        let a = abilities_for(&corrections, "ox-alpha-free")
            .expect("bare go id should tail-match the correction entry");
        assert_eq!(a.limit.as_ref().unwrap().context, 1_048_576);
        assert_eq!(a.limit.as_ref().unwrap().output, 131_072);
        assert_eq!(a.api.as_deref(), Some("openai-comp"));
    }

    #[test]
    fn corrections_layer_wins_over_models_dev_for_minimax_m3() {
        // Simulate the production layering: models.dev cache says 512000,
        // corrections file says 1000000, the merge result must be 1000000.
        let mut defaults = HashMap::new();
        defaults.insert(
            "MiniMax-M3".into(),
            ModelAbilities {
                reasoning: Some(true),
                tool_call: Some(true),
                attachment: Some(true),
                temperature: None,
                limit: Some(ModelLimit { context: 512_000, output: 128_000, input: None }),
                modalities: None,
                api: None,
            },
        );
        let corrections = subset_for(&load_corrections(), &["MiniMax-M3".into()]);
        let merged = merge_into(defaults, corrections);
        let a = merged.get("MiniMax-M3").unwrap();
        assert_eq!(a.limit.as_ref().unwrap().context, 1_000_000, "corrections must override cache");
        // Non-overlapping fields from the cache survive.
        assert_eq!(a.reasoning, Some(true));
        assert_eq!(a.attachment, Some(true));
    }

    #[test]
    fn user_overrides_defeat_corrections_layer() {
        // User override is the highest tier — if the user explicitly sets a
        // value, neither the cache nor the bundled corrections can win.
        let mut defaults = HashMap::new();
        defaults.insert(
            "MiniMax-M3".into(),
            ModelAbilities {
                reasoning: Some(true),
                tool_call: Some(true),
                attachment: Some(true),
                temperature: None,
                limit: Some(ModelLimit { context: 512_000, output: 128_000, input: None }),
                modalities: None,
                api: None,
            },
        );
        let corrections = subset_for(&load_corrections(), &["MiniMax-M3".into()]);
        let with_corrections = merge_into(defaults, corrections);
        let mut user = HashMap::new();
        user.insert(
            "MiniMax-M3".into(),
            ModelAbilities {
                reasoning: None,
                tool_call: None,
                attachment: None,
                temperature: None,
                limit: Some(ModelLimit { context: 42, output: 13, input: None }),
                modalities: None,
                api: None,
            },
        );
        let merged = merge_into(with_corrections, user);
        let a = merged.get("MiniMax-M3").unwrap();
        assert_eq!(a.limit.as_ref().unwrap().context, 42, "user override must win");
    }

    #[test]
    fn parse_entry_picks_up_modalities_from_models_dev_payload() {
        // MiniMax-M3 in models.dev carries modalities.input=[text,image,video].
        // parse_entry must surface it (the attachment bool alone is too coarse
        // — video vs image distinction is lost).
        let v = serde_json::json!({
            "reasoning": true,
            "tool_call": true,
            "attachment": true,
            "modalities": { "input": ["text", "image", "video"], "output": ["text"] }
        });
        let a = parse_entry(&v).expect("entry with modalities must parse");
        let mods = a.modalities.expect("modalities must be populated");
        assert_eq!(mods.input, vec![Modality::Text, Modality::Image, Modality::Video]);
        assert_eq!(mods.output, vec![Modality::Text]);
    }

    #[test]
    fn to_model_entry_fields_emits_modalities_in_schema_shape() {
        // The OpenCode schema requires modalities as
        // `{ "input": ["text","image",...], "output": [...] }` — verify the
        // emitter produces exactly that shape, with the lowercase enum
        // tokens the schema enum expects.
        let a = ModelAbilities {
            reasoning: None,
            tool_call: None,
            attachment: None,
            temperature: None,
            limit: None,
            modalities: Some(Modalities {
                input: vec![Modality::Text, Modality::Image, Modality::Video],
                output: vec![Modality::Text],
            }),
            api: None,
        };
        let fields = to_model_entry_fields(&a);
        let modalities_field = fields
            .iter()
            .find(|(k, _)| k == "modalities")
            .expect("modalities field must be emitted");
        let obj = modalities_field.1.as_object().unwrap();
        let input: Vec<&str> = obj.get("input").unwrap().as_array().unwrap()
            .iter().map(|x| x.as_str().unwrap()).collect();
        let output: Vec<&str> = obj.get("output").unwrap().as_array().unwrap()
            .iter().map(|x| x.as_str().unwrap()).collect();
        assert_eq!(input, vec!["text", "image", "video"]);
        assert_eq!(output, vec!["text"]);
    }

    #[test]
    fn parse_modalities_drops_unknown_tokens_silently() {
        // If models.dev introduces a new modality token (e.g. "3d"), the
        // parser must not fail the whole entry — drop the unknown value.
        let v = serde_json::json!({
            "input": ["text", "unknown-future-modality"],
            "output": ["text"]
        });
        let mods = parse_modalities(&v).expect("at least one valid token survives");
        assert_eq!(mods.input, vec![Modality::Text]);
    }
}
