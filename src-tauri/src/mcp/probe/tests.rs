use super::*;
use crate::db;
use crate::mcp::providers;
use crate::testutil::temp_home;

/// Stdio probe: spawn a Python script that replies to the first line with
/// a JSON-RPC-shaped object and asserts the probe sees it as OK.
#[test]
#[cfg(not(target_os = "windows"))]
fn probe_stdio_sees_echoing_server() {
    let _lock = crate::HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (home, _home_g) = temp_home();
    std::env::set_var("NESTRA_HOME_DIR", &home);
    let conn = Connection::open_in_memory().unwrap();
    db::migrate(&conn).unwrap();

    // Tiny echo server: read one line, write one line. Uses `readline()`
    // (not `for line in sys.stdin`) so it answers as soon as the newline
    // arrives — the probe keeps stdin open during the read (like a real
    // client), so a server that blocked waiting for EOF would never
    // answer. A real stdio MCP server reads exactly this way.
    let srv = home.join("echo.py");
    std::fs::write(
        &srv,
        "import sys\nsys.stdin.readline()\nsys.stdout.write('{\"ok\":true}\\n')\nsys.stdout.flush()\n",
    )
    .unwrap();

    db::upsert_mcp_server(
        &conn,
        "echo",
        "echo",
        &serde_json::to_string(&crate::mcp::McpTransport {
            kind: crate::mcp::McpKind::Stdio,
            command: Some("python3".into()),
            args: vec![srv.to_string_lossy().to_string()],
            env: Default::default(),
            url: None,
        })
        .unwrap(),
        &[],
        &[],
    )
    .unwrap();

    let r = probe(&conn, "echo").unwrap();
    assert!(r.ok, "stdio echo should succeed: {:?}", r.reason);
}

/// Stdio probe of a non-existent command must NOT panic and must report
/// `ok: false` with a reason.
#[test]
fn probe_stdio_handles_missing_command() {
    let _lock = crate::HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (home, _home_g) = temp_home();
    std::env::set_var("NESTRA_HOME_DIR", &home);
    let conn = Connection::open_in_memory().unwrap();
    db::migrate(&conn).unwrap();
    db::upsert_mcp_server(
        &conn,
        "missing",
        "missing",
        &serde_json::to_string(&crate::mcp::McpTransport {
            kind: crate::mcp::McpKind::Stdio,
            command: Some("definitely-not-a-real-binary-xyz".into()),
            args: vec![],
            env: Default::default(),
            url: None,
        })
        .unwrap(),
        &[],
        &[],
    )
    .unwrap();
    let r = probe(&conn, "missing").unwrap();
    assert!(!r.ok);
    assert!(r.reason.is_some());
}

/// Unknown id should be a clean AppError, not a panic.
#[test]
fn probe_unknown_id_returns_not_found() {
    let _lock = crate::HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (home, _home_g) = temp_home();
    std::env::set_var("NESTRA_HOME_DIR", &home);
    let conn = Connection::open_in_memory().unwrap();
    db::migrate(&conn).unwrap();
    let e = probe(&conn, "nope").unwrap_err();
    matches!(e, AppError::NotFound(_));
}

/// Sanity: providers::wrap_command_for_windows stays accessible (the
/// probe module depends on it via the providers re-export).
#[test]
fn wrap_command_for_windows_is_reachable() {
    let _ = providers::wrap_command_for_windows(
        &Some("python".into()),
        &["x".into()],
        std::path::Path::new("/tmp"),
    );
}