use crate::error::{AppError, AppResult};
use crate::config_writer::backup_path_for;
use crate::db;
use std::sync::Arc;
use super::agents::do_switch_provider;
use super::{run_blocking, snapshot_state, AppParts};
use tauri::State;

// ---- gateway: per-agent opt-in + status -----------------------------------
//
// The gateway runs unconditionally (lib.rs spawns it at startup); per-agent
// opt-in is a config-write concern. When `agent_set_gateway_enabled(id, true)`
// runs, the agent's ConfigAdapter writes the gateway's stable alias as its
// base_url (via `apply_gateway_set`) instead of the real upstream. Toggling
// off restores the direct-config binding. The opt-in flag itself lives in
// `setting_kv` (no schema change needed): key `orchestration.gateway.<id>`.

/// `setting_kv` key prefix for the per-agent gateway opt-in flag.
const GW_FLAG_PREFIX: &str = "orchestration.gateway.";

fn gw_flag_key(agent_id: &str) -> String {
    format!("{GW_FLAG_PREFIX}{agent_id}")
}

/// Read the gateway opt-in flag for one agent. `false` when unset.
fn gateway_enabled_for(conn: &rusqlite::Connection, agent_id: &str) -> bool {
    db::get_setting(conn, &gw_flag_key(agent_id))
        .ok()
        .flatten()
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Toggle gateway routing for an agent. When enabling, writes the stable
/// gateway alias into the agent's config (the agent then talks to the gateway,
/// which resolves the real upstream per-task). When disabling, restores the
/// direct-config binding (the active provider's real URL + key). Enabling is
/// GATED on the gateway service being `Running` — the UI surfaces this as a
/// disabled Routed segment (BLOCKED) when the service is down.
#[tauri::command]
pub async fn agent_set_gateway_enabled(
    state: State<'_, crate::AppState>,
    agent_id: String,
    enabled: bool,
) -> AppResult<()> {
    use crate::orchestration::gateway::control::GatewayRuntimeState;

    // Fetch the live base URL + token BEFORE the blocking config write. Both
    // come from the running gateway; if it isn't Running we refuse the enable
    // (the flag is intent-only until the service is up).
    let (base_url, token) = if enabled {
        let snap = state.gateway.snapshot().await;
        if snap.state != GatewayRuntimeState::Running || snap.base_url.is_empty() {
            return Err(AppError::Internal(
                "gateway not running — enable the Gateway service first".into(),
            ));
        }
        let token = state.gateway.token.read().await.clone();
        (snap.base_url, token)
    } else {
        (String::new(), String::new())
    };

    let parts = snapshot_state(&state);
    let db = state.db.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;

        // Persist the intent flag first so the UI reflects the toggle
        // immediately even if the config write below fails (the next enable
        // attempt re-runs when the gateway is up).
        db::set_setting(&conn, &gw_flag_key(&agent_id), &serde_json::json!(enabled))?;
        drop(conn);

        if enabled {
            let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
            write_gateway_alias_blocking(&conn, &agent_id, &base_url, &token)?;
        } else {
            apply_direct_config_blocking(&parts, &agent_id)?;
        }
        Ok(())
    })
    .await
}

/// Write the stable gateway alias into the agent's config (`base_url` + the
/// loopback `token` as its key slot). `base_url` is the gateway's live
/// `http://127.0.0.1:<port>` (fetched from the control snapshot by the caller);
/// `token` is the real loopback secret (NOT the legacy `"nestra"` sentinel).
/// The alias embeds an agent-id path prefix so the dispatcher can identify
/// the agent from the request path without agent-specific headers.
///
/// The alias advertises the REAL steady-state model abilities (context window
/// above all) so the agent doesn't fall back to its 200k guess — see
/// [`build_gateway_alias`].
fn write_gateway_alias_blocking(
    conn: &rusqlite::Connection,
    agent_id: &str,
    base_url: &str,
    token: &str,
) -> AppResult<()> {
    let spec = crate::agents::agent_spec(agent_id)
        .ok_or_else(|| AppError::NotFound(format!("unknown agent '{agent_id}'")))?;
    let adapter = crate::agents::adapter_for(spec.config.writer)
        .ok_or_else(|| AppError::Internal(format!("no adapter for '{}'", spec.config.writer)))?;
    let config_path = crate::db::home_dir()?.join(&spec.config.relative_path);
    let prefixed_base = format!("{base_url}/{agent_id}");
    let alias = build_gateway_alias(conn, agent_id, &prefixed_base, token);
    adapter.apply_gateway_set(&config_path, &alias)?;
    tracing::info!("agent '{agent_id}' now routed via gateway {base_url}");
    Ok(())
}

/// Build the abilities-aware gateway alias. Resolves the steady-state route
/// (main role; per-tier for Claude Code) and attaches the resolved model's
/// abilities so each writer can advertise the real context/output window.
///
/// Alias ids must be names the agent ACCEPTS locally before sending: Claude
/// Code validates model names (real CC tier ids), everything else takes the
/// conventional `"nestra"`. The gateway rewrites the model to the resolved
/// one before hitting upstream in every case; the distinct Claude Code tier
/// ids additionally classify tier intent at the gateway (`tier:*` policies).
///
/// Best-effort throughout: a failed steady-state resolution (no endpoints,
/// empty catalog) degrades to abilities-less slots, which each writer renders
/// as conservative placeholders — the alias write itself never fails here.
fn build_gateway_alias(
    conn: &rusqlite::Connection,
    agent_id: &str,
    prefixed_base: &str,
    token: &str,
) -> crate::config_writer::GatewayAlias {
    use crate::orchestration::identity::BudgetTier;
    use crate::orchestration::router::{self, RouterInputs};

    // Fresh catalog so the advertised abilities match current endpoint config.
    // Best-effort: a rebuild failure leaves the previous catalog, which still
    // beats placeholder values.
    let _ = crate::orchestration::capability_registry::rebuild(conn);
    // Transient stores — same steady-state shape as `orch_resolve_preview`:
    // empty health (assume healthy), persisted quota, no affinity. Default
    // task-grain affinity records in memory only, so this write path has no
    // persisted side effects.
    let health = crate::orchestration::health::ProviderHealth::new();
    let quota = crate::orchestration::quota_state::load_all_from_db(conn)
        .unwrap_or_default();
    let affinity = crate::orchestration::router::RouteAffinity::new();
    let inputs = RouterInputs { conn, health: &health, quota: &quota, affinity: &affinity };
    let abilities = |tier: Option<&BudgetTier>| {
        router::steady_state(&inputs, agent_id, tier).and_then(|s| s.abilities)
    };

    match agent_id {
        "claude-code-cli" => {
            let tier_slot = |id: &str, t: BudgetTier| crate::config_writer::AliasModel {
                id: id.to_string(),
                abilities: abilities(Some(&t)),
            };
            let sonnet = tier_slot("claude-sonnet-4-5", BudgetTier::Sonnet);
            crate::config_writer::GatewayAlias {
                gateway_base_url: prefixed_base.to_string(),
                // Primary = the sonnet-class slot (Claude Code's main thread).
                model_alias: sonnet.clone(),
                tier_aliases: Some(crate::config_writer::TierAliases {
                    haiku: tier_slot("claude-haiku-4-5", BudgetTier::Haiku),
                    sonnet,
                    opus: tier_slot("claude-opus-4-5", BudgetTier::Opus),
                }),
                sentinel_key: token.to_string(),
            }
        }
        _ => crate::config_writer::GatewayAlias {
            gateway_base_url: prefixed_base.to_string(),
            model_alias: crate::config_writer::AliasModel {
                id: "nestra".to_string(),
                abilities: abilities(None),
            },
            tier_aliases: None,
            sentinel_key: token.to_string(),
        },
    }
}

/// Re-write the gateway alias for one agent IF it is flagged routed and the
/// gateway is Running. Called after edits that change the steady-state route
/// (policy edits, endpoint model/ability edits, binding switches) so the
/// agent's advertised context window tracks reality. Best-effort: failures
/// log a warning and surface nothing — the next toggle/gateway lifecycle op
/// re-applies the alias anyway.
///
/// ponytail: steady-state advertisement, not live tracking — a mid-task
/// migration to a smaller-window model is NOT re-advertised (the agent keeps
/// its perception until the next refresh; the migration row already marks
/// `generation_broken` honestly). Upgrade path: push an abilities update to
/// agents that support one.
pub(crate) async fn refresh_alias_if_routed(state: &State<'_, crate::AppState>, agent_id: &str) {
    use crate::orchestration::gateway::control::GatewayRuntimeState;
    let snap = state.gateway.snapshot().await;
    if snap.state != GatewayRuntimeState::Running || snap.base_url.is_empty() {
        return;
    }
    let routed = {
        let conn = state
            .db_read
            .lock()
            .map_err(|e| AppError::Internal(e.to_string()));
        match conn {
            Ok(conn) => gateway_enabled_for(&conn, agent_id),
            Err(e) => {
                tracing::warn!("alias refresh: db read failed for '{agent_id}': {e}");
                return;
            }
        }
    };
    if !routed {
        return;
    }
    let token = state.gateway.token.read().await.clone();
    let db = state.db.clone();
    let aid = agent_id.to_string();
    let base_url = snap.base_url;
    let res = run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        write_gateway_alias_blocking(&conn, &aid, &base_url, &token)
    })
    .await;
    if let Err(e) = res {
        tracing::warn!("alias refresh failed for agent '{agent_id}': {e}");
    }
}

/// Refresh the alias of EVERY intent-flagged agent (gateway Running only).
/// Used after endpoint edits that can change any routed agent's steady-state
/// model.
pub(crate) async fn refresh_all_routed(state: &State<'_, crate::AppState>) {
    for aid in enabled_agent_ids(state).await.unwrap_or_default() {
        refresh_alias_if_routed(state, &aid).await;
    }
}

/// Restore direct-config mode: re-apply the agent's active provider binding
/// so it talks directly to the upstream again. Reuses the existing switch path
/// (`do_switch_provider`), which resolves the config path + writes the real
/// upstream URL + key.
fn apply_direct_config_blocking(
    parts: &AppParts,
    agent_id: &str,
) -> AppResult<()> {
    let conn = parts.db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
    let agent = db::list_agents(&conn)?
        .into_iter()
        .find(|a| a.id == agent_id)
        .ok_or_else(|| AppError::NotFound(format!("agent '{agent_id}' not in DB")))?;
    drop(conn);
    match agent.active_provider_id {
        Some(ep_id) => {
            let ep_id_log = ep_id.clone();
            // Re-run the single-slot switch to write the real upstream back.
            do_switch_provider(parts, agent_id.to_string(), ep_id)?;
            tracing::info!("agent '{agent_id}' restored to direct-config (endpoint {ep_id_log})");
            Ok(())
        }
        None => {
            // No active binding — restore the agent's pre-Nestra config. If no
            // backup exists the config was never managed (or already restored):
            // toggling to direct is a no-op, not an error.
            let spec = crate::agents::agent_spec(agent_id)
                .ok_or_else(|| AppError::NotFound(format!("unknown agent '{agent_id}'")))?;
            let adapter = crate::agents::adapter_for(spec.config.writer)
                .ok_or_else(|| AppError::Internal("no adapter".into()))?;
            let config_path = crate::db::home_dir()?.join(&spec.config.relative_path);
            if backup_path_for(&config_path).exists() {
                adapter.restore(&config_path)?;
            }
            Ok(())
        }
    }
}

/// Live gateway status (back-comat shim): `up`/`base_url`/`agents_enabled`,
/// derived from the new control snapshot. Retained for the migration period so
/// the existing `GatewayStatusBar` keeps working; the rich surface is
/// [`gateway_get_status`]. Safe to remove once the UI migrates fully.
#[tauri::command]
pub async fn orch_status(state: State<'_, crate::AppState>) -> AppResult<GatewayStatus> {
    let snap = state.gateway.snapshot().await;
    let agents_enabled = enabled_agent_ids(&state).await.unwrap_or_default();
    Ok(GatewayStatus {
        up: snap.state == crate::orchestration::gateway::control::GatewayRuntimeState::Running,
        base_url: snap.base_url,
        agents_enabled,
    })
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct GatewayStatus {
    pub up: bool,
    pub base_url: String,
    pub agents_enabled: Vec<String>,
}

// ---- Gateway Service control surface --------------------------------------
//
// The Service itself: global enable, port (with Hybrid fallback), loopback
// token, restart, and rich runtime status. Distinct from routing policy /
// provider health — this owns ONLY the gateway process. Runtime state
// (liveness, started_at, last_error) is in-memory; only the enable flag +
// configured port persist (token in the keychain, never the DB).

/// Rich runtime status for the `/gateway` page. Credential-free: the token is
/// `has_token: bool` only — the plaintext is fetched on explicit Reveal via
/// [`gateway_token_get`], never cached in this struct (and thus never in the
/// React Query cache).
#[derive(Debug, Clone, serde::Serialize)]
pub struct GatewayServiceStatus {
    /// `stopped` | `starting` | `running` | `error`.
    pub state: String,
    /// Global enable flag (persisted intent).
    pub enabled: bool,
    /// Configured port (persisted; default 18777).
    pub configured_port: u16,
    /// Live `http://127.0.0.1:<port>` when running, else empty.
    pub bound_base_url: String,
    /// Epoch-ms the active listener bound (scopes session counters). null down.
    pub started_at: Option<i64>,
    pub uptime_secs: Option<i64>,
    pub last_error: Option<String>,
    /// Agent ids whose routing intent flag is true (independent of liveness).
    pub agents_enabled: Vec<String>,
    /// True when a loopback token is configured (fail-closed otherwise).
    pub has_token: bool,
    pub stats: GatewayActivityStats,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct GatewayActivityStats {
    /// Routed requests observed since this gateway run started (real
    /// `route_request` rows; never synthesized).
    pub total_requests: i64,
    pub last_request_at: Option<i64>,
    /// Non-terminal tasks right now (`lifecycle NOT IN` the terminal set).
    pub active_tasks: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct GatewayToggleResult {
    pub ok: bool,
    /// Agents whose config was rewritten (Direct on OFF, alias on ON).
    pub reverted: Vec<String>,
    /// Agents whose config rewrite FAILED — surfaced in the UI, not hidden.
    /// On OFF these still point at the (now-stopped) gateway; the user must
    /// fix them (the intent flag is preserved so re-enabling retries).
    pub failed: Vec<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct GatewayPortResult {
    pub ok: bool,
    /// The port actually bound (equals the requested port on success, or the
    /// auto-picked port from [`gateway_autopick_port`]).
    pub bound_port: u16,
    pub failed: Vec<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct GatewayTokenInfo {
    pub has_token: bool,
    /// Plaintext loopback token. Returned ONLY by this explicit reveal command
    /// (local UI); never part of `GatewayServiceStatus` or any cached query.
    pub token: String,
}

/// Read the rich runtime status. Composes the in-memory control snapshot with
/// persisted flag/port + DB-sourced agent list + real `route_request` counters.
#[tauri::command]
pub async fn gateway_get_status(
    state: State<'_, crate::AppState>,
) -> AppResult<GatewayServiceStatus> {
    let snap = state.gateway.snapshot().await;
    let now = chrono::Utc::now().timestamp_millis();
    let (enabled, configured_port, agents_enabled) = {
        let conn = state
            .db_read
            .lock()
            .map_err(|e| AppError::Internal(e.to_string()))?;
        let enabled = crate::orchestration::gateway::control::read_enabled(&conn).unwrap_or(false);
        let port = crate::orchestration::gateway::control::read_port(&conn)
            .unwrap_or(crate::orchestration::gateway::GATEWAY_PORT);
        let agents = enabled_agent_ids_from(&conn);
        (enabled, port, agents)
    };
    let (total_requests, last_request_at, active_tasks) = {
        let conn = state
            .db_read
            .lock()
            .map_err(|e| AppError::Internal(e.to_string()))?;
        let (count, last) = crate::orchestration::store::gateway_session_stats(&conn, snap.started_at)?;
        let active = crate::orchestration::store::active_task_count(&conn)?;
        (count, last, active)
    };
    let has_token = !state.gateway.token.read().await.is_empty();
    let uptime_secs = snap.started_at.map(|s| ((now - s) / 1000).max(0));
    Ok(GatewayServiceStatus {
        state: snap.state.as_str().to_string(),
        enabled,
        configured_port,
        bound_base_url: snap.base_url,
        started_at: snap.started_at,
        uptime_secs,
        last_error: snap.last_error,
        agents_enabled,
        has_token,
        stats: GatewayActivityStats {
            total_requests,
            last_request_at,
            active_tasks,
        },
    })
}

/// Global enable/disable. ON spawns the gateway + re-applies the alias to every
/// agent whose intent flag is true. OFF reverts every flagged agent to Direct
/// FIRST (so configs never point at a stopped listener), then stops the
/// listener. Partial failures on either path are collected into `failed` and
/// surfaced; the intent flags are always preserved.
#[tauri::command]
pub async fn gateway_set_enabled(
    state: State<'_, crate::AppState>,
    enabled: bool,
) -> AppResult<GatewayToggleResult> {
    // Persist the flag first so the UI reflects intent immediately.
    {
        let db = state.db.clone();
        let key = crate::orchestration::gateway::control::ENABLED_KEY.to_string();
        run_blocking(move || {
            let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
            db::set_setting(&conn, &key, &serde_json::json!(enabled))
        })
        .await?;
    }

    if enabled {
        // ON: spawn on the configured port, then bulk-apply aliases.
        let port = read_configured_port(&state).await?;
        let gw_state = build_gateway_state(&state)?;
        if let Err(e) = state.gateway.start(gw_state, port).await {
            return Ok(GatewayToggleResult {
                ok: false,
                reverted: vec![],
                failed: vec![],
                error: Some(e.to_string()),
            });
        }
        let (base_url, token) = live_base_url_and_token(&state).await?;
        let (applied, failed) = rewrite_all_flagged(&state, &base_url, &token, true).await?;
        Ok(GatewayToggleResult {
            ok: true,
            reverted: applied,
            failed,
            error: None,
        })
    } else {
        // OFF: revert to Direct first, then stop the listener.
        let (reverted, failed) = rewrite_all_flagged(&state, "", "", false).await?;
        state.gateway.stop().await;
        Ok(GatewayToggleResult {
            ok: true,
            reverted,
            failed,
            error: None,
        })
    }
}

/// The N most-recent routed requests (credential-free `RouteRecord`
/// projections), newest-first. Real `route_request` data for the Gateway page's
/// activity list — never synthesized; an empty result is shown as-is.
#[tauri::command]
pub fn gateway_recent_activity(
    state: State<'_, crate::AppState>,
    limit: Option<i64>,
) -> AppResult<Vec<crate::orchestration::identity::RouteRecord>> {
    let conn = state
        .db_read
        .lock()
        .map_err(|e| AppError::Internal(e.to_string()))?;
    crate::orchestration::store::recent_route_requests(&conn, limit.unwrap_or(10))
}

/// Read the live gateway tuning (timeouts + circuit-breaker parameters).
#[tauri::command]
pub fn gateway_tuning_get(
    state: State<'_, crate::AppState>,
) -> AppResult<crate::orchestration::gateway::tuning::GatewayTuning> {
    Ok(crate::orchestration::gateway::tuning::snapshot(
        &state.gateway_tuning,
    ))
}

/// Update the tuning: persist (clamped) to `setting_kv` AND write the shared
/// in-memory slot — applies to the next request with no gateway restart.
#[tauri::command]
pub fn gateway_tuning_set(
    state: State<'_, crate::AppState>,
    tuning: crate::orchestration::gateway::tuning::GatewayTuning,
) -> AppResult<crate::orchestration::gateway::tuning::GatewayTuning> {
    let clamped = tuning.clamped();
    {
        let conn = state
            .db
            .lock()
            .map_err(|e| AppError::Internal(e.to_string()))?;
        clamped.save(&conn)?;
    }
    if let Ok(mut slot) = state.gateway_tuning.write() {
        *slot = clamped;
    }
    Ok(clamped)
}

/// Live per-endpoint breaker snapshot for the Providers page health badges.
#[tauri::command]
pub fn provider_health_snapshot(
    state: State<'_, crate::AppState>,
) -> AppResult<Vec<crate::orchestration::health::EndpointHealthSnap>> {
    Ok(state.orch_health.snapshot_all())
}

/// Reset all breaker state (the "reset health" action) — every endpoint
/// becomes eligible immediately.
#[tauri::command]
pub fn provider_health_reset(state: State<'_, crate::AppState>) -> AppResult<()> {
    state.orch_health.clear();
    Ok(())
}

/// Restart the gateway on the configured port (e.g. after a config change that
/// does not alter port/token). Token rotation does NOT need this — it writes
/// the shared token RwLock in place.
#[tauri::command]
pub async fn gateway_restart(state: State<'_, crate::AppState>) -> AppResult<GatewayToggleResult> {
    let port = read_configured_port(&state).await?;
    let gw_state = build_gateway_state(&state)?;
    state.gateway.restart(gw_state, port).await?;
    Ok(GatewayToggleResult {
        ok: true,
        reverted: vec![],
        failed: vec![],
        error: None,
    })
}

/// Change the configured port and (if running) rebind. Ordering is fail-safe:
/// bind the NEW listener first → rewrite all configs to the new URL → retire
/// the OLD listener (kept alive until next stop/quit so a failed rewrite on
/// some agent still reaches a live port). A bind failure leaves the old
/// listener + old configs untouched (no divergence).
#[tauri::command]
pub async fn gateway_set_port(
    state: State<'_, crate::AppState>,
    port: u16,
) -> AppResult<GatewayPortResult> {
    if !(1..=65535).contains(&port) {
        return Err(AppError::Validation(format!("port {port} out of range 1..=65535")));
    }
    persist_configured_port(&state, port).await?;

    let snap = state.gateway.snapshot().await;
    use crate::orchestration::gateway::control::GatewayRuntimeState;
    if snap.state != GatewayRuntimeState::Running {
        // Not running — the port takes effect on the next start.
        return Ok(GatewayPortResult {
            ok: true,
            bound_port: port,
            failed: vec![],
            error: None,
        });
    }

    // Bind the new listener (old still serving).
    let gw_state = build_gateway_state(&state)?;
    let (new_handle, join) = match crate::orchestration::gateway::spawn(gw_state, port).await {
        Ok(v) => v,
        Err(e) => {
            // Bind failed: the OLD listener is untouched and still serving on
            // its port — return that so the UI shows the still-live port.
            return Ok(GatewayPortResult {
                ok: false,
                bound_port: snap.port,
                failed: vec![],
                error: Some(e.to_string()),
            });
        }
    };
    let new_url = new_handle.base_url();
    let token = state.gateway.token.read().await.clone();
    // Rewrite all configs to the new URL. Per-agent failures are surfaced; the
    // old listener is retired regardless (kept alive in `retired` so a failed
    // agent still reaches the old port).
    let (_applied, failed) = rewrite_all_flagged_to_url(&state, &new_url, &token).await?;
    let failed_empty = failed.is_empty();
    let error = if failed_empty {
        None
    } else {
        Some(format!("{} agent config(s) could not be rewritten", failed.len()))
    };
    state.gateway.install_and_retire(new_handle, join).await;
    Ok(GatewayPortResult {
        ok: failed_empty,
        bound_port: port,
        failed,
        error,
    })
}

/// Hybrid fallback: find the next free loopback port after the configured one
/// and rebind to it (user-triggered — the default port always tries first and a
/// conflict surfaces as `error`). Persists the chosen port so it sticks across
/// restarts; "Reset to default" is a subsequent `gateway_set_port(18777)`.
#[tauri::command]
pub async fn gateway_autopick_port(state: State<'_, crate::AppState>) -> AppResult<GatewayPortResult> {
    let base = read_configured_port(&state).await?;
    let Some(port) = crate::orchestration::gateway::control::find_free_loopback_port(base, 50)
    else {
        return Ok(GatewayPortResult {
            ok: false,
            bound_port: base,
            failed: vec![],
            error: Some(format!("no free loopback port in {}..{}", base + 1, base + 50)),
        });
    };
    gateway_set_port(state, port).await
}

/// Reveal the loopback token (masked in the UI; this returns plaintext for the
/// explicit Reveal/Copy action). Local-only — the token never rides in the
/// cached `GatewayServiceStatus`.
#[tauri::command]
pub async fn gateway_token_get(state: State<'_, crate::AppState>) -> AppResult<GatewayTokenInfo> {
    let token = state.gateway.token.read().await.clone();
    Ok(GatewayTokenInfo {
        has_token: !token.is_empty(),
        token,
    })
}

/// Regenerate the loopback token. Writes the new value to the keychain + the
/// shared token RwLock (so the very next inbound request requires it — old
/// token is rejected immediately, no restart), then rewrites every routed
/// agent's config with the new token. In-flight requests already past auth are
/// unaffected.
#[tauri::command]
pub async fn gateway_token_regenerate(
    state: State<'_, crate::AppState>,
) -> AppResult<GatewayToggleResult> {
    let new_token = crate::orchestration::gateway::control::regenerate_token()?;
    state.gateway.set_token(new_token.clone()).await?;
    // Rewrite configs only if the gateway is up (agents route through it then).
    let snap = state.gateway.snapshot().await;
    use crate::orchestration::gateway::control::GatewayRuntimeState;
    if snap.state == GatewayRuntimeState::Running && !snap.base_url.is_empty() {
        let (applied, failed) =
            rewrite_all_flagged_to_url(&state, &snap.base_url, &new_token).await?;
        let failed_empty = failed.is_empty();
        let error = if failed_empty {
            None
        } else {
            Some(format!("{} agent config(s) not updated", failed.len()))
        };
        Ok(GatewayToggleResult {
            ok: failed_empty,
            reverted: applied,
            failed,
            error,
        })
    } else {
        Ok(GatewayToggleResult {
            ok: true,
            reverted: vec![],
            failed: vec![],
            error: None,
        })
    }
}

// ---- gateway control helpers ----------------------------------------------

/// Agent ids whose routing intent flag is true (async, locks db_read).
async fn enabled_agent_ids(state: &State<'_, crate::AppState>) -> AppResult<Vec<String>> {
    let conn = state
        .db_read
        .lock()
        .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(enabled_agent_ids_from(&conn))
}

fn enabled_agent_ids_from(conn: &rusqlite::Connection) -> Vec<String> {
    crate::agents::agents()
        .iter()
        .filter(|a| gateway_enabled_for(conn, a.id))
        .map(|a| a.id.to_string())
        .collect()
}

async fn read_configured_port(state: &State<'_, crate::AppState>) -> AppResult<u16> {
    let conn = state
        .db_read
        .lock()
        .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(crate::orchestration::gateway::control::read_port(&conn)
        .unwrap_or(crate::orchestration::gateway::GATEWAY_PORT))
}

async fn persist_configured_port(state: &State<'_, crate::AppState>, port: u16) -> AppResult<()> {
    let db = state.db.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        db::set_setting(
            &conn,
            crate::orchestration::gateway::control::PORT_KEY,
            &serde_json::json!(port),
        )
    })
    .await
}

/// Build a fresh `GatewayState` (own DB conn + the shared health/quota/affinity
/// stores + credential_reader + the shared loopback token). One per lifecycle
/// op — the gateway task owns the connection after `start`.
fn build_gateway_state(
    state: &State<'_, crate::AppState>,
) -> AppResult<crate::orchestration::gateway::GatewayState> {
    let conn = crate::db::open(&crate::db::data_dir()?)?;
    Ok(crate::orchestration::gateway::GatewayState {
        db: Arc::new(tokio::sync::Mutex::new(conn)),
        health: state.orch_health.clone(),
        quota: state.orch_quota.clone(),
        affinity: state.orch_affinity.clone(),
        credential_reader: Arc::new(|endpoint_id| crate::secrets::get(endpoint_id)),
        loopback_token: state.gateway.token.clone(),
        tuning: state.gateway_tuning.clone(),
    })
}

/// Live base URL + token, fetched from the running gateway. Errors if not up.
async fn live_base_url_and_token(
    state: &State<'_, crate::AppState>,
) -> AppResult<(String, String)> {
    let snap = state.gateway.snapshot().await;
    use crate::orchestration::gateway::control::GatewayRuntimeState;
    if snap.state != GatewayRuntimeState::Running || snap.base_url.is_empty() {
        return Err(AppError::Internal(
            "gateway not running — enable it first".into(),
        ));
    }
    let token = state.gateway.token.read().await.clone();
    Ok((snap.base_url, token))
}

/// Rewrite every intent-flagged agent's config. `mode == true` writes the
/// gateway alias (Routed); `false` reverts to Direct. Returns `(ok_ids, fail_ids)`.
async fn rewrite_all_flagged(
    state: &State<'_, crate::AppState>,
    base_url: &str,
    token: &str,
    mode: bool,
) -> AppResult<(Vec<String>, Vec<String>)> {
    if mode {
        rewrite_all_flagged_to_url(state, base_url, token).await
    } else {
        let parts = snapshot_state(state);
        let flagged = run_blocking({
            let db = state.db.clone();
            move || {
                let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
                Ok(enabled_agent_ids_from(&conn))
            }
        })
        .await?;
        let mut ok = Vec::new();
        let mut fail = Vec::new();
        for aid in &flagged {
            match apply_direct_config_blocking(&parts, aid) {
                Ok(()) => ok.push(aid.clone()),
                Err(e) => {
                    tracing::warn!("OFF revert failed for agent '{aid}': {e}");
                    fail.push(aid.clone());
                }
            }
        }
        Ok((ok, fail))
    }
}

/// Rewrite every intent-flagged agent's config to `base_url` + `token`
/// (Routed). Returns `(ok_ids, fail_ids)`.
async fn rewrite_all_flagged_to_url(
    state: &State<'_, crate::AppState>,
    base_url: &str,
    token: &str,
) -> AppResult<(Vec<String>, Vec<String>)> {
    let base_url = base_url.to_string();
    let token = token.to_string();
    let db = state.db.clone();
    let flagged = run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        Ok(enabled_agent_ids_from(&conn))
    })
    .await?;
    let mut ok = Vec::new();
    let mut fail = Vec::new();
    for aid_orig in &flagged {
        let db = state.db.clone();
        let aid = aid_orig.clone();
        let base_url = base_url.clone();
        let token = token.clone();
        let res = run_blocking(move || {
            let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
            write_gateway_alias_blocking(&conn, &aid, &base_url, &token)
        })
        .await;
        // `aid` was moved into the closure; reuse the borrowed `aid_orig`.
        match res {
            Ok(()) => ok.push(aid_orig.clone()),
            Err(e) => {
                tracing::warn!("alias rewrite failed for agent '{}': {e}", aid_orig);
                fail.push(aid_orig.clone());
            }
        }
    }
    Ok((ok, fail))
}
