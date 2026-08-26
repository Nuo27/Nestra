import { describe, expect, it } from "vitest";
import { quoteArg, splitArgs } from "./mcp";

describe("splitArgs", () => {
  it("splits on runs of whitespace", () => {
    expect(splitArgs("a b\t c")).toEqual(["a", "b", "c"]);
  });

  it("keeps quoted sections as one arg", () => {
    expect(splitArgs('--config "a b" c')).toEqual(["--config", "a b", "c"]);
  });

  it("yields an empty-string arg for explicit empty quotes", () => {
    expect(splitArgs('a "" b')).toEqual(["a", "", "b"]);
  });

  it("honors backslash escapes inside and outside quotes", () => {
    expect(splitArgs(String.raw`--filter "{\"a b\"}"`)).toEqual(['--filter', '{"a b"}']);
    expect(splitArgs(String.raw`a\ b`)).toEqual(["a b"]);
  });

  it("keeps the remainder as the last arg on an unclosed quote", () => {
    expect(splitArgs('a "b c')).toEqual(["a", "b c"]);
  });

  it("round-trips args with whitespace through quoteArg", () => {
    const args = ["--config", "a b", "plain"];
    expect(splitArgs(args.map(quoteArg).join(" "))).toEqual(args);
  });
});
