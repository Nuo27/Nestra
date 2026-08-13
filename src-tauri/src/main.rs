// Prevents an extra console window opening on Windows in release builds.
// In debug the console stays attached so the tracing logs (logging.rs also
// writes to stderr) remain visible in `pnpm tauri dev`.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    nestra_lib::run();
}