use super::*;
use std::path::PathBuf;

/// Build a fixture db mirroring the real shared schema/shape (one parent
/// session with a user prompt + assistant text + reasoning + a completed
/// tool, one synthetic todo-reminder turn that must be skipped, one
/// subagent child).
fn fixture_db(dir: &Path) -> PathBuf {
    let db = dir.join("db.sqlite");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        r#"
            CREATE TABLE session (id TEXT, parent_id TEXT, directory TEXT, title TEXT,
                                  time_created INTEGER, time_updated INTEGER);
            CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER,
                                  time_updated INTEGER, data TEXT, sequence INTEGER);
            CREATE TABLE part (id TEXT, message_id TEXT, session_id TEXT, time_created INTEGER,
                               time_updated INTEGER, data TEXT, sequence INTEGER);
            "#,
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session VALUES ('sess-parent', NULL, 'C:/work/proj', 'Parent session', 100, 500)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session VALUES ('sess-child', 'sess-parent', 'C:/work/proj', 'Child session', 150, 400)",
        [],
    )
    .unwrap();

    let msg = |id: &str, sid: &str, ts: i64, data: &str| {
        conn.execute(
            "INSERT INTO message VALUES (?1, ?2, ?3, ?3, ?4, 0)",
            rusqlite::params![id, sid, ts, data],
        )
        .unwrap();
    };
    let part = |id: &str, mid: &str, sid: &str, ts: i64, data: &str| {
        conn.execute(
            "INSERT INTO part VALUES (?1, ?2, ?3, ?4, ?4, ?5, 0)",
            rusqlite::params![id, mid, sid, ts, data],
        )
        .unwrap();
    };

    msg("m1", "sess-parent", 100, r#"{"role":"user","agent":"zcode-agent"}"#);
    part("p1", "m1", "sess-parent", 110,
        r#"{"type":"text","text":"hello zcode","time":{"start":110,"end":120}}"#);
    // synthetic todo reminder (message-level, zcode style) — must be skipped
    msg("m2", "sess-parent", 130,
        r#"{"role":"user","agent":"zcode-agent","synthetic":true,"semantics":{"kind":"todo_reminder"}}"#);
    part("p2", "m2", "sess-parent", 131, r#"{"type":"text","text":"[todo reminder]"}"#);
    // synthetic text part (part-level, opencode style) — must be skipped
    msg("m5", "sess-parent", 132, r#"{"role":"user","agent":"plan"}"#);
    part("p8", "m5", "sess-parent", 133, r#"{"type":"text","synthetic":true,"text":"[snapshot]"}"#);
    msg("m3", "sess-parent", 140, r#"{"role":"assistant","agent":"zcode-agent"}"#);
    part("p3", "m3", "sess-parent", 141,
        r#"{"type":"reasoning","text":"thinking hard","metadata":{"anthropic":{"signature":"sig1"}}}"#);
    part("p4", "m3", "sess-parent", 150,
        r#"{"type":"text","text":"here is the answer","time":{"start":150,"end":160}}"#);
    part("p5", "m3", "sess-parent", 170,
        r#"{"type":"tool","callID":"call_1","tool":"Bash","state":{"status":"completed","input":{"command":"ls"},"output":"file-a file-b"}}"#);
    // opencode-style failed tool: no output, state.error carries the text
    part("p6", "m3", "sess-parent", 175,
        r#"{"type":"tool","callID":"call_2","tool":"Read","state":{"status":"error","input":{"path":"x"},"error":"no such file"}}"#);
    part("p7", "m3", "sess-parent", 180, r#"{"type":"step-finish","reason":"tool-calls"}"#);

    msg("m4", "sess-child", 200, r#"{"role":"assistant","agent":"zcode-agent"}"#);
    part("p9", "m4", "sess-child", 210, r#"{"type":"text","text":"subagent reply"}"#);

    db
}

#[test]
fn imports_sessions_parts_and_skips_synthetic() {
    let dir = tempfile::Builder::new().prefix("").tempdir().unwrap();
    let db = fixture_db(dir.path());

    let raws = collect(&db).unwrap();
    assert_eq!(raws.len(), 2, "parent + child, synthetic adds nothing");

    let parent = raws.iter().find(|r| r.canonical_id == "sess-parent").unwrap();
    assert!(!parent.is_sidechain);
    assert_eq!(parent.title, "Parent session");
    assert_eq!(parent.cwd.as_deref(), Some("C:/work/proj"));
    assert_eq!(parent.project.as_deref(), Some("proj"));
    assert_eq!(parent.started_at, 100);
    assert_eq!(parent.updated_at, 500);

    let texts: Vec<&str> = parent.events.iter().filter_map(|e| match &e.payload {
        PartPayload::UserMessage { text } | PartPayload::AssistantMessage { text } => Some(text.as_str()),
        _ => None,
    }).collect();
    assert_eq!(texts, ["hello zcode", "here is the answer"],
        "message-level AND part-level synthetic skipped");
    assert!(matches!(&parent.events[0].payload,
        PartPayload::UserMessage { text } if text == "hello zcode"));

    // reasoning carries the anthropic signature (zcode dialect)
    let reasoning = parent.events.iter().find_map(|e| match &e.payload {
        PartPayload::Thinking { text, signature } => Some((text.clone(), signature.clone())),
        _ => None,
    }).unwrap();
    assert_eq!(reasoning.0, "thinking hard");
    assert_eq!(reasoning.1.as_deref(), Some("sig1"));

    // completed tool yields paired invocation + result with the same call id
    let inv = parent.events.iter().find(|e| matches!(e.payload, PartPayload::ToolInvocation { .. })).unwrap();
    assert_eq!(inv.tool_call_id.as_deref(), Some("call_1"));
    let res = parent.events.iter().find(|e| matches!(e.payload, PartPayload::ToolResult { .. } ) && e.tool_call_id.as_deref() == Some("call_1")).unwrap();
    assert!(matches!(&res.payload, PartPayload::ToolResult { output, .. } if output == "file-a file-b"));
    assert!(matches!(&res.payload, PartPayload::ToolResult { is_error: Some(false), .. }));

    // opencode-style failed tool: state.error becomes the output, flagged error
    let err = parent.events.iter().find(|e| matches!(e.payload, PartPayload::ToolResult { .. }) && e.tool_call_id.as_deref() == Some("call_2")).unwrap();
    assert!(matches!(&err.payload, PartPayload::ToolResult { output, is_error: Some(true), .. } if output == "no such file"));

    // step-finish skipped entirely
    assert!(!parent.events.iter().any(|e| e.raw_json.contains("step-finish")));

    let child = raws.iter().find(|r| r.canonical_id == "sess-child").unwrap();
    assert!(child.is_sidechain);
    assert_eq!(child.parent_session_id.as_deref(), Some("sess-parent"));
    assert_eq!(child.agent_id.as_deref(), Some("zcode-agent"));
}

#[test]
fn missing_db_returns_empty() {
    let dir = tempfile::Builder::new().prefix("").tempdir().unwrap();
    assert!(collect(&dir.path().join("none.sqlite")).unwrap().is_empty());
}

#[test]
fn wrong_schema_returns_empty() {
    let dir = tempfile::Builder::new().prefix("").tempdir().unwrap();
    let db = dir.path().join("db.sqlite");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch("CREATE TABLE other (x)").unwrap();
    assert!(collect(&db).unwrap().is_empty());
}