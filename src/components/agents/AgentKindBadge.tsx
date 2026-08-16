/// Kind badge for a registry agent id, following the registry's own suffix
/// convention (`-cli` → CLI, `-desktop` → Desktop — see AGENTS.md). Renders
/// nothing for ids that don't match, so the badge is safe to drop anywhere an
/// agent label shows. Token classes only; "CLI"/"Desktop" are universal
/// abbreviations, so no i18n (consistent with backend English display names).
export function AgentKindBadge({ id }: { id: string }) {
  const label = id.endsWith("-cli")
    ? "CLI"
    : id.endsWith("-desktop")
      ? "Desktop"
      : null;
  if (!label) return null;
  return (
    <span className="rounded-sm border border-border bg-inset px-1 text-2xs leading-4 text-subtle">
      {label}
    </span>
  );
}
