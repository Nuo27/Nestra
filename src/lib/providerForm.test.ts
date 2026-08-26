import { describe, expect, it } from "vitest";
import { emptyToForm, isDirty } from "./providerForm";
import type { EndpointInfo } from "../ipc";

function endpoint(overrides: Partial<EndpointInfo> = {}): EndpointInfo {
  return {
    id: "test",
    display_name: "Test",
    protocols: [{ protocol: "anthropic", base_url: "https://x" }],
    status: "unvalidated",
    has_api_key: false,
    preset_id: null,
    models: { default: "m1", haiku: null, sonnet: null, opus: null, available: ["m1", "m2"] },
    advanced_env: { B: "1", A: "2" },
    model_abilities: { m1: { limit: { context: 1, output: 2 }, temperature: false } },
    ...overrides,
  } as EndpointInfo;
}

describe("isDirty", () => {
  it("clean form is not dirty", () => {
    const e = endpoint();
    expect(isDirty(emptyToForm(e), e)).toBe(false);
  });

  it("detects models_available edits (the field the old check missed)", () => {
    const e = endpoint();
    const f = { ...emptyToForm(e), models_available: ["m1", "m2", "m3"] };
    expect(isDirty(f, e)).toBe(true);
  });

  it("ignores object key order in advanced_env / model_abilities", () => {
    const e = endpoint();
    const f = emptyToForm(e);
    // Same content, reversed key order — must NOT read as dirty.
    f.advanced_env = { A: "2", B: "1" };
    f.model_abilities = {
      m1: { temperature: false, limit: { output: 2, context: 1 } },
    };
    expect(isDirty(f, e)).toBe(false);
  });

  it("detects real model_abilities changes", () => {
    const e = endpoint();
    const f = emptyToForm(e);
    f.model_abilities = { m1: { limit: { context: 9, output: 2 } } };
    expect(isDirty(f, e)).toBe(true);
  });
});
