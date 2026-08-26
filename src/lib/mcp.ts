import type { McpKind } from "../ipc";

export function transportLabel(kind: McpKind, target: string | null) {
  const k = kind === "stdio" ? "stdio" : kind;
  return `${k} · ${target ?? "…"}`;
}

/// Split a space-separated arg string, honoring double quotes and backslash
/// escapes: the naive `split(/\s+/)` broke `--config "a b"` into three args.
/// An explicit `""` yields an empty-string argument (shell semantics), and an
/// unclosed quote keeps the remainder as one (last) arg rather than dropping
/// it.
export function splitArgs(s: string): string[] {
  const out: string[] = [];
  let cur = "";
  let inQuote = false;
  let escaped = false;
  let sawQuote = false;
  for (const ch of s) {
    if (escaped) {
      cur += ch;
      escaped = false;
    } else if (ch === "\\") {
      escaped = true;
    } else if (ch === '"') {
      inQuote = !inQuote;
      sawQuote = true;
    } else if ((ch === " " || ch === "\t") && !inQuote) {
      if (cur.length > 0 || sawQuote) {
        out.push(cur);
        cur = "";
        sawQuote = false;
      }
    } else {
      cur += ch;
    }
  }
  if (cur.length > 0 || inQuote || sawQuote) out.push(cur);
  return out;
}

/// Quote an argument for round-tripping through `splitArgs` when pre-filling
/// an editable args string: anything with whitespace needs double quotes.
export function quoteArg(arg: string): string {
  return /\s/.test(arg) ? `"${arg}"` : arg;
}
