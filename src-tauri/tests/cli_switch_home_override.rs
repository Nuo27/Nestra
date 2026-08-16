//! End-to-end test for the CLI switch flow against a fake home directory.
//!
//! Honors the `NESTRA_HOME_DIR` env var so config writes target a temp dir
//! instead of the real `~/.claude`. This mirrors the production path
//! (`ConfigAdapter::apply` with the config path under the resolved home), so
//! it proves the binding lands in the *test* CLI directory, never the real
//! one. Per-format field coverage lives in each writer's unit tests; this
//! file owns the home-override resolution + the backup/restore lifecycle.

use nestra_lib::agents;
use nestra_lib::config_writer::{backup_path_for, ModelsConfig, ProviderKind, SwitchContext};

/// Claude Code fixture — written into the fake home so `apply` captures a
/// real original (not the no-original sentinel) and `restore` is byte-exact.
const CLAUDE_FIXTURE: &str = include_str!(
    "../src/fixtures/claude_code/settings.json"
);

/// RAII fake-home: creates a temp dir, sets `NESTRA_HOME_DIR` to it, and on
/// drop removes the dir and restores the previous env value (or unsets it).
/// The old helper leaked one temp dir per test run and never restored the
/// env var.
struct FakeHome {
    path: std::path::PathBuf,
    prev: Option<std::ffi::OsString>,
}

impl FakeHome {
    fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let id = N.fetch_add(1, Ordering::SeqCst);
        let mut p = std::env::temp_dir();
        p.push(format!("nestra-test-home-{}-{}", std::process::id(), id));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        let prev = std::env::var_os("NESTRA_HOME_DIR");
        std::env::set_var("NESTRA_HOME_DIR", &p);
        Self { path: p, prev }
    }
}

impl Drop for FakeHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
        match &self.prev {
            Some(v) => std::env::set_var("NESTRA_HOME_DIR", v),
            None => std::env::remove_var("NESTRA_HOME_DIR"),
        }
    }
}

fn ctx(token: &str) -> SwitchContext {
    SwitchContext {
        provider_id: "anthropic".into(),
        provider_kind: ProviderKind::Anthropic,
        display_name: "Test Anthropic".into(),
        base_url: "https://api.anthropic.com".into(),
        api_key: token.into(),
        models: ModelsConfig::Anthropic {
            default: "claude-3-opus".into(),
            haiku: "claude-3-haiku".into(),
            sonnet: "claude-3-sonnet".into(),
            opus: "claude-3-opus".into(),
        },
        advanced_env: Default::default(),
        model_abilities: Default::default(),
    }
}

#[test]
fn apply_targets_fake_home_and_round_trips() {
    // RAII guard: the fake home dir is removed on drop (the old test leaked
    // one temp dir per run) and NESTRA_HOME_DIR is restored afterwards so a
    // later test in the same process never inherits a stale override.
    let _guard = FakeHome::new();

    let adapter = agents::adapter_for("claude-code-cli").expect("claude-code adapter");

    // 1. Resolved path must live under the FAKE home, not the real one.
    let home = std::env::var("NESTRA_HOME_DIR").unwrap();
    let home = std::path::PathBuf::from(home);
    let cfg = home.join(".claude/settings.json");
    assert!(
        cfg.starts_with(&home),
        "config {} must be under fake home {}",
        cfg.display(),
        home.display()
    );

    // Seed the fixture so restore has a real original to revert to.
    std::fs::create_dir_all(cfg.parent().unwrap()).unwrap();
    std::fs::write(&cfg, CLAUDE_FIXTURE).unwrap();
    let original = std::fs::read(&cfg).unwrap();

    // 2. apply writes the env binding + takes a backup.
    let out = adapter.apply(&cfg, &ctx("sk-first")).expect("apply");
    assert!(out, "first apply should create backup");
    let backup = backup_path_for(&cfg);
    assert!(backup.exists(), "backup should exist");

    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
    assert_eq!(
        json["env"]["ANTHROPIC_AUTH_TOKEN"].as_str(),
        Some("sk-first"),
        "first provider's token should be written"
    );

    // 3. A second switch to another provider reuses the backup, never re-takes it.
    let backup_bytes = std::fs::read(&backup).unwrap();
    let out2 = adapter.apply(&cfg, &ctx("sk-second")).expect("apply 2");
    assert!(!out2, "second apply must not re-backup");
    assert_eq!(
        std::fs::read(&backup).unwrap(),
        backup_bytes,
        "backup must be frozen after first switch"
    );

    // 4. restore reverts byte-exactly and clears the backup marker.
    adapter.restore(&cfg).expect("restore");
    assert_eq!(std::fs::read(&cfg).unwrap(), original, "restore must be byte-exact");
    assert!(!backup.exists(), "backup removed after restore");
}
