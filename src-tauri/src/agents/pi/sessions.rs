//! Pi session importer.
//!
//! ~/.pi/agent/sessions/*.jsonl; canonical id = type:"session" header id.

use crate::error::AppResult;
use crate::session::RawFile;
use crate::session::{import_jsonl_dir, jsonl_snapshot, self_dir, SessionImporter};

pub struct PiImporter;

/// Registry constructor — see [super::SPEC].
pub fn new() -> Box<dyn crate::session::SessionImporter> {
    Box::new(PiImporter)
}
impl SessionImporter for PiImporter {
    fn snapshot(&self) -> AppResult<Vec<(String, i64)>> {
        jsonl_snapshot(self_dir(".pi", &["agent", "sessions"])?)
    }
    fn import(&self) -> AppResult<Vec<RawFile>> {
        import_jsonl_dir(self_dir(".pi", &["agent", "sessions"])?)
    }
}
