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
mod tests;
