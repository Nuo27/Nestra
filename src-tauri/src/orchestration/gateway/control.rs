//! Gateway **Service** control plane — the runtime state machine, loopback
//! auth token, and port-fallback helper.
//!
//! This is deliberately distinct from routing policy / provider health: it
//! owns only the gateway *process* — is it up, on what port, with what token,
//! since when. The router/health/quota stores live in the parent
//! [`orchestration`](crate::orchestration) modules and are untouched here.
//!
//! ## State machine
//! ```text
//!   Stopped ──enable/restart──▶ Starting ──bind OK──▶ Running
//!      ▲                           │                     │
//!      │                         bind fail              accept loop exit (panic)
//!      │                           ▼                     ▼
//!      └──────────────────────── Error ◀────────────────┘
//!                retry / auto-pick port ──▶ Starting
//! ```
//! - `start` is idempotent (no-op when already Running) and serialized via the
//!   inner tokio Mutex, so concurrent start/stop/restart can't race.
//! - A deliberate `stop` flips to `Stopped` *before* draining, so the accept
//!   loop's watcher (which marks `Error` on unexpected exit) stays quiet.
//!
//! ## Token
//! The loopback token is generated with `OsRng`, stored encrypted in the
//! keychain ([`secrets`]), and read on every inbound request by
//! [`dispatch`](super::dispatch). It is NEVER placed in the DB, a `Serialize`
//! struct, a log line, or the agent-facing error body. Rotation writes the new
//! value to the shared `RwLock`, so the very next request sees it — no restart.
//!
//! ## Persistence
//! Only two non-secret values persist (in `setting_kv`): the global enable flag
//! and the configured port. Runtime state (liveness, started_at, last_error) is
//! in-memory and rebuilt each launch from the persisted flag + a fresh bind.

use std::sync::Arc;

use tokio::sync::{Mutex, RwLock};

use crate::db;
use crate::error::AppResult;
use crate::secrets;

/// `setting_kv` key for the global gateway enable flag (JSON bool). Absent =
/// OFF (the gateway does not auto-start).
pub const ENABLED_KEY: &str = "orchestration.gateway.enabled";
/// `setting_kv` key for the configured loopback port (JSON number). Absent =
/// [`super::GATEWAY_PORT`] (18777).
pub const PORT_KEY: &str = "orchestration.gateway.port";
/// Keychain id for the loopback auth token. Lives under `<data_dir>/keychain/`.
const LOOPBACK_TOKEN_KEY: &str = "gateway-loopback-token";

/// Gateway runtime state. Mirrors the four UI states exactly. Serialized for
/// IPC (snake_case); the token is never part of any serialized struct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GatewayRuntimeState {
    Stopped,
    Starting,
    Running,
    Error,
}

impl GatewayRuntimeState {
    /// Stable lowercase wire string (matches the serde rename).
    pub fn as_str(&self) -> &'static str {
        match self {
            GatewayRuntimeState::Stopped => "stopped",
            GatewayRuntimeState::Starting => "starting",
            GatewayRuntimeState::Running => "running",
            GatewayRuntimeState::Error => "error",
        }
    }
}

/// Credential-free runtime snapshot (no DB access). The `gateway_get_status`
/// command composes this with the persisted flag/port + DB-sourced agent list.
#[derive(Debug, Clone)]
pub struct GatewaySnapshot {
    pub state: GatewayRuntimeState,
    pub base_url: String,
    /// Currently-bound port (0 when not running).
    pub port: u16,
    pub started_at: Option<i64>,
    pub last_error: Option<String>,
}

struct GatewayInner {
    state: GatewayRuntimeState,
    handle: Option<super::GatewayHandle>,
    last_error: Option<String>,
    /// Previously-bound listeners kept alive across a port rebind so that an
    /// agent whose config rewrite FAILED still reaches a live listener (its
    /// config still points at the old port). Drained on the next clean stop /
    /// app quit. Without this, a partial config rewrite on a port change would
    /// leave some agents pointing at a drained (dead) listener.
    retired: Vec<super::GatewayHandle>,
}

/// Process-wide gateway control handle. Stored in [`AppState`](crate::AppState).
/// Cloneable — all fields are
/// `Arc`, so cheap to clone into commands and the spawn task.
#[derive(Clone)]
pub struct GatewayControl {
    inner: Arc<Mutex<GatewayInner>>,
    /// Loopback auth token shared with [`GatewayState`](super::GatewayState).
    /// `dispatch` reads this per request; rotation is a single write.
    pub token: Arc<RwLock<String>>,
}

impl GatewayControl {
    /// Build a control handle around an already-loaded token (see
    /// [`gateway_loopback_token`]). An empty token means "no token yet" — the
    /// gateway will fail-closed until one is generated.
    pub fn new(token: String) -> Self {
        Self {
            inner: Arc::new(Mutex::new(GatewayInner {
                state: GatewayRuntimeState::Stopped,
                handle: None,
                last_error: None,
                retired: Vec::new(),
            })),
            token: Arc::new(RwLock::new(token)),
        }
    }

    /// Read-only runtime snapshot. Locks briefly; never blocks on the network.
    pub async fn snapshot(&self) -> GatewaySnapshot {
        let g = self.inner.lock().await;
        GatewaySnapshot {
            state: g.state,
            base_url: g.handle.as_ref().map(|h| h.base_url()).unwrap_or_default(),
            port: g.handle.as_ref().map(|h| h.addr.port()).unwrap_or(0),
            started_at: g.handle.as_ref().map(|h| h.started_at),
            last_error: g.last_error.clone(),
        }
    }

    /// Bind `port` (loopback only), rebuild the model catalog, spawn the accept
    /// loop + a watcher, and transition to `Running`. On bind failure
    /// transitions to `Error` and returns `Err`. Idempotent: a no-op when
    /// already `Running`. Serialized — concurrent callers wait on the bind.
    pub async fn start(&self, state: super::GatewayState, port: u16) -> AppResult<()> {
        // ONE critical section from the Running check through spawn + state
        // set. Releasing the lock between them let two concurrent starts
        // both pass the check; the loser's failed bind then marked the LIVE
        // gateway as Error, wedging restarts against a running listener.
        let mut g = self.inner.lock().await;
        if g.state == GatewayRuntimeState::Running && g.handle.is_some() {
            return Ok(());
        }
        g.state = GatewayRuntimeState::Starting;
        g.last_error = None;
        match super::spawn(state, port).await {
            Ok((handle, join)) => {
                let watched_addr = handle.addr;
                g.handle = Some(handle);
                g.state = GatewayRuntimeState::Running;
                drop(g);
                self.spawn_watcher(watched_addr, join);
                Ok(())
            }
            Err(e) => {
                g.state = GatewayRuntimeState::Error;
                g.last_error = Some(e.to_string());
                Err(e)
            }
        }
    }

    /// Swap in a freshly-bound listener (for a port change) and retire the
    /// previous one into `retired` (kept alive so a failed config rewrite on
    /// some agent still reaches a live listener at the old port). The caller
    /// has already `spawn`-bound the new listener and rewritten configs; this
    /// just makes the new handle active. The retired listener is drained on the
    /// next [`stop`] / app quit.
    pub async fn install_and_retire(
        &self,
        new: super::GatewayHandle,
        join: tauri::async_runtime::JoinHandle<()>,
    ) {
        let watched_addr = new.addr;
        {
            let mut g = self.inner.lock().await;
            if let Some(prev) = g.handle.replace(new) {
                g.retired.push(prev);
            }
            g.state = GatewayRuntimeState::Running;
            g.last_error = None;
        }
        self.spawn_watcher(watched_addr, join);
    }

    /// Spawn a watcher for one accept-loop `JoinHandle`. When that loop exits,
    /// mark `Error` ONLY if the still-active handle is the one we're watching
    /// (by bound addr) — a retired listener's exit must not flip the state.
    fn spawn_watcher(
        &self,
        watched_addr: std::net::SocketAddr,
        join: tauri::async_runtime::JoinHandle<()>,
    ) {
        let inner = self.inner.clone();
        tauri::async_runtime::spawn(async move {
            let _ = join.await;
            let mut g = inner.lock().await;
            // Only flag Error if the ACTIVE handle is the one whose loop just
            // ended. A retired listener ending (drained earlier, or superseded)
            // is expected and must not change the visible state.
            if g.state == GatewayRuntimeState::Running
                && g.handle.as_ref().map(|h| h.addr) == Some(watched_addr)
            {
                g.state = GatewayRuntimeState::Error;
                g.last_error = Some("gateway accept loop exited unexpectedly".into());
                g.handle = None;
            }
        });
    }

    /// Stop the gateway: mark `Stopped`, take the handle + any retired
    /// listeners, and drain them all. Idempotent.
    pub async fn stop(&self) {
        let (handle, retired) = {
            let mut g = self.inner.lock().await;
            // Set Stopped BEFORE taking the handles so the watchers do not flag
            // this drain as an unexpected exit.
            g.state = GatewayRuntimeState::Stopped;
            g.last_error = None;
            (g.handle.take(), std::mem::take(&mut g.retired))
        };
        if let Some(h) = handle {
            h.shutdown().await;
        }
        for h in retired {
            h.shutdown().await;
        }
    }

    /// Stop then start (e.g. after a port change). Token rotation does NOT use
    /// this — it writes the shared `RwLock` and rewrites configs in place.
    pub async fn restart(&self, state: super::GatewayState, port: u16) -> AppResult<()> {
        self.stop().await;
        self.start(state, port).await
    }

    /// Exit-path drain for the tray/RunEvent shutdown paths: take the active
    /// handle (and any retired listeners) without async-lock contention. The
    /// caller drains the returned active handle on the runtime; retired
    /// listeners simply drop (their accept loops end with the process).
    pub fn try_take_for_shutdown(&self) -> Option<super::GatewayHandle> {
        match self.inner.try_lock() {
            Ok(mut g) => {
                g.state = GatewayRuntimeState::Stopped;
                let _ = std::mem::take(&mut g.retired); // drop retired; process exiting
                g.handle.take()
            }
            Err(_) => None,
        }
    }

    /// Rotate the loopback token in memory (takes effect on the next request)
    /// and persist it to the keychain. Caller rewrites agent configs after.
    pub async fn set_token(&self, token: String) -> AppResult<()> {
        secrets::set(LOOPBACK_TOKEN_KEY, &token)?;
        *self.token.write().await = token;
        Ok(())
    }
}

// ---- persisted settings (non-secret) --------------------------------------

/// Read the global enable flag. `None` = unset (treated as OFF by callers).
pub fn read_enabled(conn: &rusqlite::Connection) -> Option<bool> {
    db::get_setting(conn, ENABLED_KEY)
        .ok()
        .flatten()
        .and_then(|v| v.as_bool())
}

/// Read the configured port. `None` = unset → caller uses [`super::GATEWAY_PORT`].
pub fn read_port(conn: &rusqlite::Connection) -> Option<u16> {
    db::get_setting(conn, PORT_KEY)
        .ok()
        .flatten()
        .and_then(|v| v.as_u64())
        .and_then(|n| if (1..=65535).contains(&n) { Some(n as u16) } else { None })
}

// ---- loopback token (keychain) --------------------------------------------

/// Get-or-generate the loopback token. The token is 32 random bytes (OsRng),
/// hex-encoded, stored encrypted at rest via [`secrets::set`]. It never enters
/// the DB, a `Serialize` projection, or a log. Returned to the local UI only by
/// the explicit `gateway_token_get` reveal command.
pub fn gateway_loopback_token() -> AppResult<String> {
    if let Some(Some(t)) = secrets::get(LOOPBACK_TOKEN_KEY).ok() {
        if !t.is_empty() {
            return Ok(t);
        }
    }
    let token = generate_token();
    secrets::set(LOOPBACK_TOKEN_KEY, &token)?;
    Ok(token)
}

/// Regenerate a fresh token (does not read the old one). Caller rotates the
/// in-memory `RwLock` + rewrites agent configs.
pub fn regenerate_token() -> AppResult<String> {
    let token = generate_token();
    secrets::set(LOOPBACK_TOKEN_KEY, &token)?;
    Ok(token)
}

fn generate_token() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    hex_encode(&buf)
}

fn hex_encode(b: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(b.len() * 2);
    for byte in b {
        // unwrap: writing to a String never fails
        let _ = write!(s, "{byte:02x}");
    }
    s
}

/// Probe `127.0.0.1` for the first free port after `start`, scanning up to
/// `range` candidates. Used only by the user-triggered "Auto-pick" action — the
/// default port is always tried first and a conflict surfaces as `Error`. A
/// tiny TOCTOU window between probe and real bind is recoverable: a race
/// surfaces as a bind error at [`spawn`](super::spawn) → `Error` → retry.
pub fn find_free_loopback_port(start: u16, range: u16) -> Option<u16> {
    let mut port = start.wrapping_add(1);
    for _ in 0..range {
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Some(port);
        }
        port = port.wrapping_add(1);
    }
    None
}

/// Constant-time string equality. A length mismatch returns `false` early (the
/// length of a token is not a meaningful secret). Used by inbound auth so a
/// timing oracle can't recover the token byte-by-byte.
pub(crate) fn ct_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.as_bytes().iter().zip(b.as_bytes().iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests;
