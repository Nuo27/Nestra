/// Shared role-chip anatomy. Three surfaces render the same chip with
/// different wrapper ELEMENTS — the agent detail page's Link chips (navigate
/// to the policy editor) and the policy editor's two button strips (add a
/// role / add a tier preset). This module owns the class strings so the
/// visual language can't drift between them; the caller picks the element.
export const ROLE_CHIP_BASE =
  "inline-flex items-center gap-1.5 border border-border bg-inset px-1.5 py-0.5 font-mono text-2xs transition-colors duration-fast ";

/// Clickable variant (adds a new policy row when used).
export const ROLE_CHIP_ACTIVE =
  "text-fg hover:border-accent-border hover:bg-raised disabled:opacity-50";

/// Already-configured variant (informational, not clickable).
export const ROLE_CHIP_DONE = "cursor-default text-muted";
