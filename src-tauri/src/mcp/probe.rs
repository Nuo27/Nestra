//! MCP server health probe.
//!
//! Spins up the server's transport in a short-lived subprocess (stdio) or
//! issues a single request (http/sse) and reports back whether it answered.
//! Used by the MCP page's "Test" button — click-to-test, no background
//! polling, no schema change.
//!
//! Every fallible step returns `AppError` rather than panicking; release is
//! `panic = "abort"`, so a panic here would kill the app.

use crate::db::home_dir;
use crate::error::{AppError, AppResult};
use crate::mcp::providers;
use rusqlite::Connection;
use serde::Serialize;
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// `CREATE_NO_WINDOW` — this app is `windows_subsystem = "windows"` (no parent
/// console), so a console child (e.g. an `npx`/`.cmd` MCP server) spawned
/// without this flag flashes its own console. Keeps the "Test" probe silent.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// How long any single probe attempt is allowed to take before we kill it.
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);

/// JSON-RPC `initialize` request sent on the wire to start a stdio MCP
/// session. The MCP spec requires it before any other call; servers that
/// answer with a JSON object (any object) count as reachable.
const INIT_REQUEST: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"nestra","version":"0.1"}}}"#;

/// Outcome of one probe attempt.
#[derive(Debug, Serialize)]
pub struct ProbeResult {
    pub ok: bool,
    pub latency_ms: Option<u64>,
    pub reason: Option<String>,
}

/// Look up a server's transport from the DB. DB-only — call under a short
/// lock, drop it, then run the probe itself without holding it.
pub fn fetch_transport(conn: &Connection, id: &str) -> AppResult<crate::mcp::McpTransport> {
    let row = crate::db::get_mcp_server(conn, id)?
        .ok_or_else(|| AppError::NotFound(format!("mcp server not found: {id}")))?;
    serde_json::from_str(&row.transport_json)
        .map_err(|e| AppError::Internal(format!("corrupt mcp transport: {e}")))
}

#[cfg(test)]
/// Probe a managed MCP server (test-only helper — the live path is
/// `mcp_probe` in commands.rs via `fetch_transport` + `probe_transport`).
pub fn probe(conn: &Connection, id: &str) -> AppResult<ProbeResult> {
    let transport = fetch_transport(conn, id)?;
    probe_transport(&transport)
}

/// Run one probe of a server's transport. Pure runtime check — no `&Connection`,
/// no lock, so a slow/hung server only ties up this (blocking-pool) thread.
pub fn probe_transport(transport: &crate::mcp::McpTransport) -> AppResult<ProbeResult> {
    match transport.kind {
        crate::mcp::McpKind::Stdio => probe_stdio(transport),
        crate::mcp::McpKind::Http => probe_http(transport),
        crate::mcp::McpKind::Sse => probe_sse(transport),
    }
}

fn probe_stdio(t: &crate::mcp::McpTransport) -> AppResult<ProbeResult> {
    let cmd_str = match t.command.as_deref() {
        Some(c) if !c.trim().is_empty() => c.to_string(),
        _ => return Ok(fail("no command")),
    };
    // `Command::spawn` panics on a NUL byte in the program path/args/env —
    // the transport comes from the DB (user-editable), so validate pre-spawn.
    if cmd_str.contains('\0')
        || t.args.iter().any(|a| a.contains('\0'))
        || t.env.values().any(|v| v.contains('\0'))
        || t.env.keys().any(|k| k.contains('\0') || k.is_empty() || k.contains('='))
    {
        return Ok(fail("command contains invalid characters (NUL / empty / '=' env key)"));
    }
    let home = home_dir().ok();
    // Pick a representative config path for the wrap test (any will do; the
    // check is per-command, not per-target).
    let cfg_path = providers::for_agent("claude-code-cli")
        .map(|p| p.config_path(home.as_deref().unwrap_or_else(|| std::path::Path::new("."))))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let (cmd, args) = providers::wrap_command_for_windows(&Some(cmd_str), &t.args, &cfg_path);

    let mut command = match cmd {
        Some(c) => Command::new(c),
        None => return Ok(fail("empty command")),
    };
    for a in &args {
        command.arg(a);
    }
    for (k, v) in &t.env {
        command.env(k, v);
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // Hide the console window a `.cmd`/console child would otherwise flash
    // while the user clicks "Test".
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);

    let started = Instant::now();
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => return Ok(fail(&format!("spawn failed: {e}"))),
    };

    // kill + wait on EVERY early exit: kill alone leaves a zombie child.
    let mut stdin = match child.stdin.take() {
        Some(s) => s,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(fail("no stdin"));
        }
    };
    let mut stdout = match child.stdout.take() {
        Some(s) => s,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(fail("no stdout"));
        }
    };

    // Write initialize line; MCP stdio servers expect one JSON-RPC request
    // per line.
    if let Err(e) = writeln!(stdin, "{INIT_REQUEST}") {
        let _ = child.kill();
        let _ = child.wait();
        return Ok(fail(&format!("write failed: {e}")));
    }
    if let Err(e) = stdin.flush() {
        let _ = child.kill();
        let _ = child.wait();
        return Ok(fail(&format!("flush failed: {e}")));
    }
    // NOTE: do NOT close stdin yet. A conformant MCP server needs the session
    // alive to answer `initialize` — it's the first request of a long-lived
    // JSON-RPC connection. Closing stdin immediately after writing it made
    // servers with async startup (e.g. `codegraph`) exit before writing any
    // response (`server closed stdout with no output`). The read below is
    // bounded by `PROBE_TIMEOUT`, after which we kill the child, so the held
    // stdin never leaks. This mirrors how a real stdio client (Claude Code, …)
    // drives a server.

    // Read stdout on a helper thread so we can apply a recv_timeout on the
    // main thread without blocking the whole runtime. The handle is joined
    // on the timeout path so a grandchild holding the pipe can't leak the
    // thread + fd per probe.
    let (tx, rx) = mpsc::channel::<AppResult<Option<String>>>();
    let reader = thread::spawn(move || {
        let mut buf = Vec::with_capacity(4096);
        let mut byte = [0u8; 1];
        loop {
            match stdout.read(&mut byte) {
                Ok(0) => break, // EOF
                Ok(_) => {
                    buf.push(byte[0]);
                    if byte[0] == b'\n' {
                        break;
                    }
                    if buf.len() > 64 * 1024 {
                        break; // cap at 64 KiB so a runaway server doesn't OOM us
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(AppError::Io(e)));
                    return;
                }
            }
        }
        // Distinguish a UTF-8 decode failure from a clean EOF: the former is
        // "server said something garbage", the latter "closed with no output".
        let line = if buf.is_empty() {
            None
        } else {
            match String::from_utf8(buf) {
                Ok(s) => Some(s.trim_end_matches(['\r', '\n']).to_string()),
                Err(_) => {
                    let _ = tx.send(Err(AppError::Internal(
                        "server output was not valid UTF-8".into(),
                    )));
                    return;
                }
            }
        };
        let _ = tx.send(Ok(line));
    });

    let outcome = match rx.recv_timeout(PROBE_TIMEOUT) {
        // A line only counts as a SUCCESS when it parses as a JSON object —
        // the MCP initialize response is `{"jsonrpc":"2.0",...}`. Any old
        // line (e.g. a bare log line before the handshake) is not a probe
        // success.
        Ok(Ok(Some(line))) => {
            let valid = serde_json::from_str::<serde_json::Value>(&line)
                .map(|v| v.is_object())
                .unwrap_or(false);
            if valid {
                ProbeResult {
                    ok: true,
                    latency_ms: Some(started.elapsed().as_millis() as u64),
                    reason: None,
                }
            } else {
                fail("server output was not a JSON-RPC response")
            }
        }
        Ok(Ok(None)) => fail("server closed stdout with no output"),
        Ok(Err(e)) => fail(&format!("read failed: {e}")),
        Err(_timeout) => {
            let _ = child.kill();
            // Reap the reader so a pipe-holding grandchild can't leak the
            // thread — but BOUNDED: if the grandchild keeps the write end
            // open the read never EOFs and a bare join() would block this
            // probe forever. Wait a short grace period, then detach (the
            // thread exits by itself once the pipe finally closes).
            let (joined_tx, joined_rx) = mpsc::channel::<()>();
            // Detached: if the grace period lapses, this helper (and the
            // reader inside it) keep running until the pipe closes — the
            // caller is never held hostage.
            thread::spawn(move || {
                let _ = reader.join();
                let _ = joined_tx.send(());
            });
            let _ = joined_rx.recv_timeout(Duration::from_secs(2));
            fail(&format!("timed out after {}s", PROBE_TIMEOUT.as_secs()))
        }
    };
    // Now that we have a result (or timed out), close stdin and reap the child
    // so it doesn't linger as a zombie. stdin was deliberately held open for
    // the duration of the read — see the NOTE above.
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    Ok(outcome)
}

fn probe_http(t: &crate::mcp::McpTransport) -> AppResult<ProbeResult> {
    let url = match t.url.as_deref() {
        Some(u) if !u.trim().is_empty() => u,
        _ => return Ok(fail("no url")),
    };
    // `ureq::post` panics on an unparseable URL (missing/unsupported scheme) —
    // the URL comes from the DB, so validate before dialing.
    if url::Url::parse(url).is_err() {
        return Ok(fail("invalid url"));
    }
    let started = Instant::now();
    // MCP-over-HTTP: POST the initialize request; any 2xx (or a parseable JSON
    // object back) counts as reachable. We don't validate the response body —
    // a future server may speak a newer protocol version and that's fine.
    let req = ureq::post(url)
        .timeout(PROBE_TIMEOUT)
        .set("Content-Type", "application/json")
        .set("Accept", "application/json, text/event-stream");
    let resp = req.send_string(INIT_REQUEST);
    match resp {
        Ok(r) => Ok(ProbeResult {
            ok: (200..300).contains(&r.status()),
            latency_ms: Some(started.elapsed().as_millis() as u64),
            reason: if !(200..300).contains(&r.status()) {
                Some(format!("HTTP {}", r.status()))
            } else {
                None
            },
        }),
        Err(ureq::Error::Status(code, _)) => Ok(ProbeResult {
            ok: false,
            latency_ms: Some(started.elapsed().as_millis() as u64),
            reason: Some(format!("HTTP {code}")),
        }),
        Err(e) => Ok(fail(&format!("request failed: {e}"))),
    }
}

fn probe_sse(t: &crate::mcp::McpTransport) -> AppResult<ProbeResult> {
    let url = match t.url.as_deref() {
        Some(u) if !u.trim().is_empty() => u,
        _ => return Ok(fail("no url")),
    };
    // Same pre-dial validation as probe_http (ureq panics on bad URLs).
    if url::Url::parse(url).is_err() {
        return Ok(fail("invalid url"));
    }
    let started = Instant::now();
    // SSE: a plain GET — the server should respond 200 with `text/event-stream`
    // headers. We only check reachability + content-type; reading the actual
    // event stream would keep the connection open past our timeout budget.
    let resp = ureq::get(url)
        .timeout(PROBE_TIMEOUT)
        .set("Accept", "text/event-stream")
        .call();
    match resp {
        Ok(r) => {
            let ct = r
                .header("Content-Type")
                .unwrap_or("")
                .to_ascii_lowercase();
            let ok = (200..300).contains(&r.status()) && ct.contains("event-stream");
            Ok(ProbeResult {
                ok,
                latency_ms: Some(started.elapsed().as_millis() as u64),
                reason: if !ok {
                    Some(format!("HTTP {} ct={}", r.status(), ct))
                } else {
                    None
                },
            })
        }
        Err(ureq::Error::Status(code, _)) => Ok(ProbeResult {
            ok: false,
            latency_ms: Some(started.elapsed().as_millis() as u64),
            reason: Some(format!("HTTP {code}")),
        }),
        Err(e) => Ok(fail(&format!("request failed: {e}"))),
    }
}

fn fail(reason: &str) -> ProbeResult {
    ProbeResult {
        ok: false,
        latency_ms: None,
        reason: Some(reason.to_string()),
    }
}

#[cfg(test)]
mod tests;