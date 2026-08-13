import { describe, expect, it } from "vitest";
import { cancellableInvoke, makeGuard } from "./guard";

describe("guard", () => {
  it("returns the value when the call is still current", async () => {
    const g = makeGuard();
    const res = await cancellableInvoke(g, async () => 42);
    expect(res).toEqual({ stale: false, value: 42 });
  });

  it("returns stale when superseded before resolving", async () => {
    const g = makeGuard();
    const p = cancellableInvoke(g, () => new Promise<number>((r) => setTimeout(() => r(1), 10)));
    g.supersede(); // a newer call started / unmount
    expect(await p).toEqual({ stale: true });
  });

  it("swallows a rejection so superseded calls never surface", async () => {
    const g = makeGuard();
    const p = cancellableInvoke(g, async () => {
      throw new Error("boom");
    });
    // No await-throw, no unhandled rejection — reported as stale (with the
    // error attached for debugging).
    expect(await p).toEqual({ stale: true, error: new Error("boom") });
  });

  it("isCurrent reflects generation bumps", () => {
    const g = makeGuard();
    const a = g.start();
    expect(g.isCurrent(a)).toBe(true);
    const b = g.start();
    expect(g.isCurrent(a)).toBe(false);
    expect(g.isCurrent(b)).toBe(true);
    g.supersede();
    expect(g.isCurrent(b)).toBe(false);
  });
});
