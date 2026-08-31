import { describe, expect, it } from "vitest";
import {
  composeEndpointConfig,
  planFromSelectValue,
  planToSelectValue,
  shouldCatchUpRefresh,
} from "./quota";
import type { RefreshEndpointConfig } from "../ipc";

/// A fully-populated config as the server would return it — including the
/// OpenCode Go workspace ID, which lives in the settings blob.
const FULL: RefreshEndpointConfig = {
  enabled: true,
  protocol: "openai-comp",
  model: "deepseek-v4-flash",
  target_quota_name: "5h-token",
  last_status: "ok",
  check_rate_secs: 180,
  reset_grace_secs: 180,
  extractor: null,
  query_plan: { source: "preset", kind: "opencode_go" },
  provisioned: true,
  preview_windows: ["5h-token"],
  opencode_workspace_id: "ws_abc-123",
};

describe("composeEndpointConfig", () => {
  it("carries opencode_workspace_id through a full-blob write", () => {
    // `quota_refresh_set_settings` replaces the WHOLE blob, so a field
    // omitted here is silently erased server-side (serde default) —
    // including the opencode workspace id.
    const next = composeEndpointConfig({ ...FULL, enabled: false });
    expect(next.opencode_workspace_id).toBe("ws_abc-123");
    expect(next.enabled).toBe(false);
  });

  it("carries null workspace id (explicit clear)", () => {
    const next = composeEndpointConfig({ ...FULL, opencode_workspace_id: null });
    expect(next.opencode_workspace_id).toBeNull();
  });

  it("defaults check/reset grace to 180 when absent", () => {
    const next = composeEndpointConfig({ ...FULL, check_rate_secs: 0, reset_grace_secs: 0 });
    expect(next.check_rate_secs).toBe(180);
    expect(next.reset_grace_secs).toBe(180);
  });

  it("round-trips every persisted field unchanged", () => {
    const next = composeEndpointConfig(FULL);
    expect(next).toEqual(FULL);
  });
});

describe("planToSelectValue / planFromSelectValue", () => {
  it("round-trips the opencode_go built-in through the select value", () => {
    const plan = planFromSelectValue("opencode_go", { source: "none" }, null);
    expect(plan).toEqual({ source: "preset", kind: "opencode_go" });
    expect(planToSelectValue(plan)).toBe("opencode_go");
  });
});

describe("shouldCatchUpRefresh", () => {
  // The single refresh authority for the Quota card. Fires when the absolute
  // deadline has passed, no fetch is in flight, and the last attempt is older
  // than one interval. These tests pin the no-hammer / no-stuck /
  // catch-up-on-resume semantics.
  const base = {
    auto: true,
    isFetching: false,
    now: 100_000,
    lastAttemptAt: 0,
    intervalSec: 10,
  };

  it("fires when the deadline has passed and nothing is in flight", () => {
    expect(shouldCatchUpRefresh({ ...base, nextRefreshAt: 95_000 })).toBe(true);
  });

  it("does not fire before the deadline", () => {
    expect(shouldCatchUpRefresh({ ...base, nextRefreshAt: 110_000 })).toBe(false);
  });

  it("does not fire with no deadline armed", () => {
    expect(shouldCatchUpRefresh({ ...base, nextRefreshAt: 0 })).toBe(false);
  });

  it("does not fire while a fetch is in flight", () => {
    expect(
      shouldCatchUpRefresh({ ...base, nextRefreshAt: 95_000, isFetching: true }),
    ).toBe(false);
  });

  it("does not fire when auto-refresh is off", () => {
    expect(
      shouldCatchUpRefresh({ ...base, nextRefreshAt: 95_000, auto: false }),
    ).toBe(false);
  });

  it("throttles re-attempts to once per interval (failed fetch)", () => {
    // A failed fetch leaves the deadline unchanged, so without the throttle
    // the effect would re-fire on every render — a hammer loop. After an
    // attempt at 96_000, 4s later is still within the 10s interval → no fire.
    expect(
      shouldCatchUpRefresh({ ...base, nextRefreshAt: 95_000, lastAttemptAt: 96_000, now: 100_000 }),
    ).toBe(false);
    // One full interval after the attempt → allowed to retry.
    expect(
      shouldCatchUpRefresh({ ...base, nextRefreshAt: 95_000, lastAttemptAt: 96_000, now: 106_000 }),
    ).toBe(true);
  });

  it("catches up immediately after a long hidden period", () => {
    // Window hidden for hours: `now` jumps far past the deadline (the UI
    // clock re-syncs on focus/visibility regain), and no attempt has happened
    // this session → exactly one catch-up fetch fires on resume.
    expect(
      shouldCatchUpRefresh({ ...base, nextRefreshAt: 50_000, now: 3_600_000 }),
    ).toBe(true);
  });
});
