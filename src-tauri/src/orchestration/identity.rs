//! Canonical routing/work identity for the Nestra orchestration layer.
//!
//! ## Identity hierarchy (correction #1)
//!
//! ```text
//! Agent          — claude-code-cli | opencode-desktop | pi-cli  (stable registry id)
//!  └ LogicalSession — agent-native session id (Claude sessionId, Pi header id, …)
//!     └ Task — one Nestra-owned unit of routing/work
//!        └ Request — one HTTP request; may retry/migrate without changing Task
//! ```
//!
//! - `Task` is a **Nestra orchestration concept**, not an agent-native one
//!   (clarification #2). `task_id` is Nestra's own routing/work identity; it is
//!   NOT required to be 1:1 with any agent's notion of "task". An agent-native
//!   task handle, when the agent exposes one, is carried optionally in
//!   [`NativeTaskRef`] and is never load-bearing for routing.
//! - A `Request` is one HTTP call. Retries and migrations within a single Task
//!   issue new `request_id`s without changing `task_id` — that is how logical
//!   continuity is preserved across provider migration (correction #2).
//!
//! ## Continuity contract (correction #2 / clarification #3)
//!
//! Migration preserves `task_id` / `session_id` (logical continuity). It does
//! **not** preserve upstream generation: a retry/migration after the upstream
//! has streamed bytes is a fresh generation, recorded with
//! [`RouteRecord::generation_broken`] = true. The router/migration engine
//! is what sets that flag; this module only carries the field and
//! documents the invariant.
//!
//! ## Credential boundary (correction #5)
//!
//! [`CredentialHandle`] holds the API key resolved from `secrets.rs` at request
//! time. It is intentionally **not** `Serialize` and is not part of any
//! persisted struct. [`RouteRecord`] is the credential-free projection of a
//! [`ResolvedRoute`] that is the only thing ever written to SQLite. The
//! `no_persisted_secret_fields` test in [`crate::orchestration::store`]
//! enforces this at test time by serializing every persisted struct and
//! asserting no key/secret/credential/token-named field appears.

use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config_writer::ProviderKind;

// ===========================================================================
// Identity hierarchy
// ===========================================================================

/// Who spawned a Task / which sub-agent role is running.
///
/// `Main` is the conservative default the inbound adapters fall back to when
/// an agent cannot supply structured subagent metadata (the heuristic path
/// must never *guess* a named role — it defaults to `Main`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SubagentRole {
    /// The agent's primary/main thread. Default when role is unknown.
    Main,
    /// A named Claude Code subagent (from `~/.claude/agents/<name>.md`).
    ClaudeAgent { name: String },
    /// A named Pi subagent role.
    PiSubagent { role: String },
    /// A named OpenCode agent block (from `~/.config/opencode/opencode.json`).
    OpenCodeAgent { name: String },
}

impl SubagentRole {
    /// Stable string used as the `routing_policy.role` key. `"main"` for the
    /// default; the agent-native role name otherwise.
    pub fn as_policy_key(&self) -> String {
        match self {
            SubagentRole::Main => "main".to_string(),
            SubagentRole::ClaudeAgent { name } => format!("claude:{name}"),
            SubagentRole::PiSubagent { role } => format!("pi:{role}"),
            SubagentRole::OpenCodeAgent { name } => format!("opencode:{name}"),
        }
    }

    /// Conservatively detect a Claude Code subagent from an Anthropic Messages
    /// `system` field (string or content-block array). Returns `Main` when
    /// nothing conclusive is found — the heuristic path must never *guess* a
    /// named role from ambiguous content (correction: no prompt classification
    /// as a primary signal; this is a structured-pattern fallback only).
    ///
    /// Patterns (Claude Code's actual subagent system prompts):
    ///   1. `You are Claude Code's <name> subagent, ...` — built-in agents.
    ///   2. `You are <name>, operating as ...` / `... working as ...` —
    ///      custom `.claude/agents/<name>.md` agents.
    ///
    /// The main thread's system prompt (`You are Claude Code, Anthropic's
    /// official CLI...`) matches neither pattern, so it stays `Main`.
    pub fn from_system_prompt(system: &serde_json::Value) -> SubagentRole {
        let text = system_text(system);
        let lower = text.to_ascii_lowercase();

        // Pattern 1: built-in Claude Code subagents.
        // "you are claude code's researcher subagent"
        if let Some(idx) = lower.find("you are claude code's") {
            let rest = &lower[idx + "you are claude code's".len()..];
            if let Some(name) = rest.trim_start().split(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_').next() {
                if !name.is_empty() && name != "main" {
                    // Verify the "subagent" qualifier appears nearby (avoids
                    // matching "you are claude code's official cli"). The
                    // window must end on a char boundary — the system prompt
                    // is user-controlled UTF-8, and a mid-multibyte cut would
                    // panic on the slice.
                    let end = (idx + 200).min(lower.len());
                    let window = &lower[idx..lower.floor_char_boundary(end)];
                    if window.contains("subagent") {
                        return SubagentRole::ClaudeAgent { name: name.to_string() };
                    }
                }
            }
        }

        // Pattern 2: custom subagents — "you are <name>, operating/working as".
        // OpenCode agents additionally carry an AI-SDK signature line
        // ("You are powered by the model named <id>") that marks the prompt
        // as an OpenCode agent (policy key `opencode:<name>`); Claude Code
        // custom agents never contain it. Scoped to the FIRST 500 bytes: the
        // SDK signature sits at the top of OpenCode's system prompt — a
        // whole-text match could be tripped by the phrase appearing anywhere
        // in a long Claude prompt body.
        let is_opencode = lower[..lower.len().min(500)].contains("you are powered by the model named");

        // Noise tokens that can precede the real name in the template
        // ("You are a research subagent operating as..." → "research").
        // `s` is the possessive residue of "claude code's" (split leaves
        // ["claude","code","s"]) — never a role name.
        const NOISE: &[&str] = &["a", "an", "the", "subagent", "agent", "s"];
        for marker in ["operating as", "working as", "acting as"] {
            if let Some(idx) = lower.find(marker) {
                let before = &lower[..idx];
                if let Some(you_idx) = before.rfind("you are") {
                    let name_part = &before[you_idx + "you are".len()..];
                    let name = name_part
                        .trim()
                        .trim_start_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_')
                        .split(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_')
                        .filter(|t| !t.is_empty() && !NOISE.contains(t))
                        .find(|t| *t != "claude" && *t != "code")
                        .unwrap_or("")
                        .to_string();
                    if !name.is_empty() {
                        return if is_opencode {
                            SubagentRole::OpenCodeAgent { name }
                        } else {
                            SubagentRole::ClaudeAgent { name }
                        };
                    }
                }
            }
        }

        // Pattern 3: Pi sub-agents (pi-subagents plugin) — a structured
        // `<active_agent name="X"/>` tag marks the child session (both the
        // plugin's append and replace prompt modes carry it). The tag is
        // plugin-specific, so Claude/OpenCode prompts can never collide.
        if let Some(tag_idx) = lower.find("<active_agent") {
            if let Some(name_idx) = lower[tag_idx..].find("name=\"") {
                let rest = &lower[tag_idx + name_idx + "name=\"".len()..];
                let name: String = rest
                    .split(|c: char| c == '"' || c == ' ' || c == '>' || c == '/')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if !name.is_empty() {
                    return SubagentRole::PiSubagent { role: name };
                }
            }
        }

        SubagentRole::Main
    }
}

/// Flatten an Anthropic Messages `system` field (string OR content-block
/// array) into plain text for role detection.
pub(crate) fn system_text(system: &serde_json::Value) -> String {
    match system {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| {
                b.get("text")
                    .and_then(|t| t.as_str())
                    .map(str::to_string)
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

impl fmt::Display for SubagentRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_policy_key())
    }
}

impl Default for SubagentRole {
    fn default() -> Self {
        SubagentRole::Main
    }
}

/// How a [`SubagentRole`] was derived. Drives UI honesty ("was this native or
/// guessed?") and is persisted on every row. Heuristic is always conservative
/// — the adapter must default to [`SubagentRole::Main`] rather than guess a
/// named role from prompt content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleSource {
    /// Derived from agent-native structured metadata (header, config block,
    /// declared role).
    Native,
    /// No structured metadata was available; role was defaulted conservatively.
    /// NEVER the result of prompt-content classification.
    Heuristic,
}

impl Default for RoleSource {
    fn default() -> Self {
        RoleSource::Heuristic
    }
}

/// Optional agent-native task handle (clarification #2). Carried verbatim for
/// UI/observability correlation; **never** load-bearing for routing. The
/// router keys off `task_id` (Nestra-owned); this ref is opaque to it.
///
/// Populated by an inbound adapter when the agent exposes structured task
/// metadata (Claude `Task` tool call, OpenCode task tool, Pi task). Left
/// `None` otherwise — no adapter is rejected for not supplying it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeTaskRef {
    /// Agent id this ref belongs to (`claude-code-cli` | `opencode-desktop` | `pi-cli`).
    pub agent: String,
    /// Coarse kind, e.g. `"task_tool_call"`, `"user_turn"`, `"pi_task"`.
    /// Free-form; the router does not interpret it.
    pub kind: String,
    /// Agent-native id, preserved verbatim. Never reinterpreted by Nestra.
    pub ref_id: String,
}

/// Capabilities a Task requires of its resolved model. Derived by the inbound
/// adapter from the request (e.g. an image attachment ⇒ `vision = true`; a
/// tool-using turn ⇒ `tool_call = true`) and consumed by the router's
/// capability filter. All fields default to "no constraint".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityReq {
    /// Model must support extended reasoning.
    pub reasoning: bool,
    /// Model must support tool/function calling.
    pub tool_call: bool,
    /// Model must accept image input.
    pub vision: bool,
    /// Minimum context window (tokens) the model must expose.
    pub context_floor: Option<u64>,
}

/// Budget tier a request belongs to, classified from the model id the agent
/// sent (Claude Code's per-tier env slots point at distinct alias ids in
/// Routed mode). Feeds the policy lookup chain (`role` → `tier:<t>` → `*`)
/// so e.g. background/haiku-tier traffic can be steered to a cheaper endpoint
/// via a `tier:haiku` policy row — no schema change, `tier:*` is just a role
/// string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetTier {
    Haiku,
    Sonnet,
    Opus,
}

impl BudgetTier {
    pub fn as_str(&self) -> &'static str {
        match self {
            BudgetTier::Haiku => "haiku",
            BudgetTier::Sonnet => "sonnet",
            BudgetTier::Opus => "opus",
        }
    }

    /// `routing_policy.role` key for this tier (`tier:haiku` | …).
    pub fn as_policy_key(&self) -> String {
        format!("tier:{}", self.as_str())
    }

    /// Classify a model id by its tier token (case-insensitive substring, so
    /// marker suffixes like `[1m]` don't matter). Works for both real ids
    /// (`claude-haiku-4-5`) and any alias carrying the token. `None` for
    /// anything unclassifiable (e.g. the generic `nestra` alias).
    pub fn from_model_id(id: &str) -> Option<Self> {
        let l = id.to_ascii_lowercase();
        if l.contains("haiku") {
            Some(BudgetTier::Haiku)
        } else if l.contains("opus") {
            Some(BudgetTier::Opus)
        } else if l.contains("sonnet") {
            Some(BudgetTier::Sonnet)
        } else {
            None
        }
    }
}

/// Lifecycle of a Task. The router/migration engine drives transitions;
/// the vocabulary is defined here so the `task` table's `lifecycle` column is
/// typed end-to-end.
///
/// `GenerationBroken` is the honest label for a Task that survived a migration
/// only by re-issuing a fresh upstream generation (correction #2 /
/// clarification #3 state 2/3). It is distinct from `Done` so the UI can show
/// the user the response was not a lossless continuation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
// NOT `rename_all = "snake_case"`: the persisted vocabulary (schema.rs
// `task.lifecycle`, and `as_str()` below) uses "inflight" / "generationbroken"
// — snake_case would render "in_flight" / "generation_broken" and any
// deserialized value would silently disagree with what the DB holds.
#[serde(rename_all = "snake_case")]
pub enum TaskLifecycle {
    /// Created, not yet routed.
    Born,
    /// Router resolved an endpoint/model; request not yet sent.
    Routed,
    /// Request in flight to the upstream.
    #[serde(rename = "inflight")]
    InFlight,
    /// Mid-task migration in progress (a new Request is being issued against a
    /// different endpoint under the same `task_id`).
    Migrating,
    /// Completed only after a migration that broke upstream generation
    /// continuity — the final response is a fresh generation, not a
    /// continuation. UI labels this honestly.
    #[serde(rename = "generationbroken")]
    GenerationBroken,
    /// Completed normally.
    Done,
    /// Terminal failure (no eligible route, or surfaced error).
    Failed,
}

impl Default for TaskLifecycle {
    fn default() -> Self {
        TaskLifecycle::Born
    }
}

impl TaskLifecycle {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskLifecycle::Born => "born",
            TaskLifecycle::Routed => "routed",
            TaskLifecycle::InFlight => "inflight",
            TaskLifecycle::Migrating => "migrating",
            TaskLifecycle::GenerationBroken => "generationbroken",
            TaskLifecycle::Done => "done",
            TaskLifecycle::Failed => "failed",
        }
    }

    /// `true` for terminal states (no further transitions expected).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TaskLifecycle::Done | TaskLifecycle::Failed | TaskLifecycle::GenerationBroken
        )
    }
}

// ===========================================================================
// TaskContext — the object every inbound adapter produces
// ===========================================================================

/// The canonical Nestra routing/work context. Built by an inbound adapter
/// from one agent HTTP request, consumed by the router to produce
/// a [`ResolvedRoute`].
///
/// Every id except `agent_id` is Nestra-assigned at parse time:
/// - `task_id` / `parent_task_id` / `request_id` are Nestra UUIDs.
/// - `logical_session_id` is the agent-native session id, carried verbatim.
///
/// `native_task_ref` is the ONLY agent-native identity here, and it is
/// optional + non-load-bearing (clarification #2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskContext {
    /// Stable agent registry id (`claude-code-cli` | `opencode-desktop` | `pi-cli`).
    pub agent_id: String,
    /// Agent-native session id (Claude `sessionId`, Pi header `id`, …), when
    /// known. `None` when the request carries no session.
    pub logical_session_id: Option<String>,
    /// Nestra-owned routing/work identity. Stable across retries/migrations
    /// within one logical unit of work.
    pub task_id: Uuid,
    /// Nestra UUID of the spawning Task, for sub-agent chains. `None` for
    /// top-level tasks.
    pub parent_task_id: Option<Uuid>,
    /// Nestra UUID for THIS request. New on every retry/migration; `task_id`
    /// is what stays constant.
    pub request_id: Uuid,
    /// Which sub-agent role is running. Defaults to [`SubagentRole::Main`].
    pub subagent_role: SubagentRole,
    /// How `subagent_role` was derived (native metadata vs. conservative
    /// default). Persisted on every row for honest UI.
    pub role_source: RoleSource,
    /// Optional agent-native task handle (clarification #2). Never
    /// load-bearing for routing.
    pub native_task_ref: Option<NativeTaskRef>,
    /// Model the agent asked for (from the request body), if any. Advisory —
    /// the router may resolve a different model.
    pub requested_model: Option<String>,
    /// Provider/endpoint the agent asked for, if any. Advisory.
    pub requested_provider: Option<String>,
    /// Budget tier classified from `requested_model` (Claude Code's tier env
    /// slots). Falls between the exact role and the `*` catch-all in the
    /// policy lookup chain. `None` = unclassified.
    pub budget_tier: Option<BudgetTier>,
    /// Capabilities the resolved model must satisfy.
    pub required_capabilities: CapabilityReq,
    /// Inbound protocol direction (set by the gateway handler: Anthropic vs
    /// OpenAI). The router picks the matching `endpoint_protocol` row for the
    /// upstream base_url; `None` keeps the historical first-row behavior.
    pub protocol_hint: Option<ProviderKind>,
    /// Endpoints that already failed on THIS request (set by the migration
    /// loop before a re-resolve so the next attempt walks past them in the
    /// policy's route-target list). Empty on the first attempt.
    pub failed_endpoints: Vec<String>,
    /// Current lifecycle state.
    pub lifecycle: TaskLifecycle,
}

impl TaskContext {
    /// Construct a fresh context for a new request, with Nestra-assigned ids.
    /// Used by inbound adapters. `task_id` is taken from the caller so a
    /// retry/migration can preserve it while rotating `request_id`.
    pub fn new_for_request(
        agent_id: impl Into<String>,
        task_id: Uuid,
        logical_session_id: Option<String>,
    ) -> Self {
        Self {
            agent_id: agent_id.into(),
            logical_session_id,
            task_id,
            parent_task_id: None,
            request_id: Uuid::new_v4(),
            subagent_role: SubagentRole::Main,
            role_source: RoleSource::Heuristic,
            native_task_ref: None,
            requested_model: None,
            requested_provider: None,
            budget_tier: None,
            required_capabilities: CapabilityReq::default(),
            protocol_hint: None,
            failed_endpoints: Vec::new(),
            lifecycle: TaskLifecycle::Born,
        }
    }

    /// Begin a brand-new Task (no parent). Convenience for the common
    /// top-level case.
    pub fn new_task(agent_id: impl Into<String>, logical_session_id: Option<String>) -> Self {
        Self::new_for_request(agent_id, Uuid::new_v4(), logical_session_id)
    }

    /// Stable `routing_policy.role` key derived from `subagent_role`.
    pub fn policy_role_key(&self) -> String {
        self.subagent_role.as_policy_key()
    }
}

// ===========================================================================
// ResolvedRoute / RouteRecord — with the credential boundary
// ===========================================================================

/// Why the router picked a particular endpoint/model. Persisted on every
/// `route_request` row for observability ("why this provider/model?").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteReason {
    /// User/agent explicitly requested this provider/model.
    Explicit,
    /// Task-grain route affinity reused the previous route (cache-friendly).
    Affinity,
    /// First healthy entry of the policy's ordered route-target list.
    /// (Historical rows may carry `capability`, the pre-route-targets
    /// resolution reason.)
    Policy,
    /// Capability-eligible endpoint, ranked best on cost/latency/cache.
    /// Legacy: produced by the pre-route-targets resolver; historical
    /// `route_request` rows still carry the string.
    Capability,
    /// Chosen as a fallback after a migration trigger (quota/rate-limit/5xx).
    Fallback,
    /// No eligible route was found; the request could not be served.
    NoEligible,
}

impl RouteReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            RouteReason::Explicit => "explicit",
            RouteReason::Affinity => "affinity",
            RouteReason::Policy => "policy",
            RouteReason::Capability => "capability",
            RouteReason::Fallback => "fallback",
            RouteReason::NoEligible => "no_eligible",
        }
    }
}

/// Provider-aware prompt-cache strategy. Cache is a routing optimization,
/// not a hard constraint; breakpoints are decided per Anthropic official
/// semantics + observed provider behavior, **not** hardcoded here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheStrategy {
    /// No cache management. Passthrough.
    Off,
    /// Anthropic explicit caching — `cache_control` injection gated by
    /// `routing_policy.inject_cache_control` (default off). Breakpoint choice
    /// is decided from official semantics, not in this enum.
    AnthropicExplicit,
    /// DeepSeek automatic caching (URL-detected; no body mutation).
    DeepSeekAuto,
    /// OpenRouter provider-specific caching (URL-detected; no body mutation).
    OpenRouterPassthrough,
}

impl Default for CacheStrategy {
    fn default() -> Self {
        CacheStrategy::Off
    }
}

/// Process-local handle to a resolved API key. Created by the router at
/// request time from `secrets::get(endpoint_id)` and handed to the protocol
/// handler for THIS request only.
///
/// **Credential boundary (correction #5):** this type is deliberately NOT
/// `Serialize`, does NOT implement `Display`, and is NOT a field of any
/// persisted struct. It never enters route history, SQLite, logs, or any
/// `Serialize` type. The only credential-free projection of a route that gets
/// persisted is [`RouteRecord`]. The `Debug` impl redacts the key.
#[derive(Clone)]
pub struct CredentialHandle {
    /// The endpoint id this credential resolves for. Safe to log/persist.
    endpoint_id: String,
    /// The plaintext API key. Never logged, never serialized, never persisted.
    key: secrecy_key::SecretKey,
}

/// Tiny inline wrapper so we don't take a `secrecy` crate dependency just for
/// one redacting Debug. Holds the bytes; zeroizes on drop.
mod secrecy_key {
    /// Owning wrapper around a plaintext API key. Provides a redacting Debug
    /// and zeroes its allocation on drop as defense-in-depth. The real
    /// guarantee is at the type-system level: `CredentialHandle` is not
    /// `Serialize`, so the key can never reach a persisted struct.
    pub struct SecretKey {
        bytes: Vec<u8>,
    }

    impl SecretKey {
        pub fn new(s: String) -> Self {
            Self {
                bytes: s.into_bytes(),
            }
        }

        pub fn expose(&self) -> &str {
            // SAFETY: we only ever construct this from a `String`, so the
            // bytes are valid UTF-8.
            std::str::from_utf8(&self.bytes).unwrap_or("")
        }
    }

    impl std::fmt::Debug for SecretKey {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("[REDACTED]")
        }
    }

    impl Drop for SecretKey {
        fn drop(&mut self) {
            // Best-effort zeroize (defense-in-depth; not constant-time).
            for b in self.bytes.iter_mut() {
                // SAFETY: `b` is a live `&mut u8` into `self.bytes` (we hold
                // `&mut self`), and a single-byte volatile write is valid for
                // any aligned address — the write is what defeats the
                // optimizer's dead-store elimination.
                unsafe { std::ptr::write_volatile(b, 0) };
            }
            std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl Clone for SecretKey {
        fn clone(&self) -> Self {
            Self {
                bytes: self.bytes.clone(),
            }
        }
    }
}

impl CredentialHandle {
    /// Construct from the plaintext key read out of `secrets.rs`. The handle
    /// takes ownership; the caller should not retain the plaintext.
    pub fn new(endpoint_id: impl Into<String>, key: String) -> Self {
        Self {
            endpoint_id: endpoint_id.into(),
            key: secrecy_key::SecretKey::new(key),
        }
    }

    /// The endpoint id this credential is for. Safe to log/persist.
    pub fn endpoint_id(&self) -> &str {
        &self.endpoint_id
    }

    /// Expose the plaintext key, request-time only. The protocol handler calls
    /// this to set the upstream `Authorization` / `x-api-key` header. Nothing
    /// else should call it.
    pub fn expose_key(&self) -> &str {
        self.key.expose()
    }
}

impl std::fmt::Debug for CredentialHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialHandle")
            .field("endpoint_id", &self.endpoint_id)
            .field("key", &"[REDACTED]")
            .finish()
    }
}

/// The router's resolution for one request. Carries the live
/// [`CredentialHandle`], so this type is **not** `Serialize` and is never
/// persisted — it lives only for the duration of one proxied request.
#[derive(Debug, Clone)]
pub struct ResolvedRoute {
    pub endpoint_id: String,
    pub provider_kind: ProviderKind,
    /// Resolved model id to send upstream (may differ from the agent's
    /// `requested_model`).
    pub model: String,
    pub base_url: String,
    /// Wire protocol to speak to the upstream (Anthropic Messages or OpenAI
    /// Chat Completions). Usually matches `provider_kind` unless a future
    /// cross-protocol bridge is in play, gated).
    pub protocol: ProviderKind,
    /// Request-time credential. Not serialized, not persisted.
    pub credential: CredentialHandle,
    pub cache_strategy: CacheStrategy,
    pub reason: RouteReason,
    /// Prior `request_id`s for this Task (route history). Carried so a
    /// migration can record lineage; the persisted form lives in the
    /// `route_request` / `route_migration` tables.
    pub route_lineage: Vec<Uuid>,
}

/// Credential-free projection of a [`ResolvedRoute`] + its observed outcome.
/// This — and ONLY this — is what gets persisted to `route_request`
/// (correction #5). Constructed by the protocol handler after the upstream
/// response is observed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteRecord {
    pub request_id: Uuid,
    pub task_id: Uuid,
    pub agent_id: String,
    pub logical_session: Option<String>,
    pub subagent_role: Option<String>,
    pub role_source: Option<String>,
    pub requested_model: Option<String>,
    pub requested_provider: Option<String>,
    pub resolved_endpoint_id: Option<String>,
    pub resolved_model: Option<String>,
    pub protocol: Option<String>,
    pub route_reason: String,
    pub http_status: Option<i64>,
    pub usage_input: Option<i64>,
    pub usage_output: Option<i64>,
    pub cache_creation: Option<i64>,
    pub cache_read: Option<i64>,
    /// Distinct tool calls observed in the response stream (count only —
    /// names are deferred). Backfilled after a streaming response ends.
    pub tool_calls: Option<i64>,
    /// JSON `{name: count}` of gateway-observed tool-call invocations (raw
    /// JSON passthrough; the MCP usage aggregation parses it).
    pub tool_names: Option<String>,
    /// `true` when this record's response was produced by a fresh upstream
    /// generation after a mid-stream migration (correction #2 /
    /// clarification #3). The UI uses this to label the response honestly.
    pub generation_broken: bool,
    pub started_at: i64,
    pub ended_at: Option<i64>,
}

impl RouteRecord {
    /// Build the persisted projection from a [`ResolvedRoute`] at request-send
    /// time. Outcome fields (`http_status`, `usage_*`, `ended_at`,
    /// `generation_broken`) start empty and are filled by the protocol
    /// handler as the response streams.
    pub fn from_route(ctx: &TaskContext, route: &ResolvedRoute, started_at: i64) -> Self {
        Self {
            request_id: ctx.request_id,
            task_id: ctx.task_id,
            agent_id: ctx.agent_id.clone(),
            logical_session: ctx.logical_session_id.clone(),
            subagent_role: Some(ctx.subagent_role.to_string()),
            role_source: Some(match ctx.role_source {
                RoleSource::Native => "native",
                RoleSource::Heuristic => "heuristic",
            })
            .map(str::to_string),
            requested_model: ctx.requested_model.clone(),
            requested_provider: ctx.requested_provider.clone(),
            resolved_endpoint_id: Some(route.endpoint_id.clone()),
            resolved_model: Some(route.model.clone()),
            protocol: Some(route.protocol.as_str().to_string()),
            route_reason: route.reason.as_str().to_string(),
            http_status: None,
            usage_input: None,
            usage_output: None,
            cache_creation: None,
            cache_read: None,
            tool_calls: None,
            tool_names: None,
            generation_broken: false,
            started_at,
            ended_at: None,
        }
    }
}

#[cfg(test)]
mod tests;
