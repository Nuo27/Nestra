pub mod handoff;
mod model;
pub(crate) mod partdb;
pub mod provider;
pub mod semantic;
pub mod store;

pub use model::{Message, MessageWindow, RawFile, Session};
pub use semantic::{Attachment, McpProvenance, Part, PartPayload, SemanticEvent};

use crate::error::{AppError, AppResult};
use serde_json::Value;
use std::io::BufRead;
use std::path::{Path, PathBuf};

/// All provider ids the session layer knows how to discover/import —
/// every registry agent that declares a session integration. The session
/// registry (`session::provider::default_provider_registry`) is
/// a strict subset: only providers with a resumable CLI appear there.
pub fn all_providers() -> Vec<&'static str> {
    crate::agents::agents()
        .iter()
        .filter(|a| a.session.is_some())
        .map(|a| a.id)
        .collect()
}

// ============================================================================
// Importer trait + registry
//
// One [`SessionImporter`] per provider owns its native format end to end:
// discovery (`snapshot`), parsing (`import`), canonical-id extraction, and
// sidechain detection. There is NO central `match provider` dispatch — each
// provider is self-contained. Adding a provider = one struct + one registry
// line. The shared, provider-agnostic [`assemble`] turns the emitted
// [`SemanticEvent`] stream into the canonical model.
// ============================================================================

/// A provider-native → universal importer. Owns discovery + parsing; emits
/// semantic events the assembler normalizes. One per provider.
pub trait SessionImporter: Send + Sync {
    /// On-disk files backing this provider, as `(path, mtime)`, for change
    /// detection. Returning an empty vec (missing dir, no db) is normal.
    fn snapshot(&self) -> AppResult<Vec<(String, i64)>>;
    /// Parse every source into pre-grouping [`RawFile`]s. Each `RawFile`
    /// carries semantic events, not flat messages — the assembler sequences
    /// and pairs them. Idempotent over a stable snapshot.
    fn import(&self) -> AppResult<Vec<RawFile>>;
}

/// Resolve the importer for a provider id. Derived from the `AGENTS`
/// agent registry via each spec's `importer` constructor hook — the
/// importers themselves live in `agents/<id>/sessions.rs`.
fn importer_for(provider_id: &str) -> Option<Box<dyn SessionImporter>> {
    crate::agents::agent_spec(provider_id).and_then(|a| a.importer.map(|f| f()))
}

// ============================================================================
// Shared parsing helpers
// ============================================================================

/// Parse an ISO-8601 / RFC-3339 timestamp to unix millis.
pub(crate) fn parse_iso(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.timestamp_millis())
}

pub(crate) fn mtime_millis(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// All `.jsonl` files under `dir`, recursively. Providers store sessions at
/// varying depths (claude: `<project>/<id>.jsonl`), so a flat `read_dir`
/// never finds them.
pub(crate) fn jsonl_files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        // `is_dir` follows symlinks — a link to an ancestor would recurse
        // forever (shared by the Claude & Pi importers). Inspect the entry
        // type directly and skip links entirely.
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            out.extend(jsonl_files_under(&p));
        } else if p.extension().map(|e| e == "jsonl").unwrap_or(false) {
            out.push(p);
        }
    }
    out
}

/// Append `s` to `out` with a newline separator (no-op for empty).
fn append_line(out: &mut String, s: &str) {
    if s.is_empty() {
        return;
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(s);
}

/// Pull a unix-millis timestamp from a JSON value's `timestamp` (ISO) or
/// `ts` (int) field.
fn line_ts(v: &Value) -> Option<i64> {
    v.get("timestamp")
        .and_then(|x| x.as_str())
        .and_then(parse_iso)
        .or_else(|| v.get("ts").and_then(|x| x.as_i64()))
}

/// `tool_result.content` may be a string or an array of `{type:"text",text}`
/// blocks (Claude). Flatten either to a string.
fn text_or_json(v: &Value) -> String {
    if let Some(s) = v.as_str() {
        return s.to_string();
    }
    if let Some(arr) = v.as_array() {
        let mut out = String::new();
        for item in arr {
            if let Some(t) = item.get("text").and_then(|x| x.as_str()) {
                append_line(&mut out, t);
            }
        }
        return out;
    }
    v.to_string()
}

/// Concatenate the `text` fields of a content-block array into one string.
fn blocks_to_text(arr: &[Value]) -> String {
    let mut out = String::new();
    for item in arr {
        if let Some(t) = item.get("text").and_then(|x| x.as_str()) {
            append_line(&mut out, t);
        }
    }
    out
}

/// Interpret a Claude/Pi content-block array as `(payload, call_id)` pairs:
/// each
/// recognized block becomes a typed [`PartPayload`]; unrecognized blocks become
/// `Unknown` with the verbatim JSON (never silently dropped). The `call_id` is
/// set on `tool_use`/`tool_result` payloads so the importer can stamp the
/// `SemanticEvent.tool_call_id` for pairing. `mcp` is derived from the tool
/// name (Claude prefixes MCP tools as `mcp__<server>__<tool>`).
fn interpret_content_blocks(arr: &[Value], role: &str) -> Vec<(PartPayload, Option<String>)> {
    let mut out = Vec::new();
    let mut text_buf = String::new();
    for item in arr {
        let ty = item.get("type").and_then(|x| x.as_str()).unwrap_or("");
        match ty {
            "text" => {
                if let Some(t) = item.get("text").and_then(|x| x.as_str()) {
                    append_line(&mut text_buf, t);
                }
            }
            "tool_use" => {
                flush_text_buf(&mut text_buf, role, &mut out);
                let name = item
                    .get("name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("tool")
                    .to_string();
                let input = item.get("input").map(|x| x.to_string());
                let mcp = parse_mcp_tool_name(&name);
                let call_id = item.get("id").and_then(|x| x.as_str()).map(String::from);
                // A `Task` tool invocation spawns a sub-agent; emit a SubAgent
                // event so the assembler can link parent→child session.
                if name.eq_ignore_ascii_case("Task") {
                    let agent_id = input
                        .as_deref()
                        .and_then(|s| serde_json::from_str::<Value>(s).ok())
                        .and_then(|v| {
                            v.get("subagent_type")
                                .or_else(|| v.get("agent_id"))
                                .and_then(|x| x.as_str())
                                .map(String::from)
                        });
                    out.push((
                        PartPayload::SubAgent {
                            agent_id: agent_id.unwrap_or_else(|| name.clone()),
                            child_session_id: None,
                            description: Some(name.clone()),
                        },
                        None,
                    ));
                }
                out.push((
                    PartPayload::ToolInvocation {
                        name,
                        input,
                        mcp,
                        child_session_id: None,
                    },
                    call_id,
                ));
            }
            "tool_result" => {
                flush_text_buf(&mut text_buf, role, &mut out);
                let payload = item
                    .get("content")
                    .map(text_or_json)
                    .unwrap_or_default();
                let is_error = item.get("is_error").and_then(|x| x.as_bool());
                let call_id = item
                    .get("tool_use_id")
                    .and_then(|x| x.as_str())
                    .map(String::from);
                out.push((
                    PartPayload::ToolResult {
                        output: payload,
                        is_error,
                        mcp: None,
                    },
                    call_id,
                ));
            }
            "thinking" => {
                flush_text_buf(&mut text_buf, role, &mut out);
                let content = item
                    .get("thinking")
                    .and_then(|x| x.as_str())
                    .or_else(|| item.get("text").and_then(|x| x.as_str()))
                    .unwrap_or("")
                    .to_string();
                if !content.is_empty() {
                    let signature = item
                        .get("signature")
                        .and_then(|x| x.as_str())
                        .map(String::from);
                    out.push((
                        PartPayload::Thinking {
                            text: content,
                            signature,
                        },
                        None,
                    ));
                }
            }
            "thought" | "thoughtSummary" => {
                flush_text_buf(&mut text_buf, role, &mut out);
                let content = item
                    .get("text")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                if !content.is_empty() {
                    out.push((
                        PartPayload::Thinking {
                            text: content,
                            signature: None,
                        },
                        None,
                    ));
                }
            }
            "image" => {
                flush_text_buf(&mut text_buf, role, &mut out);
                let data_ref = item
                    .get("data")
                    .and_then(|x| x.as_str())
                    .or_else(|| item.get("url").and_then(|x| x.as_str()))
                    .unwrap_or("")
                    .to_string();
                out.push((
                    PartPayload::Attachment(Attachment {
                        kind: "image".into(),
                        mime: item
                            .get("media_type")
                            .or_else(|| item.get("mime_type"))
                            .and_then(|x| x.as_str())
                            .map(String::from),
                        data_ref,
                        title: None,
                    }),
                    None,
                ));
            }
            "file" => {
                flush_text_buf(&mut text_buf, role, &mut out);
                let data_ref = item
                    .get("data")
                    .and_then(|x| x.as_str())
                    .or_else(|| item.get("url").and_then(|x| x.as_str()))
                    .unwrap_or("")
                    .to_string();
                out.push((
                    PartPayload::Attachment(Attachment {
                        kind: "file".into(),
                        mime: item
                            .get("media_type")
                            .or_else(|| item.get("mime_type"))
                            .and_then(|x| x.as_str())
                            .map(String::from),
                        data_ref,
                        title: item.get("name").and_then(|x| x.as_str()).map(String::from),
                    }),
                    None,
                ));
            }
            _ => {
                // Losslessness invariant: never drop. Preserve verbatim.
                flush_text_buf(&mut text_buf, role, &mut out);
                out.push((
                    PartPayload::Unknown {
                        raw_json: item.to_string(),
                    },
                    None,
                ));
            }
        }
    }
    flush_text_buf(&mut text_buf, role, &mut out);
    out
}

/// Flush accumulated text as a message payload before every non-text
/// block, so text keeps its source position relative to tool calls and
/// separated text blocks stay separate messages.
fn flush_text_buf(
    text_buf: &mut String,
    role: &str,
    out: &mut Vec<(PartPayload, Option<String>)>,
) {
    if text_buf.is_empty() {
        return;
    }
    let payload = if role == "user" {
        PartPayload::UserMessage { text: std::mem::take(text_buf) }
    } else {
        PartPayload::AssistantMessage { text: std::mem::take(text_buf) }
    };
    out.push((payload, None));
}

/// Claude records MCP-served tool calls as `mcp__<server>__<tool>`; split that
/// into provenance. Returns `None` for ordinary (non-MCP) tool names.
/// `pub(crate)`: also the attribution rule for the MCP usage aggregation —
/// one source of truth for the namespace.
pub(crate) fn parse_mcp_tool_name(name: &str) -> Option<McpProvenance> {
    let rest = name.strip_prefix("mcp__")?;
    let mut it = rest.splitn(2, "__");
    let server = it.next()?.to_string();
    let tool_name = it.next().map(String::from);
    Some(McpProvenance {
        server: Some(server),
        tool_name,
    })
}

// ============================================================================
// JSONL importer core (Claude, Pi)
//
// All share a JSONL-on-disk shape; only canonical-id extraction and a
// couple of envelope quirks differ. `parse_jsonl_events` does one forward pass
// producing semantic events; `canonical_id_and_events` resolves identity per
// provider.
// ============================================================================

/// Result of one JSONL pass: identity + scalar metadata + semantic events.
pub(crate) struct JsonlParse {
    canonical_id: String,
    is_sidechain: bool,
    parent_session_id: Option<String>,
    agent_id: Option<String>,
    cwd: Option<String>,
    started_at: Option<i64>,
    ended_at: Option<i64>,
    title: Option<String>,
    summary: Option<String>,
    events: Vec<SemanticEvent>,
}

/// One forward pass over a `.jsonl` file producing semantic events + identity.
/// `provider` is passed only for the resulting `RawFile.provider` tag; identity
/// resolution is shape-based (not a `match provider`).
pub(crate) fn parse_jsonl_events(path: &Path) -> AppResult<JsonlParse> {
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();

    let mut canonical_id = stem.clone();
    let mut is_sidechain = false;
    let mut parent_session_id: Option<String> = None;
    let mut agent_id: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut first_ts: Option<i64> = None;
    let mut last_ts: Option<i64> = None;
    let mut title: Option<String> = None;
    let mut ai_title: Option<String> = None;
    let mut summary: Option<String> = None;
    let mut events: Vec<SemanticEvent> = Vec::new();

    for line in reader.lines().flatten() {
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // --- Identity ---
        let line_sidechain = v.get("isSidechain").and_then(|x| x.as_bool()) == Some(true);
        if line_sidechain {
            is_sidechain = true;
        }
        let line_agent = v.get("agentId").and_then(|x| x.as_str()).map(String::from);
        let line_session = v
            .get("sessionId")
            .and_then(|x| x.as_str())
            .map(String::from)
            .or_else(|| v.get("session_id").and_then(|x| x.as_str()).map(String::from));

        if is_sidechain {
            if let Some(a) = line_agent {
                agent_id.get_or_insert(a);
            }
            if let Some(s) = line_session {
                parent_session_id = Some(s);
            }
            if canonical_id == stem {
                if let Some(a) = &agent_id {
                    canonical_id = a.clone();
                }
            }
        } else if let Some(s) = line_session {
            canonical_id = s;
        } else if v.get("type").and_then(|x| x.as_str()) == Some("session") {
            if let Some(id) = v.get("id").and_then(|x| x.as_str()) {
                canonical_id = id.to_string();
            }
        }

        if cwd.is_none() {
            if let Some(c) = v.get("cwd").and_then(|x| x.as_str()) {
                cwd = Some(c.to_string());
            }
        }
        // Claude's `ai-title` line is the model-generated conversation title —
        // prefer it over the first user message (which is often a context-
        // continuation summary, not the real prompt). Collected into its own
        // slot: the title line usually arrives at the END of the file, long
        // after the first user message already claimed `title` — a
        // first-wins guard here would make aiTitle dead code.
        if let Some(t) = v
            .get("aiTitle")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
        {
            ai_title = Some(t.to_string());
        }
        let ts = line_ts(&v);
        if let Some(t) = ts {
            if first_ts.is_none() {
                first_ts = Some(t);
            }
            last_ts = Some(t);
        }

        // --- Message body → events ---
        let message_id = v.get("uuid").and_then(|x| x.as_str()).map(String::from);
        let parent_message_id = v
            .get("parentUuid")
            .filter(|x| !x.is_null())
            .and_then(|x| x.as_str())
            .map(String::from);

        let raw = line.clone();
        // Usage/model metadata for THIS line (Claude carries both inside the
        // `message` envelope; flat envelopes may carry them top-level).
        // Attached to the line's FIRST event only — one line is one billing
        // record, so per-block attachment would multiply token counts.
        let meta = semantic::provider_meta(
            v.get("message")
                .filter(|m| m.is_object())
                .unwrap_or(&v),
        );
        let mut meta_attached = false;
        let mut emit = |payload: PartPayload,
                        call_id: Option<String>,
                        title: &mut Option<String>,
                        summary: &mut Option<String>| {
            // Capture title (first user) / summary (last assistant) as a side
            // effect of the walk.
            match &payload {
                PartPayload::UserMessage { text } if title.is_none() => {
                    *title = Some(text.chars().take(80).collect());
                }
                PartPayload::AssistantMessage { text } => {
                    *summary = Some(text.chars().take(160).collect());
                }
                _ => {}
            }
            let provider_metadata_json = if meta_attached {
                "{}".into()
            } else {
                meta_attached = true;
                meta.clone()
            };
            events.push(SemanticEvent {
                tool_call_id: call_id,
                message_id: message_id.clone(),
                parent_message_id: parent_message_id.clone(),
                ts,
                raw_json: raw.clone(),
                provider_metadata_json,
                payload,
            });
        };

        if let Some(role) = role_from_message(&v) {
            let content = &v["message"]["content"];
            if content.is_null() {
                continue;
            }
            match content {
                Value::String(s) => {
                    let payload = if role == "user" {
                        PartPayload::UserMessage { text: s.clone() }
                    } else {
                        PartPayload::AssistantMessage { text: s.clone() }
                    };
                    emit(payload, None, &mut title, &mut summary);
                }
                Value::Array(arr) => {
                    // interpret_content_blocks returns (payload, call_id) per
                    // block; the call_id is already pulled from the block's
                    // `id` (tool_use) / `tool_use_id` (tool_result).
                    for (p, call_id) in interpret_content_blocks(arr, role) {
                        emit(p, call_id, &mut title, &mut summary);
                    }
                }
                _ => {}
            }
        } else {
            // Flat envelope: top-level type/kind + content.
            let ty = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
            let kind_field = v.get("kind").and_then(|x| x.as_str());
            let role = match (ty, kind_field) {
                ("user", _) | ("human", _) | (_, Some("Prompt")) => Some("user"),
                ("assistant", _) | ("model", _) | (_, Some("AssistantMessage")) => Some("assistant"),
                _ => None,
            };
            if let Some(role) = role {
                let content = v
                    .get("content")
                    .or_else(|| v.get("data").and_then(|d| d.get("content")))
                    .or_else(|| v.get("data").and_then(|d| d.get("text")))
                    .unwrap_or(&Value::Null);
                let text = match content {
                    Value::String(s) => s.clone(),
                    Value::Array(arr) => {
                        let pairs = interpret_content_blocks(arr, role);
                        let mut had_payloads = false;
                        for (p, call_id) in pairs {
                            had_payloads = true;
                            emit(p, call_id, &mut title, &mut summary);
                        }
                        if had_payloads {
                            continue;
                        }
                        blocks_to_text(arr)
                    }
                    _ => String::new(),
                };
                if !text.is_empty() {
                    let payload = if role == "user" {
                        PartPayload::UserMessage { text }
                    } else {
                        PartPayload::AssistantMessage { text }
                    };
                    emit(payload, None, &mut title, &mut summary);
                }
            } else {
                // Top-level tool line (flat).
                let tool_ty = match ty {
                    "tool_use" | "tool_result" | "ToolResults" | "tool" => Some(ty),
                    _ => None,
                };
                if let Some(tty) = tool_ty {
                    let name = v
                        .get("name")
                        .and_then(|x| x.as_str())
                        .or_else(|| v.get("tool_name").and_then(|x| x.as_str()))
                        .unwrap_or("tool")
                        .to_string();
                    let is_input = tty == "tool_use" || tty == "tool";
                    let payload = v
                        .get("input")
                        .or_else(|| v.get("output"))
                        .map(|x| x.to_string());
                    let call_id = v
                        .get("id")
                        .or_else(|| v.get("call_id"))
                        .or_else(|| v.get("tool_use_id"))
                        .and_then(|x| x.as_str())
                        .map(String::from);
                    let p = if is_input {
                        PartPayload::ToolInvocation {
                            name,
                            input: payload,
                            mcp: None,
                            child_session_id: None,
                        }
                    } else {
                        PartPayload::ToolResult {
                            output: payload.unwrap_or_default(),
                            is_error: None,
                            mcp: None,
                        }
                    };
                    emit(p, call_id, &mut title, &mut summary);
                } else if ty == "system" || ty == "system_event" {
                    let text = v
                        .get("content")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    if !text.is_empty() {
                        emit(
                            PartPayload::SystemEvent {
                                kind: ty.into(),
                                text,
                                model: None,
                            },
                            None,
                            &mut title,
                            &mut summary,
                        );
                    }
                } else if !line.trim().is_empty() && v.is_object() {
                    // Claude emits many session-lifecycle bookkeeping lines
                    // (ai-title, mode, permission-mode, file-history-snapshot,
                    // task_reminder, last-prompt, hook_*, queue-operation,
                    // compact_file_reference, plan_mode*, …). These are NOT
                    // conversation turns — they're per-session metadata that
                    // repeats across the file (e.g. `mode` ×34). Emitting them
                    // as Unknown parts would flood the message stream. They
                    // carry no message content, so we skip them here. The
                    // verbatim records remain on disk for an exporter; nothing
                    // conversational is lost.
                    if !is_claude_metadata_type(ty) {
                        emit(
                            PartPayload::Unknown { raw_json: raw.clone() },
                            None,
                            &mut title,
                            &mut summary,
                        );
                    }
                }
            }
        }
    }

    Ok(JsonlParse {
        canonical_id,
        is_sidechain,
        parent_session_id,
        agent_id,
        cwd,
        started_at: first_ts,
        ended_at: last_ts,
        // The model-generated title (when present) beats the first-user-
        // message fallback.
        title: ai_title.or(title),
        summary,
        events,
    })
}

/// Read `message.role` and accept user/human/assistant/model.
fn role_from_message(v: &Value) -> Option<&'static str> {
    let role = v.get("message")?.get("role").and_then(|x| x.as_str())?;
    match role {
        "user" | "human" => Some("user"),
        "assistant" | "model" => Some("assistant"),
        _ => None,
    }
}

/// Claude Code session-lifecycle bookkeeping types. These are per-session
/// metadata lines (not conversation turns) that repeat across the file. The
/// importer skips them so they don't flood the message stream; their verbatim
/// records remain on disk for faithful re-export. Keep this list in sync with
/// Claude's on-disk format as observed in real logs.
fn is_claude_metadata_type(ty: &str) -> bool {
    matches!(
        ty,
        "ai-title"
            | "agent-name"
            | "mode"
            | "permission-mode"
            | "file-history-snapshot"
            | "file-history-delta"
            | "file-history-reconstruction"
            | "last-prompt"
            | "task_reminder"
            | "hook_additional_context"
            | "hook_success"
            | "hook_block"
            | "queue-operation"
            | "compact_file_reference"
            | "plan_mode"
            | "plan_mode_exit"
            | "plan_file_reference"
            | "summary"
            | "mcp_instructions_delta"
            | "agent_listing_delta"
            | "skill_listing"
            | "create"
            | "update"
            // Pi's JSONL opens with a `type:"session"` header line (id,
            // timestamp, cwd) — session identity, not a conversation turn.
            // It's consumed for canonical_id/cwd above and must not ALSO
            // surface as an Unknown part in every Pi session.
            | "session"
            // Context-injection line types: Claude attaches these as a
            // top-level `attachment` field (skill listings, file contents,
            // banner text, …) — they are NOT conversation turns. Skipping
            // keeps them out of the message stream; the verbatim records
            // remain on disk for export.
            | "attachment"
            | "file"
    )
}

/// Build a `RawFile` from a parsed JSONL result.
pub(crate) fn rawfile_from_jsonl(path: &Path, p: JsonlParse) -> RawFile {
    let project = p
        .cwd
        .as_deref()
        .and_then(|c| Path::new(c).file_name())
        .and_then(|n| n.to_str())
        .map(String::from);
    let started = p.started_at;
    let updated = p.ended_at.unwrap_or_else(|| mtime_millis(path));
    RawFile {
        path: path.to_path_buf(),
        canonical_id: p.canonical_id,
        is_sidechain: p.is_sidechain,
        parent_session_id: p.parent_session_id,
        agent_id: p.agent_id,
        title: p.title.unwrap_or_default(),
        summary: p.summary.unwrap_or_default(),
        project,
        cwd: p.cwd,
        started_at: started.unwrap_or(updated),
        updated_at: updated,
        ended_at: p.ended_at,
        events: p.events,
        mtime: mtime_millis(path),
    }
}

// --- OpenCode (Desktop) --------------------------------------------------
// SQLite at the OpenCode data dir (probed in `opencode_db_path`). The Desktop
// importer (session::desktop) also scans JSONL session dirs and calls
// `collect_opencode_raw` below for the SQLite path, so both layouts surface
// under the single `opencode-desktop` provider.

pub(crate) fn opencode_db_path() -> PathBuf {
    if let Some(xdg) = dirs::data_local_dir()
        .map(|d| d.join("opencode").join("opencode.db"))
        .filter(|p| p.is_file())
    {
        return xdg;
    }
    // OpenCode resolves its own data dir XDG-style (`~/.local/share/opencode`)
    // even on Windows, where `data_local_dir()` above points at
    // `%LOCALAPPDATA%` — probe that location too.
    if let Some(home_share) = crate::db::home_dir()
        .ok()
        .map(|h| h.join(".local").join("share").join("opencode").join("opencode.db"))
        .filter(|p| p.is_file())
    {
        return home_share;
    }
    // Legacy fallback: `~/.opencode/opencode.db` — the FILE, not the
    // directory. Callers `is_file()` this path; returning the dir made the
    // fallback permanently unusable (and the snapshot counted the dir).
    crate::db::home_dir()
        .map(|h| h.join(".opencode").join("opencode.db"))
        .unwrap_or_else(|_| PathBuf::from(".opencode/opencode.db"))
}

/// session row → one `RawFile` (path is the db file itself). Current OpenCode
/// stores sessions in the same session/message/part + JSON-`data` layout as
/// ZCode (verified against a real `opencode.db`), so this delegates to the
/// shared [`partdb`] pipeline.
pub(crate) fn collect_opencode_raw() -> AppResult<Vec<RawFile>> {
    let db = opencode_db_path();
    if !db.is_file() {
        return Ok(vec![]);
    }
    partdb::collect(&db)
}

// --- shared dir helpers ---------------------------------------------------

pub(crate) fn self_dir(dot: &str, rest: &[&str]) -> AppResult<PathBuf> {
    let home = crate::db::home_dir()?;
    let mut p = home.join(dot);
    for r in rest {
        p = p.join(r);
    }
    Ok(p)
}

pub(crate) fn jsonl_snapshot(dir: PathBuf) -> AppResult<Vec<(String, i64)>> {
    if !dir.exists() {
        return Ok(vec![]);
    }
    Ok(jsonl_files_under(&dir)
        .into_iter()
        .map(|p| (p.to_string_lossy().to_string(), mtime_millis(&p)))
        .collect())
}

pub(crate) fn import_jsonl_dir(dir: PathBuf) -> AppResult<Vec<RawFile>> {
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut out = Vec::new();
    for f in jsonl_files_under(&dir) {
        if let Ok(p) = parse_jsonl_events(&f) {
            out.push(rawfile_from_jsonl(&f, p));
        }
    }
    Ok(out)
}

// ============================================================================
// Public parse entrypoints (used by store.rs)
// ============================================================================

/// Discover every raw source file for `provider` as `(path, mtime)` — the
/// change-detection snapshot used by the reconcile pass.
pub fn provider_snapshot(provider: &str) -> AppResult<Vec<(String, i64)>> {
    let importer = importer_for(provider)
        .ok_or_else(|| AppError::NotFound(format!("unknown provider '{provider}'")))?;
    importer.snapshot()
}

/// Parse every raw source for `provider` into pre-grouping `RawFile`s.
pub fn collect_raw_files(provider: &str) -> AppResult<Vec<RawFile>> {
    let importer = importer_for(provider)
        .ok_or_else(|| AppError::NotFound(format!("unknown provider '{provider}'")))?;
    importer.import()
}

/// Parse ONE indexed session's parts straight from its source files. This is
/// the body read behind the index-only session store: `session` rows carry
/// location + rollups, transcripts are never mirrored into SQLite. File
/// shape picks the parser — a `.jsonl` source parses line-wise (Claude / Pi /
/// Codex / OpenCode's JSONL layout all store one file per session), anything
/// else is a part-style SQLite db keyed by session id (zcode /
/// `opencode.db`). `agent_to_child` links Task-tool spawns to child sessions
/// (built from index rows — see `store::agent_to_child_map`).
pub fn read_session_parts(
    session: &Session,
    agent_to_child: &std::collections::BTreeMap<String, String>,
) -> AppResult<Vec<Part>> {
    let mut files: Vec<RawFile> = Vec::new();
    for path in &session.source_files {
        let p = Path::new(path);
        // A deleted/moved source degrades the session to an empty body — the
        // index row survives until the next reconcile drops it.
        if !p.is_file() {
            continue;
        }
        if p.extension().map(|e| e == "jsonl").unwrap_or(false) {
            if let Ok(parsed) = parse_jsonl_events(p) {
                files.push(rawfile_from_jsonl(p, parsed));
            }
        } else if let Some(rf) = partdb::collect_one(p, &session.id)? {
            files.push(rf);
        }
    }
    // Only this conversation's files (source_files came out of the same
    // grouping, so this is belt-and-braces against a shared/moved file).
    files.retain(|f| f.canonical_id == session.id);
    files.sort_by_key(|f| (f.started_at, f.path.clone()));
    Ok(assemble_parts(&files, agent_to_child))
}

// ============================================================================
// Shared, provider-agnostic assembler
//
// One parse per reconcile. Groups RawFiles by canonical id, splits sidechains,
// pairs
// ToolInvocation↔ToolResult by call_id, links SubAgent events to child
// sessions (filling child_session_id on the parent ToolInvocation), derives
// title/summary/cwd/timestamps, and re-sequences.
// ============================================================================

/// One assembled conversation: the session header plus its sequenced parts.
pub struct AssembledSession {
    pub session: Session,
    pub parts: Vec<Part>,
}

/// Resume command for one session id. Inline so the persisted
/// `resume_command` field and `build_resume_command` stay consistent — these
/// must match the templates in `provider.rs::default_provider_registry`.
/// (Pi/OpenCode use `--session`, not `--resume`/`--resume-id`.)
/// Single source: the `SessionRef.resume_command` template in the agent
/// registry.
fn resume_for(provider: &str, id: &str) -> String {
    resume_command_for(provider)
        .map(|t| t.replace("{id}", id))
        .unwrap_or_default()
}

/// Assemble `raws` into canonical `(Session, Vec<Part>)` pairs. This is the
/// only place grouping/pairing happens; importers never touch it.
pub fn assemble(provider: &str, raws: Vec<RawFile>) -> Vec<AssembledSession> {
    use std::collections::BTreeMap;

    let mut groups: BTreeMap<String, Vec<RawFile>> = BTreeMap::new();
    let mut sidechains: BTreeMap<String, Vec<RawFile>> = BTreeMap::new();
    for rf in raws {
        if rf.is_sidechain {
            sidechains.entry(rf.canonical_id.clone()).or_default().push(rf);
        } else {
            groups.entry(rf.canonical_id.clone()).or_default().push(rf);
        }
    }

    // child_count: sidechains grouped by their parent_session_id.
    let mut child_counts: BTreeMap<String, u32> = BTreeMap::new();
    for rf in sidechains.values().flatten() {
        if let Some(parent) = &rf.parent_session_id {
            *child_counts.entry(parent.clone()).or_default() += 1;
        }
    }

    // Map agent_id → child canonical id, so parent ToolInvocation/SubAgent parts
    // can be linked. (For Claude they are the same value.)
    let agent_to_child: BTreeMap<String, String> = sidechains
        .keys()
        .map(|k| (k.clone(), k.clone()))
        .collect();

    let resume_of = |id: &str| resume_for(provider, id);

    let mut out: Vec<AssembledSession> = Vec::new();

    // Top-level conversations.
    for (id, files) in groups {
        out.push(assemble_group(provider, &id, files, false, &agent_to_child, &child_counts, &resume_of));
    }

    // Sidechain (subagent) conversations.
    for (id, files) in sidechains {
        out.push(assemble_group(provider, &id, files, true, &agent_to_child, &BTreeMap::new(), &resume_of));
    }

    // newest first
    out.sort_by_key(|a| std::cmp::Reverse(a.session.updated_at));
    out
}

/// One grouped conversation → its `AssembledSession`. The single-group core
/// the full [`assemble`] and the incremental [`assemble_session`] share: sort
/// the group's files, derive the header (sidechain conversations resume via
/// their PARENT — resuming the subagent directly isn't meaningful; the user
/// resumes the parent and the agent is re-spawned), sequence + link the parts.
fn assemble_group(
    provider: &str,
    id: &str,
    mut files: Vec<RawFile>,
    is_sidechain: bool,
    agent_to_child: &std::collections::BTreeMap<String, String>,
    child_counts: &std::collections::BTreeMap<String, u32>,
    resume_of: &dyn Fn(&str) -> String,
) -> AssembledSession {
    files.sort_by_key(|f| (f.started_at, f.path.clone()));
    let parent = files.iter().rev().find_map(|f| f.parent_session_id.clone());
    let agent_id = files.iter().rev().find_map(|f| f.agent_id.clone());
    let parts = assemble_parts(&files, agent_to_child);
    let header = if is_sidechain {
        let parent_for_resume = parent.clone();
        let sidechain_resume =
            move |_ignored: &str| parent_for_resume.as_deref().map(resume_of).unwrap_or_default();
        derive_header(
            provider,
            id,
            &files,
            true,
            parent,
            agent_id,
            &std::collections::BTreeMap::new(),
            &sidechain_resume,
        )
    } else {
        derive_header(provider, id, &files, false, None, None, child_counts, resume_of)
    };
    AssembledSession {
        session: header,
        parts,
    }
}

/// Re-assemble ONE session from its (already-parsed) files — the per-session
/// core of the incremental reconcile. `agent_to_child` links Task spawns to
/// child sessions (existing index rows + any NEW sidechain files among
/// `files`). `None` when there is nothing to assemble — the caller treats
/// that as "the session's source material is gone" and drops the index row.
/// `child_count` starts at 0; the store's `recompute_child_counts` fixes it
/// in the same transaction.
pub fn assemble_session(
    provider: &str,
    id: &str,
    files: Vec<RawFile>,
    agent_to_child: &std::collections::BTreeMap<String, String>,
) -> Option<AssembledSession> {
    if files.is_empty() {
        return None;
    }
    let is_sidechain = files.iter().any(|f| f.is_sidechain);
    let resume_of = |sid: &str| resume_for(provider, sid);
    Some(assemble_group(provider, id, files, is_sidechain, agent_to_child, &std::collections::BTreeMap::new(), &resume_of))
}

/// Merge events from `files` into sequenced, paired [`Part`]s.
fn assemble_parts(files: &[RawFile], agent_to_child: &std::collections::BTreeMap<String, String>) -> Vec<Part> {
    let mut parts: Vec<Part> = Vec::new();
    for f in files {
        for ev in &f.events {
            parts.push(Part {
                seq: 0,
                payload: ev.payload.clone(),
                tool_call_id: ev.tool_call_id.clone(),
                message_id: ev.message_id.clone(),
                parent_message_id: ev.parent_message_id.clone(),
                ts: ev.ts,
                raw_json: ev.raw_json.clone(),
                provider_metadata_json: ev.provider_metadata_json.clone(),
            });
        }
    }
    // Stable order: (timestamp, message_id) — mirrors the existing behavior.
    parts.sort_by_key(|p| (p.ts.unwrap_or(0), p.message_id.clone()));

    // Link SubAgent parts and parent Task ToolInvocations to child sessions.
    let mut updated: Vec<Part> = parts
        .into_iter()
        .map(|mut p| {
            match &mut p.payload {
                PartPayload::SubAgent { agent_id, child_session_id, .. } => {
                    if child_session_id.is_none() {
                        if let Some(child) = agent_to_child.get(agent_id) {
                            *child_session_id = Some(child.clone());
                        }
                    }
                }
                PartPayload::ToolInvocation { name, input, child_session_id, .. }
                    if name.eq_ignore_ascii_case("Task") =>
                {
                    if child_session_id.is_none() {
                        // Resolve the REAL child from the tool input's
                        // subagent_type/agent_id — linking to the first
                        // arbitrary child mis-associates when a parent spawns
                        // several subagents.
                        let want = input
                            .as_deref()
                            .and_then(|s| serde_json::from_str::<Value>(s).ok())
                            .and_then(|v| {
                                v.get("subagent_type")
                                    .or_else(|| v.get("agent_id"))
                                    .and_then(|x| x.as_str())
                                    .map(String::from)
                            });
                        match want {
                            Some(a) => {
                                if let Some(child) = agent_to_child.get(&a) {
                                    *child_session_id = Some(child.clone());
                                }
                            }
                            None => {
                                // No role hint: fall back to the first child
                                // (single-subagent sessions are the norm).
                                if let Some((_, child)) = agent_to_child.iter().next() {
                                    *child_session_id = Some(child.clone());
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
            p
        })
        .collect();

    for (i, p) in updated.iter_mut().enumerate() {
        p.seq = i as u32;
    }
    updated
}

/// Derive the `Session` header scalars from a group of files (title, summary,
/// cwd, timestamps, source_path, child_count, resume_command).
fn derive_header(
    provider: &str,
    id: &str,
    files: &[RawFile],
    is_subagent: bool,
    parent_session_id: Option<String>,
    agent_id: Option<String>,
    child_counts: &std::collections::BTreeMap<String, u32>,
    resume_of: &dyn Fn(&str) -> String,
) -> Session {
    let mut title = String::new();
    let mut summary = String::new();
    let mut project: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut started_at = i64::MAX;
    let mut updated_at = i64::MIN;
    let mut ended_at: Option<i64> = None;
    let mut source_files: Vec<String> = Vec::new();
    let mut latest_path: Option<PathBuf> = None;
    let mut latest_mtime: i64 = i64::MIN;

    for f in files {
        if title.is_empty() {
            title = f.title.clone();
        }
        summary = f.summary.clone();
        if project.is_none() {
            project = f.project.clone();
        }
        if cwd.is_none() {
            cwd = f.cwd.clone();
        }
        started_at = started_at.min(f.started_at);
        updated_at = updated_at.max(f.updated_at);
        ended_at = match (ended_at, f.ended_at) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        source_files.push(f.path.to_string_lossy().to_string());
        if f.mtime >= latest_mtime {
            latest_mtime = f.mtime;
            latest_path = Some(f.path.clone());
        }
    }
    let started_at = if started_at == i64::MAX { updated_at } else { started_at };
    // message_count from the parts is computed by the caller's store path; for
    // the header we set it to the file-level event count as an approximation
    // (the store uses the actual persisted part count).
    let approx_count: u32 = files.iter().map(|f| f.events.len() as u32).sum();
    Session {
        id: id.to_string(),
        provider: provider.to_string(),
        title: if title.is_empty() {
            if is_subagent { "(subagent)".into() } else { "(empty session)".into() }
        } else {
            title
        },
        summary,
        project,
        cwd,
        started_at,
        updated_at,
        ended_at,
        message_count: approx_count,
        source_path: latest_path
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default(),
        parent_session_id,
        agent_id,
        is_subagent,
        resume_command: resume_of(id),
        child_count: *child_counts.get(id).unwrap_or(&0),
        source_files,
        provider_metadata_json: "{}".into(),
    }
}

/// Resume command template for a provider. Inline so the persisted
/// `resume_command` field and `build_resume_command` stay consistent — these
/// must match the templates in `provider.rs::default_provider_registry`.
/// (Pi/OpenCode use `--session`, not `--resume`/`--resume-id`.)
/// Single source: the `SessionRef.resume_command` template in the agent
/// registry.
fn resume_command_for(provider: &str) -> Option<&'static str> {
    crate::agents::agent_spec(provider)
        .and_then(|a| a.session)
        .and_then(|s| s.resume_command)
}

// ============================================================================
// Legacy-compatible normalization entrypoints
//
// The store still consumes `(Session, Vec<Message>)`. We assemble parts then
// project each part to a flat Message via `Part::to_message`. One parse per
// reconcile (no double-parse).
// ============================================================================

/// Assemble-only entrypoint returning parts (used by the store + tests).
pub fn normalize_with_parts(provider: &str) -> AppResult<Vec<AssembledSession>> {
    let raws = collect_raw_files(provider)?;
    Ok(assemble(provider, raws))
}

#[cfg(test)]
mod tests;
