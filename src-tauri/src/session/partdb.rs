//! Shared SQLite importer pipeline for agent harnesses that store sessions as
//! `session` / `message` / `part` tables with JSON `data` blobs (verified
//! against real installs of both):
//!
//! - **zcode-desktop** — `~/.zcode/cli/db/db.sqlite`
//! - **opencode-desktop** — `~/.local/share/opencode/opencode.db`
//!
//! One `session` row per conversation (`parent_id` marks subagent children), a
//! `message` row per turn carrying role metadata, a `part` row per content
//! block (`type`: text / reasoning / tool / step-* / …). Tool parts are updated
//! in place — `state.input` is the invocation, `state.output` (or
//! `state.error`) the result once finished. The two dialects differ only in
//! details the pipeline absorbs: zcode marks synthetic turns on the *message*,
//! opencode on the *text part*; a failed opencode tool carries `state.error`
//! instead of `state.output`.

use crate::error::AppResult;
use crate::session::semantic::{PartPayload, SemanticEvent};
use crate::session::{mtime_millis, RawFile};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

struct SessionRow {
    id: String,
    parent_id: Option<String>,
    directory: Option<String>,
    title: Option<String>,
    created: Option<i64>,
    updated: Option<i64>,
}

/// Read every session out of a part-style SQLite db. One `RawFile` per session
/// row (path is the db file itself). Missing tables → empty vec, not an error
/// (wrong/older layout = nothing to import).
pub(crate) fn collect(db: &Path) -> AppResult<Vec<RawFile>> {
    let Some(conn) = open_part_db(db) else {
        return Ok(vec![]);
    };
    let sessions = read_session_rows(&conn)?;
    let mut events = HashMap::new();
    let mut agent_names = HashMap::new();
    collect_events(&conn, None, &mut events, &mut agent_names)?;

    let db_mtime = mtime_millis(db);
    let mut out = Vec::new();
    for s in sessions {
        let evs = events.remove(&s.id).unwrap_or_default();
        if let Some(rf) = raw_file_for(s, evs, &agent_names, db, db_mtime) {
            out.push(rf);
        }
    }
    Ok(out)
}

/// ONE session out of a part-style db — the on-demand body read behind the
/// index-only session store (`session.source_files` names the db; the mirror
/// tables are gone). `None` when the id has no row or no conversational
/// events (same empty-shell skip as `collect`).
pub(crate) fn collect_one(db: &Path, session_id: &str) -> AppResult<Option<RawFile>> {
    let Some(conn) = open_part_db(db) else {
        return Ok(None);
    };
    let Some(s) = read_session_rows(&conn)?.into_iter().find(|s| s.id == session_id) else {
        return Ok(None);
    };
    let mut events = HashMap::new();
    let mut agent_names = HashMap::new();
    collect_events(&conn, Some(session_id), &mut events, &mut agent_names)?;
    let evs = events.remove(session_id).unwrap_or_default();
    Ok(raw_file_for(s, evs, &agent_names, db, mtime_millis(db)))
}

/// Cheap per-session change key for the incremental reconcile: the source
/// db's own `session` table as `(id, time_updated)` — a few hundred small
/// rows, no `part` scan. Diffing THIS against the stored index means one new
/// message in a 100k-part db only re-parses the ONE session it belongs to.
/// Empty when the db is missing / lacks the layout — the caller reads that
/// as "every session in it is gone" (same open-failure semantics as
/// `collect`).
pub(crate) fn session_index(db: &Path) -> Vec<(String, Option<i64>)> {
    let Some(conn) = open_part_db(db) else {
        return vec![];
    };
    let Ok(mut stmt) = conn.prepare("SELECT id, time_updated FROM session") else {
        return vec![];
    };
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1).ok().flatten()))
    });
    match rows {
        Ok(rows) => rows.flatten().collect(),
        Err(_) => vec![],
    }
}

/// Open the db read-only and verify the shared layout. `None` (not an error)
/// when the file can't open or the tables aren't there — a wrong/older layout
/// means nothing to import.
fn open_part_db(db: &Path) -> Option<rusqlite::Connection> {
    let conn = rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(|e| tracing::warn!("session db open failed ({}): {e}", db.display()))
    .ok()?;
    for table in ["session", "message", "part"] {
        if !table_exists(&conn, table) {
            return None;
        }
    }
    Some(conn)
}

fn read_session_rows(conn: &rusqlite::Connection) -> AppResult<Vec<SessionRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, parent_id, directory, title, time_created, time_updated FROM session",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(SessionRow {
            id: r.get(0)?,
            parent_id: r.get(1).ok().flatten(),
            directory: r.get(2).ok().flatten(),
            title: r.get(3).ok().flatten(),
            created: r.get(4).ok().flatten(),
            updated: r.get(5).ok().flatten(),
        })
    })?;
    Ok(rows.flatten().collect())
}

/// Fill per-session event buckets in one pass over the parts (joined to their
/// message for role/synthetic). time_created + rowid = native order. When
/// `only` is set, just that session's parts are read — the single-session
/// body read must not scan every part row in the db (the WHERE is built
/// per-shape because an `?1 IS NULL OR …` catch-all would defeat any index
/// the source app created on `part.session_id`).
fn collect_events(
    conn: &rusqlite::Connection,
    only: Option<&str>,
    events: &mut HashMap<String, Vec<SemanticEvent>>,
    agent_names: &mut HashMap<String, String>,
) -> AppResult<()> {
    let filter = match only {
        Some(_) => "WHERE p.session_id = ?1",
        None => "",
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT p.session_id, p.time_created, p.data, m.data
         FROM part p LEFT JOIN message m ON p.message_id = m.id
         {filter}
         ORDER BY p.time_created ASC, p.rowid ASC"
    ))?;
    // Bind exactly as many params as the SQL carries (0 or 1).
    let rows = stmt.query_map(rusqlite::params_from_iter(only.into_iter()), |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<i64>>(1).ok().flatten(),
            r.get::<_, Option<String>>(2).ok().flatten(),
            r.get::<_, Option<String>>(3).ok().flatten(),
        ))
    })?;
    for (session_id, part_ts, part_data, msg_data) in rows.flatten() {
        let Some(part_data) = part_data else { continue };
        let part: Value = match serde_json::from_str(&part_data) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let msg: Option<Value> = msg_data
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok());
        // Synthetic turns (todo reminders, context snapshots) are internal
        // bookkeeping, not conversation content. zcode marks the message,
        // opencode the text part — check both.
        let synthetic = part.get("synthetic").and_then(|s| s.as_bool()) == Some(true);
        if synthetic
            || msg.as_ref().and_then(|m| m.get("synthetic")).and_then(|s| s.as_bool())
                == Some(true)
        {
            continue;
        }
        if let Some(agent) = msg
            .as_ref()
            .and_then(|m| m.get("agent"))
            .and_then(|a| a.as_str())
        {
            agent_names.entry(session_id.clone()).or_insert(agent.to_string());
        }
        let role = msg.as_ref().and_then(|m| m.get("role")).and_then(|r| r.as_str());
        let ts = part
            .pointer("/time/start")
            .and_then(|t| t.as_i64())
            .or(part_ts);
        for mut ev in interpret_part(&part, role) {
            ev.ts = ts;
            events.entry(session_id.clone()).or_default().push(ev);
        }
    }
    Ok(())
}

/// Session row + its events → the `RawFile` both `collect` and `collect_one`
/// produce. `None` for empty shells (sessions compacted away / metadata-only
/// rows with no conversational events).
fn raw_file_for(
    s: SessionRow,
    evs: Vec<SemanticEvent>,
    agent_names: &HashMap<String, String>,
    db: &Path,
    db_mtime: i64,
) -> Option<RawFile> {
    if evs.is_empty() {
        return None;
    }
    let cwd = s.directory.filter(|d| !d.is_empty());
    let project = cwd
        .as_deref()
        .and_then(|c| Path::new(c).file_name())
        .and_then(|n| n.to_str())
        .map(String::from);
    let started = s.created.unwrap_or(db_mtime);
    let updated = s.updated.unwrap_or(db_mtime);
    let agent_id = agent_names.get(&s.id).cloned();
    Some(RawFile {
        path: db.to_path_buf(),
        canonical_id: s.id,
        is_sidechain: s.parent_id.is_some(),
        parent_session_id: s.parent_id,
        agent_id,
        title: s.title.unwrap_or_else(|| "(untitled)".into()),
        summary: String::new(),
        project,
        cwd,
        started_at: started,
        updated_at: updated,
        ended_at: Some(updated),
        events: evs,
        mtime: db_mtime,
    })
}

/// Map one native `part` row onto semantic events. A finished tool part yields
/// the invocation AND its result (same `call_id`, so the assembler pairs them);
/// `step-start`/`step-finish` are structural turn markers with no content and
/// are skipped; anything unrecognized is preserved verbatim (`Unknown`).
fn interpret_part(part: &Value, message_role: Option<&str>) -> Vec<SemanticEvent> {
    let ty = part.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let one = |payload: PartPayload| {
        let mut ev = SemanticEvent::new(payload);
        ev.raw_json = part.to_string();
        ev
    };
    match ty {
        "text" => {
            let text = part.get("text").and_then(|t| t.as_str()).unwrap_or("");
            if text.is_empty() {
                return vec![];
            }
            let payload = if message_role == Some("user") {
                PartPayload::UserMessage { text: text.into() }
            } else {
                PartPayload::AssistantMessage { text: text.into() }
            };
            vec![one(payload)]
        }
        "reasoning" => {
            let text = part.get("text").and_then(|t| t.as_str()).unwrap_or("");
            if text.is_empty() {
                return vec![];
            }
            vec![one(PartPayload::Thinking {
                text: text.into(),
                // zcode carries an anthropic signature; opencode has none.
                signature: part
                    .pointer("/metadata/anthropic/signature")
                    .and_then(|s| s.as_str())
                    .map(String::from),
            })]
        }
        "tool" => {
            let name = part.get("tool").and_then(|t| t.as_str()).unwrap_or("tool");
            let state = part.get("state").cloned().unwrap_or(Value::Null);
            let call_id = part.get("callID").and_then(|c| c.as_str()).map(String::from);
            let mut out = vec![one(PartPayload::ToolInvocation {
                name: name.into(),
                input: state.get("input").map(|i| i.to_string()),
                mcp: None,
                child_session_id: None,
            })];
            if let Some(ev) = out.first_mut() {
                ev.tool_call_id = call_id.clone();
            }
            // The tool row is updated in place: the result appears once the
            // call finishes (`state.output`; a failed opencode call carries
            // `state.error` instead). Absent → still running, invocation only.
            if let Some(output) = state
                .get("output")
                .and_then(|o| o.as_str())
                .or_else(|| state.get("error").and_then(|e| e.as_str()))
            {
                let status = state.get("status").and_then(|s| s.as_str()).unwrap_or("");
                let failed = matches!(status, "error" | "failed") || state.get("error").is_some();
                let mut ev = one(PartPayload::ToolResult {
                    output: output.to_string(),
                    is_error: Some(failed),
                    mcp: None,
                });
                ev.tool_call_id = call_id;
                out.push(ev);
            }
            out
        }
        "step-start" | "step-finish" => vec![],
        _ => vec![one(PartPayload::Unknown {
            raw_json: part.to_string(),
        })],
    }
}

fn table_exists(conn: &rusqlite::Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [name],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n > 0)
    .unwrap_or(false)
}

#[cfg(test)]
mod tests;
