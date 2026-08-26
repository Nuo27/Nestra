use super::*;
use std::fs;

#[test]
fn gateway_alias_debug_redacts_sentinel_key() {
    let alias = GatewayAlias::simple(
        "http://127.0.0.1:18777/claude-code-cli",
        "claude-haiku-4-5",
        "super-secret-token-value",
    );
    let s = format!("{alias:?}");
    assert!(s.contains("sentinel_key"), "field name should appear");
    assert!(
        !s.contains("super-secret-token-value"),
        "Debug must NOT leak the token; got: {s}"
    );
    assert!(s.contains("<redacted>"));
}

#[test]
fn switch_context_debug_redacts_api_key() {
    let ctx = SwitchContext {
        provider_id: "z-ai".into(),
        provider_kind: ProviderKind::Anthropic,
        display_name: "Z.ai".into(),
        base_url: "https://api.z.ai".into(),
        api_key: "sk-super-secret-value".into(),
        models: ModelsConfig::Anthropic {
            default: "glm-4.7".into(),
            haiku: "glm-4.7-air".into(),
            sonnet: "glm-4.7".into(),
            opus: "glm-4.7-plus".into(),
        },
        advanced_env: Default::default(),
        model_abilities: Default::default(),
    };
    let s = format!("{ctx:?}");
    assert!(s.contains("api_key"), "field name should appear");
    assert!(
        !s.contains("sk-super-secret-value"),
        "Debug must NOT leak the api key; got: {s}"
    );
    assert!(s.contains("<redacted>"));
}

#[test]
fn json_serialize_preserves_key_order_probe() {
    // Probe: serde_json is built WITH `preserve_order` (Map = IndexMap —
    // parse→modify→serialize keeps the document's original key order).
    // This is load-bearing for the gateway: `rewrite_model` re-serializes
    // request bodies, and order-sensitive upstreams (opencode-go's MiniMax
    // backend rejects alphabetized message objects with
    // "[1214] Incorrect role information") must see the agent's own field
    // order. Config writers also benefit: re-writing a user-maintained
    // config keeps the user's key order instead of re-sorting the file.
    let v: serde_json::Value =
        serde_json::from_str(r#"{"z":1,"a":2,"m":{"y":1,"b":2}}"#).unwrap();
    let out = serde_json::to_string(&v).unwrap();
    assert_eq!(out, r#"{"z":1,"a":2,"m":{"y":1,"b":2}}"#);
}

#[test]
fn backup_taken_once_then_reused() {
    let (tmp, _tmp_g) = tempfile_dir();
    let cfg = tmp.join("cfg.json");
    fs::write(&cfg, "{ \"orig\": true }").unwrap();

    assert!(ensure_backup(&cfg).unwrap()); // created
    assert!(!ensure_backup(&cfg).unwrap()); // reused, not re-captured

    // Mutate live file; backup must still hold the original.
    fs::write(&cfg, "{ \"changed\": true }").unwrap();
    let backup_content = fs::read_to_string(backup_path_for(&cfg)).unwrap();
    assert!(backup_content.contains("orig"));
}

#[test]
fn restore_reverts_to_original() {
    let (tmp, _tmp_g) = tempfile_dir();
    let cfg = tmp.join("cfg.json");
    fs::write(&cfg, "ORIGINAL").unwrap();
    ensure_backup(&cfg).unwrap();
    fs::write(&cfg, "NESTRA-OWNED").unwrap();

    restore_from_backup(&cfg).unwrap();
    assert_eq!(fs::read_to_string(&cfg).unwrap(), "ORIGINAL");
    assert!(!backup_path_for(&cfg).exists());
}

#[test]
fn restore_deletes_file_when_no_original_existed() {
    let (tmp, _tmp_g) = tempfile_dir();
    let cfg = tmp.join("cfg.json");
    // No original; apply would create the sentinel backup + write live.
    assert!(ensure_backup(&cfg).unwrap());
    fs::write(&cfg, "{}").unwrap();

    restore_from_backup(&cfg).unwrap();
    assert!(!cfg.exists());
}

#[test]
fn restore_without_backup_errors() {
    let (tmp, _tmp_g) = tempfile_dir();
    let cfg = tmp.join("cfg.json");
    assert!(restore_from_backup(&cfg).is_err());
}

#[test]
fn factory_captured_once_then_preserved() {
    let (tmp, _tmp_g) = tempfile_dir();
    let cfg = tmp.join("cfg.json");
    fs::write(&cfg, "FACTORY-ORIG").unwrap();

    capture_factory(&cfg, false).unwrap();
    // second non-forced capture is a no-op even after the live file changes
    fs::write(&cfg, "CHANGED").unwrap();
    capture_factory(&cfg, false).unwrap();
    assert_eq!(
        fs::read_to_string(factory_path_for(&cfg)).unwrap(),
        "FACTORY-ORIG"
    );
}

#[test]
fn factory_force_overwrites() {
    let (tmp, _tmp_g) = tempfile_dir();
    let cfg = tmp.join("cfg.json");
    fs::write(&cfg, "FIRST").unwrap();
    capture_factory(&cfg, false).unwrap();
    fs::write(&cfg, "SECOND").unwrap();
    capture_factory(&cfg, true).unwrap();
    assert_eq!(
        fs::read_to_string(factory_path_for(&cfg)).unwrap(),
        "SECOND"
    );
}

#[test]
fn atomic_write_replaces_existing_and_leaves_no_temp() {
    let (tmp, _tmp_g) = tempfile_dir();
    let cfg = tmp.join("cfg.json");
    fs::write(&cfg, "old").unwrap();
    atomic_write(&cfg, b"new").unwrap();
    assert_eq!(fs::read_to_string(&cfg).unwrap(), "new");
    // only the target remains —no leftover temp from a partial write
    assert_eq!(fs::read_dir(&tmp).unwrap().count(), 1);
}

#[test]
fn atomic_write_creates_parent_dirs() {
    let (tmp, _tmp_g) = tempfile_dir();
    let cfg = tmp.join("nested").join("deep").join("cfg.json");
    atomic_write(&cfg, b"x").unwrap();
    assert_eq!(fs::read_to_string(&cfg).unwrap(), "x");
}

/// A persistent rename failure (here: the target is a directory, which no
/// amount of backoff fixes) must exhaust the retry ladder, surface the error,
/// and clean the temp file — the contract the Windows sharing-violation
/// backoff relies on for its bounded-failure path.
#[test]
fn atomic_write_rename_retry_exhaustion_surfaces_error_and_cleans_temp() {
    let (tmp, _tmp_g) = tempfile_dir();
    let target = tmp.join("occupied");
    fs::create_dir(&target).unwrap();
    assert!(atomic_write(&target, b"x").is_err());
    // Only the (untouched) directory remains — the temp was removed.
    let entries: Vec<_> = fs::read_dir(&tmp).unwrap().collect();
    assert_eq!(entries.len(), 1, "temp file must not survive a failed write");
    assert!(entries[0].as_ref().unwrap().path().is_dir());
}

// ---- switch transactions (apply_set_atomic) --------------------------------

/// Two-file mock adapter (main + sibling `auth.json`, the Pi shape). `fail_at`
/// injects a failure after that many files were written, exercising the
/// rollback contract's restore/remove paths.
struct TwoFileMock {
    fail_at: usize,
}

impl ConfigAdapter for TwoFileMock {
    fn accepts(&self) -> &'static [ProviderKind] {
        &[ProviderKind::Custom]
    }
    fn model_selection(&self) -> ModelSelection {
        ModelSelection::FreeForm
    }
    fn apply_set(&self, config_path: &Path, _set: &ProviderSet) -> AppResult<bool> {
        let extra = extra_path(config_path);
        atomic_write(config_path, b"NEW-MAIN")?;
        if self.fail_at == 1 {
            return Err(AppError::Validation("injected mid-switch failure".into()));
        }
        atomic_write(&extra, b"NEW-EXTRA")?;
        if self.fail_at == 2 {
            return Err(AppError::Validation("injected mid-switch failure".into()));
        }
        Ok(true)
    }
    fn restore(&self, _config_path: &Path) -> AppResult<()> {
        Ok(())
    }
    fn extra_config_paths(&self, config_path: &Path) -> Vec<PathBuf> {
        vec![extra_path(config_path)]
    }
}

fn extra_path(config_path: &Path) -> PathBuf {
    config_path.with_file_name("auth.json")
}

fn empty_set() -> ProviderSet {
    ProviderSet {
        entries: vec![],
        default_provider_id: String::new(),
        default_model: String::new(),
    }
}

#[test]
fn apply_set_atomic_rolls_back_every_file_on_mid_switch_failure() {
    let (tmp, _tmp_g) = tempfile_dir();
    let cfg = tmp.join("models.json");
    fs::write(&cfg, "OLD-MAIN").unwrap();
    fs::write(&extra_path(&cfg), "OLD-EXTRA").unwrap();

    let err = apply_set_atomic(&TwoFileMock { fail_at: 1 }, &cfg, &empty_set())
        .expect_err("injected failure must surface");
    assert!(err.to_string().contains("injected"));

    // Both files hold their PRE-SWITCH bytes — not the pre-Nestra backup.
    assert_eq!(fs::read_to_string(&cfg).unwrap(), "OLD-MAIN");
    assert_eq!(fs::read_to_string(&extra_path(&cfg)).unwrap(), "OLD-EXTRA");
    assert_eq!(fs::read_dir(&tmp).unwrap().count(), 2, "no temp residue");
}

#[test]
fn apply_set_atomic_removes_files_the_failed_switch_created() {
    let (tmp, _tmp_g) = tempfile_dir();
    let cfg = tmp.join("models.json");
    fs::write(&cfg, "OLD-MAIN").unwrap();
    // auth.json did not exist; the mock writes it, then fails.

    apply_set_atomic(&TwoFileMock { fail_at: 2 }, &cfg, &empty_set())
        .expect_err("injected failure must surface");

    assert_eq!(fs::read_to_string(&cfg).unwrap(), "OLD-MAIN");
    assert!(!extra_path(&cfg).exists(), "created file must be removed");
}

#[test]
fn apply_set_atomic_success_writes_through_without_rollback() {
    let (tmp, _tmp_g) = tempfile_dir();
    let cfg = tmp.join("models.json");
    fs::write(&cfg, "OLD-MAIN").unwrap();

    assert!(apply_set_atomic(&TwoFileMock { fail_at: 0 }, &cfg, &empty_set()).unwrap());

    assert_eq!(fs::read_to_string(&cfg).unwrap(), "NEW-MAIN");
    assert_eq!(fs::read_to_string(&extra_path(&cfg)).unwrap(), "NEW-EXTRA");
}

/// Returns `(path, guard)`: the guard deletes the directory when it
/// drops (end of the test fn, including panics). The old hand-rolled
/// dirs leaked thousands of `nestra-cfg-test-*` folders in the system
/// temp dir (process-exit cleanup doesn't run under `cargo test`).
fn tempfile_dir() -> (PathBuf, tempfile::TempDir) {
    let dir = tempfile::Builder::new()
        .prefix("nestra-cfg-test-")
        .tempdir()
        .expect("tempdir");
    (dir.path().to_path_buf(), dir)
}