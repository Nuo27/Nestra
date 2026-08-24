use super::*;
use crate::orchestration::capability_registry;

fn seed_endpoint(
    conn: &Connection,
    id: &str,
    protocol: &str,
    base_url: &str,
    default_model: &str,
) {
    conn.execute(
        "INSERT INTO provider_endpoint (id, kind, display_name, has_api_key, status, models_json)
             VALUES (?1,'custom','Main',0,'unvalidated',?2)",
        rusqlite::params![id, format!("{{\"default\":\"{default_model}\"}}")],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO endpoint_protocol (endpoint_id, protocol, base_url)
             VALUES (?1,?2,?3)",
        rusqlite::params![id, protocol, base_url],
    )
    .unwrap();
}

fn seed_binding(conn: &Connection, agent_id: &str, endpoint_id: &str) {
    conn.execute(
        "INSERT INTO agent_provider_binding (agent_id, endpoint_id, active, created_at)
             VALUES (?1,?2,1,0)",
        rusqlite::params![agent_id, endpoint_id],
    )
    .unwrap();
}

/// Seed a routing policy with ordered (endpoint, model) targets.
fn seed_policy(conn: &Connection, agent_id: &str, role: &str, targets: &[(&str, &str)]) {
    let row = store::RoutingPolicyRow {
        agent_id: agent_id.into(),
        role: role.into(),
        route_targets: Some(
            serde_json::to_string(
                &targets
                    .iter()
                    .map(|(ep, m)| store::RouteTarget {
                        endpoint: ep.to_string(),
                        model: m.to_string(),
                    })
                    .collect::<Vec<_>>(),
            )
            .unwrap(),
        ),
        migrate_on_quota: true,
        inject_cache_control: false,
        affinity_scope: "task".into(),
        updated_at: chrono::Utc::now().timestamp_millis(),
    };
    store::upsert_routing_policy(conn, &row).unwrap();
}

/// Test harness owning the stores so borrows in `RouterInputs` outlive the
/// `resolve()` call. Construct per test; call `.inputs()` to borrow.
struct TestEnv {
    conn: rusqlite::Connection,
    health: ProviderHealth,
    quota: QuotaState,
    affinity: RouteAffinity,
}
impl TestEnv {
    fn new() -> Self {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::schema::build_v1(&conn).unwrap();
        // Seed the agent registry rows the bindings FK references. The
        // router only routes for agents that exist; tests use claude-code.
        for a in crate::agents::agents() {
            conn.execute(
                "INSERT OR IGNORE INTO agent (id, kind, display_name, status, last_detected_at, enabled)
                     VALUES (?1, ?2, ?3, 'ok', 0, 1)",
                rusqlite::params![a.id, a.kind, a.display_name],
            )
            .unwrap();
        }
        Self {
            conn,
            health: ProviderHealth::new(),
            quota: QuotaState::new(),
            affinity: RouteAffinity::new(),
        }
    }
    fn inputs(&self) -> RouterInputs<'_> {
        RouterInputs {
            conn: &self.conn,
            health: &self.health,
            quota: &self.quota,
            affinity: &self.affinity,
        }
    }
}

#[test]
fn explicit_pin_wins_when_endpoint_healthy() {
    let env = TestEnv::new();
    seed_endpoint(&env.conn, "ep-1", "anthropic", "https://api.anthropic.com", "claude-sonnet");

    let mut ctx = TaskContext::new_task("claude-code-cli", None);
    ctx.requested_provider = Some("ep-1".into());
    ctx.requested_model = Some("claude-sonnet".into());

    let r = resolve(&ctx, &env.inputs()).unwrap();
    assert_eq!(r.reason, RouteReason::Explicit);
    assert_eq!(r.endpoint_id, "ep-1");
    assert_eq!(r.model, "claude-sonnet");
}

#[test]
fn no_policy_targets_fails_closed() {
    // A role with no policy row (and no `*` row) synthesizes an empty
    // policy — routing must fail closed, not fall back to bindings.
    let env = TestEnv::new();
    seed_endpoint(&env.conn, "ep-1", "anthropic", "https://x", "m-1");
    seed_binding(&env.conn, "claude-code-cli", "ep-1");

    let ctx = TaskContext::new_task("claude-code-cli", None);
    let r = resolve(&ctx, &env.inputs()).unwrap();
    assert_eq!(r.reason, RouteReason::NoEligible);
    assert!(r.endpoint_id.is_empty());
}

#[test]
fn empty_target_list_fails_closed() {
    // The `*` row exists but carries no targets — same honest outcome.
    let env = TestEnv::new();
    seed_endpoint(&env.conn, "ep-1", "anthropic", "https://x", "m-1");
    seed_policy(&env.conn, "claude-code-cli", "*", &[]);

    let ctx = TaskContext::new_task("claude-code-cli", None);
    let r = resolve(&ctx, &env.inputs()).unwrap();
    assert_eq!(r.reason, RouteReason::NoEligible);
}

#[test]
fn first_healthy_target_wins_in_order() {
    let env = TestEnv::new();
    seed_endpoint(&env.conn, "ep-1", "openai-comp", "https://x", "m-1");
    seed_endpoint(&env.conn, "ep-2", "openai-comp", "https://y", "m-2");
    seed_policy(
        &env.conn,
        "claude-code-cli",
        "*",
        &[("ep-1", "m-1"), ("ep-2", "m-2")],
    );

    let ctx = TaskContext::new_task("claude-code-cli", None);
    let r = resolve(&ctx, &env.inputs()).unwrap();
    assert_eq!(r.reason, RouteReason::Policy);
    assert_eq!(r.endpoint_id, "ep-1");
    assert_eq!(r.model, "m-1");
}

#[test]
fn target_model_is_honored_verbatim() {
    // The target's model is the user's explicit intent — even when it is
    // NOT the endpoint's default model.
    let env = TestEnv::new();
    seed_endpoint(&env.conn, "z-ai", "openai-comp", "https://api.z.ai/api/paas/v4", "glm-4.7");
    seed_policy(&env.conn, "opencode-desktop", "*", &[("z-ai", "glm-5.2")]);

    let mut ctx = TaskContext::new_task("opencode-desktop", None);
    ctx.protocol_hint = Some(ProviderKind::Openai);
    let r = resolve(&ctx, &env.inputs()).unwrap();
    assert_eq!(r.model, "glm-5.2");
    assert_eq!(r.reason, RouteReason::Policy);
}

#[test]
fn quota_exhausted_target_is_skipped() {
    let env = TestEnv::new();
    seed_endpoint(&env.conn, "ep-1", "openai-comp", "https://x", "m-1");
    seed_endpoint(&env.conn, "ep-2", "openai-comp", "https://y", "m-2");
    seed_policy(
        &env.conn,
        "claude-code-cli",
        "*",
        &[("ep-1", "m-1"), ("ep-2", "m-2")],
    );

    env.quota.mark_exhausted("ep-1", Some("5h window elapsed".into()));

    let ctx = TaskContext::new_task("claude-code-cli", None);
    let r = resolve(&ctx, &env.inputs()).unwrap();
    assert_eq!(r.endpoint_id, "ep-2");
    assert_eq!(r.model, "m-2");
}

#[test]
fn degraded_target_is_skipped() {
    let env = TestEnv::new();
    seed_endpoint(&env.conn, "ep-1", "openai-comp", "https://x", "m-1");
    seed_endpoint(&env.conn, "ep-2", "openai-comp", "https://y", "m-2");
    seed_policy(
        &env.conn,
        "claude-code-cli",
        "*",
        &[("ep-1", "m-1"), ("ep-2", "m-2")],
    );
    for _ in 0..3 {
        env.health.record(
            "ep-1",
            "m-1",
            crate::orchestration::health::HealthOutcome::Fail(
                crate::orchestration::health::FailureClass::Temp5xx,
            ),
            503,
        );
    }

    let ctx = TaskContext::new_task("claude-code-cli", None);
    let r = resolve(&ctx, &env.inputs()).unwrap();
    assert_eq!(r.endpoint_id, "ep-2");
}

#[test]
fn failed_endpoint_exclusion_walks_forward() {
    // The migration loop marks failed endpoints on the context — a
    // re-resolve must walk PAST them in the target list (this is how
    // failover walks the ordered list without waiting for health degrade).
    let env = TestEnv::new();
    seed_endpoint(&env.conn, "ep-1", "openai-comp", "https://x", "m-1");
    seed_endpoint(&env.conn, "ep-2", "openai-comp", "https://y", "m-2");
    seed_policy(
        &env.conn,
        "claude-code-cli",
        "*",
        &[("ep-1", "m-1"), ("ep-2", "m-2")],
    );

    let mut ctx = TaskContext::new_task("claude-code-cli", None);
    ctx.failed_endpoints.push("ep-1".into());
    let r = resolve(&ctx, &env.inputs()).unwrap();
    assert_eq!(r.endpoint_id, "ep-2");

    // All targets excluded → fail closed.
    let mut ctx = TaskContext::new_task("claude-code-cli", None);
    ctx.failed_endpoints.push("ep-1".into());
    ctx.failed_endpoints.push("ep-2".into());
    let r = resolve(&ctx, &env.inputs()).unwrap();
    assert_eq!(r.reason, RouteReason::NoEligible);
}

#[test]
fn missing_endpoint_target_is_skipped_with_warning() {
    let env = TestEnv::new();
    seed_endpoint(&env.conn, "ep-2", "openai-comp", "https://y", "m-2");
    seed_policy(
        &env.conn,
        "claude-code-cli",
        "*",
        &[("ep-gone", "m-x"), ("ep-2", "m-2")],
    );

    let ctx = TaskContext::new_task("claude-code-cli", None);
    let r = resolve(&ctx, &env.inputs()).unwrap();
    assert_eq!(r.endpoint_id, "ep-2");
}

#[test]
fn affinity_reuses_previous_route_for_same_task() {
    let env = TestEnv::new();
    seed_endpoint(&env.conn, "ep-1", "anthropic", "https://x", "m-1");
    seed_policy(&env.conn, "claude-code-cli", "*", &[("ep-1", "m-1")]);

    // First request: policy pick records affinity.
    let ctx1 = TaskContext::new_task("claude-code-cli", None);
    let r1 = resolve(&ctx1, &env.inputs()).unwrap();
    assert_eq!(r1.reason, RouteReason::Policy);
    assert_eq!(r1.endpoint_id, "ep-1");

    // Second request for the SAME task_id → affinity hit.
    let mut ctx2 = TaskContext::new_for_request("claude-code-cli", ctx1.task_id, None);
    ctx2.lifecycle = crate::orchestration::identity::TaskLifecycle::InFlight;
    let r2 = resolve(&ctx2, &env.inputs()).unwrap();
    assert_eq!(r2.reason, RouteReason::Affinity, "same task_id must hit affinity");
    assert_eq!(r2.endpoint_id, "ep-1");
}

#[test]
fn subagent_role_policy_wins_over_tier_and_wildcard() {
    // The tier layer (between exact role and `*`) must not weaken subagent
    // role routing: a claude:researcher request resolves via the researcher
    // policy's first target even when a tier:haiku row and a `*` row exist.
    let env = TestEnv::new();
    seed_endpoint(&env.conn, "ep-researcher", "anthropic", "https://y", "m-researcher");
    seed_endpoint(&env.conn, "ep-tier", "anthropic", "https://z", "m-tier");
    seed_endpoint(&env.conn, "ep-wild", "anthropic", "https://w", "m-wild");
    seed_policy(&env.conn, "claude-code-cli", "claude:researcher", &[("ep-researcher", "m-researcher")]);
    seed_policy(&env.conn, "claude-code-cli", "tier:haiku", &[("ep-tier", "m-tier")]);
    seed_policy(&env.conn, "claude-code-cli", "*", &[("ep-wild", "m-wild")]);

    let mut ctx = TaskContext::new_task("claude-code-cli", None);
    ctx.subagent_role = crate::orchestration::identity::SubagentRole::ClaudeAgent {
        name: "researcher".into(),
    };
    ctx.budget_tier = Some(crate::orchestration::identity::BudgetTier::Haiku);
    let r = resolve(&ctx, &env.inputs()).unwrap();
    assert_eq!(r.endpoint_id, "ep-researcher", "exact role policy must beat tier:*");
    assert_eq!(r.model, "m-researcher");

    // Same agent, main thread + haiku tier (no role row) → the tier row.
    let mut ctx = TaskContext::new_task("claude-code-cli", None);
    ctx.budget_tier = Some(crate::orchestration::identity::BudgetTier::Haiku);
    let r = resolve(&ctx, &env.inputs()).unwrap();
    assert_eq!(r.endpoint_id, "ep-tier");

    // No tier → the `*` catch-all.
    let ctx = TaskContext::new_task("claude-code-cli", None);
    let r = resolve(&ctx, &env.inputs()).unwrap();
    assert_eq!(r.endpoint_id, "ep-wild");
}

/// Overwrite an endpoint's `models_json` and rebuild the catalog.
fn seed_endpoint_models(conn: &Connection, id: &str, models_json: &str) {
    conn.execute(
        "UPDATE provider_endpoint SET models_json = ?2 WHERE id = ?1",
        rusqlite::params![id, models_json],
    )
    .unwrap();
    capability_registry::rebuild(conn).unwrap();
}

fn add_protocol_row(conn: &Connection, id: &str, protocol: &str, base_url: &str) {
    conn.execute(
        "INSERT INTO endpoint_protocol (endpoint_id, protocol, base_url) VALUES (?1,?2,?3)",
        rusqlite::params![id, protocol, base_url],
    )
    .unwrap();
}

/// The upstream base_url follows the inbound direction: an OpenAI-shape
/// request picks the endpoint's `openai` protocol row, an Anthropic one
/// the `anthropic` row — not blindly the first row.
#[test]
fn protocol_hint_picks_matching_endpoint_row() {
    let env = TestEnv::new();
    seed_endpoint(&env.conn, "z-ai", "anthropic", "https://api.z.ai/api/anthropic", "glm-5.2");
    add_protocol_row(&env.conn, "z-ai", "openai-comp", "https://api.z.ai/api/paas/v4");
    seed_endpoint_models(&env.conn, "z-ai", r#"{"available":["glm-5.2"],"default":"glm-5.2"}"#);
    seed_policy(&env.conn, "opencode-desktop", "*", &[("z-ai", "glm-5.2")]);

    let mut openai_ctx = TaskContext::new_task("opencode-desktop", None);
    openai_ctx.protocol_hint = Some(ProviderKind::Openai);
    assert_eq!(
        resolve(&openai_ctx, &env.inputs()).unwrap().base_url,
        "https://api.z.ai/api/paas/v4"
    );

    let mut anthropic_ctx = TaskContext::new_task("claude-code-cli", None);
    anthropic_ctx.protocol_hint = Some(ProviderKind::Anthropic);
    seed_policy(&env.conn, "claude-code-cli", "*", &[("z-ai", "glm-5.2")]);
    assert_eq!(
        resolve(&anthropic_ctx, &env.inputs()).unwrap().base_url,
        "https://api.z.ai/api/anthropic"
    );

    // No hint → historical first-row behavior.
    let bare_ctx = TaskContext::new_task("claude-code-cli", None);
    assert_eq!(
        resolve(&bare_ctx, &env.inputs()).unwrap().base_url,
        "https://api.z.ai/api/anthropic"
    );
}

/// A same-gateway endpoint (opencode-go: anthropic + openai rows on ONE
/// base_url) routes an Anthropic inbound to the openai row so the
/// conversion layer can speak the official per-model protocol; distinct
/// base_urls keep the direction-matched row. The WIRE then follows the
/// model's api dialect: deepseek-v4-flash is anthropic-class (corrections
/// map), so it dials Messages directly — no conversion needed.
#[test]
fn same_gateway_dual_rows_prefer_openai_for_anthropic_inbound() {
    let env = TestEnv::new();
    seed_endpoint(&env.conn, "opencode-go", "anthropic", "https://opencode.ai/zen/go/v1", "deepseek-v4-flash");
    add_protocol_row(&env.conn, "opencode-go", "openai-comp", "https://opencode.ai/zen/go/v1");
    seed_policy(&env.conn, "claude-code-cli", "*", &[("opencode-go", "deepseek-v4-flash")]);
    seed_policy(&env.conn, "opencode-desktop", "*", &[("opencode-go", "deepseek-v4-flash")]);
    seed_endpoint_models(
        &env.conn,
        "opencode-go",
        r#"{"available":["deepseek-v4-flash"],"default":"deepseek-v4-flash"}"#,
    );

    let mut anthropic_ctx = TaskContext::new_task("claude-code-cli", None);
    anthropic_ctx.protocol_hint = Some(ProviderKind::Anthropic);
    let route = resolve(&anthropic_ctx, &env.inputs()).unwrap();
    assert_eq!(
        route.protocol,
        ProviderKind::Anthropic,
        "deepseek-v4-flash is anthropic-class (corrections) → dials Messages directly"
    );
    assert_eq!(route.base_url, "https://opencode.ai/zen/go/v1");

    // OpenAI inbound keeps the openai wire (anthropic-class models accept
    // the chat wire too).
    let mut openai_ctx = TaskContext::new_task("opencode-desktop", None);
    openai_ctx.protocol_hint = Some(ProviderKind::Openai);
    let route = resolve(&openai_ctx, &env.inputs()).unwrap();
    assert_eq!(route.protocol, ProviderKind::Openai);
}

/// Distinct base_urls (DeepSeek/Moonshot real dual endpoints) keep the
/// direction-matched row — Anthropic inbound hits the anthropic endpoint.
#[test]
fn distinct_base_urls_keep_direction_matched_row() {
    let env = TestEnv::new();
    seed_endpoint(&env.conn, "deepseek", "anthropic", "https://api.deepseek.com/anthropic", "deepseek-chat");
    add_protocol_row(&env.conn, "deepseek", "openai-comp", "https://api.deepseek.com/v1");
    seed_policy(&env.conn, "claude-code-cli", "*", &[("deepseek", "deepseek-chat")]);
    seed_endpoint_models(
        &env.conn,
        "deepseek",
        r#"{"available":["deepseek-chat"],"default":"deepseek-chat"}"#,
    );

    let mut anthropic_ctx = TaskContext::new_task("claude-code-cli", None);
    anthropic_ctx.protocol_hint = Some(ProviderKind::Anthropic);
    let route = resolve(&anthropic_ctx, &env.inputs()).unwrap();
    assert_eq!(route.protocol, ProviderKind::Anthropic);
    assert_eq!(route.base_url, "https://api.deepseek.com/anthropic");
}

/// Mock-style endpoints declare one row that serves both shapes; a
/// direction with no matching row falls back to the first one.
#[test]
fn protocol_hint_falls_back_to_first_row_when_no_match() {
    let env = TestEnv::new();
    seed_endpoint(&env.conn, "mock-a", "anthropic", "http://127.0.0.1:8787", "claude-haiku-4-5");
    seed_policy(&env.conn, "pi-cli", "*", &[("mock-a", "claude-haiku-4-5")]);
    seed_endpoint_models(
        &env.conn,
        "mock-a",
        r#"{"available":["claude-haiku-4-5"],"default":"claude-haiku-4-5"}"#,
    );

    let mut ctx = TaskContext::new_task("pi-cli", None);
    ctx.protocol_hint = Some(ProviderKind::Openai);
    let route = resolve(&ctx, &env.inputs()).unwrap();
    assert_eq!(route.base_url, "http://127.0.0.1:8787");
}

/// Overwrite a catalog row's abilities `api` dialect (simulating the
/// corrections/override layer) so wire selection can be tested directly.
fn set_catalog_api(conn: &Connection, endpoint: &str, model: &str, api: &str) {
    let abilities = format!(r#"{{"api":"{api}"}}"#);
    conn.execute(
        "UPDATE model_catalog SET abilities_json = ?3 WHERE endpoint_id = ?1 AND model_id = ?2",
        rusqlite::params![endpoint, model, abilities],
    )
    .unwrap();
}

/// A responses-class model (grok-4.5) on an Anthropic inbound resolves to
/// the Responses wire.
#[test]
fn wire_responses_class_routes_to_responses_api() {
    let env = TestEnv::new();
    seed_endpoint(&env.conn, "opencode-go", "anthropic", "https://opencode.ai/zen/go/v1", "grok-4.5");
    add_protocol_row(&env.conn, "opencode-go", "openai-comp", "https://opencode.ai/zen/go/v1");
    seed_policy(&env.conn, "claude-code-cli", "*", &[("opencode-go", "grok-4.5")]);
    seed_endpoint_models(
        &env.conn,
        "opencode-go",
        r#"{"available":["grok-4.5"],"default":"grok-4.5"}"#,
    );
    set_catalog_api(&env.conn, "opencode-go", "grok-4.5", "response-api");

    let mut ctx = TaskContext::new_task("claude-code-cli", None);
    ctx.protocol_hint = Some(ProviderKind::Anthropic);
    let route = resolve(&ctx, &env.inputs()).unwrap();
    assert_eq!(route.protocol, ProviderKind::Responses);
    assert_eq!(route.base_url, "https://opencode.ai/zen/go/v1");
    assert_eq!(route.model, "grok-4.5");
}

/// An openai-class model (kimi-k3) on an Anthropic inbound resolves to
/// the Chat wire (conversion) — it rejects the Anthropic wire.
#[test]
fn wire_openai_class_routes_to_chat() {
    let env = TestEnv::new();
    seed_endpoint(&env.conn, "opencode-go", "anthropic", "https://opencode.ai/zen/go/v1", "kimi-k3");
    add_protocol_row(&env.conn, "opencode-go", "openai-comp", "https://opencode.ai/zen/go/v1");
    seed_policy(&env.conn, "claude-code-cli", "*", &[("opencode-go", "kimi-k3")]);
    seed_endpoint_models(
        &env.conn,
        "opencode-go",
        r#"{"available":["kimi-k3"],"default":"kimi-k3"}"#,
    );
    set_catalog_api(&env.conn, "opencode-go", "kimi-k3", "openai-comp");

    let mut ctx = TaskContext::new_task("claude-code-cli", None);
    ctx.protocol_hint = Some(ProviderKind::Anthropic);
    let route = resolve(&ctx, &env.inputs()).unwrap();
    assert_eq!(route.protocol, ProviderKind::Openai);
}

/// Explicit targets are the user's intent — capability gating no longer
/// overrides them (the vision/text-only capability filter was abolished
/// with the route-target model; abilities are display data now).
#[test]
fn explicit_target_bypasses_capability_gating() {
    let env = TestEnv::new();
    seed_endpoint(&env.conn, "ep-text", "anthropic", "https://t", "m-text");
    seed_policy(&env.conn, "claude-code-cli", "*", &[("ep-text", "m-text")]);
    env.conn
        .execute(
            "UPDATE provider_endpoint SET model_abilities_json = ?2 WHERE id = 'ep-text'",
            rusqlite::params!["ep-text", r#"{"m-text":{"modalities":{"input":["text"]}}}"#],
        )
        .unwrap();
    capability_registry::rebuild(&env.conn).unwrap();

    // An image-bearing request still routes to the explicit target — the
    // upstream (not the router) is the authority on what it can serve.
    let body = br#"{"model":"m-text","messages":[{"role":"user","content":[
             {"type":"text","text":"see"},{"type":"image","source":{"data":"x"}}]}]}"#;
    let mut ctx = TaskContext::new_task("claude-code-cli", None);
    ctx.required_capabilities =
        capability_registry::derive_capability_req(body, ProviderKind::Anthropic);
    assert!(ctx.required_capabilities.vision);
    let r = resolve(&ctx, &env.inputs()).unwrap();
    assert_eq!(r.endpoint_id, "ep-text");
    assert_eq!(r.model, "m-text");
}

#[test]
fn session_affinity_survives_simulated_restart() {
    let conn = Connection::open_in_memory().unwrap();
    crate::schema::build_v1(&conn).unwrap();
    let a = RouteAffinity::new();
    let ctx = TaskContext::new_task("claude-code-cli", Some("sess-1".to_string()));
    a.record(&conn, &ctx, "ep-a", "model-a");

    // Fresh instance = a Nestra restart: only session-grain affinity
    // comes back (task entries are ephemeral by design).
    let b = RouteAffinity::new();
    b.load_sessions(&conn);
    assert_eq!(
        b.lookup(&ctx, AffinityScope::Session),
        Some(("ep-a".to_string(), "model-a".to_string()))
    );
    assert_eq!(b.lookup(&ctx, AffinityScope::Task), None);
}

#[test]
fn affinity_persist_is_debounced() {
    let conn = Connection::open_in_memory().unwrap();
    crate::schema::build_v1(&conn).unwrap();
    let a = RouteAffinity::new();
    let ctx = TaskContext::new_task("claude-code-cli", Some("sess-1".to_string()));
    a.record(&conn, &ctx, "ep-a", "model-a");

    // Second record inside the debounce window must not rewrite the
    // setting (detect via row deletion: a rewrite would re-insert it).
    conn.execute("DELETE FROM setting_kv WHERE key = 'route_affinity'", [])
        .unwrap();
    a.record(&conn, &ctx, "ep-b", "model-b");
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM setting_kv WHERE key = 'route_affinity'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 0, "debounced record must skip the setting write");
}

#[test]
fn low_remaining_target_is_soft_skipped_for_healthier_target() {
    let env = TestEnv::new();
    seed_endpoint(&env.conn, "ep-1", "openai-comp", "https://x", "m-1");
    seed_endpoint(&env.conn, "ep-2", "openai-comp", "https://y", "m-2");
    seed_policy(
        &env.conn,
        "claude-code-cli",
        "*",
        &[("ep-1", "m-1"), ("ep-2", "m-2")],
    );

    // ep-1 nearly spent (proactive fetch said 3% left) — the walk moves on.
    env.quota.set_remaining("ep-1", 3.0);
    let ctx = TaskContext::new_task("claude-code-cli", None);
    let r = resolve(&ctx, &env.inputs()).unwrap();
    assert_eq!(r.endpoint_id, "ep-2", "low-remaining target yields to the next");

    // Unknown budget (no signal) never skips: ep-2's signal absent, ep-1
    // still low → but here both lack a signal on a fresh env.
    let env2 = TestEnv::new();
    seed_endpoint(&env2.conn, "ep-1", "openai-comp", "https://x", "m-1");
    seed_policy(&env2.conn, "claude-code-cli", "*", &[("ep-1", "m-1")]);
    let r = resolve(&TaskContext::new_task("claude-code-cli", None), &env2.inputs()).unwrap();
    assert_eq!(r.endpoint_id, "ep-1", "no quota signal → policy order holds");
}

#[test]
fn all_targets_low_still_serves_first_rather_than_failing_closed() {
    let env = TestEnv::new();
    seed_endpoint(&env.conn, "ep-1", "openai-comp", "https://x", "m-1");
    seed_endpoint(&env.conn, "ep-2", "openai-comp", "https://y", "m-2");
    seed_policy(
        &env.conn,
        "claude-code-cli",
        "*",
        &[("ep-1", "m-1"), ("ep-2", "m-2")],
    );

    env.quota.set_remaining("ep-1", 1.0);
    env.quota.set_remaining("ep-2", 0.5);

    let ctx = TaskContext::new_task("claude-code-cli", None);
    let r = resolve(&ctx, &env.inputs()).unwrap();
    assert_eq!(
        r.endpoint_id, "ep-1",
        "every target low → first still wins (nearly-empty beats fail-closed)"
    );
}
