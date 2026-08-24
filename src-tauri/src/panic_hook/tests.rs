use super::*;

#[test]
fn crash_entry_lands_in_target_dir_with_payload_and_location() {
    let tmp = tempfile::tempdir().unwrap();
    install_at(tmp.path().to_path_buf());

    let _ = std::panic::catch_unwind(|| panic!("boom-crash-log-test"));

    let content =
        std::fs::read_to_string(tmp.path().join("crash.log")).expect("crash.log written");
    assert!(content.contains("boom-crash-log-test"), "payload captured");
    assert!(content.contains("panic_hook"), "location names this file");
    assert!(content.contains("panicked at"), "entry is formatted");
}
