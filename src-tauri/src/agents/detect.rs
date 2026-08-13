//! Provider-aware coding-agent detection. Each agent declares its probe
//! strategy in [`crate::agents::AgentSpec`]: PATH candidates, known install
//! paths, and a config-dir soft signal. Manual overrides (set via
//! `agent_set_override`) short-circuit the algorithm.

use crate::agents::{AgentSpec, DetectorPath};
use crate::db::{self, PlatformDirs};
use crate::error::AppResult;
use std::path::{Path, PathBuf};

/// How long a single `--version` probe may run before we kill it. Real CLIs
/// answer in well under a second; a hung one must never stall the UI or DB.
const VERSION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Result of a probe. `cli_path` is the resolved executable when one was
/// found; `version` is the `--version` stdout when capture succeeded.
#[derive(Debug, Default)]
pub struct ProbeResult {
    pub status: ProbeStatus,
    pub cli_path: Option<PathBuf>,
    pub config_path: Option<PathBuf>,
    pub installed_version: Option<String>,
}

#[derive(Debug, PartialEq, Eq, Default)]
pub enum ProbeStatus {
    /// Found via auto-detection (PATH, install dir, or config-dir signal).
    #[default]
    Ok,
    /// No signal found.
    Missing,
    /// Manual override is set but the file no longer exists.
    ManualMissing,
}

/// Run the detection algorithm for one agent. `cli_path_override` and
/// `config_path_override` come from the `cli` row; when the user has set
/// them, they short-circuit auto-detection.
pub fn probe(
    spec: &AgentSpec,
    cli_path_override: Option<&Path>,
    config_path_override: Option<&Path>,
) -> AppResult<ProbeResult> {
    let detect = &spec.detect;

    // 1. Manual override takes priority.
    if let Some(override_path) = cli_path_override {
        if override_path.exists() {
            let version = if detect.skip_version_probe {
                None
            } else {
                capture_version(override_path)
            };
            return Ok(ProbeResult {
                status: ProbeStatus::Ok,
                cli_path: Some(override_path.to_path_buf()),
                config_path: config_path_override.map(Path::to_path_buf),
                installed_version: version,
            });
        }
        return Ok(ProbeResult {
            status: ProbeStatus::ManualMissing,
            cli_path: None,
            config_path: config_path_override.map(Path::to_path_buf),
            installed_version: None,
        });
    }

    let dirs = db::platform_dirs()?;

    // 2. PATH candidates.
    for name in detect.binary_candidates {
        if let Some(found) = which::which(name).ok() {
            let version = if detect.skip_version_probe {
                None
            } else {
                capture_version(&found)
            };
            // A PATH hit is a real binary — infer config_path from the
            // config-relative rule exactly like the install-dir branch does
            // (the old code left it None, so the UI showed no config).
            let config_path = config_path_override
                .map(Path::to_path_buf)
                .or_else(|| detect.config_relative.map(|r| dirs.home.join(r)));
            return Ok(ProbeResult {
                status: ProbeStatus::Ok,
                cli_path: Some(found),
                config_path,
                installed_version: version,
            });
        }
    }

    // 3. Install paths.
    for path in detect.install_paths {
        if let Some(resolved) = resolve_install_path(path, &dirs) {
            // `is_file` (not `exists`): a DIRECTORY named like the binary
            // must not count as a valid CLI install. Exception: agents with
            // a config-relative rule (desktop GUI apps like opencode-desktop,
            // detected by their app-data directory) legitimately resolve to
            // a directory.
            let valid = resolved.is_file() || detect.config_relative.is_some();
            if valid {
                let version = if detect.skip_version_probe {
                    None
                } else {
                    capture_version(&resolved)
                };
                let config_path = config_path_override
                    .map(Path::to_path_buf)
                    .or_else(|| detect.config_relative.map(|r| dirs.home.join(r)));
                return Ok(ProbeResult {
                    status: ProbeStatus::Ok,
                    cli_path: Some(resolved),
                    config_path,
                    installed_version: version,
                });
            }
        }
    }

    // 4. Config-dir soft signal.
    if let Some(rel) = detect.config_relative {
        let candidate = dirs.home.join(rel);
        if candidate.exists() {
            let config_path = config_path_override
                .map(Path::to_path_buf)
                .unwrap_or(candidate);
            return Ok(ProbeResult {
                status: ProbeStatus::Ok,
                cli_path: None,
                config_path: Some(config_path),
                installed_version: None,
            });
        }
    }

    Ok(ProbeResult {
        status: ProbeStatus::Missing,
        cli_path: None,
        config_path: config_path_override.map(Path::to_path_buf),
        installed_version: None,
    })
}

fn resolve_install_path(path: &DetectorPath, dirs: &PlatformDirs) -> Option<PathBuf> {
    match path {
        DetectorPath::PlatformLocalAppData(rel) => dirs.local_app_data.as_ref().map(|p| p.join(rel)),
        DetectorPath::PlatformAppData(rel) => dirs.app_data.as_ref().map(|p| p.join(rel)),
        DetectorPath::HomeRelative(rel) => Some(dirs.home.join(rel)),
        DetectorPath::Absolute(p) => Some(PathBuf::from(p)),
        DetectorPath::WindowsAppsGlob { prefix, suffix } => resolve_appx_package(prefix, suffix),
    }
}

/// Resolve an MSIX/Store-packaged app's exe. `prefix` is the package family
/// prefix, `suffix` is the exe path inside the package folder.
///
/// Two strategies, fast-then-reliable:
/// 1. **Glob** `C:\Program Files\WindowsApps\<prefix>*\<suffix>` — works only
///    for callers with list permission on that locked-down directory (admins).
/// 2. **Fallback:** `Get-AppxPackage` (the official WinRT API) returns the
///    install location for any user. The directory itself is not listable by
///    normal users, so step 1 returns `None` and we fall through here. ~500ms,
///    paid once per detection cycle — not a hot path.
///
/// On non-Windows both paths return `None`.
fn resolve_appx_package(prefix: &str, suffix: &str) -> Option<PathBuf> {
    let base = Path::new(r"C:\Program Files\WindowsApps");
    if let Some(found) = windows_apps_glob(base, prefix, suffix) {
        return Some(found);
    }
    let install_root = appx_install_location(prefix)?;
    let candidate = install_root.join(suffix);
    candidate.exists().then_some(candidate)
}

/// Glob `<base>\<prefix>*\<suffix>` for the first existing match. `base` is a
/// parameter so the matcher is unit-testable against a tempdir; production
/// callers pass the real `C:\Program Files\WindowsApps`. Returns `None` when
/// `base` is unreadable or no match exists — including on non-Windows where
/// the path never resolves.
fn windows_apps_glob(base: &Path, prefix: &str, suffix: &str) -> Option<PathBuf> {
    let read = match std::fs::read_dir(base) {
        Ok(r) => r,
        Err(_) => return None,
    };
    for entry in read.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(prefix) {
            continue;
        }
        let candidate = entry.path().join(suffix);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Query `Get-AppxPackage` for the install location of the package whose
/// family name starts with `prefix` (trailing `_` stripped). Windows-only;
/// returns `None` elsewhere or when PowerShell / the package is unavailable.
#[cfg(target_os = "windows")]
fn appx_install_location(prefix: &str) -> Option<PathBuf> {
    // Family name = `prefix` without the trailing `_`.
    let family = prefix.trim_end_matches('_');
    let script = format!(
        "Get-AppxPackage -Name '{family}' | Select-Object -First 1 -ExpandProperty InstallLocation"
    );
    // `Command::output()` blocks unboundedly — a hung PowerShell holds up
    // detection (which can run while the DB lock is held). Run it on a
    // helper thread with the same deadline as version probing.
    let (tx, rx) = std::sync::mpsc::channel::<Option<String>>();
    std::thread::spawn(move || {
        let out = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
        let _ = tx.send(out);
    });
    let line = rx.recv_timeout(VERSION_TIMEOUT).ok().flatten()?;
    let path = line.lines().find(|l| !l.trim().is_empty())?.trim();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

#[cfg(not(target_os = "windows"))]
fn appx_install_location(_prefix: &str) -> Option<PathBuf> {
    None
}

/// Spawn `<path> --version` with a tight timeout and capture stdout. A
/// failure is non-fatal — detection still reports `Ok` without a version.
///
/// The whole spawn+wait+read sequence runs on a helper thread so the deadline
/// covers *both* stages: `read_to_string` would otherwise block forever when a
/// daemonized child keeps the stdout pipe's write end open, and this probe can
/// run while `detect_all_agents` holds the DB lock — one stall would hang all
/// database access. Best-effort; on timeout the direct child is killed.
fn capture_version(path: &Path) -> Option<String> {
    use std::io::Read;
    use std::time::Instant;

    let path = path.to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = (|| -> Option<String> {
            let mut child = std::process::Command::new(&path)
                .arg("--version")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .stdin(std::process::Stdio::null())
                .spawn()
                .ok()?;
            let started = Instant::now();
            // Poll `try_wait` so a hung binary can't block the helper thread
            // (or, via `recv_timeout`, the caller holding the DB lock) forever.
            let status = loop {
                match child.try_wait() {
                    Ok(Some(st)) => break st,
                    Ok(None) => {
                        if started.elapsed() > VERSION_TIMEOUT {
                            let _ = child.kill();
                            let _ = child.wait();
                            return None;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(_) => return None,
                }
            };
            if !status.success() {
                return None;
            }
            let mut out = String::new();
            if let Some(mut stdout) = child.stdout.take() {
                let _ = stdout.read_to_string(&mut out);
            }
            let v = out.trim().to_string();
            (!v.is_empty()).then_some(v)
        })();
        let _ = tx.send(result);
    });
    rx.recv_timeout(VERSION_TIMEOUT).ok().flatten()
}

#[cfg(test)]
mod tests {
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
    fn detector_registry_has_three_entries() {
        assert_eq!(agents().len(), 3);
        let ids: Vec<&str> = agents().iter().map(|d| d.id).collect();
        assert!(ids.contains(&"claude-code"));
        assert!(ids.contains(&"opencode-desktop"));
        assert!(ids.contains(&"pi"));
    }
}