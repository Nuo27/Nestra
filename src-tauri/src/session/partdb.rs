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

/// Read every session out of a part-style SQLite db. One `RawFile` per session
/// row (path is the db file itself). Missing tables → empty vec, not an error
/// (wrong/older layout = nothing to import).
pub(crate) fn collect(db: &Path) -> AppResult<Vec<RawFile>> {
    let conn = match rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("session db open failed ({}): {e}", db.display());
            return Ok(vec![]);
        }
    };
    for table in ["session", "message", "part"] {
        if !table_exists(&conn, table) {
            return Ok(vec![]);
        }
    }

    struct SessionRow {
        id: String,
        parent_id: Option<String>,
        directory: Option<String>,
        title: Option<String>,
        created: Option<i64>,
        updated: Option<i64>,
    }
    let mut stmt = conn.prepare(
        "SELECT id, parent_id, directory, title, time_created, time_updated FROM session",
    )?;
    let sessions: Vec<SessionRow> = stmt
        .query_map([], |r| {
            Ok(SessionRow {
                id: r.get(0)?,
                parent_id: r.get(1).ok().flatten(),
                directory: r.get(2).ok().flatten(),
                title: r.get(3).ok().flatten(),
                created: r.get(4).ok().flatten(),
                updated: r.get(5).ok().flatten(),
            })
        })?
        .flatten()
        .collect();
    drop(stmt);

    // Per-session event buckets, filled in one pass over the parts (joined to
    // their message for role/synthetic). time_created + rowid = native order.
    let mut events: HashMap<String, Vec<SemanticEvent>> = HashMap::new();
    let mut agent_names: HashMap<String, String> = HashMap::new();
    let mut stmt = conn.prepare(
        "SELECT p.session_id, p.time_created, p.data, m.data
         FROM part p LEFT JOIN message m ON p.message_id = m.id
         ORDER BY p.time_created ASC, p.rowid ASC",
    )?;
    let rows = stmt.query_map([], |r| {
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
    drop(stmt);

    let db_mtime = mtime_millis(db);
    let mut out = Vec::new();
    for s in sessions {
        let sidechain = s.parent_id.is_some();
        let cwd = s.directory.filter(|d| !d.is_empty());
        let project = cwd
            .as_deref()
            .and_then(|c| Path::new(c).file_name())
            .and_then(|n| n.to_str())
            .map(String::from);
        let started = s.created.unwrap_or(db_mtime);
        let updated = s.updated.unwrap_or(db_mtime);
        let evs = events.remove(&s.id).unwrap_or_default();
        // Skip empty shells (sessions compacted away / metadata-only rows).
        if evs.is_empty() {
            continue;
        }
        let agent_id = agent_names.get(&s.id).cloned();
        out.push(RawFile {
            path: db.to_path_buf(),
            canonical_id: s.id,
            is_sidechain: sidechain,
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
        });
    }
    Ok(out)
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
