use crate::error::{AppError, AppResult};
use rusqlite::Connection;
use std::path::PathBuf;

pub fn data_dir() -> AppResult<PathBuf> {
    let base = dirs::data_local_dir().ok_or_else(|| AppError::Internal("no local data dir".into()))?;
    Ok(base.join("dev.nestra.app"))
}

pub fn log_dir() -> AppResult<PathBuf> {
    Ok(data_dir()?.join("logs"))
}

pub fn home_dir() -> AppResult<PathBuf> {
    // Honor NESTRA_HOME_DIR so the app can be tested against a fake home
    // without touching the real ~/.claude / ~/.pi / etc.
    if let Ok(p) = std::env::var("NESTRA_HOME_DIR") {
        if !p.is_empty() {
            return Ok(PathBuf::from(p));
        }
    }
    dirs::home_dir().ok_or_else(|| AppError::Internal("no home dir".into()))
}

/// Platform-specific directories used by Desktop-app detection. Each field
/// is `None` when the platform does not provide a corresponding location.
/// Tests can override individual fields via the `NESTRA_*_DIR` env vars.
#[derive(Debug, Clone)]
pub struct PlatformDirs {
    pub home: PathBuf,
    pub app_data: Option<PathBuf>,
    pub local_app_data: Option<PathBuf>,
}

pub fn platform_dirs() -> AppResult<PlatformDirs> {
    let home = home_dir()?;
    let local_app_data = std::env::var("NESTRA_LOCAL_APPDATA_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(dirs::data_local_dir);
    let app_data = std::env::var("NESTRA_APPDATA_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(dirs::config_dir);
    Ok(PlatformDirs {
        home,
        app_data,
        local_app_data,
    })
}

pub fn open(data_dir: &std::path::Path) -> AppResult<Connection> {
    let db_path = data_dir.join("nestra.db");
    let conn = Connection::open(&db_path)?;
    // Tuned for a local single-user desktop app with bursty writes (the
    // gateway commits observability rows per proxied request) and 3
    // independent connections (UI, quota worker, gateway).
    //   • WAL + synchronous=NORMAL is the documented safe combo: no corruption
    //     on crash (only the very last txn may be lost — fine for best-effort
    //     observability data) and it removes the per-commit fsync that FULL
    //     imposed on every gateway request.
    //   • busy_timeout lets concurrent writers wait instead of failing with
    //     SQLITE_BUSY the moment the write lock is held.
    //   • temp_store=MEMORY + a larger cache + mmap speed the read-heavy UI
    //     (session lists, route history) without meaningful RAM cost.
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;\
         PRAGMA synchronous=NORMAL;\
         PRAGMA busy_timeout=5000;\
         PRAGMA foreign_keys=ON;\
         PRAGMA temp_store=MEMORY;\
         PRAGMA cache_size=-20000;\
         PRAGMA mmap_size=268435456;\
         PRAGMA wal_autocheckpoint=1000;",
    )?;
    Ok(conn)
}

/// Open a read-only connection (WAL allows unlimited concurrent readers).
/// Used by UI read commands so they don't serialize on the write mutex or
/// stall behind the gateway's writes. `query_only=ON` makes it a hard
/// read-only guard at the SQLite level.
/// Open the DB in true read-only mode (SQLITE_OPEN_READ_ONLY): the old
/// implementation opened read-write (WAL pragmas, possibly CREATING the file)
/// and only then set `query_only` — a "read" command could create nestra.db
/// and run WAL checkpointing. Read-only opens never create or mutate the file.
pub fn open_readonly(data_dir: &std::path::Path) -> AppResult<Connection> {
    let conn = Connection::open_with_flags(
        data_dir.join("nestra.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    // Still set query_only as a belt-and-braces guard against a stray
    // write statement (rusqlite enforces it at the SQLite layer).
    conn.execute_batch("PRAGMA query_only=ON;")?;
    Ok(conn)
}

/// Migrator entrypoint. Delegates to [`crate::schema::migrate`], which builds
/// the canonical v1 schema on first launch and refuses a pre-release database
/// (any `schema_version` other than 1) per the fresh-data policy. See
/// `schema.rs` for the full contract and the credential-boundary guarantees.
pub fn migrate(conn: &Connection) -> AppResult<()> {
    crate::schema::migrate(conn)?;
    // Keep the `agent` table in lock-step with the current agent registry: a
    // new AgentSpec appears on first launch as a `missing` row. Idempotent.
    let now = chrono::Utc::now().timestamp_millis();
    sync_agents_from_registry(conn, now)?;
    Ok(())
}

/// Insert-or-ignore every agent declared in [`crate::agents::AGENTS`] into
/// the `agent` table. Existing rows (with real detection state) are untouched;
/// only missing rows gain a `missing`-status placeholder. This is pure
/// data-sync, not a migration: a new AgentSpec surfaces as a `missing` row on
/// first launch without needing a new schema version.
fn sync_agents_from_registry(conn: &Connection, now: i64) -> AppResult<()> {
    use crate::agents;
    for agent in agents::agents() {
        // Upsert: refresh display_name/kind on conflict — INSERT OR IGNORE
        // left a renamed/restructured registry agent with stale display text
        // in the UI forever (the row existed, so the ignore never updated).
        conn.execute(
            "INSERT INTO agent (id, kind, display_name, status, last_detected_at)
             VALUES (?1, ?2, ?3, 'missing', ?4)
             ON CONFLICT(id) DO UPDATE SET
               kind=excluded.kind,
               display_name=excluded.display_name,
               last_detected_at=excluded.last_detected_at",
            rusqlite::params![agent.id, agent.kind, agent.display_name, now],
        )?;
    }
    Ok(())
}

// ---- setting_kv ----------------------------------------------------------

pub fn get_setting(conn: &Connection, key: &str) -> AppResult<Option<serde_json::Value>> {
    let mut stmt = conn.prepare("SELECT value FROM setting_kv WHERE key = ?1")?;
    let mut rows = stmt.query(rusqlite::params![key])?;
    if let Some(row) = rows.next()? {
        let s: String = row.get(0)?;
        Ok(Some(serde_json::from_str(&s)?))
    } else {
        Ok(None)
    }
}

pub fn set_setting(conn: &Connection, key: &str, value: &serde_json::Value) -> AppResult<()> {
    let s = serde_json::to_string(value)?;
    conn.execute(
        "INSERT INTO setting_kv (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        rusqlite::params![key, s],
    )?;
    Ok(())
}

// ---- provider_endpoint (LLM endpoint + key metadata) ----

#[derive(Debug, Clone, Default)]
pub struct ProtocolEntry {
    pub protocol: String,
    pub base_url: String,
}

#[derive(Debug, Clone)]
pub struct EndpointRow {
    pub id: String,
    pub display_name: String,
    pub has_api_key: bool,
    pub status: String,
    pub last_validated_at: Option<i64>,
    pub models_json: Option<String>,
    pub models_fetched_at: Option<i64>,
    pub advanced_env_json: Option<String>,
    /// Per-model ability overrides keyed by the provider's own model id.
    /// `None` when nothing has been saved yet — the UI defaults to whatever
    /// the models.dev cache produces. Free-form JSON shaped like
    /// `{"<model_id>": { "reasoning": true, "tool_call": true, ... }}` so
    /// the schema can grow without further migrations.
    pub model_abilities_json: Option<String>,
    pub protocols: Vec<ProtocolEntry>,
}

fn row_to_endpoint(r: &rusqlite::Row<'_>) -> rusqlite::Result<EndpointRow> {
    Ok(EndpointRow {
        id: r.get("id")?,
        display_name: r.get("display_name")?,
        has_api_key: r.get::<_, i64>("has_api_key")? != 0,
        status: r.get("status")?,
        last_validated_at: r.get("last_validated_at")?,
        models_json: r.get("models_json")?,
        models_fetched_at: r.get("models_fetched_at")?,
        advanced_env_json: r.get("advanced_env_json")?,
        // Propagate read failures (a corrupt row must not silently lose its
        // ability overrides — the old `.ok()` did exactly that).
        model_abilities_json: r.get("model_abilities_json")?,
        protocols: Vec::new(),
    })
}

fn load_protocols(conn: &Connection, endpoint_id: &str) -> AppResult<Vec<ProtocolEntry>> {
    let mut stmt = conn.prepare(
        "SELECT protocol, base_url FROM endpoint_protocol WHERE endpoint_id = ?1 ORDER BY protocol",
    )?;
    let rows = stmt.query_map(rusqlite::params![endpoint_id], |r| {
        Ok(ProtocolEntry {
            protocol: r.get("protocol")?,
            base_url: r.get("base_url")?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Derive the URL the quota fetcher should hit for an endpoint: the first
/// `openai`/`custom` protocol, falling back to the first protocol. The single
/// source of truth for quota-URL resolution.
pub fn pick_quota_url(rows: &[ProtocolEntry]) -> Option<String> {
    rows.iter()
        .find(|p| p.protocol == "openai-comp" || p.protocol == "custom")
        .or_else(|| rows.first())
        .map(|p| p.base_url.clone())
}

pub fn list_endpoints(conn: &Connection) -> AppResult<Vec<EndpointRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, kind, display_name, has_api_key, status, last_validated_at,
                models_json, models_fetched_at, advanced_env_json, model_abilities_json
         FROM provider_endpoint ORDER BY id",
    )?;
    let rows = stmt.query_map([], row_to_endpoint)?;
    let mut endpoints: Vec<EndpointRow> = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    for ep in &mut endpoints {
        ep.protocols = load_protocols(conn, &ep.id)?;
    }
    Ok(endpoints)
}

pub fn get_endpoint(conn: &Connection, id: &str) -> AppResult<Option<EndpointRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, kind, display_name, has_api_key, status, last_validated_at,
                models_json, models_fetched_at, advanced_env_json, model_abilities_json
         FROM provider_endpoint WHERE id = ?1",
    )?;
    let mut rows = stmt.query(rusqlite::params![id])?;
    match rows.next()? {
        Some(r) => {
            let mut ep = row_to_endpoint(r)?;
            ep.protocols = load_protocols(conn, &ep.id)?;
            Ok(Some(ep))
        }
        None => Ok(None),
    }
}

pub fn create_endpoint(
    conn: &Connection,
    id: &str,
    kind: &str,
    display_name: &str,
) -> AppResult<()> {
    conn.execute(
        "INSERT INTO provider_endpoint (id, kind, display_name, has_api_key, status)
         VALUES (?1, ?2, ?3, 0, 'unvalidated')",
        rusqlite::params![id, kind, display_name],
    )?;
    Ok(())
}

pub fn delete_endpoint(conn: &Connection, id: &str) -> AppResult<bool> {
    let tx = conn.unchecked_transaction()?;
    // Clear the config-backup pointer of every agent bound to this endpoint:
    // it references a provider that no longer exists, and a later restore
    // would resurrect a stale config. (Binding rows themselves cascade.)
    tx.execute(
        "UPDATE agent SET backup_path = NULL WHERE id IN (
            SELECT agent_id FROM agent_provider_binding WHERE endpoint_id = ?1
        )",
        rusqlite::params![id],
    )?;
    let n = tx.execute(
        "DELETE FROM provider_endpoint WHERE id = ?1",
        rusqlite::params![id],
    )?;
    tx.commit()?;
    Ok(n > 0)
}

pub fn set_endpoint_name(conn: &Connection, id: &str, display_name: &str) -> AppResult<()> {
    conn.execute(
        "UPDATE provider_endpoint SET display_name = ?1 WHERE id = ?2",
        rusqlite::params![display_name, id],
    )?;
    Ok(())
}

pub fn set_endpoint_models(conn: &Connection, id: &str, models_json: &str) -> AppResult<()> {
    let now = chrono::Utc::now().timestamp_millis();
    conn.execute(
        "UPDATE provider_endpoint SET models_json = ?1, models_fetched_at = ?2 WHERE id = ?3",
        rusqlite::params![models_json, now, id],
    )?;
    Ok(())
}

pub fn set_endpoint_advanced_env(conn: &Connection, id: &str, env_json: &str) -> AppResult<()> {
    conn.execute(
        "UPDATE provider_endpoint SET advanced_env_json = ?1 WHERE id = ?2",
        rusqlite::params![env_json, id],
    )?;
    Ok(())
}

/// Replace the persisted per-model ability overrides for an endpoint.
/// `None` clears all overrides (defaults are then whatever models.dev has).
pub fn set_endpoint_model_abilities(
    conn: &Connection,
    id: &str,
    abilities_json: Option<&str>,
) -> AppResult<()> {
    conn.execute(
        "UPDATE provider_endpoint SET model_abilities_json = ?1 WHERE id = ?2",
        rusqlite::params![abilities_json, id],
    )?;
    Ok(())
}

pub fn mark_endpoint_key(
    conn: &Connection,
    id: &str,
    has_key: bool,
    status: &str,
) -> AppResult<()> {
    let now = chrono::Utc::now().timestamp_millis();
    conn.execute(
        "UPDATE provider_endpoint SET has_api_key = ?1, status = ?2, last_validated_at = ?3 WHERE id = ?4",
        rusqlite::params![has_key as i64, status, now, id],
    )?;
    Ok(())
}

// ---- endpoint_protocol (N protocol→URL mappings per provider) ----

#[derive(Debug, Clone)]
pub struct EndpointProtocolRow {
    pub protocol: String,
    pub base_url: String,
}

pub fn upsert_endpoint_protocol(
    conn: &Connection,
    endpoint_id: &str,
    protocol: &str,
    base_url: &str,
) -> AppResult<()> {
    conn.execute(
        "INSERT INTO endpoint_protocol (endpoint_id, protocol, base_url)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(endpoint_id, protocol) DO UPDATE SET base_url = excluded.base_url",
        rusqlite::params![endpoint_id, protocol, base_url],
    )?;
    Ok(())
}

pub fn delete_endpoint_protocol(
    conn: &Connection,
    endpoint_id: &str,
    protocol: &str,
) -> AppResult<bool> {
    let n = conn.execute(
        "DELETE FROM endpoint_protocol WHERE endpoint_id = ?1 AND protocol = ?2",
        rusqlite::params![endpoint_id, protocol],
    )?;
    Ok(n > 0)
}

pub fn endpoint_protocols(
    conn: &Connection,
    endpoint_id: &str,
) -> AppResult<Vec<EndpointProtocolRow>> {
    let mut stmt = conn.prepare(
        "SELECT protocol, base_url FROM endpoint_protocol
         WHERE endpoint_id = ?1 ORDER BY protocol",
    )?;
    let rows = stmt.query_map(rusqlite::params![endpoint_id], |r| {
        Ok(EndpointProtocolRow {
            protocol: r.get(0)?,
            base_url: r.get(1)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// ---- agent (detected binaries) ----

#[derive(Debug, Clone)]
pub struct AgentRow {
    pub id: String,
    pub kind: String,
    pub display_name: String,
    pub path: Option<String>,
    pub installed_version: Option<String>,
    pub status: String,
    pub config_path: Option<String>,
    /// The active provider endpoint id, derived from the single `active=1`
    /// row in `agent_provider_binding`. `None` when no binding is active.
    pub active_provider_id: Option<String>,
    pub backup_path: Option<String>,
    /// User-specified binary path override; bypasses automatic detection.
    pub path_override: Option<String>,
    /// User-specified config file location override.
    pub config_path_override: Option<String>,
    /// When false, Nestra stops writing the agent's config file. Detection
    /// still runs and the row remains visible, but provider switching is
    /// inert until re-enabled.
    pub enabled: bool,
    /// Path to the permanent Factory Configuration snapshot
    /// (`<config>.nestra-factory`). NULL until the first enable/switch.
    pub factory_backup_path: Option<String>,
    /// Free-form detection hint surfaced to the UI. NULL for agents without
    /// extra detail; always NULL for the currently-supported agents.
    pub status_detail: Option<String>,
}

fn row_to_agent(r: &rusqlite::Row<'_>) -> rusqlite::Result<AgentRow> {
    Ok(AgentRow {
        id: r.get("id")?,
        kind: r.get("kind")?,
        display_name: r.get("display_name")?,
        path: r.get("path")?,
        installed_version: r.get("installed_version")?,
        status: r.get("status")?,
        config_path: r.get("config_path")?,
        // Derived from the binding table via the LEFT JOIN / subquery in the
        // list_agents SELECT. `.ok().flatten()` keeps callers that read a
        // single agent row without the subquery working.
        active_provider_id: r.get("active_provider_id").ok().flatten(),
        backup_path: r.get("backup_path")?,
        path_override: r.get("path_override").ok(),
        config_path_override: r.get("config_path_override").ok(),
        // Propagate a read failure instead of defaulting to ENABLED: a
        // corrupt `enabled` column must not silently re-enable an agent the
        // user disabled.
        enabled: r.get::<_, i64>("enabled")? != 0,
        factory_backup_path: r.get("factory_backup_path").ok(),
        status_detail: r.get("status_detail").ok(),
    })
}

pub fn list_agents(conn: &Connection) -> AppResult<Vec<AgentRow>> {
    let mut stmt = conn.prepare(
        "SELECT a.id, a.kind, a.display_name, a.path, a.installed_version,
                a.status, a.config_path, a.backup_path, a.path_override,
                a.config_path_override, a.enabled, a.factory_backup_path,
                a.status_detail,
                (SELECT b.endpoint_id FROM agent_provider_binding b
                 WHERE b.agent_id = a.id AND b.active = 1) AS active_provider_id
         FROM agent a
         ORDER BY a.id",
    )?;
    let rows = stmt.query_map([], row_to_agent)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn upsert_agent(
    conn: &Connection,
    id: &str,
    kind: &str,
    display_name: &str,
    path: Option<&str>,
    installed_version: Option<&str>,
    status: &str,
    config_path: Option<&str>,
) -> AppResult<()> {
    let now = chrono::Utc::now().timestamp_millis();
    conn.execute(
        "INSERT INTO agent (id, kind, display_name, path, installed_version, status, config_path, last_detected_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(id) DO UPDATE SET
           path=excluded.path,
           installed_version=excluded.installed_version,
           status=excluded.status,
           config_path=excluded.config_path,
           last_detected_at=excluded.last_detected_at",
        rusqlite::params![id, kind, display_name, path, installed_version, status, config_path, now],
    )?;
    Ok(())
}

/// Persist the user-supplied path overrides for an agent row. Pass `None` to
/// clear a column. The caller is expected to validate that `path_override`
/// already exists on disk before invoking this.
pub fn set_agent_overrides(
    conn: &Connection,
    id: &str,
    path_override: Option<&str>,
    config_path_override: Option<&str>,
) -> AppResult<()> {
    conn.execute(
        "UPDATE agent SET path_override = ?1, config_path_override = ?2 WHERE id = ?3",
        rusqlite::params![path_override, config_path_override, id],
    )?;
    Ok(())
}

/// Persist the on-disk config backup path for an agent. The active provider
/// is derived from `agent_provider_binding.active=1` and is not stored here.
pub fn set_agent_binding(
    conn: &Connection,
    id: &str,
    backup_path: Option<&str>,
) -> AppResult<()> {
    conn.execute(
        "UPDATE agent SET backup_path = ?1 WHERE id = ?2",
        rusqlite::params![backup_path, id],
    )?;
    Ok(())
}

/// Toggle whether Nestra actively manages this agent's config file. Detection
/// still runs and the row remains visible, but writes are inert while
/// disabled. Returns the new value.
pub fn set_agent_enabled(conn: &Connection, id: &str, enabled: bool) -> AppResult<bool> {
    conn.execute(
        "UPDATE agent SET enabled = ?1 WHERE id = ?2",
        rusqlite::params![enabled as i64, id],
    )?;
    Ok(enabled)
}

/// Persist the path to the Factory Configuration snapshot. Pass `None` only
/// when explicitly clearing (there is normally no reason to — the factory is
/// permanent). The snapshot bytes themselves live on disk at the path.
pub fn set_agent_factory_path(
    conn: &Connection,
    id: &str,
    factory_backup_path: Option<&str>,
) -> AppResult<()> {
    conn.execute(
        "UPDATE agent SET factory_backup_path = ?1 WHERE id = ?2",
        rusqlite::params![factory_backup_path, id],
    )?;
    Ok(())
}

// ---- mcp_server (SSOT for MCP servers, synced to provider config files) ----

/// Canonical server row read from the DB. `transport_json` is the serialized
/// `McpTransport` (stdio + http variants) owned by crate::mcp. `enabled_agents`
/// = written with `enabled: true`; `disabled_agents` = written with
/// `enabled: false` (formats that carry the flag); in neither = not written.
/// The two lists are kept disjoint by the mcp module's mutation entry points.
pub struct McpServerRow {
    pub id: String,
    pub name: String,
    pub transport_json: String,
    pub enabled_agents: Vec<String>,
    pub disabled_agents: Vec<String>,
}

fn row_to_mcp_server(r: &rusqlite::Row<'_>) -> rusqlite::Result<McpServerRow> {
    let enabled_json: String = r.get("enabled_agents")?;
    let disabled_json: String = r.get("disabled_agents")?;
    Ok(McpServerRow {
        id: r.get("id")?,
        name: r.get("name")?,
        transport_json: r.get("transport_json")?,
        enabled_agents: serde_json::from_str(&enabled_json).unwrap_or_default(),
        disabled_agents: serde_json::from_str(&disabled_json).unwrap_or_default(),
    })
}

pub fn list_mcp_servers(conn: &Connection) -> AppResult<Vec<McpServerRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, transport_json, enabled_agents, disabled_agents
         FROM mcp_server ORDER BY name",
    )?;
    let rows = stmt
        .query_map([], row_to_mcp_server)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn get_mcp_server(conn: &Connection, id: &str) -> AppResult<Option<McpServerRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, transport_json, enabled_agents, disabled_agents
         FROM mcp_server WHERE id = ?1",
    )?;
    let mut rows = stmt.query(rusqlite::params![id])?;
    match rows.next()? {
        Some(r) => Ok(Some(row_to_mcp_server(r)?)),
        None => Ok(None),
    }
}

pub fn upsert_mcp_server(
    conn: &Connection,
    id: &str,
    name: &str,
    transport_json: &str,
    enabled_agents: &[String],
    disabled_agents: &[String],
) -> AppResult<()> {
    let now = chrono::Utc::now().timestamp_millis();
    conn.execute(
        "INSERT INTO mcp_server (id, name, transport_json, enabled_agents, disabled_agents, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(id) DO UPDATE SET
           name=excluded.name,
           transport_json=excluded.transport_json,
           enabled_agents=excluded.enabled_agents,
           disabled_agents=excluded.disabled_agents",
        rusqlite::params![
            id,
            name,
            transport_json,
            serde_json::to_string(enabled_agents)?,
            serde_json::to_string(disabled_agents)?,
            now
        ],
    )?;
    Ok(())
}

/// Rewrite only the per-agent columns of one managed server (used by the
/// registry-churn repair in `mcp::sync_all` — the read boundary filters
/// unknown agent ids; this persists the pruned set).
pub fn update_mcp_server_agents(
    conn: &Connection,
    id: &str,
    enabled_agents: &[String],
    disabled_agents: &[String],
) -> AppResult<()> {
    conn.execute(
        "UPDATE mcp_server SET enabled_agents = ?2, disabled_agents = ?3 WHERE id = ?1",
        rusqlite::params![
            id,
            serde_json::to_string(enabled_agents)?,
            serde_json::to_string(disabled_agents)?,
        ],
    )?;
    Ok(())
}

pub fn delete_mcp_server(conn: &Connection, id: &str) -> AppResult<bool> {
    if conn.is_autocommit() {
        // Both deletes in ONE transaction: two auto-commit statements left
        // orphaned env rows when the second delete failed (and that error
        // was swallowed with `let _ =`).
        let tx = conn.unchecked_transaction()?;
        tx.execute("DELETE FROM mcp_server_env WHERE server_id = ?1", rusqlite::params![id])?;
        let n = tx.execute("DELETE FROM mcp_server WHERE id = ?1", rusqlite::params![id])?;
        tx.commit()?;
        Ok(n > 0)
    } else {
        // Caller already holds a transaction (e.g. save()'s rename cleanup) —
        // just run both statements; they commit with the caller's tx.
        conn.execute("DELETE FROM mcp_server_env WHERE server_id = ?1", rusqlite::params![id])?;
        let n = conn.execute("DELETE FROM mcp_server WHERE id = ?1", rusqlite::params![id])?;
        Ok(n > 0)
    }
}

/// All per-agent env overrides for a server, keyed by agent_id then env_key.
pub fn list_mcp_env_overrides(
    conn: &Connection,
    server_id: &str,
) -> AppResult<std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>>
{
    use std::collections::BTreeMap;
    let mut stmt =
        conn.prepare("SELECT agent_id, env_key, env_value FROM mcp_server_env WHERE server_id = ?1")?;
    let mut out: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let rows = stmt.query_map(rusqlite::params![server_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (agent, k, v) = row?;
        out.entry(agent).or_default().insert(k, v);
    }
    Ok(out)
}

/// Replace the full set of per-agent env overrides for a server (delete all,
/// insert the given map). Callers pass the authoritative map they want stored.
/// Wrapped in one transaction so a failure mid-loop can't leave the table
/// half-wiped (and so it's one commit, not N+1).
pub fn replace_mcp_env_overrides(
    conn: &Connection,
    server_id: &str,
    overrides: &std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>,
) -> AppResult<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "DELETE FROM mcp_server_env WHERE server_id = ?1",
        rusqlite::params![server_id],
    )?;
    for (agent, env) in overrides {
        for (k, v) in env {
            tx.execute(
                "INSERT OR REPLACE INTO mcp_server_env (server_id, agent_id, env_key, env_value)
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![server_id, agent, k, v],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}

// ---- agent_provider_binding (live agent↔endpoint link) ----
//
// One row per (agent, endpoint) link. The active binding (there is exactly
// one per agent) is what `do_switch_provider` reads to determine which
// endpoint to inject. Model, base_url, protocol, API key, display name, and
// status all live on the endpoint itself — the binding carries no copies.

/// All bindings for one agent, joined with endpoint display data the UI needs.
/// Ordered so the active binding is first, then by created_at then endpoint id
/// for a stable secondary order.
#[derive(Debug, Clone)]
pub struct BindingRow {
    pub agent_id: String,
    pub endpoint_id: String,
    pub active: bool,
    pub display_name: String,
    pub has_api_key: bool,
    pub status: String,
    pub last_validated_at: Option<i64>,
    /// First protocol on the endpoint that the agent's ConfigAdapter accepts.
    /// `None` when the endpoint has no compatible protocol (UI flags it).
    pub resolved_protocol: Option<String>,
    pub resolved_base_url: Option<String>,
}

pub fn list_bindings(conn: &Connection, agent_id: &str) -> AppResult<Vec<BindingRow>> {
    let accepted: Vec<&'static str> = crate::agents::adapter_for(agent_id)
        .map(|a| a.accepts().iter().map(|k| k.as_str()).collect())
        .unwrap_or_default();
    let mut stmt = conn.prepare(
        "SELECT b.endpoint_id, b.active, b.protocol,
                e.display_name, e.has_api_key, e.status, e.last_validated_at
         FROM agent_provider_binding b
         JOIN provider_endpoint e ON b.endpoint_id = e.id
         WHERE b.agent_id = ?1
         ORDER BY b.active DESC, b.created_at, b.endpoint_id",
    )?;
    let mut rows = stmt.query(rusqlite::params![agent_id])?;
    let mut out: Vec<BindingRow> = Vec::new();
    while let Some(r) = rows.next()? {
        let endpoint_id: String = r.get(0)?;
        let active: bool = r.get::<_, i64>(1)? != 0;
        let stored: Option<String> = r.get(2)?;
        let display_name: String = r.get(3)?;
        let has_api_key: bool = r.get::<_, i64>(4)? != 0;
        let status: String = r.get(5)?;
        let last_validated_at: Option<i64> = r.get(6)?;

        // Resolve the wire for display: a stored per-binding override wins
        // when still valid; otherwise the first accepted protocol row
        // (alphabetical — reproduces the pre-picker result for a NULL override).
        let protos = endpoint_protocols(conn, &endpoint_id)?;
        let (resolved_protocol, resolved_base_url) =
            resolve_binding_wire(&stored, &accepted, &protos);

        out.push(BindingRow {
            agent_id: agent_id.to_string(),
            endpoint_id,
            active,
            display_name,
            has_api_key,
            status,
            last_validated_at,
            resolved_protocol,
            resolved_base_url,
        });
    }
    Ok(out)
}

/// Resolve `(protocol, base_url)` for a binding: a stored per-binding
/// override wins when it is still valid (in the adapter's `accepts()` AND the
/// endpoint still carries that protocol row); otherwise the first accepted
/// protocol row (alphabetical — the historical behavior, so a NULL override
/// reproduces the pre-picker result exactly). `accepted` is the agent
/// adapter's `accepts()` list; `protos` is the endpoint's protocol rows.
fn resolve_binding_wire(
    stored: &Option<String>,
    accepted: &[&'static str],
    protos: &[EndpointProtocolRow],
) -> (Option<String>, Option<String>) {
    let valid_stored = stored
        .as_ref()
        .filter(|p| accepted.iter().any(|a| *a == p.as_str()))
        .filter(|p| protos.iter().any(|pr| &pr.protocol == *p));
    match valid_stored {
        Some(p) => {
            let url = protos
                .iter()
                .find(|pr| &pr.protocol == p)
                .map(|pr| pr.base_url.clone());
            (Some(p.clone()), url)
        }
        None => protos
            .iter()
            .find(|pr| accepted.iter().any(|a| *a == pr.protocol.as_str()))
            .map(|pr| (Some(pr.protocol.clone()), Some(pr.base_url.clone())))
            .unwrap_or((None, None)),
    }
}

pub fn list_all_bindings(conn: &Connection) -> AppResult<Vec<BindingRow>> {
    let mut stmt = conn.prepare(
        "SELECT b.agent_id, b.endpoint_id, b.active, b.protocol,
                e.display_name, e.has_api_key, e.status, e.last_validated_at
         FROM agent_provider_binding b
         JOIN provider_endpoint e ON b.endpoint_id = e.id
         ORDER BY b.agent_id, b.active DESC, b.created_at, b.endpoint_id",
    )?;
    let mut out = Vec::new();
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let agent_id: String = r.get(0)?;
        let endpoint_id: String = r.get(1)?;
        let active: bool = r.get::<_, i64>(2)? != 0;
        let stored: Option<String> = r.get(3)?;
        let display_name: String = r.get(4)?;
        let has_api_key: bool = r.get::<_, i64>(5)? != 0;
        let status: String = r.get(6)?;
        let last_validated_at: Option<i64> = r.get(7)?;

        let accepted: Vec<&'static str> = crate::agents::adapter_for(&agent_id)
            .map(|a| a.accepts().iter().map(|k| k.as_str()).collect())
            .unwrap_or_default();
        let protos = endpoint_protocols(conn, &endpoint_id)?;
        let (resolved_protocol, resolved_base_url) =
            resolve_binding_wire(&stored, &accepted, &protos);

        out.push(BindingRow {
            agent_id,
            endpoint_id,
            active,
            display_name,
            has_api_key,
            status,
            last_validated_at,
            resolved_protocol,
            resolved_base_url,
        });
    }
    Ok(out)
}

pub fn upsert_binding(
    conn: &Connection,
    agent_id: &str,
    endpoint_id: &str,
) -> AppResult<bool> {
    let now = chrono::Utc::now().timestamp_millis();
    let n = conn.execute(
        "INSERT OR IGNORE INTO agent_provider_binding
           (agent_id, endpoint_id, active, created_at)
         VALUES (?1, ?2, 0, ?3)",
        rusqlite::params![agent_id, endpoint_id, now],
    )?;
    Ok(n > 0)
}

pub fn set_active_binding(
    conn: &Connection,
    agent_id: &str,
    endpoint_id: &str,
) -> AppResult<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "UPDATE agent_provider_binding SET active = 0 WHERE agent_id = ?1",
        rusqlite::params![agent_id],
    )?;
    let n = tx.execute(
        "UPDATE agent_provider_binding SET active = 1
         WHERE agent_id = ?1 AND endpoint_id = ?2",
        rusqlite::params![agent_id, endpoint_id],
    )?;
    if n == 0 {
        tx.rollback()?;
        return Err(AppError::NotFound(format!(
            "no binding for agent '{agent_id}' -> endpoint '{endpoint_id}'"
        )));
    }
    tx.commit()?;
    Ok(())
}

/// Replace the entire binding set for an agent in one transaction. Each entry
/// in `selected` is `(endpoint_id, per-binding protocol override)` — the
/// override is `None` for "resolve the default (first accepted)". The entry
/// whose endpoint matches `default_endpoint` is marked active. Anything
/// currently bound but missing from `selected` is removed. Returns the previous
/// active endpoint id, when it changed, so callers can decide whether the
/// on-disk config needs a rewrite.
pub fn replace_bindings(
    conn: &Connection,
    agent_id: &str,
    selected: &[(String, Option<String>)],
    default_endpoint: &str,
) -> AppResult<String> {
    let tx = conn.unchecked_transaction()?;
    let previous_default: Option<String> = tx
        .query_row(
            "SELECT endpoint_id FROM agent_provider_binding
             WHERE agent_id = ?1 AND active = 1",
            rusqlite::params![agent_id],
            |r| r.get(0),
        )
        .ok();
    tx.execute(
        "DELETE FROM agent_provider_binding WHERE agent_id = ?1",
        rusqlite::params![agent_id],
    )?;
    let now = chrono::Utc::now().timestamp_millis();
    for (ep, protocol) in selected {
        let active = if ep == default_endpoint { 1 } else { 0 };
        tx.execute(
            "INSERT INTO agent_provider_binding
               (agent_id, endpoint_id, active, protocol, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![agent_id, ep, active, protocol, now],
        )?;
    }
    if !selected.iter().any(|(e, _)| e == default_endpoint) {
        // No binding is active; clear the backup pointer too (matches the
        // "no active provider" semantics callers rely on).
        tx.execute(
            "UPDATE agent SET backup_path = NULL WHERE id = ?1",
            rusqlite::params![agent_id],
        )?;
    }
    tx.commit()?;
    Ok(previous_default.unwrap_or_default())
}

/// The per-binding Direct-wire override stored on `agent_provider_binding`.
/// `None` when unset (no row, or the row's `protocol` column is NULL) — the
/// caller then resolves the default (first accepted protocol).
pub fn binding_protocol(
    conn: &Connection,
    agent_id: &str,
    endpoint_id: &str,
) -> AppResult<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT protocol FROM agent_provider_binding
             WHERE agent_id = ?1 AND endpoint_id = ?2",
            rusqlite::params![agent_id, endpoint_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten())
}

/// Remove every binding for an agent and clear the backup path. Used by the
/// disable-Nestra-management flow.
pub fn clear_all_bindings(conn: &Connection, agent_id: &str) -> AppResult<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "DELETE FROM agent_provider_binding WHERE agent_id = ?1",
        rusqlite::params![agent_id],
    )?;
    tx.execute(
        "UPDATE agent SET backup_path = NULL WHERE id = ?1",
        rusqlite::params![agent_id],
    )?;
    tx.commit()?;
    Ok(())
}

/// Remove whatever binding is currently active for `agent_id`. Returns the
/// endpoint id that was bound, or `None` when no binding was active. Used by
/// `agent_clear_provider` / `agent_restore_factory` to unbind without naming
/// an endpoint explicitly.
pub fn clear_active_binding(
    conn: &Connection,
    agent_id: &str,
) -> AppResult<Option<String>> {
    let tx = conn.unchecked_transaction()?;
    let active: Option<String> = tx
        .query_row(
            "SELECT endpoint_id FROM agent_provider_binding
             WHERE agent_id = ?1 AND active = 1",
            rusqlite::params![agent_id],
            |r| r.get(0),
        )
        .ok();
    if let Some(ref ep) = active {
        tx.execute(
            "DELETE FROM agent_provider_binding
             WHERE agent_id = ?1 AND endpoint_id = ?2",
            rusqlite::params![agent_id, ep],
        )?;
    }
    tx.execute(
        "UPDATE agent SET backup_path = NULL WHERE id = ?1",
        rusqlite::params![agent_id],
    )?;
    tx.commit()?;
    Ok(active)
}

/// Prune observability data older than the configured retention window.
///
/// Deletes `task` rows — and their cascaded children `route_request`,
/// `route_migration` (all `ON DELETE CASCADE` in the v1 schema) — whose
/// timestamps predate the `log_retention_days` setting (default 30). This is
/// the backing implementation for the "log retention" UI setting, which
/// promises "older entries are pruned automatically" but was previously
/// orphaned — the backend never read it.
///
/// Called once per launch from [`crate::commands::run_launch_reconcile`] on
/// the dedicated `reconcile_db` connection (off the UI locks). SQLite reuses
/// freed pages for new inserts, so the file size holds at its high-water mark
/// without a VACUUM — growth is bounded, which is the goal.
pub fn prune_observability_data(conn: &Connection) -> AppResult<u64> {
    let days = get_setting(conn, "log_retention_days")
        .ok()
        .flatten()
        .and_then(|v| v.as_u64())
        .unwrap_or(30)
        .max(1); // clamp: 0 would prune everything immediately
    let now = chrono::Utc::now().timestamp_millis();
    let cutoff = now - (days as i64 * 86_400_000);

    // Deleting `task` rows cascades to route_request → route_migration.
    let tasks = conn.execute(
        "DELETE FROM task WHERE started_at < ?1",
        rusqlite::params![cutoff],
    )? as u64;

    if tasks > 0 {
        tracing::info!(days, tasks, "pruned observability data older than retention window");
    }
    Ok(tasks)
}

#[cfg(test)]
mod tests;
