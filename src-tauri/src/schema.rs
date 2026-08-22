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
mod tests;
