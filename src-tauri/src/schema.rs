//! Canonical schema — single source of truth for every table.
//!
//! The entire schema is defined here as one canonical block (`SCHEMA_V1`),
//! built in one shot and stamped `schema_version = 1`. There is no incremental
//! migration code; future schema evolution resumes versioning from v1
//! (v2 = additive `ALTER` block appended to [`migrate`]).
//!
//! ## Fresh-data policy
//!
//! Nestra builds the database from `SCHEMA_V1` on first launch. An existing
//! database whose `schema_version` is anything other than `1` is treated as a
//! pre-release build's data directory: [`migrate`] returns a clear error and
//! the caller exits safely without modifying the database, starting workers,
//! or initializing application state. Public v0.1.0 does not migrate or import
//! any pre-release database.
//!
//! **Credential boundary:** no table carries a credential, API-key, token, or
//! secret column. Credentials live only in `secrets.rs` and are resolved at
//! request time via `CredentialHandle` (see `orchestration/identity.rs`).
//! Enforced by `tests::no_secret_columns`.
//!
//! One deliberate exception: `mcp_server_env.env_value` stores MCP server
//! environment overrides as plaintext. These are USER-CONFIGURED values the
//! agent's own config file already carries verbatim (a stdio server's env,
//! e.g. a local MCP server's API key) — Nestra mirrors what the user typed
//! into the agent, so encrypting it here would not reduce exposure on disk.
//! Nestra-MANAGED credentials (provider API keys, gateway tokens, opencode
//! cookies) never touch this table; they live in `secrets.rs` only.

use crate::error::{AppError, AppResult};
use rusqlite::Connection;

/// The one canonical schema version. Future additive changes bump this and add
/// a guarded `ALTER` block to [`migrate`].
pub const SCHEMA_VERSION: i32 = 1;

/// The full canonical schema as one DDL string. Every table Nestra owns is
/// declared here exactly once — there is no per-version ALTER history.
///
/// **Credential boundary:** no table carries a credential, API-key, token, or
/// secret column. Credentials live only in `secrets.rs` and are resolved at
/// request time via `CredentialHandle` (see `orchestration/identity.rs`).
/// Enforced by `tests::no_secret_columns`.
const SCHEMA_V1: &str = r#"
-- ---- bookkeeping ---------------------------------------------------------
CREATE TABLE IF NOT EXISTS schema_version (
  version    INTEGER PRIMARY KEY,
  applied_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS setting_kv (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

-- ---- providers / endpoints ----------------------------------------------
CREATE TABLE IF NOT EXISTS provider_endpoint (
  id                  TEXT PRIMARY KEY,
  kind                TEXT NOT NULL,
  display_name        TEXT NOT NULL,
  has_api_key         INTEGER NOT NULL DEFAULT 0,
  status              TEXT NOT NULL DEFAULT 'unvalidated',
  last_validated_at   INTEGER,
  models_json         TEXT,
  models_fetched_at   INTEGER,
  advanced_env_json   TEXT,
  model_abilities_json TEXT,
  -- Per-endpoint opt-in to the Nestra gateway (off by default; the agent
  -- still talks directly to the upstream until the user turns this on).
  gateway_enabled     INTEGER NOT NULL DEFAULT 0,
  -- Best-effort reactive quota snapshot written by the gateway when it
  -- observes quota/rate-limit signals for this endpoint. NULL when none.
  last_quota_state    TEXT
);

CREATE TABLE IF NOT EXISTS endpoint_protocol (
  endpoint_id TEXT NOT NULL,
  protocol    TEXT NOT NULL,
  base_url    TEXT NOT NULL,
  PRIMARY KEY (endpoint_id, protocol),
  FOREIGN KEY (endpoint_id) REFERENCES provider_endpoint(id) ON DELETE CASCADE
);

-- ---- agents + bindings ---------------------------------------------------
CREATE TABLE IF NOT EXISTS agent (
  id                   TEXT PRIMARY KEY,
  kind                 TEXT NOT NULL,
  display_name         TEXT NOT NULL,
  path                 TEXT,
  installed_version    TEXT,
  status               TEXT NOT NULL,
  config_path          TEXT,
  backup_path          TEXT,
  last_detected_at     INTEGER NOT NULL,
  enabled              INTEGER NOT NULL DEFAULT 1,
  factory_backup_path  TEXT,
  status_detail        TEXT,
  path_override        TEXT,
  config_path_override TEXT
);

CREATE TABLE IF NOT EXISTS agent_provider_binding (
  agent_id    TEXT NOT NULL,
  endpoint_id TEXT NOT NULL,
  active      INTEGER NOT NULL DEFAULT 0,
  -- Per-binding Direct-wire override. NULL = resolve the default (first
  -- protocol row the agent's adapter accepts). When set, the value must be
  -- one of the endpoint's `endpoint_protocol.protocol` rows AND in the
  -- agent's `accepts()` list; `build_switch_context` validates both.
  protocol    TEXT,
  created_at  INTEGER NOT NULL,
  PRIMARY KEY (agent_id, endpoint_id),
  FOREIGN KEY (agent_id)    REFERENCES agent(id)            ON DELETE CASCADE,
  FOREIGN KEY (endpoint_id) REFERENCES provider_endpoint(id) ON DELETE CASCADE
);
-- idx_agent_provider_binding_agent(agent_id) was redundant with the PK
-- prefix (agent_id, endpoint_id) — dropped to halve binding-mutation cost.

-- ---- skills + mcp --------------------------------------------------------
CREATE TABLE IF NOT EXISTS skill (
  id             TEXT PRIMARY KEY,
  name           TEXT NOT NULL,
  description    TEXT,
  source         TEXT NOT NULL,
  enabled_agents TEXT NOT NULL DEFAULT '[]',
  ssot_path      TEXT NOT NULL,
  created_at     INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS mcp_server (
  id              TEXT PRIMARY KEY,
  name            TEXT NOT NULL,
  transport_json  TEXT NOT NULL,
  enabled_agents  TEXT NOT NULL DEFAULT '[]',
  disabled_agents TEXT NOT NULL DEFAULT '[]',
  created_at      INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS mcp_server_env (
  server_id TEXT NOT NULL,
  agent_id  TEXT NOT NULL,
  env_key   TEXT NOT NULL,
  env_value TEXT NOT NULL,
  PRIMARY KEY (server_id, agent_id, env_key)
);

-- ---- sessions (derived cache, regenerated from agent logs) ---------------
CREATE TABLE IF NOT EXISTS session (
  provider               TEXT NOT NULL,
  id                     TEXT NOT NULL,
  title                  TEXT NOT NULL,
  summary                TEXT NOT NULL,
  project                TEXT,
  cwd                    TEXT,
  started_at             INTEGER NOT NULL,
  updated_at             INTEGER NOT NULL,
  ended_at               INTEGER,
  message_count          INTEGER NOT NULL,
  source_path            TEXT NOT NULL,
  parent_session_id      TEXT,
  agent_id               TEXT,
  is_subagent            INTEGER NOT NULL DEFAULT 0,
  resume_command         TEXT NOT NULL,
  child_count            INTEGER NOT NULL DEFAULT 0,
  source_files_json      TEXT NOT NULL DEFAULT '[]',
  provider_metadata_json TEXT NOT NULL DEFAULT '{}',
  PRIMARY KEY (provider, id)
);
CREATE INDEX IF NOT EXISTS idx_session_parent ON session(parent_session_id);
-- Top-level (non-subagent) sessions ordered by most-recent activity — serves
-- the Sessions list (`WHERE is_subagent = 0 ORDER BY updated_at DESC LIMIT N`)
-- as an index range scan with no filesort. The most-opened screen.
CREATE INDEX IF NOT EXISTS idx_session_top_level
  ON session(is_subagent, updated_at DESC);

CREATE TABLE IF NOT EXISTS session_source (
  provider   TEXT NOT NULL,
  path       TEXT NOT NULL,
  file_mtime INTEGER NOT NULL,
  PRIMARY KEY (provider, path)
);

CREATE TABLE IF NOT EXISTS session_message (
  provider               TEXT NOT NULL,
  session_id             TEXT NOT NULL,
  seq                    INTEGER NOT NULL,
  role                   TEXT NOT NULL,
  content_text           TEXT NOT NULL,
  tool_name              TEXT,
  tool_input             TEXT,
  tool_output            TEXT,
  parent_message_id      TEXT,
  message_id             TEXT,
  timestamp              INTEGER,
  tool_call_id           TEXT,
  thinking               TEXT,
  provider_metadata_json TEXT NOT NULL DEFAULT '{}',
  PRIMARY KEY (provider, session_id, seq)
);
-- idx_session_message(provider, session_id, seq) was identical to this PK —
-- dropped (it doubled write cost on every session reconcile for no gain).

CREATE TABLE IF NOT EXISTS session_part (
  provider               TEXT NOT NULL,
  session_id             TEXT NOT NULL,
  seq                    INTEGER NOT NULL,
  part_idx               INTEGER NOT NULL DEFAULT 0,
  kind                   TEXT NOT NULL,
  payload_json           TEXT NOT NULL,
  tool_call_id           TEXT,
  ts                     INTEGER,
  raw_json               TEXT NOT NULL,
  provider_metadata_json TEXT NOT NULL DEFAULT '{}',
  PRIMARY KEY (provider, session_id, seq, part_idx)
);
-- idx_session_part(provider, session_id, seq) was a strict prefix of this PK
-- — dropped (redundant; doubled reconcile write cost).

-- =========================================================================
-- orchestration control plane
-- =========================================================================

-- Per-(agent, subagent role) routing policy. The router reads this to decide
-- preferred/fallback endpoints, allowed models, migration behavior, cache
-- injection, and route-affinity scope. `role = '*'` is the catch-all default.
CREATE TABLE IF NOT EXISTS routing_policy (
  agent_id             TEXT NOT NULL,
  role                 TEXT NOT NULL,
  preferred_endpoints  TEXT,          -- JSON array of endpoint ids, in priority order
  fallback_endpoints   TEXT,          -- JSON array of endpoint ids
  allowed_models       TEXT,          -- JSON array of model-id globs, or NULL = any
  migrate_on_quota     INTEGER NOT NULL DEFAULT 1,
  inject_cache_control INTEGER NOT NULL DEFAULT 0,
  affinity_scope       TEXT NOT NULL DEFAULT 'task',  -- 'task' | 'session' | 'none'
  updated_at           INTEGER NOT NULL,
  PRIMARY KEY (agent_id, role)
);

-- A Task = one Nestra-owned unit of routing/work. `task_id` is Nestra's own
-- identity; `native_task_ref` optionally carries an agent-native task handle
-- (Claude Task tool, OpenCode task, Pi task) for UI correlation and is NEVER
-- load-bearing for routing. See orchestration/identity.rs §"Task is a Nestra
-- orchestration concept".
CREATE TABLE IF NOT EXISTS task (
  id              TEXT PRIMARY KEY,            -- Nestra UUID (task_id)
  parent_task_id  TEXT,                         -- NULL for top-level tasks
  lifecycle       TEXT NOT NULL DEFAULT 'born', -- born|routed|inflight|migrating|generationbroken|done|failed
  native_task_ref TEXT,                         -- JSON {agent,kind,ref_id}, or NULL
  started_at      INTEGER NOT NULL,
  ended_at        INTEGER,
  FOREIGN KEY (parent_task_id) REFERENCES task(id) ON DELETE SET NULL
);
CREATE INDEX IF NOT EXISTS idx_task_parent ON task(parent_task_id);

-- One row per proxied HTTP request through the Nestra gateway. The credential
-- itself is NEVER stored here; only the credential-free resolution + observed
-- outcome. `generation_broken` records whether a mid-stream retry/migration
-- forced a fresh upstream generation — the UI uses this to label the response
-- honestly.
CREATE TABLE IF NOT EXISTS route_request (
  request_id        TEXT PRIMARY KEY,          -- Nestra UUID
  task_id           TEXT NOT NULL,
  agent_id          TEXT NOT NULL,
  logical_session   TEXT,                      -- denormalized for fast session queries
  subagent_role     TEXT,
  role_source       TEXT,
  requested_model   TEXT,
  requested_provider TEXT,
  resolved_endpoint_id TEXT,
  resolved_model    TEXT,
  protocol          TEXT,
  route_reason      TEXT NOT NULL,
  http_status       INTEGER,
  usage_input       INTEGER,
  usage_output      INTEGER,
  cache_creation    INTEGER,
  cache_read        INTEGER,
  tool_calls        INTEGER,                   -- distinct tool_use/tool_call ids seen in the stream
  tool_names        TEXT,                      -- JSON {name: count} — gateway-observed tool-call invocations
  generation_broken INTEGER NOT NULL DEFAULT 0,
  started_at        INTEGER NOT NULL,
  ended_at          INTEGER,
  FOREIGN KEY (task_id)              REFERENCES task(id)            ON DELETE CASCADE,
  FOREIGN KEY (resolved_endpoint_id) REFERENCES provider_endpoint(id) ON DELETE SET NULL
);
-- Per-task history + the latest-status lookup in task_summaries: a composite
-- (task_id, started_at DESC) serves both the `WHERE task_id=?` equality AND
-- the `ORDER BY started_at` / "latest row per task" read without a sort.
-- (Supersedes the old single-column idx_route_request_task.)
CREATE INDEX IF NOT EXISTS idx_route_request_task_started
  ON route_request(task_id, started_at DESC);
CREATE INDEX IF NOT EXISTS idx_route_request_session ON route_request(agent_id, logical_session);
-- tasks_for_session filters on logical_session alone (agent_id unconstrained)
-- — idx_route_request_session can't serve a non-leading-column equality.
-- This index fixes that read, which scales with lifetime request volume.
CREATE INDEX IF NOT EXISTS idx_route_request_logical_session
  ON route_request(logical_session, started_at);

-- One row per migration event. Records why a request moved from one endpoint
-- to another. Reason vocabulary: 'quota_exhausted' | 'rate_limit' | 'temp_5xx'
-- | 'timeout' | 'policy' | 'user_override' (see the failure taxonomy in
-- orchestration/router.rs). Auth/4xx errors are NOT migrations and never
-- appear here.
CREATE TABLE IF NOT EXISTS route_migration (
  id               TEXT PRIMARY KEY,
  request_id       TEXT NOT NULL,
  task_id          TEXT NOT NULL,
  from_endpoint_id TEXT,
  to_endpoint_id   TEXT,
  reason           TEXT NOT NULL,
  detail           TEXT,
  at_ms            INTEGER NOT NULL,
  FOREIGN KEY (request_id) REFERENCES route_request(request_id) ON DELETE CASCADE,
  FOREIGN KEY (task_id)    REFERENCES task(id)                  ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_route_migration_task ON route_migration(task_id);

-- One row per generated context handoff (Context Lifecycle R1). The markdown
-- artifact on disk is what the agent reads; this row is Nestra's index
-- (source session, snapshots, artifact path, structured sections for search).
-- `artifact_path` is absolute. The file is user-editable after creation —
-- Nestra never rewrites an existing artifact.
CREATE TABLE IF NOT EXISTS handoff (
  id                TEXT PRIMARY KEY,           -- Nestra uuid
  source_provider   TEXT NOT NULL,
  source_session_id TEXT NOT NULL,              -- the session handed off FROM
  target_session_id TEXT,                       -- the new session handed off TO (null until known)
  token_snapshot    INTEGER,                    -- tokens shed (null when usage wasn't recorded)
  cost_snapshot     REAL,
  artifact_path     TEXT NOT NULL,              -- the .md file the agent reads
  sections_json     TEXT NOT NULL,              -- structured extraction (for UI/search)
  created_at        INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_handoff_source ON handoff(source_provider, source_session_id);

-- One row per spawned review (Review Runtime). The review itself is a normal
-- Pi session Nestra spawned (`pi --mode rpc` + the reviewer role marker); this
-- row tracks its lifecycle and verdict. No credential columns — the review
-- session talks to the gateway alias, which owns the CredentialHandle.
CREATE TABLE IF NOT EXISTS review (
  id                        TEXT PRIMARY KEY,        -- Nestra uuid
  agent_id                  TEXT NOT NULL,           -- 'pi-cli'
  reviewed_session_provider TEXT NOT NULL,
  reviewed_session_id       TEXT NOT NULL,           -- the impl session
  review_session_id         TEXT,                    -- null until known
  review_session_provider   TEXT,
  status                    TEXT NOT NULL,           -- pending|reviewing|verdict|failed|aborted
  review_role               TEXT,                    -- 'pi:reviewer'
  reviewer_endpoint_id      TEXT,
  reviewer_model            TEXT,
  task_id                   TEXT,                    -- link to orchestration task (later)
  context_pack_json         TEXT,
  verdict_summary           TEXT,
  verdict_status            TEXT,                    -- pass|changes_requested|fail (later)
  artifact_path             TEXT,                    -- .nestra/reviews/<id>/context.md
  created_at                INTEGER NOT NULL,
  finished_at               INTEGER
);
CREATE INDEX IF NOT EXISTS idx_review_session ON review(reviewed_session_provider, reviewed_session_id);

-- Queryable model index: every (endpoint, model) pair the router can consider,
-- with its merged `ModelAbilities` payload. Built by
-- orchestration/capability_registry.rs from `provider_endpoint.model_abilities_json`
-- + the models.dev cache.
CREATE TABLE IF NOT EXISTS model_catalog (
  endpoint_id   TEXT NOT NULL,
  model_id      TEXT NOT NULL,
  abilities_json TEXT NOT NULL,                -- serialized ModelAbilities
  PRIMARY KEY (endpoint_id, model_id),
  FOREIGN KEY (endpoint_id) REFERENCES provider_endpoint(id) ON DELETE CASCADE
);
"#;

/// Build the canonical v1 schema into `conn` and stamp it. Idempotent: every
/// statement uses `CREATE TABLE IF NOT EXISTS`, so this is safe to call on an
/// already-v1 database. The stamp is upserted.
pub fn build_v1(conn: &Connection) -> AppResult<()> {
    let now = chrono::Utc::now().timestamp_millis();
    conn.execute_batch(SCHEMA_V1)?;
    // `CREATE TABLE IF NOT EXISTS` never backfills a column that was added to
    // the canonical DDL after an install was created. Close that gap
    // idempotently for the one column that has drifted (see the test below).
    ensure_column(conn, "mcp_server", "disabled_agents", "TEXT NOT NULL DEFAULT '[]'")?;
    // Per-binding Direct-wire override added with the protocol picker.
    ensure_column(conn, "agent_provider_binding", "protocol", "TEXT")?;
    // Streaming-usage capture (Smart Gateway fix 1): tool-call count observed
    // on the SSE relay, backfilled after the stream ends.
    ensure_column(conn, "route_request", "tool_calls", "INTEGER")?;
    // P1-1 tool-usage stats: per-tool-name invocation counts (observed on the
    // SSE relay AND the buffered path).
    ensure_column(conn, "route_request", "tool_names", "TEXT")?;
    // The `ProviderKind::Openrouter` variant was removed — OpenRouter now
    // binds through anthropic/openai rows like any OpenAI-compatible provider.
    // Normalize any legacy `openrouter` protocol rows to `openai` (they were
    // wire-equivalent: same `/v1/chat/completions` path, Bearer auth).
    conn.execute_batch(
        "UPDATE endpoint_protocol SET protocol = 'openai-comp' WHERE protocol = 'openrouter';",
    )?;
    // Drop indexes removed from the canonical DDL (redundant with PKs, or
    // superseded by a composite). `CREATE INDEX IF NOT EXISTS` in SCHEMA_V1
    // creates the replacements; these DROPs clear the legacy ones from
    // pre-existing databases. Idempotent.
    conn.execute_batch(
        "DROP INDEX IF EXISTS idx_session_message;\
         DROP INDEX IF EXISTS idx_session_part;\
         DROP INDEX IF EXISTS idx_agent_provider_binding_agent;\
         DROP INDEX IF EXISTS idx_route_request_task;",
    )?;
    conn.execute(
        "INSERT INTO schema_version (version, applied_at) VALUES (?1, ?2)
         ON CONFLICT(version) DO UPDATE SET applied_at = excluded.applied_at",
        rusqlite::params![SCHEMA_VERSION, now],
    )?;
    Ok(())
}

/// Add a column to an existing table only when it's missing. Guards against
/// schema drift between the canonical DDL and pre-existing v1 databases
/// (e.g. `mcp_server.disabled_agents`, added after some installs were made).
fn ensure_column(
    conn: &Connection,
    table: &str,
    column: &str,
    ddl: &str,
) -> AppResult<()> {
    let has: i64 = conn.query_row(
        "SELECT count(*) FROM pragma_table_info(?1) WHERE name = ?2",
        rusqlite::params![table, column],
        |r| r.get(0),
    )?;
    if has == 0 {
        conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {ddl}"))?;
    }
    Ok(())
}

/// Current on-disk schema version, or `None` when the DB is empty (no
/// `schema_version` table at all — a brand-new install).
fn on_disk_version(conn: &Connection) -> AppResult<Option<i32>> {
    let has_table: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master
         WHERE type = 'table' AND name = 'schema_version'",
        [],
        |r| r.get(0),
    )?;
    if has_table == 0 {
        return Ok(None);
    }
    let v: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    // 0 means the table exists but has no rows — treat as empty/new.
    Ok((v != 0).then_some(v as i32))
}

/// Schema migrator. Logic:
///   - empty DB (no `schema_version` table) → [`build_v1`].
///   - already at the canonical v1 → idempotent rebuild (cheap no-op).
///   - any other version → error. This is the fresh-data policy: a pre-release
///     build's data directory is NOT migrated. The caller must surface this as
///     a clear message and exit without modifying the database or starting
///     workers.
///
/// When the next additive schema change ships (v2), a `Some(v) if v >= 2` arm
/// goes here to apply guarded `ALTER` blocks.
pub fn migrate(conn: &Connection) -> AppResult<()> {
    match on_disk_version(conn)? {
        None => build_v1(conn),
        Some(SCHEMA_VERSION) => {
            // Already at v1. `build_v1` is idempotent (CREATE TABLE IF NOT
            // EXISTS), so re-running is a cheap no-op that also backfills any
            // table added in a patch release without bumping the version.
            build_v1(conn)
        }
        // A pre-release or unrecognized database. Refuse to guess: do NOT
        // migrate, do NOT modify, do NOT start the app against it. The caller
        // shows a clear message and exits safely.
        Some(other) => Err(AppError::Internal(format!(
            "This data directory is from a pre-release build of Nestra (database schema version {other}). \
             Public v0.1.0 does not migrate pre-release data. Back up and remove this directory, then relaunch:\n  {}",
            crate::db::data_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "<Nestra data dir>".into())
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The canonical schema must build cleanly on an empty DB and be idempotent.
    #[test]
    fn build_v1_is_clean_and_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        build_v1(&conn).unwrap();
        // Re-running is a no-op (every statement is IF NOT EXISTS).
        build_v1(&conn).unwrap();
        let v: i32 = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }

    /// A fresh DB (no schema_version table) reports `None`, and migrate()
    /// builds the canonical schema for it.
    #[test]
    fn migrate_empty_db_builds_v1() {
        let conn = Connection::open_in_memory().unwrap();
        assert!(on_disk_version(&conn).unwrap().is_none());
        migrate(&conn).unwrap();
        let v: i32 = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        // Orchestration tables exist.
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='route_request'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    /// Drift repair: a pre-existing v1 DB whose `mcp_server` predates the
    /// `disabled_agents` column must get it backfilled by `build_v1`, with the
    /// existing rows surviving and defaulting to `'[]'`.
    #[test]
    fn build_v1_backfills_mcp_disabled_agents_on_drifted_db() {
        let conn = Connection::open_in_memory().unwrap();
        // Simulate the drifted install: an `mcp_server` table without the
        // tri-state `disabled_agents` column, plus one managed row.
        conn.execute_batch(
            "CREATE TABLE mcp_server (
                id             TEXT PRIMARY KEY,
                name           TEXT NOT NULL,
                transport_json TEXT NOT NULL,
                enabled_agents TEXT NOT NULL DEFAULT '[]',
                created_at     INTEGER NOT NULL
             );
             INSERT INTO mcp_server (id, name, transport_json, created_at)
             VALUES ('s1', 'codegraph', '{}', 0);",
        )
        .unwrap();

        build_v1(&conn).unwrap();

        // The column now exists...
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM pragma_table_info('mcp_server') WHERE name='disabled_agents'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "disabled_agents must be backfilled onto a drifted table");
        // ...the existing row survives with the default value...
        let v: String = conn
            .query_row(
                "SELECT disabled_agents FROM mcp_server WHERE id='s1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, "[]");
        // ...and re-running is a no-op (no duplicate column error).
        build_v1(&conn).unwrap();
    }

    #[test]
    fn build_v1_backfills_route_request_tool_calls_on_drifted_db() {
        let conn = Connection::open_in_memory().unwrap();
        // Simulate the drifted install: a v1 `route_request` without the
        // streaming `tool_calls` column (every other column present, so the
        // canonical DDL's index statements still apply — only `ensure_column`
        // can add the missing one).
        conn.execute_batch(
            "CREATE TABLE route_request (
                request_id        TEXT PRIMARY KEY,
                task_id           TEXT NOT NULL,
                agent_id          TEXT NOT NULL,
                logical_session   TEXT,
                subagent_role     TEXT,
                role_source       TEXT,
                requested_model   TEXT,
                requested_provider TEXT,
                resolved_endpoint_id TEXT,
                resolved_model    TEXT,
                protocol          TEXT,
                route_reason      TEXT NOT NULL,
                http_status       INTEGER,
                usage_input       INTEGER,
                usage_output      INTEGER,
                cache_creation    INTEGER,
                cache_read        INTEGER,
                generation_broken INTEGER NOT NULL DEFAULT 0,
                started_at        INTEGER NOT NULL,
                ended_at          INTEGER
             );",
        )
        .unwrap();

        build_v1(&conn).unwrap();

        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM pragma_table_info('route_request') WHERE name='tool_calls'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "tool_calls must be backfilled onto a drifted table");
        // Re-running is a no-op (no duplicate column error).
        build_v1(&conn).unwrap();
    }

    /// P1-1 dual-path verification: `tool_names` round-trips on a FRESH db
    /// (full canonical schema) and on an EXISTING db upgraded through
    /// `ensure_column` — insert + read back in both cases.
    #[test]
    fn tool_names_round_trips_on_fresh_and_upgraded_dbs() {
        for upgraded in [false, true] {
            let conn = Connection::open_in_memory().unwrap();
            if upgraded {
                // Existing install: the v1 table WITHOUT the P1-1 columns,
                // then build_v1's ensure_column upgrades it in place.
                conn.execute_batch(
                    "CREATE TABLE route_request (
                        request_id        TEXT PRIMARY KEY,
                        task_id           TEXT NOT NULL,
                        agent_id          TEXT NOT NULL,
                        logical_session   TEXT,
                        subagent_role     TEXT,
                        role_source       TEXT,
                        requested_model   TEXT,
                        requested_provider TEXT,
                        resolved_endpoint_id TEXT,
                        resolved_model    TEXT,
                        protocol          TEXT,
                        route_reason      TEXT NOT NULL,
                        http_status       INTEGER,
                        usage_input       INTEGER,
                        usage_output      INTEGER,
                        cache_creation    INTEGER,
                        cache_read        INTEGER,
                        tool_calls        INTEGER,
                        generation_broken INTEGER NOT NULL DEFAULT 0,
                        started_at        INTEGER NOT NULL,
                        ended_at          INTEGER
                     );",
                )
                .unwrap();
            }
            build_v1(&conn).unwrap();

            let rec = crate::orchestration::identity::RouteRecord {
                request_id: uuid::Uuid::new_v4(),
                task_id: uuid::Uuid::new_v4(),
                agent_id: "pi-cli".into(),
                logical_session: None,
                subagent_role: None,
                role_source: None,
                requested_model: None,
                requested_provider: None,
                resolved_endpoint_id: None,
                resolved_model: None,
                protocol: None,
                route_reason: "explicit".into(),
                http_status: None,
                usage_input: None,
                usage_output: None,
                cache_creation: None,
                cache_read: None,
                tool_calls: Some(2),
                tool_names: Some(r#"{"mcp__fs__read": 2}"#.into()),
                generation_broken: false,
                started_at: 1,
                ended_at: None,
            };
            // route_request.task_id has an FK to task(id).
            conn.execute(
                "INSERT INTO task (id, lifecycle, started_at) VALUES (?1,'born',0)",
                rusqlite::params![rec.task_id.to_string()],
            )
            .unwrap();
            crate::orchestration::store::insert_route_request(&conn, &rec).unwrap();
            let got = crate::orchestration::store::route_history_for_task(
                &conn,
                &rec.task_id.to_string(),
            )
            .unwrap();
            assert_eq!(got.len(), 1, "upgraded={upgraded}");
            assert_eq!(got[0].tool_calls, Some(2));
            assert_eq!(got[0].tool_names.as_deref(), Some(r#"{"mcp__fs__read": 2}"#));
        }
    }

    #[test]
    fn build_v1_recreates_handoff_table_on_pre_handoff_db() {
        let conn = Connection::open_in_memory().unwrap();
        build_v1(&conn).unwrap();
        // Simulate a pre-handoff install (0.1.1) — no `handoff` table.
        conn.execute_batch("DROP TABLE handoff;").unwrap();
        build_v1(&conn).unwrap();
        // The table is back and accepts a row (patch-release additive backfill;
        // new tables ride the `CREATE TABLE IF NOT EXISTS` in the batch).
        conn.execute(
            "INSERT INTO handoff (id, source_provider, source_session_id, artifact_path,
                                  sections_json, created_at)
             VALUES ('h1','pi-cli','s1','/tmp/h1.md','{}',0)",
            [],
        )
        .unwrap();
        let n: i64 = conn
            .query_row("SELECT count(*) FROM handoff", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn build_v1_recreates_review_table_on_pre_review_db() {
        let conn = Connection::open_in_memory().unwrap();
        build_v1(&conn).unwrap();
        conn.execute_batch("DROP TABLE review;").unwrap();
        build_v1(&conn).unwrap();
        conn.execute(
            "INSERT INTO review (id, agent_id, reviewed_session_provider, reviewed_session_id,
                                 status, created_at)
             VALUES ('r1','pi-cli','pi-cli','s1','pending',0)",
            [],
        )
        .unwrap();
        let n: i64 = conn
            .query_row("SELECT count(*) FROM review", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    /// Re-running migrate on an already-v1 DB is a no-op (data preserved).
    #[test]
    fn migrate_on_v1_is_noop() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute(
            "INSERT INTO provider_endpoint (id, kind, display_name, has_api_key, status)
             VALUES ('ep-x','custom','X',0,'unvalidated')",
            [],
        )
        .unwrap();
        // Second migrate must not drop the row.
        migrate(&conn).unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM provider_endpoint WHERE id='ep-x'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    /// Credential-boundary guard: no orchestration table may carry a column
    /// whose name suggests a secret. This is a static check over the canonical
    /// DDL so a future contributor cannot accidentally add one without
    /// updating this test.
    #[test]
    fn no_secret_columns_in_orchestration_tables() {
        let forbidden_substrings = ["key", "secret", "credential", "token", "password", "passwd"];
        let orchestration_tables = [
            "routing_policy",
            "task",
            "route_request",
            "route_migration",
            "model_catalog",
        ];
        // Parse column names out of SCHEMA_V1 for the orchestration tables.
        for table in orchestration_tables {
            let block = extract_table_block(SCHEMA_V1, table);
            assert!(!block.is_empty(), "table {table} missing from SCHEMA_V1");
            for line in block.lines() {
                let trimmed = line.trim();
                if !trimmed.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
                    continue; // not a column line
                }
                let col_name = trimmed
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_');
                if col_name.is_empty() {
                    continue;
                }
                let lower = col_name.to_ascii_lowercase();
                for bad in forbidden_substrings {
                    assert!(
                        !lower.contains(bad),
                        "orchestration table {table} column {col_name} contains forbidden substring {bad:?} (credential boundary)"
                    );
                }
            }
        }
    }

    /// Crude extraction of one `CREATE TABLE` block by table name. Returns the
    /// inner column-definition lines (between the parens), not including the
    /// `PRIMARY KEY` / `FOREIGN KEY` / `CREATE INDEX` lines.
    fn extract_table_block(ddl: &str, table: &str) -> String {
        let needle = format!("CREATE TABLE IF NOT EXISTS {table} ");
        let start = match ddl.find(&needle) {
            Some(i) => i,
            None => return String::new(),
        };
        let after_open = match ddl[start..].find('(') {
            Some(i) => start + i + 1,
            None => return String::new(),
        };
        // find the matching close paren at depth 0
        let mut depth = 1isize;
        let mut end = after_open;
        for (i, c) in ddl[after_open..].char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = after_open + i;
                        break;
                    }
                }
                _ => {}
            }
        }
        ddl[after_open..end].to_string()
    }

    /// Fresh-data policy: a pre-release database (any version other than 1) is
    /// refused. migrate() must return an error, must NOT modify the database,
    /// and the original tables/rows must be intact afterwards.
    #[test]
    fn pre_release_database_is_refused_and_left_intact() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_version (version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL);
             CREATE TABLE provider_endpoint (id TEXT PRIMARY KEY, display_name TEXT);
             INSERT INTO schema_version (version, applied_at) VALUES (23, 0);
             INSERT INTO provider_endpoint (id, display_name) VALUES ('ep-1','Main');",
        )
        .unwrap();

        // migrate() must refuse (pre-release version 23).
        let err = migrate(&conn);
        assert!(err.is_err(), "pre-release DB must be refused");

        // The database must be left intact (not modified, not rebuilt).
        let v: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 23, "original schema_version must be untouched");
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM provider_endpoint WHERE id='ep-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "original data must be untouched");
        // The canonical v1 tables must NOT have been created.
        let has_route: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='route_request'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_route, 0, "no canonical tables created on refusal");
    }

    /// Fresh-data policy: a future/unknown version is also refused.
    #[test]
    fn unknown_version_is_refused() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_version (version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL);
             INSERT INTO schema_version (version, applied_at) VALUES (99, 0);",
        )
        .unwrap();
        assert!(migrate(&conn).is_err());
    }

    /// The Sessions-list query must use `idx_session_top_level` — i.e. an
    /// index range scan instead of a full scan + filesort. Pins the index
    /// optimization so a future schema edit can't silently regress it.
    #[test]
    fn session_list_query_uses_top_level_index() {
        let conn = Connection::open_in_memory().unwrap();
        build_v1(&conn).unwrap();
        for i in 0..8 {
            conn.execute(
                "INSERT INTO session
                   (provider, id, title, summary, message_count, source_path,
                    resume_command, started_at, updated_at)
                 VALUES ('p', ?1, 't', '', 1, 'x', 'y', 0, ?2)",
                rusqlite::params![format!("s{i}"), i * 1000],
            )
            .unwrap();
        }
        let plan: String = conn
            .query_row(
                "EXPLAIN QUERY PLAN
                 SELECT provider FROM session WHERE is_subagent = 0
                 ORDER BY updated_at DESC LIMIT 300",
                [],
                |r| r.get(3), // column 3 = detail (col 2 is the "notused" int in modern SQLite)
            )
            .unwrap();
        assert!(
            plan.contains("idx_session_top_level"),
            "expected idx_session_top_level in query plan, got: {plan}"
        );
    }

    /// A pre-optimization database may still carry the old redundant /
    /// superseded indexes. build_v1 must drop them and create the new
    /// composites, idempotently.
    #[test]
    fn build_v1_drops_legacy_indexes_and_creates_new() {
        let conn = Connection::open_in_memory().unwrap();
        build_v1(&conn).unwrap();
        // Simulate a pre-optimization install: re-add the legacy indexes.
        conn.execute_batch(
            "CREATE INDEX idx_session_message ON session_message(provider, session_id, seq);
             CREATE INDEX idx_agent_provider_binding_agent ON agent_provider_binding(agent_id);
             CREATE INDEX idx_route_request_task ON route_request(task_id);",
        )
        .unwrap();
        build_v1(&conn).unwrap();
        let names: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='index'")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        for legacy in ["idx_session_message", "idx_agent_provider_binding_agent", "idx_route_request_task"] {
            assert!(!names.iter().any(|n| n == legacy), "{legacy} should be dropped");
        }
        for keep in [
            "idx_session_top_level",
            "idx_route_request_task_started",
            "idx_route_request_logical_session",
        ] {
            assert!(names.iter().any(|n| n == keep), "{keep} should exist");
        }
    }
}
