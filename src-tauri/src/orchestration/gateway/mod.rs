//! Local gateway — the HTTP server agents point at.
//!
//! Lifecycle (single process):
//!   1. [`spawn`] binds `127.0.0.1:[`GATEWAY_PORT`]` (a FIXED port, so the
//!      stable alias written into agent configs survives app restarts) and
//!      starts serving on the Tauri async runtime (Tokio under the hood).
//!   2. The bound address + a [`GatewayHandle`] are returned; `lib.rs` stores
//!      the handle in `AppState` so commands can read it and shut the gateway
//!      down on app quit.
//!   3. Each inbound request is dispatched by the [`adapters`] layer into a
//!      [`TaskContext`], resolved by the [`router`], and served by the matching
//!      [`protocol`] handler.
//!
//! Loopback-only: the server never binds a non-loopback interface, so agent
//! traffic (and the API keys that flow through it) never leaves the machine.
//!
//! Protocols: Anthropic Messages (`/v1/messages`) for Claude Code; OpenAI Chat
//! Completions (`/v1/chat/completions`) for OpenCode Desktop and Pi; the
//! OpenAI Responses API (`/v1/responses`) as an upstream wire for
//! responses-class models (grok-4.5, gpt-5.6-luna) and as an inbound for
//! future Responses-speaking clients. Migration is handled by the shared
//! retry/migrate loop; prompt-cache injection is policy-gated on the
//! Anthropic path.

pub mod control;
pub mod convert;
pub mod convert_responses;
pub mod forward;
pub mod protocol_anthropic;
pub mod protocol_openai;
pub mod protocol_responses;
pub mod stream;
pub mod stream_convert;
pub mod stream_responses;
pub mod trace;
pub mod tuning;

#[cfg(test)]
mod tests_capture;
#[cfg(test)]
mod tests_e2e;

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::StatusCode;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::sync::Mutex;

use crate::error::{AppError, AppResult};

/// A handle to the running gateway. Stored in `AppState`; `shutdown()` is
/// called on app quit. The bound address is what config writers put into an
/// agent's `base_url` (the stable alias) when the agent opts into the gateway.
#[derive(Clone)]
pub struct GatewayHandle {
    pub addr: SocketAddr,
    shutdown_tx: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    /// Epoch-ms this gateway bound. Scopes the "this session" request counters
    /// (`WHERE started_at >= started_at`) so the Gateway page shows per-run
    /// activity honestly, reset each launch.
    pub started_at: i64,
}

impl GatewayHandle {
    /// The base URL config writers use as the agent's stable alias. Always
    /// `http://127.0.0.1:<port>` — agents talk to the gateway, not the real
    /// upstream, when gateway-routed.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Stop accepting new connections and drain in-flight requests. Idempotent.
    pub async fn shutdown(&self) {
        if let Some(tx) = self.shutdown_tx.lock().await.take() {
            let _ = tx.send(());
        }
    }
}

/// Per-process shared state the gateway handlers read. Built once at spawn
/// time and cloned into each connection task.
#[derive(Clone)]
pub struct GatewayState {
    /// A dedicated DB connection (the gateway must not contend for the UI's
    /// `AppState.db` Mutex on every proxied request). Opened by the caller
    /// and handed in so tests can supply an in-memory connection.
    pub db: Arc<Mutex<rusqlite::Connection>>,
    /// Process-global health/quota/affinity stores. The protocol handler
    /// records outcomes into `health` and `quota`.
    pub health: Arc<crate::orchestration::health::ProviderHealth>,
    pub quota: Arc<crate::orchestration::quota_state::QuotaState>,
    pub affinity: Arc<crate::orchestration::router::RouteAffinity>,
    /// Credential resolution for a resolved endpoint. Defaults to
    /// `secrets::get`; tests inject a stub so the loop is exercisable without
    /// touching the on-disk keychain. The resolved key is used in memory for
    /// one request and never persisted.
    pub credential_reader: Arc<dyn Fn(&str) -> crate::error::AppResult<Option<String>> + Send + Sync>,
    /// Loopback auth token. `dispatch` reads this on every inbound request and
    /// rejects (401) when it is empty (fail-closed) or the presented credential
    /// does not constant-time-match. Rotated via the control surface without a
    /// restart. NEVER serialized or logged — lives only in the keychain + this
    /// shared `RwLock`.
    pub loopback_token: Arc<tokio::sync::RwLock<String>>,
    /// Live runtime tuning (timeouts + breaker parameters). Shared with
    /// `AppState` and `ProviderHealth` — a Settings edit applies to the next
    /// request with no restart. Std RwLock: brief, never-across-`.await`
    /// sections only (see `tuning.rs`).
    pub tuning: tuning::SharedTuning,
}

/// The gateway's fixed loopback port. FIXED (not ephemeral) so the stable
/// alias written into agent configs (`http://127.0.0.1:18777/<agent-id>`)
/// stays valid across app restarts — an ephemeral port meant every restart
/// silently broke every opted-in agent's connection.
pub const GATEWAY_PORT: u16 = 18777;

/// Bind loopback (`127.0.0.1:<port>`) and start serving. Returns the live
/// handle AND the accept loop's `JoinHandle` (so [`control::GatewayControl`]
/// can watch for an unexpected exit). Runs the accept loop as a background
/// task on the Tauri async runtime; `GatewayHandle::shutdown` stops it.
///
/// `port` is the configured/default port — the caller decides fallback
/// policy (`control::find_free_loopback_port`); a busy port here is a hard
/// error so the stable alias in agent configs can never silently point at a
/// dead listener.
pub async fn spawn(
    state: GatewayState,
    port: u16,
) -> AppResult<(GatewayHandle, tauri::async_runtime::JoinHandle<()>)> {
    // Bind the loopback port ONLY. Never bind 0.0.0.0 — agent traffic and the
    // API keys that flow through must never leave the machine.
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| {
            AppError::Internal(format!(
                "gateway bind failed on 127.0.0.1:{port} (is another Nestra instance running?): {e}"
            ))
        })?;

    // Ensure the model catalog is built before serving: the router's
    // capability branch reads `model_catalog` (endpoint × model pairs) and
    // would otherwise find nothing to route to. Rebuild is local-only (DB +
    // the models.dev ability cache), cheap, and idempotent. It also runs on
    // every `orch_model_catalog_rebuild` command after endpoint edits, so
    // this startup build is belt-and-braces.
    {
        let conn = state.db.lock().await;
        if let Err(e) = crate::orchestration::capability_registry::rebuild(&conn) {
            tracing::warn!("gateway: model catalog rebuild failed at startup: {e}");
        }
    }

    let addr = listener.local_addr().map_err(|e| {
        AppError::Internal(format!("gateway local_addr failed: {e}"))
    })?;
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
    let handle = GatewayHandle {
        addr,
        shutdown_tx: Arc::new(Mutex::new(Some(shutdown_tx))),
        started_at: chrono::Utc::now().timestamp_millis(),
    };

    let accept_state = state.clone();
    // Spawn the accept loop. We do NOT `await` it — it runs until shutdown.
    // The JoinHandle is returned so the control plane can detect an unexpected
    // exit (panic in dev builds) and surface it as a runtime Error.
    let join = tauri::async_runtime::spawn(async move {
        loop {
            // Accept with a shutdown race: if shutdown fires, stop accepting.
            tokio::select! {
                biased;
                _ = &mut shutdown_rx => {
                    break;
                }
                accept = listener.accept() => {
                    let (stream, _peer) = match accept {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::warn!("gateway accept error: {e}");
                            // Back off briefly to avoid a tight error loop.
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                            continue;
                        }
                    };
                    let io = TokioIo::new(stream);
                    let conn_state = accept_state.clone();
                    tauri::async_runtime::spawn(async move {
                        // Lazy DoS hardening: cap the header block + overall
                        // body size (a hostile local process could otherwise
                        // stream unbounded data into memory), bound the
                        // per-connection idle time, and drop `with_upgrades`
                        // (no websocket/upgrade surface is needed on the
                        // loopback gateway).
                        //
                        // serve_connection runs in an inner task; the outer
                        // awaits its JoinHandle so a handler PANIC is logged
                        // instead of silently tearing the socket down (the
                        // agent saw a bare ECONNRESET).
                        let serve = tauri::async_runtime::spawn(async move {
                            // ponytail: `header_read_timeout` was dropped —
                            // hyper 1.11.0 panics ("timeout set, but no timer
                            // set") on every connection when the raw
                            // http1::Builder isn't wired to a hyper timer, and
                            // no timer API exists on this builder. The guard
                            // was loopback-only DoS hardening; auth (token)
                            // + 64KB header cap remain. Upgrade path: route
                            // through `hyper_util::server::conn` (auto
                            // builder + TokioTimer) to restore it.
                            if let Err(e) = http1::Builder::new()
                                .max_buf_size(64 * 1024)
                                .serve_connection(io, hyper::service::service_fn(move |req| {
                                    let st = conn_state.clone();
                                    async move { dispatch(req, st).await }
                                }))
                                .await
                            {
                                tracing::warn!("gateway connection error: {e}");
                            }
                        });
                        if let Err(panic) = serve.await {
                            tracing::error!("gateway: connection handler panicked: {panic:?}");
                        }
                    });
                }
            }
        }
    });

    tracing::info!("nestra gateway listening on http://{addr}");
    Ok((handle, join))
}

/// Route one inbound request to the matching protocol handler. This is the
/// gateway's dispatch core — it identifies the agent from the request path
/// prefix (the config writer embeds `/<agent-id>/v1/...` into the base_url)
/// and hands off to the protocol handler for the agent's declared
/// [`GatewayWire`](crate::agents::GatewayWire) (Anthropic Messages for
/// claude-code-cli/zcode-desktop, Chat Completions for
/// opencode-desktop/pi-cli).
///
/// Requests without a recognized agent prefix fall through to the Anthropic
/// handler (backward-compat with prefix-less Claude Code config — the
/// `/v1/messages` path is unambiguous).
async fn dispatch(
    req: hyper::Request<Incoming>,
    state: GatewayState,
) -> Result<hyper::Response<stream::GatewayBody>, AppError> {
    // ── Loopback auth (fail-closed) ──
    // The token lives only in the keychain + the shared RwLock; it never
    // reaches the DB, a Serialize struct, or a log. Empty token = reject all
    // (no open loopback). A mismatch returns a bare 401 with no header echo.
    let token = state.loopback_token.read().await.clone();
    if token.is_empty() || !request_token_matches(req.headers(), &token) {
        return Ok(unauthorized());
    }

    let path = req.uri().path().to_string();
    let agent_id = extract_agent_id(&path);
    let wire = agent_id
        .as_deref()
        .and_then(crate::agents::agent_spec)
        .map(|s| s.gateway_wire)
        .unwrap_or(crate::agents::GatewayWire::Anthropic);
    let handled = match wire {
        crate::agents::GatewayWire::Chat => {
            protocol_openai::handle(req, state, agent_id.unwrap().as_str()).await
        }
        // claude-code-cli (explicit prefix) or None (prefix-less request
        // to /v1/messages) → Anthropic path. Default the agent id to
        // "claude-code-cli" when no prefix was present.
        crate::agents::GatewayWire::Anthropic => {
            protocol_anthropic::handle(req, state, agent_id.as_deref().unwrap_or("claude-code-cli"))
                .await
        }
        crate::agents::GatewayWire::Responses => {
            protocol_responses::handle(req, state, agent_id.unwrap().as_str()).await
        }
    };
    Ok(match handled {
        Ok(resp) => resp,
        Err(e) => {
            // A handler error used to tear the connection down with no
            // response — the agent saw a bare ECONNRESET. Surface a 500 +
            // a log line instead so failures are diagnosable.
            tracing::error!("gateway: handler error: {e:?}");
            internal_error()
        }
    })
}

/// Check the inbound credential against the loopback token. Accepts either
/// `Authorization: Bearer <token>` (Claude Code, OpenAI-compatible) or
/// `x-api-key: <token>` (raw Anthropic-style). Comparison is constant-time
/// ([`control::ct_eq`]) so a timing oracle can't recover the token. The
/// presented value is never logged.
fn request_token_matches(headers: &hyper::HeaderMap, token: &str) -> bool {
    if let Some(v) = headers
        .get(hyper::header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
    {
        if let Some(rest) = v.strip_prefix("Bearer ") {
            return control::ct_eq(rest, token);
        }
    }
    if let Some(v) = headers
        .get("x-api-key")
        .and_then(|h| h.to_str().ok())
    {
        return control::ct_eq(v, token);
    }
    false
}

/// Bare 401 for failed auth. The body carries no echoed header or token; just
/// a minimal Anthropic-style error envelope so configured clients surface it.
fn unauthorized() -> hyper::Response<stream::GatewayBody> {
    let mut r = hyper::Response::new(stream::GatewayBody::Full(Full::new(Bytes::from_static(
        b"{\"type\":\"error\",\"error\":{\"type\":\"authentication_error\",\"message\":\"unauthorized\"}}",
    ))));
    *r.status_mut() = StatusCode::UNAUTHORIZED;
    r
}

/// 500 for an internal handler failure — keeps the connection alive with a
/// real response (instead of the silent close the agent saw as ECONNRESET).
/// The handler's error is logged by the caller.
fn internal_error() -> hyper::Response<stream::GatewayBody> {
    let mut r = hyper::Response::new(stream::GatewayBody::json_full(serde_json::json!({
        "type": "error",
        "error": { "type": "nestra_gateway_error", "message": "internal gateway error" }
    })));
    *r.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
    r
}

/// Extract the agent id from a `/<agent-id>/v1/...` path prefix. Returns
/// `None` when the path doesn't start with a known agent id (e.g. a prefix-less
/// request to `/v1/messages`). Known agent ids are the registry ids; the
/// pre-rename ids (`claude-code`, `pi`) are aliased so gateway base_urls
/// written into agent configs before the `-cli` rename keep routing — route
/// attribution only, not a data migration.
fn extract_agent_id(path: &str) -> Option<String> {
    let trimmed = path.strip_prefix('/')?;
    let first = trimmed.split('/').next()?;
    if crate::agents::agents().iter().any(|a| a.id == first) {
        return Some(first.to_string());
    }
    match first {
        "claude-code" => Some("claude-code-cli".to_string()),
        "pi" => Some("pi-cli".to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod auth_tests;

#[cfg(test)]
mod agent_prefix_tests;

