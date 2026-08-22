//! ZCode session importer — `~/.zcode/cli/db/db.sqlite`.
//!
//! ZCode stores sessions in its own SQLite using the shared
//! session/message/part + JSON-`data` layout (see [`super::partdb`]); this
//! module only resolves the db path. Canonical id = `session.id`; subagent
//! children link back via `parent_id`. Read-only — the resumable CLI ships
//! inside the desktop app and is not on PATH.

use crate::error::AppResult;
use crate::session::partdb;
use crate::session::{self_dir, mtime_millis, RawFile, SessionImporter};
use std::path::PathBuf;

pub struct ZCodeImporter;

/// Registry constructor — see [super::SPEC].
pub fn new() -> Box<dyn crate::session::SessionImporter> {
    Box::new(ZCodeImporter)
}

impl SessionImporter for ZCodeImporter {
    fn snapshot(&self) -> AppResult<Vec<(String, i64)>> {
        let db = zcode_db_path();
        if !db.is_file() {
            return Ok(vec![]);
        }
        Ok(vec![(db.to_string_lossy().to_string(), mtime_millis(&db))])
    }

    fn import(&self) -> AppResult<Vec<RawFile>> {
        let db = zcode_db_path();
        if !db.is_file() {
            return Ok(vec![]);
        }
        partdb::collect(&db)
    }
}

fn zcode_db_path() -> PathBuf {
    self_dir(".zcode", &["cli", "db", "db.sqlite"])
        .unwrap_or_else(|_| PathBuf::from(".zcode/cli/db/db.sqlite"))
}
