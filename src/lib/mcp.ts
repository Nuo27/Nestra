import type { McpKind } from "../ipc";

export function transportLabel(kind: McpKind, target: string | null) {
  const k = kind === "stdio" ? "stdio" : kind;
  return `${k} · ${target ?? "…"}`;
}

/// Split a space-separated arg string, honoring double quotes: the naive
/// `split(/\s+/)` broke `--config "a b"` into three args. Unclosed quotes
/// keep the remainder as one (last) arg rather than dropping it.
export function splitArgs(s: string): string[] {
  const out: string[] = [];
  let cur = "";
  let inQuote = false;
  for (const ch of s) {
    if (ch === '"') {
      inQuote = !inQuote;
    } else if (ch === " " || ch === "\t") {
      if (inQuote) {
        cur += ch;
      } else if (cur.length > 0) {
        out.push(cur);
        cur = "";
      }
    } else {
      cur += ch;
    }
  }
  if (cur.length > 0 || inQuote) out.push(cur);
  return out;
}
