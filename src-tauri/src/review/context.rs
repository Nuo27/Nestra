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
mod tests;
