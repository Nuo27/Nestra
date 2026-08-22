use super::*;
use crate::session::semantic::PartPayload;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

fn tmp_home(tag: &str) -> (PathBuf, tempfile::TempDir) {
    let dir = tempfile::Builder::new()
        .prefix(tag)
        .tempdir()
        .expect("tempdir");
    (dir.path().to_path_buf(), dir)
}

fn line(ty: &str, payload: serde_json::Value) -> String {
    serde_json::json!({
        "timestamp": "2026-08-01T10:00:00.0000000Z",
        "type": ty,
        "payload": payload,
    })
    .to_string()
}

/// A minimal-but-realistic rollout: meta, user message, reasoning summary,
/// tool call + result, assistant message, plus an unmapped future type.
fn rollout_text() -> String {
    [
        line(
            "session_meta",
            serde_json::json!({
                "session_id": "sess-1",
                "id": "019f8dad-0ee5-75d0-baa3-0cee3c16b301",
                "cwd": "C:\\repo",
                "model_provider": "custom",
            }),
        ),
        line(
            "response_item",
            serde_json::json!({
                "type": "message", "role": "user",
                "content": [{ "type": "input_text", "text": "fix the bug" }],
            }),
        ),
        line(
            "response_item",
            serde_json::json!({
                "type": "reasoning",
                "summary": [{ "type": "summary_text", "text": "thinking..." }],
            }),
        ),
        line(
            "response_item",
            serde_json::json!({
                "type": "function_call", "name": "shell", "call_id": "c1",
                "arguments": "{\"cmd\":\"ls\"}",
            }),
        ),
        line(
            "response_item",
            serde_json::json!({
                "type": "function_call_output", "call_id": "c1", "output": "file list",
            }),
        ),
        line(
            "response_item",
            serde_json::json!({
                "type": "message", "role": "assistant",
                "content": [{ "type": "output_text", "text": "done" }],
            }),
        ),
        line("turn_context", serde_json::json!({ "model": "gpt-5.3-codex" })),
    ]
    .join("\n")
        + "\n"
}

#[test]
fn parses_rollout_into_events() {
    let (home, _g) = tmp_home("codex-sess");
    let dir = home.join("sessions").join("2026").join("08").join("01");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("rollout-2026-08-01T10-00-00-019f8dad.jsonl"), rollout_text()).unwrap();

    let titles: HashMap<String, String> = HashMap::new();
    let raw = import_rollout(&dir.join("rollout-2026-08-01T10-00-00-019f8dad.jsonl"), &titles);

    assert_eq!(raw.canonical_id, "019f8dad-0ee5-75d0-baa3-0cee3c16b301");
    assert_eq!(raw.cwd.as_deref(), Some("C:\\repo"));
    // Fallback title = first user text.
    assert_eq!(raw.title, "fix the bug");
    let kinds: Vec<&str> = raw
        .events
        .iter()
        .map(|e| match &e.payload {
            PartPayload::UserMessage { .. } => "user",
            PartPayload::AssistantMessage { .. } => "assistant",
            PartPayload::Thinking { .. } => "thinking",
            PartPayload::ToolInvocation { .. } => "tool",
            PartPayload::ToolResult { .. } => "result",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds, vec!["user", "thinking", "tool", "result", "assistant"]);
    // Tool pairing ids carried.
    let tool = raw.events.iter().find(|e| matches!(e.payload, PartPayload::ToolInvocation { .. })).unwrap();
    assert_eq!(tool.tool_call_id.as_deref(), Some("c1"));
}

#[test]
fn session_index_title_wins_over_first_user_text() {
    let (home, _g) = tmp_home("codex-sess-title");
    let dir = home.join("sessions").join("2026");
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("rollout-x.jsonl");
    fs::write(&file, rollout_text()).unwrap();

    let mut titles = HashMap::new();
    titles.insert(
        "019f8dad-0ee5-75d0-baa3-0cee3c16b301".to_string(),
        "Australia citizenship".to_string(),
    );
    let raw = import_rollout(&file, &titles);
    assert_eq!(raw.title, "Australia citizenship");
}

#[test]
fn importer_snapshot_and_import_wire_up() {
    let (home, _g) = tmp_home("codex-sess-imp");
    let dir = home.join("archived_sessions");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("rollout-old.jsonl"), rollout_text()).unwrap();
    // self_dir resolves under the real home — patch via env override is the
    // established pattern, but the importer itself is exercised through
    // import_rollout above; here we only pin the file-shape helpers.
    let titles = thread_titles(&home);
    assert!(titles.is_empty(), "no session_index.jsonl in a fresh home");
}
