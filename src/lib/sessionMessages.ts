import type { SessionMessage } from "../ipc";

// A render-ready item after we merge adjacent tool rows by tool_call_id.
// Order in the list matches the source seq order modulo the tool grouping.
export type RenderItem =
  | { kind: "single"; m: SessionMessage }
  | { kind: "thinking"; m: SessionMessage }
  | { kind: "tool_unpaired"; m: SessionMessage }
  | { kind: "tool_pair"; use: SessionMessage; result?: SessionMessage };

// Collapse runs of `tool` rows into pair-by-id items. Non-tool rows break the
// runs (so a stray user message between a use and its result renders as its
// own card and the tool step becomes unpaired). A run with >1 row per id
// (rare but legal — duplicated tool_use_ids) is rendered as unpaired to
// avoid hiding duplicates.
export function groupRenderItems(messages: SessionMessage[]): RenderItem[] {
  const out: RenderItem[] = [];
  let buf: SessionMessage[] = [];
  const flush = () => {
    if (buf.length === 0) return;
    const byId = new Map<string, SessionMessage[]>();
    const unpaired: SessionMessage[] = [];
    for (const m of buf) {
      const id = m.tool_call_id;
      if (id) {
        const arr = byId.get(id) ?? [];
        arr.push(m);
        byId.set(id, arr);
      } else {
        unpaired.push(m);
      }
    }
    for (const m of unpaired) out.push({ kind: "tool_unpaired", m });
    for (const arr of byId.values()) {
      if (arr.length === 2) {
        // First is use, second is result — the parser emits use before result.
        out.push({ kind: "tool_pair", use: arr[0], result: arr[1] });
      } else {
        for (const m of arr) out.push({ kind: "tool_unpaired", m });
      }
    }
    buf = [];
  };
  for (const m of messages) {
    if (m.role === "tool") {
      buf.push(m);
    } else {
      flush();
      if (m.role === "thinking") out.push({ kind: "thinking", m });
      else out.push({ kind: "single", m });
    }
  }
  flush();
  return out;
}

/// Shared "is this message an error?" parse — the `is_error` flag in the
/// provider metadata blob (used by MetadataBadges and ToolPairRow).
export function isErrorMeta(meta: string | undefined): boolean {
  if (!meta) return false;
  try {
    const v = JSON.parse(meta);
    return !!v && typeof v === "object" && (v as Record<string, unknown>).is_error === true;
  } catch {
    return false;
  }
}
