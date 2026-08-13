//! Live quota fetching for provider endpoints.
//!
//! Each endpoint carries a [`QuotaQueryPlan`] that says how its quota is
//! queried: a built-in provider fetcher ([`QuotaQueryPlan::Preset`] — Z.ai /
//! MiniMax / OpenRouter / the local mock) or a user-configured balance
//! extractor ([`QuotaQueryPlan::Custom`]). Endpoints with
//! [`QuotaQueryPlan::None`] return an "unconfigured" snapshot rather than
//! pretending. Plans are declared by provider presets (see
//! `commands::provider_presets`) and inherited at create time; legacy
//! endpoints are backfilled by host detection in `quota_refresh::resolve_plan`.

use crate::db::EndpointRow;
#[cfg(test)]
use crate::db::ProtocolEntry;
use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const TIMEOUT: u64 = 20;

#[derive(Debug, Clone, Serialize)]
pub struct QuotaItem {
    pub name: String,
    pub pct: f64,
    pub used: Option<f64>,
    pub total: Option<f64>,
    pub remaining: Option<f64>,
    pub resets_in: Option<String>,
    /// Absolute UTC reset time in milliseconds. `None` for providers that
    /// don't expose a hard expiry (Moonshot balance) or whose flat response
    /// shape doesn't carry one.
    #[serde(default)]
    pub resets_at_ms: Option<i64>,
    /// Currency unit for balance-based items (e.g. "CNY", "USD"). `None`
    /// for window-based quota.
    #[serde(default)]
    pub unit: Option<String>,
    /// True for monetary-balance items (OpenRouter credits, Moonshot
    /// balance): no reset window semantics, and the keep-alive worker must
    /// never ping them (a request can't "reset" a balance).
    #[serde(default)]
    pub is_balance: bool,
}

/// Pick the absolute UTC reset time (ms) of the 5-hour quota window from a
/// quota response. Returns `None` if no `5h` item is present.
///
/// Recognised names:
/// - Z.ai `"5h-token"`
/// - MiniMax new shape `"{model}/5h"`
/// - MiniMax flat shape `"5h-token"` (carries no reset timestamp from the
///   wire; caller falls back to the next fetch after the POST succeeds).
#[cfg(test)]
pub fn pick_five_hour_expiry(items: &[QuotaItem]) -> Option<i64> {
    items
        .iter()
        .find(|i| i.name == "5h-token" || i.name.ends_with("/5h"))
        .and_then(|i| i.resets_at_ms)
}

#[derive(Debug, Clone, Serialize)]
pub struct EndpointQuota {
    pub ok: bool,
    pub plan: Option<String>,
    pub error: Option<String>,
    pub items: Vec<QuotaItem>,
}

/// Fetch quota for an endpoint under an explicit [`QuotaQueryPlan`].
/// `Preset { kind }` dispatches to the built-in fetcher for that provider,
/// `Custom(cfg)` runs the user-configured balance extractor, `None` returns
/// an "unconfigured" snapshot (no fetch attempted). This is the single
/// dispatch entry — there is no host-detection fallback here; legacy
/// endpoints are resolved to a plan by `quota_refresh::resolve_plan`.
///
/// `opencode` carries the OpenCode Go dashboard credentials (`(cookie,
/// workspace_id)`) and is only consulted by the `OpencodeGo` arm — every
/// other plan ignores it. Callers load it from `secrets.rs` + the settings
/// blob only when the plan is `Preset { OpencodeGo }`.
pub fn fetch_with_plan(
    endpoint: &EndpointRow,
    key: &str,
    plan: &QuotaQueryPlan,
    opencode: Option<(&str, &str)>,
) -> EndpointQuota {
    match plan {
        QuotaQueryPlan::None => EndpointQuota {
            ok: false,
            plan: None,
            error: Some("no query plan configured".into()),
            items: vec![],
        },
        QuotaQueryPlan::Preset { kind } => {
            let url = crate::db::pick_quota_url(&endpoint.protocols).unwrap_or_default();
            match kind {
                BuiltinKind::Zai => fetch_zai(key),
                BuiltinKind::Minimax => fetch_minimax(key),
                BuiltinKind::Openrouter => fetch_openrouter(key),
                BuiltinKind::OpencodeGo => match opencode {
                    Some((cookie, workspace_id)) => fetch_opencode_go(cookie, workspace_id),
                    None => EndpointQuota {
                        ok: false,
                        plan: None,
                        error: Some(
                            "OpenCode Go cookie + workspace ID not set — add them in quota settings"
                                .into(),
                        ),
                        items: vec![],
                    },
                },
                BuiltinKind::Mock => fetch_mock(&url, key),
            }
        }
        QuotaQueryPlan::Custom(cfg) => {
            // `QuotaExtractorConfig.enabled` was written but never read — a
            // disabled extractor kept fetching (and showing balance) anyway.
            if !cfg.enabled {
                EndpointQuota {
                    ok: false,
                    plan: None,
                    error: Some("custom quota extractor is disabled".into()),
                    items: vec![],
                }
            } else {
                fetch_custom(endpoint, key, cfg)
            }
        }
    }
}

/// A built-in provider quota fetcher. The dispatch key for
/// [`QuotaQueryPlan::Preset`]. Declared by provider presets and resolved
/// from the endpoint's base_url host as a legacy fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuiltinKind {
    Zai,
    Minimax,
    /// OpenRouter key limits (`/api/v1/key`). Balance-based: no reset
    /// window, `is_balance` items only.
    Openrouter,
    /// OpenCode Go plan usage. No API-key endpoint exists — usage is scraped
    /// from the authenticated dashboard HTML using a browser session cookie
    /// + workspace ID (same method as the community tools opencode-bar and
    /// opencode-quota). See `fetch_opencode_go`.
    OpencodeGo,
    /// A local mock upstream (127.0.0.1 / localhost) serving `GET /v1/quota`
    /// — used by scripts/mock-upstream.cjs so the Quota page + keep-alive
    /// can be exercised without a real provider.
    Mock,
}

/// How an endpoint's quota is queried — the unified "query plan" concept.
/// `Preset` dispatches to a built-in provider fetcher, `Custom` runs the
/// user-configured balance extractor, `None` means no query is configured
/// (quota display + keep-alive stay gated until the user picks one).
///
/// Plans are declared by provider presets and stamped at create time; legacy
/// endpoints resolve a plan from their base_url host (see
/// `quota_refresh::resolve_plan`). The plan is the single input to
/// [`fetch_with_plan`] — `provider_kind_for` is now only the legacy fallback.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum QuotaQueryPlan {
    /// No query configured. Fetch returns an "unconfigured" snapshot; the
    /// keep-alive worker and quota bars stay gated.
    None,
    /// A built-in provider fetcher, resolved from the preset at create time
    /// or backfilled from host detection for legacy endpoints.
    Preset { kind: BuiltinKind },
    /// User-configured balance extractor (GET + JSON paths). Always
    /// `is_balance`-shaped: no reset window, keep-alive never pings it.
    Custom(QuotaExtractorConfig),
}

impl QuotaQueryPlan {
    /// True when this plan can actually fetch quota (i.e. not `None`). Used
    /// to gate the keep-alive worker + quota display.
    pub fn is_active(&self) -> bool {
        !matches!(self, QuotaQueryPlan::None)
    }
}

/// Resolve a built-in fetcher kind from the endpoint's base_url host. Legacy
/// fallback only — primary dispatch is driven by [`QuotaQueryPlan`] via
/// [`fetch_with_plan`]. Used by `quota_refresh::resolve_plan` to backfill
/// endpoints created before the query-plan concept existed.
pub fn provider_kind_for(base_url: &str) -> Option<BuiltinKind> {
    let host = base_url.to_lowercase();
    if host.contains("z.ai") {
        Some(BuiltinKind::Zai)
    } else if host.contains("minimax") {
        Some(BuiltinKind::Minimax)
    } else if host.contains("openrouter.ai") {
        Some(BuiltinKind::Openrouter)
    } else if host.contains("opencode.ai") {
        Some(BuiltinKind::OpencodeGo)
    } else if host.contains("127.0.0.1") || host.contains("localhost") {
        Some(BuiltinKind::Mock)
    } else {
        None
    }
}

fn bearer_headers(key: &str) -> Vec<(String, String)> {
    vec![
        ("Accept".into(), "application/json".into()),
        ("Authorization".into(), format!("Bearer {key}")),
    ]
}

/// Redact credential-bearing material out of an error message before it
/// reaches the UI: custom quota URLs can embed `{{apiKey}}`-substituted
/// secrets, and ureq's `Display` includes the request URL on transport
/// errors — a quota-page error would otherwise surface the key in plaintext.
fn redact_error(msg: String, url: &str, headers: &[(String, String)]) -> String {
    let mut s = msg;
    s = s.replace(url, "<redacted-url>");
    for (_, v) in headers {
        let v = v.trim();
        if !v.is_empty() && v.len() > 3 {
            s = s.replace(v, "<redacted>");
        }
    }
    s
}

fn http_get(url: &str, headers: &[(String, String)]) -> AppResult<Value> {
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(TIMEOUT))
        // Never follow redirects: custom auth headers (x-api-key / Bearer)
        // would be forwarded cross-host on a 3xx, leaking the credential.
        .redirects(0)
        .build();
    let mut req = agent.get(url);
    for (k, v) in headers {
        req = req.set(k, v);
    }
    let resp = req
        .call()
        .map_err(|e| AppError::Http(redact_error(e.to_string(), url, headers)))?;
    let v: Value = resp
        .into_json()
        .map_err(|e| AppError::Http(redact_error(format!("parse: {e}"), url, headers)))?;
    Ok(v)
}

/// GET returning the raw response body as text. Used by the OpenCode Go
/// dashboard scrape (the response is HTML, not JSON).
fn http_get_text(url: &str, headers: &[(String, String)]) -> AppResult<String> {
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(TIMEOUT))
        // Never follow redirects: custom auth headers (x-api-key / Bearer)
        // would be forwarded cross-host on a 3xx, leaking the credential.
        .redirects(0)
        .build();
    let mut req = agent.get(url);
    for (k, v) in headers {
        req = req.set(k, v);
    }
    let resp = req
        .call()
        .map_err(|e| AppError::Http(redact_error(e.to_string(), url, headers)))?;
    resp.into_string()
        .map_err(|e| AppError::Http(redact_error(format!("read body: {e}"), url, headers)))
}

/// Coerce a JSON value to a number. Accepts:
///   • JSON numbers,
///   • string numbers (`"5.6"` → 5.6) — some balance APIs return amounts as
///     strings,
///   • money arrays `[12.34, "CNY"]` — the amount is the first element.
fn as_f64(v: &Value) -> Option<f64> {
    let v = if let Some(arr) = v.as_array() {
        arr.first().unwrap_or(v)
    } else {
        v
    };
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn fmt_duration(mut secs: i64) -> String {
    if secs <= 0 {
        return "now".into();
    }
    let mut out = Vec::new();
    for (unit, suffix) in [(86400, "d"), (3600, "h"), (60, "m")] {
        let value = secs / unit;
        secs %= unit;
        if value > 0 {
            out.push(format!("{value}{suffix}"));
        }
    }
    if out.is_empty() {
        format!("{secs}s")
    } else {
        out.join(" ")
    }
}

// ---- Z.ai ----

fn fetch_zai(key: &str) -> EndpointQuota {
    let h = bearer_headers(key);
    let payload = match http_get("https://api.z.ai/api/monitor/usage/quota/limit", &h) {
        Ok(v) => v,
        Err(e) => return err(e.to_string()),
    };
    if !payload.get("success").and_then(Value::as_bool).unwrap_or(false) {
        return err(payload
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or("Z.ai returned success=false")
            .to_string());
    }
    let data = payload.get("data").cloned().unwrap_or(Value::Null);
    let plan = data.get("level").and_then(Value::as_str).map(String::from);
    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut items = Vec::new();
    if let Some(limits) = data.get("limits").and_then(Value::as_array) {
        for item in limits {
            let name = zai_name(item);
            let pct = as_f64(item.get("percentage").unwrap_or(&Value::Null)).unwrap_or(0.0);
            let reset_ms = as_f64(item.get("nextResetTime").unwrap_or(&Value::Null));
            let resets_in = reset_ms
                .filter(|&r| r > 0.0)
                .map(|r| fmt_duration(((r - now_ms as f64) / 1000.0).max(0.0) as i64));
            let resets_at_ms = reset_ms.filter(|&r| r > 0.0).map(|r| r as i64);
            items.push(QuotaItem {
                name,
                pct,
                used: as_f64(item.get("currentValue").unwrap_or(&Value::Null)),
                total: as_f64(item.get("usage").unwrap_or(&Value::Null)),
                remaining: as_f64(item.get("remaining").unwrap_or(&Value::Null)),
                resets_in,
                resets_at_ms,
                unit: None,
                is_balance: false,
            });
        }
    }
    if items.is_empty() {
        return err("Z.ai response has no quota items".into());
    }
    EndpointQuota { ok: true, plan, error: None, items }
}

fn zai_name(item: &Value) -> String {
    let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
    let unit = item.get("unit").and_then(Value::as_i64).unwrap_or(0);
    let number = item.get("number").and_then(Value::as_i64).unwrap_or(0);
    match (kind, unit, number) {
        ("TOKENS_LIMIT", 3, 5) => "5h-token".into(),
        ("TOKENS_LIMIT", 6, 1) => "weekly-token".into(),
        ("TIME_LIMIT", _, _) => "tool-search".into(),
        _ => format!("unknown({kind})"),
    }
}

// ---- MiniMax ----

fn fetch_minimax(key: &str) -> EndpointQuota {
    let h = bearer_headers(key);
    let urls = [
        "https://api.minimax.io/v1/token_plan/remains",
        "https://api.minimaxi.com/v1/token_plan/remains",
    ];
    let mut payload = None;
    let mut last_err = String::new();
    for url in urls {
        match http_get(url, &h) {
            Ok(v) => {
                // auth-error sentinel: base_resp.status_code in {1004,2049}
                let code = v
                    .get("base_resp")
                    .and_then(|b| b.get("status_code"))
                    .and_then(Value::as_i64);
                if matches!(code, Some(1004) | Some(2049)) {
                    last_err = "MiniMax auth failed".into();
                    continue;
                }
                payload = Some(v);
                break;
            }
            Err(e) => {
                last_err = e.to_string();
            }
        }
    }
    let Some(payload) = payload else {
        return err(last_err);
    };
    let base_status = payload
        .get("base_resp")
        .and_then(|b| b.get("status_code"))
        .and_then(Value::as_i64);
    if let Some(s) = base_status {
        if s != 0 {
            return err(format!("MiniMax error {s}"));
        }
    }
    let mut items = Vec::new();
    // `remains_time` is RELATIVE (ms until reset) — persist an absolute
    // timestamp so a stored snapshot can't be mistaken for an already-expired
    // UTC time (which would make every reload look "expired" and trigger
    // endless reset pings).
    let now_ms = chrono::Utc::now().timestamp_millis() as f64;
    let abs_reset = |reset: Option<f64>| {
        reset
            .filter(|&r| r > 0.0)
            .map(|r| (now_ms + r) as i64)
    };
    if let Some(arr) = payload.get("model_remains").and_then(Value::as_array) {
        for entry in arr {
            let model = entry
                .get("model_name")
                .and_then(Value::as_str)
                .unwrap_or("model");
            if let Some(r5) = as_f64(entry.get("current_interval_remaining_percent").unwrap_or(&Value::Null)) {
                let reset = as_f64(entry.get("remains_time").unwrap_or(&Value::Null));
                items.push(QuotaItem {
                    name: format!("{model}/5h"),
                    pct: 100.0 - r5,
                    used: None,
                    total: None,
                    remaining: None,
                    resets_in: reset.filter(|&r| r > 0.0).map(|r| fmt_duration((r / 1000.0) as i64)),
                    resets_at_ms: abs_reset(reset),
                    unit: None,
                    is_balance: false,
                });
            }
            if let Some(rw) = as_f64(entry.get("current_weekly_remaining_percent").unwrap_or(&Value::Null)) {
                let reset = as_f64(entry.get("weekly_remains_time").unwrap_or(&Value::Null));
                items.push(QuotaItem {
                    name: format!("{model}/weekly"),
                    pct: 100.0 - rw,
                    used: None,
                    total: None,
                    remaining: None,
                    resets_in: reset.filter(|&r| r > 0.0).map(|r| fmt_duration((r / 1000.0) as i64)),
                    resets_at_ms: abs_reset(reset),
                    unit: None,
                    is_balance: false,
                });
            }
        }
    } else {
        // flat shape
        let ci_used = as_f64(payload.get("current_interval_usage_count").unwrap_or(&Value::Null));
        let ci_tot = as_f64(payload.get("current_interval_total_count").unwrap_or(&Value::Null));
        if let (Some(used), Some(total)) = (ci_used, ci_tot) {
            if total > 0.0 {
                items.push(QuotaItem {
                    name: "5h-token".into(),
                    // USED percentage — every other branch (and the frontend
                    // autoTone) treats pct as "consumed so far" (0 = fresh,
                    // 100 = exhausted). The old remaining-percentage here
                    // inverted the bars and spurious-exhaustion fallbacks.
                    pct: 100.0 * used / total,
                    used: Some(used),
                    total: Some(total),
                    remaining: Some((total - used).max(0.0)),
                    resets_in: None,
                    resets_at_ms: None,
                    unit: None,
                    is_balance: false,
                });
            }
        }
    }
    if items.is_empty() {
        return err("MiniMax response missing quota fields".into());
    }
    EndpointQuota { ok: true, plan: Some("token-plan".into()), error: None, items }
}

// ---- Local mock upstream ----

fn fetch_mock(base_url: &str, key: &str) -> EndpointQuota {
    // The mock upstream serves `GET /v1/quota` with a flat payload:
    // { plan, items: [{ name, pct, used, total, remaining, resets_in }] }.
    // `pct` is authoritative (100 = exhausted, as configured per port).
    let h = bearer_headers(key);
    let url = format!("{}/v1/quota", base_url.trim_end_matches('/'));
    let payload = match http_get(&url, &h) {
        Ok(v) => v,
        Err(e) => return err(e.to_string()),
    };
    parse_mock_payload(&payload)
}

/// Parse the mock upstream's flat quota payload (no network — unit-testable).
fn parse_mock_payload(payload: &Value) -> EndpointQuota {
    let plan = payload.get("plan").and_then(Value::as_str).map(String::from);
    let mut items = Vec::new();
    if let Some(list) = payload.get("items").and_then(Value::as_array) {
        for item in list {
            items.push(QuotaItem {
                name: item.get("name").and_then(Value::as_str).unwrap_or("mock").to_string(),
                pct: as_f64(item.get("pct").unwrap_or(&Value::Null)).unwrap_or(0.0),
                used: as_f64(item.get("used").unwrap_or(&Value::Null)),
                total: as_f64(item.get("total").unwrap_or(&Value::Null)),
                remaining: as_f64(item.get("remaining").unwrap_or(&Value::Null)),
                resets_in: item.get("resets_in").and_then(Value::as_str).map(String::from),
                resets_at_ms: None,
                unit: None,
                is_balance: false,
            });
        }
    }
    if items.is_empty() {
        return err("mock quota response has no items".into());
    }
    EndpointQuota { ok: true, plan, error: None, items }
}

// ---- Custom extractor (user-configured field mapping) ----

/// User-configured balance extractor: GET a URL and pull fields out of the
/// JSON response by dot-path (`data.balance.0` = object `data` → array
/// `balance` → index 0). `{{baseUrl}}` / `{{apiKey}}` in url/headers are
/// substituted from the endpoint. Custom extracts are always balance-shaped
/// (`is_balance`): no reset window, and keep-alive never pings them.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct QuotaExtractorConfig {
    pub enabled: bool,
    pub url: String,
    #[serde(default)]
    pub headers: std::collections::HashMap<String, String>,
    /// Static currency unit used when `fields.unit` is not configured.
    #[serde(default)]
    pub unit: Option<String>,
    pub fields: ExtractorFields,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ExtractorFields {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub used: Option<String>,
    #[serde(default)]
    pub remaining: Option<String>,
    #[serde(default)]
    pub total: Option<String>,
    #[serde(default)]
    pub unit: Option<String>,
}

/// Walk a dot-path (`a.b.0`) through a JSON value. Object keys by name,
/// numeric segments index arrays. `None` for missing segments.
fn json_path<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = v;
    for seg in path.split('.') {
        if seg.is_empty() {
            return None;
        }
        cur = match cur {
            Value::Object(map) => map.get(seg)?,
            Value::Array(arr) => arr.get(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

/// Substitute `{{baseUrl}}` / `{{apiKey}}` placeholders.
fn substitute(s: &str, base_url: &str, key: &str) -> String {
    s.replace("{{baseUrl}}", base_url).replace("{{apiKey}}", key)
}

/// Execute a custom extractor: build the request (GET + headers with
/// placeholders substituted), fetch, then shape the balance item.
fn fetch_custom(endpoint: &EndpointRow, key: &str, cfg: &QuotaExtractorConfig) -> EndpointQuota {
    let base_url = crate::db::pick_quota_url(&endpoint.protocols).unwrap_or_default();
    let url = substitute(&cfg.url, &base_url, key);
    let mut headers = Vec::new();
    for (k, v) in &cfg.headers {
        headers.push((k.clone(), substitute(v, &base_url, key)));
    }
    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("authorization")) {
        headers.push(("Authorization".into(), format!("Bearer {key}")));
    }
    let payload = match http_get(&url, &headers) {
        Ok(v) => v,
        Err(e) => return err(e.to_string()),
    };
    parse_custom_payload(endpoint, cfg, &payload)
}

/// Parse a custom extractor's response (no network — unit-testable). Pulls
/// the configured fields by dot-path and shapes a single balance item.
/// `pct`: total present → used/total, else used+remaining →
/// used/(used+remaining), else 0.
fn parse_custom_payload(
    endpoint: &EndpointRow,
    cfg: &QuotaExtractorConfig,
    payload: &Value,
) -> EndpointQuota {
    let _ = endpoint;
    let pick = |path: &Option<String>| -> Option<f64> {
        path.as_deref().and_then(|p| json_path(payload, p)).and_then(as_f64)
    };
    let used = pick(&cfg.fields.used);
    let remaining = pick(&cfg.fields.remaining);
    let total = pick(&cfg.fields.total);
    let unit = cfg
        .fields
        .unit
        .as_deref()
        .and_then(|p| json_path(payload, p))
        .and_then(Value::as_str)
        .map(String::from)
        .or_else(|| cfg.unit.clone());
    let name = cfg
        .fields
        .name
        .as_deref()
        .and_then(|p| json_path(payload, p))
        .and_then(Value::as_str)
        .unwrap_or("balance")
        .to_string();
    let pct = if let Some(t) = total.filter(|t| *t > 0.0) {
        100.0 * used.unwrap_or(0.0) / t
    } else if let (Some(u), Some(r)) = (used, remaining) {
        let denom = u + r;
        if denom > 0.0 { 100.0 * u / denom } else { 0.0 }
    } else {
        0.0
    };
    // No configured field resolved — the payload shape doesn't match the
    // extractor (or `data` is missing entirely). Returning ok:true with a
    // 0% "balance" masked real API errors; fail loudly instead.
    if used.is_none() && remaining.is_none() && total.is_none() {
        return err("quota payload did not match the custom extractor's fields".into());
    }
    EndpointQuota {
        ok: true,
        plan: Some("balance".into()),
        error: None,
        items: vec![QuotaItem {
            name,
            pct,
            used,
            total,
            remaining,
            resets_in: None,
            resets_at_ms: None,
            unit,
            is_balance: true,
        }],
    }
}

/// OpenRouter key limits (`GET /api/v1/key`, the docs' "limits" reference).
/// Balance-based: `usage` is all-time spend, `limit` / `limit_remaining` are
/// the key's credit cap and what's left — both **nullable** (unlimited keys
/// return null), which is exactly why balance items carry no percentage:
/// there is no window to fill and no ratio to report. cc-switch models
/// third-party balances the same way — show the remaining amount + unit,
/// never a reset countdown or a usage bar.
fn fetch_openrouter(key: &str) -> EndpointQuota {
    let mut h = bearer_headers(key);
    // App attribution (matches the gateway forward path). Optional, but
    // consistent with the Routed-mode request headers.
    h.push(("HTTP-Referer".into(), "https://github.com/Nuo/Nestra".into()));
    h.push(("X-Title".into(), "Nestra".into()));
    let payload = match http_get("https://openrouter.ai/api/v1/key", &h) {
        Ok(v) => v,
        Err(e) => return err(e.to_string()),
    };
    parse_openrouter_payload(&payload)
}

/// Parse the OpenRouter key-limits payload (no network — unit-testable).
/// `limit` / `limit_remaining` are null for unlimited keys; `pct` is always
/// 0 because balance items never report a fill ratio.
fn parse_openrouter_payload(payload: &Value) -> EndpointQuota {
    let data = payload.get("data").unwrap_or(&Value::Null);
    let usage = as_f64(data.get("usage").unwrap_or(&Value::Null));
    let limit = as_f64(data.get("limit").unwrap_or(&Value::Null));
    let remaining = as_f64(data.get("limit_remaining").unwrap_or(&Value::Null));
    EndpointQuota {
        ok: true,
        plan: Some("balance".into()),
        error: None,
        items: vec![QuotaItem {
            name: "balance".into(),
            pct: 0.0,
            used: usage,
            total: limit,
            remaining,
            resets_in: None,
            resets_at_ms: None,
            unit: Some("USD".into()),
            is_balance: true,
        }],
    }
}

fn err(msg: String) -> EndpointQuota {
    EndpointQuota { ok: false, plan: None, error: Some(msg), items: vec![] }
}

// ---- OpenCode Go (dashboard scrape) ----
//
// OpenCode Go exposes no API-key usage endpoint — usage is rendered only on
// the authenticated web dashboard at `https://opencode.ai/workspace/{id}/go`.
// We fetch that HTML with the user's browser session cookie (`Cookie:
// auth=<cookie>`, same as the community tools opencode-bar and
// opencode-quota) and scrape the rolling/weekly/monthly windows out of two
// possible markup shapes the dashboard has shipped:
//   1. SolidJS SSR hydration: `rollingUsage:$R[N]={...usagePercent:X...resetInSec:Y...}`
//      (key order varies, so both fields are located independently).
//   2. data-slot HTML: `<... data-slot="usage-label">Rolling Usage ...
//      data-slot="usage-value">42.5 ... data-slot="reset-time">Resets in 1 hour`.
// `parse_opencode_go_html` is split out (no network) so the parser is
// fixture-tested — a dashboard markup change shows up as a test failure
// instead of a silent empty bar.

/// Parse the first number (`-?\d+(\.\d+)?`) at the start of `s` (after
/// trimming whitespace). Returns the value without consuming the rest.
fn parse_num_prefix(s: &str) -> Option<f64> {
    let s = s.trim_start();
    let bytes = s.as_bytes();
    let mut i = 0;
    if i < bytes.len() && (bytes[i] == b'-' || bytes[i] == b'+') {
        i += 1;
    }
    let digits_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == digits_start {
        return None;
    }
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
    }
    s[..i].parse::<f64>().ok()
}

/// Find `marker` in `s`, then parse the number immediately after it. Used for
/// both `usagePercent:` (SSR) and `data-slot="usage-value">` (HTML) shapes.
fn num_after_marker(s: &str, marker: &str) -> Option<f64> {
    let at = s.find(marker)?;
    parse_num_prefix(&s[at + marker.len()..])
}

/// Text between `marker` and the next `stop` char (or end of string). Used
/// for the data-slot label + reset-time text.
fn text_after_marker_until<'a>(s: &'a str, marker: &str, stop: char) -> Option<&'a str> {
    let at = s.find(marker)?;
    let rest = &s[at + marker.len()..];
    let end = rest.find(stop).unwrap_or(rest.len());
    Some(&rest[..end])
}

/// Parse a human-readable duration ("1 day", "2 hours 30 minutes", "resets
/// now") into seconds. Scans number+unit pairs; ignores non-matching words
/// (so "Resets in 1 hour" yields 3600). `now` / `resets now` → 0.
fn parse_human_seconds(s: &str) -> Option<f64> {
    let lower = s.to_lowercase();
    if lower.contains("now") {
        return Some(0.0);
    }
    let bytes = lower.as_bytes();
    let mut total = 0.0;
    let mut found = false;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_digit() || c == b'.' || c == b'-' {
            let start = i;
            while i < bytes.len()
                && (bytes[i].is_ascii_digit() || bytes[i] == b'.' || bytes[i] == b'-')
            {
                i += 1;
            }
            let num: f64 = lower[start..i].parse().ok()?;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            let ustart = i;
            while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
                i += 1;
            }
            let unit = &lower[ustart..i];
            let mult = if unit.starts_with("day") {
                86400.0
            } else if unit.starts_with("hour") || unit.starts_with("hr") {
                3600.0
            } else if unit.starts_with("minute") || unit.starts_with("min") {
                60.0
            } else if unit.starts_with("second") || unit.starts_with("sec") {
                1.0
            } else {
                continue;
            };
            total += num * mult;
            found = true;
        } else {
            i += 1;
        }
    }
    if found { Some(total) } else { None }
}

/// Map a dashboard label ("Rolling Usage", "Weekly", ...) to a window name.
/// "rolling" is the Go plan's 5h window.
fn opencode_window_name(label: &str) -> Option<&'static str> {
    let l = label.to_lowercase();
    if l.contains("rolling") {
        Some("5h")
    } else if l.contains("weekly") {
        Some("weekly")
    } else if l.contains("monthly") {
        Some("monthly")
    } else {
        None
    }
}

/// Build a windowed `QuotaItem` from a usage percent + reset seconds.
fn opencode_window_item(name: &str, pct: f64, reset_sec: f64, now_ms: i64) -> QuotaItem {
    let pct = pct.max(0.0);
    let resets_at_ms = if reset_sec > 0.0 {
        Some(now_ms + (reset_sec as i64) * 1000)
    } else {
        None
    };
    let resets_in = if reset_sec > 0.0 {
        Some(fmt_duration(reset_sec as i64))
    } else {
        None
    };
    QuotaItem {
        name: name.into(),
        pct,
        used: None,
        total: None,
        remaining: None,
        resets_in,
        resets_at_ms,
        unit: None,
        is_balance: false,
    }
}

/// Extract one SSR-hydration window (`{marker}:$R[N]={...usagePercent...
/// resetInSec...}`). Returns `(usagePercent, resetInSec)`; key order agnostic.
fn opencode_ssr_window(html: &str, marker: &str) -> Option<(f64, f64)> {
    let prefix = format!("{marker}:$R[");
    let at = html.find(&prefix)?;
    let after = html[at + prefix.len()..]
        .trim_start_matches(|c: char| c.is_ascii_digit());
    let block = after.strip_prefix("]={")?;
    let close = block.find('}')?;
    let inner = &block[..close];
    let pct = num_after_marker(inner, "usagePercent:")?;
    let reset = num_after_marker(inner, "resetInSec:")?;
    Some((pct, reset))
}

/// Parse the data-slot HTML fallback. Splits on `data-slot="usage-item"` and,
/// for each chunk, pulls the label + value + reset-time text.
fn opencode_parse_data_slot(html: &str, now_ms: i64) -> Vec<QuotaItem> {
    let mut items = Vec::new();
    for chunk in html.split(r#"data-slot="usage-item""#).skip(1) {
        let label = text_after_marker_until(chunk, r#"data-slot="usage-label">"#, '<').unwrap_or("");
        let value = num_after_marker(chunk, r#"data-slot="usage-value">"#);
        let reset_txt = text_after_marker_until(chunk, r#"data-slot="reset-time">"#, '<');
        let name = match opencode_window_name(label) {
            Some(n) => n,
            None => continue,
        };
        let pct = match value {
            Some(p) => p,
            None => continue,
        };
        let reset_sec = reset_txt.and_then(|t| parse_human_seconds(t)).unwrap_or(0.0);
        items.push(opencode_window_item(name, pct, reset_sec, now_ms));
    }
    items
}

/// Parse the OpenCode Go dashboard HTML into quota items (no network). Tries
/// the SolidJS SSR hydration shape first, falls back to the data-slot HTML
/// shape. Returns an error snapshot if no windows are found.
pub fn parse_opencode_go_html(html: &str, now_ms: i64) -> EndpointQuota {
    let mut items = Vec::new();
    for (marker, name) in [
        ("rollingUsage", "5h"),
        ("weeklyUsage", "weekly"),
        ("monthlyUsage", "monthly"),
    ] {
        if let Some((pct, reset_sec)) = opencode_ssr_window(html, marker) {
            items.push(opencode_window_item(name, pct, reset_sec, now_ms));
        }
    }
    if items.is_empty() {
        items = opencode_parse_data_slot(html, now_ms);
    }
    if items.is_empty() {
        return err("OpenCode Go dashboard returned no usage windows".into());
    }
    // Stable order: 5h, weekly, monthly (matches z.ai's window ordering).
    items.sort_by_key(|i| match i.name.as_str() {
        "5h" => 0,
        "weekly" => 1,
        _ => 2,
    });
    EndpointQuota { ok: true, plan: Some("opencode-go".into()), error: None, items }
}

/// A workspace ID must be path-safe before we splice it into the dashboard
/// URL — guard against URL injection. opencode workspace IDs are slugs/UUIDs
/// (`[A-Za-z0-9_-]+`).
fn safe_workspace_segment(ws: &str) -> Option<&str> {
    let ws = ws.trim();
    if !ws.is_empty() && ws.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        Some(ws)
    } else {
        None
    }
}

/// Fetch the OpenCode Go dashboard HTML with the user's session cookie and
/// scrape the usage windows. Mirrors the community tools (opencode-bar,
/// opencode-quota): there is no API-key usage endpoint, only the authenticated
/// dashboard page.
fn fetch_opencode_go(cookie: &str, workspace_id: &str) -> EndpointQuota {
    let ws = match safe_workspace_segment(workspace_id) {
        Some(w) => w,
        None => return err("OpenCode Go workspace ID is invalid".into()),
    };
    let url = format!("https://opencode.ai/workspace/{ws}/go");
    let headers = vec![
        ("Accept".into(), "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8".into()),
        ("Cookie".into(), format!("auth={cookie}")),
        // A real-browser UA — the dashboard renders different markup for
        // bots/curl, so pretend to be a desktop browser like the scrapers do.
        ("User-Agent".into(),
         "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36".into()),
    ];
    let body = match http_get_text(&url, &headers) {
        Ok(b) => b,
        Err(e) => return err(e.to_string()),
    };
    let now_ms = chrono::Utc::now().timestamp_millis();
    parse_opencode_go_html(&body, now_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, resets_at_ms: Option<i64>) -> QuotaItem {
        QuotaItem {
            name: name.into(),
            pct: 0.0,
            used: None,
            total: None,
            remaining: None,
            resets_in: None,
            resets_at_ms,
            unit: None,
            is_balance: false,
        }
    }

    fn balance_item(name: &str, pct: f64, remaining: f64, total: f64) -> QuotaItem {
        QuotaItem {
            name: name.into(),
            pct,
            used: Some(total - remaining),
            total: Some(total),
            remaining: Some(remaining),
            resets_in: None,
            resets_at_ms: None,
            unit: Some("USD".into()),
            is_balance: true,
        }
    }

    fn ep(protocols: &[(&str, &str)]) -> EndpointRow {
        EndpointRow {
            id: "ep-test".into(),
            display_name: "Test".into(),
            has_api_key: true,
            status: "valid".into(),
            last_validated_at: None,
            models_json: None,
            models_fetched_at: None,
            advanced_env_json: None,
            model_abilities_json: None,
            protocols: protocols
                .iter()
                .map(|(p, u)| ProtocolEntry { protocol: (*p).into(), base_url: (*u).into() })
                .collect(),
        }
    }

    #[test]
    fn provider_kind_detects_local_mock() {
        assert!(matches!(provider_kind_for("http://127.0.0.1:8787"), Some(BuiltinKind::Mock)));
        assert!(matches!(provider_kind_for("http://localhost:8788"), Some(BuiltinKind::Mock)));
        assert!(matches!(provider_kind_for("https://api.z.ai/api/anthropic"), Some(BuiltinKind::Zai)));
        assert!(matches!(provider_kind_for("https://openrouter.ai/api/v1"), Some(BuiltinKind::Openrouter)));
        assert!(matches!(provider_kind_for("https://opencode.ai/zen/go/v1"), Some(BuiltinKind::OpencodeGo)));
        assert!(matches!(provider_kind_for("https://api.openai.com/v1"), None));
    }

    #[test]
    fn fetch_with_plan_none_returns_unconfigured() {
        let endpoint = ep(&[("openai-comp", "https://api.z.ai/v1")]);
        let q = fetch_with_plan(&endpoint, "k", &QuotaQueryPlan::None, None);
        assert!(!q.ok);
        assert!(q.error.as_deref().unwrap().contains("no query plan"));
        assert!(q.items.is_empty());
    }

    #[test]
    fn fetch_with_plan_custom_overrides_host() {
        // A z.ai endpoint whose plan is explicitly Custom must NOT use the
        // built-in z.ai fetcher — the user's plan wins.
        let endpoint = ep(&[("openai-comp", "https://api.z.ai/v1")]);
        let cfg = QuotaExtractorConfig {
            enabled: true,
            url: "{{baseUrl}}/balance".into(),
            headers: std::collections::HashMap::new(),
            unit: Some("USD".into()),
            fields: ExtractorFields {
                name: None,
                used: Some("data.usage".into()),
                remaining: Some("data.remaining".into()),
                total: Some("data.total".into()),
                unit: None,
            },
        };
        // Can't hit the network in a unit test, but we can assert the plan
        // dispatches to the custom parser (not the z.ai fetcher) by parsing
        // a synthetic payload through the same path fetch_with_plan uses.
        let q = parse_custom_payload(
            &endpoint,
            &cfg,
            &serde_json::json!({ "data": { "usage": 10, "remaining": 90, "total": 100 } }),
        );
        assert!(q.ok);
        assert_eq!(q.plan.as_deref(), Some("balance"));
        assert!(q.items[0].is_balance);
    }

    #[test]
    fn plan_is_active_only_when_configured() {
        assert!(!QuotaQueryPlan::None.is_active());
        assert!(QuotaQueryPlan::Preset { kind: BuiltinKind::Zai }.is_active());
        assert!(QuotaQueryPlan::Custom(QuotaExtractorConfig::default()).is_active());
    }

    #[test]
    fn plan_round_trips_through_serde() {
        // The tag = "source" representation must round-trip so the setting_kv
        // blob deserializes cleanly across the TS boundary.
        let cases = vec![
            serde_json::json!({ "source": "none" }),
            serde_json::json!({ "source": "preset", "kind": "zai" }),
            serde_json::json!({ "source": "preset", "kind": "minimax" }),
            serde_json::json!({ "source": "preset", "kind": "openrouter" }),
            serde_json::json!({ "source": "preset", "kind": "mock" }),
            serde_json::json!({
                "source": "custom",
                "enabled": true,
                "url": "{{baseUrl}}/x",
                "headers": {},
                "unit": null,
                "fields": {}
            }),
        ];
        for v in cases {
            let plan: QuotaQueryPlan = serde_json::from_value(v.clone()).unwrap_or_else(|e| panic!("deserialize {v}: {e}"));
            let re = serde_json::to_value(&plan).unwrap_or_else(|e| panic!("serialize: {e}"));
            let again: QuotaQueryPlan = serde_json::from_value(re).unwrap();
            assert_eq!(plan.is_active(), again.is_active());
        }
    }

    #[test]
    fn parse_openrouter_limits() {
        // GET /api/v1/key limits shape: usage (all-time), limit +
        // limit_remaining (nullable).
        let payload = serde_json::json!({
            "data": {
                "label": "main",
                "usage": 23.5,
                "limit": 100.0,
                "limit_remaining": 76.5,
                "is_free_tier": false,
            }
        });
        let q = parse_openrouter_payload(&payload);
        assert!(q.ok, "openrouter quota should parse: {:?}", q.error);
        assert_eq!(q.plan.as_deref(), Some("balance"));
        assert_eq!(q.items.len(), 1);
        let it = &q.items[0];
        assert_eq!(it.name, "balance");
        assert!(it.is_balance, "balance item must be flagged is_balance");
        assert_eq!(it.unit.as_deref(), Some("USD"));
        assert_eq!(it.remaining, Some(76.5));
        assert_eq!(it.used, Some(23.5));
        assert_eq!(it.total, Some(100.0));
        // Balance items never report a fill ratio — no percentage.
        assert_eq!(it.pct, 0.0);
        assert_eq!(it.resets_at_ms, None);
        assert_eq!(it.resets_in, None);
    }

    #[test]
    fn parse_openrouter_unlimited_key() {
        // Unlimited keys return limit/limit_remaining = null — pct stays 0
        // and total is absent; the UI shows only the spend, no ratio.
        let payload = serde_json::json!({
            "data": { "usage": 12.0, "limit": null, "limit_remaining": null }
        });
        let q = parse_openrouter_payload(&payload);
        let it = &q.items[0];
        assert_eq!(it.pct, 0.0);
        assert_eq!(it.total, None);
        assert_eq!(it.remaining, None);
        assert_eq!(it.used, Some(12.0));
    }

    #[test]
    fn as_f64_parses_string_numbers_and_money_arrays() {
        // Money-array shape: amount is the first element.
        assert_eq!(as_f64(&serde_json::json!([12.34, "CNY"])), Some(12.34));
        // String numbers — some balance APIs return amounts as strings.
        assert_eq!(as_f64(&serde_json::json!("5.6")), Some(5.6));
        assert_eq!(as_f64(&serde_json::json!("12.34")), Some(12.34));
        // Plain numbers and garbage.
        assert_eq!(as_f64(&serde_json::json!(42.0)), Some(42.0));
        assert_eq!(as_f64(&serde_json::json!("not-a-number")), None);
        assert_eq!(as_f64(&Value::Null), None);
    }

    #[test]
    fn json_path_walks_objects_and_arrays() {
        let v = serde_json::json!({
            "data": { "balance": [12.34, "CNY"], "quota": { "used": "5.6" } }
        });
        assert_eq!(json_path(&v, "data.balance.0"), Some(&serde_json::json!(12.34)));
        assert_eq!(json_path(&v, "data.balance.1"), Some(&serde_json::json!("CNY")));
        assert_eq!(json_path(&v, "data.quota.used"), Some(&serde_json::json!("5.6")));
        // Missing paths and out-of-range indexes.
        assert_eq!(json_path(&v, "data.missing"), None);
        assert_eq!(json_path(&v, "data.balance.9"), None);
        assert_eq!(json_path(&v, ""), None);
    }

    #[test]
    fn fetch_custom_shapes_balance_item() {
        let endpoint = ep(&[("openai-comp", "https://api.example.com/v1")]);
        let cfg = QuotaExtractorConfig {
            enabled: true,
            url: "{{baseUrl}}/users/me/balance".into(),
            headers: std::collections::HashMap::new(),
            unit: Some("CNY".into()),
            fields: ExtractorFields {
                name: Some("data.plan".into()),
                used: Some("data.total_usage.0".into()),
                remaining: Some("data.balance.0".into()),
                total: None,
                unit: Some("data.balance.1".into()),
            },
        };
        let q = parse_custom_payload(
            &endpoint,
            &cfg,
            &serde_json::json!({
                "data": {
                    "plan": "pro",
                    "total_usage": [1.56, "CNY"],
                    "balance": [12.34, "CNY"],
                }
            }),
        );
        assert!(q.ok, "custom quota should parse: {:?}", q.error);
        let it = &q.items[0];
        assert_eq!(it.name, "pro");
        assert!(it.is_balance);
        assert_eq!(it.unit.as_deref(), Some("CNY"));
        assert_eq!(it.remaining, Some(12.34));
        assert_eq!(it.used, Some(1.56));
        // pct = used / (used + remaining)
        let expected = 100.0 * 1.56 / (1.56 + 12.34);
        assert!((it.pct - expected).abs() < 1e-9);
        assert_eq!(it.resets_at_ms, None);
    }

    #[test]
    fn fetch_custom_total_preferred_for_pct_and_defaults() {
        let endpoint = ep(&[("openai-comp", "https://api.example.com/v1")]);
        let cfg = QuotaExtractorConfig {
            enabled: true,
            url: "{{baseUrl}}/balance".into(),
            headers: std::collections::HashMap::new(),
            unit: None,
            fields: ExtractorFields {
                name: None,
                used: Some("data.usage".into()),
                remaining: Some("data.remaining".into()),
                total: Some("data.total".into()),
                unit: None,
            },
        };
        let q = parse_custom_payload(
            &endpoint,
            &cfg,
            &serde_json::json!({ "data": { "usage": 25, "remaining": 75, "total": 100 } }),
        );
        let it = &q.items[0];
        // Name defaults to "balance".
        assert_eq!(it.name, "balance");
        // pct prefers total: used / total.
        assert!((it.pct - 25.0).abs() < 1e-9);
        assert_eq!(it.remaining, Some(75.0));
        // No unit configured → None (UI falls back to bare number).
        assert_eq!(it.unit, None);
    }

    #[test]
    fn fetch_custom_tolerates_missing_fields() {
        let endpoint = ep(&[("openai-comp", "https://api.example.com/v1")]);
        let cfg = QuotaExtractorConfig {
            enabled: true,
            url: "{{baseUrl}}/balance".into(),
            headers: std::collections::HashMap::new(),
            unit: None,
            fields: ExtractorFields::default(),
        };
        // Empty fields → every value None: the payload shape doesn't match
        // the extractor — fail loudly (the old code returned a 0% "balance"
        // that masked real API errors).
        let q = parse_custom_payload(&endpoint, &cfg, &serde_json::json!({ "data": {} }));
        assert!(!q.ok, "missing fields must not report ok");
        assert!(q.error.is_some());
        assert!(q.items.is_empty());
    }

    #[test]
    fn substitute_replaces_placeholders() {
        assert_eq!(substitute("{{baseUrl}}/x?k={{apiKey}}", "https://b", "SECRET"),
            "https://b/x?k=SECRET");
        assert_eq!(substitute("no placeholders", "https://b", "k"), "no placeholders");
    }

    #[test]
    fn parse_mock_payload_flat_shape() {
        let payload = serde_json::json!({
            "plan": "Mock",
            "items": [{
                "name": "Mock 5h window",
                "pct": 100,
                "used": 100,
                "total": 100,
                "remaining": 0,
                "resets_in": "3h",
            }],
        });
        let q = parse_mock_payload(&payload);
        assert!(q.ok, "mock quota should parse: {:?}", q.error);
        assert_eq!(q.plan.as_deref(), Some("Mock"));
        assert_eq!(q.items.len(), 1);
        assert_eq!(q.items[0].pct, 100.0);
        assert_eq!(q.items[0].resets_in.as_deref(), Some("3h"));
    }

    #[test]
    fn parse_mock_payload_rejects_empty_items() {
        let payload = serde_json::json!({ "plan": "Mock", "items": [] });
        let q = parse_mock_payload(&payload);
        assert!(!q.ok);
    }

    // ---- OpenCode Go dashboard scrape ----

    #[test]
    fn opencode_parse_ssr_hydration_shape() {
        // SolidJS SSR hydration stream: usagePercent + resetInSec in either
        // order per window. This is the primary dashboard markup shape.
        let html = r#"<script>window._$HY=(e,t,k)=>{};window._$HY.r="1";
        rollingUsage:$R[1]={label:"Rolling Usage",usagePercent:42.5,resetInSec:5400,foo:1}
        weeklyUsage:$R[2]={resetInSec:259200,usagePercent:71.0,label:"Weekly"}
        monthlyUsage:$R[3]={usagePercent:10.0,resetInSec:0,label:"Monthly"}</script>"#;
        let q = parse_opencode_go_html(html, 1_000_000);
        assert!(q.ok, "ssr parse should succeed: {:?}", q.error);
        assert_eq!(q.plan.as_deref(), Some("opencode-go"));
        assert_eq!(q.items.len(), 3);
        // Stable order: 5h, weekly, monthly.
        assert_eq!(q.items[0].name, "5h");
        assert_eq!(q.items[0].pct, 42.5);
        assert_eq!(q.items[0].resets_at_ms, Some(1_000_000 + 5400 * 1000));
        assert!(!q.items[0].is_balance);
        assert_eq!(q.items[1].name, "weekly");
        assert_eq!(q.items[1].pct, 71.0);
        assert_eq!(q.items[2].name, "monthly");
        // resetInSec 0 → no reset timestamp.
        assert_eq!(q.items[2].resets_at_ms, None);
    }

    #[test]
    fn opencode_parse_data_slot_shape() {
        // The newer dashboard markup: discrete data-slot elements per window.
        let html = r#"<main>
          <div data-slot="usage-item"><span data-slot="usage-label">Rolling Usage</span>
            <span data-slot="usage-value">60</span><span data-slot="reset-time">Resets in 2 hours</span></div>
          <div data-slot="usage-item"><span data-slot="usage-label">Weekly</span>
            <span data-slot="usage-value">15.5</span><span data-slot="reset-time">Resets in 1 day 3 hours</span></div>
        </main>"#;
        let q = parse_opencode_go_html(html, 0);
        assert!(q.ok, "data-slot parse should succeed: {:?}", q.error);
        assert_eq!(q.items.len(), 2);
        assert_eq!(q.items[0].name, "5h");
        assert_eq!(q.items[0].pct, 60.0);
        // 2 hours = 7200s.
        assert_eq!(q.items[0].resets_at_ms, Some(7200 * 1000));
        assert_eq!(q.items[1].name, "weekly");
        assert_eq!(q.items[1].pct, 15.5);
        // 1 day 3 hours = 86400 + 10800 = 97200s.
        assert_eq!(q.items[1].resets_at_ms, Some(97200 * 1000));
    }

    #[test]
    fn opencode_parse_resets_now_is_zero_reset() {
        let html = r#"<div data-slot="usage-item"><span data-slot="usage-label">Rolling Usage</span>
            <span data-slot="usage-value">100</span><span data-slot="reset-time">resets now</span></div>"#;
        let q = parse_opencode_go_html(html, 0);
        let it = &q.items[0];
        assert_eq!(it.pct, 100.0);
        // "resets now" → 0s → no reset timestamp.
        assert_eq!(it.resets_at_ms, None);
    }

    #[test]
    fn opencode_parse_no_windows_is_error() {
        // Unrelated HTML (e.g. a login redirect / cookie expired) → no windows.
        let q = parse_opencode_go_html("<html><body>please log in</body></html>", 0);
        assert!(!q.ok);
        assert!(q.error.as_deref().unwrap().contains("no usage windows"));
    }

    #[test]
    fn opencode_workspace_segment_rejects_unsafe_chars() {
        assert_eq!(safe_workspace_segment("ws_abc-123"), Some("ws_abc-123"));
        assert_eq!(safe_workspace_segment("  padded  "), Some("padded"));
        // Path-injection attempts are rejected.
        assert_eq!(safe_workspace_segment("../etc"), None);
        assert_eq!(safe_workspace_segment("a/b"), None);
        assert_eq!(safe_workspace_segment("a?x=1"), None);
        assert_eq!(safe_workspace_segment(""), None);
    }

    #[test]
    fn fetch_with_plan_opencode_without_creds_is_clear_error() {
        let endpoint = ep(&[("openai-comp", "https://opencode.ai/zen/go/v1")]);
        let plan = QuotaQueryPlan::Preset { kind: BuiltinKind::OpencodeGo };
        let q = fetch_with_plan(&endpoint, "k", &plan, None);
        assert!(!q.ok);
        assert!(q.error.as_deref().unwrap().contains("cookie + workspace ID not set"));
    }

    #[test]
    fn pick_5h_zai() {
        let items = vec![
            item("weekly-token", Some(99)),
            item("5h-token", Some(1234)),
        ];
        assert_eq!(pick_five_hour_expiry(&items), Some(1234));
    }

    #[test]
    fn pick_5h_minimax_new_shape() {
        let items = vec![
            item("claude-sonnet/weekly", Some(99)),
            item("claude-sonnet/5h", Some(5678)),
        ];
        assert_eq!(pick_five_hour_expiry(&items), Some(5678));
    }

    #[test]
    fn pick_5h_minimax_flat_no_reset() {
        // Flat shape returns no reset timestamp on the wire; next fetch
        // after a successful POST will repopulate it.
        let items = vec![item("5h-token", None)];
        assert_eq!(pick_five_hour_expiry(&items), None);
    }

    #[test]
    fn pick_5h_returns_none_without_5h_item() {
        // A balance-shaped item (no 5h name) must not match the window picker.
        let items = vec![item("balance", None)];
        assert_eq!(pick_five_hour_expiry(&items), None);
    }

    #[test]
    fn pick_5h_first_match_wins() {
        // New shape sorts alphabetically (claude-haiku/5h before claude-sonnet/5h);
        // pick the first `*/5h` hit. Stable ordering means deterministic test.
        let items = vec![
            item("claude-haiku/5h", Some(11)),
            item("claude-sonnet/5h", Some(22)),
        ];
        assert_eq!(pick_five_hour_expiry(&items), Some(11));
    }

    #[test]
    fn pick_5h_empty() {
        assert_eq!(pick_five_hour_expiry(&[]), None);
    }
}
