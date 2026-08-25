mod commands;
pub mod agents;
pub mod config_writer;
mod db;
mod endpoint_quota;
mod error;
mod protocol_url;
mod logging;
mod model_abilities;
mod mcp;
pub mod orchestration;
mod panic_hook;
mod quota_refresh;
mod review;
mod schema;
mod secrets;
mod session;
mod skills;
mod tray;

use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Manager, WindowEvent};

/// Single process-wide mutex that all home-scoped tests must hold while
/// touching `NESTRA_HOME_DIR`. Lifted to lib root so the session, skills, and
/// any future home-scoped test can share a single lock.
#[cfg(test)]
pub(crate) static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub struct AppState {
    pub db: Arc<Mutex<rusqlite::Connection>>,
    /// Read-only connection (WAL allows unlimited concurrent readers) for UI
    /// read commands. Routing reads through `db_read` (instead of `db`) means
    /// reads never serialize on the write mutex and don't stall behind the
    /// gateway's writes or session reconciliation. `query_only=ON` enforces
    /// read-only at the SQLite level.
    pub db_read: Arc<Mutex<rusqlite::Connection>>,
    /// Dedicated read-write connection for session reconciliation, so the
    /// (slow, disk-walking) reconcile never holds the UI's `db`/`db_read`
    /// locks. WAL lets it write concurrently with UI reads.
    pub reconcile_db: Arc<Mutex<rusqlite::Connection>>,
    /// Handle for the system tray icon, populated by `tray::install`.
    /// `None` until setup completes (and in tests).
    pub tray: Mutex<Option<tauri::tray::TrayIcon>>,
    /// Handle for the tray's "Launch at startup" check item, populated by
    /// `tray::install`. Kept so autostart toggles can flip the checkmark IN
    /// PLACE — rebuilding the menu mid-click destroys the native popup being
    /// processed on Windows and leaves the item grayed/stale.
    pub autostart_item: Mutex<Option<tauri::menu::CheckMenuItem<tauri::Wry>>>,
    /// Cached `AppHandle` for background workers (kept in a Mutex so the value
    /// can be replaced during setup, after `.manage()` has already cloned the
    /// state).
    pub app_handle: Mutex<Option<AppHandle>>,
    /// Whether the universal session store has been reconciled against disk at
    /// least once this process. The first session command triggers the scan.
    /// `Arc` so the check-and-reconcile can run off the UI thread.
    pub session_reconciled: Arc<Mutex<bool>>,
    /// Process-global orchestration stores the gateway feeds and the router
    /// reads. Built once at setup; cloned into each proxied request.
    /// `None` only briefly during setup before the gateway binds.
    pub orch_health: Arc<orchestration::health::ProviderHealth>,
    pub orch_quota: Arc<orchestration::quota_state::QuotaState>,
    pub orch_affinity: Arc<orchestration::router::RouteAffinity>,
    /// Live gateway tuning (timeouts + breaker parameters), shared with the
    /// gateway state and `ProviderHealth` — Settings edits hot-apply.
    pub gateway_tuning: orchestration::gateway::tuning::SharedTuning,
    /// The single active review session (Review Runtime R1 — one at a time).
    /// Clone handle; the runner task + abort command coordinate through it.
    pub reviews: review::ReviewRegistry,
    /// The running gateway handle, once spawned. `None` if the gateway failed
    /// to bind (the app still runs, just without routing). `Arc<tokio::sync::Mutex>`
    /// so the spawn task can store the handle after `.manage()` already cloned
    /// the state.
    pub gateway: orchestration::gateway::control::GatewayControl,
    /// Per-agent locks serializing every config-rewrite path (switch, mode
    /// toggle, alias refresh) — see `commands::AgentSwitchLocks`.
    pub switch_locks: commands::AgentSwitchLocks,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    logging::init();
    // Crash reports append to <log_dir>/crash.log; the rolling appenders
    // keep daily nestra.<date>.log / .json generations — see both modules.
    panic_hook::install();

    let data_dir = db::data_dir().expect("failed to resolve data dir");
    std::fs::create_dir_all(&data_dir).expect("failed to create data dir");
    let log_dir = db::log_dir().expect("failed to resolve log dir");
    std::fs::create_dir_all(&log_dir).expect("failed to create log dir");

    let conn = db::open(&data_dir).expect("failed to open database");
    db::migrate(&conn).expect("failed to run migrations");
    // Dedicated read-only connection so UI reads don't serialize on the write
    // mutex or block behind gateway writes / reconciliation (WAL readers).
    let read_conn = db::open_readonly(&data_dir).expect("failed to open read-only database");
    // Dedicated connection for session reconciliation (read-write) — keeps the
    // slow disk-walking reconcile off the UI's locks.
    let reconcile_conn = db::open(&data_dir).expect("failed to open reconcile database");

    let gateway_tuning = orchestration::gateway::tuning::shared_default();
    let orch_health = Arc::new(orchestration::health::ProviderHealth::with_tuning(
        gateway_tuning.clone(),
    ));
    let orch_quota = Arc::new(orchestration::quota_state::QuotaState::new());
    let orch_affinity = Arc::new(orchestration::router::RouteAffinity::new());
    // Restore the restart-persistent routing state (Smart Gateway fix 3):
    // session-grain affinity keeps a session on its prior provider (prompt-
    // cache locality across a Nestra restart), and the degraded-endpoint
    // circuit keeps a known-dead endpoint out of rotation. Both loads are
    // TTL/best-effort and touch only non-secret routing ids. `conn` is still
    // the plain pre-AppState connection here — no lock to take.
    orch_affinity.load_sessions(&conn);
    orch_health.load(&conn);
    // Tuning overrides land last so persisted breaker state restores under
    // default parameters, then the user's saved knobs take over.
    if let Ok(mut t) = gateway_tuning.write() {
        *t = orchestration::gateway::tuning::GatewayTuning::load(&conn);
    }
    // Restore the persisted log settings now that the DB is open
    // (logging::init ran before it): verbosity preset + full-body capture.
    // NESTRA_LOG, when set, wins for the preset.
    logging::apply_persisted_settings(&conn);

    // Loopback auth token: get-or-generate up front (encrypted keychain). The
    // gateway is fail-closed without it; generating at launch means it is ready
    // whenever the service is enabled, and `gateway_token_get` works even while
    // the gateway is stopped. NEVER stored in the DB.
    let gateway_control = orchestration::gateway::control::GatewayControl::new(
        orchestration::gateway::control::gateway_loopback_token()
            .unwrap_or_else(|e| {
                tracing::error!("gateway loopback token unavailable: {e}");
                String::new()
            }),
    );

    let state = AppState {
        db: Arc::new(Mutex::new(conn)),
        db_read: Arc::new(Mutex::new(read_conn)),
        reconcile_db: Arc::new(Mutex::new(reconcile_conn)),
        tray: Mutex::new(None),
        autostart_item: Mutex::new(None),
        app_handle: Mutex::new(None),
        session_reconciled: Arc::new(Mutex::new(false)),
        orch_health: orch_health.clone(),
        orch_quota: orch_quota.clone(),
        orch_affinity: orch_affinity.clone(),
        gateway_tuning: gateway_tuning.clone(),
        reviews: review::ReviewRegistry::default(),
        gateway: gateway_control,
        switch_locks: Default::default(),
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.show();
                let _ = win.set_focus();
            }
        }))
        .plugin(tauri_plugin_window_state::Builder::new().build())
        // Open URLs/files in the user's default handler. Used by the
        // update-check card to open the GitHub release page for download.
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        // Launch-at-login (Windows HKCU Run key / macOS LaunchAgent / Linux
        // XDG). The extra arg lands in the autostart entry so the app can tell
        // a login launch apart from a manual one and start hidden in the tray.
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--auto-launch"]),
        ))
        .manage(state)
        .setup(move |app| {
            // Autostart at login: start hidden in the tray instead of popping
            // a window over the user's session. Manual launches (no arg) show
            // the window as usual.
            if std::env::args().any(|a| a == "--auto-launch") {
                if let Some(win) = app.get_webview_window("main") {
                    let _ = win.hide();
                }
            }

            // Cache the live AppHandle so non-Tauri entry points (tray rebuild,
            // do_switch helpers called from menu events) can reach it.
            if let Ok(mut guard) = app.state::<AppState>().app_handle.lock() {
                *guard = Some(app.handle().clone());
            }

            // Prime CLI detection once at launch, unless cadence is "manual".
            let manual = app
                .state::<AppState>()
                .db
                .lock()
                .ok()
                .map(|c| commands::agents::detection_cadence(&c) == "manual")
                .unwrap_or(false);
            if !manual {
                if let Ok(conn) = app.state::<AppState>().db.lock() {
                    let _ = commands::agents::detect_all_agents(&conn);
                }
            }

            // Background first-launch session reconcile on its OWN connection
            // — doesn't block UI reads (db_read) or the gateway. Emits
            // `sessions-reconciled` when done so the frontend refreshes.
            commands::run_launch_reconcile(
                app.state::<AppState>().reconcile_db.clone(),
                app.state::<AppState>().session_reconciled.clone(),
                app.handle().clone(),
            );

            // Install system tray (close-to-tray + quick-switch menu).
            if let Err(e) = tray::install(app) {
                tracing::warn!("system tray install failed: {e}");
            }

            // Close button hides to tray; Quit (tray menu) flips REALLY_QUIT
            // first so the same handler lets it through.
            if let Some(win) = app.get_webview_window("main") {
                let win_clone = win.clone();
                win.on_window_event(move |event| {
                    if let WindowEvent::CloseRequested { api, .. } = event {
                        if tray::really_quit() {
                            return;
                        }
                        api.prevent_close();
                        let _ = win_clone.hide();
                    }
                });
            }

            // Spawn the 5h-quota refresh worker on its own OS thread. It
            // observes each enabled endpoint's quota on a slow interval
            // and fires one ping when the 5h window has elapsed — z.ai
            // resets on the next request, so the ping IS that request.
            // Tray "Quit" flips `quota_refresh::request_exit()` so the
            // loop unwinds cleanly. A separate Connection keeps the
            // worker's network calls off the UI's DB lock. The shared
            // `orch_quota` store is passed in so a successful reset ping
            // also clears endpoint exhaustion in the router's view.
            let worker_conn = db::open(&data_dir)
                .map_err(|e| Box::<dyn std::error::Error>::from(e.to_string()))?;
            quota_refresh::spawn_worker(
                Arc::new(Mutex::new(worker_conn)),
                orch_quota.clone(),
            );

            // Spawn the orchestration gateway ONLY when the global enable flag
            // is set (default OFF — see `orchestration.gateway.enabled`).
            // Per-agent opt-in (`orchestration.gateway.<id>`) is a separate
            // config-write concern. The gateway binds the configured loopback
            // port (default 18777), runs on the Tauri async runtime, and stops
            // on app quit via the control handle. A dedicated DB connection
            // keeps the gateway's proxied request reads off the UI's DB lock.
            //
            // One-time seed: if the enable flag is unset but any agent is
            // already opted into routing (a legacy always-on install), default
            // it ON so an existing routed setup isn't silently broken by the
            // new default-OFF contract.
            let (gw_enabled, gw_port) = match app.state::<AppState>().db.lock() {
                Ok(conn) => {
                    let enabled =
                        match orchestration::gateway::control::read_enabled(&conn) {
                            Some(v) => v,
                            None => {
                                let any_routed = crate::agents::agents().iter().any(|a| {
                                    crate::db::get_setting(
                                        &conn,
                                        &format!("orchestration.gateway.{}", a.id),
                                    )
                                    .ok()
                                    .flatten()
                                    .and_then(|v| v.as_bool())
                                    .unwrap_or(false)
                                });
                                if any_routed {
                                    let _ = crate::db::set_setting(
                                        &conn,
                                        orchestration::gateway::control::ENABLED_KEY,
                                        &serde_json::json!(true),
                                    );
                                }
                                any_routed
                            }
                        };
                    let port = orchestration::gateway::control::read_port(&conn)
                        .unwrap_or(orchestration::gateway::GATEWAY_PORT);
                    (enabled, port)
                }
                Err(_) => (false, orchestration::gateway::GATEWAY_PORT),
            };
            if gw_enabled {
                match db::open(&data_dir) {
                    Ok(gw_conn) => {
                        let gw_state = orchestration::gateway::GatewayState {
                            db: Arc::new(tokio::sync::Mutex::new(gw_conn)),
                            health: orch_health.clone(),
                            quota: orch_quota.clone(),
                            affinity: orch_affinity.clone(),
                            credential_reader: Arc::new(|endpoint_id| {
                                crate::secrets::get(endpoint_id)
                            }),
                            loopback_token: app.state::<AppState>().gateway.token.clone(),
                            tuning: app.state::<AppState>().gateway_tuning.clone(),
                        };
                        let ctrl = app.state::<AppState>().gateway.clone();
                        let refresh_handle = app.handle().clone();
                        tauri::async_runtime::spawn(async move {
                            if let Err(e) = ctrl.start(gw_state, gw_port).await {
                                tracing::error!("gateway failed to start: {e}");
                            }
                            // Gateway start is a lifecycle op: refresh every
                            // routed agent's alias once per launch so config
                            // files carry the current steady-state abilities
                            // (context/output limits) even when no
                            // policy/endpoint edit has fired since the last
                            // write.
                            let state = refresh_handle.state::<AppState>();
                            commands::gateway::refresh_all_routed(&state).await;
                        });
                    }
                    Err(e) => {
                        tracing::error!("gateway DB connection failed: {e}");
                    }
                }
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            // Session
            commands::sessions::session_list,
            commands::sessions::session_read,
            commands::sessions::session_search,
            commands::sessions::session_children,
            commands::sessions::session_get,
            commands::sessions::session_refresh,
            commands::sessions::session_export,
            commands::sessions::session_open,
            commands::sessions::session_reveal,
            commands::sessions::session_delete,
            // Handoff (Context Lifecycle)
            commands::handoff::session_context_pressure,
            commands::handoff::handoff_preview,
            commands::handoff::handoff_save,
            commands::handoff::handoff_list,
            commands::handoff::handoff_delete,
            commands::handoff::handoff_inject,
            commands::handoff::handoff_inject_remove,
            commands::handoff::handoff_to_knowledge,
            commands::handoff::handoff_spawn,
            // Review Runtime
            commands::review::review_create,
            commands::review::review_start,
            commands::review::review_abort,
            commands::review::review_list,
            commands::review::review_get,
            // Skills
            commands::skills::skills_list,
            commands::skills::skills_reveal,
            commands::skills::skills_install,
            commands::skills::skills_uninstall,
            commands::skills::skills_toggle,
            commands::skills::skills_import_scan,
            commands::skills::skills_import_one,
            commands::skills::skills_unmanage,
            // MCP
            commands::mcp::mcp_list,
            commands::mcp::mcp_save,
            commands::mcp::mcp_set_state,
            commands::mcp::mcp_delete,
            commands::mcp::mcp_unmanage,
            commands::mcp::mcp_import_scan,
            commands::mcp::mcp_import_all,
            commands::mcp::mcp_import_one,
            commands::mcp::mcp_sync_agent,
            commands::mcp::mcp_usage_stats,
            commands::mcp::mcp_sync_all,
            commands::mcp::mcp_probe,
            // Settings
            commands::settings::setting_get,
            commands::settings::setting_set,
            commands::settings::setting_delete,
            // Palette
            commands::palette::palette_search,
            // Diagnostics
            commands::diagnostics::diag_export_logs,
            commands::diagnostics::diag_health,
            commands::diagnostics::diag_open_data_dir,
            commands::diagnostics::diag_log_files,
            commands::diagnostics::diag_read_logs,
            commands::diagnostics::diag_log_level_get,
            commands::diagnostics::diag_log_level_set,
            commands::diagnostics::diag_log_full_bodies_get,
            commands::diagnostics::diag_log_full_bodies_set,
            // Updates — GitHub Release version check (manual trigger)
            commands::updates::updates_check,
            // Autostart (launch at login)
            commands::autostart::autostart_is_enabled,
            commands::autostart::autostart_set,
            // Provider (endpoint) — user-managed
            commands::endpoints::endpoint_list,
            commands::endpoints::endpoint_get,
            commands::endpoints::endpoint_create,
            commands::endpoints::endpoint_delete,
            commands::endpoints::endpoint_add_protocol,
            commands::endpoints::endpoint_remove_protocol,
            commands::endpoints::endpoint_set_name,
            commands::endpoints::endpoint_set_models,
            commands::endpoints::endpoint_set_advanced_env,
            commands::endpoints::endpoint_set_model_abilities,
            commands::endpoints::endpoint_set_api_key,
            commands::endpoints::endpoint_clear_api_key,
            commands::endpoints::endpoint_create_with_preset,
            commands::endpoints::endpoint_fetch_models,
            commands::endpoints::endpoint_fetch_quota,
            // Quota / keep-alive / OpenCode Go creds
            commands::quota::quota_refresh_get_settings,
            commands::quota::quota_refresh_set_settings,
            commands::quota::opencode_get_creds,
            commands::quota::opencode_set_creds,
            commands::quota::quota_keepalive_preview,
            commands::quota::quota_ping_now,
            commands::quota::quota_keepalive_status,
            // Presets
            commands::presets::provider_presets,
            // Agent
            commands::agents::agent_list,
            commands::agents::agent_detect,
            commands::agents::agent_clear_provider,
            commands::agents::agent_apply_provider_selection,
            commands::agents::agent_read_config,
            commands::agents::agent_remove_detected,
            commands::agents::agent_set_override,
            commands::agents::agent_clear_override,
            commands::agents::agent_set_enabled,
            // Orchestration — routing policy
            commands::orchestration::routing_policy_list,
            commands::orchestration::routing_policy_upsert,
            commands::orchestration::routing_policy_delete,
            // Orchestration control plane: model catalog / quota / resolve preview
            commands::orchestration::orch_model_catalog,
            commands::orchestration::orch_model_catalog_rebuild,
            commands::orchestration::orch_quota_state,
            commands::orchestration::orch_resolve_preview,
            // Gateway: per-agent opt-in + live status
            commands::gateway::agent_set_gateway_enabled,
            commands::gateway::orch_status,
            // Gateway Service control surface: global enable, port, token, restart
            commands::gateway::gateway_get_status,
            commands::gateway::gateway_set_enabled,
            commands::gateway::gateway_restart,
            commands::gateway::gateway_set_port,
            commands::gateway::gateway_autopick_port,
            commands::gateway::gateway_token_get,
            commands::gateway::gateway_token_regenerate,
            commands::gateway::gateway_recent_activity,
            commands::gateway::gateway_tuning_get,
            commands::gateway::gateway_tuning_set,
            commands::gateway::provider_health_snapshot,
            commands::gateway::provider_health_reset,
            // Route history + migration events
            commands::orchestration::orch_route_history,
            commands::orchestration::orch_migrations,
            // Task summaries + detected roles
            commands::orchestration::orch_tasks,
            commands::orchestration::orch_detected_roles,
            commands::orchestration::orch_session_tasks,
            // Usage dashboard (tokens + read-time cost per day/agent/model)
            commands::orchestration::orch_usage_summary,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            if let tauri::RunEvent::ExitRequested { .. } = event {
                drain_gateway_on_exit(app_handle);
            }
        });
}

/// Belt-and-braces gateway drain for the non-tray exit paths. The tray Quit
/// menu drains inline before `app.exit(0)`; this covers any other exit
/// (window destroyed without tray, OS shutdown, panic-driven exit) so the fixed
/// loopback port and the gateway's dedicated DB connection always release.
fn drain_gateway_on_exit(app: &AppHandle) {
    use tauri::Manager;
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    // Final affinity snapshot (no debounce) so the ≤5s tail of session pins
    // survives the exit; health persists on transitions and needs no flush.
    if let Ok(conn) = state.db.lock() {
        state.orch_affinity.flush_sessions(&conn);
    }
    if let Some(handle) = state.gateway.try_take_for_shutdown() {
        tauri::async_runtime::block_on(async {
            handle.shutdown().await;
        });
        tracing::info!("gateway drained on exit");
    }
}