//! Review context gathering (Review Runtime R1): assemble the review input
//! from the reviewed session's semantic parts + the working tree, render the
//! review prompt, and parse the verdict out of the reviewer's final message.
//! The structured verdict FILE (`.nestra/reviews/<id>/verdict.md`) is R2;
//! R1 asks the reviewer to open its reply with a `VERDICT:` line instead.

use rusqlite::Connection;

use crate::error::{AppError, AppResult};
use crate::session::handoff;

/// What the reviewer sees. `diff` is `None` when git is absent or the cwd
/// isn't a repo — the review degrades to "files only".
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ContextPack {
    pub title: String,
    pub goal: Option<String>,
    pub modified_files: Vec<String>,
    pub failed_attempts: Vec<String>,
    /// Best-effort TEST-RELATED execution evidence (recent, ≤3): lines like
    /// `Bash (failed) — first output line`. NOT a test detector — a
    /// conservative heuristic, prefer missing over false positives. Status
    /// comes ONLY from the structured `ToolResult.is_error`; `None` renders
    /// without a status word.
    pub test_results: Vec<String>,
    pub diff: Option<String>,
}

/// Cap on the embedded diff — a huge diff overflows the reviewer's own
/// context (spec risk); the file list survives regardless.
const DIFF_CAP: usize = 30_000;

pub fn gather(conn: &Connection, provider: &str, session_id: &str) -> AppResult<(ContextPack, Option<String>)> {
    let (title, cwd): (String, Option<String>) = conn
        .query_row(
            "SELECT title, cwd FROM session WHERE provider = ?1 AND id = ?2",
            rusqlite::params![provider, session_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|_| AppError::Validation(format!("session {provider}/{session_id} not found")))?;
    let parts = handoff::parts_for_session(conn, provider, session_id)?;
    let sections = handoff::build_sections(&parts);
    let test_results = extract_test_results(&parts);
    let diff = cwd
        .as_deref()
        .filter(|c| !c.is_empty())
        .and_then(|c| git_diff(c));
    Ok((
        ContextPack {
            title,
            goal: sections.goal,
            modified_files: sections.modified_files,
            failed_attempts: sections.failed_attempts,
            test_results,
            diff,
        },
        cwd,
    ))
}

/// Conservative test-related invocation detection. Shell tools count only
/// when the command's tokens match known test vocabulary; a tool whose NAME
/// contains `test` also counts. "latest"-style false positives are excluded
/// by construction (token must equal/start with the test vocabulary, or be a
/// known runner followed by a test-prefixed token).
fn is_test_invocation(tool_name: &str, input: &str) -> bool {
    let lower = tool_name.to_lowercase();
    let shell = matches!(lower.as_str(), "bash" | "zsh" | "sh" | "powershell" | "pwsh");
    if !shell {
        return lower.contains("test");
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(input) else {
        return false;
    };
    let Some(cmd) = ["command", "cmd", "script"]
        .iter()
        .find_map(|k| v.get(*k).and_then(|c| c.as_str()))
    else {
        return false;
    };
    let tokens: Vec<&str> = cmd.split_whitespace().collect();
    if tokens.is_empty() {
        return false;
    }
    let testish = |t: &str| {
        matches!(t, "test" | "tests" | "pytest" | "vitest" | "jest")
            || t.starts_with("test:")
            || t.starts_with("test-")
    };
    if tokens.iter().any(|t| testish(t)) {
        return true;
    }
    const RUNNERS: [&str; 8] = ["npm", "pnpm", "yarn", "cargo", "go", "gradle", "mvn", "uv"];
    tokens.first().is_some_and(|f| RUNNERS.contains(f))
        && tokens.iter().skip(1).any(|t| t.starts_with("test"))
}

/// The last ≤3 test-related results as one-line evidence, using ONLY the
/// structured `is_error` field for status.
fn extract_test_results(parts: &[crate::session::Part]) -> Vec<String> {
    // call_id → (tool name, input) — the pairing anchor shared with handoff.
    let mut calls: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();
    let mut out: Vec<String> = Vec::new();
    for p in parts {
        match &p.payload {
            crate::session::PartPayload::ToolInvocation { name, input, .. } => {
                if let Some(id) = &p.tool_call_id {
                    calls.insert(id.clone(), (name.clone(), input.clone().unwrap_or_default()));
                }
            }
            crate::session::PartPayload::ToolResult { output, is_error, .. } => {
                let Some(id) = &p.tool_call_id else { continue };
                let Some((name, input)) = calls.get(id) else { continue };
                if !is_test_invocation(name, input) {
                    continue;
                }
                let status = match is_error {
                    Some(true) => " (failed)",
                    Some(false) => " (passed)",
                    None => "",
                };
                let first_line = output
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .trim();
                let line = truncate(first_line, 160);
                out.push(if line.is_empty() {
                    format!("{name}{status}")
                } else {
                    format!("{name}{status} — {line}")
                });
            }
            _ => {}
        }
    }
    out.into_iter().rev().take(3).collect::<Vec<_>>().into_iter().rev().collect()
}

/// `git -C <cwd> diff HEAD` (staged + unstaged vs HEAD), truncated. Best-effort:
/// any git failure yields `None` (degrade, don't fail the review).
fn git_diff(cwd: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["diff", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    if text.is_empty() {
        return None;
    }
    Some(truncate(&text, DIFF_CAP))
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let cut = s.char_indices().nth(max - 1).map_or(max, |(i, _)| i + 1);
    format!("{}\n… (truncated)", &s[..cut])
}

/// The review prompt. Two verdict surfaces are requested: the structured
/// `.nestra/reviews/<id>/verdict.md` artifact (primary) and the `VERDICT:`
/// first reply line (fallback when the file wasn't written).
pub fn render_prompt(pack: &ContextPack, review_id: &str) -> String {
    let mut p = String::new();
    p.push_str("You are reviewing a completed coding session. Assess the work and reply with a verdict.\n\n");
    p.push_str(&format!("## Task\n{}\n", pack.title));
    if let Some(goal) = &pack.goal {
        p.push_str(&format!("\n{goal}\n"));
    }
    if !pack.modified_files.is_empty() {
        p.push_str("\n## Modified files\n");
        for f in &pack.modified_files {
            p.push_str(&format!("- `{f}`\n"));
        }
    }
    if !pack.failed_attempts.is_empty() {
        p.push_str("\n## Failed attempts during the session\n");
        for f in &pack.failed_attempts {
            p.push_str(&format!("- {f}\n"));
        }
    }
    if !pack.test_results.is_empty() {
        p.push_str("\n## Test results (observed)\n");
        for t in &pack.test_results {
            p.push_str(&format!("- {t}\n"));
        }
    }
    if let Some(diff) = &pack.diff {
        p.push_str("\n## Working-tree diff\n```diff\n");
        p.push_str(diff);
        p.push_str("\n```\n");
    } else {
        p.push_str("\n(No git diff available — review against the file list.)\n");
    }
    p.push_str(&format!(
        "\nWrite your verdict to `.nestra/reviews/{review_id}/verdict.md` with YAML frontmatter:\n\
         ```\n---\nstatus: pass | changes_requested | fail\nseverity: low | medium | high\nsummary: <one line>\n---\n<findings>\n```\n"
    ));
    p.push_str("\nALSO start your reply with EXACTLY one of:\nVERDICT: pass\nVERDICT: changes_requested\nVERDICT: fail\nThen a short summary of findings.\n");
    p
}

/// Path of the structured verdict artifact next to the context pack.
pub fn verdict_file_path(cwd: Option<&str>, review_id: &str) -> std::path::PathBuf {
    let base = match cwd.filter(|c| !c.is_empty()) {
        Some(c) => std::path::PathBuf::from(c).join(".nestra"),
        None => crate::db::home_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")).join(".nestra"),
    };
    base.join("reviews").join(review_id).join("verdict.md")
}

/// Parsed verdict frontmatter (`status`/`severity`/`summary` + freeform body).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedVerdict {
    pub status: Option<String>,
    pub severity: Option<String>,
    pub summary: Option<String>,
}

/// Minimal `key: value` frontmatter reader (between two `---` lines). No YAML
/// dependency — the format is three known scalar keys.
pub fn parse_verdict_frontmatter(content: &str) -> ParsedVerdict {
    let mut lines = content.trim_start().lines();
    if lines.next() != Some("---") {
        return ParsedVerdict::default();
    }
    let mut out = ParsedVerdict::default();
    let mut in_frontmatter = true;
    let mut body: Vec<&str> = Vec::new();
    for line in lines {
        if in_frontmatter {
            if line.trim() == "---" {
                in_frontmatter = false;
                continue;
            }
            if let Some((k, v)) = line.split_once(':') {
                let v = v.trim();
                match k.trim() {
                    "status" => out.status = Some(v.to_string()),
                    "severity" => out.severity = Some(v.to_string()),
                    "summary" => out.summary = Some(v.to_string()),
                    _ => {}
                }
            }
        } else {
            body.push(line);
        }
    }
    if out.summary.is_none() {
        let joined = body.join("\n").trim().to_string();
        if !joined.is_empty() {
            out.summary = Some(joined);
        }
    }
    out
}

/// Merge the two verdict surfaces: the artifact file (when written) wins over
/// the reply's `VERDICT:` line. Invalid file statuses are ignored (fallback
/// applies), and the summary falls back reply→file→body.
pub fn merge_verdict(file_content: Option<&str>, reply_text: &str) -> (Option<String>, String) {
    let (mut status, mut summary) = verdict_from_text(reply_text);
    if let Some(content) = file_content {
        let pv = parse_verdict_frontmatter(content);
        if matches!(
            pv.status.as_deref(),
            Some("pass") | Some("changes_requested") | Some("fail")
        ) {
            status = pv.status;
        }
        if let Some(s) = pv.summary {
            if !s.is_empty() {
                summary = truncate(&s, 600);
            }
        }
    }
    (status, summary)
}

/// Parse the verdict out of the reviewer's final message: status from the
/// `VERDICT:` first line, summary = the rest (capped).
pub fn verdict_from_text(text: &str) -> (Option<String>, String) {
    let mut lines = text.lines();
    let mut status = None;
    if let Some(first) = lines.next() {
        let low = first.trim().to_lowercase();
        for s in ["pass", "changes_requested", "fail"] {
            if low == format!("verdict: {s}") {
                status = Some(s.to_string());
                break;
            }
        }
    }
    let summary: String = lines.collect::<Vec<_>>().join(" ");
    let summary = summary.trim();
    let summary = if summary.is_empty() {
        first_line_or(text)
    } else {
        summary.to_string()
    };
    (status, truncate(&summary, 600))
}

fn first_line_or(text: &str) -> String {
    text.lines().next().unwrap_or("").to_string()
}

/// Write the context pack to `.nestra/reviews/<id>/context.md` for
/// reproducibility (repo cwd when known, else the Nestra home).
pub fn write_context_md(cwd: Option<&str>, review_id: &str, pack: &ContextPack) -> AppResult<String> {
    let base = match cwd.filter(|c| !c.is_empty()) {
        Some(c) => std::path::PathBuf::from(c).join(".nestra"),
        None => crate::db::home_dir()?.join(".nestra"),
    };
    let dir = base.join("reviews").join(review_id);
    std::fs::create_dir_all(&dir)
        .map_err(|e| AppError::Internal(format!("create review dir: {e}")))?;
    let path = dir.join("context.md");
    std::fs::write(&path, serde_json::to_string_pretty(pack).unwrap_or_default())
        .map_err(|e| AppError::Internal(format!("write review context: {e}")))?;
    Ok(path.to_string_lossy().into_owned())
}

/// Parse a stored `context_pack_json` back (best-effort).
pub fn pack_from_json(v: &str) -> Option<ContextPack> {
    serde_json::from_str(v).ok()
}

#[cfg(test)]
mod tests {
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
        conn.execute(
            "INSERT INTO session (provider, id, title, summary, started_at, updated_at,
                                  message_count, source_path, resume_command, cwd)
             VALUES ('pi-cli','s-1','Fix login','',0,0,1,'x','pi --session s-1',?1)",
            rusqlite::params![dir.path().to_string_lossy()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_part (provider, session_id, seq, part_idx, kind, payload_json,
                                       tool_call_id, raw_json, provider_metadata_json)
             VALUES ('pi-cli','s-1',0,0,'user_message',?1,NULL,'','{}')",
            rusqlite::params![
                serde_json::to_string(&crate::session::PartPayload::UserMessage {
                    text: "goal text".into()
                })
                .unwrap()
            ],
        )
        .unwrap();
        let (pack, cwd) = gather(&conn, "pi-cli", "s-1").unwrap();
        assert_eq!(pack.title, "Fix login");
        assert_eq!(pack.goal.as_deref(), Some("goal text"));
        assert_eq!(cwd.as_deref().map(str::to_string), Some(dir.path().to_string_lossy().into_owned()));
        // No git repo in the tempdir → diff None, gather still succeeds.
        assert!(pack.diff.is_none());
    }
}
