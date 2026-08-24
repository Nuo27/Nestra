use super::*;

fn ab(reasoning: bool, tool_call: bool, attachment: bool) -> ModelAbilities {
    ModelAbilities {
        reasoning: Some(reasoning),
        tool_call: Some(tool_call),
        attachment: Some(attachment),
        temperature: None,
        limit: Some(ModelLimit { context: 200_000, output: 8_000, input: None }),
        modalities: None,
        api: None,
        cost: None,
    }
}

#[test]
fn normalize_strips_prefix_markers_and_date() {
    assert_eq!(normalize("models/gpt-4o"), "gpt-4o");
    assert_eq!(normalize(" GPT-4o "), "gpt-4o");
    // Bracket content is DROPPED, not kept: the old behavior produced
    // "claude-sonnet-4-51m" which never matched the generated id.
    assert_eq!(normalize("claude-sonnet-4-5[1M]"), "claude-sonnet-4-5");
    assert_eq!(normalize("minimax-m3[1m]"), "minimax-m3");
    assert_eq!(normalize("grok-4(beta)"), "grok-4");
    assert_eq!(normalize("claude-sonnet-4-5-20250929"), "claude-sonnet-4-5");
    assert_eq!(normalize("openai/gpt-4o"), "openai/gpt-4o");
}

#[test]
fn bracket_marker_ids_match_generated_ids() {
    // The generated catalog id (with bracket suffix) must resolve to the
    // same normalized key as the plain id — this is what makes ability
    // routing actually work for bracketed models.
    let idx = build_index(&serde_json::json!({
        "minimax/minimax-m3[1m]": {
            "id": "minimax/minimax-m3[1m]",
            "reasoning": true,
            "tool_call": true,
            "attachment": false,
            "limit": { "context": 200000, "output": 64000 }
        }
    }));
    assert_eq!(normalize("minimax-m3[1m]"), normalize("minimax-m3"));
    let a = abilities_for(&idx, "MiniMax-M3[1M]").expect("bracketed lookup");
    assert_eq!(a.reasoning, Some(true), "reasoning survives the bracket");
    let b = abilities_for(&idx, "minimax-m3").expect("plain lookup");
    assert_eq!(a.limit, b.limit);
}

#[test]
fn partial_limit_overlap_merges_not_conflicts() {
    // Same context/output, but `input` reported by only ONE source —
    // the old whole-struct `!=` treated the None-vs-Some as a conflict
    // and dropped the mergeable data.
    let a = ModelAbilities {
        limit: Some(ModelLimit { context: 128000, output: 16384, input: Some(100000) }),
        ..Default::default()
    };
    let b = ModelAbilities {
        limit: Some(ModelLimit { context: 128000, output: 16384, input: None }),
        ..Default::default()
    };
    assert!(
        !field_conflicts(&a, &b),
        "input None-vs-Some is mergeable, not a conflict"
    );
    let conflict_a = ModelAbilities {
        limit: Some(ModelLimit { context: 128000, output: 0, input: None }),
        ..Default::default()
    };
    let conflict_b = ModelAbilities {
        limit: Some(ModelLimit { context: 64000, output: 0, input: None }),
        ..Default::default()
    };
    assert!(
        field_conflicts(&conflict_a, &conflict_b),
        "same limit field with different values IS a conflict"
    );
}

#[test]
fn build_index_keys_by_normalized_id() {
    let payload = serde_json::json!({
        "openai/gpt-4o": {
            "id": "openai/gpt-4o",
            "reasoning": false,
            "tool_call": true,
            "attachment": true,
            "limit": { "context": 128000, "output": 16384 }
        },
        "anthropic/claude-sonnet-4-5": {
            "id": "anthropic/claude-sonnet-4-5",
            "reasoning": true,
            "tool_call": true,
            "attachment": true
        }
    });
    let idx = build_index(&payload);
    assert!(idx.contains_key("openai/gpt-4o"));
    assert!(idx.contains_key("anthropic/claude-sonnet-4-5"));
}

#[test]
fn abilities_for_exact_match() {
    let payload = serde_json::json!({
        "openai/gpt-4o": { "id": "openai/gpt-4o", "reasoning": false, "tool_call": true, "attachment": true }
    });
    let idx = build_index(&payload);
    let a = abilities_for(&idx, "openai/gpt-4o").unwrap();
    assert_eq!(a.reasoning, Some(false));
    assert_eq!(a.tool_call, Some(true));
}

#[test]
fn abilities_for_tail_segment_matches_bare_id() {
    let payload = serde_json::json!({
        "openai/gpt-4o": { "id": "openai/gpt-4o", "reasoning": false, "tool_call": true, "attachment": true }
    });
    let idx = build_index(&payload);
    // Provider lists the bare id; we match on the tail segment.
    assert!(abilities_for(&idx, "gpt-4o").is_some());
    assert!(abilities_for(&idx, "models/gpt-4o").is_some());

    // Snapshot date (YYYYMMDD) stripped, then tail-segment matches.
    let payload2 = serde_json::json!({
        "anthropic/claude-sonnet-4-5": { "id": "anthropic/claude-sonnet-4-5", "reasoning": true, "tool_call": true, "attachment": true }
    });
    let idx2 = build_index(&payload2);
    assert!(abilities_for(&idx2, "claude-sonnet-4-5-20250929").is_some());
}

#[test]
fn abilities_for_ambiguous_tail_returns_none() {
    // Two distinct labs shipping the same tail segment → ambiguous.
    let payload = serde_json::json!({
        "openai/gemini": { "id": "openai/gemini", "reasoning": true, "tool_call": true, "attachment": true },
        "google/gemini": { "id": "google/gemini", "reasoning": false, "tool_call": false, "attachment": false }
    });
    let idx = build_index(&payload);
    assert!(abilities_for(&idx, "gemini").is_none());
}

#[test]
fn abilities_for_aliased_tail_resolves_when_abilities_match() {
    // Two index entries, same tail, identical abilities → not ambiguous.
    let payload = serde_json::json!({
        "openai/gpt-4o": { "id": "openai/gpt-4o", "reasoning": false, "tool_call": true, "attachment": true },
        "mirror/gpt-4o": { "id": "mirror/gpt-4o", "reasoning": false, "tool_call": true, "attachment": true }
    });
    let idx = build_index(&payload);
    assert!(abilities_for(&idx, "gpt-4o").is_some());
}

#[test]
fn abilities_for_unmatched_returns_none() {
    let idx: HashMap<String, ModelAbilities> = HashMap::new();
    assert!(abilities_for(&idx, "does-not-exist").is_none());
}

#[test]
fn subset_for_dedupes_and_keeps_matches_only() {
    let payload = serde_json::json!({
        "openai/gpt-4o": { "id": "openai/gpt-4o", "reasoning": false, "tool_call": true, "attachment": true }
    });
    let idx = build_index(&payload);
    let ids = vec!["gpt-4o".to_string(), "gpt-4o".to_string(), "nope".to_string()];
    let sub = subset_for(&idx, &ids);
    assert_eq!(sub.len(), 1);
    assert!(sub.contains_key("gpt-4o"));
}

#[test]
fn to_model_entry_fields_emits_only_present_keys() {
    // ab(true, true, false): reasoning + tool_call present, attachment
    // present (false), limit present. temperature absent.
    let a = ab(true, true, false);
    let fields = to_model_entry_fields(&a);
    let keys: Vec<&str> = fields.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, vec!["reasoning", "tool_call", "attachment", "limit"]);

    // Only reasoning present (bool) + attachment (bool false) → two keys.
    let a2 = ModelAbilities {
        reasoning: Some(true),
        tool_call: None,
        attachment: Some(false),
        temperature: None,
        limit: None,
        modalities: None,
        api: None,
        cost: None,
    };
    let f2 = to_model_entry_fields(&a2);
    let keys2: Vec<&str> = f2.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys2, vec!["reasoning", "attachment"]);
}

fn at(context: u64) -> ModelAbilities {
    ModelAbilities {
        reasoning: None,
        tool_call: None,
        attachment: None,
        temperature: None,
        limit: Some(ModelLimit { context, output: 8_000, input: None }),
        modalities: None,
        api: None,
        cost: None,
    }
}

#[test]
fn claude_code_model_id_passes_through_when_abilities_absent() {
    assert_eq!(claude_code_model_id("MiniMax-M3", None), "MiniMax-M3");
}

#[test]
fn claude_code_model_id_passes_through_when_limit_absent() {
    let a = ModelAbilities {
        reasoning: Some(true),
        tool_call: None,
        attachment: None,
        temperature: None,
        limit: None,
        modalities: None,
   api: None,
   cost: None,
    };
    assert_eq!(claude_code_model_id("glm-5.2", Some(&a)), "glm-5.2");
}

#[test]
fn claude_code_model_id_passes_through_below_1m_threshold() {
    // 200_000 is the default Anthropic context — no suffix needed.
    assert_eq!(claude_code_model_id("claude-opus", Some(&at(200_000))), "claude-opus");
    // Just under the threshold — still no suffix.
    assert_eq!(claude_code_model_id("foo", Some(&at(999_999))), "foo");
}

#[test]
fn claude_code_model_id_appends_marker_at_threshold_and_above() {
    assert_eq!(claude_code_model_id("MiniMax-M3", Some(&at(1_000_000))), "MiniMax-M3[1m]");
    assert_eq!(claude_code_model_id("glm-5.2", Some(&at(2_000_000))), "glm-5.2[1m]");
}

#[test]
fn claude_code_model_id_is_idempotent_against_existing_marker() {
    let a = at(1_000_000);
    // Lowercase marker already present.
    assert_eq!(claude_code_model_id("foo[1m]", Some(&a)), "foo[1m]");
    // Uppercase M — case-insensitive match.
    assert_eq!(claude_code_model_id("foo[1M]", Some(&a)), "foo[1M]");
    // Mixed case.
    assert_eq!(claude_code_model_id("foo[1m]", Some(&a)), "foo[1m]");
}

#[test]
fn claude_code_model_id_handles_empty_input() {
    let a = at(1_000_000);
    assert_eq!(claude_code_model_id("", None), "");
    assert_eq!(claude_code_model_id("", Some(&a)), "");
}

fn full(
    reasoning: bool,
    tool_call: bool,
    attachment: bool,
    temperature: bool,
    ctx: u64,
    out: u64,
) -> ModelAbilities {
    ModelAbilities {
        reasoning: Some(reasoning),
        tool_call: Some(tool_call),
        attachment: Some(attachment),
        temperature: Some(temperature),
        limit: Some(ModelLimit { context: ctx, output: out, input: None }),
        modalities: None,
        api: None,
        cost: None,
    }
}

#[test]
fn merge_field_overrides_wins_only_on_set_fields() {
    let def = full(true, true, true, false, 200_000, 8_000);
    let ov = ModelAbilities {
        reasoning: Some(false), // override: flip off
        tool_call: None,        // inherit
        attachment: Some(true), // explicit (matches default — same result)
        temperature: Some(true),
        limit: None,
        modalities: None,
   api: None,
   cost: None,
    };
    let merged = merge_field_overrides(def, ov);
    assert_eq!(merged.reasoning, Some(false));
    assert_eq!(merged.tool_call, Some(true), "tool_call inherits from default");
    assert_eq!(merged.attachment, Some(true));
    assert_eq!(merged.temperature, Some(true));
    assert_eq!(merged.limit.as_ref().unwrap().context, 200_000);
}

#[test]
fn merge_field_overrides_fills_gaps_when_default_is_empty() {
    // No default data for this model id; the override fully populates.
    let def = ModelAbilities {
        reasoning: None,
        tool_call: None,
        attachment: None,
        temperature: None,
        limit: None,
        modalities: None,
   api: None,
   cost: None,
    };
    let ov = full(true, true, true, false, 100_000, 4_000);
    let merged = merge_field_overrides(def, ov);
    assert_eq!(merged.reasoning, Some(true));
    assert_eq!(merged.tool_call, Some(true));
    assert!(merged.limit.is_some());
}

#[test]
fn merge_into_unions_keys_and_resolves_collisions() {
    let mut defaults = HashMap::new();
    defaults.insert("a".into(), full(true, true, true, false, 200_000, 8_000));
    defaults.insert("b-only-default".into(), full(false, true, false, false, 1, 1));

    let mut overrides = HashMap::new();
    // Collision: override flips reasoning off but inherits everything else.
    overrides.insert(
        "a".into(),
        ModelAbilities {
            reasoning: Some(false),
            tool_call: None,
            attachment: None,
            temperature: None,
            limit: None,
            modalities: None,
       api: None,
       cost: None,
    },
    );
    // New model id the cache doesn't know about.
    overrides.insert("custom".into(), full(true, false, false, true, 4096, 1024));

    let out = merge_into(defaults, overrides);
    assert_eq!(out.len(), 3, "default-only key + collision + new key");

    let a = out.get("a").unwrap();
    assert_eq!(a.reasoning, Some(false), "override wins on collision");
    assert_eq!(a.tool_call, Some(true), "default inherits on collision");
    assert_eq!(a.limit.as_ref().unwrap().context, 200_000);

    assert!(out.contains_key("b-only-default"));
    assert!(out.contains_key("custom"));
}

#[test]
fn parse_overrides_handles_none_empty_and_malformed() {
    assert!(parse_overrides(None).is_empty());
    assert!(parse_overrides(Some("")).is_empty());
    assert!(parse_overrides(Some("not json")).is_empty());
    // Non-object root (e.g. an array) is also rejected.
    assert!(parse_overrides(Some("[1,2,3]")).is_empty());
    // A bad row is skipped, a good one survives — partial JSON is best-effort.
    let mixed = r#"{
            "ok": {"reasoning": true},
            "broken": "not an abilities object"
        }"#;
    let parsed = parse_overrides(Some(mixed));
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed.get("ok").unwrap().reasoning, Some(true));
}

#[test]
fn load_corrections_overrides_minimax_m3_context_to_1m() {
    // Vendor-authoritative correction: models.dev reports 512000 for
    // MiniMax-M3, MiniMax's own docs publish 1,000,000. The bundled
    // corrections file must win over the cache value.
    let corrections = load_corrections();
    let a = abilities_for(&corrections, "MiniMax-M3")
        .expect("MiniMax-M3 should match a correction entry");
    assert_eq!(a.limit.as_ref().unwrap().context, 1_000_000, "context must be 1M per MiniMax docs");
    assert_eq!(a.limit.as_ref().unwrap().output, 128_000);
}

#[test]
fn corrections_cover_opencode_go_ox_alpha_free_from_bare_id() {
    // models.dev doesn't list ox-alpha at all and OpenCode Zen's /models
    // is ids-only, so the bundled correction is the only ability source.
    // The endpoint's fetched id is bare ("ox-alpha-free") — it must
    // tail-match the "opencode-go/ox-alpha-free" key. Limit figures are
    // the vendor's own (mirrored on OpenRouter as stealth/ox-alpha).
    // The wire is Chat Completions — the vendor's own docs declare it
    // (`https://opencode.ai/docs/go`: chat/completions +
    // @ai-sdk/openai-compatible). The free model's tool-carrying streams DO
    // intermittently terminate in-band (`finish_reason: "network_error"` on
    // a 200); the gateway's first-event probe turns that into a retryable
    // failure that walks the policy's route-target list rather than a wire
    // override here.
    let corrections = load_corrections();
    let a = abilities_for(&corrections, "ox-alpha-free")
        .expect("bare go id should tail-match the correction entry");
    assert_eq!(a.limit.as_ref().unwrap().context, 1_048_576);
    assert_eq!(a.limit.as_ref().unwrap().output, 131_072);
    assert_eq!(a.api.as_deref(), Some("openai-comp"));
}

#[test]
fn corrections_layer_wins_over_models_dev_for_minimax_m3() {
    // Simulate the production layering: models.dev cache says 512000,
    // corrections file says 1000000, the merge result must be 1000000.
    let mut defaults = HashMap::new();
    defaults.insert(
        "MiniMax-M3".into(),
        ModelAbilities {
            reasoning: Some(true),
            tool_call: Some(true),
            attachment: Some(true),
            temperature: None,
            limit: Some(ModelLimit { context: 512_000, output: 128_000, input: None }),
            modalities: None,
            api: None,
            cost: None,
        },
    );
    let corrections = subset_for(&load_corrections(), &["MiniMax-M3".into()]);
    let merged = merge_into(defaults, corrections);
    let a = merged.get("MiniMax-M3").unwrap();
    assert_eq!(a.limit.as_ref().unwrap().context, 1_000_000, "corrections must override cache");
    // Non-overlapping fields from the cache survive.
    assert_eq!(a.reasoning, Some(true));
    assert_eq!(a.attachment, Some(true));
}

#[test]
fn user_overrides_defeat_corrections_layer() {
    // User override is the highest tier — if the user explicitly sets a
    // value, neither the cache nor the bundled corrections can win.
    let mut defaults = HashMap::new();
    defaults.insert(
        "MiniMax-M3".into(),
        ModelAbilities {
            reasoning: Some(true),
            tool_call: Some(true),
            attachment: Some(true),
            temperature: None,
            limit: Some(ModelLimit { context: 512_000, output: 128_000, input: None }),
            modalities: None,
            api: None,
            cost: None,
        },
    );
    let corrections = subset_for(&load_corrections(), &["MiniMax-M3".into()]);
    let with_corrections = merge_into(defaults, corrections);
    let mut user = HashMap::new();
    user.insert(
        "MiniMax-M3".into(),
        ModelAbilities {
            reasoning: None,
            tool_call: None,
            attachment: None,
            temperature: None,
            limit: Some(ModelLimit { context: 42, output: 13, input: None }),
            modalities: None,
            api: None,
            cost: None,
        },
    );
    let merged = merge_into(with_corrections, user);
    let a = merged.get("MiniMax-M3").unwrap();
    assert_eq!(a.limit.as_ref().unwrap().context, 42, "user override must win");
}

#[test]
fn parse_entry_picks_up_modalities_from_models_dev_payload() {
    // MiniMax-M3 in models.dev carries modalities.input=[text,image,video].
    // parse_entry must surface it (the attachment bool alone is too coarse
    // — video vs image distinction is lost).
    let v = serde_json::json!({
        "reasoning": true,
        "tool_call": true,
        "attachment": true,
        "modalities": { "input": ["text", "image", "video"], "output": ["text"] }
    });
    let a = parse_entry(&v).expect("entry with modalities must parse");
    let mods = a.modalities.expect("modalities must be populated");
    assert_eq!(mods.input, vec![Modality::Text, Modality::Image, Modality::Video]);
    assert_eq!(mods.output, vec![Modality::Text]);
}

#[test]
fn to_model_entry_fields_emits_modalities_in_schema_shape() {
    // The OpenCode schema requires modalities as
    // `{ "input": ["text","image",...], "output": [...] }` — verify the
    // emitter produces exactly that shape, with the lowercase enum
    // tokens the schema enum expects.
    let a = ModelAbilities {
        reasoning: None,
        tool_call: None,
        attachment: None,
        temperature: None,
        limit: None,
        modalities: Some(Modalities {
            input: vec![Modality::Text, Modality::Image, Modality::Video],
            output: vec![Modality::Text],
        }),
        api: None,
        cost: None,
    };
    let fields = to_model_entry_fields(&a);
    let modalities_field = fields
        .iter()
        .find(|(k, _)| k == "modalities")
        .expect("modalities field must be emitted");
    let obj = modalities_field.1.as_object().unwrap();
    let input: Vec<&str> = obj.get("input").unwrap().as_array().unwrap()
        .iter().map(|x| x.as_str().unwrap()).collect();
    let output: Vec<&str> = obj.get("output").unwrap().as_array().unwrap()
        .iter().map(|x| x.as_str().unwrap()).collect();
    assert_eq!(input, vec!["text", "image", "video"]);
    assert_eq!(output, vec!["text"]);
}

#[test]
fn parse_modalities_drops_unknown_tokens_silently() {
    // If models.dev introduces a new modality token (e.g. "3d"), the
    // parser must not fail the whole entry — drop the unknown value.
    let v = serde_json::json!({
        "input": ["text", "unknown-future-modality"],
        "output": ["text"]
    });
    let mods = parse_modalities(&v).expect("at least one valid token survives");
    assert_eq!(mods.input, vec![Modality::Text]);
}
// ---- pi.dev catalog source ----

#[test]
fn pi_index_maps_limits_reasoning_and_api_dialect() {
    let catalog = serde_json::json!({
        "ox-alpha-free": {
            "id": "ox-alpha-free", "name": "Ox Alpha Free", "api": "openai-completions",
            "reasoning": true,
            "contextWindow": 1000000, "maxTokens": 131072,
            "cost": {"input": 0, "output": 0}
        },
        "grok-4.5": {
            "id": "grok-4.5", "api": "openai-responses",
            "contextWindow": 500000, "maxTokens": 500000
        },
        "weird": {"id": "weird", "api": "google-generative"},
        "empty": {"id": "empty", "name": "Nothing"}
    });
    let idx = build_pi_index("opencode-go", &catalog);
    let a = &idx["opencode-go/ox-alpha-free"];
    assert_eq!(a.limit.as_ref().unwrap().context, 1_000_000);
    assert_eq!(a.limit.as_ref().unwrap().output, 131_072);
    assert_eq!(a.reasoning, Some(true));
    assert_eq!(a.api.as_deref(), Some("openai-comp"));
    assert_eq!(idx["opencode-go/grok-4.5"].api.as_deref(), Some("response-api"));
    // Unknown api vocabulary → field unset (not a wrong guess); entries with
    // nothing usable are dropped entirely.
    assert!(!idx.contains_key("opencode-go/weird"), "unknown api + nothing else → dropped");
    assert!(!idx.contains_key("opencode-go/empty"));
}

#[test]
fn pi_layer_sits_between_models_dev_and_corrections() {
    // The full merge ladder for one model: models.dev says 256k, pi.dev says
    // 1M, corrections say the api dialect + 1M — the catalog merge order is
    // models.dev < pi.dev < corrections.
    let mut base = HashMap::new();
    base.insert(
        "ox-alpha-free".into(),
        ModelAbilities { limit: Some(crate::model_abilities::ModelLimit {
            context: 256_000, output: 32_000, input: None,
        }), ..Default::default() },
    );
    let mut pi = HashMap::new();
    pi.insert(
        "opencode-go/ox-alpha-free".into(),
        ModelAbilities { limit: Some(crate::model_abilities::ModelLimit {
            context: 1_000_000, output: 131_072, input: None,
        }), ..Default::default() },
    );
    let corrections = load_corrections();
    let merged = crate::model_abilities::merge_into_tail(
        crate::model_abilities::merge_into_tail(base, pi),
        corrections,
    );
    let a = abilities_for(&merged, "ox-alpha-free").unwrap();
    assert_eq!(a.limit.as_ref().unwrap().context, 1_048_576, "corrections win");
    assert_eq!(a.limit.as_ref().unwrap().output, 131_072);
    assert_eq!(a.api.as_deref(), Some("openai-comp"));
}

#[test]
fn pi_catalog_host_matching() {
    assert_eq!(pi_catalog_for_base_url("https://opencode.ai/zen/go/v1"), Some("opencode-go"));
    assert_eq!(pi_catalog_for_base_url("https://api.minimaxi.com/anthropic"), Some("minimax-cn"));
    assert_eq!(pi_catalog_for_base_url("https://api.z.ai/api/paas/v4"), Some("zai"));
    assert_eq!(pi_catalog_for_base_url("https://api.openai.com/v1"), None);
}

#[test]
fn parse_entry_reads_cost_map() {
    let a = parse_entry(&serde_json::json!({
        "cost": { "input": 3.0, "output": 15.0, "cache_read": 0.3, "cache_write": 3.75 }
    }))
    .expect("cost-only entry is worth keeping");
    let c = a.cost.expect("cost parsed");
    assert_eq!(c.input, Some(3.0));
    assert_eq!(c.output, Some(15.0));
    assert_eq!(c.cache_read, Some(0.3));
    assert_eq!(c.cache_write, Some(3.75));

    // Partial map: missing components stay None, not zero.
    let a = parse_entry(&serde_json::json!({ "cost": { "input": 1.0 } })).unwrap();
    let c = a.cost.unwrap();
    assert_eq!(c.input, Some(1.0));
    assert_eq!(c.output, None);

    // Empty/absent map → no cost field, and a cost-only-empty entry is None.
    let a = parse_entry(&serde_json::json!({ "reasoning": true })).unwrap();
    assert!(a.cost.is_none());
    assert!(parse_entry(&serde_json::json!({ "cost": {} })).is_none());
}

#[test]
fn merge_keeps_override_cost_over_default() {
    let base = parse_entry(&serde_json::json!({ "cost": { "input": 1.0 } })).unwrap();
    let over = parse_entry(&serde_json::json!({ "cost": { "input": 9.0, "output": 2.0 } })).unwrap();
    let merged = merge_field_overrides(base, over);
    let c = merged.cost.unwrap();
    assert_eq!(c.input, Some(9.0), "override price wins");
    assert_eq!(c.output, Some(2.0));
}
