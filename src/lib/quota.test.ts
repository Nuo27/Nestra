import { describe, expect, it } from "vitest";
import { composeEndpointConfig, planFromSelectValue, planToSelectValue } from "./quota";
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
    // Regression: `quota_refresh_set_settings` replaces the WHOLE blob, so a
    // field omitted here is silently erased server-side (serde default). The
    // workspace ID used to get wiped by any unrelated settings write (keep-
    // alive toggle, plan change, preview windows) — the cookie survived in
    // the keychain but the field in the blob died with no error anywhere.
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
