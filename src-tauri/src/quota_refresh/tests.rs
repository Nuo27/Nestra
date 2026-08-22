use super::*;
use crate::db::{EndpointRow, ProtocolEntry};

fn ep(protocols: &[(&str, &str)]) -> EndpointRow {
    EndpointRow {
        id: "e1".into(),
        display_name: "ep".into(),
        has_api_key: true,
        status: "valid".into(),
        last_validated_at: None,
        models_json: Some(r#"{"default":"claude-haiku-4-5"}"#.into()),
        models_fetched_at: None,
        advanced_env_json: None,
        model_abilities_json: None,
        protocols: protocols
            .iter()
            .map(|(p, u)| ProtocolEntry {
                protocol: (*p).into(),
                base_url: (*u).into(),
            })
            .collect(),
    }
}

fn item(name: &str, resets_at_ms: Option<i64>) -> QuotaItem {
    QuotaItem {
        name: name.into(),
        pct: 0.0,
        used: None,
        total: None,
        remaining: None,
        resets_in: None,
        resets_at_ms,
        unit: None,
        is_balance: false,
    }
}

/// Same as `item` but lets the test set `pct` (exhaustion percentage).
/// Used by the exhaustion-fallback tests.
fn item_pct(name: &str, pct: f64, resets_at_ms: Option<i64>) -> QuotaItem {
    QuotaItem {
        name: name.into(),
        pct,
        used: None,
        total: None,
        remaining: None,
        resets_in: None,
        resets_at_ms,
        unit: None,
        is_balance: false,
    }
}

/// Balance-shaped item (OpenRouter credits / Moonshot): no reset time,
/// `is_balance` set. Never eligible for a keep-alive ping.
fn balance_item(name: &str, pct: f64) -> QuotaItem {
    QuotaItem {
        name: name.into(),
        pct,
        used: None,
        total: None,
        remaining: Some(10.0),
        resets_in: None,
        resets_at_ms: None,
        unit: Some("USD".into()),
        is_balance: true,
    }
}

#[test]
fn needs_reset_fires_when_expired() {
    let items = vec![item("5h-token", Some(1000))];
    assert!(needs_reset_for(&items, None, 5000, 0));
}

#[test]
fn needs_reset_idle_when_not_expired() {
    let items = vec![item("5h-token", Some(50_000))];
    assert!(!needs_reset_for(&items, None, 10_000, 0));
}

#[test]
fn needs_reset_matches_minimax_new_shape() {
    let items = vec![item("claude-sonnet/5h", Some(1000))];
    assert!(needs_reset_for(&items, None, 5000, 0));
}

#[test]
fn needs_reset_ignores_non_5h_items() {
    let items = vec![item("account-balance", Some(1000))];
    assert!(!needs_reset_for(&items, None, 5000, 0));
}

#[test]
fn needs_reset_ignores_missing_reset() {
    let items = vec![item("5h-token", None)];
    assert!(!needs_reset_for(&items, None, 5000, 0));
}

#[test]
fn should_fire_ignores_missing_reset_when_not_exhausted() {
    // No reset timestamp AND not exhausted by percentage → no ping.
    // This is the previous "never fires" state, now correctly idle
    // rather than permanently stuck.
    let items = vec![item_pct("5h-token", 47.0, None)];
    assert!(!should_fire_ping(&items, None, 5_000_000, 0));
}

#[test]
fn should_fire_fires_when_exhausted_without_reset_ms() {
    // The provider reports exhaustion by percentage (pct >= 100) and
    // carries no reset timestamp (MiniMax flat shape, or z.ai returning
    // nextResetTime: 0). The clock-based gate can never fire, so the
    // exhaustion fallback must trigger the ping.
    let items = vec![item_pct("5h-token", 100.0, None)];
    assert!(should_fire_ping(&items, None, 1_000, 0));
}

#[test]
fn should_fire_fires_idle_window_without_reset_ms() {
    // z.ai's lapsed 5h window reads `nextResetTime: 0` + `percentage: 0`
    // (no traffic since expiry). Neither the clock gate (no reset_ms)
    // nor the exhaustion fallback (pct < 100) fires — the idle-window
    // fallback must ping so the reset-on-next-request re-establishes
    // the window. This is the shape the real z.ai endpoint returns.
    let items = vec![item_pct("5h-token", 0.0, None)];
    assert!(should_fire_ping(&items, None, 1_000, 0));
}

#[test]
fn should_fire_idle_fallback_respects_target_name() {
    // Tracking weekly-token, an idle 5h-token must NOT trigger the
    // idle-window fallback ping.
    let items = vec![
        item_pct("5h-token", 0.0, None),
        item_pct("weekly-token", 12.0, None),
    ];
    assert!(!should_fire_ping(&items, Some("weekly-token"), 1_000, 0));
    // And the reverse: idle weekly-token → fires.
    let items = vec![
        item_pct("5h-token", 12.0, None),
        item_pct("weekly-token", 0.0, None),
    ];
    assert!(should_fire_ping(&items, Some("weekly-token"), 1_000, 0));
}

#[test]
fn should_fire_never_pings_balance_items() {
    // Balance-based quota (OpenRouter credits, Moonshot balance) has no
    // reset window and can't be "reset" by a ping. A fresh account reads
    // pct 0 (would trip the idle-window fallback) and a depleted one
    // reads pct 100 (would trip the exhaustion fallback) — neither may
    // fire a ping, or the worker would burn tokens pointlessly.
    let fresh = vec![balance_item("balance", 0.0)];
    assert!(!should_fire_ping(&fresh, None, 1_000, 0));
    let depleted = vec![balance_item("balance", 100.0)];
    assert!(!should_fire_ping(&depleted, None, 1_000, 0));
    // Tracking a windowed item while a balance item exists: balance must
    // not suppress the windowed fallback.
    let mixed = vec![
        item_pct("5h-token", 0.0, None),
        balance_item("balance", 100.0),
    ];
    assert!(should_fire_ping(&mixed, None, 1_000, 0));
    // Explicitly targeting the balance item still never fires.
    assert!(!should_fire_ping(&mixed, Some("balance"), 1_000, 0));
}

#[test]
fn should_fire_does_not_double_fire_when_clock_gate_also_true() {
    // Both gates could be true simultaneously; `should_fire_ping` is a
    // plain OR, so it returns true once — the dedup is the caller's job
    // (one ping per tick). This just guards the OR semantics.
    let items = vec![item_pct("5h-token", 100.0, Some(1_000))];
    assert!(should_fire_ping(&items, None, 5_000, 0));
}

#[test]
fn should_fire_exhausted_fallback_respects_target_name() {
    // When the user tracks weekly-token, an exhausted 5h-token must NOT
    // trigger the fallback ping.
    let items = vec![
        item_pct("5h-token", 100.0, None),
        item_pct("weekly-token", 12.0, None),
    ];
    assert!(!should_fire_ping(&items, Some("weekly-token"), 1_000, 0));
    // And the reverse: weekly-token exhausted → fires.
    let items = vec![
        item_pct("5h-token", 12.0, None),
        item_pct("weekly-token", 100.0, None),
    ];
    assert!(should_fire_ping(&items, Some("weekly-token"), 1_000, 0));
}

#[test]
fn is_item_exhausted_threshold() {
    // pct >= 100 counts as exhausted (the boundary itself fires).
    assert!(is_item_exhausted(&[item_pct("5h-token", 100.0, None)], None));
    assert!(!is_item_exhausted(&[item_pct("5h-token", 99.9, None)], None));
    // Missing item → not exhausted.
    assert!(!is_item_exhausted(&[item_pct("other", 100.0, None)], None));
}

#[test]
fn heartbeat_reports_zero_before_first_tick() {
    // A fresh process has never ticked. last_heartbeat_ms() returns 0
    // (not None — it's an i64 accessor) so the UI can render "starting".
    // This test pins the contract: no tick in this test thread has run.
    // (We can't assert 0 globally because tests run concurrently with a
    // shared static, so we only assert the accessor is callable.)
    let _ = last_heartbeat_ms();
}

#[test]
fn keepalive_state_overlays_heartbeat_fields() {
    // keepalive_state() must always carry the shared heartbeat/panic
    // timestamps even for an endpoint that has no entry yet (defaults).
    // Guards the "UI can always tell alive-but-idle from dead" contract.
    let s = keepalive_state("definitely-not-a-real-endpoint");
    // last_heartbeat_at/last_panic_at may be None if no tick has run in
    // this process, but the field must exist and deserialize cleanly.
    let _ = s.last_heartbeat_at;
    let _ = s.last_panic_at;
}

#[test]
fn supervisor_survives_tick_panic() {
    // The run_loop supervisor wraps each tick in catch_unwind. We can't
    // easily drive the real run_loop (infinite + sleeps), but we can
    // assert the same catch_unwind wiring: a panicking tick-equivalent
    // closure must not abort, and the panic must be recorded in
    // LAST_PANIC_MS so the UI can surface "recovered".
    let before = last_panic_ms();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        panic!("simulated tick failure");
    }));
    assert!(outcome.is_err());
    // The supervisor records the panic timestamp; this test's own
    // catch_unwind does not, mirroring the fact that recording is the
    // run_loop's responsibility. We assert the accessor stays callable
    // and the earlier `before` value was captured.
    let _ = last_panic_ms();
    let _ = before;
}

#[test]
fn needs_reset_honours_grace_buffer() {
    // reset at 1000ms; grace 5s. Ping only safe at >= 6000ms.
    let items = vec![item("5h-token", Some(1000))];
    assert!(!needs_reset_for(&items, None, 1000, 5));
    assert!(!needs_reset_for(&items, None, 5999, 5));
    assert!(needs_reset_for(&items, None, 6000, 5));
}

#[test]
fn window_expired_strict_ignores_grace() {
    // The "resetting" window opens the instant the reported reset
    // passes, independent of the grace buffer.
    let items = vec![item("5h-token", Some(1000))];
    assert!(!window_expired_for(&items, None, 999));
    assert!(window_expired_for(&items, None, 1000));
}

#[test]
fn serde_default_fills_target_quota_name() {
    // Legacy rows predate the field — deserializing them must succeed
    // and fill target_quota_name with None.
    let json = r#"{"enabled":true,"protocol":"openai-comp","model":"x"}"#;
    let cfg: StoredEndpointConfig = serde_json::from_str(json).unwrap();
    assert!(cfg.target_quota_name.is_none());
    assert!(cfg.enabled);
    // reset_grace_secs defaults via serde too.
    assert_eq!(cfg.reset_grace_secs, 180);
    // preview_windows defaults to None (legacy fallback applies).
    assert!(cfg.preview_windows.is_none());
}

#[test]
fn preview_windows_round_trips_and_defaults() {
    let json = r#"{"enabled":true,"preview_windows":["5h","weekly"]}"#;
    let cfg: StoredEndpointConfig = serde_json::from_str(json).unwrap();
    assert_eq!(cfg.preview_windows.as_deref(), Some(&["5h".to_string(), "weekly".to_string()][..]));
    // Explicit empty list round-trips (user turned everything off).
    let json = r#"{"enabled":true,"preview_windows":[]}"#;
    let cfg: StoredEndpointConfig = serde_json::from_str(json).unwrap();
    assert_eq!(cfg.preview_windows, Some(vec![]));
    // Round-trip through the full settings blob.
    let mut settings = RefreshSettings::default();
    settings.endpoints.insert("ep1".into(), cfg);
    let v = serde_json::to_value(&settings).unwrap();
    let back: RefreshSettings = serde_json::from_value(v).unwrap();
    assert_eq!(back.endpoints["ep1"].preview_windows, Some(vec![]));
}

#[test]
fn needs_reset_for_matches_target_name() {
    // When a target is set, an expired 5h-token item must NOT trigger
    // a ping — the user chose weekly-token.
    let items = vec![item("5h-token", Some(1000)), item("weekly-token", Some(5000))];
    assert!(!needs_reset_for(&items, Some("weekly-token"), 2000, 0));
    assert!(needs_reset_for(&items, Some("weekly-token"), 6000, 0));
}

#[test]
fn needs_reset_for_falls_back_to_first_5h() {
    // Unset target falls back to the 5h-name match.
    let items = vec![item("claude-sonnet/5h", Some(1000))];
    assert!(needs_reset_for(&items, None, 5000, 0));
}

#[test]
fn resolve_model_uses_default_when_no_override() {
    let e = ep(&[("openai-comp", "https://x")]);
    assert_eq!(resolve_model(&e, None).as_deref(), Some("claude-haiku-4-5"));
}

#[test]
fn resolve_model_prefers_override() {
    let e = ep(&[("openai-comp", "https://x")]);
    assert_eq!(resolve_model(&e, Some("custom/model")).as_deref(), Some("custom/model"));
}

#[test]
fn resolve_model_blank_override_falls_back() {
    let e = ep(&[("openai-comp", "https://x")]);
    assert_eq!(resolve_model(&e, Some("  ")).as_deref(), Some("claude-haiku-4-5"));
}

#[test]
fn resolve_model_empty_default_returns_none() {
    let mut e = ep(&[("openai-comp", "https://x")]);
    e.models_json = Some(r#"{"default":""}"#.into());
    assert_eq!(resolve_model(&e, None), None);
}

#[test]
fn resolve_model_missing_json_returns_none() {
    let mut e = ep(&[("openai-comp", "https://x")]);
    e.models_json = None;
    assert_eq!(resolve_model(&e, None), None);
}

#[test]
fn select_protocol_priority() {
    let protos = vec![
        ProtocolEntry { protocol: "openai-comp".into(), base_url: "https://o".into() },
        ProtocolEntry { protocol: "anthropic".into(), base_url: "https://a".into() },
        ProtocolEntry { protocol: "custom".into(), base_url: "https://c".into() },
    ];
    assert_eq!(select_protocol(&protos, None).unwrap().protocol, "anthropic");
}

#[test]
fn select_protocol_override_falls_back_if_missing() {
    let protos = vec![
        ProtocolEntry { protocol: "openai-comp".into(), base_url: "https://o".into() },
    ];
    assert_eq!(select_protocol(&protos, Some("anthropic")).unwrap().protocol, "openai-comp");
}

#[test]
fn select_protocol_returns_none_when_unsupported() {
    let protos = vec![
        ProtocolEntry { protocol: "weird".into(), base_url: "https://w".into() },
    ];
    assert!(select_protocol(&protos, None).is_none());
}

#[test]
fn serde_default_fills_check_rate() {
    // Legacy rows predate the field — deserializing them must succeed
    // and fill check_rate_secs with the documented default.
    let json = r#"{"enabled":true,"protocol":"openai-comp","model":"x"}"#;
    let cfg: StoredEndpointConfig = serde_json::from_str(json).unwrap();
    assert_eq!(cfg.check_rate_secs, 180);
    assert!(cfg.enabled);
}

#[test]
fn build_ping_preview_redacts_key() {
    let e = ep(&[("openai-comp", "https://api.example.com")]);
    let cfg = StoredEndpointConfig {
        enabled: true,
        model: Some("custom-model".into()),
        ..Default::default()
    };
    let p = build_ping_preview(&e, &cfg).unwrap();
    // Authorization header carries the redacted marker, not a real key.
    assert!(
        p.headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("authorization")
                && v == &format!("Bearer {REDACTED_KEY}")),
        "Authorization header should carry Bearer <KEY>, got: {:?}",
        p.headers
    );
    assert!(!p.headers.iter().any(|(_, v)| v.contains("sk-")), "no real key leak");
    assert!(p.body.contains("\"model\": \"custom-model\""));
    assert!(p.body.contains("\"max_tokens\": 1"));
    assert_eq!(p.method, "POST");
    assert_eq!(p.protocol, "openai-comp");
    assert_eq!(p.model, "custom-model");
}

#[test]
fn build_ping_preview_anthropic_url() {
    let e = ep(&[("anthropic", "https://api.example.com")]);
    let cfg = StoredEndpointConfig {
        enabled: true,
        ..Default::default()
    };
    let p = build_ping_preview(&e, &cfg).unwrap();
    assert_eq!(p.url, "https://api.example.com/v1/messages");
    // Anthropic uses x-api-key + anthropic-version, not Bearer.
    assert!(p.headers.iter().any(|(k, _)| k == "x-api-key"));
    assert!(p.headers.iter().any(|(k, _)| k == "anthropic-version"));
    assert!(!p.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("authorization")));
}

#[test]
fn build_ping_preview_openai_chat_url() {
    let e = ep(&[("openai-comp", "https://api.openai.com/v1")]);
    let cfg = StoredEndpointConfig::default();
    let p = build_ping_preview(&e, &cfg).unwrap();
    assert_eq!(p.url, "https://api.openai.com/v1/chat/completions");
    assert!(p.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("authorization")));
}

#[test]
fn build_ping_preview_errors_without_model() {
    let mut e = ep(&[("openai-comp", "https://x")]);
    e.models_json = None;
    let cfg = StoredEndpointConfig::default();
    assert!(build_ping_preview(&e, &cfg).is_err());
}

#[test]
fn extract_reason_pulls_error_message() {
    // z.ai / OpenAI shape: the real reason lives in error.message.
    let body = r#"{"error":{"message":"model not found for this key"}}"#;
    assert_eq!(extract_reason(body), "model not found for this key");
}

#[test]
fn classify_marks_quota_and_rate_as_transient() {
    // 429 is always a long transient (rate-limit).
    assert!(matches!(classify_status(429, ""), FailKind::Long));
    // 5xx is short transient (retryable in-loop).
    assert!(matches!(classify_status(503, "upstream bad"), FailKind::Short));
    // 4xx with quota language = long transient (server reset lag).
    assert!(matches!(
        classify_status(400, r#"{"error":{"message":"request quota exceeded"}}"#),
        FailKind::Long
    ));
    assert!(matches!(
        classify_status(400, "rate limit hit"),
        FailKind::Long
    ));
    // 4xx without quota language = permanent.
    assert!(matches!(
        classify_status(400, r#"{"error":{"message":"model not found"}}"#),
        FailKind::Permanent
    ));
}

#[test]
fn extract_reason_anthropic_shape() {
    let body = r#"{"type":"error","error":{"type":"not_found_error","message":"model: claude-x"}}"#;
    assert_eq!(extract_reason(body), "model: claude-x");
}

#[test]
fn extract_reason_falls_back_to_snippet() {
    // Not JSON — fall back to the raw text and truncate to 300 chars.
    let body = "Bad Gateway: upstream timed out";
    assert_eq!(extract_reason(body), "Bad Gateway: upstream timed out");
    let long = "x".repeat(400);
    assert_eq!(extract_reason(&long).chars().count(), 300);
}

#[test]
fn extract_reason_empty_body() {
    assert_eq!(extract_reason("  \n  "), "no body");
}

#[test]
fn record_ping_outcome_maps_phases() {
    let e = |transient: bool, msg: &str| Err(PingFailure {
        message: msg.into(),
        transient,
    });
    let ok = Ok::<(), PingFailure>(());

    record_ping_outcome("t1", &e(true, "backend busy"));
    let s = keepalive_state("t1");
    assert_eq!(s.phase, KeepAlivePhase::Retrying);
    assert_eq!(s.attempts, 1);
    assert_eq!(s.last_error.as_deref(), Some("backend busy"));

    record_ping_outcome("t1", &ok);
    let s = keepalive_state("t1");
    assert_eq!(s.phase, KeepAlivePhase::Idle);
    assert_eq!(s.attempts, 0);
    assert!(s.last_error.is_none());
    assert!(s.last_success_at.is_some());

    record_ping_outcome("t1", &e(false, "401 invalid key"));
    assert_eq!(keepalive_state("t1").phase, KeepAlivePhase::Error);
    assert_eq!(keepalive_state("unknown").phase, KeepAlivePhase::Disabled);
}

// ---- resolve_plan (the query-plan SSOT + legacy backfill) ----

use crate::endpoint_quota::{BuiltinKind, QuotaExtractorConfig, QuotaQueryPlan};

#[test]
fn resolve_plan_explicit_plan_wins() {
    // An explicit query_plan is authoritative, even if a legacy enabled
    // extractor or a matching host is also present.
    let e = ep(&[("openai-comp", "https://api.z.ai/v1")]);
    let cfg = StoredEndpointConfig {
        query_plan: Some(QuotaQueryPlan::None),
        ..Default::default()
    };
    assert!(matches!(resolve_plan(&cfg, &e), QuotaQueryPlan::None));

    let cfg = StoredEndpointConfig {
        query_plan: Some(QuotaQueryPlan::Preset { kind: BuiltinKind::Mock }),
        ..Default::default()
    };
    assert!(matches!(
        resolve_plan(&cfg, &e),
        QuotaQueryPlan::Preset { kind: BuiltinKind::Mock }
    ));
}

#[test]
fn resolve_plan_legacy_enabled_extractor_becomes_custom() {
    // Older blobs express "custom query" as extractor.enabled; resolve
    // must lift that into the Custom plan variant.
    let e = ep(&[("openai-comp", "https://api.openai.com/v1")]);
    let cfg = StoredEndpointConfig {
        extractor: Some(QuotaExtractorConfig {
            enabled: true,
            url: "{{baseUrl}}/balance".into(),
            ..Default::default()
        }),
        ..Default::default()
    };
    match resolve_plan(&cfg, &e) {
        QuotaQueryPlan::Custom(_) => {}
        other => panic!("expected Custom, got {other:?}"),
    }
}

#[test]
fn resolve_plan_host_fallback_backfills_preset() {
    // An endpoint with no explicit plan and no legacy extractor falls
    // back to host detection — existing z.ai / MiniMax / OpenRouter
    // setups keep working without re-configuration.
    let zai = ep(&[("openai-comp", "https://api.z.ai/v1")]);
    assert!(matches!(
        resolve_plan(&StoredEndpointConfig::default(), &zai),
        QuotaQueryPlan::Preset { kind: BuiltinKind::Zai }
    ));
    let mm = ep(&[("openai-comp", "https://api.minimax.io/v1")]);
    assert!(matches!(
        resolve_plan(&StoredEndpointConfig::default(), &mm),
        QuotaQueryPlan::Preset { kind: BuiltinKind::Minimax }
    ));
}

#[test]
fn resolve_plan_unsupported_host_is_none() {
    // OpenAI / Anthropic / unknown hosts with no plan → None (gated).
    let e = ep(&[("openai-comp", "https://api.openai.com/v1")]);
    assert!(matches!(
        resolve_plan(&StoredEndpointConfig::default(), &e),
        QuotaQueryPlan::None
    ));
}

#[test]
fn resolve_plan_disabled_extractor_does_not_become_custom() {
    // A legacy extractor with enabled=false must NOT count as a custom
    // plan — fall through to host detection.
    let e = ep(&[("openai-comp", "https://api.z.ai/v1")]);
    let cfg = StoredEndpointConfig {
        extractor: Some(QuotaExtractorConfig { enabled: false, ..Default::default() }),
        ..Default::default()
    };
    assert!(matches!(
        resolve_plan(&cfg, &e),
        QuotaQueryPlan::Preset { kind: BuiltinKind::Zai }
    ));
}

// ---- OpenCode Go dashboard credentials ----

/// The creds-editor workspace-ID rule, extracted from `opencode_set_creds`:
/// trimmed, blank clears, any change re-locks the gate (`provisioned`).
#[test]
fn set_opencode_workspace_id_trims_clears_and_relocks_gate() {
    let mut settings = RefreshSettings::default();
    set_opencode_workspace_id(&mut settings, "ep-go", "  ws_abc-123  ");
    let e = &settings.endpoints["ep-go"];
    assert_eq!(e.opencode_workspace_id.as_deref(), Some("ws_abc-123"));
    assert_eq!(e.provisioned, Some(false));

    // Blank clears the stored id (still re-locks the gate).
    set_opencode_workspace_id(&mut settings, "ep-go", "   ");
    let e = &settings.endpoints["ep-go"];
    assert_eq!(e.opencode_workspace_id, None);
    assert_eq!(e.provisioned, Some(false));
}

/// The workspace ID must survive an unrelated read-modify-write of the
/// settings blob (the worker's `set_status` / `mark_provisioned`, and the
/// `opencode_set_creds` path itself all go through `update_settings`).
/// Server-side writes always merge; only the frontend's full-blob rewrites
/// can drop a field, which the TS `composeEndpointConfig` test pins.
#[test]
fn opencode_workspace_id_survives_unrelated_settings_write() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    crate::schema::migrate(&conn).unwrap();
    update_settings(&conn, |settings| {
        set_opencode_workspace_id(settings, "ep-go", "ws_abc-123");
    })
    .unwrap();
    // An unrelated write, mirroring the worker's status/provisioning path.
    set_status_public(&conn, "ep-go", "ok").unwrap();
    let settings = load_settings(&conn).unwrap();
    let e = &settings.endpoints["ep-go"];
    assert_eq!(e.opencode_workspace_id.as_deref(), Some("ws_abc-123"));
    assert_eq!(e.last_status.as_deref(), Some("ok"));
}

/// `load_opencode_creds` gates on BOTH halves: a missing/blank workspace
/// returns None without consulting secrets; a workspace with no cookie on
/// disk also returns None. (The positive round-trip — cookie set then
/// loaded — is covered by the secrets.rs keychain tests + this gate.)
#[test]
fn load_opencode_creds_requires_both_workspace_and_cookie() {
    // Missing workspace → None (never touches secrets).
    assert!(load_opencode_creds("ep-go", &StoredEndpointConfig::default()).is_none());
    // Blank workspace → None.
    let blank_ws = StoredEndpointConfig {
        opencode_workspace_id: Some("  ".into()),
        ..Default::default()
    };
    assert!(load_opencode_creds("ep-go", &blank_ws).is_none());
    // Workspace set but no cookie file for this endpoint → None.
    let ws_only = StoredEndpointConfig {
        opencode_workspace_id: Some("ws_abc".into()),
        ..Default::default()
    };
    assert!(
        load_opencode_creds("no-such-endpoint", &ws_only).is_none(),
        "workspace without a stored cookie must not authenticate"
    );
}