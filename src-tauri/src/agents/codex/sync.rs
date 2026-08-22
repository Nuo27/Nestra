//! Provider-visibility sync for Codex.
//!
//! Codex records the active `model_provider` in TWO places per conversation:
//! the first line (`session_meta`) of each rollout JSONL under
//! `~/.codex/sessions/**` (and `archived_sessions/**`), and the `threads`
//! table of the state SQLite DB (`~/.codex/sqlite/*.db`, older builds:
//! `~/.codex/state_5.sqlite`). The Desktop app filters its conversation list
//! by the CURRENT root `model_provider` — after a provider switch, every
//! older conversation would vanish from the UI until its metadata is
//! rewritten to the new provider id.
//!
//! This module performs that rewrite (the same recipe CodexPlusPlus's
//! provider-sync validated): first-line rewrite for rollouts + a single
//! `UPDATE threads` per DB, with a one-time `.nestra-backup` of each DB
//! before the first write. Best-effort by design — a failure is logged and
//! skipped, never propagated: the config write that triggered the sync has
//! already succeeded, and a locked/live DB must not roll it back.

use crate::error::AppResult;
use std::path::{Path, PathBuf};

/// Rewrite session metadata so conversations recorded under other providers
/// become visible under `new_provider`. `config_path` is the agent's
/// `config.toml`; the codex home is its parent.
pub(crate) fn sync_provider_visibility(config_path: &Path, new_provider: &str) {
    let Some(home) = config_path.parent() else {
        return;
    };
    let rollouts = rewrite_rollout_first_lines(home, new_provider);
    let threads = update_thread_dbs(home, new_provider);
    if rollouts > 0 || threads > 0 {
        tracing::info!(
            provider = new_provider,
            rollouts,
            threads,
            "codex provider-visibility sync"
        );
    }
}

/// Rewrite the `model_provider` inside the first line of every rollout file.
/// Returns the number of files changed.
fn rewrite_rollout_first_lines(home: &Path, new_provider: &str) -> usize {
    let mut changed = 0;
    for dir in ["sessions", "archived_sessions"] {
        let root = home.join(dir);
        let Ok(entries) = walk_rollouts(&root) else {
            continue;
        };
        for path in entries {
            match rewrite_one_rollout(&path, new_provider) {
                Ok(true) => changed += 1,
                Ok(false) => {}
                Err(e) => tracing::warn!(path = %path.display(), error = %e, "codex rollout sync skipped"),
            }
        }
    }
    changed
}

/// Collect `rollout-*.jsonl` files under `root` (any depth).
fn walk_rollouts(root: &Path) -> AppResult<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if entry.file_name().to_string_lossy().starts_with("rollout-")
                && path.extension().is_some_and(|e| e == "jsonl")
            {
                out.push(path);
            }
        }
    }
    Ok(out)
}

/// Rewrite one rollout when its first line is a `session_meta` carrying a
/// different provider. Only the first line is touched — the transcript body
/// is copied through verbatim. `Ok(true)` = file was rewritten.
fn rewrite_one_rollout(path: &Path, new_provider: &str) -> AppResult<bool> {
    let text = std::fs::read_to_string(path)?;
    let (first, rest) = match text.split_once('\n') {
        Some((f, r)) => (f, r),
        None => (text.as_str(), ""),
    };
    let mut line: serde_json::Value = serde_json::from_str(first)
        .map_err(|e| crate::error::AppError::Internal(format!("rollout first line: {e}")))?;
    // RolloutLine envelope: {"timestamp", "type": "session_meta", "payload": {...}}.
    if line.get("type").and_then(|t| t.as_str()) != Some("session_meta") {
        return Ok(false);
    }
    let Some(payload) = line.get_mut("payload").and_then(|p| p.as_object_mut()) else {
        return Ok(false);
    };
    match payload.get("model_provider").and_then(|v| v.as_str()) {
        Some(p) if p == new_provider => Ok(false),
        _ => {
            payload.insert(
                "model_provider".to_string(),
                serde_json::Value::String(new_provider.to_string()),
            );
            let mut out = serde_json::to_string(&line)?;
            out.push('\n');
            out.push_str(rest);
            crate::config_writer::atomic_write(path, out.as_bytes())?;
            Ok(true)
        }
    }
}

/// `UPDATE threads SET model_provider = ?` in every state DB. Returns the
/// total number of rows changed.
fn update_thread_dbs(home: &Path, new_provider: &str) -> usize {
    let mut dbs: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(home.join("sqlite")) {
        for entry in entries.flatten() {
            let p = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".db")
                && !name.ends_with("-wal")
                && !name.ends_with("-shm")
                && !name.ends_with(".nestra-backup")
            {
                dbs.push(p);
            }
        }
    }
    let legacy = home.join("state_5.sqlite");
    if legacy.exists() {
        dbs.push(legacy);
    }

    let mut total = 0;
    for db in dbs {
        match update_one_db(&db, new_provider) {
            Ok(n) => total += n,
            Err(e) => tracing::warn!(db = %db.display(), error = %e, "codex threads sync skipped"),
        }
    }
    total
}

fn update_one_db(db: &Path, new_provider: &str) -> AppResult<usize> {
    use rusqlite::params;
    if !db.exists() {
        return Ok(0);
    }
    // One-time safety net before the first write to this DB.
    let backup = db.with_extension("db.nestra-backup");
    if !backup.exists() {
        let _ = std::fs::copy(db, &backup);
    }
    let conn = rusqlite::Connection::open(db)?;
    conn.busy_timeout(std::time::Duration::from_millis(2000))?;
    let changed = conn.execute(
        "UPDATE threads SET model_provider = ?1 WHERE model_provider IS NOT NULL AND model_provider <> ?1",
        params![new_provider],
    )?;
    Ok(changed)
}

#[cfg(test)]
mod tests;
