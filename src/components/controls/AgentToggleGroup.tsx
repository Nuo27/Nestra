import { useTranslation } from "react-i18next"
import { AgentKindBadge } from "../agents/AgentKindBadge"

/// One agent's toggle inside the group. Unlike `SegmentedControl` (which is
/// single-select), each segment here is an independent on/off — the bordered
/// row gives the radio-group *look*, but the semantics are multi-select
/// (a skill/MCP can be enabled for several agents at once).
interface AgentToggleItem {
  id: string
  label: string
  checked: boolean
  /// Greyed-out and non-interactive. Used by the MCP page for disconnected
  /// agents so users can still see (and untoggle) stale enablements until the
  /// agent reconnects.
  disabled?: boolean
  /// Shows a spinner in place of the bullet while a toggle mutation is in
  /// flight for this agent.
  pending?: boolean
}

/// Per-agent enable group used by the Skills and MCP pages. Renders the
/// agents in a single bordered row that reads like a segmented control, with
/// **clear vertical dividers between segments** and a token-coloured bullet
/// before each label: `●` accent when enabled, `○` subtle when off. The
/// checked segment also fills with `accent-soft`. Clicking one segment flips
/// that single agent. `role="group"` (not `radiogroup`) — the toggles are
/// independent, this is the radio-group *look* only.
///
/// The dividers are explicit `<span>` elements (not Tailwind `border-r`) so
/// they render at a consistent, visible width regardless of the segment's
/// background fill — a `border-r` on a `bg-accent-soft` segment can read as
/// part of the fill on some monitors, and `last:` variants make the "drop the
/// trailing divider" rule easy to get wrong.
export function AgentToggleGroup({
  items,
  onToggle,
  size = "sm",
  ariaLabel = "per-agent enable",
}: {
  items: AgentToggleItem[]
  onToggle: (id: string, next: boolean) => void
  size?: "sm" | "md"
  ariaLabel?: string
}) {
  const { t } = useTranslation()
  const pad = size === "sm" ? "px-2.5 py-1 text-xs" : "px-3 py-1.5 text-sm"
  const dot = size === "sm" ? "text-2xs" : "text-xs"
  return (
    <div
      role="group"
      aria-label={ariaLabel}
      className="inline-flex flex-wrap items-stretch border border-border-strong bg-inset"
    >
      {items.map((it, i) => {
        const blocked = it.disabled || it.pending
        const last = i === items.length - 1
        return (
          <div key={it.id} className="flex items-stretch">
            <button
              type="button"
              aria-pressed={it.checked}
              // pending is non-interactive too: the mutation is in flight, so
              // the segment must not stay Tab-focusable / keyboard-triggerable.
              disabled={blocked}
              onClick={() => !blocked && onToggle(it.id, !it.checked)}
              title={
                it.disabled
                  ? t("skills.notDetected", { agent: it.label })
                  : t("skills.toggleFor", { agent: it.label })
              }
              className={`flex items-center gap-1.5 ${pad} font-medium transition-[background-color,color] duration-fast focus-visible:outline-none focus-visible:shadow-focus disabled:opacity-40 disabled:pointer-events-none ${
                it.checked ? "bg-accent-soft text-accent" : "text-muted hover:text-fg hover:bg-raised"
              }`}
            >
              {it.pending ? (
                <span className={`${dot} leading-none animate-pulse`}>~</span>
              ) : (
                <span className={`${dot} leading-none ${it.checked ? "text-accent" : "text-subtle"}`}>
                  {it.checked ? "●" : "○"}
                </span>
              )}
              <span className="flex items-center gap-1.5">{it.label}<AgentKindBadge id={it.id} /></span>
            </button>
            {!last && <span aria-hidden="true" className="w-px self-stretch bg-border-strong" />}
          </div>
        )
      })}
    </div>
  )
}

// ---------------------------------------------------------------------------
// Tri-state variant (MCP page): absent / disabled / enabled per agent.
// ---------------------------------------------------------------------------

export type AgentState = "absent" | "disabled" | "enabled"

interface AgentStateItem {
  id: string
  label: string
  state: AgentState
  /// The agent's config format carries an `enabled` field — offer the middle
  /// "written but disabled" state. Without it the control flips absent ↔
  /// enabled, because a "disabled" entry is meaningless in a format that
  /// runs every listed server regardless.
  tri?: boolean
  /// Greyed-out and non-interactive (disconnected agents).
  disabled?: boolean
  /// Shows a spinner in place of the bullet while a mutation is in flight.
  pending?: boolean
}

const STATE_CYCLE: AgentState[] = ["absent", "enabled", "disabled"]

function nextState(cur: AgentState, tri: boolean): AgentState {
  if (!tri) return cur === "enabled" ? "absent" : "enabled"
  const i = STATE_CYCLE.indexOf(cur)
  return STATE_CYCLE[(i + 1) % STATE_CYCLE.length]
}

const STATE_TIP_KEY: Record<AgentState, string> = {
  absent: "orchestration.stateAbsent",
  disabled: "orchestration.stateDisabled",
  enabled: "orchestration.stateEnabled",
}

// Per-state button style — one lookup instead of a nested ternary.
const STATE_BTN_CLASS: Record<AgentState, string> = {
  enabled: "bg-accent-soft text-accent",
  disabled: "bg-raised text-muted hover:text-fg",
  absent: "text-muted hover:text-fg hover:bg-raised",
};

const STATE_DOT_CLASS: Record<AgentState, string> = {
  enabled: "text-accent",
  disabled: "text-subtle",
  absent: "text-subtle",
};

const STATE_GLYPH: Record<AgentState, string> = {
  enabled: "●",
  disabled: "◐",
  absent: "○",
};

/// Per-agent tri-state group used by the MCP page. Same bordered-row visual
/// language as `AgentToggleGroup`, but each segment carries one of three
/// states: `●` accent = enabled, `◐` muted = written but disabled, `○` =
/// absent. Clicking cycles absent → enabled → disabled → absent for agents
/// whose format supports the flag (`tri`), otherwise it flips absent ↔
/// enabled. `role="group"` — the toggles are independent per agent.
export function AgentStateGroup({
  items,
  onSetState,
  size = "sm",
  ariaLabel = "per-agent state",
}: {
  items: AgentStateItem[]
  onSetState: (id: string, next: AgentState) => void
  size?: "sm" | "md"
  ariaLabel?: string
}) {
  const { t } = useTranslation()
  const pad = size === "sm" ? "px-2.5 py-1 text-xs" : "px-3 py-1.5 text-sm"
  const dot = size === "sm" ? "text-[10px]" : "text-xs"
  return (
    <div
      role="group"
      aria-label={ariaLabel}
      className="inline-flex flex-wrap items-stretch border border-border-strong bg-inset"
    >
      {items.map((it, i) => {
        const blocked = it.disabled || it.pending
        const last = i === items.length - 1
        const next = nextState(it.state, !!it.tri)
        return (
          <div key={it.id} className="flex items-stretch">
            <button
              type="button"
              aria-pressed={it.state !== "absent"}
              aria-label={`${it.label}: ${t(STATE_TIP_KEY[it.state])}`}
              // pending is non-interactive too (mutation in flight).
              disabled={blocked}
              onClick={() => !blocked && onSetState(it.id, next)}
              title={`${it.label} — ${t(STATE_TIP_KEY[it.state])}. ${t("orchestration.click")}: ${t(STATE_TIP_KEY[next])}.`}
              className={`flex items-center gap-1.5 ${pad} font-medium transition-[background-color,color] duration-fast focus-visible:outline-none focus-visible:shadow-focus disabled:opacity-40 disabled:pointer-events-none ${STATE_BTN_CLASS[it.state]}`}
            >
              {it.pending ? (
                <span className={`${dot} leading-none animate-pulse`}>~</span>
              ) : (
                <span className={`${dot} leading-none ${STATE_DOT_CLASS[it.state]}`}>
                  {STATE_GLYPH[it.state]}
                </span>
              )}
              <span className="flex items-center gap-1.5">{it.label}<AgentKindBadge id={it.id} /></span>
            </button>
            {!last && <span aria-hidden="true" className="w-px self-stretch bg-border-strong" />}
          </div>
        )
      })}
    </div>
  )
}
