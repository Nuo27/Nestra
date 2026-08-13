import { describe, expect, it } from "vitest";
import { formatRelative, formatTime } from "./format";

describe("formatTime", () => {
  it("returns empty for null/0", () => {
    expect(formatTime(null)).toBe("");
    expect(formatTime(0)).toBe("");
  });

  it("formats same-day timestamps as HH:MM", () => {
    const now = new Date();
    const ts = now.getTime();
    expect(formatTime(ts)).toMatch(/^\d{2}:\d{2}$/);
  });

  it("labels yesterday", () => {
    const yesterday = new Date();
    yesterday.setDate(yesterday.getDate() - 1);
    expect(formatTime(yesterday.getTime())).toBe("yesterday");
  });
});

describe("formatRelative", () => {
  it("returns empty for null/0", () => {
    expect(formatRelative(null)).toBe("");
    expect(formatRelative(0)).toBe("");
  });

  it("handles seconds, minutes, hours, days, months, years", () => {
    const now = Date.now();
    expect(formatRelative(now - 30_000)).toBe("30s ago");
    expect(formatRelative(now - 5 * 60_000)).toBe("5m ago");
    expect(formatRelative(now - 3 * 3_600_000)).toBe("3h ago");
    expect(formatRelative(now - 5 * 86_400_000)).toBe("5d ago");
    expect(formatRelative(now - 40 * 86_400_000)).toBe("1mo ago");
    expect(formatRelative(now - 14 * 30 * 86_400_000)).toBe("1y ago");
  });
});