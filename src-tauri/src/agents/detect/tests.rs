use super::*;
use crate::agents::{agent_spec, agents, DetectSpec};
use std::fs;

fn isolated_dirs(_prefix: &str) -> (PlatformDirs, tempfile::TempDir, std::sync::MutexGuard<'static, ()>) {
    let guard = crate::HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let local = tmp.path().join("local");
    let roaming = tmp.path().join("roaming");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&local).unwrap();
    fs::create_dir_all(&roaming).unwrap();
    std::env::set_var("NESTRA_HOME_DIR", &home);
    std::env::set_var("NESTRA_LOCAL_APPDATA_DIR", &local);
    std::env::set_var("NESTRA_APPDATA_DIR", &roaming);
    (
        PlatformDirs {
            home,
            app_data: Some(roaming),
            local_app_data: Some(local),
        },
        tmp,
        guard,
    )
}

/// A bare-bones agent with no signal sources. Use this when you want to
/// assert "no signal found" — production agents always have at least a
/// binary candidate, and a developer's PATH may legitimately expose
/// `claude`/`opencode`, making the Missing branch unreachable for
/// real agents.
fn empty_agent() -> AgentSpec {
    test_agent("test-empty", DetectSpec {
        binary_candidates: &[],
        install_paths: &[],
        config_relative: None,
        skip_version_probe: false,
    })
}

/// Build a minimal AgentSpec with the given detect data. Probe only reads
/// `spec.detect`; the other fields are filled with harmless defaults.
fn test_agent(id: &'static str, detect: DetectSpec) -> AgentSpec {
    use crate::agents::{AgentKind, Capability, ConfigRef, SessionRef};
    AgentSpec {
        id,
        display_name: id,
        kind: id,
        agent_kind: AgentKind::Cli,
        detect,
        capability: Capability {
            manageable: false,
            supports_provider_configuration: false,
            supports_multiple_providers: false,
            supports_provider_injection: false,
            supports_factory_restore: false,
            supports_sessions: false,
            supports_mcp: false,
            supports_mcp_enabled: false,
            supports_skills: false,
            supports_gateway: false,
        },
        config: ConfigRef { writer: "", relative_path: "" },
        session: Some(SessionRef { reader: "", resume_command: None, unsupported_reason: None }),
        skill_dir: None,
        skill_name_matches_dir: false,
    }
}

#[test]
fn probe_reports_missing_when_no_signal() {
    let (_dirs, _tmp, _guard) = isolated_dirs("empty");
    let r = probe(&empty_agent(), None, None).unwrap();
    assert_eq!(r.status, ProbeStatus::Missing);
    assert!(r.cli_path.is_none());
}

#[test]
fn probe_finds_config_dir_signal() {
    let (dirs, _tmp, _guard) = isolated_dirs("config-only");
    let d = test_agent("test-cfg", DetectSpec {
        binary_candidates: &[],
        install_paths: &[],
        config_relative: Some(".claude"),
        skip_version_probe: false,
    });
    fs::create_dir_all(dirs.home.join(".claude")).unwrap();
    let r = probe(&d, None, None).unwrap();
    assert_eq!(r.status, ProbeStatus::Ok);
    assert!(r.cli_path.is_none());
    assert_eq!(r.config_path, Some(dirs.home.join(".claude")));
}

#[test]
fn override_valid_path_returns_ok_with_override_path() {
    let (_dirs, tmp, _guard) = isolated_dirs("override");
    let bin = tmp.path().join("my-claude");
    fs::write(&bin, "").unwrap();
    let r = probe(&empty_agent(), Some(&bin), None).unwrap();
    assert_eq!(r.status, ProbeStatus::Ok);
    assert_eq!(r.cli_path, Some(bin));
}

#[test]
fn override_missing_path_returns_manual_missing() {
    let (_dirs, _tmp, _guard) = isolated_dirs("override-missing");
    let bogus = std::path::PathBuf::from("/nonexistent/path/to/claude");
    let r = probe(&empty_agent(), Some(&bogus), None).unwrap();
    assert_eq!(r.status, ProbeStatus::ManualMissing);
    assert!(r.cli_path.is_none());
}

#[test]
fn opencode_desktop_found_via_app_data() {
    let (dirs, _tmp, _guard) = isolated_dirs("opencode-desktop-appdata");
    let app_data = dirs.app_data.as_ref().unwrap();
    fs::create_dir_all(app_data.join("OpenCode")).unwrap();
    let d = agent_spec("opencode-desktop").unwrap();
    let r = probe(d, None, None).unwrap();
    assert_eq!(r.status, ProbeStatus::Ok);
}

#[test]
fn detector_registry_has_four_entries() {
    assert_eq!(agents().len(), 4);
    let ids: Vec<&str> = agents().iter().map(|d| d.id).collect();
    assert!(ids.contains(&"claude-code-cli"));
    assert!(ids.contains(&"opencode-desktop"));
    assert!(ids.contains(&"pi-cli"));
    assert!(ids.contains(&"zcode-desktop"));
}