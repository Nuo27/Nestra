//! Shared test seam: one `temp_home()` instead of five per-file copies that
//! differed only in the tempdir prefix (a debugging aid tempfile already
//! randomizes). `#[cfg(test)]`-gated — never compiled into the binary.

use std::path::PathBuf;

pub(crate) fn temp_home() -> (PathBuf, tempfile::TempDir) {
    let dir = tempfile::Builder::new()
        .prefix("nestra-test-")
        .tempdir()
        .expect("tempdir");
    (dir.path().to_path_buf(), dir)
}
