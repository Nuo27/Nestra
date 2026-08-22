use super::*;

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

fn balance_item(name: &str, pct: f64, remaining: f64, total: f64) -> QuotaItem {
    QuotaItem {
        name: name.into(),
        pct,
        used: Some(total - remaining),
        total: Some(total),
        remaining: Some(remaining),
        resets_in: None,
        resets_at_ms: None,
        unit: Some("USD".into()),
        is_balance: true,
    }
}

fn ep(protocols: &[(&str, &str)]) -> EndpointRow {
    EndpointRow {
        id: "ep-test".into(),
        display_name: "Test".into(),
        has_api_key: true,
        status: "valid".into(),
        last_validated_at: None,
        models_json: None,
        models_fetched_at: None,
        advanced_env_json: None,
        model_abilities_json: None,
        protocols: protocols
            .iter()
            .map(|(p, u)| ProtocolEntry { protocol: (*p).into(), base_url: (*u).into() })
            .collect(),
    }
}

#[test]
fn provider_kind_detects_local_mock() {
    assert!(matches!(provider_kind_for("http://127.0.0.1:8787"), Some(BuiltinKind::Mock)));
    assert!(matches!(provider_kind_for("http://localhost:8788"), Some(BuiltinKind::Mock)));
    assert!(matches!(provider_kind_for("https://api.z.ai/api/anthropic"), Some(BuiltinKind::Zai)));
    assert!(matches!(provider_kind_for("https://openrouter.ai/api/v1"), Some(BuiltinKind::Openrouter)));
    assert!(matches!(provider_kind_for("https://opencode.ai/zen/go/v1"), Some(BuiltinKind::OpencodeGo)));
    assert!(matches!(provider_kind_for("https://api.openai.com/v1"), None));
}

#[test]
fn fetch_with_plan_none_returns_unconfigured() {
    let endpoint = ep(&[("openai-comp", "https://api.z.ai/v1")]);
    let q = fetch_with_plan(&endpoint, "k", &QuotaQueryPlan::None, None);
    assert!(!q.ok);
    assert!(q.error.as_deref().unwrap().contains("no query plan"));
    assert!(q.items.is_empty());
}

#[test]
fn fetch_with_plan_custom_overrides_host() {
    // A z.ai endpoint whose plan is explicitly Custom must NOT use the
    // built-in z.ai fetcher — the user's plan wins.
    let endpoint = ep(&[("openai-comp", "https://api.z.ai/v1")]);
    let cfg = QuotaExtractorConfig {
        enabled: true,
        url: "{{baseUrl}}/balance".into(),
        headers: std::collections::HashMap::new(),
        unit: Some("USD".into()),
        fields: ExtractorFields {
            name: None,
            used: Some("data.usage".into()),
            remaining: Some("data.remaining".into()),
            total: Some("data.total".into()),
            unit: None,
        },
    };
    // Can't hit the network in a unit test, but we can assert the plan
    // dispatches to the custom parser (not the z.ai fetcher) by parsing
    // a synthetic payload through the same path fetch_with_plan uses.
    let q = parse_custom_payload(
        &endpoint,
        &cfg,
        &serde_json::json!({ "data": { "usage": 10, "remaining": 90, "total": 100 } }),
    );
    assert!(q.ok);
    assert_eq!(q.plan.as_deref(), Some("balance"));
    assert!(q.items[0].is_balance);
}

#[test]
fn plan_is_active_only_when_configured() {
    assert!(!QuotaQueryPlan::None.is_active());
    assert!(QuotaQueryPlan::Preset { kind: BuiltinKind::Zai }.is_active());
    assert!(QuotaQueryPlan::Custom(QuotaExtractorConfig::default()).is_active());
}

#[test]
fn plan_round_trips_through_serde() {
    // The tag = "source" representation must round-trip so the setting_kv
    // blob deserializes cleanly across the TS boundary.
    let cases = vec![
        serde_json::json!({ "source": "none" }),
        serde_json::json!({ "source": "preset", "kind": "zai" }),
        serde_json::json!({ "source": "preset", "kind": "minimax" }),
        serde_json::json!({ "source": "preset", "kind": "openrouter" }),
        serde_json::json!({ "source": "preset", "kind": "mock" }),
        serde_json::json!({
            "source": "custom",
            "enabled": true,
            "url": "{{baseUrl}}/x",
            "headers": {},
            "unit": null,
            "fields": {}
        }),
    ];
    for v in cases {
        let plan: QuotaQueryPlan = serde_json::from_value(v.clone()).unwrap_or_else(|e| panic!("deserialize {v}: {e}"));
        let re = serde_json::to_value(&plan).unwrap_or_else(|e| panic!("serialize: {e}"));
        let again: QuotaQueryPlan = serde_json::from_value(re).unwrap();
        assert_eq!(plan.is_active(), again.is_active());
    }
}

#[test]
fn parse_openrouter_limits() {
    // GET /api/v1/key limits shape: usage (all-time), limit +
    // limit_remaining (nullable).
    let payload = serde_json::json!({
        "data": {
            "label": "main",
            "usage": 23.5,
            "limit": 100.0,
            "limit_remaining": 76.5,
            "is_free_tier": false,
        }
    });
    let q = parse_openrouter_payload(&payload);
    assert!(q.ok, "openrouter quota should parse: {:?}", q.error);
    assert_eq!(q.plan.as_deref(), Some("balance"));
    assert_eq!(q.items.len(), 1);
    let it = &q.items[0];
    assert_eq!(it.name, "balance");
    assert!(it.is_balance, "balance item must be flagged is_balance");
    assert_eq!(it.unit.as_deref(), Some("USD"));
    assert_eq!(it.remaining, Some(76.5));
    assert_eq!(it.used, Some(23.5));
    assert_eq!(it.total, Some(100.0));
    // Balance items never report a fill ratio — no percentage.
    assert_eq!(it.pct, 0.0);
    assert_eq!(it.resets_at_ms, None);
    assert_eq!(it.resets_in, None);
}

#[test]
fn parse_openrouter_unlimited_key() {
    // Unlimited keys return limit/limit_remaining = null — pct stays 0
    // and total is absent; the UI shows only the spend, no ratio.
    let payload = serde_json::json!({
        "data": { "usage": 12.0, "limit": null, "limit_remaining": null }
    });
    let q = parse_openrouter_payload(&payload);
    let it = &q.items[0];
    assert_eq!(it.pct, 0.0);
    assert_eq!(it.total, None);
    assert_eq!(it.remaining, None);
    assert_eq!(it.used, Some(12.0));
}

#[test]
fn as_f64_parses_string_numbers_and_money_arrays() {
    // Money-array shape: amount is the first element.
    assert_eq!(as_f64(&serde_json::json!([12.34, "CNY"])), Some(12.34));
    // String numbers — some balance APIs return amounts as strings.
    assert_eq!(as_f64(&serde_json::json!("5.6")), Some(5.6));
    assert_eq!(as_f64(&serde_json::json!("12.34")), Some(12.34));
    // Plain numbers and garbage.
    assert_eq!(as_f64(&serde_json::json!(42.0)), Some(42.0));
    assert_eq!(as_f64(&serde_json::json!("not-a-number")), None);
    assert_eq!(as_f64(&Value::Null), None);
}

#[test]
fn json_path_walks_objects_and_arrays() {
    let v = serde_json::json!({
        "data": { "balance": [12.34, "CNY"], "quota": { "used": "5.6" } }
    });
    assert_eq!(json_path(&v, "data.balance.0"), Some(&serde_json::json!(12.34)));
    assert_eq!(json_path(&v, "data.balance.1"), Some(&serde_json::json!("CNY")));
    assert_eq!(json_path(&v, "data.quota.used"), Some(&serde_json::json!("5.6")));
    // Missing paths and out-of-range indexes.
    assert_eq!(json_path(&v, "data.missing"), None);
    assert_eq!(json_path(&v, "data.balance.9"), None);
    assert_eq!(json_path(&v, ""), None);
}

#[test]
fn fetch_custom_shapes_balance_item() {
    let endpoint = ep(&[("openai-comp", "https://api.example.com/v1")]);
    let cfg = QuotaExtractorConfig {
        enabled: true,
        url: "{{baseUrl}}/users/me/balance".into(),
        headers: std::collections::HashMap::new(),
        unit: Some("CNY".into()),
        fields: ExtractorFields {
            name: Some("data.plan".into()),
            used: Some("data.total_usage.0".into()),
            remaining: Some("data.balance.0".into()),
            total: None,
            unit: Some("data.balance.1".into()),
        },
    };
    let q = parse_custom_payload(
        &endpoint,
        &cfg,
        &serde_json::json!({
            "data": {
                "plan": "pro",
                "total_usage": [1.56, "CNY"],
                "balance": [12.34, "CNY"],
            }
        }),
    );
    assert!(q.ok, "custom quota should parse: {:?}", q.error);
    let it = &q.items[0];
    assert_eq!(it.name, "pro");
    assert!(it.is_balance);
    assert_eq!(it.unit.as_deref(), Some("CNY"));
    assert_eq!(it.remaining, Some(12.34));
    assert_eq!(it.used, Some(1.56));
    // pct = used / (used + remaining)
    let expected = 100.0 * 1.56 / (1.56 + 12.34);
    assert!((it.pct - expected).abs() < 1e-9);
    assert_eq!(it.resets_at_ms, None);
}

#[test]
fn fetch_custom_total_preferred_for_pct_and_defaults() {
    let endpoint = ep(&[("openai-comp", "https://api.example.com/v1")]);
    let cfg = QuotaExtractorConfig {
        enabled: true,
        url: "{{baseUrl}}/balance".into(),
        headers: std::collections::HashMap::new(),
        unit: None,
        fields: ExtractorFields {
            name: None,
            used: Some("data.usage".into()),
            remaining: Some("data.remaining".into()),
            total: Some("data.total".into()),
            unit: None,
        },
    };
    let q = parse_custom_payload(
        &endpoint,
        &cfg,
        &serde_json::json!({ "data": { "usage": 25, "remaining": 75, "total": 100 } }),
    );
    let it = &q.items[0];
    // Name defaults to "balance".
    assert_eq!(it.name, "balance");
    // pct prefers total: used / total.
    assert!((it.pct - 25.0).abs() < 1e-9);
    assert_eq!(it.remaining, Some(75.0));
    // No unit configured → None (UI falls back to bare number).
    assert_eq!(it.unit, None);
}

#[test]
fn fetch_custom_tolerates_missing_fields() {
    let endpoint = ep(&[("openai-comp", "https://api.example.com/v1")]);
    let cfg = QuotaExtractorConfig {
        enabled: true,
        url: "{{baseUrl}}/balance".into(),
        headers: std::collections::HashMap::new(),
        unit: None,
        fields: ExtractorFields::default(),
    };
    // Empty fields → every value None: the payload shape doesn't match
    // the extractor — fail loudly (the old code returned a 0% "balance"
    // that masked real API errors).
    let q = parse_custom_payload(&endpoint, &cfg, &serde_json::json!({ "data": {} }));
    assert!(!q.ok, "missing fields must not report ok");
    assert!(q.error.is_some());
    assert!(q.items.is_empty());
}

#[test]
fn substitute_replaces_placeholders() {
    assert_eq!(substitute("{{baseUrl}}/x?k={{apiKey}}", "https://b", "SECRET"),
        "https://b/x?k=SECRET");
    assert_eq!(substitute("no placeholders", "https://b", "k"), "no placeholders");
}

#[test]
fn parse_mock_payload_flat_shape() {
    let payload = serde_json::json!({
        "plan": "Mock",
        "items": [{
            "name": "Mock 5h window",
            "pct": 100,
            "used": 100,
            "total": 100,
            "remaining": 0,
            "resets_in": "3h",
        }],
    });
    let q = parse_mock_payload(&payload);
    assert!(q.ok, "mock quota should parse: {:?}", q.error);
    assert_eq!(q.plan.as_deref(), Some("Mock"));
    assert_eq!(q.items.len(), 1);
    assert_eq!(q.items[0].pct, 100.0);
    assert_eq!(q.items[0].resets_in.as_deref(), Some("3h"));
}

#[test]
fn parse_mock_payload_rejects_empty_items() {
    let payload = serde_json::json!({ "plan": "Mock", "items": [] });
    let q = parse_mock_payload(&payload);
    assert!(!q.ok);
}

// ---- OpenCode Go dashboard scrape ----

#[test]
fn opencode_parse_ssr_hydration_shape() {
    // SolidJS SSR hydration stream: usagePercent + resetInSec in either
    // order per window. This is the primary dashboard markup shape.
    let html = r#"<script>window._$HY=(e,t,k)=>{};window._$HY.r="1";
        rollingUsage:$R[1]={label:"Rolling Usage",usagePercent:42.5,resetInSec:5400,foo:1}
        weeklyUsage:$R[2]={resetInSec:259200,usagePercent:71.0,label:"Weekly"}
        monthlyUsage:$R[3]={usagePercent:10.0,resetInSec:0,label:"Monthly"}</script>"#;
    let q = parse_opencode_go_html(html, 1_000_000);
    assert!(q.ok, "ssr parse should succeed: {:?}", q.error);
    assert_eq!(q.plan.as_deref(), Some("opencode-go"));
    assert_eq!(q.items.len(), 3);
    // Stable order: 5h, weekly, monthly.
    assert_eq!(q.items[0].name, "5h");
    assert_eq!(q.items[0].pct, 42.5);
    assert_eq!(q.items[0].resets_at_ms, Some(1_000_000 + 5400 * 1000));
    assert!(!q.items[0].is_balance);
    assert_eq!(q.items[1].name, "weekly");
    assert_eq!(q.items[1].pct, 71.0);
    assert_eq!(q.items[2].name, "monthly");
    // resetInSec 0 → no reset timestamp.
    assert_eq!(q.items[2].resets_at_ms, None);
}

#[test]
fn opencode_parse_data_slot_shape() {
    // The newer dashboard markup: discrete data-slot elements per window.
    let html = r#"<main>
          <div data-slot="usage-item"><span data-slot="usage-label">Rolling Usage</span>
            <span data-slot="usage-value">60</span><span data-slot="reset-time">Resets in 2 hours</span></div>
          <div data-slot="usage-item"><span data-slot="usage-label">Weekly</span>
            <span data-slot="usage-value">15.5</span><span data-slot="reset-time">Resets in 1 day 3 hours</span></div>
        </main>"#;
    let q = parse_opencode_go_html(html, 0);
    assert!(q.ok, "data-slot parse should succeed: {:?}", q.error);
    assert_eq!(q.items.len(), 2);
    assert_eq!(q.items[0].name, "5h");
    assert_eq!(q.items[0].pct, 60.0);
    // 2 hours = 7200s.
    assert_eq!(q.items[0].resets_at_ms, Some(7200 * 1000));
    assert_eq!(q.items[1].name, "weekly");
    assert_eq!(q.items[1].pct, 15.5);
    // 1 day 3 hours = 86400 + 10800 = 97200s.
    assert_eq!(q.items[1].resets_at_ms, Some(97200 * 1000));
}

#[test]
fn opencode_parse_resets_now_is_zero_reset() {
    let html = r#"<div data-slot="usage-item"><span data-slot="usage-label">Rolling Usage</span>
            <span data-slot="usage-value">100</span><span data-slot="reset-time">resets now</span></div>"#;
    let q = parse_opencode_go_html(html, 0);
    let it = &q.items[0];
    assert_eq!(it.pct, 100.0);
    // "resets now" → 0s → no reset timestamp.
    assert_eq!(it.resets_at_ms, None);
}

#[test]
fn opencode_parse_no_windows_is_error() {
    // Unrelated HTML (e.g. a login redirect / cookie expired) → no windows.
    let q = parse_opencode_go_html("<html><body>please log in</body></html>", 0);
    assert!(!q.ok);
    assert!(q.error.as_deref().unwrap().contains("no usage windows"));
}

#[test]
fn opencode_workspace_segment_rejects_unsafe_chars() {
    assert_eq!(safe_workspace_segment("ws_abc-123"), Some("ws_abc-123"));
    assert_eq!(safe_workspace_segment("  padded  "), Some("padded"));
    // Path-injection attempts are rejected.
    assert_eq!(safe_workspace_segment("../etc"), None);
    assert_eq!(safe_workspace_segment("a/b"), None);
    assert_eq!(safe_workspace_segment("a?x=1"), None);
    assert_eq!(safe_workspace_segment(""), None);
}

#[test]
fn fetch_with_plan_opencode_without_creds_is_clear_error() {
    let endpoint = ep(&[("openai-comp", "https://opencode.ai/zen/go/v1")]);
    let plan = QuotaQueryPlan::Preset { kind: BuiltinKind::OpencodeGo };
    let q = fetch_with_plan(&endpoint, "k", &plan, None);
    assert!(!q.ok);
    assert!(q.error.as_deref().unwrap().contains("cookie + workspace ID not set"));
}

#[test]
fn pick_5h_zai() {
    let items = vec![
        item("weekly-token", Some(99)),
        item("5h-token", Some(1234)),
    ];
    assert_eq!(pick_five_hour_expiry(&items), Some(1234));
}

#[test]
fn pick_5h_minimax_new_shape() {
    let items = vec![
        item("claude-sonnet/weekly", Some(99)),
        item("claude-sonnet/5h", Some(5678)),
    ];
    assert_eq!(pick_five_hour_expiry(&items), Some(5678));
}

#[test]
fn pick_5h_minimax_flat_no_reset() {
    // Flat shape returns no reset timestamp on the wire; next fetch
    // after a successful POST will repopulate it.
    let items = vec![item("5h-token", None)];
    assert_eq!(pick_five_hour_expiry(&items), None);
}

#[test]
fn pick_5h_returns_none_without_5h_item() {
    // A balance-shaped item (no 5h name) must not match the window picker.
    let items = vec![item("balance", None)];
    assert_eq!(pick_five_hour_expiry(&items), None);
}

#[test]
fn pick_5h_first_match_wins() {
    // New shape sorts alphabetically (claude-haiku/5h before claude-sonnet/5h);
    // pick the first `*/5h` hit. Stable ordering means deterministic test.
    let items = vec![
        item("claude-haiku/5h", Some(11)),
        item("claude-sonnet/5h", Some(22)),
    ];
    assert_eq!(pick_five_hour_expiry(&items), Some(11));
}

#[test]
fn pick_5h_empty() {
    assert_eq!(pick_five_hour_expiry(&[]), None);
}