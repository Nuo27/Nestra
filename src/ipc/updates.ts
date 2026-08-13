import { invoke } from "@tauri-apps/api/core";

/// Result of `updates_check`. Mirrors the Rust `UpdateInfo` projection —
/// credential-free, safe to hold in UI state.
export interface UpdateInfo {
  /** Running app version, e.g. "0.1.0". */
  current: string;
  /** Latest release version (leading `v` stripped). */
  latest: string;
  /** True only when `latest > current` by semver rules. */
  hasUpdate: boolean;
  /** Whether a published release exists at all (false = 404, none yet). */
  found: boolean;
  /** `html_url` of the release — opened in the browser to download. */
  releaseUrl: string;
  /** Release body (markdown). */
  notes: string;
  /** ISO-8601 publish timestamp from GitHub. */
  publishedAt: string;
}

/// Query GitHub Releases for the latest version and compare against the
/// running app. Throws `AppError` on network/parse failure.
export const updatesCheck = () => invoke<UpdateInfo>("updates_check");
