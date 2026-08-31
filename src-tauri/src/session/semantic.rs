//! Semantic session vocabulary — the provider-neutral abstraction layer.
//!
//! This module defines the *meaning* of a conversation, independent of any one
//! provider's on-disk format. It is the heart of the compatibility layer:
//!
//! - **Importers** (one per provider) read their native format and emit a stream
//!   of [`SemanticEvent`]s. An importer never produces a raw `Message` or
//!   `Part`; it only speaks this vocabulary.
//! - The **assembler** (`assemble` in `mod.rs`) is provider-agnostic. It groups
//!   events into conversations, pairs tool invocations with their results,
//!   links sub-agents to their parent, sequences everything, and emits the
//!   canonical [`Part`] list stored per session.
//! - The **store/UI** read [`Part`]s — on demand, parsed straight from the
//!   agents' own logs (the v3 index-only session store mirrors nothing). A
//!   [`Part`] projects losslessly to the flat [`Message`](super::Message) row
//!   so the existing IPC contract is unchanged; the typed payload is the
//!   source of truth.
//!
//! ## The losslessness invariant
//!
//! `extract_blocks` in the old pipeline had a `_ => {}` catch-all that silently
//! dropped MCP identity, attachments, images, file blocks, usage, model name,
//! and the parent→child Task-tool link. That cannot happen here:
//!
//! - Any content block an importer does not explicitly recognize becomes a
//!   [`PartPayload::Unknown`] carrying the verbatim JSON, never dropped.
//! - Every [`Part`] also carries `provider_metadata_json`: provider fields
//!   the typed payload doesn't model (token usage, model name) are PROMOTED
//!   there via [`provider_meta`] as normalized `usage` + `model` keys.
//!   (`raw_json` is kept in-memory only — the on-demand parse holds it for
//!   the lifetime of the read; nothing body-shaped is persisted.)
//!
//! Adding a new provider = implement one importer (emit [`SemanticEvent`]s) +
//! register it. No changes to the assembler, store, or UI.

use serde::{Deserialize, Serialize};

use super::Message;

// ---------------------------------------------------------------------------
// Supporting structured types
// ---------------------------------------------------------------------------

/// Provenance for a tool call/result that came through an MCP server, when the
/// provider records which server served the tool. `None` fields mean the
/// provider did not expose that detail; the call is still preserved.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct McpProvenance {
    /// MCP server name / id, when known (Claude records `server_name`).
    pub server: Option<String>,
    /// The MCP tool name as exposed by the server (may differ from the
    /// provider's flattened tool label).
    pub tool_name: Option<String>,
}

/// A non-text artifact attached to a message — image, file, or other media.
/// `data_ref` is whatever pointer the provider used (a path, a base64 data
/// URL, an id); we never inline large binary blobs into the session store.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Attachment {
    /// `image` | `file` | `resource` | provider-specific kind.
    pub kind: String,
    /// MIME type when the source carried one.
    pub mime: Option<String>,
    /// Path, data URL, or opaque id referencing the attachment payload.
    pub data_ref: String,
    /// Human-readable title/filename when present.
    pub title: Option<String>,
}

/// Stable string tags for each part kind — the shared vocabulary for
/// renderer dispatch and consumer-side filtering.
///
/// Keep these strings stable: part kinds are compared across the codebase.
pub mod kind {
    pub const USER_MESSAGE: &str = "user_message";
    pub const ASSISTANT_MESSAGE: &str = "assistant_message";
    pub const THINKING: &str = "thinking";
    pub const TOOL_INVOCATION: &str = "tool_invocation";
    pub const TOOL_RESULT: &str = "tool_result";
    pub const SUB_AGENT: &str = "sub_agent";
    pub const ATTACHMENT: &str = "attachment";
    pub const SYSTEM_EVENT: &str = "system_event";
    pub const UNKNOWN: &str = "unknown";
}

// ---------------------------------------------------------------------------
// PartPayload — the typed meaning of one atomic conversation unit
// ---------------------------------------------------------------------------

/// The typed meaning of a single conversation unit, independent of provider.
    ///
    /// Each variant maps to one of `UserMessage | AssistantMessage | Thinking |
    /// ToolInvocation | ToolResult | SubAgent | Attachment | SystemEvent |
    /// Unknown`. An importer maps its native blocks/records onto these variants;
    /// anything it cannot map becomes [`Unknown`](Self::Unknown) with the verbatim
    /// JSON.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum PartPayload {
    /// A user-authored text message.
    UserMessage { text: String },
    /// The assistant's user-visible text reply (distinct from its reasoning).
    AssistantMessage { text: String },
    /// Chain-of-thought / reasoning content. `signature` is the provider's
    /// reasoning signature (Anthropic `signature`) when present — needed for
    /// faithful same-provider re-export.
    Thinking {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// The model requested a tool be run. `input` is the structured input as a
    /// JSON string. `mcp` is set when the tool was served by an MCP server.
    /// `child_session_id` is filled by the assembler when this invocation
    /// spawned a sub-agent whose session is also indexed.
    ToolInvocation {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mcp: Option<McpProvenance>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        child_session_id: Option<String>,
    },
    /// The result of a [`ToolInvocation`], paired by `call_id`. `is_error`
    /// distinguishes error results so the UI can flag them.
    ToolResult {
        output: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mcp: Option<McpProvenance>,
    },
    /// A sub-agent was spawned. `agent_id` is the provider's id for the spawned
    /// agent (Claude `agentId`); `child_session_id` is filled by the assembler
    /// once the child's own session is known. This is what links a parent's
    /// `Task` tool call to the child conversation — previously lost.
    SubAgent {
        agent_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        child_session_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
    /// A non-text attachment on a message.
    Attachment(Attachment),
    /// Provider lifecycle / bookkeeping the UI may want to surface as a system
    /// line: model in use, token usage, system prompt, environment events.
    SystemEvent {
        kind: String,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    /// The importer could not classify the record. The verbatim JSON is kept so
    /// nothing is silently discarded and an exporter can round-trip it.
    Unknown { raw_json: String },
}

impl PartPayload {
    /// The stable kind tag (see [`kind`]) for this payload — used as the
    /// denormalized SQL column and for renderer dispatch.
    pub fn kind_tag(&self) -> &'static str {
        match self {
            Self::UserMessage { .. } => kind::USER_MESSAGE,
            Self::AssistantMessage { .. } => kind::ASSISTANT_MESSAGE,
            Self::Thinking { .. } => kind::THINKING,
            Self::ToolInvocation { .. } => kind::TOOL_INVOCATION,
            Self::ToolResult { .. } => kind::TOOL_RESULT,
            Self::SubAgent { .. } => kind::SUB_AGENT,
            Self::Attachment(_) => kind::ATTACHMENT,
            Self::SystemEvent { .. } => kind::SYSTEM_EVENT,
            Self::Unknown { .. } => kind::UNKNOWN,
        }
    }
}

// ---------------------------------------------------------------------------
// SemanticEvent — what an importer emits per native record/block
// ---------------------------------------------------------------------------

/// One interpreted unit emitted by a provider importer. A single native record
/// (e.g. a Claude assistant turn's content-block array) can yield several
/// events. The assembler later turns events into sequenced [`Part`]s.
///
/// `seq` is assigned by the assembler (0 here). `raw_json` is the verbatim
/// native record the event was derived from, for lossless round-tripping.
#[derive(Debug, Clone, PartialEq)]
pub struct SemanticEvent {
    pub payload: PartPayload,
    /// Set on `ToolInvocation` and `ToolResult` events so the assembler can
    /// pair them into a single `Part` (and the renderer can fold them into one
    /// card). Denormalized into the `tool_call_id` SQL column for indexing.
    pub tool_call_id: Option<String>,
    /// Native per-message id (Claude `uuid`), when present.
    pub message_id: Option<String>,
    /// Native threading pointer (Claude `parentUuid`), when present.
    pub parent_message_id: Option<String>,
    pub ts: Option<i64>,
    /// Verbatim native record (one JSONL line / one SQLite row) this event came
    /// from. Preserved on the resulting [`Part`] for lossless re-export.
    pub raw_json: String,
    /// Provider-specific extras not promoted to a typed field (e.g. `is_error`
    /// on tool results pre-pairing). Always a JSON
    /// object string; `{}` when empty.
    pub provider_metadata_json: String,
}

impl SemanticEvent {
    /// Build an event with empty bookkeeping fields. Importers call this then
    /// set `tool_call_id` / `message_id` / `ts` / `raw_json` as available.
    pub fn new(payload: PartPayload) -> Self {
        Self {
            payload,
            tool_call_id: None,
            message_id: None,
            parent_message_id: None,
            ts: None,
            raw_json: String::new(),
            provider_metadata_json: "{}".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Part — the canonical, stored, sequenced unit
// ---------------------------------------------------------------------------

/// One canonical conversation unit, post-assembly. This is what the on-demand
/// body reads produce (parsed straight from the agent's own logs) and what
/// the UI ultimately renders. It is an
/// assembler-sequenced [`SemanticEvent`] with pairing/subagent links resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct Part {
    pub seq: u32,
    pub payload: PartPayload,
    /// Set on `ToolInvocation` and `ToolResult` parts. Denormalized into the
    /// `tool_call_id` SQL column for indexing and UI pairing.
    pub tool_call_id: Option<String>,
    pub message_id: Option<String>,
    pub parent_message_id: Option<String>,
    pub ts: Option<i64>,
    pub raw_json: String,
    pub provider_metadata_json: String,
}

impl Part {
    /// Project this part onto the flat [`Message`] row, so the existing
    /// `session_read` IPC contract (`MessageWindow`) stays byte-compatible
    /// while the typed payload remains the source of truth.
    ///
    /// Rules:
    /// - `UserMessage`/`AssistantMessage` → `role` = user/assistant, text in
    ///   `content_text`.
    /// - `Thinking` → `role` = "thinking", text in `thinking`, empty body.
    /// - `ToolInvocation` → `role` = "tool", `tool_name`/`tool_input` set.
    /// - `ToolResult` → `role` = "tool", `tool_output` set; `is_error` carried
    ///   in `provider_metadata_json`.
    /// - `SubAgent`/`Attachment` → `role` = "provider_event", a
    ///   readable `content_text`, structured data in `provider_metadata_json`.
    /// - `SystemEvent` → `role` = "system".
    /// - `Unknown` → `role` = "provider_event", raw json in `content_text` +
    ///   `provider_metadata_json`, so nothing is hidden.
    pub fn to_message(&self) -> Message {
        #[allow(unused_assignments)]
        let mut role = "system".to_string();
        let mut content_text = String::new();
        let mut tool_name = None;
        let mut tool_input = None;
        let mut tool_output = None;
        let mut thinking = None;
        let mut metadata: serde_json::Value =
            if self.provider_metadata_json.is_empty()
                || self.provider_metadata_json == "{}"
            {
                serde_json::json!({})
            } else {
                // Legal-but-non-object JSON (array/string) would PANIC on the
                // `metadata["key"] = ...` IndexMut below — only keep objects.
                match serde_json::from_str::<serde_json::Value>(&self.provider_metadata_json) {
                    Ok(v @ serde_json::Value::Object(_)) => v,
                    _ => serde_json::json!({}),
                }
            };

        match &self.payload {
            PartPayload::UserMessage { text } => {
                role = "user".into();
                content_text = text.clone();
            }
            PartPayload::AssistantMessage { text } => {
                role = "assistant".into();
                content_text = text.clone();
            }
            PartPayload::Thinking { text, signature } => {
                role = "thinking".into();
                thinking = Some(text.clone());
                if let Some(sig) = signature {
                    metadata["signature"] = serde_json::json!(sig);
                }
            }
            PartPayload::ToolInvocation {
                name,
                input,
                mcp,
                child_session_id,
            } => {
                role = "tool".into();
                tool_name = Some(name.clone());
                tool_input = input.clone();
                content_text = input.clone().unwrap_or_default();
                if let Some(m) = mcp {
                    metadata["mcp"] = serde_json::to_value(m).unwrap_or(serde_json::Value::Null);
                }
                // Losslessness invariant: the child link must survive the
                // flat `session_message` projection (the typed `part` path
                // keeps it natively).
                if let Some(c) = child_session_id {
                    metadata["child_session_id"] = serde_json::json!(c);
                }
            }
            PartPayload::ToolResult {
                output, is_error, mcp,
            } => {
                role = "tool".into();
                tool_output = Some(output.clone());
                content_text = output.clone();
                if let Some(e) = is_error {
                    metadata["is_error"] = serde_json::json!(e);
                }
                if let Some(m) = mcp {
                    metadata["mcp"] = serde_json::to_value(m).unwrap_or(serde_json::Value::Null);
                }
            }
            PartPayload::SubAgent {
                agent_id,
                child_session_id,
                description,
            } => {
                role = "provider_event".into();
                content_text = description.clone().unwrap_or_else(|| format!("subagent {agent_id}"));
                metadata["subagent"] = serde_json::json!({
                    "agent_id": agent_id,
                    "child_session_id": child_session_id,
                });
            }
            PartPayload::Attachment(a) => {
                role = "provider_event".into();
                content_text = a.title.clone().unwrap_or_else(|| a.data_ref.clone());
                metadata["attachment"] = serde_json::to_value(a).unwrap_or(serde_json::Value::Null);
            }
            PartPayload::SystemEvent { kind, text, model } => {
                role = "system".into();
                content_text = text.clone();
                metadata["system_kind"] = serde_json::json!(kind);
                if let Some(m) = model {
                    metadata["model"] = serde_json::json!(m);
                }
            }
            PartPayload::Unknown { raw_json } => {
                role = "provider_event".into();
                content_text = raw_json.clone();
                metadata["unknown"] = serde_json::json!(true);
            }
        }

        Message {
            seq: self.seq,
            role,
            content_text,
            tool_name,
            tool_input,
            tool_output,
            tool_call_id: self.tool_call_id.clone(),
            thinking,
            parent_message_id: self.parent_message_id.clone(),
            message_id: self.message_id.clone(),
            timestamp: self.ts,
            provider_metadata_json: serde_json::to_string(&metadata).unwrap_or_else(|_| "{}".into()),
        }
    }
}

/// Normalize a raw provider record/message object into canonical part
/// metadata: `usage` (token fields under canonical names) + `model`. PURE
/// alias mapping — no provider semantics (exclusivity/subset rules belong to
/// the consumers, not here). Returns `"{}"` when the record carries neither.
///
/// Canonical aliases:
/// - `input_tokens | prompt_tokens | input` → `usage.input_tokens`
/// - `output_tokens | completion_tokens | output` → `usage.output_tokens`
/// - `cache_read_input_tokens | cached_tokens` → `usage.cache_read_input_tokens`
/// - `cache_creation_input_tokens` → `usage.cache_creation_input_tokens`
/// - `model | modelId` → `model`
pub fn provider_meta(v: &serde_json::Value) -> String {
    let pick = |obj: &serde_json::Map<String, serde_json::Value>,
                keys: &[&str]|
     -> Option<serde_json::Value> {
        keys.iter()
            .find_map(|k| obj.get(*k).filter(|x| x.is_number()))
            .cloned()
    };
    let mut out = serde_json::Map::new();
    if let Some(u) = v.get("usage").and_then(|u| u.as_object()) {
        let mut n = serde_json::Map::new();
        for (aliases, canonical) in [
            (&["input_tokens", "prompt_tokens", "input"][..], "input_tokens"),
            (&["output_tokens", "completion_tokens", "output"][..], "output_tokens"),
            (
                &["cache_read_input_tokens", "cached_tokens"][..],
                "cache_read_input_tokens",
            ),
            (
                &["cache_creation_input_tokens"][..],
                "cache_creation_input_tokens",
            ),
        ] {
            if let Some(x) = pick(u, aliases) {
                n.insert(canonical.to_string(), x);
            }
        }
        if !n.is_empty() {
            out.insert("usage".to_string(), serde_json::Value::Object(n));
        }
    }
    if let Some(m) = v
        .get("model")
        .or_else(|| v.get("modelId"))
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
    {
        out.insert("model".to_string(), serde_json::Value::String(m.to_string()));
    }
    serde_json::to_string(&serde_json::Value::Object(out)).unwrap_or_else(|_| "{}".into())
}

#[cfg(test)]
mod tests;
