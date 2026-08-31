use super::*;

#[test]
fn verdict_from_text_parses_status_and_summary() {
    let (status, summary) =
        verdict_from_text("VERDICT: changes_requested\nThe migration is missing a down path.\nAdd a rollback.");
    assert_eq!(status.as_deref(), Some("changes_requested"));
    assert!(summary.contains("rollback"));
    // No verdict line → status None, summary falls back to the text.
    let (status, summary) = verdict_from_text("looks fine overall");
    assert_eq!(status, None);
    assert_eq!(summary, "looks fine overall");
}

#[test]
fn render_prompt_embeds_pack() {
    let pack = ContextPack {
        title: "Fix login".into(),
        goal: Some("goal text".into()),
        modified_files: vec!["src/a.rs".into()],
        failed_attempts: vec!["Bash — boom".into()],
        diff: Some("+one line".into()),
        ..Default::default()
    };
    let p = render_prompt(&pack, "rv-9");
    assert!(p.contains("## Task\nFix login"));
    assert!(p.contains("`src/a.rs`"));
    assert!(p.contains("```diff"));
    assert!(p.contains(".nestra/reviews/rv-9/verdict.md"), "artifact instruction present");
    assert!(p.contains("VERDICT: pass"), "fallback reply-line convention kept");
}

#[test]
fn verdict_frontmatter_parses_and_merges() {
    let file = "---\nstatus: changes_requested\nseverity: high\nsummary: migration lacks rollback\n---\nDetails here.";
    let pv = parse_verdict_frontmatter(file);
    assert_eq!(pv.status.as_deref(), Some("changes_requested"));
    assert_eq!(pv.severity.as_deref(), Some("high"));
    assert_eq!(pv.summary.as_deref(), Some("migration lacks rollback"));
    // No frontmatter summary → the body becomes it.
    let pv2 = parse_verdict_frontmatter("---\nstatus: pass\n---\nbody line");
    assert_eq!(pv2.summary.as_deref(), Some("body line"));
    // Non-frontmatter content → empty.
    assert_eq!(parse_verdict_frontmatter("just text"), ParsedVerdict::default());

    // Merge: file wins over the reply's VERDICT: line…
    let (s, sum) = merge_verdict(Some(file), "VERDICT: pass\nreply summary");
    assert_eq!(s.as_deref(), Some("changes_requested"));
    assert!(sum.contains("rollback"));
    // …invalid file status falls back to the reply…
    let (s, _) = merge_verdict(Some("---\nstatus: maybe\n---\nbody"), "VERDICT: pass\nok");
    assert_eq!(s.as_deref(), Some("pass"));
    // …and no file is the plain reply path.
    let (s, _) = merge_verdict(None, "VERDICT: fail\nbad");
    assert_eq!(s.as_deref(), Some("fail"));
}


fn ev_parts(pairs: &[(&str, &str, Option<bool>, bool)]) -> Vec<crate::session::Part> {
    // (input, output, is_error, is_test) — builds invocation+result pairs.
    let mut parts = Vec::new();
    for (i, (input, output, is_error, is_test)) in pairs.iter().enumerate() {
        let name = if *is_test { "Bash" } else { "Bash" };
        let input = input.to_string();
        parts.push(crate::session::Part {
            seq: (i * 2) as u32,
            payload: crate::session::PartPayload::ToolInvocation {
                name: name.into(),
                input: Some(if *is_test { format!(r#"{{"command":"{input}"}}"#) } else { input.clone() }),
                mcp: None,
                child_session_id: None,
            },
            tool_call_id: Some(format!("c{i}")),
            message_id: None,
            parent_message_id: None,
            ts: None,
            raw_json: String::new(),
            provider_metadata_json: "{}".into(),
        });
        parts.push(crate::session::Part {
            seq: (i * 2 + 1) as u32,
            payload: crate::session::PartPayload::ToolResult {
                output: output.to_string(),
                is_error: *is_error,
                mcp: None,
            },
            tool_call_id: Some(format!("c{i}")),
            message_id: None,
            parent_message_id: None,
            ts: None,
            raw_json: String::new(),
            provider_metadata_json: "{}".into(),
        });
    }
    parts
}

#[test]
fn test_results_conservative_extraction() {
    // Test runs: one passed, one failed, one without status.
    let parts = ev_parts(&[
        (r#"cargo test --lib"#, "ok. 3 passed", Some(false), true),
        (r#"npm run test:unit"#, "error: boom", Some(true), true),
        (r#"cargo test"#, "no output", None, true),
    ]);
    let results = extract_test_results(&parts);
    assert_eq!(results.len(), 3);
    assert!(results[0].contains("(passed)") && results[0].contains("ok. 3 passed"), "{}", results[0]);
    assert!(results[1].contains("(failed)") && results[1].contains("boom"), "{}", results[1]);
    assert!(!results[2].contains('('), "None status renders without a status word: {}", results[2]);

    // False-positive guards: `npm run latest` is NOT a test run; a plain
    // `ls` is not; only the most recent 3 are kept.
    let parts2 = ev_parts(&[
        (r#"npm run latest"#, "ran latest", Some(false), true),
        (r#"ls -la"#, "files", Some(false), true),
        (r#"cargo test"#, "ok", Some(false), true),
        (r#"pytest -q"#, "ok", Some(false), true),
        (r#"go test ./..."#, "ok", Some(false), true),
    ]);
    let results2 = extract_test_results(&parts2);
    assert_eq!(results2.len(), 3, "capped to most recent three");
    assert!(!results2.iter().any(|r| r.contains("latest")), "npm run latest is not test evidence: {results2:?}");
    assert!(!results2.iter().any(|r| r.contains("files")));

    // Empty when nothing test-related.
    assert!(extract_test_results(&ev_parts(&[(r#"ls"#, "x", Some(false), false)])).is_empty());

    // Prompt section appears only when evidence exists.
    let pack = ContextPack {
        title: "t".into(),
        test_results: vec!["Bash (failed) — boom".into()],
        ..Default::default()
    };
    let p = render_prompt(&pack, "rv-1");
    assert!(p.contains("## Test results (observed)"));
    let pack2 = ContextPack { title: "t".into(), ..Default::default() };
    assert!(!render_prompt(&pack2, "rv-1").contains("## Test results"));
}

#[test]
fn gather_extracts_session_and_diff_degrades_without_git() {
    let dir = tempfile::Builder::new().prefix("").tempdir().unwrap();
    let conn = Connection::open_in_memory().unwrap();
    crate::schema::build_v1(&conn).unwrap();
    // Index row + the REAL source file (index-only store — no part mirror).
    let src = dir.path().join("s-1.jsonl");
    std::fs::write(
        &src,
        concat!(
            r#"{"type":"session","id":"s-1","timestamp":"2026-08-05T02:25:28.916Z","cwd":"C:\\x"}"#,
            "\n",
            r#"{"type":"message","id":"a","message":{"role":"user","content":[{"type":"text","text":"goal text"}]},"timestamp":"2026-08-05T02:25:30.000Z"}"#,
            "\n",
        ),
    )
    .unwrap();
    let source_files = format!(r#"["{}"]"#, src.to_string_lossy().replace('\\', "\\\\"));
    conn.execute(
        "INSERT INTO session (provider, id, title, summary, started_at, updated_at,
                                  message_count, source_path, resume_command, cwd,
                                  source_files_json)
             VALUES ('pi-cli','s-1','Fix login','',0,0,1,'x','pi --session s-1',?1,?2)",
        rusqlite::params![dir.path().to_string_lossy(), source_files],
    )
    .unwrap();
    let (pack, cwd) = gather(&conn, "pi-cli", "s-1").unwrap();
    assert_eq!(pack.title, "Fix login");
    assert_eq!(pack.goal.as_deref(), Some("goal text"));
    assert_eq!(cwd.as_deref().map(str::to_string), Some(dir.path().to_string_lossy().into_owned()));
    // No git repo in the tempdir → diff None, gather still succeeds.
    assert!(pack.diff.is_none());
}