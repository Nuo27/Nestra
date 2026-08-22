use super::*;

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
    capability_registry::rebuild(&env.conn).unwrap();

    let mut ctx = TaskContext::new_task("claude-code-cli", None);
    ctx.requested_provider = Some("ep-1".into());
    ctx.requested_model = Some("claude-sonnet".into());

    let r = resolve(&ctx, &env.inputs()).unwrap();
    assert_eq!(r.reason, RouteReason::Explicit);
    assert_eq!(r.endpoint_id, "ep-1");
    assert_eq!(r.model, "claude-sonnet");
}

#[test]
fn explicit_pin_falls_through_when_degraded() {
    let env = TestEnv::new();
    seed_endpoint(&env.conn, "ep-1", "anthropic", "https://x", "m-1");
    seed_binding(&env.conn, "claude-code-cli", "ep-1");
    capability_registry::rebuild(&env.conn).unwrap();

    // Degrade ep-1 with 3 migratable failures.
    for _ in 0..3 {
        env.health.record(
            "ep-1",
            crate::orchestration::health::HealthOutcome::Fail(
                crate::orchestration::health::FailureClass::QuotaExhausted,
            ),
            429,
        );
    }

    let mut ctx = TaskContext::new_task("claude-code-cli", None);
    ctx.requested_provider = Some("ep-1".into());
    ctx.requested_model = Some("m-1".into());
    // ep-1 is degraded AND it's the only candidate → fail closed.
    let r = resolve(&ctx, &env.inputs()).unwrap();
    assert_eq!(r.reason, RouteReason::NoEligible);
    assert!(r.endpoint_id.is_empty());
}

#[test]
fn affinity_reuses_previous_route_for_same_task() {
    let env = TestEnv::new();
    seed_endpoint(&env.conn, "ep-1", "anthropic", "https://x", "m-1");
    seed_binding(&env.conn, "claude-code-cli", "ep-1");
    capability_registry::rebuild(&env.conn).unwrap();

    // First request: capability pick records affinity.
    let ctx1 = TaskContext::new_task("claude-code-cli", None);
    let r1 = resolve(&ctx1, &env.inputs()).unwrap();
    assert_eq!(r1.reason, RouteReason::Capability);
    assert_eq!(r1.endpoint_id, "ep-1");

    // Second request for the SAME task_id → affinity hit.
    let mut ctx2 = TaskContext::new_for_request("claude-code-cli", ctx1.task_id, None);
    ctx2.lifecycle = crate::orchestration::identity::TaskLifecycle::InFlight;
    let r2 = resolve(&ctx2, &env.inputs()).unwrap();
    assert_eq!(r2.reason, RouteReason::Affinity, "same task_id must hit affinity");
    assert_eq!(r2.endpoint_id, "ep-1");
}

#[test]
fn subagent_role_policy_wins_over_tier_and_bound_default() {
    // The tier layer (added between exact role and `*`) must not weaken
    // subagent role routing: a claude:researcher request resolves via the
    // researcher policy's preferred endpoint even when the request also
    // carries a classifiable haiku tier AND a tier:haiku row exists.
    let env = TestEnv::new();
    seed_endpoint(&env.conn, "ep-bound", "anthropic", "https://x", "m-bound");
    seed_endpoint(&env.conn, "ep-researcher", "anthropic", "https://y", "m-researcher");
    seed_endpoint(&env.conn, "ep-tier", "anthropic", "https://z", "m-tier");
    seed_binding(&env.conn, "claude-code-cli", "ep-bound");
    capability_registry::rebuild(&env.conn).unwrap();

    let now = chrono::Utc::now().timestamp_millis();
    for (role, ep) in [("claude:researcher", "ep-researcher"), ("tier:haiku", "ep-tier")] {
        store::upsert_routing_policy(
            &env.conn,
            &store::RoutingPolicyRow {
                agent_id: "claude-code-cli".into(),
                role: role.into(),
                preferred_endpoints: Some(format!(r#"["{ep}"]"#).into()),
                fallback_endpoints: None,
                allowed_models: None,
                migrate_on_quota: true,
                inject_cache_control: false,
                affinity_scope: "task".into(),
                updated_at: now,
            },
        )
        .unwrap();
    }

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
}

#[test]
fn openai_inbound_with_anthropic_only_row_bridges_to_messages() {
    // A chat-wire agent bound to a single-row (anthropic) endpoint must
    // resolve the Anthropic wire so the OpenAI handler can bridge — the
    // 404 "page not found" case (MiniMax-M3 on `…/anthropic`).
    let env = TestEnv::new();
    seed_endpoint(&env.conn, "ep-a", "anthropic", "https://api.minimaxi.com/anthropic", "MiniMax-M3");
    seed_binding(&env.conn, "opencode-desktop", "ep-a");
    capability_registry::rebuild(&env.conn).unwrap();

    let mut ctx = TaskContext::new_task("opencode-desktop", None);
    ctx.protocol_hint = Some(ProviderKind::Openai);
    let r = resolve(&ctx, &env.inputs()).unwrap();
    assert_eq!(r.endpoint_id, "ep-a");
    assert_eq!(r.protocol, ProviderKind::Anthropic, "chat inbound follows the anthropic row");
}

#[test]
fn openai_inbound_keeps_chat_when_openai_row_exists() {
    // Regression: a chat-wire agent on an endpoint WITH an openai row
    // stays native Chat (no bridge).
    let env = TestEnv::new();
    // anthropic + openai-comp rows on the SAME base (mock-style dual row).
    seed_endpoint(&env.conn, "ep-a", "anthropic", "https://x", "m-1");
    env.conn
        .execute(
            "INSERT INTO endpoint_protocol (endpoint_id, protocol, base_url)
                 VALUES ('ep-a','openai-comp','https://x')",
            [],
        )
        .unwrap();
    seed_binding(&env.conn, "opencode-desktop", "ep-a");
    capability_registry::rebuild(&env.conn).unwrap();

    let mut ctx = TaskContext::new_task("opencode-desktop", None);
    ctx.protocol_hint = Some(ProviderKind::Openai);
    let r = resolve(&ctx, &env.inputs()).unwrap();
    assert_eq!(r.protocol, ProviderKind::Openai, "openai row present → native chat wire");
}

#[test]
fn allowed_models_glob_filters_capability_pick() {
    let env = TestEnv::new();
    seed_endpoint(&env.conn, "ep-1", "openai-comp", "https://x", "gpt-4o");
    seed_binding(&env.conn, "claude-code-cli", "ep-1");
    capability_registry::rebuild(&env.conn).unwrap();

    // Policy: only allow `claude-*`. ep-1 only has `gpt-4o` → no match.
    let now = chrono::Utc::now().timestamp_millis();
    store::upsert_routing_policy(
        &env.conn,
        &store::RoutingPolicyRow {
            agent_id: "claude-code-cli".into(),
            role: "*".into(),
            preferred_endpoints: None,
            fallback_endpoints: None,
            allowed_models: Some(r#"["claude-*"]"#.into()),
            migrate_on_quota: true,
            inject_cache_control: false,
            affinity_scope: "task".into(),
            updated_at: now,
        },
    )
    .unwrap();

    let ctx = TaskContext::new_task("claude-code-cli", None);
    let r = resolve(&ctx, &env.inputs()).unwrap();
    assert_eq!(r.reason, RouteReason::NoEligible, "gpt-4o blocked by claude-* glob");
}

#[test]
fn quota_exhausted_endpoint_is_skipped() {
    let env = TestEnv::new();
    seed_endpoint(&env.conn, "ep-1", "openai-comp", "https://x", "m-1");
    seed_endpoint(&env.conn, "ep-2", "openai-comp", "https://y", "m-2");
    seed_binding(&env.conn, "claude-code-cli", "ep-1");
    seed_binding(&env.conn, "claude-code-cli", "ep-2");
    capability_registry::rebuild(&env.conn).unwrap();

    env.quota.mark_exhausted("ep-1", Some("5h window elapsed".into()));

    let ctx = TaskContext::new_task("claude-code-cli", None);
    let r = resolve(&ctx, &env.inputs()).unwrap();
    // ep-1 exhausted → router skips to ep-2.
    assert_eq!(r.endpoint_id, "ep-2");
    assert_eq!(r.reason, RouteReason::Capability);
}

#[test]
fn model_allowed_helper() {
    assert!(model_allowed(&None, "anything"));
    assert!(model_allowed(&Some(r#"[]"#.into()), "anything")); // empty = permissive
    assert!(model_allowed(&Some(r#"["claude-*"]"#.into()), "claude-sonnet"));
    assert!(!model_allowed(&Some(r#"["claude-*"]"#.into()), "gpt-4o"));
    assert!(model_allowed(&Some(r#"["gpt-4o"]"#.into()), "gpt-4o"));
    assert!(!model_allowed(&Some(r#"["gpt-4o"]"#.into()), "gpt-4o-mini"));
    // Malformed JSON = permissive (never block on bad data).
    assert!(model_allowed(&Some("not json".into()), "x"));
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

/// Overwrite `models_json` WITHOUT rebuilding the catalog — proves the
/// router reads the default live instead of the cached catalog.
fn set_models_no_rebuild(conn: &Connection, id: &str, models_json: &str) {
    conn.execute(
        "UPDATE provider_endpoint SET models_json = ?2 WHERE id = ?1",
        rusqlite::params![id, models_json],
    )
    .unwrap();
}

fn add_protocol_row(conn: &Connection, id: &str, protocol: &str, base_url: &str) {
    conn.execute(
        "INSERT INTO endpoint_protocol (endpoint_id, protocol, base_url) VALUES (?1,?2,?3)",
        rusqlite::params![id, protocol, base_url],
    )
    .unwrap();
}

/// Routing semantics: the first eligible provider serves its DEFAULT
/// model. glm-4.7 sorts before glm-5.2 alphabetically — the default must
/// still win (regression: the old code picked the alphabetical-first
/// catalog model and ignored `models_json.default`).
#[test]
fn capability_pick_prefers_endpoint_default_model() {
    let env = TestEnv::new();
    seed_endpoint(&env.conn, "z-ai", "openai-comp", "https://api.z.ai/api/paas/v4", "glm-4.7");
    seed_binding(&env.conn, "opencode-desktop", "z-ai");
    seed_endpoint_models(
        &env.conn,
        "z-ai",
        r#"{"available":["glm-4.7","glm-5.2"],"default":"glm-5.2"}"#,
    );

    let mut ctx = TaskContext::new_task("opencode-desktop", None);
    ctx.requested_model = Some("nestra".into());
    ctx.protocol_hint = Some(ProviderKind::Openai);
    let route = resolve(&ctx, &env.inputs()).unwrap();

    assert_eq!(route.endpoint_id, "z-ai");
    assert_eq!(route.model, "glm-5.2", "default must win over alphabetical-first");
    assert_eq!(route.reason, RouteReason::Capability);
}

/// A default-model edit on the Provider page takes effect on the NEXT
/// request — the router reads `models_json.default` live (no catalog
/// rebuild, no gateway restart).
#[test]
fn default_model_edit_takes_effect_without_catalog_rebuild() {
    let env = TestEnv::new();
    seed_endpoint(&env.conn, "ep-1", "openai-comp", "http://127.0.0.1:8787", "glm-5.2");
    seed_binding(&env.conn, "opencode-desktop", "ep-1");
    seed_endpoint_models(
        &env.conn,
        "ep-1",
        r#"{"available":["glm-4.7","glm-5.2"],"default":"glm-5.2"}"#,
    );

    let mut ctx = TaskContext::new_task("opencode-desktop", None);
    ctx.protocol_hint = Some(ProviderKind::Openai);
    assert_eq!(resolve(&ctx, &env.inputs()).unwrap().model, "glm-5.2");

    // Flip the default to a model already in the catalog — NO rebuild.
    set_models_no_rebuild(
        &env.conn,
        "ep-1",
        r#"{"available":["glm-4.7","glm-5.2"],"default":"glm-4.7"}"#,
    );
    // A NEW task (fresh task_id) — the old task's affinity would reuse the
    // prior route by design.
    let mut ctx2 = TaskContext::new_task("opencode-desktop", None);
    ctx2.protocol_hint = Some(ProviderKind::Openai);
    assert_eq!(resolve(&ctx2, &env.inputs()).unwrap().model, "glm-4.7");
}

/// The upstream base_url follows the inbound direction: an OpenAI-shape
/// request picks the endpoint's `openai` protocol row, an Anthropic one
/// the `anthropic` row — not blindly the first row.
#[test]
fn protocol_hint_picks_matching_endpoint_row() {
    let env = TestEnv::new();
    seed_endpoint(&env.conn, "z-ai", "anthropic", "https://api.z.ai/api/anthropic", "glm-5.2");
    add_protocol_row(&env.conn, "z-ai", "openai-comp", "https://api.z.ai/api/paas/v4");
    seed_binding(&env.conn, "opencode-desktop", "z-ai");
    seed_endpoint_models(&env.conn, "z-ai", r#"{"available":["glm-5.2"],"default":"glm-5.2"}"#);

    let mut openai_ctx = TaskContext::new_task("opencode-desktop", None);
    openai_ctx.protocol_hint = Some(ProviderKind::Openai);
    assert_eq!(
        resolve(&openai_ctx, &env.inputs()).unwrap().base_url,
        "https://api.z.ai/api/paas/v4"
    );

    let mut anthropic_ctx = TaskContext::new_task("opencode-desktop", None);
    anthropic_ctx.protocol_hint = Some(ProviderKind::Anthropic);
    assert_eq!(
        resolve(&anthropic_ctx, &env.inputs()).unwrap().base_url,
        "https://api.z.ai/api/anthropic"
    );

    // No hint → historical first-row behavior.
    let bare_ctx = TaskContext::new_task("opencode-desktop", None);
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
    seed_binding(&env.conn, "claude-code-cli", "opencode-go");
    seed_binding(&env.conn, "opencode-desktop", "opencode-go");
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
    seed_binding(&env.conn, "claude-code-cli", "deepseek");
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
    seed_binding(&env.conn, "pi-cli", "mock-a");
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
/// the Responses wire — the chat wire is broken upstream for it (503).
#[test]
fn wire_responses_class_routes_to_responses_api() {
    let env = TestEnv::new();
    seed_endpoint(&env.conn, "opencode-go", "anthropic", "https://opencode.ai/zen/go/v1", "grok-4.5");
    add_protocol_row(&env.conn, "opencode-go", "openai-comp", "https://opencode.ai/zen/go/v1");
    seed_binding(&env.conn, "claude-code-cli", "opencode-go");
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
    seed_binding(&env.conn, "claude-code-cli", "opencode-go");
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

/// A chat inbound only switches wire for responses-class models;
/// anthropic-class and unknown models stay on Chat (they accept it).
#[test]
fn wire_chat_inbound_switches_only_for_responses_class() {
    let env = TestEnv::new();
    seed_endpoint(&env.conn, "opencode-go", "anthropic", "https://opencode.ai/zen/go/v1", "deepseek-v4-flash");
    add_protocol_row(&env.conn, "opencode-go", "openai-comp", "https://opencode.ai/zen/go/v1");
    seed_binding(&env.conn, "opencode-desktop", "opencode-go");
    seed_endpoint_models(
        &env.conn,
        "opencode-go",
        r#"{"available":["deepseek-v4-flash","grok-4.5"],"default":"deepseek-v4-flash"}"#,
    );

    // deepseek-v4-flash: anthropic-class (corrections) → stays Chat.
    let mut ds_ctx = TaskContext::new_task("opencode-desktop", None);
    ds_ctx.protocol_hint = Some(ProviderKind::Openai);
    ds_ctx.requested_model = Some("deepseek-v4-flash".into());
    ds_ctx.requested_provider = Some("opencode-go".into());
    let route = resolve(&ds_ctx, &env.inputs()).unwrap();
    assert_eq!(route.protocol, ProviderKind::Openai);

    // grok-4.5: responses-class → Chat inbound also switches to Responses.
    set_catalog_api(&env.conn, "opencode-go", "grok-4.5", "response-api");
    let mut gr_ctx = TaskContext::new_task("opencode-desktop", None);
    gr_ctx.protocol_hint = Some(ProviderKind::Openai);
    gr_ctx.requested_model = Some("grok-4.5".into());
    gr_ctx.requested_provider = Some("opencode-go".into());
    let route = resolve(&gr_ctx, &env.inputs()).unwrap();
    assert_eq!(route.protocol, ProviderKind::Responses);
}

/// Unknown api (no corrections/overrides) follows the row protocol —
/// historical behavior preserved.
#[test]
fn wire_unknown_api_follows_row() {
    let env = TestEnv::new();
    seed_endpoint(&env.conn, "ep-1", "anthropic", "https://api.example.com", "m-1");
    add_protocol_row(&env.conn, "ep-1", "openai-comp", "https://api.example.com/v1");
    seed_binding(&env.conn, "claude-code-cli", "ep-1");
    seed_endpoint_models(&env.conn, "ep-1", r#"{"available":["m-1"],"default":"m-1"}"#);

    // Distinct base_urls → direction-matched anthropic row → Anthropic wire.
    let mut a_ctx = TaskContext::new_task("claude-code-cli", None);
    a_ctx.protocol_hint = Some(ProviderKind::Anthropic);
    let r = resolve(&a_ctx, &env.inputs()).unwrap();
    assert_eq!(r.protocol, ProviderKind::Anthropic);

    let mut o_ctx = TaskContext::new_task("opencode-desktop", None);
    o_ctx.protocol_hint = Some(ProviderKind::Openai);
    seed_binding(&env.conn, "opencode-desktop", "ep-1");
    let r = resolve(&o_ctx, &env.inputs()).unwrap();
    assert_eq!(r.protocol, ProviderKind::Openai);
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
fn capability_req_routes_vision_away_from_text_only_model() {
    let env = TestEnv::new();
    seed_endpoint(&env.conn, "ep-text", "anthropic", "https://t", "m-text");
    seed_endpoint(&env.conn, "ep-vis", "anthropic", "https://v", "m-vis");
    seed_binding(&env.conn, "claude-code-cli", "ep-text");
    seed_binding(&env.conn, "claude-code-cli", "ep-vis");
    // ep-text's model reports text-only input; ep-vis's reports text+image.
    for (id, abilities) in [
        ("ep-text", r#"{"m-text":{"modalities":{"input":["text"]}}}"#),
        ("ep-vis", r#"{"m-vis":{"modalities":{"input":["text","image"]}}}"#),
    ] {
        env.conn
            .execute(
                "UPDATE provider_endpoint SET model_abilities_json = ?2 WHERE id = ?1",
                rusqlite::params![id, abilities],
            )
            .unwrap();
    }
    capability_registry::rebuild(&env.conn).unwrap();

    // An image-bearing request derives vision=true and must not resolve
    // onto the text-only model (Smart Gateway fix 2 activating the
    // previously inert capability stage).
    let body = br#"{"model":"m-text","messages":[{"role":"user","content":[
             {"type":"text","text":"see"},{"type":"image","source":{"data":"x"}}]}]}"#;
    let mut ctx = TaskContext::new_task("claude-code-cli", None);
    ctx.required_capabilities =
        capability_registry::derive_capability_req(body, ProviderKind::Anthropic);
    assert!(ctx.required_capabilities.vision);
    let r = resolve(&ctx, &env.inputs()).unwrap();
    assert_eq!(r.endpoint_id, "ep-vis", "vision request excludes the text-only model");

    // A text-only request stays eligible for the text-only model.
    let mut ctx2 = TaskContext::new_task("claude-code-cli", None);
    ctx2.requested_model = Some("m-text".into());
    let r2 = resolve(&ctx2, &env.inputs()).unwrap();
    assert_eq!(r2.model, "m-text");
}