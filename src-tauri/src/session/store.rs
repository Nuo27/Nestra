//! SQLite persistence for the universal session model.
//!
//! The store is the session INDEX: `session` rows carry identity, location
//! (`source_files`), and small reconcile-time rollups. Transcript bodies are
//! NOT mirrored — they are parsed on demand from the agents' own logs (see
//! `read_session_parts` in `mod.rs`). Raw per-provider logs are walked by
//! `reconcile`, which only reparses a provider when its on-disk file
//! snapshot changed. All queries return the provider-neutral `Session` /
//! `Message` types from `model.rs`.

use crate::error::{AppError, AppResult};
use rusqlite::{params, Connection};

use super::model::MessageWindow;
use super::{AssembledSession, Message, Session};
use super::{all_providers, normalize_with_parts, provider_snapshot};

/// Reconcile every provider against disk, reparsing only the ones that changed.
pub fn reconcile_all(conn: &Connection) -> AppResult<()> {
    for p in all_providers() {
        if let Err(e) = reconcile_provider(conn, p) {
            tracing::warn!(provider = p, error = %e, "session reconcile failed");
        }
    }
    prune_unknown_providers(conn)?;
    Ok(())
}

/// Delete rows whose provider is no longer in [`super::all_providers`] — e.g. the
/// pre-rename `claude-code`/`pi` ids linger as duplicate sessions after a
/// registry rename. There is no data migration by policy; the closed provider
/// list is the authority.
fn prune_unknown_providers(conn: &Connection) -> AppResult<()> {
    let known: Vec<String> = all_providers().iter().map(|p| p.to_string()).collect();
    for table in ["session", "session_source"] {
        let placeholders = std::iter::repeat("?").take(known.len()).collect::<Vec<_>>().join(",");
        conn.execute(
            &format!("DELETE FROM {table} WHERE provider NOT IN ({placeholders})"),
            rusqlite::params_from_iter(known.iter()),
        )?;
    }
    Ok(())
}

/// Reparse `provider` only when its `(path, mtime)` snapshot differs from what
/// is stored in `session_source`. The FIRST import (empty snapshot) takes the
/// full path: assemble everything, rewrite all index rows. Later changes take
/// the per-session incremental path (`reconcile_incremental`) — only the
/// sessions whose source material changed are re-parsed and re-written, so a
/// one-message update inside a 100k-part zcode/opencode db costs one
/// `collect_one`, not a full-provider walk.
pub fn reconcile_provider(conn: &Connection, provider: &str) -> AppResult<()> {
    // Sort BOTH sides: `stored_snapshot` is ORDER BY path, but the disk side
    // comes from per-importer iteration order (not guaranteed sorted). An
    // unsorted-vs-sorted comparison would ALWAYS differ for >1 file, forcing
    // a full re-parse + re-write on every reconcile.
    let mut disk = provider_snapshot(provider)?;
    disk.sort();
    let db_snap = stored_snapshot(conn, provider)?;

    if disk == db_snap {
        return Ok(()); // nothing changed
    }

    if !db_snap.is_empty() {
        return reconcile_incremental(conn, provider, &disk, &db_snap);
    }

    // First import: assemble once, persist the index rows (`session`) — the
    // transcript mirror (`session_message` / `session_part`) was removed in
    // schema v3; bodies are read on demand via [`super::read_session_parts`].
    // The reconcile also persists the small rollups (`message_count`,
    // `est_tokens`, `top_consumer`, `last_model`) that the context-pressure
    // header reads O(1).
    let assembled = normalize_with_parts(provider)?;

    let tx = conn.unchecked_transaction()?;
    replace_provider_rows(&tx, provider, &assembled)?;
    replace_sources(&tx, provider, &disk)?;
    recompute_child_counts(&tx, provider)?;
    tx.commit()?;
    Ok(())
}

/// Per-session incremental reconcile. Change detection:
/// - a changed `.jsonl` file dirties the sessions its content groups into
///   (plus any stored session whose `source_files` include it — multi-file
///   merges — or a removed file, which drops the session when nothing else
///   backs it);
/// - a changed part-style SQLite db (zcode / `opencode.db`) is diffed by its
///   OWN `session` table (`(id, time_updated)` via [`partdb::session_index`])
///   against the stored rows — the db's mtime alone must NOT dirty every
///   session in it.
///
/// Everything lands in one transaction: upserts, deletions, the refreshed
/// `session_source` snapshot, and a `child_count` recompute (cheap, indexed).
fn reconcile_incremental(
    conn: &Connection,
    provider: &str,
    disk: &[(String, i64)],
    db_snap: &[(String, i64)],
) -> AppResult<()> {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::Path;

    let disk_map: BTreeMap<&str, i64> =
        disk.iter().map(|(p, m)| (p.as_str(), *m)).collect();
    let snap_map: BTreeMap<&str, i64> =
        db_snap.iter().map(|(p, m)| (p.as_str(), *m)).collect();
    let changed: BTreeSet<&str> = disk_map
        .iter()
        .filter(|(p, m)| snap_map.get(*p) != Some(*m))
        .map(|(p, _)| *p)
        .collect();
    let removed: BTreeSet<&str> = snap_map
        .keys()
        .filter(|p| !disk_map.contains_key(*p))
        .copied()
        .collect();
    // JSONL files participate in the source_files intersect test below.
    let jsonl_touched: BTreeSet<&str> = changed
        .iter()
        .chain(removed.iter())
        .copied()
        .filter(|p| Path::new(p).extension().map(|e| e == "jsonl").unwrap_or(false))
        .collect();

    // Stored index rows: id → (updated_at, source_files).
    let mut stored: BTreeMap<String, (Option<i64>, Vec<String>)> = BTreeMap::new();
    {
        let mut stmt =
            conn.prepare("SELECT id, updated_at, source_files_json FROM session WHERE provider = ?1")?;
        let rows = stmt.query_map([provider], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        for (id, updated, files_json) in rows.flatten() {
            stored.insert(id, (updated, serde_json::from_str(&files_json).unwrap_or_default()));
        }
    }

    // Pass 1 — what changed, per shape.
    let mut touched: Vec<super::RawFile> = Vec::new(); // parsed changed JSONL files
    let mut dirty: BTreeSet<String> = BTreeSet::new(); // sessions to re-upsert
    let mut gone: BTreeSet<String> = BTreeSet::new(); // sessions to delete
    // Dirty sessions that came out of a part-style db. A NEW session has no
    // stored row yet, so this map (not `source_files`) is what tells the
    // re-assembly below which db to read it from.
    let mut partdb_of: BTreeMap<String, String> = BTreeMap::new();

    for path in &changed {
        let p = Path::new(path);
        if p.extension().map(|e| e == "jsonl").unwrap_or(false) {
            if let Ok(parsed) = super::parse_jsonl_events(p) {
                touched.push(super::rawfile_from_jsonl(p, parsed));
            }
        } else {
            // Part-style db: diff (id, time_updated) against the stored rows
            // that live in this db. A NULL time_updated falls back to the db
            // mtime — the same key `collect`/`collect_one` persist, so the
            // comparison stays stable.
            let mtime = super::mtime_millis(p);
            let src_rows = super::partdb::session_index(p);
            let src: BTreeMap<&str, i64> = src_rows
                .iter()
                .map(|(id, u)| (id.as_str(), u.unwrap_or(mtime)))
                .collect();
            // Sessions NEW to the source db (no stored row yet) are dirty too
            // — the diff must run both directions.
            for id in src_rows.iter().map(|(id, _)| id) {
                if !stored.contains_key(id) {
                    dirty.insert(id.clone());
                    partdb_of.insert(id.clone(), path.to_string());
                }
            }
            for (id, (updated, files)) in &stored {
                if !files.iter().any(|f| f == path) {
                    continue;
                }
                match src.get(id.as_str()) {
                    Some(u) if Some(*u) == *updated => {} // unchanged
                    Some(_) => {
                        dirty.insert(id.clone());
                        partdb_of.insert(id.clone(), path.to_string());
                    }
                    None => {
                        gone.insert(id.clone());
                    }
                }
            }
        }
    }

    // Parsed JSONL files dirty the sessions they group into (new or changed)…
    for rf in &touched {
        dirty.insert(rf.canonical_id.clone());
    }
    // …and any stored session whose files include a touched/removed JSONL
    // source (multi-file merges; a removed file drops the session when the
    // re-assembly below finds nothing left backing it).
    for (id, (_, files)) in &stored {
        if files.iter().any(|f| jsonl_touched.contains(f.as_str())) {
            dirty.insert(id.clone());
        }
    }

    // New sidechains parsed this round feed the Task→child link map.
    let new_children: BTreeMap<String, String> = touched
        .iter()
        .filter(|rf| rf.is_sidechain)
        .map(|rf| (rf.canonical_id.clone(), rf.canonical_id.clone()))
        .collect();
    let touched_paths: BTreeSet<String> = touched.iter().map(|rf| rf.path.to_string_lossy().to_string()).collect();

    let tx = conn.unchecked_transaction()?;
    for id in &dirty {
        // File set: the stored row's sources ∪ this round's parsed files ∪
        // the part-style db the session was diffed out of (new sessions have
        // no stored row to name it).
        let mut files: Vec<String> = stored
            .get(id)
            .map(|(_, f)| f.clone())
            .unwrap_or_default();
        if let Some(db) = partdb_of.get(id) {
            if !files.contains(db) {
                files.push(db.clone());
            }
        }
        for rf in touched.iter().filter(|rf| &rf.canonical_id == id) {
            let p = rf.path.to_string_lossy().to_string();
            if !files.contains(&p) {
                files.push(p);
            }
        }
        // Parse the file set (reusing this round's parsed JSONL), keeping
        // only this conversation's raws.
        let mut raws: Vec<super::RawFile> = Vec::new();
        for f in &files {
            if touched_paths.contains(f) {
                continue; // already parsed above
            }
            let p = Path::new(f);
            if !p.is_file() {
                continue;
            }
            if p.extension().map(|e| e == "jsonl").unwrap_or(false) {
                if let Ok(parsed) = super::parse_jsonl_events(p) {
                    raws.push(super::rawfile_from_jsonl(p, parsed));
                }
            } else if let Some(rf) = super::partdb::collect_one(p, id)? {
                raws.push(rf);
            }
        }
        raws.extend(
            touched
                .iter()
                .filter(|rf| &rf.canonical_id == id)
                .cloned(),
        );
        raws.retain(|rf| &rf.canonical_id == id);
        raws.sort_by_key(|rf| (rf.started_at, rf.path.clone()));

        let mut a2c = agent_to_child_map(&tx, provider, id)?;
        a2c.extend(new_children.clone());
        match super::assemble_session(provider, id, raws, &a2c) {
            Some(a) => upsert_session_row(&tx, &a)?,
            None => delete_session_row(&tx, provider, id)?,
        }
    }
    for id in &gone {
        delete_session_row(&tx, provider, id)?;
    }
    replace_sources(&tx, provider, disk)?;
    recompute_child_counts(&tx, provider)?;
    tx.commit()?;
    Ok(())
}

fn stored_snapshot(conn: &Connection, provider: &str) -> AppResult<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT path, file_mtime FROM session_source WHERE provider = ?1 ORDER BY path",
    )?;
    let rows = stmt.query_map([provider], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    out.sort();
    Ok(out)
}

/// Rewrite ALL of the provider's index rows (the first-import path).
fn replace_provider_rows(
    tx: &Connection,
    provider: &str,
    assembled: &[AssembledSession],
) -> AppResult<()> {
    tx.execute("DELETE FROM session WHERE provider = ?1", [provider])?;
    for a in assembled {
        upsert_session_row(tx, a)?;
    }
    Ok(())
}

/// Upsert ONE session index row (identity, location, rollups) — shared by the
/// first-import and incremental reconcile paths. Rollups come from the
/// in-memory parts (single formula source: the same width walk the pressure
/// header ran over the mirror rows pre-v3).
fn upsert_session_row(tx: &Connection, a: &AssembledSession) -> AppResult<()> {
    let s = &a.session;
    let pressure = super::handoff::context_pressure(&a.parts, None);
    let last_model = super::handoff::last_model(&a.parts);
    let source_files_json = serde_json::to_string(&s.source_files).unwrap_or_else(|_| "[]".into());
    let session_meta = if s.provider_metadata_json.is_empty() {
        "{}".to_string()
    } else {
        s.provider_metadata_json.clone()
    };
    tx.execute(
        "INSERT INTO session (
           provider, id, title, summary, project, cwd, started_at, updated_at,
           ended_at, message_count, source_path, parent_session_id, agent_id,
           is_subagent, resume_command, child_count, source_files_json,
           provider_metadata_json, est_tokens, top_consumer, last_model
         ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21)
         ON CONFLICT(provider, id) DO UPDATE SET
           title=excluded.title,
           summary=excluded.summary,
           project=excluded.project,
           cwd=excluded.cwd,
           started_at=excluded.started_at,
           updated_at=excluded.updated_at,
           ended_at=excluded.ended_at,
           message_count=excluded.message_count,
           source_path=excluded.source_path,
           parent_session_id=excluded.parent_session_id,
           agent_id=excluded.agent_id,
           is_subagent=excluded.is_subagent,
           resume_command=excluded.resume_command,
           child_count=excluded.child_count,
           source_files_json=excluded.source_files_json,
           provider_metadata_json=excluded.provider_metadata_json,
           est_tokens=excluded.est_tokens,
           top_consumer=excluded.top_consumer,
           last_model=excluded.last_model",
        params![
            s.provider,
            s.id,
            s.title,
            s.summary,
            s.project,
            s.cwd,
            s.started_at,
            s.updated_at,
            s.ended_at,
            a.parts.len() as i64,
            s.source_path,
            s.parent_session_id,
            s.agent_id,
            s.is_subagent as i64,
            s.resume_command,
            s.child_count as i64,
            source_files_json,
            session_meta,
            pressure.est_tokens,
            pressure.top_consumer,
            last_model,
        ],
    )?;
    Ok(())
}

/// Drop one session's index row (incremental path: source material gone or
/// the session vanished from the source db).
fn delete_session_row(tx: &Connection, provider: &str, id: &str) -> AppResult<()> {
    tx.execute(
        "DELETE FROM session WHERE provider = ?1 AND id = ?2",
        params![provider, id],
    )?;
    Ok(())
}

fn replace_sources(
    tx: &Connection,
    provider: &str,
    snap: &[(String, i64)],
) -> AppResult<()> {
    tx.execute(
        "DELETE FROM session_source WHERE provider = ?1",
        [provider],
    )?;
    for (path, mtime) in snap {
        tx.execute(
            "INSERT INTO session_source (provider, path, file_mtime) VALUES (?1, ?2, ?3)",
            params![provider, path, mtime],
        )?;
    }
    Ok(())
}

/// child_count = number of subagent sessions (is_subagent=1) whose
/// parent_session_id equals this session's id.
fn recompute_child_counts(tx: &Connection, provider: &str) -> AppResult<()> {
    tx.execute(
        "UPDATE session SET child_count = (
            SELECT COUNT(*) FROM session AS c
            WHERE c.provider = session.provider
              AND c.is_subagent = 1
              AND c.parent_session_id = session.id
         ) WHERE provider = ?1 AND is_subagent = 0",
        [provider],
    )?;
    Ok(())
}

fn row_to_session(r: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    let source_files_json: String = r.get("source_files_json")?;
    let source_files: Vec<String> = serde_json::from_str(&source_files_json).unwrap_or_default();
    Ok(Session {
        id: r.get("id")?,
        provider: r.get("provider")?,
        title: r.get("title")?,
        summary: r.get("summary")?,
        project: r.get("project")?,
        cwd: r.get("cwd")?,
        started_at: r.get("started_at")?,
        updated_at: r.get("updated_at")?,
        ended_at: r.get("ended_at")?,
        message_count: r.get::<_, i64>("message_count")? as u32,
        source_path: r.get("source_path")?,
        parent_session_id: r.get("parent_session_id")?,
        agent_id: r.get("agent_id")?,
        is_subagent: r.get::<_, i64>("is_subagent")? != 0,
        resume_command: r.get("resume_command")?,
        child_count: r.get::<_, i64>("child_count")? as u32,
        source_files,
        provider_metadata_json: r
            .get::<_, Option<String>>("provider_metadata_json")?
            .unwrap_or_else(|| "{}".into()),
    })
}

/// Top-level (non-subagent) sessions, newest first. Optional provider filter;
/// optional `LIKE` search over title/summary/project.
pub fn list_sessions(
    conn: &Connection,
    provider: Option<&str>,
    search: Option<&str>,
    limit: u32,
) -> AppResult<Vec<Session>> {
    let mut sql = String::from(
        "SELECT provider, id, title, summary, project, cwd, started_at, updated_at,
                ended_at, message_count, source_path, parent_session_id, agent_id,
                is_subagent, resume_command, child_count, source_files_json,
                provider_metadata_json
         FROM session WHERE is_subagent = 0",
    );
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(p) = provider {
        sql.push_str(" AND provider = ?");
        args.push(Box::new(p.to_string()));
    }
    if let Some(s) = search {
        if !s.trim().is_empty() {
            let pat = format!("%{}%", escape_like(s.trim()));
            sql.push_str(" AND (title LIKE ? ESCAPE '\\' OR summary LIKE ? ESCAPE '\\' OR project LIKE ? ESCAPE '\\')");
            args.push(Box::new(pat.clone()));
            args.push(Box::new(pat.clone()));
            args.push(Box::new(pat));
        }
    }
    sql.push_str(" ORDER BY updated_at DESC LIMIT ?");
    args.push(Box::new(limit as i64));
    let mut stmt = conn.prepare(&sql)?;
    let param_refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let rows = stmt.query_map(param_refs.as_slice(), row_to_session)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Subagent sessions of a parent, newest first.
pub fn list_children(conn: &Connection, provider: &str, parent_id: &str) -> AppResult<Vec<Session>> {
    let mut stmt = conn.prepare(
        "SELECT provider, id, title, summary, project, cwd, started_at, updated_at,
                ended_at, message_count, source_path, parent_session_id, agent_id,
                is_subagent, resume_command, child_count, source_files_json,
                provider_metadata_json
         FROM session WHERE provider = ?1 AND parent_session_id = ?2
         ORDER BY started_at ASC",
    )?;
    let rows = stmt.query_map(params![provider, parent_id], row_to_session)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

pub fn get_session(conn: &Connection, provider: &str, id: &str) -> AppResult<Option<Session>> {
    let mut stmt = conn.prepare(
        "SELECT provider, id, title, summary, project, cwd, started_at, updated_at,
                ended_at, message_count, source_path, parent_session_id, agent_id,
                is_subagent, resume_command, child_count, source_files_json,
                provider_metadata_json
         FROM session WHERE provider = ?1 AND id = ?2",
    )?;
    let mut rows = stmt.query(params![provider, id])?;
    if let Some(r) = rows.next()? {
        Ok(Some(row_to_session(r)?))
    } else {
        Ok(None)
    }
}

/// Windowed message read, ordered by `seq`. The index-only store parses the
/// session's source files on demand (no `session_message` mirror); `total` is
/// the live parsed count, not the reconcile-time `message_count`.
pub fn read_messages(
    conn: &Connection,
    provider: &str,
    id: &str,
    offset: u32,
    limit: u32,
) -> AppResult<MessageWindow> {
    let session = get_session(conn, provider, id)?.ok_or_else(|| {
        AppError::NotFound(format!("session {provider}/{id} not found"))
    })?;
    let children = agent_to_child_map(conn, provider, id)?;
    let parts = super::read_session_parts(&session, &children)?;
    let total = parts.len() as u32;
    let messages: Vec<Message> = parts.iter().map(|p| p.to_message()).collect();
    let start = (offset as usize).min(messages.len());
    let end = if limit == 0 {
        messages.len()
    } else {
        start.saturating_add(limit as usize).min(messages.len())
    };
    Ok(MessageWindow {
        messages: messages[start..end].to_vec(),
        total,
    })
}

/// Subagent-child lookup for on-demand reads: child id → itself, plus
/// `agent_id` → id when the provider records a spawn name distinct from the
/// child's canonical id. Mirrors the assembler's in-memory map — it links a
/// parent's Task-tool invocation to the child conversation.
pub(crate) fn agent_to_child_map(
    conn: &Connection,
    provider: &str,
    parent_id: &str,
) -> AppResult<std::collections::BTreeMap<String, String>> {
    let mut stmt = conn.prepare(
        "SELECT id, agent_id FROM session
         WHERE provider = ?1 AND parent_session_id = ?2 AND is_subagent = 1",
    )?;
    let rows = stmt.query_map(params![provider, parent_id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
    })?;
    let mut out = std::collections::BTreeMap::new();
    for row in rows {
        let (id, agent_id) = row?;
        out.insert(id.clone(), id.clone());
        if let Some(a) = agent_id {
            out.insert(a, id);
        }
    }
    Ok(out)
}

/// The reconcile-time rollups the context-pressure header reads O(1):
/// `(est_tokens, top_consumer, last_model)`, `None` when the session is gone.
pub(crate) fn session_rollups(
    conn: &Connection,
    provider: &str,
    id: &str,
) -> AppResult<Option<(Option<i64>, Option<String>, Option<String>)>> {
    use rusqlite::OptionalExtension;
    Ok(conn
        .query_row(
            "SELECT est_tokens, top_consumer, last_model
             FROM session WHERE provider = ?1 AND id = ?2",
            params![provider, id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?)
}

/// Total session row count (for diagnostics).
pub fn count_sessions(conn: &Connection) -> AppResult<u32> {
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM session", [], |r| r.get(0))?;
    Ok(n as u32)
}

/// Escape a user search fragment for `LIKE ? ESCAPE '\'`. Escapes the escape
/// character first, then `%` and `_`, so every metacharacter is neutralized
/// exactly once and a literal backslash cannot confuse the ESCAPE clause.
fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Delete one session from the DB and remove its source files from disk.
/// Returns the list of file paths that were removed (skipping any that
/// didn't exist).
///
/// Disk files are removed only AFTER the DB transaction commits, and only
/// when no OTHER session references the same path. This matters for
/// opencode-desktop, whose sessions all live in one shared `opencode.db` —
/// deleting one session must not delete the whole provider database.
pub fn delete_session(conn: &Connection, provider: &str, id: &str) -> AppResult<Vec<String>> {
    let session = get_session(conn, provider, id)?.ok_or_else(|| {
        AppError::NotFound(format!("session {provider}/{id} not found"))
    })?;
    let mut removed = Vec::new();
    // Dedupe source_path vs source_files (the former is also in the latter).
    let mut seen = std::collections::HashSet::new();
    for path in std::iter::once(&session.source_path).chain(session.source_files.iter()) {
        if path.is_empty() || !seen.insert(path.clone()) {
            continue;
        }
        removed.push(path.clone());
    }
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "DELETE FROM session_source
            WHERE provider = ?1 AND path IN (
                SELECT source_path FROM session WHERE provider = ?1 AND id = ?2
            )",
        params![provider, id],
    )?;
    tx.execute(
        "DELETE FROM session WHERE provider = ?1 AND id = ?2",
        params![provider, id],
    )?;
    tx.commit()?;

    // Post-commit disk cleanup: best-effort, never aborts the (already
    // committed) DB delete, and never removes a file still referenced by
    // another session (shared providers like opencode-desktop).
    for path in &removed {
        let pb = std::path::PathBuf::from(path);
        if !pb.exists() || path_is_shared(conn, provider, id, path)? {
            continue;
        }
        if let Err(e) = std::fs::remove_file(&pb) {
            tracing::warn!("session delete: failed to remove {path}: {e}");
        }
    }
    Ok(removed)
}

/// True when any OTHER session row (same provider, different id) references
/// `path` via `source_path` or the `source_files` JSON array. Shared files
/// must never be deleted on a single session's behalf.
fn path_is_shared(conn: &Connection, provider: &str, self_id: &str, path: &str) -> AppResult<bool> {
    // source_path match — the common shared case (opencode.db).
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM session
         WHERE provider = ?1 AND id != ?2 AND source_path = ?3",
        params![provider, self_id, path],
        |r| r.get(0),
    )?;
    if n > 0 {
        return Ok(true);
    }
    // source_files JSON array membership, via SQLite's json_each.
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM session, json_each(session.source_files_json)
         WHERE session.provider = ?1 AND session.id != ?2 AND json_each.value = ?3",
        params![provider, self_id, path],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

#[cfg(test)]
mod tests;


