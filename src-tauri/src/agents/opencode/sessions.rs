//! Desktop-app session importers. Each Desktop app stores its session data
//! in a platform-specific Electron data dir separate from the CLI's
//! session store. These importers are **best-effort** — they scan the
//! known data directories for JSON/JSONL session exports and surface
//! anything that resembles a serialised session. Native IndexedDB /
//! LevelDB stores are not parsed; the on-disk format is undocumented and
//! stable parsing would require mirroring Electron internals.
//!
//! The importers are read-only and provide no resume capability. Their
//! sole purpose is to surface a user's Desktop history in the session
//! list so it can be browsed and deleted.

use crate::session::{import_jsonl_dir, jsonl_snapshot, mtime_millis, SessionImporter};
use crate::db;
use crate::error::AppResult;
use crate::session::RawFile;
use std::path::PathBuf;

/// OpenCode Desktop — sole surviving OpenCode agent (the `opencode-cli`
/// variant is not supported). Surfaces sessions from BOTH layouts the Desktop app
/// may use: the SQLite store (`opencode.db`, read via `collect_opencode_raw`)
/// and the JSONL session dirs. Read-only / not resumable.
pub struct OpenCodeDesktopImporter;

/// Registry constructor — see [super::SPEC].
pub fn new() -> Box<dyn crate::session::SessionImporter> {
    Box::new(OpenCodeDesktopImporter)
}

impl SessionImporter for OpenCodeDesktopImporter {
    fn snapshot(&self) -> AppResult<Vec<(String, i64)>> {
        let mut out: Vec<(String, i64)> = Vec::new();
        // SQLite db (primary Desktop store). `is_file` (not `exists`): a
        // directory named opencode.db would otherwise be counted as a store
        // and re-parsed on every reconcile.
        let db = crate::session::opencode_db_path();
        if db.is_file() {
            out.push((db.to_string_lossy().to_string(), mtime_millis(&db)));
        }
        // JSONL session dirs (best-effort fallback / export layout).
        for dir in opencode_desktop_dirs() {
            out.extend(jsonl_snapshot(dir).unwrap_or_default());
        }
        Ok(out)
    }

    fn import(&self) -> AppResult<Vec<RawFile>> {
        let mut out = collect_opencode_safe();
        for dir in opencode_desktop_dirs() {
            out.extend(import_jsonl_dir(dir).unwrap_or_default());
        }
        Ok(out)
    }
}

/// Read OpenCode's SQLite store. Defers to the shared `collect_opencode_raw`
/// helper (same logic the CLI importer uses). A failure to
/// open the db is non-fatal — the JSONL path still runs.
fn collect_opencode_safe() -> Vec<RawFile> {
    crate::session::collect_opencode_raw().unwrap_or_default()
}

fn opencode_desktop_dirs() -> Vec<PathBuf> {
    let Ok(dirs) = db::platform_dirs() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    // Desktop session dir lives next to the CLI's, but the namespacing
    // differs. Derived from `local_app_data` (the XDG_DATA_HOME equivalent)
    // so Linux (`~/.local/share/opencode/sessions`), macOS
    // (`~/Library/Application Support/opencode/sessions`) and Windows all
    // resolve correctly — a hardcoded `~/.local/share/...` would miss
    // XDG_DATA_HOME overrides.
    if let Some(local) = &dirs.local_app_data {
        out.push(local.join("opencode").join("sessions"));
    }
    if let Some(app_data) = &dirs.app_data {
        // macOS: `local_app_data` and `app_data` can resolve to the SAME
        // directory (Application Support) — dedupe below so the JSONL
        // layout is never scanned twice (duplicated messages after the
        // canonical merge).
        out.push(app_data.join("opencode").join("sessions"));
    }
    // OpenCode resolves its own data dir XDG-style even on Windows, where
    // `local_app_data` above points at `%LOCALAPPDATA%` — probe the
    // `~/.local/share` spelling too (same path as `local_app_data` on Linux;
    // deduped below).
    if let Ok(home) = db::home_dir() {
        out.push(
            home.join(".local")
                .join("share")
                .join("opencode")
                .join("sessions"),
        );
    }
    // Dedupe identical paths, then keep only real directories.
    let mut seen = std::collections::HashSet::new();
    out.retain(|p| seen.insert(p.clone()) && p.is_dir());
    out
}