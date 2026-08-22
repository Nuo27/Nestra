//! Codex session importer — rollout JSONL files.
//!
//! Source: `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` (+ `archived_sessions/`),
//! thread titles from `~/.codex/session_index.jsonl`. The Desktop app and the
//! CLI share this storage (`CODEX_HOME`).
//!
//! Line shape (from the codex-rs `rollout` crate — no official format doc
//! exists): one JSON object per line, `{"timestamp", "type", "payload"}` with
//! `type = session_meta` on the first line (thread id, cwd, git, provider)
//! and `response_item` lines carrying the conversation (`message`,
//! `reasoning`, `function_call`, `function_call_output`). Unknown line/payload
//! types are skipped, not failed — the format is treated as experimental
//! until confirmed against real rollout samples.

use crate::error::AppResult;
use crate::session::semantic::{PartPayload, SemanticEvent};
use crate::session::{parse_iso, self_dir, SessionImporter};
use crate::session::{mtime_millis, RawFile};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;

pub struct CodexImporter;

/// Registry constructor — see [super::SPEC].
pub fn new() -> Box<dyn SessionImporter> {
    Box::new(CodexImporter)
}

impl SessionImporter for CodexImporter {
    fn snapshot(&self) -> AppResult<Vec<(String, i64)>> {
        let home = self_dir(".codex", &[])?;
        let mut out = Vec::new();
        for dir in ["sessions", "archived_sessions"] {
            for path in crate::session::jsonl_files_under(&home.join(dir)) {
                out.push((path.to_string_lossy().to_string(), mtime_millis(&path)));
            }
        }
        Ok(out)
    }

    fn import(&self) -> AppResult<Vec<RawFile>> {
        let home = self_dir(".codex", &[])?;
        let titles = thread_titles(&home);
        let mut out = Vec::new();
        for dir in ["sessions", "archived_sessions"] {
            for path in crate::session::jsonl_files_under(&home.join(dir)) {
                out.push(import_rollout(&path, &titles));
            }
        }
        Ok(out)
    }
}

/// `session_index.jsonl` → {thread id → title}. Best-effort; missing file is
/// normal (the Desktop app prunes it).
fn thread_titles(home: &std::path::Path) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Ok(text) = std::fs::read_to_string(home.join("session_index.jsonl")) else {
        return out;
    };
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let (Some(id), Some(name)) = (
            v.get("id").and_then(|x| x.as_str()),
            v.get("thread_name").and_then(|x| x.as_str()),
        ) {
            out.insert(id.to_string(), name.to_string());
        }
    }
    out
}

/// Parse one rollout file into a [`RawFile`]. Every line is best-effort: a
/// malformed line is skipped so one bad write never hides the rest of the
/// transcript.
fn import_rollout(path: &std::path::Path, titles: &HashMap<String, String>) -> RawFile {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut canonical_id = None;
    let mut cwd = None;
    let mut events: Vec<SemanticEvent> = Vec::new();

    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let ts = v.get("timestamp").and_then(|t| t.as_str()).and_then(parse_iso);
        let payload = v.get("payload").cloned().unwrap_or(Value::Null);
        match v.get("type").and_then(|t| t.as_str()) {
            Some("session_meta") => {
                let id = payload
                    .get("id")
                    .or_else(|| payload.get("session_id"))
                    .and_then(|x| x.as_str())
                    .map(str::to_string);
                if canonical_id.is_none() {
                    canonical_id = id;
                    cwd = payload.get("cwd").and_then(|c| c.as_str()).map(str::to_string);
                }
            }
            Some("response_item") => {
                if let Some(ev) = response_item_event(&payload, ts, line) {
                    events.push(ev);
                }
            }
            // turn_context / event_msg / compacted / future types: no
            // transcript content.
            _ => {}
        }
    }

    let id = canonical_id
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            path.file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default()
        });
    let title = titles
        .get(&id)
        .cloned()
        .unwrap_or_else(|| first_user_text(&events));
    let started_at = events.first().and_then(|e| e.ts).unwrap_or(0);
    let updated_at = events.last().and_then(|e| e.ts).unwrap_or(started_at);
    RawFile {
        path: PathBuf::from(path),
        canonical_id: id,
        is_sidechain: false,
        parent_session_id: None,
        agent_id: None,
        title,
        summary: String::new(),
        project: cwd
            .as_ref()
            .map(|c| {
                std::path::Path::new(c)
                    .file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_else(|| c.clone())
            }),
        cwd,
        started_at,
        updated_at,
        ended_at: None,
        mtime: mtime_millis(path),
        events,
    }
}

/// One `response_item` payload → one semantic event (`None` = not transcript
/// content).
fn response_item_event(payload: &Value, ts: Option<i64>, raw: &str) -> Option<SemanticEvent> {
    let kind = payload.get("type").and_then(|t| t.as_str())?;
    let mut ev = match kind {
        "message" => {
            let text = content_text(payload.get("content")?);
            if text.is_empty() {
                return None;
            }
            let payload = match payload.get("role").and_then(|r| r.as_str()) {
                Some("user") => PartPayload::UserMessage { text },
                _ => PartPayload::AssistantMessage { text },
            };
            SemanticEvent::new(payload)
        }
        "reasoning" => {
            let text = content_text(payload.get("summary").unwrap_or(&serde_json::Value::Null));
            if text.is_empty() {
                return None;
            }
            SemanticEvent::new(PartPayload::Thinking { text, signature: None })
        }
        "function_call" => {
            let name = payload.get("name").and_then(|n| n.as_str())?.to_string();
            SemanticEvent::new(PartPayload::ToolInvocation {
                name,
                input: payload.get("arguments").and_then(|a| a.as_str()).map(str::to_string),
                mcp: None,
                child_session_id: None,
            })
        }
        "function_call_output" => {
            let output = payload.get("output").and_then(|o| o.as_str())?.to_string();
            SemanticEvent::new(PartPayload::ToolResult {
                output,
                is_error: None,
                mcp: None,
            })
        }
        // local_shell_call, web_search_call, … — not mapped yet.
        _ => return None,
    };
    ev.ts = ts;
    ev.raw_json = raw.to_string();
    if let Some(call_id) = payload.get("call_id").and_then(|c| c.as_str()) {
        ev.tool_call_id = Some(call_id.to_string());
    }
    Some(ev)
}

/// Flatten a Responses content array (`[{type, text}]`) or bare string to
/// text.
fn content_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|i| i.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string(),
        _ => String::new(),
    }
}

fn first_user_text(events: &[SemanticEvent]) -> String {
    for ev in events {
        if let PartPayload::UserMessage { text } = &ev.payload {
            let t = text.trim();
            if !t.is_empty() {
                return t.chars().take(80).collect();
            }
        }
    }
    String::new()
}

#[cfg(test)]
mod tests;
