//! System tray icon, menu, and close-to-tray behaviour.
//!
//! Menu shape (built once at install; the autostart checkmark is updated in
//! place via `set_autostart_checked` — never rebuilt, see `install`):
//! - **Open Nestra**       → show/focus the main window.
//! - **Launch at startup** → checkable; toggles the OS autostart entry via
//!   `tauri-plugin-autostart` (HKCU Run key on Windows).
//! - **Quit**              → flips `REALLY_QUIT` and exits the app.
//!
//! Closing the main window hides it instead of quitting (`on_window_event`,
//! gated by `REALLY_QUIT`). The tray menu is the only path to actually quit.

use crate::AppState;
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Wry};

/// True once the user has chosen to actually quit (tray Quit menu or app.exit).
/// `on_window_event` consults this before honoring `CloseRequested`.
pub static REALLY_QUIT: AtomicBool = AtomicBool::new(false);

pub fn really_quit() -> bool {
    REALLY_QUIT.load(Ordering::SeqCst)
}

pub fn request_quit() {
    REALLY_QUIT.store(true, Ordering::SeqCst);
}

const ID_SHOW: &str = "tray:show";
const ID_QUIT: &str = "tray:quit";
const ID_AUTOSTART: &str = "tray:autostart";

/// Build the tray, register event handlers, and stash the handle on `AppState`
/// so subsequent calls to `rebuild_menu` can update it.
pub fn install(app: &mut tauri::App) -> tauri::Result<TrayIcon<Wry>> {
    let icon = app
        .default_window_icon()
        .cloned()
        .ok_or_else(|| tauri::Error::AssetNotFound("default window icon".into()))?;

    let (menu, autostart_item) = build_menu(app.handle())?;

    let tray = TrayIconBuilder::with_id("nestra-tray")
        .icon(icon)
        .tooltip("Nestra")
        .show_menu_on_left_click(false)
        .menu(&menu)
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button, .. } = event {
                if matches!(button, MouseButton::Left) {
                    show_main_window(tray.app_handle());
                }
            }
        })
        .on_menu_event(|app, event| {
            let id = event.id.as_ref();
            if id == ID_SHOW {
                show_main_window(app);
            } else if id == ID_QUIT {
                request_quit();
                crate::quota_refresh::request_exit();
                // Drain the loopback gateway so port 18777 and its dedicated DB
                // connection release cleanly before the process exits.
                shutdown_gateway(app);
                app.exit(0);
            } else if id == ID_AUTOSTART {
                // The native checkmark already flipped; sync the OS autostart
                // entry to match and rebuild so the menu tracks reality.
                toggle_autostart(app);
            }
        })
        .build(app)?;

    if let Some(state) = app.try_state::<AppState>() {
        if let Ok(mut guard) = state.tray.lock() {
            *guard = Some(tray.clone());
        }
        // Stash the check item so autostart toggles can flip its checkmark in
        // place — rebuilding the whole menu on a click destroys the native
        // popup being processed (Windows) and the item ends up grayed/stale.
        if let Ok(mut guard) = state.autostart_item.lock() {
            *guard = Some(autostart_item);
        }
    }

    Ok(tray)
}

fn show_main_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
}

/// Flip the OS autostart entry to the opposite of its current state — the
/// OS is the source of truth (the check item toggled natively before this
/// handler ran). Updates the checkmark IN PLACE (never rebuilds the menu —
/// see `install`) and notifies the frontend so the Settings switch stays in
/// sync even when this came from the tray.
fn toggle_autostart(app: &AppHandle) {
    use crate::commands::autostart;
    // The OS is the source of truth (the check item toggled natively before
    // this handler ran): flip whatever the current entry says.
    let target = !autostart::is_enabled(app).unwrap_or(false);
    if let Err(e) = autostart::set_enabled(app, target) {
        tracing::warn!("tray autostart toggle failed: {e}");
    }
    set_autostart_checked(app, target);
    autostart::emit_changed(app);
}

/// Flip the tray "Launch at startup" checkmark without touching the menu
/// structure. Shared by the tray toggle and the Settings command so both
/// surfaces always agree with the OS entry.
pub fn set_autostart_checked(app: &AppHandle, checked: bool) {
    if let Some(state) = app.try_state::<AppState>() {
        if let Ok(guard) = state.autostart_item.lock() {
            if let Some(item) = guard.as_ref() {
                if let Err(e) = item.set_checked(checked) {
                    tracing::warn!("tray autostart checkmark update failed: {e}");
                }
            }
        }
    }
}

/// Drain the gateway so the fixed loopback port (`:18777`) and its dedicated
/// DB connection release before the process exits. Idempotent; safe to call on
/// the quit path even when the gateway never bound. Runs `shutdown()` on the
/// Tauri async runtime and blocks briefly so the accept loop unwinds.
fn shutdown_gateway(app: &AppHandle) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    if let Some(handle) = state.gateway.try_take_for_shutdown() {
        // `shutdown()` is async but only sends a oneshot and returns; blocking
        // on the runtime is safe here because this is the terminal quit path
        // (no UI thread to stall).
        tauri::async_runtime::block_on(async {
            handle.shutdown().await;
        });
        tracing::info!("gateway drained on quit");
    }
}

/// Build the menu reflecting the current autostart state. Invoked once at
/// install; the returned check item is kept on `AppState` so later toggles
/// update it in place instead of rebuilding the native menu.
pub fn build_menu(app: &AppHandle) -> tauri::Result<(Menu<Wry>, CheckMenuItem<Wry>)> {
    let menu = Menu::new(app)?;

    let show = MenuItem::with_id(app, ID_SHOW, "Open Nestra", true, None::<&str>)?;
    menu.append(&show)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;

    let autostart = CheckMenuItem::with_id(
        app,
        ID_AUTOSTART,
        "Launch at startup",
        true,
        // NOTE: Tauri 2's arg order is (text, enabled, checked, accelerator)
        // — easy to swap. `enabled` must always be true (the item is always
        // interactive); `checked` reflects the live OS autostart entry.
        crate::commands::autostart::is_enabled(app).unwrap_or(false),
        None::<&str>,
    )?;
    menu.append(&autostart)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;

    let quit = MenuItem::with_id(app, ID_QUIT, "Quit", true, None::<&str>)?;
    menu.append(&quit)?;

    Ok((menu, autostart))
}
