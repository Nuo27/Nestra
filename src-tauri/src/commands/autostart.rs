//! Autostart (launch at login) — the typed boundary the Settings toggle and
//! the tray menu both go through, so the two surfaces never drift. The OS
//! entry is the source of truth — nothing is persisted in `setting_kv`.
//!
//! **Windows**: the HKCU `Run` key is written here directly, with the exe
//! path QUOTED — the auto-launch crate the plugin delegates to writes
//! `{path} {args}` unquoted, which silently breaks when the install
//! directory contains spaces (custom install paths). Value name stays
//! "Nestra" (the plugin's `package_info().name`), so entries written before
//! this fix are still detected and cleaned up.
//!
//! **macOS / Linux**: `tauri-plugin-autostart` handles LaunchAgent / XDG
//! autostart (both formats tolerate spaces natively).
//!
//! Both toggle paths (`autostart_set` here, the tray's `toggle_autostart`)
//! update the tray checkmark IN PLACE and emit `autostart-changed` so the
//! frontend switch and the native menu never go stale.

use crate::error::AppResult;
use tauri::{AppHandle, Emitter};

/// Frontend event name — the Settings page listens for it to invalidate its
/// autostart query when the change came from the tray.
pub const CHANGED_EVENT: &str = "autostart-changed";

#[tauri::command]
pub fn autostart_is_enabled(app: AppHandle) -> AppResult<bool> {
    is_enabled(&app)
}

#[tauri::command]
pub fn autostart_set(app: AppHandle, enabled: bool) -> AppResult<()> {
    set_enabled(&app, enabled)?;
    // Keep the tray checkmark in sync with the Settings switch, then tell the
    // frontend (the Settings page invalidates its own query, so this mainly
    // covers any other surface listening).
    crate::tray::set_autostart_checked(&app, enabled);
    emit_changed(&app);
    Ok(())
}

/// Query the OS autostart entry. Shared with the tray menu builder.
/// (`app` is only used by the non-Windows arms.)
#[cfg_attr(target_os = "windows", allow(unused_variables))]
pub fn is_enabled(app: &AppHandle) -> AppResult<bool> {
    #[cfg(target_os = "windows")]
    {
        win::is_enabled()
    }
    #[cfg(not(target_os = "windows"))]
    {
        other::is_enabled(app)
    }
}

/// Set the OS autostart entry. Shared with the tray toggle handler.
/// (`app` is only used by the non-Windows arms.)
#[cfg_attr(target_os = "windows", allow(unused_variables))]
pub fn set_enabled(app: &AppHandle, enabled: bool) -> AppResult<()> {
    #[cfg(target_os = "windows")]
    {
        win::set_enabled(enabled)
    }
    #[cfg(not(target_os = "windows"))]
    {
        other::set_enabled(app, enabled)
    }
}

/// Notify the frontend that the OS autostart entry changed (from the tray).
/// Fire-and-forget: a missing listener (page not open) is not an error.
pub fn emit_changed(app: &AppHandle) {
    if let Err(e) = app.emit(CHANGED_EVENT, ()) {
        tracing::warn!("autostart-changed emit failed: {e}");
    }
}

/// Windows registry implementation (see the module doc for why the plugin's
/// auto-launch delegate is not used here).
#[cfg(target_os = "windows")]
mod win {
    use crate::error::{AppError, AppResult};
    use std::io::ErrorKind;
    use winreg::enums::RegType::REG_BINARY;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE};
    use winreg::{RegKey, RegValue};

    /// The per-user autostart location, read by explorer at login.
    const RUN_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Run";
    /// Task Manager's startup-approval override. When the user disables the
    /// app in Task Manager's Startup tab this holds a value whose last 8
    /// bytes are non-zero; `enable` must reset it or the Run entry stays
    /// inert even though it exists.
    const STARTUP_APPROVED_KEY: &str =
        r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
    /// Same value name the autostart plugin used (`package_info().name`), so
    /// entries written before the quoting fix are found and cleaned up.
    const VALUE_NAME: &str = "Nestra";
    const ARGS: &str = "--auto-launch";

    pub fn set_enabled(enabled: bool) -> AppResult<()> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        if enabled {
            let exe = std::env::current_exe()
                .map_err(|e| AppError::Internal(format!("current_exe failed: {e}")))?;
            // The path MUST be quoted: an unquoted `C:\Program Files\...`
            // (custom install dirs, usernames with spaces) makes explorer
            // split the command at the first space and the entry silently
            // fails at login.
            let value = format!("\"{}\" {ARGS}", exe.display());
            let run = hkcu
                .open_subkey_with_flags(RUN_KEY, KEY_SET_VALUE)
                .map_err(|e| AppError::Internal(format!("open Run key failed: {e}")))?;
            run.set_value(VALUE_NAME, &value)
                .map_err(|e| AppError::Internal(format!("write Run value failed: {e}")))?;
            // Reset a Task Manager disable so the entry actually runs.
            if let Ok(approved) =
                hkcu.open_subkey_with_flags(STARTUP_APPROVED_KEY, KEY_SET_VALUE)
            {
                let _ = approved.set_raw_value(
                    VALUE_NAME,
                    &RegValue {
                        vtype: REG_BINARY,
                        bytes: vec![0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
                    },
                );
            }
        } else {
            let run = hkcu
                .open_subkey_with_flags(RUN_KEY, KEY_SET_VALUE)
                .map_err(|e| AppError::Internal(format!("open Run key failed: {e}")))?;
            match run.delete_value(VALUE_NAME) {
                Ok(()) => {}
                // Deleting an already-absent value is fine — disable is
                // idempotent.
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(AppError::Internal(format!("delete Run value failed: {e}")))
                }
            }
        }
        Ok(())
    }

    pub fn is_enabled() -> AppResult<bool> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let run = hkcu
            .open_subkey_with_flags(RUN_KEY, KEY_READ)
            .map_err(|e| AppError::Internal(format!("open Run key failed: {e}")))?;
        let present = run.get_value::<String, _>(VALUE_NAME).is_ok();
        if !present {
            return Ok(false);
        }
        // Task Manager override: non-zero last 8 bytes = user disabled the
        // entry there, so report it as off.
        let approved = hkcu
            .open_subkey_with_flags(STARTUP_APPROVED_KEY, KEY_READ)
            .ok()
            .and_then(|k| k.get_raw_value(VALUE_NAME).ok());
        let task_manager_enabled = match approved {
            Some(v) if v.bytes.len() >= 8 => v.bytes.iter().rev().take(8).all(|b| *b == 0),
            _ => true,
        };
        Ok(task_manager_enabled)
    }
}

/// Non-Windows implementations — `tauri-plugin-autostart` (macOS LaunchAgent
/// / Linux XDG), both of which tolerate spaces in paths natively.
#[cfg(not(target_os = "windows"))]
mod other {
    use crate::error::{AppError, AppResult};
    use tauri::AppHandle;
    use tauri_plugin_autostart::ManagerExt;

    pub fn is_enabled(app: &AppHandle) -> AppResult<bool> {
        app.autolaunch()
            .is_enabled()
            .map_err(|e| AppError::Internal(format!("autostart status failed: {e}")))
    }

    pub fn set_enabled(app: &AppHandle, enabled: bool) -> AppResult<()> {
        let autolaunch = app.autolaunch();
        let result = if enabled {
            autolaunch.enable()
        } else {
            autolaunch.disable()
        };
        result.map_err(|e| AppError::Internal(format!("autostart toggle failed: {e}")))
    }
}
