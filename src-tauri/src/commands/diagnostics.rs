use crate::error::{AppError, AppResult};
use crate::session::store;
use crate::db;
use serde::Serialize;
use super::run_blocking;
use tauri::State;

// ---- Diagnostics commands ----

#[derive(Serialize)]
pub struct HealthReport {
    pub ok: bool,
    pub version: String,
    /// Friendly platform name ("Windows" / "macOS" / "Linux" / …) derived from
    /// `std::env::consts::OS` — the real source, not the webview user-agent.
    pub os: String,
    /// CPU architecture from `std::env::consts::ARCH` ("x86_64", "aarch64", …).
    pub arch: String,
    /// Absolute path to the local data directory (DB, logs, encrypted keys).
    pub data_dir: String,
    pub providers_detected: u32,
    pub sessions_indexed: u32,
    pub last_errors: Vec<String>,
}

#[tauri::command]
pub async fn diag_health(state: State<'_, crate::AppState>) -> AppResult<HealthReport> {
    let db = state.db_read.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        let configured = db::list_endpoints(&conn)
            .map(|v| v.iter().filter(|e| e.has_api_key).count() as u32)
            .unwrap_or(0);
        let sessions_indexed = store::count_sessions(&conn).unwrap_or(0);
        Ok(HealthReport {
            ok: true,
            version: env!("CARGO_PKG_VERSION").to_string(),
            os: friendly_os(),
            arch: std::env::consts::ARCH.to_string(),
            data_dir: db::data_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| "(unavailable)".to_string()),
            providers_detected: configured,
            sessions_indexed,
            last_errors: vec![],
        })
    })
    .await
}

/// Map `std::env::consts::OS` to a friendly platform label, capitalizing the
/// first letter of any unrecognized platform so the row never shows raw lowercase.
fn friendly_os() -> String {
    match std::env::consts::OS {
        "windows" => "Windows".to_string(),
        "macos" => "macOS".to_string(),
        "linux" => "Linux".to_string(),
        "freebsd" => "FreeBSD".to_string(),
        "ios" => "iOS".to_string(),
        "android" => "Android".to_string(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect(),
                None => String::new(),
            }
        }
    }
}

/// Open the app's data directory in the OS file manager. Reuses the same
/// `reveal_in_explorer` helper the session/skills reveal actions use
/// (Windows Explorer via `explorer.exe`; non-Windows builds error).
#[tauri::command]
pub async fn diag_open_data_dir() -> AppResult<()> {
    let dir = db::data_dir()?;
    crate::agents::reveal_in_explorer(&dir)
        .map_err(|e| AppError::Internal(format!("open data dir failed: {e}")))?;
    Ok(())
}

#[tauri::command]
pub async fn diag_export_logs(dest_path: String) -> AppResult<()> {
    run_blocking(move || {
        let log_dir = db::log_dir()?;
        if !log_dir.exists() {
            return Err(AppError::NotFound("no logs yet".into()));
        }
        std::fs::create_dir_all(&dest_path)?;
        for entry in std::fs::read_dir(&log_dir)?.flatten() {
            let path = entry.path();
            if path.extension().map(|e| e == "log").unwrap_or(false) {
                let file_name = path.file_name().unwrap();
                let target = std::path::PathBuf::from(&dest_path).join(file_name);
                std::fs::copy(&path, &target)?;
            }
        }
        Ok(())
    })
    .await
}
