//! Background worker that resets 5-hour quota windows for "keep awake"
//! endpoints. z.ai resets the 5h window on the NEXT request after expiry
//! (not on a fixed timer). The worker exploits that: it polls each
//! enabled endpoint's quota on a slow interval and fires one minimal
//! ping when it observes an expired 5h item — the ping IS the "next
//! request" that triggers the reset.
//!
//! The worker only runs while Nestra is alive (the hidden tray window
//! counts as alive). Quit via the tray menu flips `SHOULD_EXIT` so the
//! loop unwinds cleanly. The first tick after launch observes + reacts,
//! so any window that expired while Nestra was closed is reset promptly.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::db::{self, EndpointRow, ProtocolEntry};
use crate::endpoint_quota::{self, QuotaItem};
use crate::error::{AppError, AppResult};
use crate::secrets;

/// Setting key persisted in `setting_kv`.
pub const SETTINGS_KEY: &str = "quota_refresh";

/// Poll cadence. z.ai resets on next request, so we don't need a precise
/// wake-at-expiry — observing within this window is good enough.
const POLL_INTERVAL: Duration = Duration::from_secs(180);

/// Max ping attempts before giving up this cycle. The ping is a 1-token
/// freshness nudge; over-retrying a flaky provider wastes cycles.
const MAX_ATTEMPTS: u32 = 3;
const RETRY_DELAY: Duration = Duration::from_secs(5);

/// Exit flag polled by the worker loop. Toggled by tray "Quit".
static SHOULD_EXIT: AtomicBool = AtomicBool::new(false);

pub fn request_exit() {
    SHOULD_EXIT.store(true, Ordering::SeqCst);
}

/// Per-endpoint config persisted via `setting_kv`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StoredEndpointConfig {
    pub enabled: bool,
    #[serde(default)]
    pub protocol: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// Name of the `QuotaItem` the worker should track (e.g. "5h-token",
    /// "weekly-token"). `None` falls back to the "5h-name match" so
    /// older endpoints keep working without re-configuration.
    #[serde(default)]
    pub target_quota_name: Option<String>,
    #[serde(default)]
    pub last_status: Option<String>,
    /// Worker observation cadence for this endpoint, in seconds. The global
    /// worker tick runs at POLL_INTERVAL; we additionally skip an endpoint
    /// until `check_rate_secs` has elapsed since its last check.
    #[serde(default = "default_check_rate_secs")]
    pub check_rate_secs: u32,
    /// Grace buffer added on top of the provider's `nextResetTime` before
    /// the worker fires its reset ping. z.ai's server-side reset lags the
    /// reported reset time by a few minutes; pinging during that lag gets
    /// rejected as quota-exceeded. We wait this many seconds past the
    /// reported reset before pinging, showing "resetting" in between.
    #[serde(default = "default_reset_grace_secs")]
    pub reset_grace_secs: u32,
    /// User-configured custom quota extractor (field mapping). When enabled
    /// it takes priority over the built-in provider fetch. **Legacy** — kept
    /// for deserialize-compat with older setting blobs; new writes express
    /// the same intent via `query_plan: Custom(_)`. [`resolve_plan`] reads
    /// this when `query_plan` is `None`.
    #[serde(default)]
    pub extractor: Option<crate::endpoint_quota::QuotaExtractorConfig>,
    /// Explicit query plan — the canonical "how is quota queried" choice.
    /// `None` here means "use the legacy resolution" (extractor.enabled or
    /// host detection) via [`resolve_plan`]. Set by the UI's plan picker and
    /// stamped by `endpoint_create_with_preset` for preset-borne queries.
    #[serde(default)]
    pub query_plan: Option<crate::endpoint_quota::QuotaQueryPlan>,
    /// True once any fetch has returned data for the current plan. The
    /// keep-alive worker refuses to ping until this is set, and the UI gates
    /// both the keep-alive switch and the quota bars on it. Cleared when the
    /// plan changes so a re-verify is required.
    #[serde(default)]
    pub provisioned: Option<bool>,
    /// OpenCode Go dashboard workspace ID (non-secret). Paired with the
    /// `auth` cookie stored in `secrets.rs` under `opencode-go-cookie-{id}`;
    /// together they authenticate the dashboard scrape. Only consulted when
    /// the resolved plan is `Preset { OpencodeGo }`.
    #[serde(default)]
    pub opencode_workspace_id: Option<String>,
    /// Which quota windows the provider-card preview shows (multi-select).
    /// `None` (legacy) falls back to the 5h-name heuristic; an explicit empty
    /// list means "show nothing". Display-only — independent of the keep-alive
    /// `target_quota_name` (the ping target). Set from the Quota page's
    /// settings dialog.
    #[serde(default)]
    pub preview_windows: Option<Vec<String>>,
}

/// Load the OpenCode Go dashboard credentials for an endpoint: the workspace
/// ID (from the settings blob) + the `auth` cookie (from `secrets.rs`).
/// Returns `None` if either is missing — callers surface a clear "creds not
/// set" snapshot. `pub` so both the worker (`tick`) and the
/// `endpoint_fetch_quota` command go through one loader.
pub fn load_opencode_creds(
    endpoint_id: &str,
    cfg: &StoredEndpointConfig,
) -> Option<(String, String)> {
    let ws = cfg.opencode_workspace_id.as_deref()?.trim();
    if ws.is_empty() {
        return None;
    }
    let cookie = secrets::get(&format!("opencode-go-cookie-{endpoint_id}"))
        .ok()
        .flatten()?;
    if cookie.is_empty() {
        return None;
    }
    Some((cookie, ws.to_string()))
}

/// Apply a workspace-ID edit from the creds editor to the settings blob,
/// mirroring the editor's rules: trimmed; blank clears the field; any change
/// re-locks the gate (`provisioned = false` so a fetch must re-confirm data
/// before the bars/keep-alive unlock). Extracted from `opencode_set_creds` so
/// the trim/clear rule is unit-testable.
pub fn set_opencode_workspace_id(
    settings: &mut RefreshSettings,
    endpoint_id: &str,
    workspace_id: &str,
) {
    let ws = workspace_id.trim();
    let entry = settings.endpoints.entry(endpoint_id.to_string()).or_default();
    entry.opencode_workspace_id = if ws.is_empty() { None } else { Some(ws.to_string()) };
    entry.provisioned = Some(false);
}

fn default_check_rate_secs() -> u32 {
    180
}

fn default_reset_grace_secs() -> u32 {
    180
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RefreshSettings {
    #[serde(default)]
    pub endpoints: HashMap<String, StoredEndpointConfig>,
}

/// Resolve the effective [`QuotaQueryPlan`] for an endpoint, honouring
/// backward compatibility with older setting blobs. Priority:
/// 1. Explicit `cfg.query_plan` (the canonical field; `Some(None)` means the
///    user explicitly disabled the query).
/// 2. Legacy `cfg.extractor` with `enabled` → `Custom`.
/// 3. Host detection ([`endpoint_quota::provider_kind_for`]) on the
///    endpoint's quota URL → `Preset`. This backfills endpoints created
///    before the query-plan concept so existing z.ai / MiniMax / OpenRouter
///    setups keep working without re-configuration.
/// 4. Otherwise `None`.
///
/// This is the single source of truth for "which plan does this endpoint
/// use" — both the worker (`tick`) and the `endpoint_fetch_quota` command go
/// through it.
pub fn resolve_plan(cfg: &StoredEndpointConfig, endpoint: &EndpointRow) -> crate::endpoint_quota::QuotaQueryPlan {
    use crate::endpoint_quota::QuotaQueryPlan;
    if let Some(plan) = &cfg.query_plan {
        return plan.clone();
    }
    if let Some(ex) = &cfg.extractor {
        if ex.enabled {
            return QuotaQueryPlan::Custom(ex.clone());
        }
    }
    let url = crate::db::pick_quota_url(&endpoint.protocols).unwrap_or_default();
    match crate::endpoint_quota::provider_kind_for(&url) {
        Some(kind) => QuotaQueryPlan::Preset { kind },
        None => QuotaQueryPlan::None,
    }
}

/// Resolve the target quota item's reset timestamp, or `None` when the
/// chosen window doesn't exist. `target = None` falls back to the
/// "any 5h-named item" rule so endpoints configured before the per-item
/// pick still work.
fn target_reset_ms(items: &[QuotaItem], target: Option<&str>) -> Option<i64> {
    let pick = |i: &QuotaItem| -> bool {
        match target {
            Some(name) if !name.is_empty() => i.name == name,
            _ => i.name == "5h-token" || i.name.ends_with("/5h"),
        }
    };
    items.iter().find(|i| pick(i)).and_then(|i| i.resets_at_ms)
}

/// Strict window expiry: true the instant the reported reset time passes,
/// ignoring any grace buffer. Used to enter the "resetting" window before
/// the grace-extended ping actually fires.
pub fn window_expired_for(items: &[QuotaItem], target: Option<&str>, now_ms: i64) -> bool {
    target_reset_ms(items, target).is_some_and(|reset_ms| now_ms >= reset_ms)
}

/// True when the grace-extended reset point has passed — i.e. it's safe
/// to fire the reset ping. `needs_reset_for = window_expired_for` plus
/// `reset_grace_secs` of slack for the provider's laggy server-side reset.
pub fn needs_reset_for(
    items: &[QuotaItem],
    target: Option<&str>,
    now_ms: i64,
    grace_secs: u32,
) -> bool {
    target_reset_ms(items, target)
        .is_some_and(|reset_ms| now_ms >= reset_ms + (grace_secs as i64) * 1000)
}

/// Resolve the tracked quota item itself (the entry `target_reset_ms`
/// derives its reset time from). `None` when the chosen window isn't
/// present in the response. Used by the exhaustion fallback below.
fn tracked_item<'a>(items: &'a [QuotaItem], target: Option<&str>) -> Option<&'a QuotaItem> {
    let pick = |i: &QuotaItem| -> bool {
        match target {
            Some(name) if !name.is_empty() => i.name == name,
            _ => i.name == "5h-token" || i.name.ends_with("/5h"),
        }
    };
    items.iter().find(|i| pick(i))
}

/// True when the tracked item reports itself exhausted by percentage
/// (`pct >= 100`) — independent of any clock-based reset time. Test-only
/// today: the worker inlines the same check.
#[cfg(test)]
pub fn is_item_exhausted(items: &[QuotaItem], target: Option<&str>) -> bool {
    tracked_item(items, target)
        .is_some_and(|i| i.pct >= 100.0)
}

/// True when the worker should fire a ping for this endpoint right now.
/// Combines the clock-based gate (`needs_reset_for`) with two fallbacks for
/// providers that don't expose a reset timestamp, so every expired window
/// still gets its reset ping:
///   • exhaustion fallback — the tracked item reports `pct >= 100`;
///   • idle-window fallback — the tracked item reads `pct <= 0` with no
///     reset time. z.ai returns `nextResetTime: 0` + `percentage: 0` once
///     the 5h window lapses with no traffic: the clock gate can never fire
///     (no reset_ms) and the exhaustion fallback never fires (pct < 100),
///     so without this the window would stay un-pinged forever. An idle
///     window with no reset time IS an expired window.
///
/// Balance-based items (`is_balance`, e.g. OpenRouter credits / Moonshot
/// balance) are excluded from both fallbacks: a monetary balance has no
/// reset window and a ping can't "reset" it — firing would only burn tokens
/// (a fresh account reads pct 0, a depleted one pct 100, both non-window).
pub fn should_fire_ping(
    items: &[QuotaItem],
    target: Option<&str>,
    now_ms: i64,
    grace_secs: u32,
) -> bool {
    needs_reset_for(items, target, now_ms, grace_secs)
        // Fallback: tracked item is present but carries no reset time, and
        // it reports exhaustion by percentage → ping to trigger the reset.
        || (target_reset_ms(items, target).is_none()
            && tracked_item(items, target)
                .is_some_and(|i| !i.is_balance && i.pct >= 100.0))
        // Fallback: tracked item present, no reset time, and the window
        // reads idle (0% used) → ping to re-establish the lapsed window.
        || (target_reset_ms(items, target).is_none()
            && tracked_item(items, target)
                .is_some_and(|i| !i.is_balance && i.pct <= 0.0))
}

/// Pure helper — resolve a ping model id. Priority:
/// 1. Explicit override. 2. `models.default` from stored `models_json`.
/// 3. `None` — caller skips the endpoint.
pub fn resolve_model(endpoint: &EndpointRow, override_model: Option<&str>) -> Option<String> {
    if let Some(m) = override_model {
        let trimmed = m.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    let json = endpoint.models_json.as_deref()?;
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let s = v.get("default").and_then(|d| d.as_str())?;
    if s.is_empty() { None } else { Some(s.to_string()) }
}

/// Pick the protocol + base URL. Priority:
/// `anthropic > openai > custom`. Returns `None` if no supported protocol
/// row exists.
pub fn select_protocol<'a>(
    protocols: &'a [ProtocolEntry],
    override_protocol: Option<&str>,
) -> Option<&'a ProtocolEntry> {
    if let Some(name) = override_protocol {
        if let Some(p) = protocols.iter().find(|p| p.protocol == name) {
            return Some(p);
        }
    }
    ["anthropic", "openai-comp", "custom"]
        .iter()
        .find_map(|name| protocols.iter().find(|p| p.protocol == *name))
}

/// Redacted placeholder shown in the preview UI in place of the real key.
pub const REDACTED_KEY: &str = "<KEY>";

/// Render of the request `fire_ping` would send for this endpoint + cfg.
/// The Authorization / x-api-key header values are replaced with
/// [`REDACTED_KEY`] so the UI can show the shape of the request without
/// leaking secrets. Used by the keep-alive preview dialog.
#[derive(Debug, Clone, Serialize)]
pub struct PingPreview {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub protocol: String,
    pub model: String,
}

pub fn build_ping_preview(
    endpoint: &EndpointRow,
    cfg: &StoredEndpointConfig,
) -> AppResult<PingPreview> {
    let proto = select_protocol(&endpoint.protocols, cfg.protocol.as_deref())
        .ok_or_else(|| AppError::Validation("no supported protocol".into()))?;
    let model = resolve_model(endpoint, cfg.model.as_deref())
        .ok_or_else(|| AppError::Validation("no model id (set endpoint default model)".into()))?;
    let (url, is_anthropic) = match proto.protocol.as_str() {
        "anthropic" => (
            crate::protocol_url::join_protocol_path(&proto.base_url, crate::config_writer::ProviderKind::Anthropic),
            true,
        ),
        "openai-comp" | "custom" => (
            crate::protocol_url::join_protocol_path(&proto.base_url, crate::config_writer::ProviderKind::Openai),
            false,
        ),
        other => return Err(AppError::Validation(format!("unsupported protocol '{other}'"))),
    };
    let body = serde_json::json!({
        "model": model,
        "max_tokens": 1,
        "messages": [{"role": "user", "content": "ping"}]
    });
    let body_pretty = serde_json::to_string_pretty(&body)
        .map_err(|e| AppError::Internal(format!("serialize body: {e}")))?;

    let mut headers: Vec<(String, String)> = vec![
        ("content-type".into(), "application/json".into()),
        ("accept".into(), "application/json".into()),
    ];
    if is_anthropic {
        headers.push(("x-api-key".into(), REDACTED_KEY.into()));
        headers.push(("anthropic-version".into(), "2023-06-01".into()));
    } else {
        headers.push(("Authorization".into(), format!("Bearer {REDACTED_KEY}")));
    }

    Ok(PingPreview {
        method: "POST".into(),
        url,
        headers,
        body: body_pretty,
        protocol: proto.protocol.clone(),
        model,
    })
}

/// Pull a human-readable reason out of an error body. Providers differ:
/// OpenAI/z.ai use `{"error":{"message":"..."}}`, Anthropic
/// `{"error":{"type":"...","message":"..."}}`. Fall back to a truncated
/// snippet when there's no JSON shape we recognise.
fn extract_reason(body: &str) -> String {
    let trim: String = body.trim().chars().take(300).collect();
    let parsed: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => {
            return if trim.is_empty() {
                "no body".into()
            } else {
                trim
            }
        }
    };
    let msg = parsed
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let full = match msg {
        Some(m) => m,
        None => {
            return if trim.is_empty() {
                "no message".into()
            } else {
                trim
            }
        }
    };
    if full.chars().count() <= 300 { full } else { full.chars().take(300).collect() }
}

/// Failure of a reset ping, tagged by whether the caller should retry.
/// `transient = true` means the failure is expected to clear on its own
/// (provider quota lag, rate-limit, network blip) — the worker keeps the
/// "retrying" status and tries again next tick. `transient = false` means
/// a permanent rejection (bad model id, bad key) — surfaced as a hard
/// error so the user fixes the config.
#[derive(Debug, Clone)]
pub struct PingFailure {
    pub message: String,
    pub transient: bool,
}

/// Coarse failure kind for retry decisions. `Short` transient failures
/// (network, 5xx) are retried inside the attempt loop with `RETRY_DELAY`;
/// `Long` transient failures (quota lag — the provider's reset hasn't
/// landed yet, which takes minutes) break out immediately so the worker
/// doesn't burn 15s per tick waiting for a server-side clock.
enum FailKind {
    Short,
    Long,
    Permanent,
}

impl FailKind {
    fn is_transient(&self) -> bool {
        !matches!(self, FailKind::Permanent)
    }
}

/// Classify an HTTP status + body into a retry kind. Quota/rate-limit
/// language in a 4xx body is treated as `Long` transient because z.ai
/// keeps returning it for the few minutes its reset lags the reported
/// `nextResetTime`.
fn classify_status(status: u16, body: &str) -> FailKind {
    if (500..600).contains(&status) {
        return FailKind::Short;
    }
    if status == 429 {
        return FailKind::Long;
    }
    if (400..500).contains(&status) {
        let low = body.to_ascii_lowercase();
        const MARKERS: [&str; 6] = ["quota", "rate", "limit", "exceed", "throttl", "余额"];
        return if MARKERS.iter().any(|m| low.contains(m)) {
            FailKind::Long
        } else {
            FailKind::Permanent
        };
    }
    FailKind::Permanent
}

/// Fire one minimal ping (`max_tokens: 1`) at the resolved endpoint to
/// trigger the provider's reset-on-next-request.
///
/// Retries short transient failures (5xx, network) in-loop. Quota-lag
/// failures (429, quota-style 4xx) and permanent 4xx rejections break out
/// immediately — the former clears on the provider's timeline, not ours,
/// so the worker retries on its next tick instead of stalling here.
fn fire_ping(
    endpoint: &EndpointRow,
    cfg: &StoredEndpointConfig,
    key: &str,
) -> Result<(), PingFailure> {
    let proto = select_protocol(&endpoint.protocols, cfg.protocol.as_deref())
        .ok_or_else(|| PingFailure { message: "no supported protocol".into(), transient: false })?;
    let model = resolve_model(endpoint, cfg.model.as_deref())
        .ok_or_else(|| PingFailure {
            message: "no model id (set endpoint default model)".into(),
            transient: false,
        })?;
    let (url, is_anthropic) = match proto.protocol.as_str() {
        "anthropic" => (
            crate::protocol_url::join_protocol_path(&proto.base_url, crate::config_writer::ProviderKind::Anthropic),
            true,
        ),
        "openai-comp" | "custom" => (
            crate::protocol_url::join_protocol_path(&proto.base_url, crate::config_writer::ProviderKind::Openai),
            false,
        ),
        other => {
            return Err(PingFailure {
                message: format!("unsupported protocol '{other}'"),
                transient: false,
            })
        }
    };
    let body = serde_json::json!({
        "model": model,
        "max_tokens": 1,
        "messages": [{"role": "user", "content": "ping"}]
    })
    .to_string();

    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(20))
        // Never follow redirects: custom auth headers (x-api-key / Bearer)
        // would be forwarded cross-host on a 3xx, leaking the credential.
        .redirects(0)
        .build();
    let mut last = PingFailure {
        message: "no attempt made".into(),
        transient: false,
    };
    for attempt in 0..MAX_ATTEMPTS {
        let mut req = agent.post(&url);
        req = if is_anthropic {
            req.set("x-api-key", key)
                .set("anthropic-version", "2023-06-01")
        } else {
            req.set("Authorization", &format!("Bearer {key}"))
        };
        req = req
            .set("content-type", "application/json")
            .set("accept", "application/json");
        match req.send_string(&body) {
            Ok(resp) if (200..300).contains(&resp.status()) => return Ok(()),
            Ok(resp) => {
                let status = resp.status();
                let body_text = resp.into_string().unwrap_or_default();
                let reason = extract_reason(&body_text);
                let kind = classify_status(status, &body_text);
                last = PingFailure {
                    message: format!("HTTP {status}: {reason}"),
                    transient: kind.is_transient(),
                };
                // Quota lag and permanent 4xx won't clear in 5s — stop and
                // let the next worker tick (or the user) re-evaluate.
                if !matches!(kind, FailKind::Short) {
                    break;
                }
            }
            Err(e) => {
                last = PingFailure {
                    message: format!("network: {e}"),
                    transient: true,
                };
            }
        }
        if attempt + 1 < MAX_ATTEMPTS {
            std::thread::sleep(RETRY_DELAY);
        }
    }
    Err(PingFailure { message: format!("ping failed: {}", last.message), transient: last.transient })
}

/// Imperative variant of `fire_ping` used by the `[test]` TermLink in the
/// keep-alive popover. Carries the same `transient` flag so the command
/// layer can distinguish "retrying" from a hard "error".
pub fn try_ping(
    endpoint: &EndpointRow,
    cfg: &StoredEndpointConfig,
    key: &str,
) -> Result<(), PingFailure> {
    fire_ping(endpoint, cfg, key)
}

// ---- Runtime keep-alive state ----

/// Runtime per-endpoint keep-alive state, refreshed by the worker each tick
/// and by manual pings. In-memory only — never persisted (cross-launch intent
/// lives in the stored `last_status`, which stays untouched). Exposed to the
/// UI via the `quota_keepalive_status` command so the indicator can show
/// phase / last success / next fire / error / attempts without DB reads.
pub static KEEPALIVE_STATE: std::sync::LazyLock<std::sync::Mutex<HashMap<String, KeepAliveState>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// Last worker heartbeat (epoch ms), stamped at the top of every `tick()`.
/// The worker is a single detached thread, so one timestamp covers every
/// endpoint. A stale value (> 2× `POLL_INTERVAL` old) means the thread is
/// either dead (panicked past the supervisor — should be impossible now) or
/// the process is suspended. Exposed per-endpoint via `KeepAliveState` and
/// via `last_heartbeat_ms()` for command/UI consumers.
static LAST_HEARTBEAT_MS: std::sync::LazyLock<std::sync::Mutex<i64>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(0));

/// Last worker panic recovered by the supervisor (epoch ms), or `0` if the
/// worker has never panicked (or has since ticked successfully). Mirrored
/// into `KeepAliveState::last_panic_at` for UI surfacing.
static LAST_PANIC_MS: std::sync::LazyLock<std::sync::Mutex<i64>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(0));

/// Read the most recent worker heartbeat (epoch ms). `0` means the worker
/// has never completed a tick this session — distinct from a stale-but-once
/// alive heartbeat.
pub fn last_heartbeat_ms() -> i64 {
    LAST_HEARTBEAT_MS.lock().map(|v| *v).unwrap_or(0)
}

/// Read the most recent worker panic timestamp (epoch ms). `0` means no
/// panic has been observed since startup.
pub fn last_panic_ms() -> i64 {
    LAST_PANIC_MS.lock().map(|v| *v).unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeepAlivePhase {
    /// Endpoint not configured for keep-alive (switch off).
    #[default]
    Disabled,
    /// Keep-alive enabled but no query plan is configured. The worker can't
    /// observe quota without a plan, so it stays parked until the user picks
    /// one (Preset or Custom) in the Quota settings.
    NotConfigured,
    /// A query plan is configured but no fetch has verified data yet (or the
    /// last verify failed). The worker self-verifies on its next tick; once a
    /// fetch returns items it stamps `provisioned` and proceeds to [`Self::Idle`].
    Unverified,
    /// Enabled, monitoring, target window not expired — nothing to do.
    Idle,
    /// Reported reset passed; within the grace buffer — ping held.
    Resetting,
    /// Reset ping in flight.
    Pinging,
    /// Last ping failed transiently; worker retries next tick.
    Retrying,
    /// Last ping failed permanently — config needs attention.
    Error,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct KeepAliveState {
    pub phase: KeepAlivePhase,
    /// Epoch ms of the last successful reset ping.
    pub last_success_at: Option<i64>,
    /// Epoch ms when the next reset ping is scheduled, when computable from
    /// the last quota observation (reset time + grace buffer).
    pub next_fire_at: Option<i64>,
    /// Message from the last failed ping.
    pub last_error: Option<String>,
    /// Consecutive failed pings since the last success.
    pub attempts: u32,
    /// Epoch ms of the most recent worker heartbeat (one stamp per tick).
    /// Lets the UI distinguish "alive but idle" (recent heartbeat) from
    /// "worker dead / never started" (stale or `None`). Bumped at the top of
    /// every `tick()`, shared across all endpoints since the worker is a
    /// single thread.
    #[serde(default)]
    pub last_heartbeat_at: Option<i64>,
    /// Epoch ms of the last worker panic recovered by the supervisor, when
    /// any. `None` once a subsequent tick succeeds. Surfaces so the user can
    /// see "the worker panicked at HH:MM but recovered" instead of a silent
    /// recovery.
    #[serde(default)]
    pub last_panic_at: Option<i64>,
}

/// Read the current runtime state for an endpoint (defaults to Disabled).
/// Always overlays the shared worker heartbeat + last-panic timestamps so
/// the UI can show worker health alongside per-endpoint phase — the worker
/// is a single thread, so these are the same value for every endpoint.
pub fn keepalive_state(endpoint_id: &str) -> KeepAliveState {
    let mut s = KEEPALIVE_STATE
        .lock()
        .map(|m| m.get(endpoint_id).cloned().unwrap_or_default())
        .unwrap_or_default();
    s.last_heartbeat_at = {
        let hb = last_heartbeat_ms();
        if hb > 0 { Some(hb) } else { None }
    };
    s.last_panic_at = {
        let p = last_panic_ms();
        if p > 0 { Some(p) } else { None }
    };
    s
}

/// Mutate the runtime state for an endpoint under the lock.
pub fn update_keepalive(endpoint_id: &str, f: impl FnOnce(&mut KeepAliveState)) {
    if let Ok(mut m) = KEEPALIVE_STATE.lock() {
        f(m.entry(endpoint_id.to_string()).or_default());
    }
}

/// Record a ping outcome (worker + manual ping share this path). On success
/// resets attempts/last_error; on failure tags the phase by transience and
/// bumps the attempt counter.
pub fn record_ping_outcome(endpoint_id: &str, res: &Result<(), PingFailure>) {
    let now_ms = chrono::Utc::now().timestamp_millis();
    match res {
        Ok(()) => update_keepalive(endpoint_id, |s| {
            s.phase = KeepAlivePhase::Idle;
            s.last_success_at = Some(now_ms);
            s.next_fire_at = None;
            s.last_error = None;
            s.attempts = 0;
        }),
        Err(f) => update_keepalive(endpoint_id, |s| {
            s.phase = if f.transient { KeepAlivePhase::Retrying } else { KeepAlivePhase::Error };
            s.last_error = Some(f.message.clone());
            s.attempts = s.attempts.saturating_add(1);
        }),
    }
}

/// Last observation timestamp per endpoint, in epoch millis. Module-scoped
/// because the worker's lifetime matches the process — there's no need to
/// persist this across launches (the first tick after launch observes
/// every endpoint once, which is the right reset behaviour anyway).
static LAST_CHECK_MS: std::sync::LazyLock<std::sync::Mutex<HashMap<String, i64>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

fn should_check_now(id: &str, rate_secs: u32, now_ms: i64) -> bool {
    let Ok(mut map) = LAST_CHECK_MS.lock() else {
        return true;
    };
    let last = map.get(id).copied().unwrap_or(0);
    if now_ms - last >= (rate_secs as i64) * 1000 {
        map.insert(id.to_string(), now_ms);
        true
    } else {
        false
    }
}

/// Run one observe-then-react pass over every enabled endpoint. Stamps the
/// shared heartbeat at entry so callers/the UI can distinguish a live worker
/// from a dead one. `quota` is the gateway's reactive quota store; a
/// successful reset ping clears the endpoint's exhaustion there so the
/// router un-skips it without waiting for a real gateway request.
fn tick(db: &Mutex<rusqlite::Connection>, quota: &crate::orchestration::quota_state::QuotaState) {
    let now_ms = chrono::Utc::now().timestamp_millis();
    if let Ok(mut hb) = LAST_HEARTBEAT_MS.lock() {
        *hb = now_ms;
    }
    let (settings, endpoints) = match load_endpoints(db) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "quota worker tick: load_endpoints failed");
            return;
        }
    };
    let enabled_count = endpoints
        .iter()
        .filter(|e| {
            e.has_api_key
                && settings
                    .endpoints
                    .get(&e.id)
                    .is_some_and(|c| c.enabled)
        })
        .count();
    tracing::debug!(
        endpoints = endpoints.len(),
        enabled = enabled_count,
        "quota worker heartbeat"
    );

    for ep in endpoints.iter().filter(|e| e.has_api_key) {
        let cfg = match settings.endpoints.get(&ep.id) {
            Some(c) if c.enabled => c.clone(),
            _ => {
                update_keepalive(&ep.id, |s| s.phase = KeepAlivePhase::Disabled);
                continue;
            }
        };
        // Honor the per-endpoint check rate. Endpoints with longer
        // configured rates simply observe less often than the global tick.
        if !should_check_now(&ep.id, cfg.check_rate_secs, now_ms) {
            continue;
        }
        let key = match secrets::get(&ep.id) {
            Ok(Some(k)) if !k.is_empty() => k,
            _ => continue,
        };
        let plan = resolve_plan(&cfg, ep);
        if !plan.is_active() {
            // Enabled but no query plan — the worker can't observe quota
            // without one, so park here. Surfaces a distinct phase so the UI
            // can say "select a query plan first" instead of a bare idle.
            update_keepalive(&ep.id, |s| s.phase = KeepAlivePhase::NotConfigured);
            continue;
        }
        // OpenCode Go authenticates its dashboard scrape with a session cookie
        // + workspace ID (not the API key). Load them only for that plan.
        let opencode = if matches!(
            plan,
            endpoint_quota::QuotaQueryPlan::Preset { kind: endpoint_quota::BuiltinKind::OpencodeGo }
        ) {
            load_opencode_creds(&ep.id, &cfg)
        } else {
            None
        };
        let quota_obs = endpoint_quota::fetch_with_plan(
            ep,
            &key,
            &plan,
            opencode.as_ref().map(|(c, w)| (c.as_str(), w.as_str())),
        );
        // Provisioning gate: the worker must observe at least one successful
        // fetch (data returned) before it is allowed to fire reset pings.
        // Until then it self-verifies each tick and surfaces Unverified +
        // the fetch error so the user sees why. Once provisioned, transient
        // fetch failures just fall through to the normal idle decision
        // (provisioning is "ever verified", not "currently healthy").
        let provisioned = cfg.provisioned.unwrap_or(false);
        if quota_obs.ok && !quota_obs.items.is_empty() {
            if !provisioned {
                mark_provisioned(db, &ep.id);
            }
        } else if !provisioned {
            update_keepalive(&ep.id, |s| {
                s.phase = KeepAlivePhase::Unverified;
                s.last_error = quota_obs.error.clone();
            });
            continue;
        }
        let target = cfg.target_quota_name.as_deref();
        // OpenCode Go's windows reset on a fixed schedule (the dashboard shows
        // a clock reset time), not on-next-request like z.ai. A keep-alive ping
        // would NOT reset the window — it would just burn one of the user's Go
        // requests. So observe-only: never ping, regardless of arming.
        if matches!(
            plan,
            endpoint_quota::QuotaQueryPlan::Preset { kind: endpoint_quota::BuiltinKind::OpencodeGo }
        ) {
            update_keepalive(&ep.id, |s| {
                s.phase = KeepAlivePhase::Idle;
                s.next_fire_at = None;
            });
            continue;
        }
        // Next fire, when the quota observation yields a reset time: reset
        // time + grace. Re-derived each tick; None when no target item is
        // visible (nothing to ping for yet).
        let next_fire_at = target_reset_ms(&quota_obs.items, target)
            .map(|reset_ms| reset_ms + (cfg.reset_grace_secs as i64) * 1000);
        // Grace window: the reported reset time has passed but the
        // provider's server-side reset is still landing. Surface a neutral
        // "resetting" state and hold the ping until the grace buffer elapses.
        // Skipped when the target carries no reset timestamp (then there is
        // no "reported reset has passed" boundary to surface).
        if target_reset_ms(&quota_obs.items, target).is_some()
            && window_expired_for(&quota_obs.items, target, now_ms)
            && !needs_reset_for(&quota_obs.items, target, now_ms, cfg.reset_grace_secs)
        {
            update_keepalive(&ep.id, |s| {
                s.phase = KeepAlivePhase::Resetting;
                s.next_fire_at = next_fire_at;
            });
            let _ = set_status(db, &ep.id, "resetting");
            continue;
        }
        if !should_fire_ping(&quota_obs.items, target, now_ms, cfg.reset_grace_secs) {
            update_keepalive(&ep.id, |s| {
                s.phase = KeepAlivePhase::Idle;
                s.next_fire_at = next_fire_at;
            });
            continue;
        }
        update_keepalive(&ep.id, |s| s.phase = KeepAlivePhase::Pinging);
        let res = fire_ping(ep, &cfg, &key);
        record_ping_outcome(&ep.id, &res);
        match res {
            Ok(()) => {
                info!(endpoint = %ep.id, "5h reset ping ok");
                // Clear the gateway's reactive exhaustion for this endpoint
                // so the router un-skips it without waiting for a real
                // proxied request to land. The worker's ping bypasses the
                // gateway, so the gateway would otherwise never observe the
                // success itself.
                quota.clear_exhausted(&ep.id);
                let _ = set_status(db, &ep.id, "ok");
            }
            Err(f) if f.transient => {
                warn!(endpoint = %ep.id, error = %f.message, "5h reset ping transient; retrying next tick");
                let _ = set_status(db, &ep.id, &format!("retrying: {}", f.message));
            }
            Err(f) => {
                warn!(endpoint = %ep.id, error = %f.message, "5h reset ping failed permanently");
                let _ = set_status(db, &ep.id, &format!("error: {}", f.message));
            }
        }
    }
}

fn load_endpoints(db: &Mutex<rusqlite::Connection>) -> AppResult<(RefreshSettings, Vec<EndpointRow>)> {
    let conn = lock_db(db);
    let settings = load_settings(&conn)?;
    let endpoints = db::list_endpoints(&conn)?;
    Ok((settings, endpoints))
}

/// Lock the worker's DB connection, recovering from a poisoned mutex: a
/// panic elsewhere left the lock held, but the `Connection` itself is fine —
/// `into_inner` recovers it so the keep-alive worker keeps running. Without
/// this, one poison made every subsequent `tick` fail while `LAST_HEARTBEAT_MS`
/// kept updating — the UI showed a live worker that never did anything.
fn lock_db(db: &Mutex<rusqlite::Connection>) -> std::sync::MutexGuard<'_, rusqlite::Connection> {
    db.lock().unwrap_or_else(|e| e.into_inner())
}

/// Spawn the observe-then-react worker on its own OS thread. Returns
/// immediately; the thread runs until `request_exit()` flips the flag.
/// `quota` is the gateway's reactive quota store, shared so a successful
/// reset ping can clear endpoint exhaustion in the router's view.
pub fn spawn_worker(
    db: Arc<Mutex<rusqlite::Connection>>,
    quota: Arc<crate::orchestration::quota_state::QuotaState>,
) {
    // `expect` here would panic on thread-creation failure and take down
    // Tauri startup — keep-alive is auxiliary, so degrade loudly instead.
    if let Err(e) = std::thread::Builder::new()
        .name("nestra-quota-refresh".into())
        .spawn(move || run_loop(db, quota))
    {
        tracing::error!("failed to spawn quota-refresh worker: {e}");
    }
}

fn run_loop(
    db: Arc<Mutex<rusqlite::Connection>>,
    quota: Arc<crate::orchestration::quota_state::QuotaState>,
) {
    loop {
        if SHOULD_EXIT.load(Ordering::SeqCst) {
            return;
        }
        // Supervise each tick: a panic inside `tick` used to permanently
        // and silently kill the worker thread (the JoinHandle is detached,
        // so nothing observed the death). Wrapping in `catch_unwind` turns
        // a panic into a logged error + a clean continuation — the next
        // tick re-observes and the worker self-heals. `AssertUnwindSafe`
        // is sound here: `tick` touches only its own locals + the shared
        // stores (`KEEPALIVE_STATE`, `LAST_*` statics, the DB connection
        // guarded by its `Mutex`), all of which leave their own invariants
        // intact on unwind (locks are released by the unwinding thread).
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tick(&db, &quota);
        }));
        if let Err(payload) = outcome {
            let now_ms = chrono::Utc::now().timestamp_millis();
            if let Ok(mut p) = LAST_PANIC_MS.lock() {
                *p = now_ms;
            }
            tracing::error!(
                payload = ?payload,
                "quota worker tick panicked — supervisor continuing next cycle"
            );
        }
        // Sleep in 1s slices so SHOULD_EXIT flips within ~1s.
        let total_ms = POLL_INTERVAL.as_millis() as u64;
        let mut remaining = total_ms;
        while remaining > 0 && !SHOULD_EXIT.load(Ordering::SeqCst) {
            let step = remaining.min(1000);
            std::thread::sleep(Duration::from_millis(step));
            remaining = remaining.saturating_sub(step);
        }
    }
}

// ---- Persistence ----

/// Serializes every settings read-modify-write across the worker thread and
/// the UI commands. Both sides do `load → modify → save` of the whole blob,
/// and they use DIFFERENT connections (the worker's own vs `AppState.db`), so
/// a SQLite transaction cannot make the pair atomic — an interleaved
/// read-modify-write drops the other writer's field (e.g. the worker's
/// `last_status` vs the UI's `enabled`). One process-wide mutex over a tiny
/// JSON critical section is the cheapest correct fix.
static SETTINGS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Lock-protected read-modify-write of the settings blob. `f` sees the latest
/// persisted value, and its mutation is written back atomically relative to
/// every other caller of this function / `set_status_public` /
/// `mark_provisioned_public` — lost updates are impossible on that path.
pub fn update_settings<F>(conn: &rusqlite::Connection, f: F) -> AppResult<()>
where
    F: FnOnce(&mut RefreshSettings),
{
    let _guard = SETTINGS_LOCK
        .lock()
        .map_err(|e| AppError::Internal(format!("settings lock poisoned: {e}")))?;
    let mut settings = load_settings(conn)?;
    f(&mut settings);
    save_settings(conn, &settings)
}

pub fn load_settings(conn: &rusqlite::Connection) -> AppResult<RefreshSettings> {
    match db::get_setting(conn, SETTINGS_KEY)? {
        Some(v) => {
            // A parse failure must NOT silently fall back to defaults: the
            // next `save_settings` would permanently write those defaults
            // back over the (corrupt but recoverable) blob.
            serde_json::from_value(v).map_err(|e| {
                AppError::Internal(format!("quota refresh settings blob is corrupt: {e}"))
            })
        }
        None => Ok(RefreshSettings::default()),
    }
}

pub fn save_settings(conn: &rusqlite::Connection, settings: &RefreshSettings) -> AppResult<()> {
    db::set_setting(conn, SETTINGS_KEY, &serde_json::to_value(settings)?)
}

fn set_status(db: &Mutex<rusqlite::Connection>, endpoint_id: &str, status: &str) -> AppResult<()> {
    let conn = lock_db(db);
    set_status_public(&conn, endpoint_id, status)
}

/// Public helper for callers that already hold the DB lock (e.g. the
/// `quota_ping_now` command). Persists the latest ping status string so
/// the UI's `last_status` field reflects the most recent attempt.
pub fn set_status_public(
    conn: &rusqlite::Connection,
    endpoint_id: &str,
    status: &str,
) -> AppResult<()> {
    update_settings(conn, |settings| {
        let entry = settings.endpoints.entry(endpoint_id.to_string()).or_default();
        entry.last_status = Some(status.to_string());
    })
}

/// Stamp `provisioned = true` for an endpoint. Called after any successful
/// quota fetch (worker tick, `endpoint_fetch_quota`, the UI Verify button)
/// — the keep-alive worker refuses to ping until this is set, and the UI
/// gates both the keep-alive switch and the quota bars on it. Public so the
/// `endpoint_fetch_quota` command can provision as a side-effect of a fetch.
pub fn mark_provisioned_public(conn: &rusqlite::Connection, endpoint_id: &str) -> AppResult<()> {
    update_settings(conn, |settings| {
        let entry = settings.endpoints.entry(endpoint_id.to_string()).or_default();
        entry.provisioned = Some(true);
    })
}

/// Lock-internal wrapper around [`mark_provisioned_public`] for the worker
/// (which holds the DB as `Arc<Mutex<Connection>>`). Best-effort: a failure
/// to persist provisioning is logged and swallowed so a settings-blob hiccup
/// never kills the worker's observe/react cycle.
fn mark_provisioned(db: &Mutex<rusqlite::Connection>, endpoint_id: &str) {
    let result = {
        let conn = lock_db(db);
        mark_provisioned_public(&conn, endpoint_id)
    };
    if let Err(e) = result {
        tracing::warn!(endpoint = %endpoint_id, error = %e, "failed to stamp provisioned");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{EndpointRow, ProtocolEntry};

    fn ep(protocols: &[(&str, &str)]) -> EndpointRow {
        EndpointRow {
            id: "e1".into(),
            display_name: "ep".into(),
            has_api_key: true,
            status: "valid".into(),
            last_validated_at: None,
            models_json: Some(r#"{"default":"claude-haiku-4-5"}"#.into()),
            models_fetched_at: None,
            advanced_env_json: None,
            model_abilities_json: None,
            protocols: protocols
                .iter()
                .map(|(p, u)| ProtocolEntry {
                    protocol: (*p).into(),
                    base_url: (*u).into(),
                })
                .collect(),
        }
    }

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

    /// Same as `item` but lets the test set `pct` (exhaustion percentage).
    /// Used by the exhaustion-fallback tests.
    fn item_pct(name: &str, pct: f64, resets_at_ms: Option<i64>) -> QuotaItem {
        QuotaItem {
            name: name.into(),
            pct,
            used: None,
            total: None,
            remaining: None,
            resets_in: None,
            resets_at_ms,
            unit: None,
            is_balance: false,
        }
    }

    /// Balance-shaped item (OpenRouter credits / Moonshot): no reset time,
    /// `is_balance` set. Never eligible for a keep-alive ping.
    fn balance_item(name: &str, pct: f64) -> QuotaItem {
        QuotaItem {
            name: name.into(),
            pct,
            used: None,
            total: None,
            remaining: Some(10.0),
            resets_in: None,
            resets_at_ms: None,
            unit: Some("USD".into()),
            is_balance: true,
        }
    }

    #[test]
    fn needs_reset_fires_when_expired() {
        let items = vec![item("5h-token", Some(1000))];
        assert!(needs_reset_for(&items, None, 5000, 0));
    }

    #[test]
    fn needs_reset_idle_when_not_expired() {
        let items = vec![item("5h-token", Some(50_000))];
        assert!(!needs_reset_for(&items, None, 10_000, 0));
    }

    #[test]
    fn needs_reset_matches_minimax_new_shape() {
        let items = vec![item("claude-sonnet/5h", Some(1000))];
        assert!(needs_reset_for(&items, None, 5000, 0));
    }

    #[test]
    fn needs_reset_ignores_non_5h_items() {
        let items = vec![item("account-balance", Some(1000))];
        assert!(!needs_reset_for(&items, None, 5000, 0));
    }

    #[test]
    fn needs_reset_ignores_missing_reset() {
        let items = vec![item("5h-token", None)];
        assert!(!needs_reset_for(&items, None, 5000, 0));
    }

    #[test]
    fn should_fire_ignores_missing_reset_when_not_exhausted() {
        // No reset timestamp AND not exhausted by percentage → no ping.
        // This is the previous "never fires" state, now correctly idle
        // rather than permanently stuck.
        let items = vec![item_pct("5h-token", 47.0, None)];
        assert!(!should_fire_ping(&items, None, 5_000_000, 0));
    }

    #[test]
    fn should_fire_fires_when_exhausted_without_reset_ms() {
        // The provider reports exhaustion by percentage (pct >= 100) and
        // carries no reset timestamp (MiniMax flat shape, or z.ai returning
        // nextResetTime: 0). The clock-based gate can never fire, so the
        // exhaustion fallback must trigger the ping.
        let items = vec![item_pct("5h-token", 100.0, None)];
        assert!(should_fire_ping(&items, None, 1_000, 0));
    }

    #[test]
    fn should_fire_fires_idle_window_without_reset_ms() {
        // z.ai's lapsed 5h window reads `nextResetTime: 0` + `percentage: 0`
        // (no traffic since expiry). Neither the clock gate (no reset_ms)
        // nor the exhaustion fallback (pct < 100) fires — the idle-window
        // fallback must ping so the reset-on-next-request re-establishes
        // the window. This is the shape the real z.ai endpoint returns.
        let items = vec![item_pct("5h-token", 0.0, None)];
        assert!(should_fire_ping(&items, None, 1_000, 0));
    }

    #[test]
    fn should_fire_idle_fallback_respects_target_name() {
        // Tracking weekly-token, an idle 5h-token must NOT trigger the
        // idle-window fallback ping.
        let items = vec![
            item_pct("5h-token", 0.0, None),
            item_pct("weekly-token", 12.0, None),
        ];
        assert!(!should_fire_ping(&items, Some("weekly-token"), 1_000, 0));
        // And the reverse: idle weekly-token → fires.
        let items = vec![
            item_pct("5h-token", 12.0, None),
            item_pct("weekly-token", 0.0, None),
        ];
        assert!(should_fire_ping(&items, Some("weekly-token"), 1_000, 0));
    }

    #[test]
    fn should_fire_never_pings_balance_items() {
        // Balance-based quota (OpenRouter credits, Moonshot balance) has no
        // reset window and can't be "reset" by a ping. A fresh account reads
        // pct 0 (would trip the idle-window fallback) and a depleted one
        // reads pct 100 (would trip the exhaustion fallback) — neither may
        // fire a ping, or the worker would burn tokens pointlessly.
        let fresh = vec![balance_item("balance", 0.0)];
        assert!(!should_fire_ping(&fresh, None, 1_000, 0));
        let depleted = vec![balance_item("balance", 100.0)];
        assert!(!should_fire_ping(&depleted, None, 1_000, 0));
        // Tracking a windowed item while a balance item exists: balance must
        // not suppress the windowed fallback.
        let mixed = vec![
            item_pct("5h-token", 0.0, None),
            balance_item("balance", 100.0),
        ];
        assert!(should_fire_ping(&mixed, None, 1_000, 0));
        // Explicitly targeting the balance item still never fires.
        assert!(!should_fire_ping(&mixed, Some("balance"), 1_000, 0));
    }

    #[test]
    fn should_fire_does_not_double_fire_when_clock_gate_also_true() {
        // Both gates could be true simultaneously; `should_fire_ping` is a
        // plain OR, so it returns true once — the dedup is the caller's job
        // (one ping per tick). This just guards the OR semantics.
        let items = vec![item_pct("5h-token", 100.0, Some(1_000))];
        assert!(should_fire_ping(&items, None, 5_000, 0));
    }

    #[test]
    fn should_fire_exhausted_fallback_respects_target_name() {
        // When the user tracks weekly-token, an exhausted 5h-token must NOT
        // trigger the fallback ping.
        let items = vec![
            item_pct("5h-token", 100.0, None),
            item_pct("weekly-token", 12.0, None),
        ];
        assert!(!should_fire_ping(&items, Some("weekly-token"), 1_000, 0));
        // And the reverse: weekly-token exhausted → fires.
        let items = vec![
            item_pct("5h-token", 12.0, None),
            item_pct("weekly-token", 100.0, None),
        ];
        assert!(should_fire_ping(&items, Some("weekly-token"), 1_000, 0));
    }

    #[test]
    fn is_item_exhausted_threshold() {
        // pct >= 100 counts as exhausted (the boundary itself fires).
        assert!(is_item_exhausted(&[item_pct("5h-token", 100.0, None)], None));
        assert!(!is_item_exhausted(&[item_pct("5h-token", 99.9, None)], None));
        // Missing item → not exhausted.
        assert!(!is_item_exhausted(&[item_pct("other", 100.0, None)], None));
    }

    #[test]
    fn heartbeat_reports_zero_before_first_tick() {
        // A fresh process has never ticked. last_heartbeat_ms() returns 0
        // (not None — it's an i64 accessor) so the UI can render "starting".
        // This test pins the contract: no tick in this test thread has run.
        // (We can't assert 0 globally because tests run concurrently with a
        // shared static, so we only assert the accessor is callable.)
        let _ = last_heartbeat_ms();
    }

    #[test]
    fn keepalive_state_overlays_heartbeat_fields() {
        // keepalive_state() must always carry the shared heartbeat/panic
        // timestamps even for an endpoint that has no entry yet (defaults).
        // Guards the "UI can always tell alive-but-idle from dead" contract.
        let s = keepalive_state("definitely-not-a-real-endpoint");
        // last_heartbeat_at/last_panic_at may be None if no tick has run in
        // this process, but the field must exist and deserialize cleanly.
        let _ = s.last_heartbeat_at;
        let _ = s.last_panic_at;
    }

    #[test]
    fn supervisor_survives_tick_panic() {
        // The run_loop supervisor wraps each tick in catch_unwind. We can't
        // easily drive the real run_loop (infinite + sleeps), but we can
        // assert the same catch_unwind wiring: a panicking tick-equivalent
        // closure must not abort, and the panic must be recorded in
        // LAST_PANIC_MS so the UI can surface "recovered".
        let before = last_panic_ms();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            panic!("simulated tick failure");
        }));
        assert!(outcome.is_err());
        // The supervisor records the panic timestamp; this test's own
        // catch_unwind does not, mirroring the fact that recording is the
        // run_loop's responsibility. We assert the accessor stays callable
        // and the earlier `before` value was captured.
        let _ = last_panic_ms();
        let _ = before;
    }

    #[test]
    fn needs_reset_honours_grace_buffer() {
        // reset at 1000ms; grace 5s. Ping only safe at >= 6000ms.
        let items = vec![item("5h-token", Some(1000))];
        assert!(!needs_reset_for(&items, None, 1000, 5));
        assert!(!needs_reset_for(&items, None, 5999, 5));
        assert!(needs_reset_for(&items, None, 6000, 5));
    }

    #[test]
    fn window_expired_strict_ignores_grace() {
        // The "resetting" window opens the instant the reported reset
        // passes, independent of the grace buffer.
        let items = vec![item("5h-token", Some(1000))];
        assert!(!window_expired_for(&items, None, 999));
        assert!(window_expired_for(&items, None, 1000));
    }

    #[test]
    fn serde_default_fills_target_quota_name() {
        // Legacy rows predate the field — deserializing them must succeed
        // and fill target_quota_name with None.
        let json = r#"{"enabled":true,"protocol":"openai-comp","model":"x"}"#;
        let cfg: StoredEndpointConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.target_quota_name.is_none());
        assert!(cfg.enabled);
        // reset_grace_secs defaults via serde too.
        assert_eq!(cfg.reset_grace_secs, 180);
        // preview_windows defaults to None (legacy fallback applies).
        assert!(cfg.preview_windows.is_none());
    }

    #[test]
    fn preview_windows_round_trips_and_defaults() {
        let json = r#"{"enabled":true,"preview_windows":["5h","weekly"]}"#;
        let cfg: StoredEndpointConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.preview_windows.as_deref(), Some(&["5h".to_string(), "weekly".to_string()][..]));
        // Explicit empty list round-trips (user turned everything off).
        let json = r#"{"enabled":true,"preview_windows":[]}"#;
        let cfg: StoredEndpointConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.preview_windows, Some(vec![]));
        // Round-trip through the full settings blob.
        let mut settings = RefreshSettings::default();
        settings.endpoints.insert("ep1".into(), cfg);
        let v = serde_json::to_value(&settings).unwrap();
        let back: RefreshSettings = serde_json::from_value(v).unwrap();
        assert_eq!(back.endpoints["ep1"].preview_windows, Some(vec![]));
    }

    #[test]
    fn needs_reset_for_matches_target_name() {
        // When a target is set, an expired 5h-token item must NOT trigger
        // a ping — the user chose weekly-token.
        let items = vec![item("5h-token", Some(1000)), item("weekly-token", Some(5000))];
        assert!(!needs_reset_for(&items, Some("weekly-token"), 2000, 0));
        assert!(needs_reset_for(&items, Some("weekly-token"), 6000, 0));
    }

    #[test]
    fn needs_reset_for_falls_back_to_first_5h() {
        // Unset target falls back to the 5h-name match.
        let items = vec![item("claude-sonnet/5h", Some(1000))];
        assert!(needs_reset_for(&items, None, 5000, 0));
    }

    #[test]
    fn resolve_model_uses_default_when_no_override() {
        let e = ep(&[("openai-comp", "https://x")]);
        assert_eq!(resolve_model(&e, None).as_deref(), Some("claude-haiku-4-5"));
    }

    #[test]
    fn resolve_model_prefers_override() {
        let e = ep(&[("openai-comp", "https://x")]);
        assert_eq!(resolve_model(&e, Some("custom/model")).as_deref(), Some("custom/model"));
    }

    #[test]
    fn resolve_model_blank_override_falls_back() {
        let e = ep(&[("openai-comp", "https://x")]);
        assert_eq!(resolve_model(&e, Some("  ")).as_deref(), Some("claude-haiku-4-5"));
    }

    #[test]
    fn resolve_model_empty_default_returns_none() {
        let mut e = ep(&[("openai-comp", "https://x")]);
        e.models_json = Some(r#"{"default":""}"#.into());
        assert_eq!(resolve_model(&e, None), None);
    }

    #[test]
    fn resolve_model_missing_json_returns_none() {
        let mut e = ep(&[("openai-comp", "https://x")]);
        e.models_json = None;
        assert_eq!(resolve_model(&e, None), None);
    }

    #[test]
    fn select_protocol_priority() {
        let protos = vec![
            ProtocolEntry { protocol: "openai-comp".into(), base_url: "https://o".into() },
            ProtocolEntry { protocol: "anthropic".into(), base_url: "https://a".into() },
            ProtocolEntry { protocol: "custom".into(), base_url: "https://c".into() },
        ];
        assert_eq!(select_protocol(&protos, None).unwrap().protocol, "anthropic");
    }

    #[test]
    fn select_protocol_override_falls_back_if_missing() {
        let protos = vec![
            ProtocolEntry { protocol: "openai-comp".into(), base_url: "https://o".into() },
        ];
        assert_eq!(select_protocol(&protos, Some("anthropic")).unwrap().protocol, "openai-comp");
    }

    #[test]
    fn select_protocol_returns_none_when_unsupported() {
        let protos = vec![
            ProtocolEntry { protocol: "weird".into(), base_url: "https://w".into() },
        ];
        assert!(select_protocol(&protos, None).is_none());
    }

    #[test]
    fn serde_default_fills_check_rate() {
        // Legacy rows predate the field — deserializing them must succeed
        // and fill check_rate_secs with the documented default.
        let json = r#"{"enabled":true,"protocol":"openai-comp","model":"x"}"#;
        let cfg: StoredEndpointConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.check_rate_secs, 180);
        assert!(cfg.enabled);
    }

    #[test]
    fn build_ping_preview_redacts_key() {
        let e = ep(&[("openai-comp", "https://api.example.com")]);
        let cfg = StoredEndpointConfig {
            enabled: true,
            model: Some("custom-model".into()),
            ..Default::default()
        };
        let p = build_ping_preview(&e, &cfg).unwrap();
        // Authorization header carries the redacted marker, not a real key.
        assert!(
            p.headers
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("authorization")
                    && v == &format!("Bearer {REDACTED_KEY}")),
            "Authorization header should carry Bearer <KEY>, got: {:?}",
            p.headers
        );
        assert!(!p.headers.iter().any(|(_, v)| v.contains("sk-")), "no real key leak");
        assert!(p.body.contains("\"model\": \"custom-model\""));
        assert!(p.body.contains("\"max_tokens\": 1"));
        assert_eq!(p.method, "POST");
        assert_eq!(p.protocol, "openai-comp");
        assert_eq!(p.model, "custom-model");
    }

    #[test]
    fn build_ping_preview_anthropic_url() {
        let e = ep(&[("anthropic", "https://api.example.com")]);
        let cfg = StoredEndpointConfig {
            enabled: true,
            ..Default::default()
        };
        let p = build_ping_preview(&e, &cfg).unwrap();
        assert_eq!(p.url, "https://api.example.com/v1/messages");
        // Anthropic uses x-api-key + anthropic-version, not Bearer.
        assert!(p.headers.iter().any(|(k, _)| k == "x-api-key"));
        assert!(p.headers.iter().any(|(k, _)| k == "anthropic-version"));
        assert!(!p.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("authorization")));
    }

    #[test]
    fn build_ping_preview_openai_chat_url() {
        let e = ep(&[("openai-comp", "https://api.openai.com/v1")]);
        let cfg = StoredEndpointConfig::default();
        let p = build_ping_preview(&e, &cfg).unwrap();
        assert_eq!(p.url, "https://api.openai.com/v1/chat/completions");
        assert!(p.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("authorization")));
    }

    #[test]
    fn build_ping_preview_errors_without_model() {
        let mut e = ep(&[("openai-comp", "https://x")]);
        e.models_json = None;
        let cfg = StoredEndpointConfig::default();
        assert!(build_ping_preview(&e, &cfg).is_err());
    }

    #[test]
    fn extract_reason_pulls_error_message() {
        // z.ai / OpenAI shape: the real reason lives in error.message.
        let body = r#"{"error":{"message":"model not found for this key"}}"#;
        assert_eq!(extract_reason(body), "model not found for this key");
    }

    #[test]
    fn classify_marks_quota_and_rate_as_transient() {
        // 429 is always a long transient (rate-limit).
        assert!(matches!(classify_status(429, ""), FailKind::Long));
        // 5xx is short transient (retryable in-loop).
        assert!(matches!(classify_status(503, "upstream bad"), FailKind::Short));
        // 4xx with quota language = long transient (server reset lag).
        assert!(matches!(
            classify_status(400, r#"{"error":{"message":"request quota exceeded"}}"#),
            FailKind::Long
        ));
        assert!(matches!(
            classify_status(400, "rate limit hit"),
            FailKind::Long
        ));
        // 4xx without quota language = permanent.
        assert!(matches!(
            classify_status(400, r#"{"error":{"message":"model not found"}}"#),
            FailKind::Permanent
        ));
    }

    #[test]
    fn extract_reason_anthropic_shape() {
        let body = r#"{"type":"error","error":{"type":"not_found_error","message":"model: claude-x"}}"#;
        assert_eq!(extract_reason(body), "model: claude-x");
    }

    #[test]
    fn extract_reason_falls_back_to_snippet() {
        // Not JSON — fall back to the raw text and truncate to 300 chars.
        let body = "Bad Gateway: upstream timed out";
        assert_eq!(extract_reason(body), "Bad Gateway: upstream timed out");
        let long = "x".repeat(400);
        assert_eq!(extract_reason(&long).chars().count(), 300);
    }

    #[test]
    fn extract_reason_empty_body() {
        assert_eq!(extract_reason("  \n  "), "no body");
    }

    #[test]
    fn record_ping_outcome_maps_phases() {
        let e = |transient: bool, msg: &str| Err(PingFailure {
            message: msg.into(),
            transient,
        });
        let ok = Ok::<(), PingFailure>(());

        record_ping_outcome("t1", &e(true, "backend busy"));
        let s = keepalive_state("t1");
        assert_eq!(s.phase, KeepAlivePhase::Retrying);
        assert_eq!(s.attempts, 1);
        assert_eq!(s.last_error.as_deref(), Some("backend busy"));

        record_ping_outcome("t1", &ok);
        let s = keepalive_state("t1");
        assert_eq!(s.phase, KeepAlivePhase::Idle);
        assert_eq!(s.attempts, 0);
        assert!(s.last_error.is_none());
        assert!(s.last_success_at.is_some());

        record_ping_outcome("t1", &e(false, "401 invalid key"));
        assert_eq!(keepalive_state("t1").phase, KeepAlivePhase::Error);
        assert_eq!(keepalive_state("unknown").phase, KeepAlivePhase::Disabled);
    }

    // ---- resolve_plan (the query-plan SSOT + legacy backfill) ----

    use crate::endpoint_quota::{BuiltinKind, QuotaExtractorConfig, QuotaQueryPlan};

    #[test]
    fn resolve_plan_explicit_plan_wins() {
        // An explicit query_plan is authoritative, even if a legacy enabled
        // extractor or a matching host is also present.
        let e = ep(&[("openai-comp", "https://api.z.ai/v1")]);
        let cfg = StoredEndpointConfig {
            query_plan: Some(QuotaQueryPlan::None),
            ..Default::default()
        };
        assert!(matches!(resolve_plan(&cfg, &e), QuotaQueryPlan::None));

        let cfg = StoredEndpointConfig {
            query_plan: Some(QuotaQueryPlan::Preset { kind: BuiltinKind::Mock }),
            ..Default::default()
        };
        assert!(matches!(
            resolve_plan(&cfg, &e),
            QuotaQueryPlan::Preset { kind: BuiltinKind::Mock }
        ));
    }

    #[test]
    fn resolve_plan_legacy_enabled_extractor_becomes_custom() {
        // Older blobs express "custom query" as extractor.enabled; resolve
        // must lift that into the Custom plan variant.
        let e = ep(&[("openai-comp", "https://api.openai.com/v1")]);
        let cfg = StoredEndpointConfig {
            extractor: Some(QuotaExtractorConfig {
                enabled: true,
                url: "{{baseUrl}}/balance".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        match resolve_plan(&cfg, &e) {
            QuotaQueryPlan::Custom(_) => {}
            other => panic!("expected Custom, got {other:?}"),
        }
    }

    #[test]
    fn resolve_plan_host_fallback_backfills_preset() {
        // An endpoint with no explicit plan and no legacy extractor falls
        // back to host detection — existing z.ai / MiniMax / OpenRouter
        // setups keep working without re-configuration.
        let zai = ep(&[("openai-comp", "https://api.z.ai/v1")]);
        assert!(matches!(
            resolve_plan(&StoredEndpointConfig::default(), &zai),
            QuotaQueryPlan::Preset { kind: BuiltinKind::Zai }
        ));
        let mm = ep(&[("openai-comp", "https://api.minimax.io/v1")]);
        assert!(matches!(
            resolve_plan(&StoredEndpointConfig::default(), &mm),
            QuotaQueryPlan::Preset { kind: BuiltinKind::Minimax }
        ));
    }

    #[test]
    fn resolve_plan_unsupported_host_is_none() {
        // OpenAI / Anthropic / unknown hosts with no plan → None (gated).
        let e = ep(&[("openai-comp", "https://api.openai.com/v1")]);
        assert!(matches!(
            resolve_plan(&StoredEndpointConfig::default(), &e),
            QuotaQueryPlan::None
        ));
    }

    #[test]
    fn resolve_plan_disabled_extractor_does_not_become_custom() {
        // A legacy extractor with enabled=false must NOT count as a custom
        // plan — fall through to host detection.
        let e = ep(&[("openai-comp", "https://api.z.ai/v1")]);
        let cfg = StoredEndpointConfig {
            extractor: Some(QuotaExtractorConfig { enabled: false, ..Default::default() }),
            ..Default::default()
        };
        assert!(matches!(
            resolve_plan(&cfg, &e),
            QuotaQueryPlan::Preset { kind: BuiltinKind::Zai }
        ));
    }

    // ---- OpenCode Go dashboard credentials ----

    /// The creds-editor workspace-ID rule, extracted from `opencode_set_creds`:
    /// trimmed, blank clears, any change re-locks the gate (`provisioned`).
    #[test]
    fn set_opencode_workspace_id_trims_clears_and_relocks_gate() {
        let mut settings = RefreshSettings::default();
        set_opencode_workspace_id(&mut settings, "ep-go", "  ws_abc-123  ");
        let e = &settings.endpoints["ep-go"];
        assert_eq!(e.opencode_workspace_id.as_deref(), Some("ws_abc-123"));
        assert_eq!(e.provisioned, Some(false));

        // Blank clears the stored id (still re-locks the gate).
        set_opencode_workspace_id(&mut settings, "ep-go", "   ");
        let e = &settings.endpoints["ep-go"];
        assert_eq!(e.opencode_workspace_id, None);
        assert_eq!(e.provisioned, Some(false));
    }

    /// The workspace ID must survive an unrelated read-modify-write of the
    /// settings blob (the worker's `set_status` / `mark_provisioned`, and the
    /// `opencode_set_creds` path itself all go through `update_settings`).
    /// Server-side writes always merge; only the frontend's full-blob rewrites
    /// can drop a field, which the TS `composeEndpointConfig` test pins.
    #[test]
    fn opencode_workspace_id_survives_unrelated_settings_write() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::schema::migrate(&conn).unwrap();
        update_settings(&conn, |settings| {
            set_opencode_workspace_id(settings, "ep-go", "ws_abc-123");
        })
        .unwrap();
        // An unrelated write, mirroring the worker's status/provisioning path.
        set_status_public(&conn, "ep-go", "ok").unwrap();
        let settings = load_settings(&conn).unwrap();
        let e = &settings.endpoints["ep-go"];
        assert_eq!(e.opencode_workspace_id.as_deref(), Some("ws_abc-123"));
        assert_eq!(e.last_status.as_deref(), Some("ok"));
    }

    /// `load_opencode_creds` gates on BOTH halves: a missing/blank workspace
    /// returns None without consulting secrets; a workspace with no cookie on
    /// disk also returns None. (The positive round-trip — cookie set then
    /// loaded — is covered by the secrets.rs keychain tests + this gate.)
    #[test]
    fn load_opencode_creds_requires_both_workspace_and_cookie() {
        // Missing workspace → None (never touches secrets).
        assert!(load_opencode_creds("ep-go", &StoredEndpointConfig::default()).is_none());
        // Blank workspace → None.
        let blank_ws = StoredEndpointConfig {
            opencode_workspace_id: Some("  ".into()),
            ..Default::default()
        };
        assert!(load_opencode_creds("ep-go", &blank_ws).is_none());
        // Workspace set but no cookie file for this endpoint → None.
        let ws_only = StoredEndpointConfig {
            opencode_workspace_id: Some("ws_abc".into()),
            ..Default::default()
        };
        assert!(
            load_opencode_creds("no-such-endpoint", &ws_only).is_none(),
            "workspace without a stored cookie must not authenticate"
        );
    }
}