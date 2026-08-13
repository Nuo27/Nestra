//! Update-check domain — compares the running Cargo version against the
//! latest GitHub Release. Manual-trigger only (no background polling) to
//! respect GitHub's unauthenticated rate limit (60 req/hr per IP) and the
//! app's no-chatter philosophy. The network call runs entirely on the Rust
//! side (via `ureq`), so the webview CSP/capabilities are untouched.
//!
//! Designed so a future `tauri-plugin-updater` integration can replace the
//! body of `updates_check` with `check()/download_and_install()` without
//! changing the `UpdateInfo` IPC shape.

use crate::error::{AppError, AppResult};
use serde::Serialize;

/// GitHub repository hosting the releases the updater queries. Change here
/// (and only here) if the project moves.
const GITHUB_OWNER: &str = "Nuo27";
const GITHUB_REPO: &str = "Nestra";
const TIMEOUT_SECS: u64 = 10;

/// Credential-free projection — safe to cross the IPC boundary. The frontend
/// decides presentation; this carries no secret material.
#[derive(Serialize)]
pub struct UpdateInfo {
    /// Running app version (`env!("CARGO_PKG_VERSION")`), e.g. "0.1.0".
    pub current: String,
    /// Latest release version, leading `v` stripped, e.g. "0.2.0". Empty
    /// when `found` is false (no release published yet).
    pub latest: String,
    /// True only when `latest > current` by semver rules.
    pub has_update: bool,
    /// Whether a published release exists at all. GitHub returns 404 for the
    /// `/releases/latest` endpoint until the first non-draft, non-prerelease
    /// release is published; that's a normal state, surfaced as `found =
    /// false` rather than an error.
    pub found: bool,
    /// `html_url` of the release — opened in the browser to download.
    pub release_url: String,
    /// Release body (markdown) for inline notes display.
    pub notes: String,
    /// ISO-8601 publish timestamp from GitHub.
    pub published_at: String,
}

/// Check GitHub Releases for a newer version. Runs the blocking `ureq` GET
/// on the Tauri thread pool via `run_blocking` so the UI thread never stalls.
#[tauri::command]
pub async fn updates_check() -> AppResult<UpdateInfo> {
    super::run_blocking(|| {
        let current = env!("CARGO_PKG_VERSION").to_string();
        let current_v = semver::Version::parse(&current)
            .map_err(|e| AppError::Internal(format!("parse current version: {e}")))?;

        let url = format!(
            "https://api.github.com/repos/{}/{}/releases/latest",
            GITHUB_OWNER, GITHUB_REPO
        );
        // GitHub's API requires a User-Agent header (403 otherwise) and
        // recommends the +json accept alias. No credentials are sent, so
        // following redirects is safe here (unlike the quota fetcher, which
        // pins redirects(0) to avoid leaking x-api-key cross-host).
        let agent = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
            .redirects(5)
            .build();
        let resp: serde_json::Value = match agent
            .get(&url)
            .set("User-Agent", &format!("Nestra/{current} (dev.nestra.app)"))
            .set("Accept", "application/vnd.github+json")
            .call()
        {
            Ok(r) => r.into_json()
                .map_err(|e| AppError::Http(format!("parse release json: {e}")))?,
            // GitHub returns 404 until the first published release exists —
            // a normal state, not a failure. Surface it as `found = false`.
            Err(ureq::Error::Status(404, _)) => {
                return Ok(UpdateInfo {
                    current,
                    latest: String::new(),
                    has_update: false,
                    found: false,
                    release_url: String::new(),
                    notes: String::new(),
                    published_at: String::new(),
                });
            }
            Err(e) => return Err(AppError::Http(format!("github releases: {e}"))),
        };

        let tag = resp
            .get("tag_name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Http("response missing tag_name".into()))?;
        let latest = tag.trim_start_matches('v').to_string();
        let release_url = resp
            .get("html_url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let notes = resp
            .get("body")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let published_at = resp
            .get("published_at")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Fail-safe: an unparseable tag never claims an update.
        let has_update = match semver::Version::parse(&latest) {
            Ok(latest_v) => latest_v > current_v,
            Err(_) => false,
        };

        Ok(UpdateInfo {
            current,
            latest,
            has_update,
            found: true,
            release_url,
            notes,
            published_at,
        })
    })
    .await
}
