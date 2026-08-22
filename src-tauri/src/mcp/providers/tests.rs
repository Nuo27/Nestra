use super::*;
use std::path::Path;

#[test]
fn windows_wrap_whitelisted_command() {
    let path = Path::new(r"C:\Users\me\.claude.json");
    let (cmd, args) = wrap_command_for_windows(
        &Some("npx".into()),
        &["-y".into(), "@mcps/fs".into()],
        path,
    );
    #[cfg(target_os = "windows")]
    {
        assert_eq!(cmd.as_deref(), Some("cmd"));
        // Separate tokens (not a pre-quoted `/c` line): the spawner escapes
        // each arg, and a bare command keeps `%~dp0` working in `.cmd` shims.
        assert_eq!(args, vec!["/d", "/c", "npx", "-y", "@mcps/fs"]);
    }
    #[cfg(not(target_os = "windows"))]
    {
        assert_eq!(cmd.as_deref(), Some("npx"));
        assert_eq!(args, vec!["-y", "@mcps/fs"]);
    }
}

#[test]
fn windows_wrap_quotes_metachars_against_injection() {
    // A hostile arg stays one untransformed token — Nestra never splices it
    // into a command line itself. The spawner (Rust's `Command` / Node's
    // `spawn`) quotes any token containing spaces at run time, so
    // `& | < > ^` end up inside quotes and cmd reads them literally.
    let path = Path::new(r"C:\Users\me\.claude.json");
    let (cmd, args) = wrap_command_for_windows(
        &Some("npx".into()),
        &["-y".into(), "pkg & echo pwned".into()],
        path,
    );
    #[cfg(target_os = "windows")]
    {
        assert_eq!(cmd.as_deref(), Some("cmd"));
        assert_eq!(args, vec!["/d", "/c", "npx", "-y", "pkg & echo pwned"]);
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (cmd, args);
    }
}

#[test]
fn windows_wrap_unwraps_for_wsl_path() {
    let path = Path::new(r"\\wsl$\Ubuntu\home\me\.claude.json");
    let (cmd, args) =
        wrap_command_for_windows(&Some("npx".into()), &["x".into()], path);
    // WSL configs target Linux: never wrap, on any host OS.
    assert_eq!(cmd.as_deref(), Some("npx"));
    assert_eq!(args, vec!["x"]);
}

/// A non-whitelisted command that resolves to a real `.exe` (here `python`,
/// installed as `python.exe`) is left untouched on every platform — only
/// `.cmd`/`.bat` shims need `cmd /c`.
#[test]
fn windows_wrap_skips_non_whitelisted() {
    let path = Path::new(r"C:\Users\me\.claude.json");
    let (cmd, args) =
        wrap_command_for_windows(&Some("python".into()), &["srv.py".into()], path);
    assert_eq!(cmd.as_deref(), Some("python"));
    assert_eq!(args, vec!["srv.py"]);
}

#[test]
fn windows_wrap_handles_path_prefixed_command() {
    let path = Path::new(r"C:\Users\me\.claude.json");
    // A full path with a .cmd suffix wraps via the explicit-extension rule.
    let (cmd, args) = wrap_command_for_windows(
        &Some(r"C:\nodejs\npx.cmd".into()),
        &["-y".into(), "x".into()],
        path,
    );
    #[cfg(target_os = "windows")]
    {
        assert_eq!(cmd.as_deref(), Some("cmd"));
        assert_eq!(args, vec!["/d", "/c", r"C:\nodejs\npx.cmd", "-y", "x"]);
    }
    #[cfg(not(target_os = "windows"))]
    {
        assert_eq!(cmd.as_deref(), Some(r"C:\nodejs\npx.cmd"));
    }
}

/// A `.cmd` shim that isn't in the whitelist (e.g. `codegraph`, whose
/// installer ships only `codegraph.cmd`) must still wrap on Windows —
/// CreateProcess can't run a `.cmd` directly. Deterministic: the explicit
/// `.cmd` extension is enough, no PATH lookup involved.
#[test]
fn windows_wrap_wraps_non_whitelisted_cmd_shim() {
    let path = Path::new(r"C:\Users\me\.claude.json");
    let (cmd, args) = wrap_command_for_windows(
        &Some(r"C:\tools\mycustom.cmd".into()),
        &["serve".into(), "--mcp".into()],
        path,
    );
    #[cfg(target_os = "windows")]
    {
        assert_eq!(cmd.as_deref(), Some("cmd"));
        assert_eq!(
            args,
            vec!["/d", "/c", r"C:\tools\mycustom.cmd", "serve", "--mcp"]
        );
    }
    #[cfg(not(target_os = "windows"))]
    {
        assert_eq!(cmd.as_deref(), Some(r"C:\tools\mycustom.cmd"));
    }
}

/// A *bare* command name that resolves via PATH+PATHEXT to a `.cmd` shim
/// must wrap even though it isn't whitelisted — this is the `codegraph`
/// code path (its installer puts only `codegraph.cmd` on PATH). Hermetic:
/// stages a fake shim in a temp dir and prepends it to PATH for the call,
/// restoring PATH (and serializing via `HOME_LOCK`) so no other test is
/// affected. Windows-only behavior; on other platforms it's a no-op.
#[test]
fn windows_wrap_wraps_bare_name_resolving_to_cmd_shim() {
    if cfg!(not(target_os = "windows")) {
        return;
    }
    // Serialize env mutation against other env-touching tests.
    let _lock = crate::HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let tmp = tempfile::tempdir().expect("tempdir");
    // A .cmd shim, like codegraph's installer ships — bare name on PATH.
    std::fs::write(tmp.path().join("nestra-shim-fixture.cmd"), "@echo hi\r\n")
        .expect("write shim");
    // Put the temp dir FIRST on PATH so `which` resolves our shim, keeping
    // the existing PATH appended so other resolutions still work.
    let mut new_path = tmp.path().to_string_lossy().into_owned();
    if let Some(existing) = std::env::var_os("PATH") {
        new_path.push(';');
        new_path.push_str(&existing.to_string_lossy());
    }
    let _guard = EnvGuard::set_path(new_path);

    let cfg = Path::new(r"C:\Users\me\.claude.json");
    let (cmd, args) = wrap_command_for_windows(
        &Some("nestra-shim-fixture".into()),
        &["serve".into(), "--mcp".into()],
        cfg,
    );
    assert_eq!(
        cmd.as_deref(),
        Some("cmd"),
        "bare name resolving to .cmd must wrap"
    );
    // Separate tokens: /d /c <bare command> <args…>.
    assert_eq!(args.len(), 5);
    assert_eq!(args[0], "/d");
    assert_eq!(args[1], "/c");
    assert_eq!(args[2], "nestra-shim-fixture");
    assert_eq!(args[3], "serve");
    assert_eq!(args[4], "--mcp");
}

/// Save/restore the `PATH` env var so a test can mutate it without leaking
/// to sibling tests (restores even on panic via `Drop`).
struct EnvGuard {
    original: Option<std::ffi::OsString>,
}
impl EnvGuard {
    fn set_path(new: String) -> Self {
        let original = std::env::var_os("PATH");
        std::env::set_var("PATH", new);
        EnvGuard { original }
    }
}
impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.original {
            Some(v) => std::env::set_var("PATH", v),
            None => std::env::remove_var("PATH"),
        }
    }
}

/// Live check: probe the user's real `codegraph` MCP — a bare command that
/// exists only as `codegraph.cmd` (a `.cmd`-only shim, not in the
/// whitelist). Run with
/// `cargo test --lib live_probe_codegraph -- --ignored --nocapture`.
/// Asserts (a) the sync path now wraps it as `cmd /d /s /c "codegraph …"`
/// and (b) the shared probe (the UI's Test button) launches it and gets an
/// MCP `initialize` response — both were `spawn failed: program not
/// found` before the wrapper learned to resolve `.cmd` shims.
#[test]
#[ignore]
fn live_probe_codegraph() {
    let t = crate::mcp::McpTransport {
        kind: crate::mcp::McpKind::Stdio,
        command: Some("codegraph".into()),
        args: vec!["serve".into(), "--mcp".into()],
        env: Default::default(),
        url: None,
    };
    // What the sync path would write into ~/.claude.json etc.
    let cfg = Path::new(r"C:\Users\me\.claude.json");
    let (cmd, args) = wrap_command_for_windows(&t.command.clone(), &t.args, cfg);
    eprintln!("codegraph wrapped -> cmd={cmd:?} args={args:?}");
    #[cfg(target_os = "windows")]
    assert_eq!(
        cmd.as_deref(),
        Some("cmd"),
        "codegraph (.cmd-only shim) must wrap on Windows"
    );

    // The real probe — same code path the MCP page's Test button uses.
    let r = crate::mcp::probe_transport(&t).expect("probe_transport ran");
    eprintln!("codegraph probe result: {r:?}");
    assert!(r.ok, "codegraph should probe ok: {:?}", r.reason);
}

/// Diagnostic: spawn codegraph via the wrapped `cmd /d /s /c` form WITH
/// stderr captured (the real probe nulls it) to see whether it receives
/// correct args / forwards stdin. Run with
/// `cargo test --lib live_diag_codegraph_wrapped -- --ignored --nocapture`.
#[test]
#[ignore]
fn live_diag_codegraph_wrapped() {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::Duration;
    if cfg!(not(target_os = "windows")) {
        return;
    }
    let (cmd, args) = wrap_command_for_windows(
        &Some("codegraph".into()),
        &["serve".into(), "--mcp".into()],
        Path::new(r"C:\Users\me\.claude.json"),
    );
    let cmd = cmd.expect("wrapped");
    eprintln!(">>> spawn: {cmd}  args={args:?}");
    let mut child = Command::new(&cmd);
    for a in &args {
        child.arg(a);
    }
    let mut child = child
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"nestra","version":"0.1"}}}"#;
    writeln!(stdin, "{init}").unwrap();
    stdin.flush().unwrap();
    // Collect stdout on a thread (read-to-EOF unblocks when we close stdin).
    let out = thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        s
    });
    // Give codegraph time to answer `initialize`, then close stdin (EOF) so
    // it exits and the stdout reader unblocks.
    thread::sleep(Duration::from_secs(3));
    drop(stdin);
    let out = out.join().unwrap();
    let mut err = String::new();
    let _ = child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err);
    let _ = child.kill();
    eprintln!(">>> STDOUT ({} bytes): {:?}", out.len(), &out[..out.len().min(600)]);
    eprintln!(">>> STDERR ({} bytes): {:?}", err.len(), &err[..err.len().min(600)]);
}

/// Ignored live-check: feeds the user's actual broken `opencode.json`
/// bytes into `OpenCode::apply` and prints the repaired JSON. Invoke
/// with `cargo test --lib live_repair_user_opencode_json -- --ignored
/// --nocapture` to reproduce the fix end-to-end.
#[test]
#[ignore]
fn live_repair_user_opencode_json() {
    let raw = include_str!("../../../fixtures/user_broken_opencode.json");
    let out = OpenCode.apply(raw, &k(), &[]).unwrap();
    eprintln!("--- repaired ---\n{}\n--- end ---", out);
}

fn k() -> std::collections::BTreeMap<String, Value> {
    std::collections::BTreeMap::new()
}

/// Stdio entries written by `to_native` carry the OpenCode-native array
/// command (program + args merged), `environment` (not `env`), and
/// `enabled: true` so the schema validator accepts them.
#[test]
fn opencode_to_native_stdio_uses_array_command_and_environment() {
    let mut env = std::collections::BTreeMap::new();
    env.insert("FOO".into(), "bar".into());
    let s = crate::mcp::McpTransport {
        kind: crate::mcp::McpKind::Stdio,
        command: Some("codegraph".into()),
        args: vec!["serve".into(), "--mcp".into()],
        env,
        url: None,
    };
    let v = OpenCode.to_native(&s, true);
    assert_eq!(v["type"], "local");
    assert_eq!(v["enabled"], true);
    assert_eq!(
        v["command"],
        serde_json::json!(["codegraph", "serve", "--mcp"]),
        "command must be a single array merging program + args"
    );
    assert_eq!(v["environment"]["FOO"], "bar", "env key must be renamed");
    assert!(
        v.get("env").is_none(),
        "opencode rejects the legacy `env` key"
    );
    assert!(v.get("args").is_none(), "opencode rejects the legacy `args` key");
}

/// Round-trip: an OpenCode-format native entry survives `from_native`
/// (which splits array `command` back into `command` + `args`) so an
/// imported server edits correctly.
#[test]
fn opencode_to_native_from_native_round_trip() {
    let s = crate::mcp::McpTransport {
        kind: crate::mcp::McpKind::Stdio,
        command: Some("npx".into()),
        args: vec!["-y".into(), "@mcps/filesystem".into()],
        env: Default::default(),
        url: None,
    };
    let v = OpenCode.to_native(&s, true);
    let t = crate::mcp::from_native(&v).unwrap();
    assert_eq!(t.kind, crate::mcp::McpKind::Stdio);
    assert_eq!(t.command.as_deref(), Some("npx"));
    assert_eq!(t.args, vec!["-y".to_string(), "@mcps/filesystem".to_string()]);
}

/// Servers written by older Nestra land nested at `mcp.local.<name>` /
/// `mcp.remote.<name>` — OpenCode rejects that shape (`mcp.local.enabled
/// Missing key`). The next `apply` repairs the nesting: entries lift back
/// to `mcp.<name>` (keeping their `type`), `enabled` is filled, and the
/// empty `local`/`remote` group keys disappear.
#[test]
fn opencode_apply_repairs_nested_local_remote_slots() {
    let raw = r#"{
            "mcp": {
                "local": {
                    "codegraph": {
                        "args": ["serve","--mcp"],
                        "command": "codegraph",
                        "env": { "FOO": "bar" },
                        "type": "local"
                    },
                    "unityMCP": {
                        "type": "local",
                        "command": ["uvx", "mcp-for-unity"]
                    }
                },
                "remote": {
                    "remote-srv": { "type": "remote", "url": "https://x.example/mcp" }
                }
            }
        }"#;
    let out = OpenCode.apply(raw, &k(), &[]).unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    // Nested slots gone; entries lifted flat, keyed by name.
    assert!(v["mcp"].get("local").is_none(), "local slot removed");
    assert!(v["mcp"].get("remote").is_none(), "remote slot removed");
    let cg = &v["mcp"]["codegraph"];
    assert_eq!(cg["type"], "local");
    assert_eq!(cg["enabled"], true, "missing enabled → filled");
    // Split command+args merged into opencode's array form; env renamed.
    assert_eq!(
        cg["command"],
        json!(["codegraph", "serve", "--mcp"]),
        "command+args merged into array"
    );
    assert!(cg.get("args").is_none(), "legacy args key dropped");
    assert_eq!(cg["environment"]["FOO"], "bar", "env renamed to environment");
    assert!(cg.get("env").is_none(), "legacy env key dropped");
    let um = &v["mcp"]["unityMCP"];
    assert_eq!(um["type"], "local");
    assert_eq!(um["enabled"], true);
    let rs = &v["mcp"]["remote-srv"];
    assert_eq!(rs["type"], "remote");
    assert_eq!(rs["url"], "https://x.example/mcp");
    // Already-flat entries are left alone on a second pass.
    let out2 = OpenCode.apply(&out, &k(), &[]).unwrap();
    let v2: Value = serde_json::from_str(&out2).unwrap();
    assert_eq!(v2["mcp"]["codegraph"]["enabled"], true, "idempotent");
    assert!(v2["mcp"].get("local").is_none());
    assert!(v2["mcp"].get("remote").is_none());
}

/// Newly enabled servers write directly at `mcp.<name>` (not nested),
/// replacing any stale copy of the same name.
#[test]
fn opencode_apply_writes_servers_flat() {
    let mut enabled = BTreeMap::new();
    // A stdio server and a remote server in one pass.
    enabled.insert(
        "codegraph".into(),
        json!({ "type": "local", "command": ["codegraph", "serve", "--mcp"], "enabled": true }),
    );
    enabled.insert(
        "remote-srv".into(),
        json!({ "type": "remote", "url": "https://x.example/mcp", "enabled": true }),
    );
    let out = OpenCode.apply(r#"{"mcp":{}}"#, &enabled, &[]).unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    assert!(v["mcp"].get("local").is_none());
    assert!(v["mcp"].get("remote").is_none());
    assert_eq!(v["mcp"]["codegraph"]["command"][0], "codegraph");
    assert_eq!(v["mcp"]["remote-srv"]["url"], "https://x.example/mcp");
}

/// Apply only touches server entries; unrelated nested config under
/// `mcp.<x>` is preserved untouched.
#[test]
fn opencode_apply_preserves_unrelated_top_level_keys() {
    let raw = r#"{
            "mcp": {
                "futureStuff": { "kind": "weird", "x": 1 }
            }
        }"#;
    let out = OpenCode.apply(raw, &k(), &[]).unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    assert!(v["mcp"].get("futureStuff").is_some(), "untouched");
}

/// Live-shape regression: take the user's actual broken config (nested
/// `mcp.local.*`, produced by an earlier Nestra) and confirm the next
/// `sync_agent`-equivalent pass repairs it to a valid flat file while
/// round-tripping untouched non-MCP fields.
#[test]
fn opencode_apply_repairs_real_user_broken_config() {
    let raw = r#"{
            "$schema": "https://opencode.ai/config.json",
            "model": "nestra-minimax/MiniMax-M3",
            "provider": { "nestra-minimax": { "name": "minimax (via Nestra)" } },
            "mcp": {
                "local": {
                    "codegraph": {
                        "args": ["serve","--mcp"],
                        "command": "codegraph",
                        "enabled": true,
                        "type": "local"
                    },
                    "unityMCP": {
                        "command": ["uvx","mcp-for-unity"],
                        "enabled": true,
                        "type": "local"
                    }
                }
            }
        }"#;
    let out = OpenCode.apply(raw, &k(), &[]).unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    assert!(v["mcp"].get("local").is_none());
    assert_eq!(v["mcp"]["codegraph"]["type"], "local");
    assert_eq!(v["mcp"]["unityMCP"]["type"], "local");
    // Untouched non-MCP fields round-trip.
    assert_eq!(v["model"], "nestra-minimax/MiniMax-M3");
    assert!(v["provider"]["nestra-minimax"].is_object());
}

/// An unparseable config must refuse the rewrite — never fall back to an
/// empty object (which would erase the user's whole file on the next
/// sync), and never write garbage back over a truncated file.
#[test]
fn apply_refuses_unparseable_config() {
    let raw = "{ not json \n \"mcpServers\": {";
    let err = ClaudeCode
        .apply(raw, &k(), &[] as &[String])
        .unwrap_err();
    assert!(matches!(err, AppError::Validation(_)));
    let err2 = OpenCode.apply(raw, &k(), &[] as &[String]).unwrap_err();
    assert!(matches!(err2, AppError::Validation(_)));
}

/// A non-object root (e.g. a JSON array or string) is also refused — the
/// config shape is wrong, so preserving the file beats rewriting it.
#[test]
fn apply_refuses_non_object_root() {
    let raw = r#"["not", "an", "object"]"#;
    assert!(ClaudeCode.apply(raw, &k(), &[] as &[String]).is_err());
    assert!(OpenCode.apply(raw, &k(), &[] as &[String]).is_err());
}

/// zcode round-trip: a native `mcp.servers` entry decodes through
/// `from_native`, and `to_native` writes the strict-schema shape (typed
/// stdio/http, `enabled`, `timeoutMs`) at the right path — while `apply`
/// preserves the hooks/plugins keys that share the file.
#[test]
fn zcode_native_round_trip_and_apply() {
    let s = crate::mcp::McpTransport {
        kind: crate::mcp::McpKind::Stdio,
        command: Some("npx".into()),
        args: vec!["-y".into(), "@mcps/fs".into()],
        env: Default::default(),
        url: None,
    };
    let v = ZCode.to_native(&s, true);
    assert_eq!(v["type"], "stdio");
    assert_eq!(v["enabled"], true);
    assert_eq!(v["timeoutMs"], 30_000);
    let back = crate::mcp::from_native(&v).unwrap();
    assert_eq!(back.kind, crate::mcp::McpKind::Stdio);
    assert_eq!(back.command.as_deref(), Some("npx"));
    assert_eq!(back.args, vec!["-y".to_string(), "@mcps/fs".to_string()]);

    // disabled state stays written but off
    let off = ZCode.to_native(&s, false);
    assert_eq!(off["enabled"], false);

    // http shape
    let http = crate::mcp::McpTransport {
        kind: crate::mcp::McpKind::Http,
        command: None,
        args: vec![],
        env: Default::default(),
        url: Some("https://x.test/mcp".into()),
    };
    let hv = ZCode.to_native(&http, true);
    assert_eq!(hv["type"], "http");
    assert_eq!(hv["url"], "https://x.test/mcp");

    // apply writes under mcp.servers and preserves sibling keys
    let mut enabled = BTreeMap::new();
    enabled.insert("fs".into(), v);
    let raw = r#"{ "hooks": { "enabled": true }, "mcp": { "servers": { "old": { "type": "stdio", "command": "x", "enabled": true } } } }"#;
    let out = ZCode.apply(raw, &enabled, &["old".to_string()]).unwrap();
    let parsed: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(parsed["hooks"]["enabled"], true, "sibling keys preserved");
    assert!(parsed["mcp"]["servers"].get("old").is_none(), "disabled name dropped");
    assert_eq!(parsed["mcp"]["servers"]["fs"]["command"], "npx");

    // read_raw finds the nested map
    let entries = ZCode.read_raw(&out);
    assert_eq!(entries[0].0, "fs");
}