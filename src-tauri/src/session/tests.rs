use super::*;
use std::io::Write;

fn build_sessions(provider: &str, raws: Vec<RawFile>) -> Vec<(Session, Vec<Message>)> {
    assemble(provider, raws)
        .into_iter()
        .map(|a| {
            let messages: Vec<Message> = a.parts.iter().map(|p| p.to_message()).collect();
            let mut session = a.session;
            session.message_count = messages.len() as u32;
            (session, messages)
        })
        .collect()
}

fn normalize(provider: &str, raws: Vec<RawFile>) -> Vec<Session> {
    assemble(provider, raws)
        .into_iter()
        .map(|a| a.session)
        .collect()
}

fn write_jsonl(path: &Path, lines: &[&str]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let mut f = std::fs::File::create(path).unwrap();
    for l in lines {
        writeln!(f, "{l}").unwrap();
    }
}

fn write_jsonl_parents(path: &Path, lines: &[&str]) {
    write_jsonl(path, lines)
}

/// Returns (path, guard); the guard deletes the dir on drop (test
/// cleanup — process-exit hooks don't run under cargo test).
fn tempfile_dir() -> (PathBuf, tempfile::TempDir) {
    let dir = tempfile::Builder::new()
        .prefix("nestra-session-test-")
        .tempdir()
        .expect("tempdir");
    (dir.path().to_path_buf(), dir)
}

fn temp_session_home() -> (PathBuf, tempfile::TempDir) {
    let dir = tempfile::Builder::new()
        .prefix("nestra-session-home-")
        .tempdir()
        .expect("tempdir");
    (dir.path().to_path_buf(), dir)
}

/// Set NESTRA_HOME_DIR for the lifetime of `f`, restoring the previous
/// value on exit. Serialized through `HOME_LOCK` so parallel tests can't
/// race on the process-global env var.
fn with_home<R>(home: &Path, f: impl FnOnce() -> R) -> R {
    let _guard = crate::HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let prev = std::env::var("NESTRA_HOME_DIR").ok();
    std::env::set_var("NESTRA_HOME_DIR", home);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    match prev {
        Some(p) => std::env::set_var("NESTRA_HOME_DIR", p),
        None => std::env::remove_var("NESTRA_HOME_DIR"),
    }
    match result {
        Ok(v) => v,
        Err(p) => std::panic::resume_unwind(p),
    }
}

const PI_HEADER: &str = r#"{"type":"session","id":"019fcfbd-7954-7f25-b526-9a289ccda14c","timestamp":"2026-08-05T02:25:28.916Z","cwd":"C:\\Users\\nuo\\SomeProj"}"#;

/// Boundary proof for usage/model promotion (audit requirement): a
/// Claude-shaped assistant line with multiple content blocks is ONE
/// message — metadata must attach to the line's first event only, and
/// user lines carry nothing. This test is the reproducible data-contract
/// evidence the promotion wiring depends on.
#[test]
fn jsonl_usage_model_metadata_attaches_to_first_event_of_line() {
    let (home, _home_g) = temp_session_home();
    let dir = home.join("tmp");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("usage.jsonl");
    write_jsonl(
        &p,
        &[
            r#"{"type":"message","id":"a","message":{"role":"user","content":[{"type":"text","text":"q"}]},"timestamp":"2026-08-05T02:25:30.000Z"}"#,
            r#"{"type":"message","id":"b","message":{"role":"assistant","model":"claude-sonnet-4-5","content":[{"type":"thinking","thinking":"hmm"},{"type":"text","text":"an answer"}],"usage":{"input_tokens":42,"output_tokens":7,"cache_read_input_tokens":100,"cache_creation_input_tokens":5}},"timestamp":"2026-08-05T02:25:31.000Z"}"#,
        ],
    );
    let parsed = parse_jsonl_events(&p).unwrap();
    let metas: Vec<&str> = parsed
        .events
        .iter()
        .map(|e| e.provider_metadata_json.as_str())
        .collect();
    // user text, assistant thinking, assistant text → 3 events.
    assert_eq!(metas.len(), 3);
    assert_eq!(metas[0], "{}", "user lines carry no usage metadata");
    let m: serde_json::Value = serde_json::from_str(metas[1]).unwrap();
    assert_eq!(m["model"], "claude-sonnet-4-5");
    assert_eq!(m["usage"]["input_tokens"], 42);
    assert_eq!(m["usage"]["output_tokens"], 7);
    assert_eq!(m["usage"]["cache_read_input_tokens"], 100);
    assert_eq!(m["usage"]["cache_creation_input_tokens"], 5);
    assert_eq!(
        metas[2], "{}",
        "only the FIRST event of a line carries the line's usage/model"
    );
}

#[test]
fn pi_assemble_extracts_id_title_summary_project_and_times() {
    let (home, _home_g) = temp_session_home();
    let dir = home.join(".pi").join("agent").join("sessions").join("sub");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("2026-08-05_019fcfbd-7954-7f25-b526-9a289ccda14c.jsonl");
    write_jsonl(&p, &[
        PI_HEADER,
        r#"{"type":"message","id":"a","message":{"role":"user","content":[{"type":"text","text":"first question"}]},"timestamp":"2026-08-05T02:25:30.000Z"}"#,
        r#"{"type":"message","id":"b","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hmm"},{"type":"text","text":"an answer"}]},"timestamp":"2026-08-05T02:25:31.000Z"}"#,
        r#"{"type":"message","id":"c","message":{"role":"user","content":[{"type":"text","text":"second question"}]},"timestamp":"2026-08-05T02:25:32.000Z"}"#,
        r#"{"type":"message","id":"d","message":{"role":"assistant","content":[{"type":"text","text":"final answer"}]},"timestamp":"2026-08-05T02:25:33.000Z"}"#,
    ]);

    let built = with_home(&home, || build_sessions("pi-cli", collect_raw_files("pi-cli").unwrap()));
    assert_eq!(built.len(), 1);
    let (s, msgs) = &built[0];
    assert_eq!(s.id, "019fcfbd-7954-7f25-b526-9a289ccda14c");
    assert_eq!(s.title, "first question");
    assert_eq!(s.summary, "final answer");
    assert_eq!(s.project.as_deref(), Some("SomeProj"));
    assert!(s.started_at < s.updated_at);
    // thinking block is its own message, separate from the reply text.
    let roles: Vec<&str> = msgs.iter().map(|m| m.role.as_str()).collect();
    assert!(roles.contains(&"thinking"));
    assert!(msgs.iter().any(|m| m.role == "assistant" && m.content_text == "an answer"));
}

#[test]
fn every_known_provider_has_an_importer() {
    for id in all_providers() {
        assert!(importer_for(id).is_some(), "{id} has no importer");
    }
}

#[test]
fn claude_subagent_is_grouped_under_parent() {
    let (home, _home_g) = temp_session_home();
    let proj = home.join(".claude").join("projects").join("projhash");
    let parent_id = "f8a01899-e2b1-4d5a-b60b-9ff328e13a4a";
    let main = proj.join(format!("{parent_id}.jsonl"));
    write_jsonl_parents(
        &main,
        &[
            format!(r#"{{"type":"user","message":{{"role":"user","content":"hi there"}},"sessionId":"{parent_id}","timestamp":"2026-08-06T10:00:00.000Z","cwd":"C:\\proj"}}"#).as_str(),
            format!(r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"hello back"}}]}},"sessionId":"{parent_id}","timestamp":"2026-08-06T10:00:01.000Z"}}"#).as_str(),
        ],
    );
    let agent_id = "af12ea1840a107df6";
    let sub = proj
        .join(parent_id)
        .join("subagents")
        .join(format!("agent-{agent_id}.jsonl"));
    write_jsonl_parents(
        &sub,
        &[
            format!(r#"{{"parentUuid":null,"isSidechain":true,"agentId":"{agent_id}","type":"user","message":{{"role":"user","content":"do the thing"}},"sessionId":"{parent_id}","timestamp":"2026-08-06T10:00:02.000Z","cwd":"C:\\proj"}}"#).as_str(),
        ],
    );

    let sessions = with_home(&home, || normalize("claude-code-cli", collect_raw_files("claude-code-cli").unwrap()));
    let top: Vec<&Session> = sessions.iter().filter(|s| !s.is_subagent).collect();
    let subs: Vec<&Session> = sessions.iter().filter(|s| s.is_subagent).collect();
    assert_eq!(top.len(), 1);
    assert_eq!(top[0].id, parent_id);
    assert_eq!(top[0].child_count, 1);
    assert_eq!(subs.len(), 1);
    assert_eq!(subs[0].id, agent_id);
    assert_eq!(subs[0].parent_session_id.as_deref(), Some(parent_id));
    assert_eq!(subs[0].agent_id.as_deref(), Some(agent_id));
}

/// New semantic coverage: tool_use + tool_result pair by call_id.
#[test]
fn claude_tool_use_and_result_pair_by_call_id() {
    let (home, _home_g) = temp_session_home();
    let proj = home.join(".claude").join("projects").join("p");
    std::fs::create_dir_all(&proj).unwrap();
    let p = proj.join("toolpair.jsonl");
    write_jsonl(&p, &[
        r#"{"type":"user","message":{"role":"user","content":"run it"},"sessionId":"tp","timestamp":"2026-08-06T10:00:00.000Z"}"#,
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"call_42","name":"Bash","input":{"command":"echo hi"}}]},"sessionId":"tp","timestamp":"2026-08-06T10:00:01.000Z"}"#,
        r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_42","content":"hi"}]},"sessionId":"tp","timestamp":"2026-08-06T10:00:02.000Z"}"#,
    ]);
    let built = with_home(&home, || build_sessions("claude-code-cli", collect_raw_files("claude-code-cli").unwrap()));
    let (_, msgs) = &built[0];
    let invocations: Vec<&Message> = msgs.iter().filter(|m| m.tool_name.is_some()).collect();
    let results: Vec<&Message> = msgs.iter().filter(|m| m.tool_output.is_some()).collect();
    assert_eq!(invocations.len(), 1);
    assert_eq!(results.len(), 1);
    assert_eq!(invocations[0].tool_call_id.as_deref(), Some("call_42"));
    assert_eq!(results[0].tool_call_id.as_deref(), Some("call_42"));
    assert_eq!(invocations[0].tool_name.as_deref(), Some("Bash"));
    assert_eq!(results[0].tool_output.as_deref(), Some("hi"));
}

/// New semantic coverage: unknown content blocks are preserved losslessly
/// (no silent `_ => {}` drop).
#[test]
fn unknown_content_block_is_preserved_not_dropped() {
    let (home, _home_g) = temp_session_home();
    let proj = home.join(".claude").join("projects").join("p");
    std::fs::create_dir_all(&proj).unwrap();
    let p = proj.join("unk.jsonl");
    write_jsonl(&p, &[
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"ok"},{"type":"server_tool_use","id":"x","name":"web","input":{"q":"r"}}]},"sessionId":"u","timestamp":"2026-08-06T10:00:00.000Z"}"#,
    ]);
    let built = with_home(&home, || build_sessions("claude-code-cli", collect_raw_files("claude-code-cli").unwrap()));
    let (_, msgs) = &built[0];
    // The recognized text becomes an assistant message; the unrecognized
    // server_tool_use becomes a provider_event carrying the raw json.
    assert!(msgs.iter().any(|m| m.role == "assistant" && m.content_text == "ok"));
    let unknown = msgs.iter().find(|m| m.role == "provider_event").expect("unknown preserved");
    assert!(unknown.content_text.contains("server_tool_use"));
}

/// New semantic coverage: MCP tool name is parsed into provenance metadata.
#[test]
fn mcp_tool_name_becomes_mcp_provenance() {
    let (home, _home_g) = temp_session_home();
    let proj = home.join(".claude").join("projects").join("p");
    std::fs::create_dir_all(&proj).unwrap();
    let p = proj.join("mcp.jsonl");
    write_jsonl(&p, &[
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"c1","name":"mcp__filesystem__read_file","input":{"path":"x"}}]},"sessionId":"m","timestamp":"2026-08-06T10:00:00.000Z"}"#,
    ]);
    let built = with_home(&home, || build_sessions("claude-code-cli", collect_raw_files("claude-code-cli").unwrap()));
    let (_, msgs) = &built[0];
    let inv = msgs.iter().find(|m| m.tool_name.is_some()).unwrap();
    assert!(inv.provider_metadata_json.contains("\"server\":\"filesystem\""));
}

#[test]
fn reconcile_is_idempotent() {
    let (home, _home_g) = temp_session_home();
    let proj = home.join(".claude").join("projects").join("p");
    write_jsonl_parents(
        &proj.join("abc.jsonl"),
        &[
            r#"{"type":"user","message":{"role":"user","content":"hey"},"sessionId":"abc","timestamp":"2026-08-06T10:00:00.000Z","cwd":"C:\\p"}"#,
        ],
    );

    let (tmpdb, _tmpdb_g) = tempfile_dir();
    let conn = crate::db::open(&tmpdb).unwrap();
    crate::db::migrate(&conn).unwrap();

    with_home(&home, || crate::session::store::reconcile_provider(&conn, "claude-code-cli").unwrap());
    let n1 = crate::session::store::count_sessions(&conn).unwrap();
    assert_eq!(n1, 1);

    with_home(&home, || crate::session::store::reconcile_provider(&conn, "claude-code-cli").unwrap());
    let n2 = crate::session::store::count_sessions(&conn).unwrap();
    assert_eq!(n1, n2);

    let win = crate::session::store::read_messages(&conn, "claude-code-cli", "abc", 0, 0).unwrap();
    assert_eq!(win.total, 1);
    assert_eq!(win.messages[0].role, "user");
    assert_eq!(win.messages[0].content_text, "hey");
}

/// Verifies against the user's real ~/.claude tree. Ignored by default.
#[test]
#[ignore]
fn real_claude_subagent_grouping() {
    let parent = "f8a01899-e2b1-4d5a-b60b-9ff328e13a4a";
    let agent = "af12ea1840a107df6";
    let raws = collect_raw_files("claude-code-cli").unwrap();
    let sessions = normalize("claude-code-cli", raws);
    let top = sessions
        .iter()
        .find(|s| s.id == parent && !s.is_subagent)
        .expect("parent session must exist as top-level");
    assert!(top.child_count >= 1);
    let sub = sessions.iter().find(|s| s.id == agent).expect("subagent present");
    assert!(sub.is_subagent);
    assert_eq!(sub.parent_session_id.as_deref(), Some(parent));
}

/// Claude session logs are interleaved with lifecycle bookkeeping lines
/// (`mode`, `permission-mode`, `ai-title`, `file-history-snapshot`, …) and
/// context-injection lines (`attachment` carrying skill listings / file
/// contents). These are NOT conversation turns and must NOT pollute the
/// message stream — regression for the "session won't open / shows garbage"
/// bug where they became Unknown `provider_event` rows.
#[test]
fn claude_bookkeeping_lines_do_not_pollute_message_stream() {
    let (home, _home_g) = temp_session_home();
    let proj = home.join(".claude").join("projects").join("p");
    std::fs::create_dir_all(&proj).unwrap();
    let p = proj.join("realish.jsonl");
    write_jsonl(&p, &[
        // bookkeeping (must be skipped)
        r#"{"type":"ai-title","aiTitle":"x","sessionId":"rs"}"#,
        r#"{"type":"mode","mode":"normal","sessionId":"rs"}"#,
        r#"{"type":"permission-mode","permissionMode":"plan","sessionId":"rs"}"#,
        r#"{"type":"file-history-snapshot","messageId":"m","snapshot":{}}"#,
        r#"{"type":"task_reminder","content":"remind"}"#,
        // context-injection attachment (must be skipped — not a turn)
        r#"{"type":"attachment","attachment":{"type":"skill_listing","content":"- skill: x"},"sessionId":"rs"}"#,
        r#"{"type":"file","attachment":{"type":"file","filename":"a.ts","content":{"type":"text","file":{"filePath":"a.ts","content":"x"}}},"sessionId":"rs"}"#,
        // a real user turn
        r#"{"type":"user","message":{"role":"user","content":"hello"},"sessionId":"rs","timestamp":"2026-08-06T10:00:00.000Z"}"#,
        // a real assistant turn
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hi back"}]},"sessionId":"rs","timestamp":"2026-08-06T10:00:01.000Z"}"#,
    ]);
    let built = with_home(&home, || build_sessions("claude-code-cli", collect_raw_files("claude-code-cli").unwrap()));
    assert_eq!(built.len(), 1);
    let (s, msgs) = &built[0];
    // Only the two real turns — no bookkeeping, no attachment provider_events.
    assert_eq!(msgs.len(), 2, "bookkeeping/attachment lines must be skipped");
    assert_eq!(msgs[0].role, "user");
    assert_eq!(msgs[0].content_text, "hello");
    assert_eq!(msgs[1].role, "assistant");
    assert_eq!(msgs[1].content_text, "hi back");
    // title is the AI-generated `ai-title` when present (preferred over
    // the first user message, which may be a context-continuation summary).
    assert_eq!(s.title, "x");
}