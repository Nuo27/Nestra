//! SQLite persistence for the universal session model.
//!
//! The store is the single source of truth the UI reads from. Raw per-provider
//! logs are normalized (see `mod.rs`) and upserted here by `reconcile`, which
//! only reparses a provider when its on-disk file snapshot changed. All queries
//! return the provider-neutral `Session` / `Message` types from `model.rs`.

use crate::error::{AppError, AppResult};
use rusqlite::{params, Connection};

use super::model::MessageWindow;
use super::{AssembledSession, Message, Session};
use super::{normalize_with_parts, provider_snapshot, ALL_PROVIDERS};

/// Reconcile every provider against disk, reparsing only the ones that changed.
pub fn reconcile_all(conn: &Connection) -> AppResult<()> {
    for p in ALL_PROVIDERS {
        if let Err(e) = reconcile_provider(conn, p) {
            tracing::warn!(provider = *p, error = %e, "session reconcile failed");
        }
    }
    prune_unknown_providers(conn)?;
    Ok(())
}

/// Delete rows whose provider is no longer in [`ALL_PROVIDERS`] — e.g. the
/// pre-rename `claude-code`/`pi` ids linger as duplicate sessions after a
/// registry rename. There is no data migration by policy; the closed provider
/// list is the authority.
fn prune_unknown_providers(conn: &Connection) -> AppResult<()> {
    let known: Vec<String> = ALL_PROVIDERS.iter().map(|p| p.to_string()).collect();
    for table in ["session", "session_message", "session_part", "session_source"] {
        let placeholders = std::iter::repeat("?").take(known.len()).collect::<Vec<_>>().join(",");
        conn.execute(
            &format!("DELETE FROM {table} WHERE provider NOT IN ({placeholders})"),
            rusqlite::params_from_iter(known.iter()),
        )?;
    }
    Ok(())
}

/// Reparse `provider` only when its `(path, mtime)` snapshot differs from what
/// is stored in `session_source`. On change: assemble semantic parts once,
/// persist them as the source of truth (`session_part`), project each part to a
/// flat `Message` row into the `session_message` table (so the existing
/// read path keeps its byte-compatible contract), then recompute child_counts.
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

    // Assemble once: parts are the source of truth; messages are projected.
    let assembled = normalize_with_parts(provider)?;
    let built: Vec<(Session, Vec<Message>)> = assembled
        .iter()
        .map(|a| {
            let messages: Vec<Message> = a.parts.iter().map(|p| p.to_message()).collect();
            let mut session = a.session.clone();
            session.message_count = messages.len() as u32;
            (session, messages)
        })
        .collect();

    let tx = conn.unchecked_transaction()?;
    replace_provider_rows(&tx, provider, &built)?;
    replace_provider_parts(&tx, provider, &assembled)?;
    replace_sources(&tx, provider, &disk)?;
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

fn replace_provider_rows(
    tx: &Connection,
    provider: &str,
    built: &[(Session, Vec<Message>)],
) -> AppResult<()> {
    tx.execute("DELETE FROM session WHERE provider = ?1", [provider])?;
    tx.execute("DELETE FROM session_message WHERE provider = ?1", [provider])?;
    for (s, msgs) in built {
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
               provider_metadata_json
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
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
                s.message_count as i64,
                s.source_path,
                s.parent_session_id,
                s.agent_id,
                s.is_subagent as i64,
                s.resume_command,
                s.child_count as i64,
                source_files_json,
                session_meta,
            ],
        )?;
        for m in msgs {
            let msg_meta = if m.provider_metadata_json.is_empty() {
                "{}".to_string()
            } else {
                m.provider_metadata_json.clone()
            };
            tx.execute(
                "INSERT INTO session_message (
                   provider, session_id, seq, role, content_text, tool_name,
                   tool_input, tool_output, tool_call_id, thinking,
                   parent_message_id, message_id, timestamp, provider_metadata_json
                 ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                params![
                    s.provider,
                    s.id,
                    m.seq as i64,
                    m.role,
                    m.content_text,
                    m.tool_name,
                    m.tool_input,
                    m.tool_output,
                    m.tool_call_id,
                    m.thinking,
                    m.parent_message_id,
                    m.message_id,
                    m.timestamp,
                    msg_meta,
                ],
            )?;
        }
    }
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

/// Persist the typed semantic parts. One row per part, carrying the typed
/// `payload_json`, the stable `kind` tag, and the denormalized `tool_call_id`.
/// (`raw_json` is intentionally NOT persisted — it bloated the DB ~10× with
/// verbatim records that no live code path reads back. The typed payload is
/// the source of truth; `Unknown` payloads keep their raw JSON in-memory only.)
fn replace_provider_parts(
    tx: &Connection,
    provider: &str,
    assembled: &[AssembledSession],
) -> AppResult<()> {
    tx.execute("DELETE FROM session_part WHERE provider = ?1", [provider])?;
    for a in assembled {
        for p in &a.parts {
            let meta = if p.provider_metadata_json.is_empty() {
                "{}".to_string()
            } else {
                p.provider_metadata_json.clone()
            };
            tx.execute(
                "INSERT INTO session_part (
                   provider, session_id, seq, part_idx, kind, payload_json,
                   tool_call_id, ts, raw_json, provider_metadata_json
                 ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![
                    provider,
                    a.session.id,
                    p.seq as i64,
                    0i64,
                    p.kind_tag(),
                    p.payload_json(),
                    p.tool_call_id,
                    p.ts,
                    "", // raw_json: see semantic::SemanticEvent — persisted as "" by design
                    meta,
                ],
            )?;
        }
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

/// Windowed message read from the store, ordered by `seq`.
pub fn read_messages(
    conn: &Connection,
    provider: &str,
    id: &str,
    offset: u32,
    limit: u32,
) -> AppResult<MessageWindow> {
    let total: i64 = conn.query_row(
        "SELECT COUNT(*) FROM session_message WHERE provider = ?1 AND session_id = ?2",
        params![provider, id],
        |r| r.get(0),
    )?;
    let mut stmt = conn.prepare(
        "SELECT seq, role, content_text, tool_name, tool_input, tool_output,
                tool_call_id, thinking, parent_message_id, message_id, timestamp,
                provider_metadata_json
         FROM session_message WHERE provider = ?1 AND session_id = ?2
         ORDER BY seq LIMIT ?3 OFFSET ?4",
    )?;
    let end = offset.saturating_add(limit);
    let take = if limit == 0 { -1i64 } else { (end - offset) as i64 };
    let rows = stmt.query_map(params![provider, id, take, offset as i64], |r| {
        Ok(Message {
            seq: r.get::<_, i64>("seq")? as u32,
            role: r.get("role")?,
            content_text: r.get("content_text")?,
            tool_name: r.get("tool_name")?,
            tool_input: r.get("tool_input")?,
            tool_output: r.get("tool_output")?,
            tool_call_id: r.get("tool_call_id")?,
            thinking: r.get("thinking")?,
            parent_message_id: r.get("parent_message_id")?,
            message_id: r.get("message_id")?,
            timestamp: r.get("timestamp")?,
            provider_metadata_json: r
                .get::<_, Option<String>>("provider_metadata_json")?
                .unwrap_or_else(|| "{}".into()),
        })
    })?;
    let mut messages = Vec::new();
    for r in rows {
        messages.push(r?);
    }
    Ok(MessageWindow {
        messages,
        total: total as u32,
    })
}

/// Full-text-ish search across sessions (title/summary/project) plus a capped
/// scan of message bodies. Returns deduped sessions newest first.
pub fn search_sessions(conn: &Connection, query: &str, limit: u32) -> AppResult<Vec<Session>> {
    let q = query.trim();
    if q.is_empty() {
        return list_sessions(conn, None, None, limit);
    }
    let pat = format!("%{}%", escape_like(q));

    // Sessions whose title/summary/project match.
    let mut stmt = conn.prepare(
        "SELECT DISTINCT s.provider, s.id, s.title, s.summary, s.project, s.cwd,
                s.started_at, s.updated_at, s.ended_at, s.message_count, s.source_path,
                s.parent_session_id, s.agent_id, s.is_subagent, s.resume_command,
                s.child_count, s.source_files_json, s.provider_metadata_json
         FROM session s
         LEFT JOIN session_message m ON m.provider = s.provider AND m.session_id = s.id
         WHERE s.title LIKE ?1 ESCAPE '\\'
            OR s.summary LIKE ?1 ESCAPE '\\'
            OR s.project LIKE ?1 ESCAPE '\\'
            OR m.content_text LIKE ?1 ESCAPE '\\'
         ORDER BY s.updated_at DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![pat, limit as i64], row_to_session)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Total session row count (for diagnostics).
pub fn count_sessions(conn: &Connection) -> AppResult<u32> {
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM session", [], |r| r.get(0))?;
    Ok(n as u32)
}

/// Emit a session + its full message stream as the universal interchange JSON.
pub fn export_session(conn: &Connection, provider: &str, id: &str) -> AppResult<String> {
    let session = get_session(conn, provider, id)?.ok_or_else(|| {
        AppError::NotFound(format!("session {provider}/{id} not found"))
    })?;
    let window = read_messages(conn, provider, id, 0, 0)?; // limit 0 → all
    #[derive(serde::Serialize)]
    struct Export<'a> {
        session: &'a Session,
        messages: &'a [Message],
    }
    let export = Export {
        session: &session,
        messages: &window.messages,
    };
    Ok(serde_json::to_string_pretty(&export)?)
}

/// Escape a user search fragment for `LIKE ? ESCAPE '\'`. Escapes the escape
/// character first, then `%` and `_` — the old code escaped only `%`, so
/// `_` still wildcarded (`user_id` matched `userXid`) and a literal
/// backslash confused the ESCAPE clause itself.
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
/// didn't exist). The DB delete cascades to `session_message` and
/// `session_part` via the table-level `DELETE` (no FK in the schema, but
/// the `session_message` rows are matched on `(provider, session_id)` so
/// a plain `DELETE FROM session_message WHERE provider=?1 AND session_id=?2`
/// covers them; `session_part` shares the same composite key and is deleted
/// the same way before the session row goes away).
///
/// ponytail: we delete parts/messages by their (provider, session_id) keys
/// explicitly because the schema has no FK cascade — silently relying on a
/// future FK to clean them up would leak rows on every delete.
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
        "DELETE FROM session_part WHERE provider = ?1 AND session_id = ?2",
        params![provider, id],
    )?;
    tx.execute(
        "DELETE FROM session_message WHERE provider = ?1 AND session_id = ?2",
        params![provider, id],
    )?;
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


