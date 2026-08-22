use super::extract_agent_id;

#[test]
fn known_prefixes_resolve() {
    assert_eq!(
        extract_agent_id("/zcode-desktop/v1/messages").as_deref(),
        Some("zcode-desktop")
    );
    assert_eq!(
        extract_agent_id("/claude-code-cli/v1/messages").as_deref(),
        Some("claude-code-cli")
    );
    assert_eq!(
        extract_agent_id("/pi-cli/v1/chat/completions").as_deref(),
        Some("pi-cli")
    );
}

#[test]
fn pre_rename_prefixes_alias_to_new_ids() {
    // Gateway base_urls written into agent configs before the `-cli` rename
    // must keep routing (and attribute to the new registry id).
    assert_eq!(
        extract_agent_id("/claude-code/v1/messages").as_deref(),
        Some("claude-code-cli")
    );
    assert_eq!(
        extract_agent_id("/pi/v1/chat/completions").as_deref(),
        Some("pi-cli")
    );
}

#[test]
fn prefix_less_and_unknown_are_none() {
    assert_eq!(extract_agent_id("/v1/messages"), None);
    assert_eq!(extract_agent_id("/unknown-agent/v1/messages"), None);
}