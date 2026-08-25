use std::sync::{Mutex, OnceLock};
use tracing_subscriber::{fmt, prelude::*, reload, EnvFilter};

/// How many days of rotated log files each output family keeps (text and
/// JSON independently — `diag_export_logs` ships the whole directory).
const RETAINED_FILES: usize = 14;

/// `setting_kv` key holding the persisted [`LevelPreset`] choice.
pub const LEVEL_KEY: &str = "log_level_preset";

/// `setting_kv` key holding the full-body-capture opt-in (debug events log
/// complete request/response bodies instead of 2 KiB snippets).
pub const FULL_BODIES_KEY: &str = "log_full_bodies";

/// Verbosity presets the UI offers. `Info` is the quiet default; `Debug`
/// adds the gateway's wire-level evidence (truncated request/response
/// bodies — see `orchestration/gateway/trace.rs`); `Trace` is everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LevelPreset {
    Info,
    Debug,
    Trace,
}

impl LevelPreset {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "info" => Some(Self::Info),
            "debug" => Some(Self::Debug),
            "trace" => Some(Self::Trace),
            _ => None,
        }
    }

    /// EnvFilter directives for this preset. The gateway's debug/trace
    /// instrumentation lives under `nestra_lib`, so the preset widens only
    /// Nestra's own targets — dependency chatter stays at warn.
    pub fn filter(&self) -> &'static str {
        match self {
            Self::Info => "info,tauri=warn,nestra_lib=info",
            Self::Debug => "info,tauri=warn,nestra_lib=debug",
            Self::Trace => "info,tauri=warn,nestra_lib=trace",
        }
    }
}

/// Hot-reload handle for the shared EnvFilter + the chosen preset mirror
/// (the handle has no read-back API, so `current_preset` reads the mirror).
static RELOAD: OnceLock<reload::Handle<EnvFilter, tracing_subscriber::Registry>> = OnceLock::new();
static CURRENT: Mutex<LevelPreset> = Mutex::new(LevelPreset::Info);

pub fn init() {
    let log_dir = match crate::db::log_dir() {
        Ok(d) => d,
        Err(_) => return, // logging must never block startup
    };
    let _ = std::fs::create_dir_all(&log_dir);

    // Two file families, both daily-rotated with bounded retention:
    // `nestra.<date>.log` — human-readable (full format, span prefixes);
    // `nestra.<date>.json` — machine-readable twin (JSON lines with the
    // span chain) that the in-app log viewer reads. A failed build falls
    // through to the remaining layers — never block startup on one file.
    let text = tracing_appender::rolling::RollingFileAppender::builder()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("nestra")
        .filename_suffix("log")
        .max_log_files(RETAINED_FILES)
        .build(&log_dir);
    let json = tracing_appender::rolling::RollingFileAppender::builder()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("nestra")
        .filename_suffix("json")
        .max_log_files(RETAINED_FILES)
        .build(&log_dir);

    let env_filter = EnvFilter::try_from_env("NESTRA_LOG")
        .unwrap_or_else(|_| EnvFilter::new(LevelPreset::Info.filter()));
    let (filter_layer, handle) = reload::Layer::new(env_filter);

    let subscriber = tracing_subscriber::registry()
        .with(filter_layer)
        .with(
            fmt::layer()
                .with_writer(std::io::stderr)
                .compact(),
        );
    // Both appenders write to the same directory — they succeed or fail
    // together. On failure fall back to the stderr mirror only (logging
    // must never block startup).
    let (text, json) = match (text, json) {
        (Ok(text), Ok(json)) => (text, json),
        _ => {
            let _ = tracing::subscriber::set_global_default(subscriber);
            return;
        }
    };
    let subscriber = subscriber
        .with(
            fmt::layer()
                .with_writer(MutexGuardWriter(std::sync::Arc::new(Mutex::new(text))))
                .with_ansi(false)
                .with_target(true),
        )
        .with(
            fmt::layer()
                .with_writer(MutexGuardWriter(std::sync::Arc::new(Mutex::new(json))))
                .with_ansi(false)
                .json(),
        );

    let _ = RELOAD.set(handle);
    let _ = tracing::subscriber::set_global_default(subscriber);
}

/// Switch the global filter to a preset at runtime. Returns whether the
/// live filter was actually swapped (false before `init` or if the reload
/// lock was taken — the mirror still reflects the requested preset so a
/// later `apply_persisted_preset` converges).
pub fn set_preset(preset: LevelPreset) -> bool {
    if let Ok(mut current) = CURRENT.lock() {
        *current = preset;
    }
    RELOAD.get().is_some_and(|handle| {
        handle
            .modify(|filter| *filter = EnvFilter::new(preset.filter()))
            .is_ok()
    })
}

/// The currently chosen preset (mirror; starts at Info).
pub fn current_preset() -> LevelPreset {
    *CURRENT.lock().unwrap_or_else(|e| e.into_inner())
}

/// Apply the persisted log settings once the DB is available
/// (`logging::init` runs before the DB opens): the verbosity preset (and
/// the full-body-capture opt-in for the gateway's debug wire evidence).
/// `NESTRA_LOG` wins when set — it is the developer override. Best-effort:
/// an unreadable or unknown value keeps the current level.
pub fn apply_persisted_settings(conn: &rusqlite::Connection) {
    if let Ok(Some(v)) = crate::db::get_setting(conn, FULL_BODIES_KEY) {
        crate::orchestration::gateway::trace::set_full_bodies(v.as_bool().unwrap_or(false));
    }
    if std::env::var_os("NESTRA_LOG").is_some() {
        return;
    }
    if let Ok(Some(v)) = crate::db::get_setting(conn, LEVEL_KEY) {
        if let Some(preset) = v.as_str().and_then(LevelPreset::parse) {
            set_preset(preset);
        }
    }
}

/// `fmt::MakeWriter` around `Arc<Mutex<W>>`: each `make_writer()` clones
/// the Arc and locks it, so one event's (possibly multi-call) write is
/// serialized against the concurrent gateway/quota/UI writers — a bare
/// appender interleaves calls from multiple threads and garbles long lines.
struct MutexGuardWriter<W: std::io::Write>(std::sync::Arc<Mutex<W>>);

impl<W: std::io::Write> std::io::Write for MutexGuardWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| std::io::Error::other("log mutex poisoned"))?
            .write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0
            .lock()
            .map_err(|_| std::io::Error::other("log mutex poisoned"))?
            .flush()
    }
}

impl<W: std::io::Write> tracing_subscriber::fmt::MakeWriter<'_> for MutexGuardWriter<W> {
    type Writer = Self;
    fn make_writer(&self) -> Self::Writer {
        MutexGuardWriter(self.0.clone())
    }
}

#[cfg(test)]
mod tests;
