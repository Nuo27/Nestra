use crate::endpoint_quota::BuiltinKind;
use serde::Serialize;
use super::endpoints::ProtocolInfo;

#[derive(Serialize)]
pub struct ProviderPreset {
    pub id: String,
    pub display_name: String,
    pub protocols: Vec<ProtocolInfo>,
    pub default_model: Option<String>,
    /// Built-in quota query this preset carries, if any. Endpoints created
    /// from the preset inherit this as their `QuotaQueryPlan::Preset` so the
    /// Quota page + keep-alive work without extra configuration. `None` for
    /// presets with no built-in fetcher (the user can still configure a
    /// custom extractor on the endpoint).
    pub quota_query: Option<crate::endpoint_quota::BuiltinKind>,
}

// ---- Presets ----

#[tauri::command]
pub fn provider_presets() -> Vec<ProviderPreset> {

    // Alphabetical by display name (case-insensitive); the "Custom" entry
    // is pinned last — it's the escape hatch, not a browsing item.
    let mut presets = vec![
        ProviderPreset {
            id: "minimax".into(),
            display_name: "MiniMax".into(),
            protocols: vec![ProtocolInfo {
                protocol: "openai-comp".into(),
                base_url: "https://api.minimax.io/v1".into(),
            }],
            default_model: None,
            quota_query: Some(BuiltinKind::Minimax),
        },
        ProviderPreset {
            id: "openrouter".into(),
            display_name: "OpenRouter".into(),
            protocols: vec![
                // OpenRouter natively exposes an Anthropic Messages-compatible
                // input (https://openrouter.ai/api/v1) — Claude Code binds
                // through this row and speaks its native wire directly, no
                // gateway conversion needed. OpenCode/Pi bind through the
                // `openai` row (or override via the per-binding protocol
                // picker). Same base_url on both rows: in Routed mode the
                // gateway's same-base bridge still dials the openai wire.
                ProtocolInfo {
                    protocol: "anthropic".into(),
                    base_url: "https://openrouter.ai/api/v1".into(),
                },
                ProtocolInfo {
                    protocol: "openai-comp".into(),
                    base_url: "https://openrouter.ai/api/v1".into(),
                },
            ],
            default_model: None,
            quota_query: Some(BuiltinKind::Openrouter),
        },
        ProviderPreset {
            id: "zai".into(),
            display_name: "Z.ai".into(),
            protocols: vec![
                ProtocolInfo {
                    protocol: "anthropic".into(),
                    base_url: "https://api.z.ai/api/anthropic".into(),
                },
                ProtocolInfo {
                    protocol: "openai-comp".into(),
                    base_url: "https://api.z.ai/v1".into(),
                },
            ],
            default_model: None,
            // Z.ai exposes a token-window quota monitor (5h + weekly) and is
            // the primary target of the keep-alive worker. Carrying the query
            // on the preset means a Z.ai endpoint is queryable + keep-alive-
            // ready with no extra configuration.
            quota_query: Some(BuiltinKind::Zai),
        },
        ProviderPreset {
            id: "openai-comp".into(),
            display_name: "OpenAI".into(),
            protocols: vec![ProtocolInfo {
                protocol: "openai-comp".into(),
                base_url: "https://api.openai.com/v1".into(),
            }],
            default_model: None,
            quota_query: None,
        },
        ProviderPreset {
            // OpenAI Responses API (`/v1/responses`) as a distinct endpoint —
            // for users who want a dedicated key/billing surface for the
            // Responses dialect. No current agent speaks the Responses wire
            // natively, so this binds gateway-upstream only (the router dials
            // `/v1/responses` for responses-class models via `wire_for_model`).
            id: "openai-responses".into(),
            display_name: "OpenAI Responses".into(),
            protocols: vec![ProtocolInfo {
                protocol: "response-api".into(),
                base_url: "https://api.openai.com/v1".into(),
            }],
            default_model: None,
            quota_query: None,
        },
        ProviderPreset {
            id: "anthropic".into(),
            display_name: "Anthropic".into(),
            protocols: vec![ProtocolInfo {
                protocol: "anthropic".into(),
                base_url: "https://api.anthropic.com".into(),
            }],
            default_model: None,
            quota_query: None,
        },
        ProviderPreset {
            id: "deepseek".into(),
            display_name: "DeepSeek".into(),
            protocols: vec![
                ProtocolInfo {
                    protocol: "anthropic".into(),
                    base_url: "https://api.deepseek.com/anthropic".into(),
                },
                ProtocolInfo {
                    protocol: "openai-comp".into(),
                    base_url: "https://api.deepseek.com/v1".into(),
                },
            ],
            default_model: None,
            quota_query: None,
        },
        ProviderPreset {
            id: "moonshot".into(),
            display_name: "Moonshot / Kimi".into(),
            protocols: vec![
                ProtocolInfo {
                    protocol: "anthropic".into(),
                    base_url: "https://api.moonshot.cn/anthropic".into(),
                },
                ProtocolInfo {
                    protocol: "openai-comp".into(),
                    base_url: "https://api.moonshot.cn/v1".into(),
                },
            ],
            default_model: None,
            quota_query: None,
        },
        ProviderPreset {
            id: "kimi-coding".into(),
            display_name: "Kimi For Coding".into(),
            protocols: vec![
                ProtocolInfo {
                    protocol: "anthropic".into(),
                    base_url: "https://api.kimi.com/coding/".into(),
                },
                ProtocolInfo {
                    protocol: "openai-comp".into(),
                    base_url: "https://api.kimi.com/coding/v1".into(),
                },
            ],
            default_model: None,
            quota_query: None,
        },
        ProviderPreset {
            id: "zhipu".into(),
            display_name: "Zhipu (BigModel)".into(),
            protocols: vec![
                ProtocolInfo {
                    protocol: "anthropic".into(),
                    base_url: "https://open.bigmodel.cn/api/anthropic".into(),
                },
                ProtocolInfo {
                    protocol: "openai-comp".into(),
                    base_url: "https://open.bigmodel.cn/api/coding/paas/v4".into(),
                },
            ],
            default_model: None,
            quota_query: None,
        },
        ProviderPreset {
            id: "volcengine-ark".into(),
            display_name: "Volcengine Ark (Doubao)".into(),
            protocols: vec![
                ProtocolInfo {
                    protocol: "anthropic".into(),
                    base_url: "https://ark.cn-beijing.volces.com/api/coding".into(),
                },
                ProtocolInfo {
                    protocol: "openai-comp".into(),
                    base_url: "https://ark.cn-beijing.volces.com/api/v3".into(),
                },
            ],
            default_model: None,
            quota_query: None,
        },
        ProviderPreset {
            id: "siliconflow".into(),
            display_name: "SiliconFlow".into(),
            protocols: vec![
                ProtocolInfo {
                    protocol: "anthropic".into(),
                    base_url: "https://api.siliconflow.cn".into(),
                },
                ProtocolInfo {
                    protocol: "openai-comp".into(),
                    base_url: "https://api.siliconflow.cn/v1".into(),
                },
            ],
            default_model: None,
            quota_query: None,
        },
        ProviderPreset {
            id: "hunyuan".into(),
            display_name: "Tencent Hunyuan".into(),
            protocols: vec![ProtocolInfo {
                protocol: "openai-comp".into(),
                base_url: "https://tokenhub.tencentmaas.com/v1".into(),
            }],
            default_model: None,
            quota_query: None,
        },
        ProviderPreset {
            id: "stepfun".into(),
            display_name: "StepFun".into(),
            protocols: vec![
                ProtocolInfo {
                    protocol: "anthropic".into(),
                    base_url: "https://api.stepfun.com/step_plan".into(),
                },
                ProtocolInfo {
                    protocol: "openai-comp".into(),
                    base_url: "https://api.stepfun.com/step_plan/v1".into(),
                },
            ],
            default_model: None,
            quota_query: None,
        },
        ProviderPreset {
            id: "xiaomi-mimo".into(),
            display_name: "Xiaomi MiMo".into(),
            protocols: vec![
                ProtocolInfo {
                    protocol: "anthropic".into(),
                    base_url: "https://api.xiaomimimo.com/anthropic".into(),
                },
                ProtocolInfo {
                    protocol: "openai-comp".into(),
                    base_url: "https://api.xiaomimimo.com/v1".into(),
                },
            ],
            default_model: None,
            quota_query: None,
        },
        ProviderPreset {
            id: "baidu-qianfan".into(),
            display_name: "Baidu Qianfan".into(),
            protocols: vec![
                ProtocolInfo {
                    protocol: "anthropic".into(),
                    base_url: "https://qianfan.baidubce.com/anthropic/coding".into(),
                },
                ProtocolInfo {
                    protocol: "openai-comp".into(),
                    base_url: "https://qianfan.baidubce.com/v2/coding".into(),
                },
            ],
            default_model: None,
            quota_query: None,
        },
        ProviderPreset {
            id: "modelscope".into(),
            display_name: "ModelScope".into(),
            protocols: vec![
                ProtocolInfo {
                    protocol: "anthropic".into(),
                    base_url: "https://api-inference.modelscope.cn".into(),
                },
                ProtocolInfo {
                    protocol: "openai-comp".into(),
                    base_url: "https://api-inference.modelscope.cn/v1".into(),
                },
            ],
            default_model: None,
            quota_query: None,
        },
        ProviderPreset {
            id: "baichuan".into(),
            display_name: "Baichuan".into(),
            protocols: vec![ProtocolInfo {
                protocol: "openai-comp".into(),
                base_url: "https://api.baichuan-ai.com/v1".into(),
            }],
            default_model: None,
            quota_query: None,
        },
        ProviderPreset {
            id: "xai-grok".into(),
            display_name: "xAI Grok".into(),
            protocols: vec![ProtocolInfo {
                protocol: "openai-comp".into(),
                base_url: "https://api.x.ai/v1".into(),
            }],
            default_model: None,
            quota_query: None,
        },
        ProviderPreset {
            id: "mistral".into(),
            display_name: "Mistral".into(),
            protocols: vec![ProtocolInfo {
                protocol: "openai-comp".into(),
                base_url: "https://api.mistral.ai/v1".into(),
            }],
            default_model: None,
            quota_query: None,
        },
        ProviderPreset {
            id: "groq".into(),
            display_name: "Groq".into(),
            protocols: vec![ProtocolInfo {
                protocol: "openai-comp".into(),
                base_url: "https://api.groq.com/openai/v1".into(),
            }],
            default_model: None,
            quota_query: None,
        },
        ProviderPreset {
            id: "fireworks".into(),
            display_name: "Fireworks AI".into(),
            protocols: vec![ProtocolInfo {
                protocol: "openai-comp".into(),
                base_url: "https://api.fireworks.ai/inference/v1".into(),
            }],
            default_model: None,
            quota_query: None,
        },
        ProviderPreset {
            id: "together".into(),
            display_name: "Together AI".into(),
            protocols: vec![ProtocolInfo {
                protocol: "openai-comp".into(),
                base_url: "https://api.together.xyz/v1".into(),
            }],
            default_model: None,
            quota_query: None,
        },
        ProviderPreset {
            id: "cerebras".into(),
            display_name: "Cerebras".into(),
            protocols: vec![ProtocolInfo {
                protocol: "openai-comp".into(),
                base_url: "https://api.cerebras.ai/v1".into(),
            }],
            default_model: None,
            quota_query: None,
        },
        ProviderPreset {
            id: "cohere".into(),
            display_name: "Cohere".into(),
            protocols: vec![ProtocolInfo {
                protocol: "openai-comp".into(),
                base_url: "https://api.cohere.com/v1".into(),
            }],
            default_model: None,
            quota_query: None,
        },
        ProviderPreset {
            id: "perplexity".into(),
            display_name: "Perplexity".into(),
            protocols: vec![ProtocolInfo {
                protocol: "openai-comp".into(),
                base_url: "https://api.perplexity.ai".into(),
            }],
            default_model: None,
            quota_query: None,
        },
        ProviderPreset {
            id: "nvidia-nim".into(),
            display_name: "NVIDIA NIM".into(),
            protocols: vec![
                ProtocolInfo {
                    protocol: "anthropic".into(),
                    base_url: "https://integrate.api.nvidia.com".into(),
                },
                ProtocolInfo {
                    protocol: "openai-comp".into(),
                    base_url: "https://integrate.api.nvidia.com/v1".into(),
                },
            ],
            default_model: None,
            quota_query: None,
        },
        ProviderPreset {
            id: "github-models".into(),
            display_name: "GitHub Models".into(),
            protocols: vec![ProtocolInfo {
                protocol: "openai-comp".into(),
                base_url: "https://models.github.ai/inference/v1".into(),
            }],
            default_model: None,
            quota_query: None,
        },
        ProviderPreset {
            id: "opencode-go".into(),
            display_name: "OpenCode Go".into(),
            // The gateway serves both protocols on the SAME base_url
            // (verified: /v1/chat/completions, /v1/responses, /v1/messages).
            // Dual rows let every agent bind in Direct mode: Claude Code
            // (anthropic-only) picks the anthropic row, OpenCode/Pi the
            // openai row. The per-model protocol map (model_abilities
            // corrections) filters which models are written per row.
            protocols: vec![
                ProtocolInfo {
                    protocol: "anthropic".into(),
                    base_url: "https://opencode.ai/zen/go/v1".into(),
                },
                ProtocolInfo {
                    protocol: "openai-comp".into(),
                    base_url: "https://opencode.ai/zen/go/v1".into(),
                },
            ],
            // The Go plan's flagship model (1M context).
            default_model: Some("deepseek-v4-flash".into()),
            // Usage is scraped from the authenticated dashboard (cookie +
            // workspace ID, configured per-endpoint in quota settings) — there
            // is no API-key usage endpoint. Stamped here so a Go endpoint is
            // queryable once those credentials are added.
            quota_query: Some(BuiltinKind::OpencodeGo),
        },
        ProviderPreset {
            id: "custom".into(),
            display_name: "Custom".into(),
            protocols: vec![],
            default_model: None,
            quota_query: None,
        },
    ];
    presets.sort_by(|a, b| {
        let a_custom = a.id == "custom";
        let b_custom = b.id == "custom";
        match (a_custom, b_custom) {
            (true, true) => std::cmp::Ordering::Equal,
            (true, false) => std::cmp::Ordering::Greater,
            (false, true) => std::cmp::Ordering::Less,
            (false, false) => a.display_name.to_lowercase().cmp(&b.display_name.to_lowercase()),
        }
    });
    presets
}

#[cfg(test)]
mod tests {
    use super::*;

use super::super::common::validate_protocol_kind;

    #[test]
    fn provider_presets_sorted_alphabetically_with_custom_last() {
        let presets = provider_presets();
        let names: Vec<&str> = presets.iter().map(|p| p.display_name.as_str()).collect();
        // Custom pinned last.
        assert_eq!(names.last(), Some(&"Custom"));
        // Everything before it ascending, case-insensitive.
        let mut sorted = names[..names.len() - 1].to_vec();
        sorted.sort_by_key(|s| s.to_lowercase());
        assert_eq!(&names[..names.len() - 1], sorted.as_slice());
        // OpenCode Go present with its dual-protocol rows (same gateway)
        // + Go plan default model.
        let go = presets
            .iter()
            .find(|p| p.id == "opencode-go")
            .expect("opencode-go preset");
        assert_eq!(go.protocols.len(), 2);
        assert_eq!(go.protocols[0].protocol, "anthropic");
        assert_eq!(go.protocols[1].protocol, "openai-comp");
        assert!(go.protocols.iter().all(|p| p.base_url == "https://opencode.ai/zen/go/v1"));
        assert_eq!(go.default_model.as_deref(), Some("deepseek-v4-flash"));
    }

    #[test]
    fn presets_have_unique_ids_and_canonical_protocols() {
        use std::collections::HashSet;
        let presets = provider_presets();
        let mut seen = HashSet::new();
        for p in &presets {
            assert!(seen.insert(p.id.clone()), "duplicate preset id: {}", p.id);
            assert!(!p.display_name.is_empty());
            for proto in &p.protocols {
                assert!(
                    validate_protocol_kind(&proto.protocol).is_ok(),
                    "preset {} uses non-canonical protocol {}",
                    p.id,
                    proto.protocol
                );
                assert!(
                    !proto.base_url.is_empty(),
                    "preset {} has empty base_url",
                    p.id
                );
            }
        }
        // Custom is the fallback — must always be present.
        assert!(presets.iter().any(|p| p.id == "custom"));
    }
}
