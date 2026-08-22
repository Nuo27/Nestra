use super::*;

#[test]
fn registry_has_four_agents() {
    assert_eq!(agents().len(), 4);
}

#[test]
fn agent_ids_are_unique() {
    let mut ids: Vec<&str> = agents().iter().map(|a| a.id).collect();
    ids.sort();
    let n = ids.len();
    ids.dedup();
    assert_eq!(ids.len(), n, "duplicate agent id");
}

#[test]
fn manageable_agents_have_nonempty_writer_and_path() {
    for a in agents() {
        assert!(a.manageable(), "{} should be manageable", a.id);
        assert!(!a.config.relative_path.is_empty(), "{} manageable but no path", a.id);
    }
}

#[test]
fn resumable_agents_have_resume_command_and_reader() {
    for a in agents() {
        if let Some(s) = a.session {
            if s.resume_command.is_some() {
                assert!(s.unsupported_reason.is_none(), "{} has both resume and reason", a.id);
                assert!(!s.reader.is_empty());
            }
        }
    }
}

#[test]
fn known_agents_present() {
    let ids: Vec<&str> = agents().iter().map(|a| a.id).collect();
    for expected in ["claude-code-cli", "opencode-desktop", "pi-cli", "zcode-desktop"] {
        assert!(ids.contains(&expected), "missing {expected}");
    }
}

/// Agent ids follow the naming convention: CLI agents end in `-cli`,
/// desktop agents in `-desktop`.
#[test]
fn agent_ids_follow_kind_suffix_convention() {
    for a in agents() {
        let expected_suffix = match a.agent_kind {
            AgentKind::Cli => "-cli",
            AgentKind::Desktop => "-desktop",
        };
        assert!(
            a.id.ends_with(expected_suffix),
            "{} violates the agent-id naming convention (expected {expected_suffix} suffix)",
            a.id
        );
    }
}