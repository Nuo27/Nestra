use super::*;

/// One serial test exercises the whole flow; NESTRA_HOME_DIR is process
/// (...)
/// -global, so we must not run multiple home-scoped skills tests in parallel.
#[test]
fn install_toggle_uninstall_flow() {
    // Serialized through the crate-wide `HOME_LOCK` so the session-reader
    // tests (and any future home-scoped test) don't stomp on
    // NESTRA_HOME_DIR mid-run.
    let _home_lock = crate::HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (home, _home_g) = temp_home();
    // SAFETY: confined to this single test (see comment above).
    std::env::set_var("NESTRA_HOME_DIR", &home);

    let conn = Connection::open_in_memory().unwrap();
    crate::db::migrate(&conn).unwrap();

    // build a source skill dir
    let src = home.join("src-skill");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("SKILL.md"),
        "---\nname: My Skill\ndescription: hi\n---\nbody",
    )
    .unwrap();

    // install → enabled for claude-code
    let meta = install(&conn, src.to_str().unwrap(), &["claude-code-cli".into()]).unwrap();
    assert_eq!(meta.name, "My Skill");
    assert!(meta.managed);
    assert!(meta.enabled_agents.contains(&"claude-code-cli".to_string()));
    assert!(ssot_root().unwrap().join(&meta.id).exists());
    assert!(home.join(".claude").join("skills").join(&meta.id).exists());

    // enable pi too, then disable claude
    let m2 = toggle(&conn, &meta.id, "pi-cli", true).unwrap();
    assert!(m2.enabled_agents.contains(&"pi-cli".to_string()));
    assert!(home.join(".agents").join("skills").join(&meta.id).exists());
    let m3 = toggle(&conn, &meta.id, "claude-code-cli", false).unwrap();
    assert!(!m3.enabled_agents.contains(&"claude-code-cli".to_string()));
    assert!(!home.join(".claude").join("skills").join(&meta.id).exists());

    // list includes the managed skill
    assert!(list(&conn).unwrap().iter().any(|s| s.id == meta.id && s.managed));

    // uninstall → removed from SSOT + CLI dirs; backup retained
    uninstall(&conn, &meta.id).unwrap();
    assert!(!ssot_root().unwrap().join(&meta.id).exists());
    assert!(list(&conn).unwrap().iter().all(|s| s.id != meta.id));
    assert!(backup_root().unwrap().exists());
}

/// `unmanage` drops the DB row + SSOT but LEAVES the agent-dir copies in
/// place (keeps working, becomes Importable) — the key difference from
/// `uninstall`, which also removes the agent copies.
#[test]
fn unmanage_keeps_agent_copies() {
    let _home_lock = crate::HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (home, _home_g) = temp_home();
    std::env::set_var("NESTRA_HOME_DIR", &home);

    let conn = Connection::open_in_memory().unwrap();
    crate::db::migrate(&conn).unwrap();

    let src = home.join("src-skill");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("SKILL.md"),
        "---\nname: My Skill\ndescription: hi\n---\nbody",
    )
    .unwrap();

    // install → enabled for claude-code + pi, so copies exist in both dirs
    let meta = install(&conn, src.to_str().unwrap(), &["claude-code-cli".into()]).unwrap();
    toggle(&conn, &meta.id, "pi-cli", true).unwrap();
    assert!(ssot_root().unwrap().join(&meta.id).exists());
    let claude_copy = home.join(".claude").join("skills").join(&meta.id);
    let pi_copy = home.join(".agents").join("skills").join(&meta.id);
    assert!(claude_copy.exists());
    assert!(pi_copy.exists());

    unmanage(&conn, &meta.id).unwrap();

    // No longer managed (DB row gone) and the SSOT copy is gone …
    assert!(list_managed(&conn)
        .unwrap()
        .iter()
        .all(|s| s.id != meta.id));
    assert!(!ssot_root().unwrap().join(&meta.id).exists());
    // …but the agent-dir copies are left in place (unlike uninstall)…
    assert!(claude_copy.exists(), "claude copy must survive unmanage");
    assert!(pi_copy.exists(), "pi copy must survive unmanage");
    // …so the skill re-surfaces in the merged view as unmanaged
    // (Importable), keyed by the same id.
    let entry = list(&conn)
        .unwrap()
        .into_iter()
        .find(|s| s.id == meta.id);
    assert!(entry.is_some(), "unmanaged skill should still appear in list");
    assert!(
        !entry.unwrap().managed,
        "should reappear as unmanaged, not managed"
    );
}

#[test]
fn system_dir_is_expanded_not_listed() {
    let (home, _home_g) = temp_home();
    let sys = home.join(".claude").join("skills").join(".system");
    std::fs::create_dir_all(&sys).unwrap();
    std::fs::write(sys.join(".system-skills.marker"), "x").unwrap();
    for sk in ["imagegen", "skill-creator"] {
        std::fs::create_dir_all(sys.join(sk)).unwrap();
    }
    std::fs::write(home.join(".claude").join("skills").join("separate"), "not a dir").unwrap();

    let entries = skill_entries(&home.join(".claude").join("skills"));
    let names: Vec<String> = entries.iter().map(|(n, _, _)| n.clone()).collect();
    assert!(!names.iter().any(|n| n == ".system"));
    assert!(names.contains(&"imagegen".to_string()));
    assert!(names.contains(&"skill-creator".to_string()));
    assert!(names.contains(&"separate".to_string()));
    assert!(entries.iter().all(|(n, _, b)| *b == (n == "imagegen" || n == "skill-creator")));
}

#[test]
fn claudekit_frontmatter_marks_claude_skills_builtin() {
    let (home, _home_g) = temp_home();
    let dir = home.join(".claude").join("skills");

    let bundled = dir.join("brand");
    std::fs::create_dir_all(&bundled).unwrap();
    std::fs::write(
        bundled.join("SKILL.md"),
        "---\nname: brand\ndescription: hi\nmetadata:\n  author: claudekit\n  version: \"1.0.0\"\n---\n",
    )
    .unwrap();
    assert!(is_claudekit(&bundled));
    assert!(is_builtin("claude-code-cli", &bundled, false));

    let user = dir.join("my-skill");
    std::fs::create_dir_all(&user).unwrap();
    std::fs::write(user.join("SKILL.md"), "---\nname: my-skill\ndescription: hi\n---\n").unwrap();
    assert!(!is_claudekit(&user));
    assert!(!is_builtin("claude-code-cli", &user, false));
    assert!(!is_builtin("pi-cli", &user, false));
}

/// OpenCode requires the copied frontmatter `name` to equal the dir (= id).
/// SSOT must keep the human name; only the OpenCode projection is normalized.
#[test]
fn opencode_sync_normalizes_frontmatter_name() {
    let _home_lock = crate::HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (home, _home_g) = temp_home();
    std::env::set_var("NESTRA_HOME_DIR", &home);

    let conn = Connection::open_in_memory().unwrap();
    crate::db::migrate(&conn).unwrap();

    let src = home.join("my-skill");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("SKILL.md"),
        "---\nname: My Skill\ndescription: hi\n---\nbody",
    )
    .unwrap();

    let meta = install(&conn, src.to_str().unwrap(), &["opencode-desktop".into(), "claude-code-cli".into()]).unwrap();
    assert_eq!(meta.id, "my-skill");

    let oc = std::fs::read_to_string(home.join(".config").join("opencode").join("skills").join("my-skill").join("SKILL.md")).unwrap();
    assert!(oc.lines().any(|l| l.trim() == "name: my-skill"), "opencode copy name must equal id");
    // SSOT + claude-code copies keep the human display name.
    let ssot = std::fs::read_to_string(ssot_root().unwrap().join("my-skill").join("SKILL.md")).unwrap();
    assert!(ssot.lines().any(|l| l.trim() == "name: My Skill"));
    let cc = std::fs::read_to_string(home.join(".claude").join("skills").join("my-skill").join("SKILL.md")).unwrap();
    assert!(cc.lines().any(|l| l.trim() == "name: My Skill"));
}

#[test]
fn opencode_in_skill_dirs() {
    let _home_lock = crate::HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (home, _home_g) = temp_home();
    std::env::set_var("NESTRA_HOME_DIR", &home);
    let ids: Vec<&str> = agent_skill_dirs().iter().map(|(id, _)| *id).collect();
    assert!(ids.contains(&"opencode-desktop"));
}

#[test]
fn rewrite_frontmatter_name_preserves_rest() {
    let src = "---\r\nname: Old Name\r\ndescription: keep me\r\nlicense: MIT\r\n---\r\nbody line\r\n";
    let out = rewrite_frontmatter_name(src, "new-id").unwrap();
    assert!(out.contains("name: new-id"));
    assert!(out.contains("description: keep me"));
    assert!(out.contains("license: MIT"));
    assert!(out.contains("body line"));
    assert!(out.contains("\r\n")); // CRLF preserved
    assert_eq!(rewrite_frontmatter_name("no frontmatter", "x"), None);
}

fn temp_home() -> (PathBuf, tempfile::TempDir) {
    let dir = tempfile::Builder::new()
        .prefix("")
        .tempdir()
        .expect("tempdir");
    (dir.path().to_path_buf(), dir)
}