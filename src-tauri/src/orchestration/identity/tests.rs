use super::*;

#[test]
fn subagent_role_from_builtin_system_prompt() {
    // Claude Code built-in agent: "You are Claude Code's <name> subagent"
    let sys = serde_json::json!(
        "You are Claude Code's researcher subagent. You research codebases."
    );
    assert_eq!(
        SubagentRole::from_system_prompt(&sys),
        SubagentRole::ClaudeAgent { name: "researcher".into() }
    );
}

#[test]
fn subagent_role_from_custom_system_prompt() {
    // Custom .claude/agents/<name>.md: "You are <name>, operating as ..."
    let sys = serde_json::json!(
        "You are code-reviewer, operating as a specialist. Review diffs."
    );
    assert_eq!(
        SubagentRole::from_system_prompt(&sys),
        SubagentRole::ClaudeAgent { name: "code-reviewer".into() }
    );
    let sys2 = serde_json::json!(
        "You are engineer working as a coding specialist within this session."
    );
    assert_eq!(
        SubagentRole::from_system_prompt(&sys2),
        SubagentRole::ClaudeAgent { name: "engineer".into() }
    );
}

#[test]
fn subagent_role_from_opencode_agent_prompt() {
    // Real OpenCode subagent system prompt capture: the agent's own
    // definition line (noise words skipped) + the AI-SDK signature line
    // that marks it as an OpenCode agent (policy key `opencode:research`).
    let sys = serde_json::json!(
        "You are a research subagent operating as a focused researcher. When given a task, gather information and report concise findings.
You are powered by the model named nestra. The exact model ID is nestra-gw/nestra
Here is some useful information about the environment you are running in:"
    );
    assert_eq!(
        SubagentRole::from_system_prompt(&sys),
        SubagentRole::OpenCodeAgent { name: "research".into() }
    );
}

#[test]
fn subagent_role_from_pi_plugin_prompt() {
    // Real pi-subagents plugin prompts (dist/prompts.js).
    // Append mode (parent twin): parent identity + tag + context block.
    let sys = serde_json::json!(
        "You are pi, an interactive CLI tool that helps users with software engineering tasks.

<active_agent name=\"researcher\"/>

<sub_agent_context>
You are operating as a sub-agent invoked to handle a specific task.
</sub_agent_context>"
    );
    assert_eq!(
        SubagentRole::from_system_prompt(&sys),
        SubagentRole::PiSubagent { role: "researcher".into() }
    );
    // Replace mode (built-in Explore agent).
    let sys2 = serde_json::json!(
        "<active_agent name=\"Explore\"/>

You are a pi coding agent sub-agent.
You have been invoked to handle a specific task autonomously."
    );
    assert_eq!(
        SubagentRole::from_system_prompt(&sys2),
        SubagentRole::PiSubagent { role: "explore".into() }
    );
    // Pi main thread carries no tag.
    let sys3 = serde_json::json!(
        "You are pi. Use the tools available to you to assist the user."
    );
    assert_eq!(SubagentRole::from_system_prompt(&sys3), SubagentRole::Main);
}

#[test]
fn opencode_main_thread_stays_main() {
    // The OpenCode MAIN prompt must NOT be classified (no marker phrase).
    let sys = serde_json::json!(
        "You are opencode, an interactive CLI tool that helps users with software engineering tasks. Use the instructions below and the tools available to you to assist the user."
    );
    assert_eq!(
        SubagentRole::from_system_prompt(&sys),
        SubagentRole::Main
    );
}

#[test]
fn main_thread_system_prompt_stays_main() {
    // The main thread's system prompt must NOT be misclassified.
    let sys = serde_json::json!(
        "You are Claude Code, Anthropic's official CLI for Claude. Help the user."
    );
    assert_eq!(
        SubagentRole::from_system_prompt(&sys),
        SubagentRole::Main
    );
    // No system at all / unrelated content → Main.
    assert_eq!(
        SubagentRole::from_system_prompt(&serde_json::Value::Null),
        SubagentRole::Main
    );
    assert_eq!(
        SubagentRole::from_system_prompt(&serde_json::json!("help me code")),
        SubagentRole::Main
    );
}

#[test]
fn subagent_role_from_content_block_array() {
    // Anthropic system as an array of {type:"text", text:...} blocks.
    let sys = serde_json::json!([
        {"type":"text","text":"You are Claude Code's web-search subagent."},
        {"type":"text","text":"Search the web and summarize."}
    ]);
    assert_eq!(
        SubagentRole::from_system_prompt(&sys),
        SubagentRole::ClaudeAgent { name: "web-search".into() }
    );
}

#[test]
fn subagent_role_policy_keys_are_stable_and_distinct() {
    assert_eq!(SubagentRole::Main.as_policy_key(), "main");
    assert_eq!(
        SubagentRole::ClaudeAgent { name: "researcher".into() }.as_policy_key(),
        "claude:researcher"
    );
    assert_eq!(
        SubagentRole::PiSubagent { role: "coder".into() }.as_policy_key(),
        "pi:coder"
    );
    assert_eq!(
        SubagentRole::OpenCodeAgent { name: "build".into() }.as_policy_key(),
        "opencode:build"
    );
    // Distinct roles → distinct keys (no collision).
    let keys: Vec<String> = vec![
        SubagentRole::Main.as_policy_key(),
        SubagentRole::ClaudeAgent { name: "x".into() }.as_policy_key(),
        SubagentRole::PiSubagent { role: "x".into() }.as_policy_key(),
        SubagentRole::OpenCodeAgent { name: "x".into() }.as_policy_key(),
    ];
    let mut sorted = keys.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), keys.len(), "role policy keys must be distinct");
}

#[test]
fn task_context_new_assigns_fresh_uuids() {
    let ctx = TaskContext::new_task("claude-code-cli", Some("sess-1".into()));
    assert_eq!(ctx.agent_id, "claude-code-cli");
    assert_eq!(ctx.logical_session_id.as_deref(), Some("sess-1"));
    assert_eq!(ctx.subagent_role, SubagentRole::Main);
    assert_eq!(ctx.role_source, RoleSource::Heuristic);
    assert_eq!(ctx.lifecycle, TaskLifecycle::Born);
    assert_eq!(ctx.policy_role_key(), "main");
    assert!(ctx.budget_tier.is_none(), "tier defaults to unclassified");
    // Two contexts get distinct request ids.
    let other = TaskContext::new_task("claude-code-cli", None);
    assert_ne!(ctx.request_id, other.request_id);
    assert_ne!(ctx.task_id, other.task_id);
}

#[test]
fn budget_tier_classifies_from_model_id() {
    use super::BudgetTier;
    // Real CC ids and marker-suffixed forms both classify.
    assert_eq!(BudgetTier::from_model_id("claude-haiku-4-5"), Some(BudgetTier::Haiku));
    assert_eq!(
        BudgetTier::from_model_id("claude-sonnet-4-5[1m]"),
        Some(BudgetTier::Sonnet)
    );
    assert_eq!(BudgetTier::from_model_id("CLAUDE-OPUS-4-5"), Some(BudgetTier::Opus));
    // The generic alias / arbitrary model ids stay unclassified.
    assert_eq!(BudgetTier::from_model_id("nestra"), None);
    assert_eq!(BudgetTier::from_model_id("glm-5.2"), None);
    assert_eq!(BudgetTier::from_model_id(""), None);
    assert_eq!(BudgetTier::Haiku.as_policy_key(), "tier:haiku");
}

#[test]
fn retry_preserves_task_id_rotates_request_id() {
    // The continuity contract: a retry/migration constructs a new context
    // for the SAME task_id with a NEW request_id.
    let mut first = TaskContext::new_task("pi-cli", Some("s".into()));
    first.request_id = Uuid::new_v4();
    let task_id = first.task_id;

    let retry = TaskContext::new_for_request("pi-cli", task_id, Some("s".into()));
    assert_eq!(retry.task_id, task_id, "task_id must survive retry");
    assert_ne!(
        retry.request_id, first.request_id,
        "request_id must rotate on retry"
    );
}

#[test]
fn credential_handle_debug_redacts_key() {
    // CredentialHandle is deliberately NOT Serialize (see the struct
    // doc): the persisted-projections guarantee is enforced by
    // `store::tests::no_persisted_secret_fields`, which walks every
    // serialized row. Here we pin the observable surface: Debug must
    // redact the key, and the key is only reachable via expose_key().
    let h = CredentialHandle::new("ep-1", "sk-secret".into());
    // Debug redacts the key.
    let dbg = format!("{h:?}");
    assert!(dbg.contains("ep-1"));
    assert!(!dbg.contains("sk-secret"));
    // But expose works at request time.
    assert_eq!(h.expose_key(), "sk-secret");
    assert_eq!(h.endpoint_id(), "ep-1");
}

#[test]
fn route_record_excludes_credentials() {
    // The persisted projection carries no key field by construction; this
    // is the type-level check. The serialized-payload check lives in
    // store::tests::no_persisted_secret_fields and walks the JSON.
    let ctx = TaskContext::new_task("claude-code-cli", None);
    let route = ResolvedRoute {
        endpoint_id: "ep-1".into(),
        provider_kind: ProviderKind::Anthropic,
        model: "claude-3".into(),
        base_url: "https://api.example.com".into(),
        protocol: ProviderKind::Anthropic,
        credential: CredentialHandle::new("ep-1", "sk-leak".into()),
        cache_strategy: CacheStrategy::Off,
        reason: RouteReason::Capability,
        route_lineage: vec![],
    };
    let rec = RouteRecord::from_route(&ctx, &route, 0);
    let json = serde_json::to_string(&rec).unwrap();
    assert!(
        !json.contains("sk-leak"),
        "RouteRecord JSON must never contain the credential: {json}"
    );
    assert!(json.contains("ep-1"), "endpoint id is safe to persist");
    assert!(json.contains("claude-3"));
    assert_eq!(rec.generation_broken, false);
}

#[test]
fn task_lifecycle_terminal_states() {
    assert!(TaskLifecycle::Done.is_terminal());
    assert!(TaskLifecycle::Failed.is_terminal());
    assert!(TaskLifecycle::GenerationBroken.is_terminal());
    assert!(!TaskLifecycle::Born.is_terminal());
    assert!(!TaskLifecycle::InFlight.is_terminal());
    assert!(!TaskLifecycle::Migrating.is_terminal());
}

#[test]
fn route_reason_round_trips_as_str() {
    for r in [
        RouteReason::Explicit,
        RouteReason::Affinity,
        RouteReason::Capability,
        RouteReason::Fallback,
        RouteReason::NoEligible,
    ] {
        assert!(!r.as_str().is_empty());
    }
}