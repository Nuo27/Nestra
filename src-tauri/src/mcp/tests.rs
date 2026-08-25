use super::*;
use std::path::{Path, PathBuf};

fn temp_home() -> (PathBuf, tempfile::TempDir) {
    let dir = tempfile::Builder::new()
        .prefix("nestra-mcp-test-")
        .tempdir()
        .expect("tempdir");
    (dir.path().to_path_buf(), dir)
}

/// Mark `pi-mcp-adapter` as installed inside a fake home (the npm-dir
/// fallback of `adapter_installed_at`), so pi's `mcp_available` runtime
/// gate passes. Tests that exercise pi's provider must arrange the
/// precondition the gate checks — without it pi is silently gated out of
/// the provider registry and its file never syncs.
fn install_pi_adapter(home: &Path) {
    std::fs::create_dir_all(
        home.join(".pi").join("agent").join("npm").join("pi-mcp-adapter"),
    )
    .unwrap();
}

#[test]
fn semantic_parses_command_variants() {
    // Concept keys, not field names: "bin" is a command key, "arg" a args key.
    let v = serde_json::json!({
        "bin": "npx",
        "arg": "-y @modelcontextprotocol/server-filesystem /tmp",
        "environment": {"FOO": "bar"}
    });
    let t = from_native(&v).unwrap();
    assert_eq!(t.kind, McpKind::Stdio);
    assert_eq!(t.command.as_deref(), Some("npx"));
    assert!(t.args.contains(&"@modelcontextprotocol/server-filesystem".to_string()));
    assert_eq!(t.env.get("FOO").map(String::as_str), Some("bar"));
}

#[test]
fn semantic_url_detection() {
    let v = serde_json::json!({ "endpoint": "https://example.com/mcp" });
    let t = from_native(&v).unwrap();
    assert_eq!(t.kind, McpKind::Http);
    assert_eq!(t.url.as_deref(), Some("https://example.com/mcp"));
}

#[test]
fn claude_round_trip_sync_preserves_other_keys() {
    let _lock = crate::HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (home, _home_g) = temp_home();
    std::env::set_var("NESTRA_HOME_DIR", &home);
    let config = home.join(".claude.json");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(&config, r#"{"mcpServers":{},"theme":"dark"}"#).unwrap();

    let conn = Connection::open_in_memory().unwrap();
    crate::db::migrate(&conn).unwrap();

    let srv = McpServer {
        id: "filesystem".into(),
        name: "filesystem".into(),
        transport: McpTransport {
            kind: McpKind::Stdio,
            command: Some("npx".into()),
            args: vec!["-y".into(), "@mcps/filesystem".into()],
            env: Default::default(),
            url: None,
        },
        enabled_agents: vec!["claude-code-cli".into()],
        disabled_agents: vec![],
        managed: true,
        env_overrides: BTreeMap::new(),
    };
    save(&conn, &srv).unwrap();

    let written = std::fs::read_to_string(&config).unwrap();
    assert!(written.contains("\"theme\": \"dark\""), "must preserve unrelated key");
    assert!(written.contains("\"filesystem\""), "must add server");

    // toggle off removes from the file
    toggle(&conn, "filesystem", "claude-code-cli", false).unwrap();
    let written = std::fs::read_to_string(&config).unwrap();
    assert!(!written.contains("\"filesystem\""));
    assert!(written.contains("\"theme\": \"dark\""));
}

/// The headline fix: a server installed identically in two CLIs must import
/// as a *single* managed row enabled on both, not two rows.
#[test]
fn cross_cli_dedup_merges_into_one_row() {
    let _lock = crate::HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (home, _home_g) = temp_home();
    std::env::set_var("NESTRA_HOME_DIR", &home);
    install_pi_adapter(&home);

    // claude: codegraph as stdio
    let claude = home.join(".claude.json");
    std::fs::create_dir_all(claude.parent().unwrap()).unwrap();
    std::fs::write(
        &claude,
        r#"{"mcpServers":{"codegraph":{"command":"npx","args":["-y","codegraph"]}}}"#,
    )
    .unwrap();

    // Extra `type` field on one side the other lacks. Before the fix this
    // made transports unequal and they split.
    let pi_dir = home.join(".pi").join("agent");
    std::fs::create_dir_all(&pi_dir).unwrap();
    std::fs::write(
        pi_dir.join("mcp.json"),
        r#"{"mcpServers":{"codegraph":{"type":"stdio","command":"npx","args":["-y","codegraph"]}}}"#,
    )
    .unwrap();

    let conn = Connection::open_in_memory().unwrap();
    crate::db::migrate(&conn).unwrap();

    let imported = import_all(&conn).unwrap();
    // Exactly one managed row.
    assert_eq!(imported.len(), 1, "should merge into one row, got {imported:?}");
    let s = &imported[0];
    assert_eq!(s.id, "codegraph");
    assert_eq!(s.name, "codegraph");
    assert!(
        s.enabled_agents.iter().any(|c| c == "claude-code-cli"),
        "claude-code should be enabled: {:?}",
        s.enabled_agents
    );
    assert!(
        s.enabled_agents.iter().any(|c| c == "pi-cli"),
        "pi should be enabled: {:?}",
        s.enabled_agents
    );
}

/// Different transport *kind* (stdio vs http) under the same name is a
/// genuine conflict: keep two rows with a suffixed id.
#[test]
fn kind_conflict_splits_into_two_rows() {
    let _lock = crate::HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (home, _home_g) = temp_home();
    std::env::set_var("NESTRA_HOME_DIR", &home);
    install_pi_adapter(&home);

    let claude = home.join(".claude.json");
    std::fs::create_dir_all(claude.parent().unwrap()).unwrap();
    std::fs::write(
        &claude,
        r#"{"mcpServers":{"dual":{"command":"node","args":["s.js"]}}}"#,
    )
    .unwrap();

    let pi_dir = home.join(".pi").join("agent");
    std::fs::create_dir_all(&pi_dir).unwrap();
    std::fs::write(
        pi_dir.join("mcp.json"),
        r#"{"mcpServers":{"dual":{"type":"http","url":"https://x.test/mcp"}}}"#,
    )
    .unwrap();

    let conn = Connection::open_in_memory().unwrap();
    crate::db::migrate(&conn).unwrap();

    import_all(&conn).unwrap();
    let rows = list(&conn).unwrap();
    assert_eq!(rows.len(), 2, "stdio+http conflict should keep 2 rows");
    assert!(rows.iter().any(|r| r.id == "dual"), "canonical stdio row");
    assert!(
        rows.iter().any(|r| r.id == "dual-pi-cli"),
        "suffixed http row, got ids: {:?}",
        rows.iter().map(|r| &r.id).collect::<Vec<_>>()
    );
}

/// import_scan aggregates a cross-CLI server into one candidate.
#[test]
fn import_scan_aggregates_cross_cli() {
    let _lock = crate::HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (home, _home_g) = temp_home();
    std::env::set_var("NESTRA_HOME_DIR", &home);
    install_pi_adapter(&home);

    let claude = home.join(".claude.json");
    std::fs::create_dir_all(claude.parent().unwrap()).unwrap();
    std::fs::write(
        &claude,
        r#"{"mcpServers":{"shared":{"command":"npx","args":["x"]}}}"#,
    )
    .unwrap();
    let pi_dir = home.join(".pi").join("agent");
    std::fs::create_dir_all(&pi_dir).unwrap();
    std::fs::write(
        pi_dir.join("mcp.json"),
        r#"{"mcpServers":{"shared":{"command":"npx","args":["x"]}}}"#,
    )
    .unwrap();

    let conn = Connection::open_in_memory().unwrap();
    crate::db::migrate(&conn).unwrap();

    let cands = import_scan(&conn).unwrap();
    assert_eq!(cands.len(), 1, "one aggregated candidate");
    let c = &cands[0];
    assert_eq!(c.name, "shared");
    assert!(c.agent_ids.contains(&"claude-code-cli".to_string()));
    assert!(c.agent_ids.contains(&"pi-cli".to_string()));
    assert!(!c.transports_conflict, "same kind → no conflict");
}

/// save() canonicalizes the id from the name, ignoring a dirty caller id.
#[test]
fn save_canonicalizes_id_from_name() {
    let _lock = crate::HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (home, _home_g) = temp_home();
    std::env::set_var("NESTRA_HOME_DIR", &home);
    let claude = home.join(".claude.json");
    std::fs::create_dir_all(claude.parent().unwrap()).unwrap();
    std::fs::write(&claude, r#"{"mcpServers":{}}"#).unwrap();

    let conn = Connection::open_in_memory().unwrap();
    crate::db::migrate(&conn).unwrap();

    // Caller passes an empty id and a mixed-case name with trailing junk.
    let srv = McpServer {
        id: "".into(),
        name: "Foo Bar-".into(),
        transport: McpTransport {
            kind: McpKind::Stdio,
            command: Some("npx".into()),
            args: vec!["x".into()],
            env: BTreeMap::new(),
            url: None,
        },
        enabled_agents: vec!["claude-code-cli".into()],
        disabled_agents: vec![],
        managed: true,
        env_overrides: BTreeMap::new(),
    };
    let saved = save(&conn, &srv).unwrap();
    assert_eq!(saved.id, "foo-bar", "id should be slugified from name");
    assert_eq!(saved.name, "Foo Bar-");
}

/// Per-CLI env overrides layer on top of the base env when syncing: a CLI
/// with no override gets the base env; a CLI with an override gets the
/// union, with the override winning on key clash.
#[test]
fn per_cli_env_override_merges_on_sync() {
    let _lock = crate::HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (home, _home_g) = temp_home();
    std::env::set_var("NESTRA_HOME_DIR", &home);
    install_pi_adapter(&home);

    let claude = home.join(".claude.json");
    std::fs::create_dir_all(claude.parent().unwrap()).unwrap();
    std::fs::write(&claude, r#"{"mcpServers":{}}"#).unwrap();
    let pi_dir = home.join(".pi").join("agent");
    std::fs::create_dir_all(&pi_dir).unwrap();
    std::fs::write(pi_dir.join("mcp.json"), r#"{"mcpServers":{}}"#).unwrap();

    let conn = Connection::open_in_memory().unwrap();
    crate::db::migrate(&conn).unwrap();

    // Base env {A: "1", SHARED: "base"}; pi overrides {SHARED: "pi", B: "2"}.
    let mut base_env = BTreeMap::new();
    base_env.insert("A".into(), "1".into());
    base_env.insert("SHARED".into(), "base".into());
    let mut pi_ov = BTreeMap::new();
    pi_ov.insert("SHARED".into(), "pi".into());
    pi_ov.insert("B".into(), "2".into());
    let mut overrides = BTreeMap::new();
    overrides.insert("pi-cli".into(), pi_ov);

    let srv = McpServer {
        id: "envtest".into(),
        name: "envtest".into(),
        transport: McpTransport {
            kind: McpKind::Stdio,
            command: Some("npx".into()),
            args: vec!["x".into()],
            env: base_env,
            url: None,
        },
        enabled_agents: vec!["claude-code-cli".into(), "pi-cli".into()],
        disabled_agents: vec![],
        managed: true,
        env_overrides: overrides,
    };
    save(&conn, &srv).unwrap();

    // claude (no override): base env only.
    let claude_text = std::fs::read_to_string(&claude).unwrap();
    assert!(claude_text.contains(r#""A": "1""#), "claude gets base A");
    assert!(
        claude_text.contains(r#""SHARED": "base""#),
        "claude gets base SHARED"
    );
    assert!(!claude_text.contains(r#""B""#), "claude has no B");

    // pi (override): union, SHARED overridden, B added.
    let pi_text = std::fs::read_to_string(pi_dir.join("mcp.json")).unwrap();
    assert!(pi_text.contains(r#""A": "1""#), "pi gets base A");
    assert!(
        pi_text.contains(r#""SHARED": "pi""#),
        "pi override wins for SHARED"
    );
    assert!(pi_text.contains(r#""B": "2""#), "pi gets override B");
}

/// Renaming a server must drop the old row + its CLI file entries, not
/// leave an orphan id and a stale `Old Name` key in the providers' files.
#[test]
fn rename_drops_old_row_and_cli_entries() {
    let _lock = crate::HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (home, _home_g) = temp_home();
    std::env::set_var("NESTRA_HOME_DIR", &home);

    let claude = home.join(".claude.json");
    std::fs::create_dir_all(claude.parent().unwrap()).unwrap();
    std::fs::write(&claude, r#"{"mcpServers":{}}"#).unwrap();
    let oc_dir = home.join(".config").join("opencode");
    std::fs::create_dir_all(&oc_dir).unwrap();
    std::fs::write(oc_dir.join("opencode.json"), r#"{"mcp":{}}"#).unwrap();

    let conn = Connection::open_in_memory().unwrap();
    crate::db::migrate(&conn).unwrap();

    let srv = McpServer {
        id: "old-name".into(),
        name: "Old Name".into(),
        transport: McpTransport {
            kind: McpKind::Stdio,
            command: Some("npx".into()),
            args: vec!["x".into()],
            env: BTreeMap::new(),
            url: None,
        },
        enabled_agents: vec!["claude-code-cli".into(), "opencode-desktop".into()],
        disabled_agents: vec![],
        managed: true,
        env_overrides: BTreeMap::new(),
    };
    let saved = save(&conn, &srv).unwrap();
    assert_eq!(saved.id, "old-name");
    assert!(crate::db::get_mcp_server(&conn, "old-name").unwrap().is_some());

    // Rename → new id.
    let renamed = McpServer {
        id: "old-name".into(),
        name: "New Name".into(),
        ..srv
    };
    let saved2 = save(&conn, &renamed).unwrap();
    assert_eq!(saved2.id, "new-name");
    assert!(
        crate::db::get_mcp_server(&conn, "old-name").unwrap().is_none(),
        "old row must be deleted on rename"
    );
    assert!(crate::db::get_mcp_server(&conn, "new-name").unwrap().is_some());

    // Both CLI files now reference the new name only.
    let claude_text = std::fs::read_to_string(&claude).unwrap();
    assert!(
        !claude_text.contains("Old Name"),
        "claude file dropped old name: {claude_text}"
    );
    assert!(claude_text.contains("New Name"));
    let oc_text = std::fs::read_to_string(oc_dir.join("opencode.json")).unwrap();
    assert!(
        !oc_text.contains("Old Name"),
        "opencode file dropped old name: {oc_text}"
    );
    assert!(oc_text.contains("New Name"));
}

/// Editing a server's transport/env WITHOUT renaming must NOT delete the
/// row. Only true renames (canonical id shifts) trigger cleanup.
#[test]
fn edit_transport_does_not_delete_row() {
    let _lock = crate::HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (home, _home_g) = temp_home();
    std::env::set_var("NESTRA_HOME_DIR", &home);

    let claude = home.join(".claude.json");
    std::fs::create_dir_all(claude.parent().unwrap()).unwrap();
    std::fs::write(&claude, r#"{"mcpServers":{}}"#).unwrap();

    let conn = Connection::open_in_memory().unwrap();
    crate::db::migrate(&conn).unwrap();

    let srv = McpServer {
        id: "stable".into(),
        name: "stable".into(),
        transport: McpTransport {
            kind: McpKind::Stdio,
            command: Some("npx".into()),
            args: vec!["v1".into()],
            env: BTreeMap::new(),
            url: None,
        },
        enabled_agents: vec!["claude-code-cli".into()],
        disabled_agents: vec![],
        managed: true,
        env_overrides: BTreeMap::new(),
    };
    save(&conn, &srv).unwrap();
    let edited = McpServer {
        transport: McpTransport {
            kind: McpKind::Stdio,
            command: Some("npx".into()),
            args: vec!["v2".into()],
            env: BTreeMap::new(),
            url: None,
        },
        ..srv
    };
    let saved = save(&conn, &edited).unwrap();
    assert_eq!(saved.id, "stable");
    assert!(crate::db::get_mcp_server(&conn, "stable").unwrap().is_some());
}

/// Toggling a CLI on when its config file is missing must create the file
/// (empty object) and write the entry — otherwise the UI shows enabled
/// chips that disagree with reality.
#[test]
fn sync_creates_missing_config_file() {
    let _lock = crate::HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (home, _home_g) = temp_home();
    std::env::set_var("NESTRA_HOME_DIR", &home);

    // Deliberately do NOT create ~/.claude.json — sync must create it.
    let claude = home.join(".claude.json");
    assert!(!claude.exists());

    let conn = Connection::open_in_memory().unwrap();
    crate::db::migrate(&conn).unwrap();

    let srv = McpServer {
        id: "fresh".into(),
        name: "fresh".into(),
        transport: McpTransport {
            kind: McpKind::Stdio,
            command: Some("npx".into()),
            args: vec!["x".into()],
            env: BTreeMap::new(),
            url: None,
        },
        enabled_agents: vec!["claude-code-cli".into()],
        disabled_agents: vec![],
        managed: true,
        env_overrides: BTreeMap::new(),
    };
    save(&conn, &srv).unwrap();

    let written = std::fs::read_to_string(&claude).unwrap();
    assert!(
        written.contains("\"fresh\""),
        "missing config file must be created with the entry, got: {written}"
    );
}

/// The tri-state cycle on opencode: a server can be written with the
/// native enabled flag off (`disabled`), flipped to on, and dropped
/// entirely (`absent`) — and the two agent lists stay disjoint throughout.
#[test]
fn set_state_cycles_tri_states_on_opencode() {
    let _lock = crate::HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (home, _home_g) = temp_home();
    std::env::set_var("NESTRA_HOME_DIR", &home);

    let oc_dir = home.join(".config").join("opencode");
    std::fs::create_dir_all(&oc_dir).unwrap();
    std::fs::write(oc_dir.join("opencode.json"), r#"{"mcp":{}}"#).unwrap();

    let conn = Connection::open_in_memory().unwrap();
    crate::db::migrate(&conn).unwrap();

    let srv = McpServer {
        id: "playwright".into(),
        name: "playwright".into(),
        transport: McpTransport {
            kind: McpKind::Stdio,
            command: Some("npx".into()),
            args: vec!["@playwright/mcp@latest".into()],
            env: BTreeMap::new(),
            url: None,
        },
        enabled_agents: vec!["opencode-desktop".into()],
        disabled_agents: vec![],
        managed: true,
        env_overrides: BTreeMap::new(),
    };
    save(&conn, &srv).unwrap();

    // Disabled → entry stays written with the flag off.
    let s =
        set_state(&conn, "playwright", "opencode-desktop", AgentMcpState::Disabled).unwrap();
    assert!(s.enabled_agents.is_empty());
    assert_eq!(s.disabled_agents, vec!["opencode-desktop"]);
    let text = std::fs::read_to_string(oc_dir.join("opencode.json")).unwrap();
    assert!(text.contains(r#""enabled": false"#), "disabled entry, got: {text}");

    // Enabled → flag back on, disabled list cleared (invariant).
    let s =
        set_state(&conn, "playwright", "opencode-desktop", AgentMcpState::Enabled).unwrap();
    assert_eq!(s.enabled_agents, vec!["opencode-desktop"]);
    assert!(s.disabled_agents.is_empty());
    let text = std::fs::read_to_string(oc_dir.join("opencode.json")).unwrap();
    assert!(text.contains(r#""enabled": true"#), "enabled entry, got: {text}");

    // Absent → entry removed from the file entirely.
    let s =
        set_state(&conn, "playwright", "opencode-desktop", AgentMcpState::Absent).unwrap();
    assert!(s.enabled_agents.is_empty() && s.disabled_agents.is_empty());
    let text = std::fs::read_to_string(oc_dir.join("opencode.json")).unwrap();
    assert!(!text.contains("playwright"), "entry must be dropped, got: {text}");
}

/// A server enabled on claude and written-but-disabled on opencode syncs
/// the right flag into each file: claude's format has no enabled field
/// (plain entry), opencode's carries `enabled: false`.
#[test]
fn sync_writes_disabled_entry_with_enabled_false() {
    let _lock = crate::HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (home, _home_g) = temp_home();
    std::env::set_var("NESTRA_HOME_DIR", &home);

    let claude = home.join(".claude.json");
    std::fs::create_dir_all(claude.parent().unwrap()).unwrap();
    std::fs::write(&claude, r#"{"mcpServers":{}}"#).unwrap();
    let oc_dir = home.join(".config").join("opencode");
    std::fs::create_dir_all(&oc_dir).unwrap();
    std::fs::write(oc_dir.join("opencode.json"), r#"{"mcp":{}}"#).unwrap();

    let conn = Connection::open_in_memory().unwrap();
    crate::db::migrate(&conn).unwrap();

    let srv = McpServer {
        id: "playwright".into(),
        name: "playwright".into(),
        transport: McpTransport {
            kind: McpKind::Stdio,
            command: Some("npx".into()),
            args: vec!["@playwright/mcp@latest".into()],
            env: BTreeMap::new(),
            url: None,
        },
        enabled_agents: vec!["claude-code-cli".into()],
        disabled_agents: vec!["opencode-desktop".into()],
        managed: true,
        env_overrides: BTreeMap::new(),
    };
    save(&conn, &srv).unwrap();

    let oc_text = std::fs::read_to_string(oc_dir.join("opencode.json")).unwrap();
    assert!(
        oc_text.contains(r#""enabled": false"#),
        "opencode disabled flag, got: {oc_text}"
    );
    assert!(oc_text.contains("playwright"));

    let claude_text = std::fs::read_to_string(&claude).unwrap();
    assert!(claude_text.contains("playwright"));
    assert!(
        !claude_text.contains("enabled"),
        "claude format has no enabled field: {claude_text}"
    );
}

/// Importing an opencode entry with `enabled: false` must preserve the
/// state: the agent lands in `disabled_agents` and the import sync writes
/// the flag back as false — importing never force-enables a server the
/// file says is off.
#[test]
fn import_preserves_disabled_flag() {
    let _lock = crate::HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (home, _home_g) = temp_home();
    std::env::set_var("NESTRA_HOME_DIR", &home);

    let oc_dir = home.join(".config").join("opencode");
    std::fs::create_dir_all(&oc_dir).unwrap();
    std::fs::write(
        oc_dir.join("opencode.json"),
        r#"{"mcp":{"playwright":{"type":"local","command":["npx","@playwright/mcp@latest"],"enabled":false}}}"#,
    )
    .unwrap();

    let conn = Connection::open_in_memory().unwrap();
    crate::db::migrate(&conn).unwrap();

    // Scan surfaces the candidate with the flag.
    let cands = import_scan(&conn).unwrap();
    assert_eq!(cands.len(), 1);
    assert_eq!(cands[0].disabled_in, vec!["opencode-desktop"]);

    let s = import_one(&conn, "opencode-desktop", "playwright").unwrap();
    assert!(
        s.enabled_agents.is_empty(),
        "must not force-enable: {:?}",
        s.enabled_agents
    );
    assert_eq!(s.disabled_agents, vec!["opencode-desktop"]);

    // The entry in the file still says disabled after the import sync.
    let text = std::fs::read_to_string(oc_dir.join("opencode.json")).unwrap();
    assert!(text.contains(r#""enabled": false"#), "flag preserved, got: {text}");
}

/// `toggle` off means *absent*: it drops the entry from the file AND
/// clears any prior written-but-disabled state, keeping the two lists
/// disjoint.
#[test]
fn toggle_absent_clears_disabled_state() {
    let _lock = crate::HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (home, _home_g) = temp_home();
    std::env::set_var("NESTRA_HOME_DIR", &home);

    let oc_dir = home.join(".config").join("opencode");
    std::fs::create_dir_all(&oc_dir).unwrap();
    std::fs::write(oc_dir.join("opencode.json"), r#"{"mcp":{}}"#).unwrap();

    let conn = Connection::open_in_memory().unwrap();
    crate::db::migrate(&conn).unwrap();

    let srv = McpServer {
        id: "playwright".into(),
        name: "playwright".into(),
        transport: McpTransport {
            kind: McpKind::Stdio,
            command: Some("npx".into()),
            args: vec!["@playwright/mcp@latest".into()],
            env: BTreeMap::new(),
            url: None,
        },
        enabled_agents: vec![],
        disabled_agents: vec!["opencode-desktop".into()],
        managed: true,
        env_overrides: BTreeMap::new(),
    };
    save(&conn, &srv).unwrap();
    assert!(std::fs::read_to_string(oc_dir.join("opencode.json"))
        .unwrap()
        .contains("playwright"));

    let s = toggle(&conn, "playwright", "opencode-desktop", false).unwrap();
    assert!(s.enabled_agents.is_empty());
    assert!(s.disabled_agents.is_empty(), "toggle off must clear disabled state");
    let text = std::fs::read_to_string(oc_dir.join("opencode.json")).unwrap();
    assert!(!text.contains("playwright"), "entry must be dropped, got: {text}");
}

/// `unmanage` drops the DB row but LEAVES the entry in agent config files
/// (it keeps working, becomes Importable) — the inverse of import, and the
/// key difference from `delete`, which also strips config entries.
#[test]
fn unmanage_keeps_agent_config_entries() {
    let _lock = crate::HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (home, _home_g) = temp_home();
    std::env::set_var("NESTRA_HOME_DIR", &home);

    let conn = Connection::open_in_memory().unwrap();
    crate::db::migrate(&conn).unwrap();

    let srv = McpServer {
        id: "filesystem".into(),
        name: "filesystem".into(),
        transport: McpTransport {
            kind: McpKind::Stdio,
            command: Some("npx".into()),
            args: vec!["-y".into(), "@modelcontextprotocol/server-filesystem".into()],
            env: BTreeMap::new(),
            url: None,
        },
        enabled_agents: vec!["claude-code-cli".into()],
        disabled_agents: vec![],
        managed: true,
        env_overrides: BTreeMap::new(),
    };
    save(&conn, &srv).unwrap();
    let claude = home.join(".claude.json");
    assert!(
        std::fs::read_to_string(&claude)
            .unwrap()
            .contains("filesystem"),
        "entry should be written on save"
    );

    unmanage(&conn, "filesystem").unwrap();

    // DB row is gone.
    assert!(
        crate::db::get_mcp_server(&conn, "filesystem")
            .unwrap()
            .is_none(),
        "unmanage must drop the DB row"
    );
    // …but the agent config entry is left in place (unlike delete).
    let after = std::fs::read_to_string(&claude).unwrap();
    assert!(
        after.contains("filesystem"),
        "unmanage must KEEP the config entry, got: {after}"
    );
}

/// Registry-churn guard (the "unknown agent" fix): legacy ids from before
/// agent renames (e.g. `claude-code` → `claude-code-cli`) must be dropped
/// at the read boundary, and `sync_all` must persist the pruned set.
#[test]
fn row_to_server_prunes_unknown_agent_ids_and_sync_persists() {
    // sync_all touches agent config files under the (overridden) home —
    // serialized through the crate-wide HOME_LOCK like the skills tests.
    let _home_lock = crate::HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let home = tempfile::Builder::new().prefix("").tempdir().unwrap();
    // SAFETY: confined to this serialized test (HOME_LOCK held).
    std::env::set_var("NESTRA_HOME_DIR", home.path());

    let conn = Connection::open_in_memory().unwrap();
    crate::schema::build_v1(&conn).unwrap();
    conn.execute(
        "INSERT INTO mcp_server (id, name, transport_json, enabled_agents,
                                     disabled_agents, created_at)
             VALUES ('s1','codegraph','{}',?1,'[]',0)",
        rusqlite::params![
            serde_json::to_string(&vec!["claude-code".to_string(), "pi".to_string(),
                                        "claude-code-cli".to_string()])
                .unwrap()
        ],
    )
    .unwrap();

    // Read boundary: legacy ids never surface.
    let row = crate::db::get_mcp_server(&conn, "s1").unwrap().unwrap();
    let server = row_to_server(&conn, row).unwrap();
    assert_eq!(server.enabled_agents, vec!["claude-code-cli".to_string()]);

    // sync_all persists the pruned set (the DB heals on the next sync).
    sync_all(&conn).unwrap();
    let row = crate::db::get_mcp_server(&conn, "s1").unwrap().unwrap();
    assert_eq!(row.enabled_agents, vec!["claude-code-cli".to_string()]);
}