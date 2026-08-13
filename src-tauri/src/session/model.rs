//! Universal session model — the provider-neutral data shape that everything
//! downstream (UI, search, export) consumes. Provider-specific readers in
//! `mod.rs` normalize their on-disk formats into these structs; `store.rs`
//! persists them. This is the project's core session data model, designed so
//! conversations are portable across coding agents regardless of source.

use serde::Serialize;
use std::path::PathBuf;

use super::semantic::SemanticEvent;

/// A normalized conversation. One `Session` aggregates one or more raw source
/// files that share a canonical identity (e.g. Claude `sessionId`, Pi header
/// `id`). Subagent/sidechain logs become their own
/// `Session` with `is_subagent = true` and `parent_session_id` pointing at the
/// conversation that spawned them — they are not flattened into the parent.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Session {
    /// Canonical conversation id, provider-native (Claude `sessionId`, etc.).
    pub id: String,
    /// Provider kind: `claude-code`, `pi`, `opencode-desktop`.
    pub provider: String,
    pub title: String,
    /// Short preview — typically the last assistant reply.
    pub summary: String,
    /// cwd basename, when a working directory is known.
    pub project: Option<String>,
    pub cwd: Option<String>,
    pub started_at: i64,
    pub updated_at: i64,
    pub ended_at: Option<i64>,
    /// Number of normalized messages (excludes non-message bookkeeping lines).
    pub message_count: u32,
    /// Primary backing source file (for "open folder" / reveal). When a
    /// session spans multiple files this is the most-recently-updated one.
    pub source_path: String,
    /// Set for subagents/sidechains: the parent conversation's canonical id.
    pub parent_session_id: Option<String>,
    /// The subagent's own id (e.g. Claude `agentId`), when applicable.
    pub agent_id: Option<String>,
    pub is_subagent: bool,
    /// Opaque shell command that resumes this session, provider-owned.
    pub resume_command: String,
    /// Number of subagent sessions attached to this one (top-level list hint).
    pub child_count: u32,
    /// Every raw source file that contributed to this session.
    pub source_files: Vec<String>,
    /// Opaque per-session metadata blob for provider-specific fields not
    /// promoted to first-class columns (model, usage, system prompt, tags,
    /// checkpoints, raw envelope). Always a JSON object string (`{}` when
    /// empty). UI may surface known keys; unknown keys are preserved losslessly.
    pub provider_metadata_json: String,
}

/// A single normalized message within a session. `seq` is the per-session
/// ordering assigned by the normalizer (stable across multi-file merges).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Message {
    pub seq: u32,
    /// `user` | `assistant` | `system` | `tool` | `provider_event`.
    pub role: String,
    /// Flattened human-readable text (concatenation of text content blocks).
    pub content_text: String,
    /// Tool name for `tool`-role messages.
    pub tool_name: Option<String>,
    /// Structured tool input (JSON string), when the source captured it.
    pub tool_input: Option<String>,
    /// Structured tool output (JSON string), when the source captured it.
    pub tool_output: Option<String>,
    /// Provider-native id linking a tool invocation to its result (Claude
    /// `tool_use.id`, etc.). Lets the UI pair a use to its
    /// result row even when an assistant turn emits multiple tool calls.
    pub tool_call_id: Option<String>,
    /// Reasoning / chain-of-thought content (Claude `thinking`, Pi `thinking`).
    /// Rendered separately from `content_text` so the UI can distinguish
    /// reasoning from the assistant's user-visible reply.
    pub thinking: Option<String>,
    /// Native message threading pointer (e.g. Claude `parentUuid`).
    pub parent_message_id: Option<String>,
    /// Native per-message id (e.g. Claude `uuid`), when present.
    pub message_id: Option<String>,
    pub timestamp: Option<i64>,
    /// Opaque per-message metadata blob for provider-specific fields not
    /// promoted to first-class columns (attachments, is_error, model, usage,
    /// MCP provenance, raw envelope). Always a JSON object string (`{}` when
    /// empty). UI may surface known keys; unknown keys are preserved losslessly.
    pub provider_metadata_json: String,
}

/// One raw source file fully parsed, before grouping/normalization. The
/// per-provider importers produce one of these per file; the assembler then
/// merges files that share `canonical_id` and splits sidechains out as
/// children.
#[derive(Debug, Clone)]
pub struct RawFile {
    pub path: PathBuf,
    /// Provider-native conversation identity extracted from file contents
    /// (falls back to the filename stem when none is present).
    pub canonical_id: String,
    /// True for Claude sidechain/subagent logs.
    pub is_sidechain: bool,
    /// Parent conversation id for sidechains.
    pub parent_session_id: Option<String>,
    /// Subagent id for sidechains.
    pub agent_id: Option<String>,
    pub title: String,
    pub summary: String,
    pub project: Option<String>,
    pub cwd: Option<String>,
    pub started_at: i64,
    pub updated_at: i64,
    pub ended_at: Option<i64>,
    /// Semantic events emitted by the importer for this file (seq is assigned
    /// by the assembler).
    pub events: Vec<SemanticEvent>,
    pub mtime: i64,
}

/// Windowed message read result returned to the UI.
#[derive(Debug, Clone, Serialize)]
pub struct MessageWindow {
    pub messages: Vec<Message>,
    pub total: u32,
}
