//! Skills — Nestra-managed SSOT.
//!
//! Authoritative copies live at `~/.nestra/skills/<id>/`. A skill is "enabled"
//! for an agent by copying its SSOT dir into that agent's skill dir
//! (`~/.claude/skills/<id>`, `~/.agents/skills/<id>`); "disabled" by removing
//! it. Uninstall backs up to `~/.nestra/skills-backup/` then removes SSOT + all
//! agent copies. The DB `skill` table tracks managed skills + their enabled
//! agents; skills present in an agent dir but not in the DB are "unmanaged"
//! (importable).

use crate::db::home_dir;
use crate::error::{AppError, AppResult};
use rusqlite::Connection;
use serde::Serialize;
use std::path::{Path, PathBuf};
// `reveal_in_explorer` (agents module) opens the OS file manager at the
// skill's dir. Called directly from the skills_reveal command — no Tauri
// opener/fs plugin involved (those were dead weight and removed).

#[derive(Debug, Clone, Serialize)]
pub struct SkillMeta {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub source: String, // "claude-code-cli" | "pi-cli" | "local" | "imported"
    pub path: String,   // SSOT dir (managed) or agent-dir path (unmanaged)
    pub skill_type: String,
    pub managed: bool,
    pub enabled_agents: Vec<String>,
    /// Agent-bundled skill in a `.system`/`.agents` subdir. Read-only in the UI.
    pub builtin: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct UnmanagedSkill {
    pub agent_id: String,
    pub name: String,
    pub path: String,
    pub skill_type: String,
}

fn ssot_root() -> AppResult<PathBuf> {
    Ok(home_dir()?.join(".nestra").join("skills"))
}
fn backup_root() -> AppResult<PathBuf> {
    Ok(home_dir()?.join(".nestra").join("skills-backup"))
}

/// Agent id → that agent's skill directory. Derived from the AGENTS
/// registry: every agent with `skill_dir` set contributes an entry. Agents
/// without a declared skill directory are silently absent.
pub fn agent_skill_dirs() -> Vec<(&'static str, PathBuf)> {
    let Ok(h) = home_dir() else { return vec![] };
    crate::agents::agents()
        .iter()
        .filter_map(|a| a.skill_dir.map(|rel| (a.id, h.join(rel))))
        .collect()
}
fn agent_skill_dir(agent_id: &str) -> AppResult<PathBuf> {
    agent_skill_dirs()
        .into_iter()
        .find(|(id, _)| *id == agent_id)
        .map(|(_, d)| d)
        .ok_or_else(|| AppError::NotFound(format!("unknown agent '{agent_id}' for skills")))
}

/// Expand an agent skill dir into its skill paths. The third element is true
/// for skills the agent bundles itself: those live under a `.system`/`.agents`
/// subdir (the container + its marker file are skipped).
fn skill_entries(dir: &Path) -> Vec<(String, PathBuf, bool)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    for entry in rd.flatten() {
        let p = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name == ".system" && p.is_dir() {
            if let Ok(sub) = std::fs::read_dir(&p) {
                for s in sub.flatten() {
                    let sp = s.path();
                    if sp.is_dir() {
                        out.push((s.file_name().to_string_lossy().to_string(), sp, true));
                    }
                }
            }
        } else {
            out.push((name, p, false));
        }
    }
    out
}

fn slugify(s: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in s.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out.truncate(64);
    out
}

fn id_exists(conn: &Connection, id: &str) -> bool {
    conn.query_row("SELECT 1 FROM skill WHERE id=?1", [id], |_| Ok(()))
        .is_ok()
}
fn unique_id(conn: &Connection, base: &str) -> String {
    let id = if base.is_empty() { "skill".to_string() } else { base.to_string() };
    if !id_exists(conn, &id) {
        return id;
    }
    // Loop until a FREE id is found: the old `{id}-x` fallback was never
    // existence-checked, so a colliding fallback failed the INSERT (PK
    // conflict) and aborted the whole install.
    for i in 2..10_000 {
        let c = format!("{id}-{i}");
        if !id_exists(conn, &c) {
            return c;
        }
    }
    // Practically unreachable (10k collisions), but never return a
    // known-colliding id.
    let mut suffix = 1u64;
    loop {
        let c = format!("{id}-{suffix}-x");
        suffix += 1;
        if !id_exists(conn, &c) {
            return c;
        }
    }
}

/// Recursively copy `src` into `dst`, skipping symlinks. A symlink inside the
/// source tree is copied as its target *content* by `fs::copy` — a link to an
/// ancestor would recurse forever (ENAMETOOLONG) and a link to a sensitive dir
/// (`.ssh`, `.aws`) would diffuse its contents into the skill SSOT and every
/// agent's skill dir, crossing trust boundaries. Skipping is the safe default;
/// the probe errors out on a link instead.
fn copy_tree(src: &Path, dst: &Path) -> AppResult<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        // `symlink_metadata` reads the link itself, not its target — a link
        // (file or dir) is skipped wholesale rather than followed.
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(e) => {
                return Err(AppError::Internal(format!(
                    "skill copy: cannot stat {}: {e}",
                    from.display()
                )));
            }
        };
        if file_type.is_symlink() {
            tracing::warn!("skill copy: skipping symlink {}", from.display());
            continue;
        }
        if file_type.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

fn sync_to_agent(id: &str, agent_id: &str) -> AppResult<()> {
    let ssot = ssot_root()?.join(id);
    let dst = agent_skill_dir(agent_id)?.join(id);
    let _ = std::fs::remove_dir_all(&dst);
    copy_tree(&ssot, &dst)?;
    // OpenCode drops skills whose frontmatter `name` ≠ directory name. The dir
    // name is always `id` (slug), so rewrite the copied frontmatter `name` to
    // match. Best-effort: a malformed SKILL.md must never abort the sync.
    let normalize = crate::agents::agent_spec(agent_id)
        .map(|a| a.skill_name_matches_dir)
        .unwrap_or(false);
    if normalize {
        normalize_skill_name(&dst.join("SKILL.md"), id);
    }
    Ok(())
}

/// Rewrite the SKILL.md frontmatter `name:` value to `id`, leaving everything
/// else untouched. No-op (returns without writing) if the file can't be read or
/// has no frontmatter `name:` line.
fn normalize_skill_name(skill_md: &Path, id: &str) {
    let Ok(content) = std::fs::read_to_string(skill_md) else {
        return;
    };
    let Some(rewritten) = rewrite_frontmatter_name(&content, id) else {
        return;
    };
    if rewritten != content {
        let _ = std::fs::write(skill_md, rewritten);
    }
}

/// Replace the frontmatter `name:` line's value with `id`. Returns None when
/// there is no `--- ... ---` block or no `name:` line inside it.
fn rewrite_frontmatter_name(content: &str, id: &str) -> Option<String> {
    let eol = if content.contains("\r\n") { "\r\n" } else { "\n" };
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() || lines[0].trim() != "---" {
        return None;
    }
    let close = (1..lines.len()).find(|&i| lines[i].trim() == "---")?;
    let name_idx = (1..close)
        .find(|&i| lines[i].trim_start().strip_prefix("name:").is_some())?;
    let indent_len = lines[name_idx].len() - lines[name_idx].trim_start().len();
    let indent = &lines[name_idx][..indent_len];
    let new_line = format!("{indent}name: {id}");
    let mut out = String::with_capacity(content.len() + id.len());
    for (i, l) in lines.iter().enumerate() {
        if i == name_idx {
            out.push_str(&new_line);
        } else {
            out.push_str(l);
        }
        if i + 1 < lines.len() {
            out.push_str(eol);
        }
    }
    // Preserve a trailing terminator if the original had one.
    if content.ends_with('\n') {
        out.push_str(eol);
    }
    Some(out)
}
fn remove_from_agent(id: &str, agent_id: &str) -> AppResult<()> {
    let dst = agent_skill_dir(agent_id)?.join(id);
    if dst.exists() {
        std::fs::remove_dir_all(&dst)?;
    }
    Ok(())
}

/// Copy a source path (dir, or a single SKILL.md file) into `ssot/<id>/`.
fn ingest_source(src: &Path, ssot: &Path) -> AppResult<()> {
    if src.is_dir() {
        copy_tree(src, ssot)?;
    } else {
        std::fs::create_dir_all(ssot)?;
        std::fs::copy(src, ssot.join("SKILL.md"))?;
    }
    Ok(())
}

fn insert_row(
    conn: &Connection,
    id: &str,
    name: &str,
    description: Option<&str>,
    source: &str,
    enabled_agents: &[String],
    ssot_path: &Path,
) -> AppResult<()> {
    let now = chrono::Utc::now().timestamp_millis();
    conn.execute(
        "INSERT INTO skill (id,name,description,source,enabled_agents,ssot_path,created_at) \
         VALUES (?1,?2,?3,?4,?5,?6,?7)",
        rusqlite::params![
            id,
            name,
            description,
            source,
            serde_json::to_string(enabled_agents)?,
            ssot_path.to_string_lossy(),
            now,
        ],
    )?;
    Ok(())
}

fn row_to_meta(conn: &Connection, id: &str) -> AppResult<SkillMeta> {
    conn.query_row(
        "SELECT id,name,description,source,enabled_agents,ssot_path FROM skill WHERE id=?1",
        [id],
        |r| {
            let enabled_json: String = r.get(4)?;
            let enabled: Vec<String> = serde_json::from_str(&enabled_json).unwrap_or_default();
            Ok(SkillMeta {
                id: r.get(0)?,
                name: r.get(1)?,
                description: r.get(2)?,
                source: r.get(3)?,
                enabled_agents: enabled,
                path: r.get::<_, String>(5)?,
                skill_type: "directory".into(),
                managed: true,
                builtin: false,
            })
        },
    )
    .map_err(|e| AppError::Internal(format!("skill row '{id}': {e}")))
}

/// Install a skill from a local path, enable it for the given agents.
pub fn install(conn: &Connection, source: &str, agent_ids: &[String]) -> AppResult<SkillMeta> {
    let src = PathBuf::from(source);
    if !src.exists() {
        return Err(AppError::NotFound(format!("skill source not found: {source}")));
    }
    let base = src
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("skill");
    let id = unique_id(conn, &slugify(base));
    let root = ssot_root()?;
    std::fs::create_dir_all(&root)?;
    let ssot = root.join(&id);
    ingest_source(&src, &ssot)?;
    let (name, description) = extract_metadata(&ssot, &id);
    // Sync the agent dirs BEFORE the DB row commits: if a copy fails the
    // skill must not appear enabled in the DB while missing from the agent
    // (toggle/restore would then hard-fail on the missing dir).
    for agent in agent_ids {
        sync_to_agent(&id, agent)?;
    }
    insert_row(
        conn,
        &id,
        &name,
        description.as_deref(),
        "local",
        agent_ids,
        &ssot,
    )?;
    row_to_meta(conn, &id)
}

/// Uninstall: back up SSOT, remove from all enabled agent dirs, drop SSOT + row.
pub fn uninstall(conn: &Connection, id: &str) -> AppResult<()> {
    let meta = row_to_meta(conn, id)?;
    let ssot = PathBuf::from(ssot_root()?.join(id));
    if let Ok(broot) = backup_root() {
        let _ = std::fs::create_dir_all(&broot);
        let now = chrono::Utc::now().timestamp_millis();
        let bkp = broot.join(format!("{id}-{now}"));
        if ssot.exists() {
            copy_tree(&ssot, &bkp)?;
        }
        // Keep only the N most recent backups; prune older ones. Errors here
        // are best-effort — a stale backup left behind is harmless.
        prune_backups(&broot, 20);
    }
    for agent in &meta.enabled_agents {
        // Fail loudly instead of `let _ =`: leaving the agent dir behind
        // would orphan the skill there; deleting the row first would make
        // this error unrecoverable.
        remove_from_agent(id, agent)?;
    }
    if ssot.exists() {
        // Same reasoning: an unremovable SSOT dir must not be silently
        // orphaned while the DB row disappears.
        std::fs::remove_dir_all(&ssot)?;
    }
    conn.execute("DELETE FROM skill WHERE id=?1", [id])?;
    Ok(())
}

/// Delete the oldest skill backups beyond `keep`, sorted by directory name
/// (the `<id>-<timestamp>` suffix makes newer backups sort last).
fn prune_backups(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    paths.sort();
    while paths.len() > keep {
        let p = paths.remove(0);
        let _ = std::fs::remove_dir_all(&p);
    }
}

/// Enable/disable a skill for one agent (sync copy in / remove from its dir).
pub fn toggle(
    conn: &Connection,
    id: &str,
    agent_id: &str,
    enabled: bool,
) -> AppResult<SkillMeta> {
    let mut agents = row_to_meta(conn, id)?.enabled_agents;
    if enabled {
        if !agents.iter().any(|c| c == agent_id) {
            agents.push(agent_id.to_string());
        }
        sync_to_agent(id, agent_id)?;
    } else {
        agents.retain(|c| c != agent_id);
        remove_from_agent(id, agent_id)?;
    }
    conn.execute(
        "UPDATE skill SET enabled_agents=?1 WHERE id=?2",
        rusqlite::params![serde_json::to_string(&agents)?, id],
    )?;
    row_to_meta(conn, id)
}

/// Import an unmanaged agent-dir skill into SSOT, marked enabled for that agent.
pub fn import_one(conn: &Connection, path: &str, agent_id: &str) -> AppResult<SkillMeta> {
    let src = PathBuf::from(path);
    if !src.exists() {
        return Err(AppError::NotFound(format!("skill path not found: {path}")));
    }
    let base = src
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("skill");
    let id = unique_id(conn, &slugify(base));
    let root = ssot_root()?;
    std::fs::create_dir_all(&root)?;
    let ssot = root.join(&id);
    ingest_source(&src, &ssot)?;
    let (name, description) = extract_metadata(&ssot, &id);
    insert_row(
        conn,
        &id,
        &name,
        description.as_deref(),
        "imported",
        &[agent_id.to_string()],
        &ssot,
    )?;
    // already present in the source agent dir; ensure synced.
    let _ = sync_to_agent(&id, agent_id);
    row_to_meta(conn, &id)
}

/// Stop managing a skill: drop the DB row and the SSOT copy, but LEAVE the
/// skill files already synced into agent dirs in place. The skill keeps
/// working in those agents and re-surfaces as an Importable candidate — the
/// inverse of [`import_one`]. Contrast [`uninstall`], which also removes the
/// agent-dir copies (and backs up the SSOT). The Skills page's "restore"
/// button maps here. Idempotent: `DELETE` on a missing row is a no-op.
pub fn unmanage(conn: &Connection, id: &str) -> AppResult<()> {
    let ssot = ssot_root()?.join(id);
    if ssot.exists() {
        // Best-effort: the user's actual skill survives in the agent dirs
        // regardless (we don't touch those), so a lingering SSOT dir is
        // harmless and must not block dropping the row.
        let _ = std::fs::remove_dir_all(&ssot);
    }
    conn.execute("DELETE FROM skill WHERE id=?1", [id])?;
    Ok(())
}

/// Managed skills: DB rows only. No disk access — cheap enough to run under
/// the global DB lock.
pub fn list_managed(conn: &Connection) -> AppResult<Vec<SkillMeta>> {
    let mut stmt =
        conn.prepare("SELECT id,name,description,source,enabled_agents,ssot_path FROM skill")?;
    let rows = stmt.query_map([], |r| {
        let enabled_json: String = r.get(4)?;
        let enabled: Vec<String> = serde_json::from_str(&enabled_json).unwrap_or_default();
        Ok(SkillMeta {
            id: r.get(0)?,
            name: r.get(1)?,
            description: r.get(2)?,
            source: r.get(3)?,
            enabled_agents: enabled,
            path: r.get::<_, String>(5)?,
            skill_type: "directory".into(),
            managed: true,
            builtin: false,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        if let Ok(m) = row {
            out.push(m);
        }
    }
    Ok(out)
}

/// Managed skill ids, for excluding already-known skills from an import scan.
/// DB-only.
pub fn managed_ids(conn: &Connection) -> AppResult<std::collections::HashSet<String>> {
    let mut stmt = conn.prepare("SELECT id FROM skill")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut managed = std::collections::HashSet::new();
    for row in rows {
        if let Ok(id) = row {
            managed.insert(id);
        }
    }
    Ok(managed)
}

/// Skills present in agent dirs but not yet Nestra-managed (import candidates).
/// Pure disk walk — takes the set of already-managed ids, no `&Connection`.
pub fn import_scan_unmanaged(
    managed: &std::collections::HashSet<String>,
) -> AppResult<Vec<UnmanagedSkill>> {
    let mut out = Vec::new();
    for (agent_id, dir) in agent_skill_dirs() {
        if !dir.exists() {
            continue;
        }
        for (name, p, from_system) in skill_entries(&dir) {
            if managed.contains(&name) {
                continue;
            }
            if is_builtin(agent_id, &p, from_system) {
                continue; // agent-bundled — not importable
            }
            out.push(UnmanagedSkill {
                agent_id: agent_id.to_string(),
                name: name.clone(),
                path: p.to_string_lossy().to_string(),
                skill_type: if p.is_dir() { "directory" } else { "file" }.into(),
            });
        }
    }
    Ok(out)
}

/// Append unmanaged (agent-dir) skills to `managed` and sort. Pure disk walk —
/// takes no `&Connection`, so callers hold no DB lock during it.
pub fn merge_unmanaged(mut out: Vec<SkillMeta>) -> AppResult<Vec<SkillMeta>> {
    let mut seen: std::collections::HashSet<String> =
        out.iter().map(|m| m.id.clone()).collect();
    for (agent_id, dir) in agent_skill_dirs() {
        if !dir.exists() {
            continue;
        }
        for (name, p, from_system) in skill_entries(&dir) {
            if seen.contains(&name) {
                continue;
            }
            seen.insert(name.clone());
            let (mname, mdesc) = extract_metadata(&p, &name);
            out.push(SkillMeta {
                id: name.clone(),
                name: mname,
                description: mdesc,
                source: agent_id.to_string(),
                path: p.to_string_lossy().to_string(),
                skill_type: if p.is_dir() { "directory" } else { "file" }.into(),
                managed: false,
                enabled_agents: vec![agent_id.to_string()],
                builtin: is_builtin(agent_id, &p, from_system),
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// All skills: managed (from DB) + unmanaged (agent-dir skills not in DB).
/// Holds the given `conn` for the DB read only if the caller scopes it; the
/// disk walk itself needs no connection. Kept for callers that already hold
/// the lock (palette search) — the command path uses `list_managed` +
/// `merge_unmanaged` directly so it can drop the lock before the disk walk.
pub fn list(conn: &Connection) -> AppResult<Vec<SkillMeta>> {
    merge_unmanaged(list_managed(conn)?)
}

pub fn reveal(path: &str) -> AppResult<()> {
    let p = PathBuf::from(path);
    let target = if p.is_file() {
        p.parent().map(|x| x.to_path_buf()).unwrap_or(p)
    } else {
        p
    };
    crate::agents::reveal_in_explorer(&target).map_err(|e| AppError::Internal(format!("open failed: {e}")))?;
    Ok(())
}

fn extract_metadata(path: &PathBuf, fallback_name: &str) -> (String, Option<String>) {
    let skill_md = if path.is_dir() {
        path.join("SKILL.md")
    } else {
        path.clone()
    };
    if skill_md.exists() {
        if let Ok(content) = std::fs::read_to_string(&skill_md) {
            if let Some((name, desc, _)) = parse_frontmatter(&content) {
                return (name, Some(desc));
            }
        }
    }
    (fallback_name.to_string(), None)
}

/// Claude Code bundles skills shipped with `metadata.author: claudekit`. Used
/// to treat them as built-in/read-only like `.system/*` skills.
fn is_claudekit(path: &Path) -> bool {
    let skill_md = if path.is_dir() {
        path.join("SKILL.md")
    } else {
        path.to_path_buf()
    };
    let Ok(content) = std::fs::read_to_string(&skill_md) else { return false };
    parse_frontmatter(&content)
        .map(|(_, _, author)| author.as_deref() == Some("claudekit"))
        .unwrap_or(false)
}

/// Is a scanned skill one the CLI ships itself?
fn is_builtin(cli_id: &str, path: &Path, from_system: bool) -> bool {
    from_system || (cli_id == "claude-code-cli" && is_claudekit(path))
}

/// Parse `name:`, `description:`, and `metadata.author:` from YAML-ish
/// frontmatter, returning `(name, description, author)`.
fn parse_frontmatter(content: &str) -> Option<(String, String, Option<String>)> {
    // Normalize CRLF → LF FIRST: a Windows-edited SKILL.md (`---\r\nname: ...`)
    // would otherwise fail the `\n`-anchored strip below and lose its
    // frontmatter entirely.
    let content = content.replace("\r\n", "\n");
    let trimmed = content.trim_start();
    let body = trimmed.strip_prefix("---")?;
    let rest = body.strip_prefix('\n')?;
    let end = rest.find("\n---")?;
    let fm = &rest[..end];
    let mut name = None;
    let mut desc = None;
    let mut author = None;
    for line in fm.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("name:") {
            name = Some(v.trim().trim_matches('"').trim_matches('\'').to_string());
        } else if let Some(v) = line.strip_prefix("description:") {
            desc = Some(v.trim().trim_matches('"').trim_matches('\'').to_string());
        } else if let Some(v) = line.strip_prefix("author:") {
            author = Some(v.trim().trim_matches('"').trim_matches('\'').to_string());
        }
    }
    match (name, desc) {
        (Some(n), Some(d)) => Some((n, d, author)),
        (Some(n), None) => Some((n, String::new(), author)),
        _ => None,
    }
}

#[cfg(test)]
mod tests;