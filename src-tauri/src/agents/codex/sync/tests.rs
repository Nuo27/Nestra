use super::*;
use std::fs;
use std::path::PathBuf;

fn tmp_home(tag: &str) -> (PathBuf, tempfile::TempDir) {
    let dir = tempfile::Builder::new()
        .prefix(tag)
        .tempdir()
        .expect("tempdir");
    (dir.path().to_path_buf(), dir)
}

fn rollout_line(ty: &str, payload: serde_json::Value) -> String {
    serde_json::json!({
        "timestamp": "2026-08-01T10:00:00.0000000Z",
        "type": ty,
        "payload": payload,
    })
    .to_string()
}

fn meta_line(provider: &str) -> String {
    rollout_line(
        "session_meta",
        serde_json::json!({
            "session_id": "aaa-bbb",
            "id": "019f8dad-0ee5-75d0-baa3-0cee3c16b301",
            "cwd": "C:\\repo",
            "model_provider": provider,
        }),
    )
}

#[test]
fn rewrites_rollout_first_line_only() {
    let (home, _g) = tmp_home("codex-sync-ro");
    let dir = home.join("sessions").join("2026").join("08").join("01");
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("rollout-2026-08-01T10-00-00-019f8dad.jsonl");
    let body = rollout_line(
        "response_item",
        serde_json::json!({
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": "hi" }],
        }),
    );
    fs::write(&file, format!("{}\n{}\n", meta_line("custom"), body)).unwrap();

    sync_provider_visibility(&home.join("config.toml"), "nestra-zai");

    let out = fs::read_to_string(&file).unwrap();
    let (first, rest) = out.split_once('\n').unwrap();
    let v: serde_json::Value = serde_json::from_str(first).unwrap();
    assert_eq!(
        v["payload"]["model_provider"].as_str(),
        Some("nestra-zai"),
        "first line provider rewritten"
    );
    assert_eq!(rest.trim(), body, "transcript body untouched");
}

#[test]
fn already_matching_rollout_is_untouched() {
    let (home, _g) = tmp_home("codex-sync-noop");
    let dir = home.join("sessions").join("2026").join("08");
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("rollout-x.jsonl");
    let before = format!("{}\n", meta_line("nestra-zai"));
    fs::write(&file, &before).unwrap();
    sync_provider_visibility(&home.join("config.toml"), "nestra-zai");
    assert_eq!(fs::read_to_string(&file).unwrap(), before, "no rewrite");
}

#[test]
fn updates_threads_table_with_backup() {
    let (home, _g) = tmp_home("codex-sync-db");
    fs::create_dir_all(home.join("sqlite")).unwrap();
    let db = home.join("sqlite").join("codex-dev.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute(
        "CREATE TABLE threads (id TEXT PRIMARY KEY, model_provider TEXT, cwd TEXT)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO threads (id, model_provider, cwd) VALUES ('t1', 'custom', 'C:\\repo'), ('t2', 'nestra-zai', 'C:\\repo')",
        [],
    )
    .unwrap();
    drop(conn);

    sync_provider_visibility(&home.join("config.toml"), "nestra-zai");

    let backup = db.with_extension("db.nestra-backup");
    assert!(backup.exists(), "one-time DB backup created");
    let conn = rusqlite::Connection::open(&db).unwrap();
    let providers: Vec<String> = {
        let mut stmt = conn.prepare("SELECT model_provider FROM threads ORDER BY id").unwrap();
        stmt.query_map([], |r| r.get::<_, Option<String>>(0))
            .unwrap()
            .map(|r| r.unwrap().unwrap())
            .collect()
    };
    assert_eq!(providers, vec!["nestra-zai".to_string(), "nestra-zai".to_string()]);
}

#[test]
fn missing_everything_is_silent_noop() {
    let (home, _g) = tmp_home("codex-sync-empty");
    // No sessions dir, no sqlite dir, no state_5.sqlite — must not error.
    sync_provider_visibility(&home.join("config.toml"), "nestra-zai");
}

#[test]
fn db_without_threads_table_is_skipped() {
    let (home, _g) = tmp_home("codex-sync-notable");
    fs::create_dir_all(home.join("sqlite")).unwrap();
    let db = home.join("sqlite").join("codex-dev.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute("CREATE TABLE other (x INTEGER)", []).unwrap();
    drop(conn);
    // "no such table: threads" is warned + skipped, not propagated.
    sync_provider_visibility(&home.join("config.toml"), "nestra-zai");
}
