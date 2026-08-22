use super::*;
use std::sync::Mutex;

/// Env vars are process-global and tests run in parallel, so every test
/// that points the keychain at its own temp dir must hold this lock for
/// its whole duration.
static KEYCHAIN_DIR_LOCK: Mutex<()> = Mutex::new(());

/// RAII keychain override: sets `NESTRA_KEYCHAIN_DIR` to a fresh temp dir
/// and restores the previous value on drop. Holds the global lock for its
/// whole lifetime so parallel tests can't stomp each other's env var.
struct KeychainOverride {
    _guard: std::sync::MutexGuard<'static, ()>,
    _dir: tempfile::TempDir,
    prev: Option<std::ffi::OsString>,
}

impl KeychainOverride {
    fn new() -> Self {
        let guard = KEYCHAIN_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        let prev = std::env::var_os("NESTRA_KEYCHAIN_DIR");
        std::env::set_var("NESTRA_KEYCHAIN_DIR", dir.path());
        KeychainOverride { _guard: guard, _dir: dir, prev }
    }
}

impl Drop for KeychainOverride {
    fn drop(&mut self) {
        match &self.prev {
            Some(v) => std::env::set_var("NESTRA_KEYCHAIN_DIR", v),
            None => std::env::remove_var("NESTRA_KEYCHAIN_DIR"),
        }
    }
}

#[test]
fn cookie_round_trip_encrypts_and_decrypts() {
    // The OpenCode Go dashboard cookie is stored via the same AES-256-GCM
    // path as API keys. Round-trip pins the full set → get → overwrite →
    // delete lifecycle the creds editor relies on.
    let _ov = KeychainOverride::new();
    let key = format!("opencode-go-cookie-ep-{}", std::process::id());
    assert_eq!(get(&key).unwrap(), None);
    set(&key, "session-cookie").unwrap();
    assert_eq!(get(&key).unwrap().as_deref(), Some("session-cookie"));
    // Overwrite replaces the stored value, not the master key.
    set(&key, "rotated-cookie").unwrap();
    assert_eq!(get(&key).unwrap().as_deref(), Some("rotated-cookie"));
    delete(&key).unwrap();
    assert_eq!(get(&key).unwrap(), None);
    // Deleting a missing key is not an error.
    delete(&key).unwrap();
}

#[test]
fn empty_cookie_round_trips() {
    // An empty cookie still round-trips (the caller gates on emptiness);
    // `get` returns the stored empty string so the has_cookie probe stays
    // honest about what's on disk.
    let _ov = KeychainOverride::new();
    let key = format!("opencode-go-cookie-ep-empty-{}", std::process::id());
    set(&key, "").unwrap();
    assert_eq!(get(&key).unwrap().as_deref(), Some(""));
    delete(&key).unwrap();
}

#[test]
fn provider_key_path_rejects_unsafe_ids() {
    // The OpenCode Go cookie key is `opencode-go-cookie-{endpoint_id}`;
    // endpoint ids are [a-z0-9-]+ so it must always pass. Anything else
    // must be refused loudly rather than writing outside the keychain.
    assert!(provider_key_path("opencode-go-cookie-my-endpoint-1").is_ok());
    assert!(provider_key_path("master").is_err());
    assert!(provider_key_path("").is_err());
    assert!(provider_key_path("../../etc/passwd").is_err());
    assert!(provider_key_path("has space").is_err());
}