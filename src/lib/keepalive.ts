import type { KeepAlivePhase } from "../ipc";

/// Display semantics for one keep-alive phase. Labels are translation KEYS
/// ("keepalive.*") — the shared surface for the provider-card chip, the
/// Quota-page popover trigger, and the popover content; each renders with
/// `t(meta.labelKey)` so they never drift.
interface KeepAlivePhaseMeta {
  /// Translation key for the short display label.
  labelKey: string;
  /// Text-colour class applied to the icon + label.
  color: string;
  /// `animate-pulse` for the actively-pinging phase.
  pulse?: boolean;
  /// Whether the indicator should render at all on the provider card. `false`
  /// for phases that mean keep-alive is effectively off (`disabled`,
  /// `not_configured`) so an unarmed card stays clean. The Quota-page trigger
  /// always renders (it's also the entry point to configure keep-alive).
  visible: boolean;
}

/// The keep-alive indicator is uniformly a `HeartPulse` lucide icon (the
/// "heartbeat") at 12px, tinted by `color`. There is no per-phase glyph — the
/// phase reads through colour + label. Surfaces render the icon themselves:
///   <HeartPulse data-icon size={12} className={meta.color} />
const PHASE_META: Record<KeepAlivePhase, KeepAlivePhaseMeta> = {
  disabled: { labelKey: "keepalive.disabled", color: "text-subtle", visible: false },
  not_configured: { labelKey: "keepalive.notConfigured", color: "text-subtle", visible: false },
  unverified: { labelKey: "keepalive.unverified", color: "text-warning", visible: true },
  idle: { labelKey: "keepalive.armed", color: "text-success", visible: true },
  resetting: { labelKey: "keepalive.resetting", color: "text-warning", visible: true },
  pinging: { labelKey: "keepalive.pinging", color: "text-accent", visible: true, pulse: true },
  retrying: { labelKey: "keepalive.retrying", color: "text-warning", visible: true },
  error: { labelKey: "keepalive.error", color: "text-danger", visible: true },
};

const DEFAULT_META: KeepAlivePhaseMeta = PHASE_META.disabled;

/// Resolve the display meta for a phase, with a safe fallback. Accepts
/// `undefined` (no status fetched yet) → treated as `disabled`.
export function keepaliveMeta(
  phase: KeepAlivePhase | string | undefined,
): KeepAlivePhaseMeta {
  if (!phase) return DEFAULT_META;
  return PHASE_META[phase as KeepAlivePhase] ?? DEFAULT_META;
}
