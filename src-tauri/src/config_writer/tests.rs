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
fn factory_restore_is_repeatable_and_preserves_snapshot() {
    let (tmp, _tmp_g) = tempfile_dir();
    let cfg = tmp.join("cfg.json");
    fs::write(&cfg, "ORIG").unwrap();
    capture_factory(&cfg, false).unwrap();
    fs::write(&cfg, "NESTRA-OWNED").unwrap();

    restore_factory(&cfg).unwrap();
    assert_eq!(fs::read_to_string(&cfg).unwrap(), "ORIG");
    assert!(factory_path_for(&cfg).exists(), "factory preserved");

    // mutate + restore again —still works, snapshot untouched
    fs::write(&cfg, "NESTRA-AGAIN").unwrap();
    restore_factory(&cfg).unwrap();
    assert_eq!(fs::read_to_string(&cfg).unwrap(), "ORIG");
}

#[test]
fn factory_restore_deletes_file_when_no_original_existed() {
    let (tmp, _tmp_g) = tempfile_dir();
    let cfg = tmp.join("cfg.json");
    capture_factory(&cfg, false).unwrap(); // sentinel
    fs::write(&cfg, "{}").unwrap();
    restore_factory(&cfg).unwrap();
    assert!(!cfg.exists());
    // factory file (sentinel) still around for a repeat restore
    assert!(factory_path_for(&cfg).exists());
}

#[test]
fn factory_restore_without_snapshot_errors() {
    let (tmp, _tmp_g) = tempfile_dir();
    let cfg = tmp.join("cfg.json");
    assert!(restore_factory(&cfg).is_err());
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