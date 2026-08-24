//! Crash capture: a Rust panic appends a timestamped entry to
//! `<log_dir>/crash.log` before the default hook runs. The previous session's
//! main log is retained as `nestra.log.1` (see `logging`), so a crash report
//! plus the session's tail are always available for `diag_export_logs`.
//!
//! Appends across launches (the file spans sessions until the user clears
//! it). Best-effort: a failure to write the report falls through to the
//! default hook silently — crash reporting must never be the thing that
//! crashes.

use std::io::Write;
use std::path::PathBuf;

/// Install the crash-logging panic hook against the real log dir.
pub fn install() {
    if let Ok(dir) = crate::db::log_dir() {
        install_at(dir);
    }
}

/// The install seam: crashes land in `dir` (tests point this at a tempdir).
pub(crate) fn install_at(dir: PathBuf) {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        append_entry(&dir, info);
        default(info);
    }));
}

fn append_entry(dir: &std::path::Path, info: &std::panic::PanicHookInfo<'_>) {
    let path = dir.join("crash.log");
    let payload = info.payload();
    let payload = if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    };
    let entry = format!(
        "[{}] thread '{}' panicked at {}:\n{}\n\n",
        chrono::Utc::now().to_rfc3339(),
        std::thread::current().name().unwrap_or("<unnamed>"),
        info.location()
            .map(|l| l.to_string())
            .unwrap_or_else(|| "<unknown>".into()),
        payload,
    );
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        // One write_all per entry: concurrent panics don't interleave mid-line.
        let _ = f.write_all(entry.as_bytes());
    }
}

#[cfg(test)]
mod tests;
