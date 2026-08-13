use std::fs::OpenOptions;
use std::sync::Mutex;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

pub fn init() {
    let log_dir = match crate::db::log_dir() {
        Ok(d) => d,
        Err(_) => return, // logging must never block startup
    };
    let _ = std::fs::create_dir_all(&log_dir);

    // Single file, truncated on each launch. Local-only tool — no rotation.
    let log_path = log_dir.join("nestra.log");
    let file = match OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&log_path)
    {
        Ok(f) => f,
        Err(_) => return,
    };
    // tracing's fmt layer writes the WHOLE event with one `write_all`, but a
    // bare `File` is not synchronized — concurrent writers (gateway threads,
    // quota worker, UI commands) interleave/garble long lines. An Arc<Mutex>
    // makes each event atomic; the MakeWriter clones the Arc per event.
    let file = std::sync::Arc::new(Mutex::new(file));

    let env_filter = EnvFilter::try_from_env("NESTRA_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info,tauri=warn,nestra_lib=info"));

    let subscriber = tracing_subscriber::registry()
        .with(env_filter)
        .with(
            fmt::layer()
                .with_writer(MutexGuardWriter(file))
                .with_ansi(false)
                .with_target(true),
        )
        .with(fmt::layer().with_writer(std::io::stderr).compact());

    let _ = tracing::subscriber::set_global_default(subscriber);
}

/// `fmt::MakeWriter` wrapper around `Arc<Mutex<File>>`: each `make_writer()`
/// call clones the Arc and locks it, so the returned guard serializes one
/// event's write.
struct MutexGuardWriter(std::sync::Arc<Mutex<std::fs::File>>);

impl std::io::Write for MutexGuardWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().map_err(|_| std::io::Error::other("log mutex poisoned"))?.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().map_err(|_| std::io::Error::other("log mutex poisoned"))?.flush()
    }
}

impl tracing_subscriber::fmt::MakeWriter<'_> for MutexGuardWriter {
    type Writer = MutexGuardWriter;
    fn make_writer(&self) -> Self::Writer {
        MutexGuardWriter(self.0.clone())
    }
}