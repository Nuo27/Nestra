//! Minimal Pi RPC supervisor (Review Runtime R1).
//!
//! Owns ONE review Pi child (`pi --mode rpc`) and its pipes. The RPC surface
//! is deliberately exactly what the review runner needs: send a `prompt`,
//! observe the event stream until `agent_settled`, `abort`+reap on shutdown.
//! Events are buffered (snapshot for `review_get`) and forwarded through a
//! channel the runner drains. Tauri emission stays in the command layer —
//! this module holds no Tauri types (and, per contract #2, no credentials:
//! the review session talks to the `nestra-gw` alias; the gateway owns the
//! `CredentialHandle`).
//!
//! Modeled on `mcp/probe.rs::probe_stdio` (piped stdio, `CREATE_NO_WINDOW`,
//! line reader on a helper thread, kill+wait reaping).

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

use crate::error::{AppError, AppResult};

/// The reviewer role marker appended to the review session's system prompt.
/// The gateway's `SubagentRole::from_system_prompt` heuristic turns this into
/// the `pi:reviewer` policy key → `routing_policy(pi-cli, 'pi:reviewer')`
/// routes review traffic to the stronger endpoint. Zero gateway code change.
pub const REVIEWER_MARKER: &str = "<active_agent name=\"reviewer\"/>";

pub struct PiSupervisor {
    child: Mutex<Child>,
    stdin: Mutex<Option<ChildStdin>>,
    rx: Mutex<std::sync::mpsc::Receiver<Value>>,
    /// Buffered event log — `Arc` so the reader thread keeps writing after
    /// the supervisor handle is cloned into the runner task.
    events: Arc<Mutex<Vec<Value>>>,
}

impl PiSupervisor {
    /// Spawn the supervised review session. `args` is the FULL argument
    /// vector — the caller (the review command) builds the pi RPC invocation
    /// from [`REVIEWER_MARKER`]; tests substitute a shim (`node -e <script>`).
    pub fn spawn(exe: &str, args: &[String], cwd: Option<&str>) -> AppResult<Arc<Self>> {
        let mut cmd = Command::new(exe);
        cmd.args(args);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            // Mirror mcp/probe.rs: never flash a console window for the child.
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        if let Some(dir) = cwd.filter(|d| !d.is_empty()) {
            cmd.current_dir(dir);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| AppError::Internal(format!("spawn review session: {e}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AppError::Internal("review child has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AppError::Internal("review child has no stdout".into()))?;

        let (tx, rx) = std::sync::mpsc::channel::<Value>();
        let events = Arc::new(Mutex::new(Vec::<Value>::new()));
        let log = events.clone();
        // Line-reader thread: parse each stdout line as JSON, buffer it, and
        // forward to at most one consumer (the runner loop). A reader error
        // (child exit) drops `tx`, which the runner observes as disconnect.
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                let Ok(v) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if let Ok(mut l) = log.lock() {
                    l.push(v.clone());
                }
                if tx.send(v).is_err() {
                    break;
                }
            }
        });

        Ok(Arc::new(Self {
            child: Mutex::new(child),
            stdin: Mutex::new(Some(stdin)),
            rx: Mutex::new(rx),
            events,
        }))
    }

    /// Send one JSONL request (`{"type":"prompt","text":…}` etc.). Keeps
    /// stdin open — a conformant RPC child exits when its stdin closes.
    pub fn send(&self, req: &Value) -> AppResult<()> {
        let mut guard = self
            .stdin
            .lock()
            .map_err(|_| AppError::Internal("supervisor stdin lock poisoned".into()))?;
        let Some(w) = guard.as_mut() else {
            return Err(AppError::Internal("review session stdin closed".into()));
        };
        let mut line = serde_json::to_string(req)
            .map_err(|e| AppError::Internal(format!("serialize rpc request: {e}")))?;
        line.push('\n');
        w.write_all(line.as_bytes())
            .map_err(|e| AppError::Internal(format!("write rpc request: {e}")))?;
        w.flush()
            .map_err(|e| AppError::Internal(format!("flush rpc request: {e}")))?;
        Ok(())
    }

    /// Next event within `d` (`None` on timeout — the runner re-checks the
    /// child/deadline). A poisoned/disconnected channel also yields `None`.
    pub fn next_event(&self, d: Duration) -> Option<Value> {
        self.rx.lock().ok()?.recv_timeout(d).ok()
    }

    /// Buffered event log (for `review_get` while the review runs).
    pub fn events_snapshot(&self) -> Vec<Value> {
        self.events.lock().map(|l| l.clone()).unwrap_or_default()
    }

    /// `true` once the child has exited (poll-based; never blocks).
    pub fn is_finished(&self) -> bool {
        self.child
            .lock()
            .ok()
            .and_then(|mut c| c.try_wait().ok().flatten())
            .is_some()
    }

    /// Ask, then kill + reap. Idempotent; safe on an already-exited child.
    pub fn shutdown(&self) {
        let _ = self.send(&serde_json::json!({ "type": "abort" }));
        if let Ok(mut c) = self.child.lock() {
            let _ = c.kill();
            let _ = c.wait();
        }
        if let Ok(mut g) = self.stdin.lock() {
            *g = None;
        }
    }
}

impl Drop for PiSupervisor {
    /// Safety net: if the runner task dies without calling `shutdown`
    /// (panic, task abort), the last `Arc` drop still kills + reaps the
    /// node child instead of leaking it. `get_mut` needs no locking on the
    /// exclusive `&mut self` path.
    fn drop(&mut self) {
        if let Ok(c) = self.child.get_mut() {
            let _ = c.kill();
            let _ = c.wait();
        }
        if let Ok(g) = self.stdin.get_mut() {
            *g = None;
        }
    }
}

/// `agent_settled` seen — the documented "fully done" signal.
pub fn has_settled(events: &[Value]) -> bool {
    events.iter().any(|v| {
        v.get("type").and_then(Value::as_str) == Some("agent_settled")
            || v.get("event").and_then(Value::as_str) == Some("agent_settled")
    })
}

/// The spawned session's native id, when the RPC stream reveals it:
/// `session_id` / `sessionId` top-level, or nested under a `session` object
/// (a session-start event). Tolerant on purpose — the exact event shape is
/// the child's to define; `None` until (and unless) it appears.
pub fn session_id_of(events: &[Value]) -> Option<String> {
    let non_empty = |s: &str| !s.is_empty();
    for v in events {
        for key in ["session_id", "sessionId"] {
            if let Some(s) = v.get(key).and_then(Value::as_str).filter(|s| non_empty(s)) {
                return Some(s.to_string());
            }
        }
        if let Some(s) = v
            .get("session")
            .and_then(|sess| sess.get("id"))
            .and_then(Value::as_str)
            .filter(|s| non_empty(s))
        {
            return Some(s.to_string());
        }
    }
    None
}

/// The final assistant text across the observed events. Tolerant to the
/// plausible RPC shapes (message_update with a message envelope or a flat
/// text field; a `messages` transcript response) — the review runner falls
/// back to `get_messages` when this returns nothing.
pub fn final_assistant_text(events: &[Value]) -> Option<String> {
    let mut last: Option<String> = None;
    for v in events {
        let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
        if matches!(ty, "message_update" | "message" | "messages" | "get_messages") {
            let candidates: Vec<&Value> = match v.get("messages").and_then(Value::as_array) {
                Some(arr) => arr.iter().collect(),
                None => vec![v.get("message").unwrap_or(v)],
            };
            for m in candidates {
                if m.get("role").and_then(Value::as_str) != Some("assistant") {
                    continue;
                }
                if let Some(t) = message_text(m) {
                    last = Some(t);
                }
            }
        }
    }
    last
}

/// Assistant message content: a plain string or an array of text blocks.
fn message_text(m: &Value) -> Option<String> {
    match m.get("content") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Array(blocks)) => {
            let joined: String = blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("");
            (!joined.is_empty()).then_some(joined)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests;
