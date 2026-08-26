//! Provider-switch config writers.
//!
//! Each AI Coding CLI keeps its provider config in a different file format.
//! A `ConfigWriter` knows how to express "use this Provider" in its CLI's
//! global config file, and how to revert the change.
//!
//! Semantics (see PLAN.md §Provider switching):
//! - First `apply` copies the current config to `<config>.nestra-backup`.
//!   The backup is taken once and never overwritten by later switches.
//! - `apply` rewrites only the block Nestra owns; the rest of the file is
//!   preserved as much as the format allows.
//! - `restore` copies the backup back over the live config and removes the
//!   backup marker, returning the user to the pre-Nestra state.
//!
//! All writer methods take an explicit `config_path` so they are testable
//! against a tempdir. Production callers resolve the path from the home dir.

use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const BACKUP_SUFFIX: &str = ".nestra-backup";
/// Sentinel written as the backup when no original config existed, so restore
/// knows to delete the live file rather than overwrite it with empty content.
pub const NO_ORIGINAL_SENTINEL: &str = "\x00NESTRA_NO_ORIGINAL";
/// Factory Configuration snapshot —a permanent copy of the config as it was
/// the first time Nestra began managing a CLI. Captured on first enable (or
/// lazily on first switch for rows that shipped already-enabled), preserved
/// across switches and restores, and only overwritten by an explicit user
/// action. Distinct from `BACKUP_SUFFIX` (the one-shot per-switch revert that
/// `restore_from_backup` consumes).
pub const FACTORY_SUFFIX: &str = ".nestra-factory";

/// LLM endpoint protocol a Provider speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Anthropic,
    Openai,
    /// OpenAI Responses API (`/v1/responses`) — the official dialect for
    /// xAI-family models (grok-4.5, gpt-5.6-luna). Used as the gateway's
    /// upstream wire; no agent config writes a responses row today (the
    /// binding flow lands on anthropic/openai rows), so `accepts()` lists
    /// don't include it.
    Responses,
    /// User-defined OpenAI-compatible endpoint.
    Custom,
}

impl ProviderKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::Openai => "openai-comp",
            ProviderKind::Responses => "response-api",
            ProviderKind::Custom => "custom",
        }
    }
}

/// Model selection carried by a Provider, shape depends on protocol.
#[derive(Debug, Clone)]
pub enum ModelsConfig {
    /// anthropic-protocol: three Claude Code tiers, each a model id.
    /// Tiers may repeat (e.g. sonnet == opus).
    Anthropic {
        default: String,
        haiku: String,
        sonnet: String,
        opus: String,
    },
    /// openai / custom: one default + the curated available list.
    Openai {
        default: String,
        available: Vec<String>,
    },
}

impl ModelsConfig {
    /// The provider's primary model id — written as the `default` whenever a
    /// higher-level writer needs a single string (default model field, model
    /// picker default, …).
    pub fn default_model(&self) -> &str {
        match self {
            ModelsConfig::Anthropic { default, .. } => default,
            ModelsConfig::Openai { default, .. } => default,
        }
    }

    /// Every distinct model id this provider exposes (deduped, order
    /// preserved). Used to subset the global models.dev ability index down
    /// to the ids a single provider writes.
    pub fn ids(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let push = |out: &mut Vec<String>, id: &str| {
            if !id.is_empty() && !out.iter().any(|e| e == id) {
                out.push(id.to_string());
            }
        };
        match self {
            ModelsConfig::Anthropic { default, haiku, sonnet, opus } => {
                for id in [default.as_str(), haiku.as_str(), sonnet.as_str(), opus.as_str()] {
                    push(&mut out, id);
                }
            }
            ModelsConfig::Openai { default, available } => {
                push(&mut out, default);
                for id in available {
                    push(&mut out, id);
                }
            }
        }
        out
    }
}

pub(crate) fn default_model_of(m: &ModelsConfig) -> String {
    m.default_model().to_string()
}

/// Everything a writer needs to render "use this Provider".
#[derive(Clone)]
pub struct SwitchContext {
    pub provider_id: String,
    pub provider_kind: ProviderKind,
    pub display_name: String,
    pub base_url: String,
    pub api_key: String,
    pub models: ModelsConfig,
    /// Extra env keys (per-provider advanced env). Applied verbatim by
    /// env-style writers (Claude Code). Stringified on write.
    pub advanced_env: serde_json::Map<String, serde_json::Value>,
    /// Per-model ability data (keyed by the provider's own model id)
    /// sourced from models.dev. **Only the OpenCode adapter reads this**;
    /// other writers ignore it. Empty when offline/unmatched.
    pub model_abilities: HashMap<String, crate::model_abilities::ModelAbilities>,
}

// Manual Debug: the struct carries the plaintext API key — a derived impl
// would leak it into logs/panic messages. Mirrors GatewayAlias's redaction
// of `sentinel_key`.
impl std::fmt::Debug for SwitchContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SwitchContext")
            .field("provider_id", &self.provider_id)
            .field("provider_kind", &self.provider_kind)
            .field("display_name", &self.display_name)
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .field("models", &self.models)
            .field("advanced_env", &self.advanced_env)
            .field("model_abilities", &self.model_abilities)
            .finish()
    }
}

/// The full provider set Nestra will write into the agent's config file.
/// Order is meaningful: it determines iteration order on disk and which
/// provider/model wins when more than one entry could satisfy a default
/// pick. The first entry is treated as the user's primary choice — adapters
/// that surface a `defaultProvider`/`defaultModel` pair (Pi, OpenCode) will
/// point those at entry 0.
#[derive(Debug, Clone)]
pub struct ProviderSet {
    pub entries: Vec<SwitchContext>,
    pub default_provider_id: String,
    pub default_model: String,
}

pub trait ConfigAdapter: Send + Sync {
    /// Wire protocols this adapter can inject. An adapter that writes
    /// Anthropic-style env vars accepts `anthropic`; a TOML openai-style
    /// writer accepts `openai`/`openrouter`/`custom`. This is a property of
    /// the *file format*, not the agent - the AGENTS registry reuses an
    /// adapter key across related agents.
    fn accepts(&self) -> &'static [ProviderKind];

    /// How this format surfaces model selection: tiered (Claude Code) vs a
    /// flat default + available list (everyone else). Drives the model
    /// editor shape on the wire.
    fn model_selection(&self) -> ModelSelection;

    /// Single-slot writers (Claude Code) — write the active provider block
    /// and return whether a fresh backup was captured. Default delegates to
    /// `apply_set` for backwards compatibility.
    fn apply(&self, config_path: &Path, ctx: &SwitchContext) -> AppResult<bool> {
        self.apply_set(
            config_path,
            &ProviderSet {
                entries: vec![ctx.clone()],
                default_provider_id: ctx.provider_id.clone(),
                default_model: default_model_of(&ctx.models),
            },
        )
    }

    /// Multi-slot writers (Pi, OpenCode) — atomically rewrite the full
    /// provider set Nestra owns (plus the default provider/model pointer).
    /// Returns whether a fresh backup was captured.
    fn apply_set(&self, config_path: &Path, set: &ProviderSet) -> AppResult<bool>;

    /// Revert to the pre-Nestra original.
    fn restore(&self, config_path: &Path) -> AppResult<()>;

    /// Probe the config file for provider entries the user configured directly
    /// in the agent (not via Nestra) — these are invisible to the binding
    /// table but live on disk. `managed` marks the entries Nestra wrote
    /// (`nestra-*`). Returns empty when this format has no provider map.
    fn inspect(&self, config_path: &Path) -> AppResult<Vec<DetectedProvider>> {
        let _ = config_path;
        Ok(Vec::new())
    }

    /// Remove a single detected provider key from the config file without
    /// touching anything else. Unsupported formats return an error.
    fn remove(&self, config_path: &Path, key: &str) -> AppResult<()> {
        let _ = (config_path, key);
        Err(AppError::Validation(
            "provider removal not supported for this config format".into(),
        ))
    }

    /// Additional config files this adapter manages alongside the primary
    /// `config_path`. Used by agents that split config across multiple files
    /// (e.g. Pi stores keys in a sibling `auth.json`). The backup/factory/
    /// restore machinery iterates over these so every file stays consistent.
    fn extra_config_paths(&self, _config_path: &Path) -> Vec<PathBuf> {
        Vec::new()
    }

    /// Gateway-mode write: instead of the real upstream URL + key, write a
    /// STABLE alias so the agent points at the Nestra gateway
    /// (`http://127.0.0.1:<port>`). The router then resolves the real
    /// provider/model at request time, and switching the resolved route no
    /// longer rewrites the agent's config file (preserving session/cache).
    ///
    /// `alias` carries the gateway base URL + a stable model alias (the agent
    /// sends this; the gateway ignores it and resolves the real model
    /// per-task) + the loopback token for the agent's key slot. The alias's
    /// abilities describe the steady-state model so writers can advertise the
    /// real context window.
    ///
    /// Default: not supported. Agents that opt into the gateway override this
    /// (Claude Code, OpenCode Desktop, Pi).
    fn apply_gateway_set(&self, _config_path: &Path, _alias: &GatewayAlias) -> AppResult<bool> {
        Err(AppError::Validation(
            "gateway mode not supported for this config format".into(),
        ))
    }
}

/// One alias model slot: the stable id the agent sends + the abilities of the
/// steady-state model the router resolves for that slot. Writers use the
/// abilities to advertise the REAL context/output window to the agent (a bare
/// alias makes every agent default to a 200k guess); `abilities: None` →
/// per-format conservative placeholders.
#[derive(Clone, Debug)]
pub struct AliasModel {
    pub id: String,
    pub abilities: Option<crate::model_abilities::ModelAbilities>,
}

/// Claude Code's three tier slots (`ANTHROPIC_DEFAULT_{HAIKU,SONNET,OPUS}_MODEL`).
/// Distinct ids per tier let the gateway classify tier intent from the request
/// body and route via `tier:*` policy rows (e.g. background haiku-tier traffic
/// to a cheaper endpoint). Only the Claude Code writer consumes these.
#[derive(Clone, Debug)]
pub struct TierAliases {
    pub haiku: AliasModel,
    pub sonnet: AliasModel,
    pub opus: AliasModel,
}

/// Parameters for [`ConfigAdapter::apply_gateway_set`]. The agent is written a
/// stable alias so its config file never changes when the resolved
/// provider/model does — only the Nestra-internal route moves.
///
/// `model_alias.abilities` carries the steady-state model's abilities so the
/// agent's config advertises the real context window (see [`AliasModel`]).
///
/// `sentinel_key` carries the gateway's loopback auth token (written into the
/// agent's key slot so the gateway can authenticate the inbound request). It is
/// a real secret, so the derived `Debug` redacts it — the value must never
/// appear in a log, panic backtrace, or `dbg!` output.
#[derive(Clone)]
pub struct GatewayAlias {
    /// The gateway's loopback base URL (`http://127.0.0.1:<port>`), already
    /// agent-prefixed. Written wherever the agent's `base_url`/
    /// `ANTHROPIC_BASE_URL`/etc. goes.
    pub gateway_base_url: String,
    /// Primary model alias slot. Written into the agent's model field; the
    /// gateway ignores the agent-stated model and resolves the real one
    /// per-task, so the id is arbitrary but must be stable across writes (the
    /// convention is `"nestra"`; Claude Code uses a real CC id because it
    /// validates model names locally).
    pub model_alias: AliasModel,
    /// Per-tier alias slots (Claude Code only). `None` for single-alias agents.
    pub tier_aliases: Option<TierAliases>,
    /// Sentinel API key written into the agent's key slot. The gateway holds
    /// the real credential; the agent's value is never sent upstream.
    pub sentinel_key: String,
}

impl GatewayAlias {
    /// Single-alias construction — no tiers, no abilities (the shape every
    /// non-Claude agent uses; writers render conservative placeholders).
    pub fn simple(
        gateway_base_url: impl Into<String>,
        model_alias: impl Into<String>,
        sentinel_key: impl Into<String>,
    ) -> Self {
        Self {
            gateway_base_url: gateway_base_url.into(),
            model_alias: AliasModel { id: model_alias.into(), abilities: None },
            tier_aliases: None,
            sentinel_key: sentinel_key.into(),
        }
    }
}

impl std::fmt::Debug for GatewayAlias {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayAlias")
            .field("gateway_base_url", &self.gateway_base_url)
            .field("model_alias", &self.model_alias.id)
            .field(
                "tier_aliases",
                &self.tier_aliases.as_ref().map(|t| {
                    format!(
                        "haiku={}, sonnet={}, opus={}",
                        t.haiku.id, t.sonnet.id, t.opus.id
                    )
                }),
            )
            .field("sentinel_key", &"<redacted>")
            .finish()
    }
}

/// A provider entry found in a CLI's config file. `managed` is true for the
/// `nestra-*` keys Nestra owns; everything else is user-configured and fair
/// game for a targeted delete.
#[derive(Debug, Clone, Serialize)]
pub struct DetectedProvider {
    pub key: String,
    pub display_name: String,
    pub managed: bool,
}

/// How a CLI surfaces model selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelSelection {
    /// Three Anthropic tiers (haiku/sonnet/opus) plus a default model.
    AnthropicTiers,
    /// Free-form list of model ids per provider.
    FreeForm,
}

// ---- backup helpers (shared by all writers) ----

pub fn backup_path_for(config_path: &Path) -> PathBuf {
    let mut s = config_path.as_os_str().to_os_string();
    s.push(BACKUP_SUFFIX);
    PathBuf::from(s)
}

/// Copy `config_path` to its backup location, once. Returns true if a backup
/// was created this call, false if one already existed.
pub fn ensure_backup(config_path: &Path) -> AppResult<bool> {
    let backup = backup_path_for(config_path);
    if backup.exists() {
        return Ok(false);
    }
    if config_path.exists() {
        std::fs::copy(config_path, &backup)?;
    } else {
        // No original existed —record that so restore deletes the live file.
        std::fs::write(&backup, NO_ORIGINAL_SENTINEL)?;
    }
    Ok(true)
}

/// Restore the live config from its backup and remove the backup marker.
pub fn restore_from_backup(config_path: &Path) -> AppResult<()> {
    let backup = backup_path_for(config_path);
    if !backup.exists() {
        return Err(AppError::NotFound(
            "no nestra backup to restore".to_string(),
        ));
    }
    // Read the backup as BYTES — `read_to_string` made a non-UTF-8 original
    // unrecoverable (the byte-identical copy in `ensure_backup` could never
    // be written back). Compare bytes so the sentinel still matches.
    let content = std::fs::read(&backup)?;
    if content == NO_ORIGINAL_SENTINEL.as_bytes() {
        // Original did not exist; remove the file Nestra created. Guard
        // against deleting a file the user has since hand-edited: only
        // remove when the live content still looks Nestra-written (empty
        // or a bare `{}` shell); otherwise keep it and warn loudly.
        if config_path.exists() {
            let live = std::fs::read(config_path).unwrap_or_default();
            let nestra_shell =
                live.is_empty() || live.iter().all(|b| b.is_ascii_whitespace()) || live == b"{}";
            if nestra_shell {
                std::fs::remove_file(config_path)?;
            } else {
                tracing::warn!(
                    path = %config_path.display(),
                    "restore: backup says 'no original', but the live file was modified since Nestra created it — keeping it"
                );
            }
        }
    } else {
        atomic_write(config_path, &content)?;
    }
    std::fs::remove_file(&backup)?;
    Ok(())
}

// ---- Factory Configuration (permanent pre-Nestra snapshot) ----

pub fn factory_path_for(config_path: &Path) -> PathBuf {
    let mut s = config_path.as_os_str().to_os_string();
    s.push(FACTORY_SUFFIX);
    PathBuf::from(s)
}

/// Capture the current live config as the Factory Configuration. Once-only
/// unless `force`: a re-capture only happens when the user explicitly asks to
/// overwrite the factory snapshot. When no live config exists the sentinel is
/// stored so a future restore knows to delete the file Nestra created
/// rather than write empty bytes over it.
pub fn capture_factory(config_path: &Path, force: bool) -> AppResult<()> {
    let factory = factory_path_for(config_path);
    if !force && factory.exists() {
        return Ok(());
    }
    if config_path.exists() {
        std::fs::copy(config_path, &factory)?;
    } else {
        std::fs::write(&factory, NO_ORIGINAL_SENTINEL)?;
    }
    Ok(())
}

/// Atomically write `data` to `path`: a temp file in the same directory is
/// written, fsynced, then renamed over the target. The live config is never
/// observed half-written — a crash either completes the swap or leaves the
/// previous bytes intact (the temp is removed on failure). `std::fs::rename`
/// atomically replaces a same-directory file on both Unix and Windows, so no
/// extra platform crate is needed. Mirrors cc-switch's atomic-write guarantee.
///
/// The temp name embeds a process-unique sequence so concurrent in-process
/// writers to the same path cannot truncate each other's temp file. On Unix
/// the temp is created with mode 0600 (agent configs carry credentials) and,
/// when the target already exists, the target's own permission bits are
/// re-applied before the rename — so a pre-existing 0600 key file stays 0600
/// instead of silently widening to the umask default.
///
/// The final rename is retried on a short backoff ladder (0/10/50/100ms):
/// on Windows it can fail with a sharing violation while another process
/// (AV scan, an agent re-reading its config) briefly holds the target open.
/// A persistent failure still surfaces to the caller.
pub fn atomic_write(path: &Path, data: &[u8]) -> AppResult<()> {
    use std::io::Write;
    let parent = path
        .parent()
        .ok_or_else(|| AppError::Internal("config path has no parent".into()))?;
    std::fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("nestra-config");
    static TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = parent.join(format!(
        ".{file_name}.nestra-tmp.{}.{seq}",
        std::process::id()
    ));

    let res = (|| -> AppResult<()> {
        let mut f = create_private(&tmp)?;
        // A pre-existing config keeps its own permission bits (e.g. a 0600
        // key file stays 0600); a brand-new file keeps the 0600 default.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(path) {
                let mode = meta.permissions().mode() & 0o7777;
                f.set_permissions(std::fs::Permissions::from_mode(mode))?;
            }
        }
        f.write_all(data)?;
        f.sync_all()?;
        drop(f);
        let mut last_err: Option<std::io::Error> = None;
        for backoff_ms in [0u64, 10, 50, 100] {
            if backoff_ms > 0 {
                std::thread::sleep(std::time::Duration::from_millis(backoff_ms));
            }
            match std::fs::rename(&tmp, path) {
                Ok(()) => return Ok(()),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.expect("rename ladder ran at least once").into())
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

/// Open `path` for writing with mode 0600 (umask-independent) on Unix.
/// Windows has no mode bits; the default ACLs apply.
#[cfg(unix)]
fn create_private(path: &Path) -> AppResult<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    Ok(f)
}

#[cfg(not(unix))]
fn create_private(path: &Path) -> AppResult<std::fs::File> {
    Ok(std::fs::File::create(path)?)
}

// ---- switch transactions ---------------------------------------------------

/// Pre-write bytes of one config file (and whether it existed at all).
type FileSnapshot = (PathBuf, Option<Vec<u8>>);

/// Run `apply_set` as a mini-transaction: the live bytes of `config_path` plus
/// the adapter's [`ConfigAdapter::extra_config_paths`] are snapshotted first;
/// if the write fails midway, every file is restored to its pre-switch state
/// (files the switch created are removed). This is deliberately separate from
/// the per-switch `ensure_backup` inside adapters — that backup holds the
/// pre-NESTRA original, not the pre-SWITCH state, so it cannot undo a failed
/// A→B switch.
pub fn apply_set_atomic(
    adapter: &dyn ConfigAdapter,
    config_path: &Path,
    set: &ProviderSet,
) -> AppResult<bool> {
    let snap = snapshot_for(adapter, config_path);
    match adapter.apply_set(config_path, set) {
        Ok(created) => Ok(created),
        Err(e) => {
            report_rollback(&snap);
            Err(e)
        }
    }
}

/// Gateway-alias variant of [`apply_set_atomic`] — same snapshot/rollback
/// contract around [`ConfigAdapter::apply_gateway_set`].
pub fn apply_gateway_set_atomic(
    adapter: &dyn ConfigAdapter,
    config_path: &Path,
    alias: &GatewayAlias,
) -> AppResult<bool> {
    let snap = snapshot_for(adapter, config_path);
    match adapter.apply_gateway_set(config_path, alias) {
        Ok(created) => Ok(created),
        Err(e) => {
            report_rollback(&snap);
            Err(e)
        }
    }
}

fn snapshot_for(adapter: &dyn ConfigAdapter, config_path: &Path) -> Vec<FileSnapshot> {
    let mut snap = vec![(config_path.to_path_buf(), std::fs::read(config_path).ok())];
    for p in adapter.extra_config_paths(config_path) {
        snap.push((p.clone(), std::fs::read(&p).ok()));
    }
    snap
}

/// Restore a snapshot in reverse order, attempting every file even when one
/// fails (a half-rolled-back switch is still better than none); the first
/// failure is logged, not surfaced — the caller is already returning the
/// original write error.
fn report_rollback(snap: &[FileSnapshot]) {
    for (path, old) in snap.iter().rev() {
        let res = match old {
            Some(bytes) => atomic_write(path, bytes),
            // File did not exist before the switch: remove it, unless the
            // failed write never got around to creating it.
            None => match std::fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(AppError::Io(e)),
            },
        };
        if let Err(e) = res {
            tracing::warn!(path = %path.display(), error = %e, "switch rollback incomplete");
        }
    }
}

#[cfg(test)]
mod tests;
