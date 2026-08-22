use super::*;

/// The "unknown agent" failure class is registry drift: the UI offers
/// agents from `agents::agents()` (spec.rs) while the MCP writers live in
/// this module. These two lists must never diverge — a spec agent without
/// an MCP provider renders toggles that always fail with "unknown agent".
#[test]
fn every_registry_agent_has_an_mcp_provider_and_vice_versa() {
    let registry_ids: Vec<String> = crate::agents::agents()
        .iter()
        .map(|a| a.id.to_string())
        .collect();
    let provider_ids: Vec<String> = all().iter().map(|p| p.agent_id().to_string()).collect();
    for id in &registry_ids {
        assert!(
            provider_ids.contains(id),
            "registry agent {id} has no MCP provider — its toggles fail with 'unknown agent'"
        );
    }
    for id in &provider_ids {
        assert!(
            registry_ids.contains(id),
            "MCP provider {id} is not a registry agent — agent_list never surfaces it"
        );
    }
    assert_eq!(registry_ids.len(), provider_ids.len());
}