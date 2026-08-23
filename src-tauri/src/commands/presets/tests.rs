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
    // OpenCode Go: chat-completions ONLY (the anthropic wire on that base
    // 500s — an earlier dual-row preset made Direct binds default to it).
    let go = presets
        .iter()
        .find(|p| p.id == "opencode-go")
        .expect("opencode-go preset");
    assert_eq!(go.protocols.len(), 1);
    assert_eq!(go.protocols[0].protocol, "openai-comp");
    assert_eq!(go.protocols[0].base_url, "https://opencode.ai/zen/go/v1");
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