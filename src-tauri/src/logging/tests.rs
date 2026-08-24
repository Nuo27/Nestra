use super::*;

#[test]
fn rotate_previous_preserves_one_generation() {
    let tmp = tempfile::tempdir().unwrap();

    // No previous log: no-op, nothing created.
    rotate_previous(tmp.path());
    assert!(!tmp.path().join("nestra.log").exists());
    assert!(!tmp.path().join("nestra.log.1").exists());

    // Previous session's log becomes .1; an older .1 is replaced.
    std::fs::write(tmp.path().join("nestra.log"), "NEW-SESSION").unwrap();
    std::fs::write(tmp.path().join("nestra.log.1"), "TWO-SESSIONS-AGO").unwrap();
    rotate_previous(tmp.path());
    assert!(
        !tmp.path().join("nestra.log").exists(),
        "current moved aside for the fresh truncate-open"
    );
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("nestra.log.1")).unwrap(),
        "NEW-SESSION",
        "previous session survives, older generation dropped"
    );
}
