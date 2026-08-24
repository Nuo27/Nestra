/// Live bytes-on-disk sanity check: writes a realistic minimax + z.ai
/// opencode.json to the OS temp dir so the human reviewer can open it
/// in any editor. Ignored from default test runs — invoke with
/// `cargo test --lib live_write_realistic_opencode_json -- --ignored --nocapture`
/// to produce the file at `$TEMP/nestra-live-opencode.json`.
#[test]
#[ignore]
fn live_write_realistic_opencode_json() {
    use std::env;
    let dir = env::temp_dir().join(format!("nestra-live-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("opencode.json");
    std::fs::write(&cfg, FIXTURE).unwrap();

    let mut minimax = ctx_anthropic();
    minimax.provider_id = "minimax".into();
    minimax.base_url = "https://api.minimaxi.com/anthropic".into();
    minimax.display_name = "minimax (via Nestra)".into();
    minimax.models = ModelsConfig::Openai {
        default: "MiniMax-M3".into(),
        available: vec!["MiniMax-M3".into(), "MiniMax-M2.7".into()],
    };
    for (id, ctx_limit, out_limit) in [
        ("MiniMax-M3", 512_000u64, 128_000u64),
        ("MiniMax-M2.7", 204_800, 131_072),
    ] {
        minimax.model_abilities.insert(
            id.into(),
            crate::model_abilities::ModelAbilities {
                reasoning: Some(true),
                tool_call: Some(true),
                attachment: Some(true),
                temperature: None,
                limit: Some(crate::model_abilities::ModelLimit {
                    context: ctx_limit,
                    output: out_limit,
                    input: None }),
                            modalities: None,
                            api: None,
                            cost: None,
    },
        );
    }

    let mut zai = ctx_anthropic();
    zai.provider_id = "z-ai".into();
    zai.base_url = "https://api.z.ai/api/anthropic".into();
    zai.display_name = "z.ai (via Nestra)".into();
    zai.models = ModelsConfig::Openai {
        default: "glm-4.7".into(),
        available: vec!["glm-4.7".into(), "glm-5.1".into(), "glm-5.2".into()],
    };
    for (id, ctx_limit, out_limit) in [
        ("glm-4.7", 204_800u64, 131_072u64),
        ("glm-5.1", 200_000, 131_072),
        ("glm-5.2", 1_000_000, 131_072),
    ] {
        zai.model_abilities.insert(
            id.into(),
            crate::model_abilities::ModelAbilities {
                reasoning: Some(true),
                tool_call: Some(true),
                attachment: Some(true),
                temperature: None,
                limit: Some(crate::model_abilities::ModelLimit {
                    context: ctx_limit,
                    output: out_limit,
                    input: None }),
                            modalities: None,
                            api: None,
                            cost: None,
    },
        );
    }

    let set = ProviderSet {
        entries: vec![minimax, zai],
        default_provider_id: "minimax".into(),
        default_model: "MiniMax-M3".into(),
    };
    OpenCode.apply_set(&cfg, &set).unwrap();
    eprintln!("wrote {}", cfg.display());
    let raw = std::fs::read_to_string(&cfg).unwrap();
    eprintln!("--- begin {} ---", cfg.display());
    eprintln!("{}", raw);
    eprintln!("--- end ---");
}
use super::*;
use std::path::PathBuf;
use crate::config_writer::{backup_path_for, SwitchContext};
use std::fs;

const FIXTURE: &str = include_str!("../../../fixtures/opencode/opencode.json");

fn tmp() -> (PathBuf, tempfile::TempDir) {
    let dir = tempfile::Builder::new()
        .prefix("")
        .tempdir()
        .expect("tempdir");
    (dir.path().to_path_buf(), dir)
}

fn ctx_openai() -> SwitchContext {
    SwitchContext {
        provider_id: "openai".into(),
        provider_kind: ProviderKind::Openai,
        display_name: "OpenAI".into(),
        base_url: "https://api.openai.com/v1".into(),
        api_key: "sk-openai".into(),
        models: ModelsConfig::Openai {
            default: "gpt-4o".into(),
            available: vec!["gpt-4o".into(), "gpt-4o-mini".into()],
        },
        advanced_env: Default::default(),
        model_abilities: Default::default(),
    }
}

#[test]
fn apply_writes_provider_block_and_preserves_rest() {
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("opencode.json");
    fs::write(&cfg, FIXTURE).unwrap();

    let out = OpenCode.apply(&cfg, &ctx_openai()).unwrap();
    assert!(out);

    let written: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
    let block = &written["provider"]["nestra-openai"];
    assert_eq!(block["npm"], "@ai-sdk/openai-compatible");
    assert_eq!(block["options"]["baseURL"], "https://api.openai.com/v1");
    assert_eq!(block["options"]["apiKey"], "sk-openai");
    assert_eq!(block["models"]["gpt-4o"]["name"], "gpt-4o");
    // No abilities seeded → entry stays name-only (offline/unmatched path).
    assert!(block["models"]["gpt-4o"].as_object().unwrap().len() == 1);
    // preserved
    assert_eq!(written["$schema"], "https://opencode.ai/config.json");
    assert_eq!(written["small_model"], "existing-model");
}

#[test]
fn models_map_emits_matched_abilities() {
    use crate::model_abilities::{ModelAbilities, ModelLimit};
    let mut abilities = std::collections::HashMap::new();
    abilities.insert(
        "gpt-4o".to_string(),
        ModelAbilities {
            reasoning: Some(false),
            tool_call: Some(true),
            attachment: Some(true),
            temperature: None,
            limit: Some(ModelLimit { context: 128_000, output: 16_384, input: None }),
            modalities: None,
            api: None,
            cost: None,
        },
    );
    let ctx = SwitchContext {
        provider_id: "openai".into(),
        provider_kind: ProviderKind::Openai,
        display_name: "OpenAI".into(),
        base_url: "https://api.openai.com/v1".into(),
        api_key: "sk-openai".into(),
        models: ModelsConfig::Openai {
            default: "gpt-4o".into(),
            available: vec!["gpt-4o".into(), "gpt-4o-mini".into()],
        },
        advanced_env: Default::default(),
        model_abilities: abilities,
    };
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("opencode.json");
    fs::write(&cfg, FIXTURE).unwrap();
    OpenCode.apply(&cfg, &ctx).unwrap();
    let written: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
    let entry = &written["provider"]["nestra-openai"]["models"]["gpt-4o"];
    assert_eq!(entry["name"], "gpt-4o");
    assert_eq!(entry["reasoning"], false);
    assert_eq!(entry["tool_call"], true);
    assert_eq!(entry["attachment"], true);
    assert_eq!(entry["limit"]["context"], 128_000);
    assert_eq!(entry["limit"]["output"], 16_384);
    // temperature absent (None) → key not emitted.
    assert!(entry.get("temperature").is_none());
    // gpt-4o-mini had no matched abilities → name-only.
    let mini = &written["provider"]["nestra-openai"]["models"]["gpt-4o-mini"];
    assert_eq!(mini["name"], "gpt-4o-mini");
    assert!(mini.as_object().unwrap().len() == 1);
}

#[test]
fn anthropic_uses_anthropic_npm_and_three_models() {
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("opencode.json");
    fs::write(&cfg, FIXTURE).unwrap();
    let mut ctx = ctx_openai();
    ctx.provider_id = "anthropic".into();
    ctx.provider_kind = ProviderKind::Anthropic;
    ctx.models = ModelsConfig::Anthropic {
        default: "claude-default".into(),
        haiku: "claude-haiku".into(),
        sonnet: "claude-sonnet".into(),
        opus: "claude-opus".into(),
    };
    OpenCode.apply(&cfg, &ctx).unwrap();
    let written: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
    let block = &written["provider"]["nestra-anthropic"];
    assert_eq!(block["npm"], "@ai-sdk/anthropic");
    assert!(block["models"].as_object().unwrap().contains_key("claude-haiku"));
    assert!(block["models"].as_object().unwrap().contains_key("claude-sonnet"));
    assert!(block["models"].as_object().unwrap().contains_key("claude-opus"));
}

#[test]
fn restore_is_byte_exact() {
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("opencode.json");
    fs::write(&cfg, FIXTURE).unwrap();
    let original = fs::read(&cfg).unwrap();
    OpenCode.apply(&cfg, &ctx_openai()).unwrap();
    OpenCode.restore(&cfg).unwrap();
    assert_eq!(fs::read(&cfg).unwrap(), original);
    assert!(!backup_path_for(&cfg).exists());
}

fn ctx_anthropic() -> SwitchContext {
    SwitchContext {
        provider_id: "minimax-cn".into(),
        provider_kind: ProviderKind::Anthropic,
        display_name: "MiniMax CN".into(),
        base_url: "https://api.minimax.com".into(),
        api_key: "sk-mm".into(),
        models: ModelsConfig::Anthropic {
            default: "MiniMax-M3".into(),
            haiku: "MiniMax-M3-haiku".into(),
            sonnet: "MiniMax-M3".into(),
            opus: "MiniMax-M3".into(),
        },
        advanced_env: Default::default(),
        model_abilities: Default::default(),
    }
}

#[test]
fn apply_set_writes_model_pointer_and_all_providers() {
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("opencode.json");
    fs::write(&cfg, FIXTURE).unwrap();

    let set = ProviderSet {
        entries: vec![ctx_anthropic(), ctx_openai()],
        default_provider_id: "minimax-cn".into(),
        default_model: "MiniMax-M3".into(),
    };
    OpenCode.apply_set(&cfg, &set).unwrap();

    let written: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
    assert_eq!(written["model"], "nestra-minimax-cn/MiniMax-M3");
    assert!(written["provider"]["nestra-minimax-cn"].is_object());
    assert!(written["provider"]["nestra-openai"].is_object());
}

#[test]
fn apply_set_replaces_previous_owned_providers() {
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("opencode.json");
    fs::write(&cfg, FIXTURE).unwrap();

    OpenCode.apply_set(
        &cfg,
        &ProviderSet {
            entries: vec![ctx_anthropic()],
            default_provider_id: "minimax-cn".into(),
            default_model: "MiniMax-M3".into(),
        },
    )
    .unwrap();

    OpenCode.apply_set(
        &cfg,
        &ProviderSet {
            entries: vec![ctx_openai()],
            default_provider_id: "openai".into(),
            default_model: "gpt-4o".into(),
        },
    )
    .unwrap();

    let written: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
    assert!(written["provider"].get("nestra-minimax-cn").is_none());
    assert!(written["provider"]["nestra-openai"].is_object());
    assert_eq!(written["model"], "nestra-openai/gpt-4o");
}

/// Gateway mode writes the `nestra-gw` block with the OpenAI-SDK-style
/// base URL (`…/<agent>/v1` — the SDK appends `/chat/completions` and
/// probes `GET /models`) and the `nestra-gw/<alias>` model pointer.
#[test]
fn apply_gateway_set_writes_v1_base_url_and_alias_pointer() {
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("opencode.json");
    fs::write(&cfg, FIXTURE).unwrap();

    let alias = crate::config_writer::GatewayAlias::simple(
        "http://127.0.0.1:18777/opencode-desktop",
        "nestra",
        "nestra",
    );
    OpenCode.apply_gateway_set(&cfg, &alias).unwrap();

    let written: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
    assert_eq!(
        written["provider"]["nestra-gw"]["options"]["baseURL"],
        "http://127.0.0.1:18777/opencode-desktop/v1"
    );
    assert_eq!(written["model"], "nestra-gw/nestra");
    // The placeholder model declares its capabilities inline — custom
    // providers get no models.dev data, so without these OpenCode would
    // render context 0 / "reasoning: no allow".
    let entry = &written["provider"]["nestra-gw"]["models"]["nestra"];
    assert_eq!(entry["name"], "nestra");
    assert_eq!(entry["limit"]["context"], 200000);
    assert_eq!(entry["reasoning"], true);
    // Prior nestra-owned blocks are replaced.
    assert!(written["provider"].get("nestra-minimax-cn").is_none());
}

/// The gateway model entry carries the alias slot's REAL abilities (full
/// `to_model_entry_fields` passthrough — limits + flags + modalities), so
/// OpenCode plans against the actual window the router will serve.
#[test]
fn apply_gateway_set_passes_alias_abilities_through() {
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("opencode.json");
    fs::write(&cfg, FIXTURE).unwrap();

    let alias = crate::config_writer::GatewayAlias {
        gateway_base_url: "http://127.0.0.1:18777/opencode-desktop".into(),
        model_alias: crate::config_writer::AliasModel {
            id: "nestra".into(),
            abilities: Some(crate::model_abilities::ModelAbilities {
                reasoning: Some(false),
                tool_call: Some(true),
                attachment: Some(true),
                temperature: None,
                limit: Some(crate::model_abilities::ModelLimit {
                    context: 1_000_000,
                    output: 64_000,
                    input: None,
                }),
                modalities: None,
                api: None,
                cost: None,
            }),
        },
        tier_aliases: None,
        sentinel_key: "gw-token".into(),
    };
    OpenCode.apply_gateway_set(&cfg, &alias).unwrap();

    let written: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
    let entry = &written["provider"]["nestra-gw"]["models"]["nestra"];
    assert_eq!(entry["name"], "nestra");
    assert_eq!(entry["limit"]["context"], 1_000_000);
    assert_eq!(entry["limit"]["output"], 64_000);
    assert_eq!(entry["reasoning"], false, "honest flag, not blanket true");
    assert_eq!(entry["tool_call"], true);
    assert_eq!(entry["attachment"], true);
}

/// End-to-end: a SwitchContext carrying models.dev-derived abilities
/// surfaces `reasoning: true` + `tool_call` + `attachment` + `limit` on
/// the written model entry. This is the regression test for "model is
/// text-only because Nestra emitted `{ "name": "<id>" }` only" — the
/// root cause that motivated the merge helper in model_abilities.rs.
#[test]
fn apply_set_emits_ability_fields_into_model_entry() {
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("opencode.json");
    fs::write(&cfg, FIXTURE).unwrap();

    let mut ctx = ctx_anthropic();
    // Mirror the user-facing override: `minimax` (was minimax-cn), only
    // MiniMax-M3 carries abilities. The provider key is therefore
    // `nestra-minimax` (formatted as `nestra-{provider_id}`).
    ctx.provider_id = "minimax".into();
    ctx.model_abilities.insert(
        "MiniMax-M3".into(),
        crate::model_abilities::ModelAbilities {
            reasoning: Some(true),
            tool_call: Some(true),
            attachment: Some(true),
            temperature: Some(false),
            limit: Some(crate::model_abilities::ModelLimit {
                context: 512_000,
                output: 128_000,
                input: None }),
                        modalities: None,
                        api: None,
                        cost: None,
    },
    );
    OpenCode.apply(&cfg, &ctx).unwrap();

    let written: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
    let entry = &written["provider"]["nestra-minimax"]["models"]["MiniMax-M3"];
    assert_eq!(entry["reasoning"], true, "reasoning must propagate");
    assert_eq!(entry["tool_call"], true);
    assert_eq!(entry["attachment"], true);
    assert_eq!(entry["temperature"], false);
    assert_eq!(entry["limit"]["context"], 512_000);
    assert_eq!(entry["limit"]["output"], 128_000);
    assert!(written["provider"]["nestra-minimax"]["models"]
        .as_object()
        .unwrap()
        .contains_key("MiniMax-M3"));
}

/// Live-shape regression: rewrite a real opencode.json that mirrors
/// the user's actual two-provider config (minimax + z.ai over the
/// Anthropic-compat base URLs) and confirm every model entry carries
/// the merged abilities. This is the bytes-on-disk contract the fix
/// promises — it should pass on a clean clone.
#[test]
fn apply_set_writes_realistic_minimax_plus_zai_config() {
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("opencode.json");
    fs::write(&cfg, FIXTURE).unwrap();

    let mut minimax = ctx_anthropic();
    minimax.provider_id = "minimax".into();
    minimax.provider_kind = ProviderKind::Anthropic;
    minimax.base_url = "https://api.minimaxi.com/anthropic".into();
    minimax.display_name = "minimax (via Nestra)".into();
    minimax.models = ModelsConfig::Openai {
        default: "MiniMax-M3".into(),
        available: vec!["MiniMax-M3".into(), "MiniMax-M2.7".into()],
    };
    minimax.model_abilities.insert(
        "MiniMax-M3".into(),
        crate::model_abilities::ModelAbilities {
            reasoning: Some(true),
            tool_call: Some(true),
            attachment: Some(true),
            temperature: None,
            limit: Some(crate::model_abilities::ModelLimit {
                context: 512_000,
                output: 128_000,
                input: None }),
                        modalities: None,
                        api: None,
                        cost: None,
    },
    );
    minimax.model_abilities.insert(
        "MiniMax-M2.7".into(),
        crate::model_abilities::ModelAbilities {
            reasoning: Some(true),
            tool_call: Some(true),
            attachment: Some(true),
            temperature: None,
            limit: Some(crate::model_abilities::ModelLimit {
                context: 204_800,
                output: 131_072,
                input: None }),
                        modalities: None,
                        api: None,
                        cost: None,
    },
    );

    let mut zai = ctx_anthropic();
    zai.provider_id = "z-ai".into();
    zai.provider_kind = ProviderKind::Anthropic;
    zai.base_url = "https://api.z.ai/api/anthropic".into();
    zai.display_name = "z.ai (via Nestra)".into();
    zai.models = ModelsConfig::Openai {
        default: "glm-4.7".into(),
        available: vec!["glm-4.7".into(), "glm-5.1".into(), "glm-5.2".into()],
    };
    zai.model_abilities.insert(
        "glm-4.7".into(),
        crate::model_abilities::ModelAbilities {
            reasoning: Some(true),
            tool_call: Some(true),
            attachment: Some(true),
            temperature: None,
            limit: Some(crate::model_abilities::ModelLimit {
                context: 204_800,
                output: 131_072,
                input: None }),
                        modalities: None,
                        api: None,
                        cost: None,
    },
    );
    zai.model_abilities.insert(
        "glm-5.1".into(),
        crate::model_abilities::ModelAbilities {
            reasoning: Some(true),
            tool_call: Some(true),
            attachment: Some(true),
            temperature: None,
            limit: Some(crate::model_abilities::ModelLimit {
                context: 200_000,
                output: 131_072,
                input: None }),
                        modalities: None,
                        api: None,
                        cost: None,
    },
    );
    zai.model_abilities.insert(
        "glm-5.2".into(),
        crate::model_abilities::ModelAbilities {
            reasoning: Some(true),
            tool_call: Some(true),
            attachment: Some(true),
            temperature: None,
            limit: Some(crate::model_abilities::ModelLimit {
                context: 1_000_000,
                output: 131_072,
                input: None }),
                        modalities: None,
                        api: None,
                        cost: None,
    },
    );

    let set = ProviderSet {
        entries: vec![minimax, zai],
        default_provider_id: "minimax".into(),
        default_model: "MiniMax-M3".into(),
    };
    OpenCode.apply_set(&cfg, &set).unwrap();

    let written: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
    assert_eq!(written["model"], "nestra-minimax/MiniMax-M3");

    // Anthropic-protocol providers must carry the `/v1` version root so
    // `@ai-sdk/anthropic` posts to `<base>/v1/messages`, not `<base>/messages`
    // (which MiniMax rejects with 404).
    assert_eq!(
        written["provider"]["nestra-minimax"]["options"]["baseURL"],
        "https://api.minimaxi.com/anthropic/v1"
    );
    assert_eq!(
        written["provider"]["nestra-z-ai"]["options"]["baseURL"],
        "https://api.z.ai/api/anthropic/v1"
    );

    for (provider_key, model_id, expected_ctx, expected_out) in [
        ("nestra-minimax", "MiniMax-M3", 512_000, 128_000),
        ("nestra-minimax", "MiniMax-M2.7", 204_800, 131_072),
        ("nestra-z-ai", "glm-4.7", 204_800, 131_072),
        ("nestra-z-ai", "glm-5.1", 200_000, 131_072),
        ("nestra-z-ai", "glm-5.2", 1_000_000, 131_072),
    ] {
        let entry = &written["provider"][provider_key]["models"][model_id];
        assert_eq!(
            entry["reasoning"], true,
            "{provider_key}/{model_id}: reasoning must be on"
        );
        assert_eq!(entry["tool_call"], true, "{provider_key}/{model_id}");
        assert_eq!(entry["attachment"], true, "{provider_key}/{model_id}");
        assert_eq!(
            entry["limit"]["context"], expected_ctx,
            "{provider_key}/{model_id} context"
        );
        assert_eq!(
            entry["limit"]["output"], expected_out,
            "{provider_key}/{model_id} output"
        );
    }
}