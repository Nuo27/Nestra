//! Claude Code session importer.
//!
//! ~/.claude/projects/<proj>/*.jsonl incl. nested <id>/subagents/*.jsonl.
//! Canonical id = sessionId; sidechain files (isSidechain:true) become children
//! keyed by agentId, parent = the sessionId they carry.

use crate::error::AppResult;
use crate::session::RawFile;
use crate::session::{
    jsonl_files_under, mtime_millis, parse_jsonl_events, rawfile_from_jsonl,
    self_dir, SessionImporter,
};

pub struct ClaudeImporter;

/// Registry constructor — see [super::SPEC].
pub fn new() -> Box<dyn crate::session::SessionImporter> {
    Box::new(ClaudeImporter)
}
impl SessionImporter for ClaudeImporter {
    fn snapshot(&self) -> AppResult<Vec<(String, i64)>> {
        let dir = self_dir(".claude", &["projects"])?;
        Ok(jsonl_files_under(&dir)
            .into_iter()
            .map(|p| (p.to_string_lossy().to_string(), mtime_millis(&p)))
            .collect())
    }
    fn import(&self) -> AppResult<Vec<RawFile>> {
        let dir = self_dir(".claude", &["projects"])?;
        if !dir.exists() {
            return Ok(vec![]);
        }
        let mut out = Vec::new();
        for f in jsonl_files_under(&dir) {
            if let Ok(p) = parse_jsonl_events(&f) {
                out.push(rawfile_from_jsonl(&f, p));
            }
        }
        Ok(out)
    }
}
