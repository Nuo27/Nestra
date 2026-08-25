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
        // The log dir is entirely Nestra-owned (the daily-rotated text and
        // JSON generations, crash.log), so every regular file ships — an
        // extension filter would silently drop a rotated generation.
        for entry in std::fs::read_dir(&log_dir)?.flatten() {
            let path = entry.path();
            if path.is_file() {
                let file_name = path.file_name().unwrap();
                let target = std::path::PathBuf::from(&dest_path).join(file_name);
                std::fs::copy(&path, &target)?;
            }
        }
        Ok(())
    })
    .await
}

// ---- Gateway log viewer (reads the JSON twin layer) ----

/// One parsed JSON-lines log entry. `task`/`request` are lifted out of the
/// span chain so the viewer can filter/correlate without re-parsing spans.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct LogEntry {
    pub timestamp: String,
    pub level: String,
    pub target: String,
    /// The event message plus its structured fields rendered `k=v` — one
    /// searchable string.
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request: Option<String>,
}

/// Lower-is-finer level rank: ERROR=4 … TRACE=0. Unknown → INFO.
fn level_rank(level: &str) -> u8 {
    match level.to_ascii_uppercase().as_str() {
        "ERROR" => 4,
        "WARN" => 3,
        "INFO" => 2,
        "DEBUG" => 1,
        _ => 0,
    }
}

/// Render a JSON field value as a flat string for the message line.
fn field_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Parse the JSON-lines contents into entries; malformed lines are skipped
/// (a torn final line after a crash must not hide the rest of the file).
pub(crate) fn parse_log_entries(contents: &str) -> Vec<LogEntry> {
    let mut out = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let level = v
            .get("level")
            .and_then(|l| l.as_str())
            .unwrap_or("INFO")
            .to_string();
        let target = v
            .get("target")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        let mut fields: Vec<(String, String)> = Vec::new();
        let mut message = String::new();
        if let Some(obj) = v.get("fields").and_then(|f| f.as_object()) {
            for (k, val) in obj {
                if k == "message" {
                    message = field_value(val);
                } else {
                    fields.push((k.clone(), field_value(val)));
                }
            }
        }
        // Structured fields ride along `k=v` so search hits them.
        if !fields.is_empty() {
            let rendered: Vec<String> = fields.iter().map(|(k, v)| format!("{k}={v}")).collect();
            message.push(' ');
            message.push_str(&rendered.join(" "));
        }
        // Lift correlation ids out of the span chain (innermost first).
        let mut task = None;
        let mut request = None;
        if let Some(spans) = v.get("spans").and_then(|s| s.as_array()) {
            for span in spans {
                let name = span.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let f = span.get("fields");
                match name {
                    "gw_request" => {
                        task = f.and_then(|f| f.get("task")).map(field_value);
                    }
                    "gw_attempt" => {
                        request = f.and_then(|f| f.get("request")).map(field_value);
                    }
                    _ => {}
                }
            }
        }
        out.push(LogEntry {
            timestamp: v
                .get("timestamp")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string(),
            level,
            target,
            message,
            task,
            request,
        });
    }
    out
}

/// The rotated JSON log files (newest first by date-stamped name). Only
/// files the listing itself returned are ever opened — `file` in
/// [`diag_read_logs`] is matched against this set, so no path traversal.
pub(crate) fn json_log_files(log_dir: &std::path::Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(log_dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            // tracing-appender names: nestra.<date>.json
            (name.starts_with("nestra.") && name.ends_with(".json"))
                .then_some(name)
        })
        .collect();
    // Date-stamped names sort chronologically; newest (today) first.
    names.sort_unstable();
    names.reverse();
    names
}

/// Read one JSON log file (default: newest) with level + text filters,
/// returning the LAST `limit` matching entries. The level filter is
/// severity-and-above (e.g. "warn" keeps WARN and ERROR).
#[tauri::command]
pub async fn diag_read_logs(
    file: Option<String>,
    level: Option<String>,
    search: Option<String>,
    limit: Option<usize>,
) -> AppResult<Vec<LogEntry>> {
    run_blocking(move || {
        let log_dir = db::log_dir()?;
        let files = json_log_files(&log_dir);
        if files.is_empty() {
            return Ok(Vec::new());
        }
        let name = match file.as_deref() {
            Some(f) if files.iter().any(|n| n == f) => f.to_string(),
            // Unknown/missing name falls back to the newest generation.
            _ => files[0].clone(),
        };
        let contents = std::fs::read_to_string(log_dir.join(&name))?;
        let mut entries = parse_log_entries(&contents);
        if let Some(min) = level.as_deref().and_then(|l| {
            let l = l.to_ascii_uppercase();
            (l == "ERROR" || l == "WARN" || l == "INFO" || l == "DEBUG" || l == "TRACE")
                .then_some(level_rank(&l))
        }) {
            entries.retain(|e| level_rank(&e.level) >= min);
        }
        if let Some(needle) = search.as_deref().filter(|s| !s.is_empty()) {
            let needle = needle.to_ascii_lowercase();
            entries.retain(|e| {
                e.message.to_ascii_lowercase().contains(&needle)
                    || e.task
                        .as_deref()
                        .is_some_and(|t| t.to_ascii_lowercase().contains(&needle))
                    || e.request
                        .as_deref()
                        .is_some_and(|r| r.to_ascii_lowercase().contains(&needle))
            });
        }
        let limit = limit.unwrap_or(500).clamp(1, 2000);
        if entries.len() > limit {
            entries.drain(..entries.len() - limit);
        }
        Ok(entries)
    })
    .await
}

/// Available JSON log generations (newest first) for the viewer's file
/// picker.
#[tauri::command]
pub async fn diag_log_files() -> AppResult<Vec<String>> {
    run_blocking(move || Ok(json_log_files(&db::log_dir()?))).await
}

/// The currently active verbosity preset (mirror of the live filter).
#[tauri::command]
pub fn diag_log_level_get() -> AppResult<String> {
    Ok(crate::logging::current_preset().as_str().to_string())
}

/// Hot-switch the verbosity preset AND persist it. `NESTRA_LOG`, when set,
/// wins at startup — a persisted preset still records the user's choice
/// and applies on the next env-less launch.
#[tauri::command]
pub fn diag_log_level_set(
    state: State<'_, crate::AppState>,
    preset: String,
) -> AppResult<String> {
    let parsed = crate::logging::LevelPreset::parse(&preset)
        .ok_or_else(|| AppError::Validation(format!("unknown log level preset: {preset}")))?;
    {
        let conn = state
            .db
            .lock()
            .map_err(|e| AppError::Internal(e.to_string()))?;
        crate::db::set_setting(
            &conn,
            crate::logging::LEVEL_KEY,
            &serde_json::json!(parsed.as_str()),
        )?;
    }
    crate::logging::set_preset(parsed);
    Ok(parsed.as_str().to_string())
}

/// Whether the gateway's debug wire evidence logs COMPLETE bodies (vs the
/// 2 KiB truncated snippets).
#[tauri::command]
pub fn diag_log_full_bodies_get() -> AppResult<bool> {
    Ok(crate::orchestration::gateway::trace::full_bodies())
}

/// Toggle full-body capture: persist the choice AND hot-apply it.
#[tauri::command]
pub fn diag_log_full_bodies_set(
    state: State<'_, crate::AppState>,
    enabled: bool,
) -> AppResult<bool> {
    {
        let conn = state
            .db
            .lock()
            .map_err(|e| AppError::Internal(e.to_string()))?;
        crate::db::set_setting(
            &conn,
            crate::logging::FULL_BODIES_KEY,
            &serde_json::json!(enabled),
        )?;
    }
    crate::orchestration::gateway::trace::set_full_bodies(enabled);
    Ok(enabled)
}

#[cfg(test)]
mod tests;
