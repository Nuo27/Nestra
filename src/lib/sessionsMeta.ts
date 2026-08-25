// Provider display metadata ONLY — pure chrome (dot color + label) with a
// graceful fallback for unknown providers. No resume logic lives here; the
// Rust registry resolves the native resume command in session_open. New
// registry agents need NO entry here: the fallback covers them, an entry
// only adds the brand color + a friendlier label.
export const PROVIDERS: { id: string; label: string; color: string }[] = [
  { id: "claude-code-cli", label: "Claude", color: "var(--accent)" },
  { id: "pi-cli", label: "Pi", color: "var(--brand-pi)" },
  { id: "opencode-desktop", label: "OpenCode Desktop", color: "var(--brand-opencode)" },
  { id: "zcode-desktop", label: "ZCode", color: "var(--brand-zcode)" },
  { id: "codex-desktop", label: "Codex Desktop", color: "var(--brand-codex)" },
];

export function providerMeta(id: string) {
  return (
    PROVIDERS.find((p) => p.id === id) ?? {
      id,
      label: id,
      color: "var(--brand-fallback)",
    }
  );
}
