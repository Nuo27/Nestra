use super::*;
use crate::session::semantic::PartPayload;

fn part(seq: u32, payload: PartPayload) -> Part {
    Part {
        seq,
        payload,
        tool_call_id: None,
        message_id: None,
        parent_message_id: None,
        ts: None,
        raw_json: String::new(),
        provider_metadata_json: "{}".into(),
    }
}

fn fixture_parts() -> Vec<Part> {
    vec![
        part(0, PartPayload::UserMessage { text: "Fix the login bug in auth.rs".into() }),
        part(1, PartPayload::AssistantMessage { text: "I'll start by reading the auth module.".into() }),
        part(2, PartPayload::ToolInvocation {
            name: "Edit".into(),
            input: Some(r#"{"file_path":"src/auth.rs"}"#.into()),
            mcp: None,
            child_session_id: None,
        }),
        part(3, PartPayload::ToolResult { output: "ok".into(), is_error: Some(false), mcp: None }),
        part(4, PartPayload::ToolResult { output: "".into(), is_error: Some(false), mcp: None }), // placeholder replaced below
    ]
}

#[test]
fn build_sections_extracts_all_sections() {
    let mut parts = fixture_parts();
    // Replace placeholder with an errored tool result tied to a Bash call.
    parts.pop();
    parts.push(Part {
        tool_call_id: Some("call-1".into()),
        payload: PartPayload::ToolInvocation {
            name: "Bash".into(),
            input: Some(r#"{"command":"cargo test"}"#.into()),
            mcp: None,
            child_session_id: None,
        },
        ..part(4, PartPayload::UserMessage { text: String::new() })
    });
    parts.push(Part {
        tool_call_id: Some("call-1".into()),
        payload: PartPayload::ToolResult {
            output: "error: expected `;`\n  --> src/main.rs:1:1".into(),
            is_error: Some(true),
            mcp: None,
        },
        ..part(5, PartPayload::UserMessage { text: String::new() })
    });
    parts.push(part(6, PartPayload::AssistantMessage {
        text: "Switched to token-based sessions. Next: re-run the test suite and update the migration docs.".into(),
    }));
    parts.push(part(7, PartPayload::SubAgent {
        agent_id: "researcher".into(),
        child_session_id: Some("child-9".into()),
        description: Some("find failing tests".into()),
    }));

    let s = build_sections(&parts);
    assert_eq!(s.goal.as_deref(), Some("Fix the login bug in auth.rs"));
    assert_eq!(s.modified_files, vec!["src/auth.rs".to_string()]);
    assert_eq!(s.failed_attempts.len(), 1);
    assert!(s.failed_attempts[0].starts_with("Bash — error:"), "{}", s.failed_attempts[0]);
    assert_eq!(s.subagents.len(), 1);
    assert!(s.subagents[0].contains("researcher") && s.subagents[0].contains("child-9"));
    assert_eq!(s.decisions.len(), 2);
    assert!(s.next_steps.as_deref().unwrap().contains("Next: re-run"));

    let md = render_markdown("Login bug", &s);
    assert!(md.contains("# Handoff: Login bug"));
    assert!(md.contains("`src/auth.rs`"));
    assert!(md.contains("## Failed attempts"));
}

#[test]
fn decisions_dedupe_on_full_normalized_text() {
    // Two turns that merely SHARE a prefix are distinct decisions…
    let a = "Fix the login bug in auth.rs by validating tokens.";
    let b = "Fix the login bug in auth.rs by adding rate limits.";
    // …while an exact restatement (modulo whitespace) is a duplicate.
    let c = "  Fix the login bug in   auth.rs by validating tokens.  ";
    let parts = vec![
        part(0, PartPayload::AssistantMessage { text: a.into() }),
        part(1, PartPayload::AssistantMessage { text: b.into() }),
        part(2, PartPayload::AssistantMessage { text: c.into() }),
    ];
    let s = build_sections(&parts);
    assert_eq!(s.decisions.len(), 2, "prefix-shared turns survive, exact restatement merges");
    assert!(s.decisions[0].starts_with("Fix the login bug in auth.rs by validating tokens."));
    assert!(s.decisions[1].starts_with("Fix the login bug in auth.rs by adding rate limits."));
}

#[test]
fn decisions_truncate_at_boundaries_with_recency_weighting() {
    // A long line with a newline inside the budget → cut at the newline.
    let t = format!("line one
{}", "x".repeat(300));
    let cut = truncate_at_boundary(&t, 100);
    assert!(cut.starts_with("line one"));
    assert!(cut.ends_with('…'));
    assert!(cut.len() <= 100 + "…".len() + 8, "{}", cut.len());

    // No newline but a sentence end → cut after the sentence.
    let t2 = format!("First sentence. {}second", "y".repeat(120));
    let cut2 = truncate_at_boundary(&t2, 60);
    assert_eq!(cut2, "First sentence.…");

    // No boundary at all → plain char cut + marker.
    let t3 = "z".repeat(50);
    let cut3 = truncate_at_boundary(&t3, 20);
    assert_eq!(cut3.chars().count(), 21); // 20 chars + marker
    assert!(cut3.ends_with('…'));

    // Short text stays untouched (no marker).
    assert_eq!(truncate_at_boundary("short", 100), "short");

    // Recency weighting: the NEWEST turn gets the longer budget.
    let mut parts = Vec::new();
    for i in 0..6 {
        parts.push(part(i as u32, PartPayload::AssistantMessage {
            text: format!("turn {i} {}", "d".repeat(250)),
        }));
    }
    let s = build_sections(&parts);
    assert_eq!(s.decisions.len(), 5, "count cap independent of length caps");
    let newest_len = format!("turn 5 {}", "d".repeat(250)).chars().count();
    assert_eq!(s.decisions[4].chars().count(), newest_len, "newest stays untruncated within LATEST_TRUNC");
    assert!(s.decisions[0].chars().count() <= EARLIER_TRUNC + 1, "older turns use the shorter budget");
}

#[test]
fn failed_attempts_capped_to_most_recent_five() {
    let mut parts = Vec::new();
    for i in 0..7 {
        parts.push(Part {
            tool_call_id: Some(format!("call-{i}")),
            payload: PartPayload::ToolInvocation {
                name: "Bash".into(),
                input: None,
                mcp: None,
                child_session_id: None,
            },
            ..part(i as u32, PartPayload::UserMessage { text: String::new() })
        });
        parts.push(Part {
            tool_call_id: Some(format!("call-{i}")),
            payload: PartPayload::ToolResult {
                output: format!("error {i}"),
                is_error: Some(true),
                mcp: None,
            },
            ..part(i as u32, PartPayload::UserMessage { text: String::new() })
        });
    }
    let s = build_sections(&parts);
    assert_eq!(s.failed_attempts.len(), MAX_FAILED);
    assert!(s.failed_attempts[0].contains("error 2"), "most recent five kept: {}", s.failed_attempts[0]);
}

#[test]
fn build_sections_empty_on_blank_session() {
    assert_eq!(build_sections(&[]), HandoffSections::default());
    assert_eq!(render_markdown("t", &HandoffSections::default()), "# Handoff: t\n\n");
}

#[test]
fn context_pressure_reports_estimate_and_top_consumer() {
    let mut parts = Vec::new();
    for i in 0..10 {
        parts.push(part(i, PartPayload::AssistantMessage {
            text: "x".repeat(400),
        }));
    }
    parts.push(part(10, PartPayload::ToolResult {
        output: "y".repeat(4_000),
        is_error: Some(false),
        mcp: None,
    }));
    let p = context_pressure(&parts, None);
    assert_eq!(p.est_tokens, (10 * 400 + 4_000) / 4);
    assert_eq!(p.top_consumer.as_deref(), Some("tool result"));
    assert!(p.estimated);
    assert_eq!(p.window_tokens, DEFAULT_CONTEXT_WINDOW);
    assert!(p.pct < 100);
    // A confirmed catalog window replaces the denominator only.
    let p2 = context_pressure(&parts, Some(20_000));
    assert_eq!(p2.window_tokens, 20_000);
    assert_eq!(p2.est_tokens, p.est_tokens);
}

/// Audit requirement: usage metadata alone must NOT flip the estimate to
/// "real" — the cache-exclusivity contract is unverified, so the
/// char-based path stays even when parts carry usage.
#[test]
fn context_pressure_keeps_char_estimate_when_usage_present() {
    let mut parts = vec![part(0, PartPayload::AssistantMessage {
        text: "z".repeat(400),
    })];
    parts[0].provider_metadata_json =
        r#"{"model":"m","usage":{"input_tokens":42,"output_tokens":7}}"#.into();
    let p = context_pressure(&parts, None);
    assert_eq!(p.est_tokens, 100, "char estimate, not the usage numbers");
    assert!(p.estimated);
    assert_eq!(last_model(&parts).as_deref(), Some("m"));
}

#[test]
fn window_for_model_exact_then_unique_like_then_ambiguous() {
    let conn = Connection::open_in_memory().unwrap();
    crate::schema::build_v1(&conn).unwrap();
    conn.execute(
        "INSERT INTO provider_endpoint (id, kind, display_name, has_api_key, status, models_json)
             VALUES ('ep-1','custom','M',0,'unvalidated','{}')",
        [],
    )
    .unwrap();
    let seed = |model_id: &str, ctx: i64| {
        conn.execute(
            "INSERT INTO model_catalog (endpoint_id, model_id, abilities_json) VALUES ('ep-1',?1,?2)",
            rusqlite::params![
                model_id,
                serde_json::json!({"limit": {"context": ctx, "output": 4096}}).to_string()
            ],
        )
        .unwrap();
    };
    seed("claude-sonnet-4-5", 200_000);
    seed("openai/gpt-5.6-luna", 400_000);
    seed("openai/gpt-5.6-mini", 128_000);

    // Exact match wins.
    assert_eq!(window_for_model(&conn, "claude-sonnet-4-5"), Some(200_000));
    // LIKE fallback resolves to exactly ONE distinct model → adopted.
    assert_eq!(window_for_model(&conn, "gpt-5.6-luna"), Some(400_000));
    // LIKE matches TWO distinct models → ambiguous → default window kept.
    assert_eq!(window_for_model(&conn, "gpt-5.6"), None);
    // No hit at all.
    assert_eq!(window_for_model(&conn, "nope"), None);
}


#[test]
fn usage_totals_read_provider_metadata() {
    let mut parts = fixture_parts();
    parts[0].provider_metadata_json =
        r#"{"usage":{"input_tokens":100,"output_tokens":50},"cost_usd":0.25}"#.into();
    parts[1].provider_metadata_json =
        r#"{"usage":{"prompt_tokens":10,"completion_tokens":5}}"#.into();
    let (tokens, cost) = usage_totals(&parts);
    assert_eq!(tokens, Some(165));
    assert_eq!(cost, Some(0.25));
    // No metadata at all → None/None (today's common case).
    let (t, c) = usage_totals(&[]);
    assert_eq!((t, c), (None, None));
}

#[test]
fn truncate_chars_respects_multibyte() {
    let s = "密码密码密码";
    let cut = truncate_chars(s, 2);
    assert_eq!(cut.chars().count(), 2);
}

/// Full persistence round-trip. Serialized through the crate-wide
/// `HOME_LOCK` because NESTRA_HOME_DIR is process-global (same pattern as
/// the skills tests).
#[test]
fn save_list_inject_remove_knowledge_round_trip() {
    let _home_lock = crate::HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let home = tempfile::Builder::new().prefix("").tempdir().unwrap();
    // SAFETY: confined to this serialized test (HOME_LOCK held).
    std::env::set_var("NESTRA_HOME_DIR", home.path());
    let repo = tempfile::Builder::new().prefix("").tempdir().unwrap();

    let conn = Connection::open_in_memory().unwrap();
    crate::schema::build_v1(&conn).unwrap();
    // Index row + the REAL source file (index-only store: parts come from
    // an on-demand parse, not from a mirror table).
    let src = repo.path().join("s-1.jsonl");
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
             VALUES ('pi-cli','s-1','Fix login','',0,0,1,'x','pi resume',?1,?2)",
        rusqlite::params![
            repo.path().to_string_lossy(),
            source_files,
        ],
    )
    .unwrap();

    let parts = parts_for_session(&conn, "pi-cli", "s-1").unwrap();
    assert_eq!(parts.len(), 1, "on-demand read parses the indexed source file");
    let md = render_markdown("Fix login", &build_sections(&parts));

    let info = save_handoff(&conn, "pi-cli", "s-1", &md).unwrap();
    assert!(info.artifact_path.contains(".nestra"),
        "artifact lands inside the session repo: {}",
        info.artifact_path
    );
    assert!(std::path::Path::new(&info.artifact_path).exists());
    assert_eq!(info.sections.goal.as_deref(), Some("goal text"));
    assert_eq!(info.token_snapshot, None, "no usage recorded → null snapshot");

    let listed = list_handoffs(&conn, "pi-cli", "s-1").unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, info.id);
    assert_eq!(listed[0].markdown.as_deref(), Some(md.as_str()));

    // Inject: .pi/handoff-<id>.md + one deduplicated reference line.
    let injected = inject_handoff(&conn, &info.id).unwrap();
    assert!(std::path::Path::new(&injected).exists());
    let append_path = repo.path().join(".pi").join("APPEND_SYSTEM.md");
    let append = std::fs::read_to_string(&append_path).unwrap();
    assert_eq!(append.matches("nestra-handoff").count(), 1);
    inject_handoff(&conn, &info.id).unwrap();
    let append = std::fs::read_to_string(&append_path).unwrap();
    assert_eq!(append.matches("nestra-handoff").count(), 1, "repeat inject is idempotent");

    // Remove: reference line + injected file gone, artifact untouched.
    remove_injected_handoff(&conn, &info.id).unwrap();
    assert!(!std::path::Path::new(&injected).exists());
    let append = std::fs::read_to_string(&append_path).unwrap();
    assert!(!append.contains(&info.id));
    assert!(std::path::Path::new(&info.artifact_path).exists());

    // Knowledge: frontmatter file under the (overridden) Nestra home.
    let k = handoff_to_knowledge(&conn, &info.id, "decision").unwrap();
    let k_dir = home.path().join(".nestra").join("knowledge");
    assert!(k.starts_with(k_dir.to_string_lossy().as_ref()), "{k}");
    let body = std::fs::read_to_string(&k).unwrap();
    assert!(body.starts_with("---\ntype: decision\n"));
    assert!(body.contains("# Handoff: Fix login"));

    // Delete: row gone, the user's artifact file stays.
    delete_handoff(&conn, &info.id).unwrap();
    assert!(list_handoffs(&conn, "pi-cli", "s-1").unwrap().is_empty());
    assert!(std::path::Path::new(&info.artifact_path).exists());
}

/// A session without cwd parks its artifacts under the Nestra home instead.
#[test]
fn save_without_cwd_uses_nestra_home() {
    let _home_lock = crate::HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let home = tempfile::Builder::new().prefix("").tempdir().unwrap();
    // SAFETY: confined to this serialized test (HOME_LOCK held).
    std::env::set_var("NESTRA_HOME_DIR", home.path());

    let conn = Connection::open_in_memory().unwrap();
    crate::schema::build_v1(&conn).unwrap();
    conn.execute(
        "INSERT INTO session (provider, id, title, summary, started_at, updated_at,
                                  message_count, source_path, resume_command)
             VALUES ('pi-cli','s-2','t','',0,0,0,'x','pi resume')",
        [],
    )
    .unwrap();
    let info = save_handoff(&conn, "pi-cli", "s-2", "# Handoff: t\n").unwrap();
    assert!(info.artifact_path.starts_with(
        home.path().join(".nestra").join("handoffs").to_string_lossy().as_ref()
    ));
}