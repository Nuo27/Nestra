//! Context Lifecycle R1 — the handoff artifact.
//!
//! Boundary with the agent (Pi especially): the agent owns the context window
//! and compaction. Nestra ANALYZES an imported session's semantic parts,
//! produces an explicit editable markdown artifact, and writes files the agent
//! already reads natively (`.pi/` context files). No black-box memory store.
//!
//! Pieces:
//!   - [`parts_for_session`] — the FIRST reader of the `session_part` table
//!     (until now it was write-only; the UI reads the projected
//!     `session_message` table instead).
//!   - [`build_sections`] / [`render_markdown`] — pure structural extraction
//!     (no LLM call; a condensed-decisions pass is a later option).
//!   - [`context_pressure`] — two numbers on read (estimated context use +
//!     top consumer); a nudge, not a subsystem.
//!   - persistence — markdown on disk (the artifact) + a `handoff` index row.
//!     The file is user-editable after creation; Nestra never rewrites an
//!     existing artifact (fresh uuid per handoff) and deleting a row leaves
//!     the file — it is the user's the moment it is written.
//!   - [`inject_handoff`] — the unsupervised injection path: copy into
//!     `<cwd>/.pi/handoff-<id>.md` + one removable reference line in
//!     `.pi/APPEND_SYSTEM.md`. (The supervised alternative — sending a handoff
//!     as the initial prompt of a Nestra-spawned session — is planned; the
//!     review supervisor it would reuse already exists. See
//!     docs/features/README.md P0.)

use std::io::Write as _;
use std::path::PathBuf;

use rusqlite::Connection;

use crate::error::{AppError, AppResult};

use super::semantic::{Part, PartPayload, parse_payload};

/// How many trailing assistant turns feed the "Decisions" section.
const DECISION_TURNS: usize = 5;
/// Cap on failed-attempt lines kept (most recent).
const MAX_FAILED: usize = 5;
/// Per-item truncation (chars) for extracted sections (non-decision text).
const TRUNC_CHARS: usize = 300;
/// Decisions: the NEWEST turn (most current state) may keep this many chars;
/// older kept turns get the shorter budget. The caps are independent of the
/// count cap above.
const LATEST_TRUNC: usize = 400;
const EARLIER_TRUNC: usize = 200;
/// Chars-per-token fallback for the context estimate.
const CHARS_PER_TOKEN: usize = 4;
/// ponytail: fixed comparison window — importers don't record the session's
/// active model yet (`SystemEvent.model` is never populated), so a real
/// `ModelAbilities.limit.context` lookup has nothing to key off. Revisit when
/// an importer starts promoting the model into parts.
pub const DEFAULT_CONTEXT_WINDOW: i64 = 200_000;

// ---- read side: the first session_part reader ------------------------------

/// All parts of one session, ordered. Undecodable payloads are skipped (the
/// typed payload is the source of truth; `raw_json` is persisted blank).
pub fn parts_for_session(
    conn: &Connection,
    provider: &str,
    session_id: &str,
) -> AppResult<Vec<Part>> {
    let mut stmt = conn.prepare(
        "SELECT seq, payload_json, tool_call_id, ts, provider_metadata_json
         FROM session_part
         WHERE provider = ?1 AND session_id = ?2
         ORDER BY seq, part_idx",
    )?;
    let rows = stmt.query_map(rusqlite::params![provider, session_id], |r| {
        Ok((
            r.get::<_, u32>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<i64>>(3)?,
            r.get::<_, String>(4)?,
        ))
    })?;
    let mut parts = Vec::new();
    for row in rows {
        let (seq, payload_json, tool_call_id, ts, provider_metadata_json) = row?;
        if let Some(payload) = parse_payload(&payload_json) {
            parts.push(Part {
                seq,
                payload,
                tool_call_id,
                // `message_id`/`parent_message_id` are not persisted columns —
                // branch-targeted handoffs are a later option.
                message_id: None,
                parent_message_id: None,
                ts,
                raw_json: String::new(),
                provider_metadata_json,
            });
        }
    }
    Ok(parts)
}

// ---- structural extraction --------------------------------------------------

/// Structured handoff sections (persisted as `handoff.sections_json` for
/// UI/search). Lossy by design — the editable markdown is the artifact.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HandoffSections {
    pub goal: Option<String>,
    pub decisions: Vec<String>,
    pub modified_files: Vec<String>,
    pub failed_attempts: Vec<String>,
    pub subagents: Vec<String>,
    pub next_steps: Option<String>,
}

/// Tool names (lowercased) whose invocation edits a file. Cross-agent
/// vocabulary: Claude Code, OpenCode, ZCode tool spellings.
const FILE_TOOLS: [&str; 9] = [
    "edit",
    "write",
    "multiedit",
    "notebookedit",
    "apply_patch",
    "applypatch",
    "str_replace_based_edit_tool",
    "edit_file",
    "write_file",
];

/// Extract the structured sections from a session's parts. Pure — no DB, no
/// LLM, no clock.
pub fn build_sections(parts: &[Part]) -> HandoffSections {
    let mut s = HandoffSections::default();
    let mut assistant_texts: Vec<String> = Vec::new();
    // tool_call_id → invocation name, for pairing error results with tools.
    let mut tool_names: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for p in parts {
        match &p.payload {
            PartPayload::UserMessage { text } => {
                if s.goal.is_none() && !text.trim().is_empty() {
                    s.goal = Some(truncate_chars(text.trim(), 600));
                }
            }
            PartPayload::AssistantMessage { text } => {
                if !text.trim().is_empty() {
                    assistant_texts.push(text.trim().to_string());
                }
            }
            PartPayload::ToolInvocation { name, input, .. } => {
                if let Some(id) = &p.tool_call_id {
                    tool_names.insert(id.clone(), name.clone());
                }
                if FILE_TOOLS.contains(&name.to_lowercase().as_str()) {
                    if let Some(path) = input.as_deref().and_then(file_path_of) {
                        if !s.modified_files.contains(&path) {
                            s.modified_files.push(path);
                        }
                    }
                }
            }
            PartPayload::ToolResult { output, is_error, .. } => {
                if is_error.unwrap_or(false) {
                    let tool = p
                        .tool_call_id
                        .as_ref()
                        .and_then(|id| tool_names.get(id))
                        .cloned()
                        .unwrap_or_else(|| "tool".into());
                    let line = output.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
                    s.failed_attempts
                        .push(format!("{tool} — {}", truncate_chars(line.trim(), 160)));
                }
            }
            PartPayload::SubAgent {
                agent_id,
                child_session_id,
                description,
            } => {
                let mut line = agent_id.clone();
                if let Some(d) = description {
                    if !d.trim().is_empty() {
                        line.push_str(": ");
                        line.push_str(&truncate_chars(d.trim(), 120));
                    }
                }
                if let Some(c) = child_session_id {
                    line.push_str(&format!(" (child {c})"));
                }
                if !s.subagents.contains(&line) {
                    s.subagents.push(line);
                }
            }
            _ => {}
        }
    }
    // Pipeline order is fixed: dedupe on the FULL normalized text FIRST,
    // then truncate — truncated fragments must never participate in the
    // duplicate check (common prefixes would otherwise merge distinct turns).
    let kept: Vec<&String> = assistant_texts
        .iter()
        .rev()
        .take(DECISION_TURNS)
        .rev()
        .fold(Vec::new(), |mut acc, t| {
            let norm = normalize_decision(t);
            if !acc.iter().any(|(_, n)| *n == norm) {
                acc.push((t, norm));
            }
            acc
        })
        .into_iter()
        .map(|(t, _)| t)
        .collect();
    if s.failed_attempts.len() > MAX_FAILED {
        s.failed_attempts.drain(0..s.failed_attempts.len() - MAX_FAILED);
    }
    s.decisions = kept
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let max = if i == kept.len() - 1 { LATEST_TRUNC } else { EARLIER_TRUNC };
            truncate_at_boundary(t, max)
        })
        .collect();
    // "Next steps": the tail of the last assistant message (its closing is
    // where agents state what remains).
    if let Some(last) = assistant_texts.last() {
        let tail_start = last.len().saturating_sub(TRUNC_CHARS);
        let tail_start = last.floor_char_boundary(tail_start);
        s.next_steps = Some(last[tail_start..].trim().to_string());
    }
    s
}

/// Pull a file path out of a tool-invocation input JSON (any common key
/// spelling). `None` when the input isn't JSON or carries no path.
fn file_path_of(input: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(input).ok()?;
    for key in ["file_path", "path", "filePath", "target_file"] {
        if let Some(p) = v.get(key).and_then(|x| x.as_str()) {
            if !p.is_empty() {
                return Some(p.to_string());
            }
        }
    }
    None
}

/// Truncate on a char boundary (a byte slice mid-multibyte would panic).
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

/// Stable, reproducible duplicate key for a decision: trim + collapse
/// internal whitespace runs. Case-sensitive (predictable), full-string only —
/// no prefix/similarity matching, so distinct decisions sharing an opening
/// are never merged.
fn normalize_decision(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Truncate `s` to at most `max` chars at the largest boundary within the
/// limit: last newline → last sentence end (`. `/`。`) → plain char boundary.
/// A cut appends the `…` marker so the truncation is visible.
fn truncate_at_boundary(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    // Byte index just past the max-th char (a char boundary by construction).
    let limit = s.char_indices().nth(max).map_or(s.len(), |(i, _)| i);
    let window = &s[..limit];
    let cut = window
        .rfind('\n')
        .or_else(|| window.rfind(". ").map(|i| i + 1))
        .or_else(|| window.rfind('。').map(|i| i + 1))
        .unwrap_or(limit);
    format!("{}…", window[..cut].trim_end())
}

/// Render the sections as the handoff markdown artifact. Empty sections are
/// omitted; the user edits this freely before/after committing.
pub fn render_markdown(title: &str, s: &HandoffSections) -> String {
    let mut md = String::new();
    md.push_str(&format!("# Handoff: {title}\n\n"));
    if let Some(goal) = &s.goal {
        md.push_str("## Goal\n\n");
        md.push_str(goal);
        md.push_str("\n\n");
    }
    if !s.decisions.is_empty() {
        md.push_str("## Decisions\n\n");
        for d in &s.decisions {
            md.push_str(&format!("- {}\n", d.replace('\n', " ")));
        }
        md.push('\n');
    }
    if !s.modified_files.is_empty() {
        md.push_str("## Modified files\n\n");
        for f in &s.modified_files {
            md.push_str(&format!("- `{f}`\n"));
        }
        md.push('\n');
    }
    if !s.failed_attempts.is_empty() {
        md.push_str("## Failed attempts\n\n");
        for f in &s.failed_attempts {
            md.push_str(&format!("- {f}\n"));
        }
        md.push('\n');
    }
    if !s.subagents.is_empty() {
        md.push_str("## Subagents spawned\n\n");
        for a in &s.subagents {
            md.push_str(&format!("- {a}\n"));
        }
        md.push('\n');
    }
    if let Some(next) = &s.next_steps {
        md.push_str("## Next steps\n\n");
        md.push_str(next);
        md.push_str("\n\n");
    }
    md
}

// ---- context pressure -------------------------------------------------------

/// Two numbers on read: estimated context use vs. a comparison window, plus
/// the single largest part. A nudge to generate a handoff, not a subsystem.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ContextPressure {
    pub est_tokens: i64,
    pub window_tokens: i64,
    /// 0–100, clamped.
    pub pct: u8,
    /// Always true today: the char-based estimate is the only path. A
    /// real-usage path (summing input/cache/output) stays disabled until the
    /// fields' mutual exclusivity is confirmed against a real data contract
    /// (docs/features/README.md, P0) — usage metadata is not force-counted.
    pub estimated: bool,
    /// Label of the largest single part (e.g. `tool result`, `tool call: Bash`).
    pub top_consumer: Option<String>,
}

pub fn context_pressure(parts: &[Part], window_override: Option<i64>) -> ContextPressure {
    let mut total_chars: usize = 0;
    let mut top: Option<(usize, String)> = None;
    for p in parts {
        let (w, label) = match &p.payload {
            PartPayload::UserMessage { text } | PartPayload::AssistantMessage { text }
            | PartPayload::Thinking { text, .. } => (text.len(), "message text".into()),
            PartPayload::ToolInvocation { input, name, .. } => (
                input.as_deref().map_or(0, str::len),
                format!("tool call: {name}"),
            ),
            PartPayload::ToolResult { output, .. } => (output.len(), "tool result".into()),
            PartPayload::SubAgent { description, .. } => {
                (description.as_deref().map_or(0, str::len), "subagent".into())
            }
            PartPayload::Attachment(a) => (a.title.as_deref().map_or(0, str::len), "attachment".into()),
            PartPayload::SystemEvent { text, .. } => (text.len(), "system event".into()),
            PartPayload::Unknown { raw_json } => (raw_json.len(), "unknown".into()),
        };
        total_chars += w;
        if top.as_ref().map_or(true, |(tw, _)| w > *tw) {
            top = Some((w, label));
        }
    }
    let est_tokens = (total_chars / CHARS_PER_TOKEN) as i64;
    let window_tokens = window_override.unwrap_or(DEFAULT_CONTEXT_WINDOW);
    let pct = (est_tokens * 100 / window_tokens).clamp(0, 100) as u8;
    ContextPressure {
        est_tokens,
        window_tokens,
        pct,
        estimated: true,
        top_consumer: top.filter(|(w, _)| *w > 0).map(|(_, l)| l),
    }
}

/// The session's active model — the newest part metadata that names one.
pub fn last_model(parts: &[Part]) -> Option<String> {
    for p in parts.iter().rev() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&p.provider_metadata_json) {
            if let Some(m) = v
                .get("model")
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
            {
                return Some(m.to_string());
            }
        }
    }
    None
}

/// Context window for a model id from the catalog. Exact match first (raw id,
/// then normalized); a `LIKE` fallback only when it resolves to exactly ONE
/// distinct model_id — ambiguity (or no hit) keeps the caller's default
/// window rather than guessing. Read-only over `model_catalog`.
pub fn window_for_model(conn: &Connection, model: &str) -> Option<i64> {
    let exact = [model.to_string(), crate::model_abilities::normalize(model)]
        .iter()
        .find_map(|m| {
            conn.query_row(
                "SELECT abilities_json FROM model_catalog WHERE model_id = ?1",
                rusqlite::params![m],
                |r| r.get::<_, String>(0),
            )
            .ok()
        });
    let abilities_json = match exact {
        Some(j) => j,
        None => {
            let like = format!("%{}%", crate::model_abilities::normalize(model));
            let ids: Vec<String> = conn
                .prepare("SELECT DISTINCT model_id FROM model_catalog WHERE model_id LIKE ?1")
                .ok()?
                .query_map(rusqlite::params![like], |r| r.get::<_, String>(0))
                .ok()?
                .collect::<rusqlite::Result<_>>()
                .ok()?;
            if ids.len() != 1 {
                return None;
            }
            conn.query_row(
                "SELECT abilities_json FROM model_catalog WHERE model_id = ?1",
                rusqlite::params![ids[0]],
                |r| r.get::<_, String>(0),
            )
            .ok()?
        }
    };
    let abilities: crate::model_abilities::ModelAbilities =
        serde_json::from_str(&abilities_json).ok()?;
    abilities.limit.map(|l| l.context as i64)
}

/// Real usage sums from part metadata, when importers recorded them.
/// `(tokens, cost)` — both `None` when no part carried usage.
///
/// Independent consumer from [`context_pressure`]: per-line `input + output`
/// only. The input/output mapping follows the gateway's canonical vocabulary
/// merge (`gateway::stream::merge_usage_obj`, pinned by its tests — both
/// vocabularies map input-side → `input_tokens`, output-side →
/// `output_tokens`). Cache fields are deliberately NOT summed here, so this
/// is unaffected by the cache-exclusivity verification that gates the
/// pressure real-usage path.
fn usage_totals(parts: &[Part]) -> (Option<i64>, Option<f64>) {
    let mut tokens: i64 = 0;
    let mut token_seen = false;
    let mut cost: f64 = 0.0;
    let mut cost_seen = false;
    for p in parts {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&p.provider_metadata_json) else {
            continue;
        };
        if let Some(u) = v.get("usage") {
            let inp = u
                .get("input_tokens")
                .or_else(|| u.get("prompt_tokens"))
                .and_then(|x| x.as_i64());
            let out = u
                .get("output_tokens")
                .or_else(|| u.get("completion_tokens"))
                .and_then(|x| x.as_i64());
            if let Some(n) = inp {
                tokens += n;
                token_seen = true;
            }
            if let Some(n) = out {
                tokens += n;
                token_seen = true;
            }
        }
        if let Some(c) = v
            .get("cost_usd")
            .or_else(|| v.get("total_cost"))
            .and_then(|x| x.as_f64())
        {
            cost += c;
            cost_seen = true;
        }
    }
    (token_seen.then_some(tokens), cost_seen.then_some(cost))
}

// ---- persistence ------------------------------------------------------------

/// One `handoff` row (+ the artifact content, best-effort — the user may have
/// moved or deleted the file).
#[derive(Debug, Clone, serde::Serialize)]
pub struct HandoffInfo {
    pub id: String,
    pub source_provider: String,
    pub source_session_id: String,
    pub target_session_id: Option<String>,
    pub token_snapshot: Option<i64>,
    pub cost_snapshot: Option<f64>,
    pub artifact_path: String,
    pub created_at: i64,
    pub sections: HandoffSections,
    /// The artifact file's current content, when readable.
    pub markdown: Option<String>,
}

/// Where handoff artifacts for a session live: inside the session's repo when
/// its cwd is known (`.nestra/handoffs/`), else under the Nestra home
/// (`~/.nestra/handoffs/` — same root convention as skills).
fn artifact_dir(cwd: Option<&str>) -> AppResult<PathBuf> {
    let base = match cwd {
        Some(c) if !c.is_empty() => PathBuf::from(c).join(".nestra"),
        _ => crate::db::home_dir()?.join(".nestra"),
    };
    Ok(base.join("handoffs"))
}

fn insert_handoff_row(conn: &Connection, info: &HandoffInfo) -> AppResult<()> {
    conn.execute(
        "INSERT INTO handoff
           (id, source_provider, source_session_id, target_session_id,
            token_snapshot, cost_snapshot, artifact_path, sections_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            info.id,
            info.source_provider,
            info.source_session_id,
            info.target_session_id,
            info.token_snapshot,
            info.cost_snapshot,
            info.artifact_path,
            serde_json::to_string(&info.sections).unwrap_or_else(|_| "{}".into()),
            info.created_at,
        ],
    )?;
    Ok(())
}

fn row_to_info(r: &rusqlite::Row<'_>) -> rusqlite::Result<HandoffInfo> {
    let artifact_path: String = r.get(6)?;
    let sections_json: String = r.get(7)?;
    Ok(HandoffInfo {
        id: r.get(0)?,
        source_provider: r.get(1)?,
        source_session_id: r.get(2)?,
        target_session_id: r.get(3)?,
        token_snapshot: r.get(4)?,
        cost_snapshot: r.get(5)?,
        created_at: r.get(8)?,
        sections: serde_json::from_str(&sections_json).unwrap_or_default(),
        markdown: std::fs::read_to_string(&artifact_path).ok(),
        artifact_path,
    })
}

const HANDOFF_COLS: &str =
    "id, source_provider, source_session_id, target_session_id, token_snapshot, \
     cost_snapshot, artifact_path, sections_json, created_at";

/// Build + persist one handoff: extract sections, snapshot usage, write the
/// (user-provided, possibly edited) markdown artifact, insert the index row.
/// Sections/snapshots are recomputed server-side from the parts — the editable
/// text is the artifact, the structure is the index.
pub fn save_handoff(
    conn: &Connection,
    provider: &str,
    session_id: &str,
    markdown: &str,
) -> AppResult<HandoffInfo> {
    let (_title, cwd): (String, Option<String>) = conn
        .query_row(
            "SELECT title, cwd FROM session WHERE provider = ?1 AND id = ?2",
            rusqlite::params![provider, session_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|_| {
            AppError::Validation(format!("session {provider}/{session_id} not found"))
        })?;
    let parts = parts_for_session(conn, provider, session_id)?;
    let sections = build_sections(&parts);
    let (token_snapshot, cost_snapshot) = usage_totals(&parts);

    let id = uuid::Uuid::new_v4().to_string();
    let dir = artifact_dir(cwd.as_deref())?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| AppError::Internal(format!("create handoff dir: {e}")))?;
    let path = dir.join(format!("{id}.md"));
    std::fs::write(&path, markdown)
        .map_err(|e| AppError::Internal(format!("write handoff artifact: {e}")))?;

    let info = HandoffInfo {
        id,
        source_provider: provider.to_string(),
        source_session_id: session_id.to_string(),
        target_session_id: None,
        token_snapshot,
        cost_snapshot,
        artifact_path: path.to_string_lossy().into_owned(),
        created_at: chrono::Utc::now().timestamp_millis(),
        sections,
        markdown: Some(markdown.to_string()),
    };
    insert_handoff_row(conn, &info)?;
    Ok(info)
}

/// Handoffs generated from one session, newest-first.
pub fn list_handoffs(
    conn: &Connection,
    provider: &str,
    session_id: &str,
) -> AppResult<Vec<HandoffInfo>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {HANDOFF_COLS} FROM handoff
         WHERE source_provider = ?1 AND source_session_id = ?2
         ORDER BY created_at DESC"
    ))?;
    let rows = stmt.query_map(rusqlite::params![provider, session_id], row_to_info)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn get_handoff_row(conn: &Connection, id: &str) -> AppResult<HandoffInfo> {
    conn.query_row(
        &format!("SELECT {HANDOFF_COLS} FROM handoff WHERE id = ?1"),
        rusqlite::params![id],
        row_to_info,
    )
    .map_err(|_| AppError::Validation(format!("handoff {id} not found")))
}

/// Delete the index row. The artifact file stays — it is the user's from the
/// moment it was written (Nestra never deletes a user-editable file it no
/// longer owns).
pub fn delete_handoff(conn: &Connection, id: &str) -> AppResult<()> {
    conn.execute("DELETE FROM handoff WHERE id = ?1", rusqlite::params![id])?;
    Ok(())
}

/// Record the native session a handoff was delivered into via RPC spawn
/// (`target_session_id`'s only writer — null until the spawn's stream
/// reveals the id).
pub fn set_target_session(conn: &Connection, id: &str, session_id: &str) -> AppResult<()> {
    conn.execute(
        "UPDATE handoff SET target_session_id = ?2 WHERE id = ?1",
        rusqlite::params![id, session_id],
    )?;
    Ok(())
}

// ---- injection (unsupervised, context-file path) ----------------------------

/// The reference line appended to `.pi/APPEND_SYSTEM.md` — Pi's designated
/// append surface, so the user's `AGENTS.md` is never touched.
fn reference_line(id: &str) -> String {
    format!("- [nestra-handoff] context: read .pi/handoff-{id}.md before continuing")
}

/// Copy the artifact into the session repo's `.pi/` context dir + add the
/// removable reference line. Idempotent: a repeated inject only refreshes the
/// file (the reference line is deduplicated).
pub fn inject_handoff(conn: &Connection, id: &str) -> AppResult<String> {
    let info = get_handoff_row(conn, id)?;
    let cwd: String = conn
        .query_row(
            "SELECT cwd FROM session WHERE provider = ?1 AND id = ?2",
            rusqlite::params![info.source_provider, info.source_session_id],
            |r| r.get(0),
        )
        .map_err(|_| {
            AppError::Validation(format!(
                "session {}/{} not found",
                info.source_provider, info.source_session_id
            ))
        })?;
    if cwd.is_empty() {
        return Err(AppError::Validation(
            "session has no working directory — inject needs the repo's .pi/ dir".into(),
        ));
    }
    let content = info
        .markdown
        .ok_or_else(|| AppError::Internal("handoff artifact unreadable".into()))?;
    let pi_dir = PathBuf::from(&cwd).join(".pi");
    std::fs::create_dir_all(&pi_dir)
        .map_err(|e| AppError::Internal(format!("create .pi dir: {e}")))?;
    let handoff_path = pi_dir.join(format!("handoff-{id}.md"));
    std::fs::write(&handoff_path, content)
        .map_err(|e| AppError::Internal(format!("write .pi handoff: {e}")))?;

    let append_path = pi_dir.join("APPEND_SYSTEM.md");
    let existing = std::fs::read_to_string(&append_path).unwrap_or_default();
    let line = reference_line(id);
    if !existing.lines().any(|l| l == line) {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&append_path)
            .map_err(|e| AppError::Internal(format!("open APPEND_SYSTEM.md: {e}")))?;
        writeln!(f, "{line}").map_err(|e| AppError::Internal(format!("append reference: {e}")))?;
    }
    Ok(handoff_path.to_string_lossy().into_owned())
}

/// Remove the injected `.pi/handoff-<id>.md` and its reference line (the
/// anti-pollution counterpart of [`inject_handoff`]). Missing pieces are fine.
pub fn remove_injected_handoff(conn: &Connection, id: &str) -> AppResult<()> {
    let info = get_handoff_row(conn, id)?;
    let cwd: Option<String> = conn
        .query_row(
            "SELECT cwd FROM session WHERE provider = ?1 AND id = ?2",
            rusqlite::params![info.source_provider, info.source_session_id],
            |r| r.get(0),
        )
        .unwrap_or(None);
    let Some(cwd) = cwd.filter(|c| !c.is_empty()) else {
        return Ok(());
    };
    let pi_dir = PathBuf::from(cwd).join(".pi");
    let line = reference_line(id);
    let append_path = pi_dir.join("APPEND_SYSTEM.md");
    if let Ok(existing) = std::fs::read_to_string(&append_path) {
        if existing.lines().any(|l| l == line) {
            let kept: String = existing
                .lines()
                .filter(|l| *l != line)
                .collect::<Vec<_>>()
                .join("\n");
            std::fs::write(&append_path, if kept.is_empty() { String::new() } else { format!("{kept}\n") })
                .map_err(|e| AppError::Internal(format!("rewrite APPEND_SYSTEM.md: {e}")))?;
        }
    }
    let _ = std::fs::remove_file(pi_dir.join(format!("handoff-{id}.md")));
    Ok(())
}

// ---- knowledge files ----------------------------------------------------------

/// Sanitize a title into a filename slug.
fn slugify(s: &str) -> String {
    let slug: String = s
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        "knowledge".into()
    } else {
        truncate_chars(&slug, 48).trim_end_matches('-').to_string()
    }
}

/// Promote a handoff to a durable knowledge file: `~/.nestra/knowledge/
/// <slug>-<short>.md` with YAML frontmatter. The file IS the store — no table,
/// no route; the agent reads it via the same context-file mechanism.
pub fn handoff_to_knowledge(conn: &Connection, id: &str, kind: &str) -> AppResult<String> {
    let info = get_handoff_row(conn, id)?;
    let title = info
        .sections
        .goal
        .clone()
        .unwrap_or_else(|| format!("handoff {}", info.source_session_id));
    let short: String = info.id.chars().take(8).collect();
    let dir = crate::db::home_dir()?.join(".nestra").join("knowledge");
    std::fs::create_dir_all(&dir)
        .map_err(|e| AppError::Internal(format!("create knowledge dir: {e}")))?;
    let path = dir.join(format!("{}-{short}.md", slugify(&truncate_chars(&title, 60))));
    let body = info.markdown.ok_or_else(|| {
        AppError::Internal("handoff artifact unreadable — cannot promote".into())
    })?;
    let front = format!(
        "---\ntype: {kind}\nstatus: active\napplies_to: {}\ntags: [handoff]\ncreated_at: {}\n---\n\n",
        info.source_provider,
        chrono::Utc::now().to_rfc3339()
    );
    std::fs::write(&path, format!("{front}{body}"))
        .map_err(|e| AppError::Internal(format!("write knowledge file: {e}")))?;
    Ok(path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::semantic::PartPayload;

    fn part(seq: u32, payload: PartPayload) -> Part {
        Part {
            seq,
            payload,
            tool_call_id: None,
            message_id: None,
            parent_message_id: None,
            ts: None,
            raw_json: String::new(),
            provider_metadata_json: "{}".into(),
        }
    }

    fn fixture_parts() -> Vec<Part> {
        vec![
            part(0, PartPayload::UserMessage { text: "Fix the login bug in auth.rs".into() }),
            part(1, PartPayload::AssistantMessage { text: "I'll start by reading the auth module.".into() }),
            part(2, PartPayload::ToolInvocation {
                name: "Edit".into(),
                input: Some(r#"{"file_path":"src/auth.rs"}"#.into()),
                mcp: None,
                child_session_id: None,
            }),
            part(3, PartPayload::ToolResult { output: "ok".into(), is_error: Some(false), mcp: None }),
            part(4, PartPayload::ToolResult { output: "".into(), is_error: Some(false), mcp: None }), // placeholder replaced below
        ]
    }

    #[test]
    fn build_sections_extracts_all_sections() {
        let mut parts = fixture_parts();
        // Replace placeholder with an errored tool result tied to a Bash call.
        parts.pop();
        parts.push(Part {
            tool_call_id: Some("call-1".into()),
            payload: PartPayload::ToolInvocation {
                name: "Bash".into(),
                input: Some(r#"{"command":"cargo test"}"#.into()),
                mcp: None,
                child_session_id: None,
            },
            ..part(4, PartPayload::UserMessage { text: String::new() })
        });
        parts.push(Part {
            tool_call_id: Some("call-1".into()),
            payload: PartPayload::ToolResult {
                output: "error: expected `;`\n  --> src/main.rs:1:1".into(),
                is_error: Some(true),
                mcp: None,
            },
            ..part(5, PartPayload::UserMessage { text: String::new() })
        });
        parts.push(part(6, PartPayload::AssistantMessage {
            text: "Switched to token-based sessions. Next: re-run the test suite and update the migration docs.".into(),
        }));
        parts.push(part(7, PartPayload::SubAgent {
            agent_id: "researcher".into(),
            child_session_id: Some("child-9".into()),
            description: Some("find failing tests".into()),
        }));

        let s = build_sections(&parts);
        assert_eq!(s.goal.as_deref(), Some("Fix the login bug in auth.rs"));
        assert_eq!(s.modified_files, vec!["src/auth.rs".to_string()]);
        assert_eq!(s.failed_attempts.len(), 1);
        assert!(s.failed_attempts[0].starts_with("Bash — error:"), "{}", s.failed_attempts[0]);
        assert_eq!(s.subagents.len(), 1);
        assert!(s.subagents[0].contains("researcher") && s.subagents[0].contains("child-9"));
        assert_eq!(s.decisions.len(), 2);
        assert!(s.next_steps.as_deref().unwrap().contains("Next: re-run"));

        let md = render_markdown("Login bug", &s);
        assert!(md.contains("# Handoff: Login bug"));
        assert!(md.contains("`src/auth.rs`"));
        assert!(md.contains("## Failed attempts"));
    }

    #[test]
    fn decisions_dedupe_on_full_normalized_text() {
        // Two turns that merely SHARE a prefix are distinct decisions…
        let a = "Fix the login bug in auth.rs by validating tokens.";
        let b = "Fix the login bug in auth.rs by adding rate limits.";
        // …while an exact restatement (modulo whitespace) is a duplicate.
        let c = "  Fix the login bug in   auth.rs by validating tokens.  ";
        let parts = vec![
            part(0, PartPayload::AssistantMessage { text: a.into() }),
            part(1, PartPayload::AssistantMessage { text: b.into() }),
            part(2, PartPayload::AssistantMessage { text: c.into() }),
        ];
        let s = build_sections(&parts);
        assert_eq!(s.decisions.len(), 2, "prefix-shared turns survive, exact restatement merges");
        assert!(s.decisions[0].starts_with("Fix the login bug in auth.rs by validating tokens."));
        assert!(s.decisions[1].starts_with("Fix the login bug in auth.rs by adding rate limits."));
    }

    #[test]
    fn decisions_truncate_at_boundaries_with_recency_weighting() {
        // A long line with a newline inside the budget → cut at the newline.
        let t = format!("line one
{}", "x".repeat(300));
        let cut = truncate_at_boundary(&t, 100);
        assert!(cut.starts_with("line one"));
        assert!(cut.ends_with('…'));
        assert!(cut.len() <= 100 + "…".len() + 8, "{}", cut.len());

        // No newline but a sentence end → cut after the sentence.
        let t2 = format!("First sentence. {}second", "y".repeat(120));
        let cut2 = truncate_at_boundary(&t2, 60);
        assert_eq!(cut2, "First sentence.…");

        // No boundary at all → plain char cut + marker.
        let t3 = "z".repeat(50);
        let cut3 = truncate_at_boundary(&t3, 20);
        assert_eq!(cut3.chars().count(), 21); // 20 chars + marker
        assert!(cut3.ends_with('…'));

        // Short text stays untouched (no marker).
        assert_eq!(truncate_at_boundary("short", 100), "short");

        // Recency weighting: the NEWEST turn gets the longer budget.
        let mut parts = Vec::new();
        for i in 0..6 {
            parts.push(part(i as u32, PartPayload::AssistantMessage {
                text: format!("turn {i} {}", "d".repeat(250)),
            }));
        }
        let s = build_sections(&parts);
        assert_eq!(s.decisions.len(), 5, "count cap independent of length caps");
        let newest_len = format!("turn 5 {}", "d".repeat(250)).chars().count();
        assert_eq!(s.decisions[4].chars().count(), newest_len, "newest stays untruncated within LATEST_TRUNC");
        assert!(s.decisions[0].chars().count() <= EARLIER_TRUNC + 1, "older turns use the shorter budget");
    }

    #[test]
    fn failed_attempts_capped_to_most_recent_five() {
        let mut parts = Vec::new();
        for i in 0..7 {
            parts.push(Part {
                tool_call_id: Some(format!("call-{i}")),
                payload: PartPayload::ToolInvocation {
                    name: "Bash".into(),
                    input: None,
                    mcp: None,
                    child_session_id: None,
                },
                ..part(i as u32, PartPayload::UserMessage { text: String::new() })
            });
            parts.push(Part {
                tool_call_id: Some(format!("call-{i}")),
                payload: PartPayload::ToolResult {
                    output: format!("error {i}"),
                    is_error: Some(true),
                    mcp: None,
                },
                ..part(i as u32, PartPayload::UserMessage { text: String::new() })
            });
        }
        let s = build_sections(&parts);
        assert_eq!(s.failed_attempts.len(), MAX_FAILED);
        assert!(s.failed_attempts[0].contains("error 2"), "most recent five kept: {}", s.failed_attempts[0]);
    }

    #[test]
    fn build_sections_empty_on_blank_session() {
        assert_eq!(build_sections(&[]), HandoffSections::default());
        assert_eq!(render_markdown("t", &HandoffSections::default()), "# Handoff: t\n\n");
    }

    #[test]
    fn context_pressure_reports_estimate_and_top_consumer() {
        let mut parts = Vec::new();
        for i in 0..10 {
            parts.push(part(i, PartPayload::AssistantMessage {
                text: "x".repeat(400),
            }));
        }
        parts.push(part(10, PartPayload::ToolResult {
            output: "y".repeat(4_000),
            is_error: Some(false),
            mcp: None,
        }));
        let p = context_pressure(&parts, None);
        assert_eq!(p.est_tokens, (10 * 400 + 4_000) / 4);
        assert_eq!(p.top_consumer.as_deref(), Some("tool result"));
        assert!(p.estimated);
        assert_eq!(p.window_tokens, DEFAULT_CONTEXT_WINDOW);
        assert!(p.pct < 100);
        // A confirmed catalog window replaces the denominator only.
        let p2 = context_pressure(&parts, Some(20_000));
        assert_eq!(p2.window_tokens, 20_000);
        assert_eq!(p2.est_tokens, p.est_tokens);
    }

    /// Audit requirement: usage metadata alone must NOT flip the estimate to
    /// "real" — the cache-exclusivity contract is unverified, so the
    /// char-based path stays even when parts carry usage.
    #[test]
    fn context_pressure_keeps_char_estimate_when_usage_present() {
        let mut parts = vec![part(0, PartPayload::AssistantMessage {
            text: "z".repeat(400),
        })];
        parts[0].provider_metadata_json =
            r#"{"model":"m","usage":{"input_tokens":42,"output_tokens":7}}"#.into();
        let p = context_pressure(&parts, None);
        assert_eq!(p.est_tokens, 100, "char estimate, not the usage numbers");
        assert!(p.estimated);
        assert_eq!(last_model(&parts).as_deref(), Some("m"));
    }

    #[test]
    fn window_for_model_exact_then_unique_like_then_ambiguous() {
        let conn = Connection::open_in_memory().unwrap();
        crate::schema::build_v1(&conn).unwrap();
        conn.execute(
            "INSERT INTO provider_endpoint (id, kind, display_name, has_api_key, status, models_json)
             VALUES ('ep-1','custom','M',0,'unvalidated','{}')",
            [],
        )
        .unwrap();
        let seed = |model_id: &str, ctx: i64| {
            conn.execute(
                "INSERT INTO model_catalog (endpoint_id, model_id, abilities_json) VALUES ('ep-1',?1,?2)",
                rusqlite::params![
                    model_id,
                    serde_json::json!({"limit": {"context": ctx, "output": 4096}}).to_string()
                ],
            )
            .unwrap();
        };
        seed("claude-sonnet-4-5", 200_000);
        seed("openai/gpt-5.6-luna", 400_000);
        seed("openai/gpt-5.6-mini", 128_000);

        // Exact match wins.
        assert_eq!(window_for_model(&conn, "claude-sonnet-4-5"), Some(200_000));
        // LIKE fallback resolves to exactly ONE distinct model → adopted.
        assert_eq!(window_for_model(&conn, "gpt-5.6-luna"), Some(400_000));
        // LIKE matches TWO distinct models → ambiguous → default window kept.
        assert_eq!(window_for_model(&conn, "gpt-5.6"), None);
        // No hit at all.
        assert_eq!(window_for_model(&conn, "nope"), None);
    }


    #[test]
    fn usage_totals_read_provider_metadata() {
        let mut parts = fixture_parts();
        parts[0].provider_metadata_json =
            r#"{"usage":{"input_tokens":100,"output_tokens":50},"cost_usd":0.25}"#.into();
        parts[1].provider_metadata_json =
            r#"{"usage":{"prompt_tokens":10,"completion_tokens":5}}"#.into();
        let (tokens, cost) = usage_totals(&parts);
        assert_eq!(tokens, Some(165));
        assert_eq!(cost, Some(0.25));
        // No metadata at all → None/None (today's common case).
        let (t, c) = usage_totals(&[]);
        assert_eq!((t, c), (None, None));
    }

    #[test]
    fn truncate_chars_respects_multibyte() {
        let s = "密码密码密码";
        let cut = truncate_chars(s, 2);
        assert_eq!(cut.chars().count(), 2);
    }

    /// Full persistence round-trip. Serialized through the crate-wide
    /// `HOME_LOCK` because NESTRA_HOME_DIR is process-global (same pattern as
    /// the skills tests).
    #[test]
    fn save_list_inject_remove_knowledge_round_trip() {
        let _home_lock = crate::HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let home = tempfile::Builder::new().prefix("").tempdir().unwrap();
        // SAFETY: confined to this serialized test (HOME_LOCK held).
        std::env::set_var("NESTRA_HOME_DIR", home.path());
        let repo = tempfile::Builder::new().prefix("").tempdir().unwrap();

        let conn = Connection::open_in_memory().unwrap();
        crate::schema::build_v1(&conn).unwrap();
        conn.execute(
            "INSERT INTO session (provider, id, title, summary, started_at, updated_at,
                                  message_count, source_path, resume_command, cwd)
             VALUES ('pi-cli','s-1','Fix login','',0,0,2,'x','pi resume',?1)",
            rusqlite::params![repo.path().to_string_lossy()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_part (provider, session_id, seq, part_idx, kind, payload_json,
                                       tool_call_id, raw_json, provider_metadata_json)
             VALUES ('pi-cli','s-1',0,0,'user_message',?1,NULL,'','{}')",
            rusqlite::params![
                serde_json::to_string(&PartPayload::UserMessage { text: "goal text".into() })
                    .unwrap()
            ],
        )
        .unwrap();

        let parts = parts_for_session(&conn, "pi-cli", "s-1").unwrap();
        assert_eq!(parts.len(), 1, "the first session_part reader returns the seeded part");
        let md = render_markdown("Fix login", &build_sections(&parts));

        let info = save_handoff(&conn, "pi-cli", "s-1", &md).unwrap();
        assert!(info.artifact_path.contains(".nestra"),
            "artifact lands inside the session repo: {}",
            info.artifact_path
        );
        assert!(std::path::Path::new(&info.artifact_path).exists());
        assert_eq!(info.sections.goal.as_deref(), Some("goal text"));
        assert_eq!(info.token_snapshot, None, "no usage recorded → null snapshot");

        let listed = list_handoffs(&conn, "pi-cli", "s-1").unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, info.id);
        assert_eq!(listed[0].markdown.as_deref(), Some(md.as_str()));

        // Inject: .pi/handoff-<id>.md + one deduplicated reference line.
        let injected = inject_handoff(&conn, &info.id).unwrap();
        assert!(std::path::Path::new(&injected).exists());
        let append_path = repo.path().join(".pi").join("APPEND_SYSTEM.md");
        let append = std::fs::read_to_string(&append_path).unwrap();
        assert_eq!(append.matches("nestra-handoff").count(), 1);
        inject_handoff(&conn, &info.id).unwrap();
        let append = std::fs::read_to_string(&append_path).unwrap();
        assert_eq!(append.matches("nestra-handoff").count(), 1, "repeat inject is idempotent");

        // Remove: reference line + injected file gone, artifact untouched.
        remove_injected_handoff(&conn, &info.id).unwrap();
        assert!(!std::path::Path::new(&injected).exists());
        let append = std::fs::read_to_string(&append_path).unwrap();
        assert!(!append.contains(&info.id));
        assert!(std::path::Path::new(&info.artifact_path).exists());

        // Knowledge: frontmatter file under the (overridden) Nestra home.
        let k = handoff_to_knowledge(&conn, &info.id, "decision").unwrap();
        let k_dir = home.path().join(".nestra").join("knowledge");
        assert!(k.starts_with(k_dir.to_string_lossy().as_ref()), "{k}");
        let body = std::fs::read_to_string(&k).unwrap();
        assert!(body.starts_with("---\ntype: decision\n"));
        assert!(body.contains("# Handoff: Fix login"));

        // Delete: row gone, the user's artifact file stays.
        delete_handoff(&conn, &info.id).unwrap();
        assert!(list_handoffs(&conn, "pi-cli", "s-1").unwrap().is_empty());
        assert!(std::path::Path::new(&info.artifact_path).exists());
    }

    /// A session without cwd parks its artifacts under the Nestra home instead.
    #[test]
    fn save_without_cwd_uses_nestra_home() {
        let _home_lock = crate::HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let home = tempfile::Builder::new().prefix("").tempdir().unwrap();
        // SAFETY: confined to this serialized test (HOME_LOCK held).
        std::env::set_var("NESTRA_HOME_DIR", home.path());

        let conn = Connection::open_in_memory().unwrap();
        crate::schema::build_v1(&conn).unwrap();
        conn.execute(
            "INSERT INTO session (provider, id, title, summary, started_at, updated_at,
                                  message_count, source_path, resume_command)
             VALUES ('pi-cli','s-2','t','',0,0,0,'x','pi resume')",
            [],
        )
        .unwrap();
        let info = save_handoff(&conn, "pi-cli", "s-2", "# Handoff: t\n").unwrap();
        assert!(info.artifact_path.starts_with(
            home.path().join(".nestra").join("handoffs").to_string_lossy().as_ref()
        ));
    }
}
