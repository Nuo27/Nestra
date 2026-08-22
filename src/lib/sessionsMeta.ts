// Provider display metadata ONLY — pure chrome (dot color + label) with a
// graceful fallback for unknown providers. No resume logic lives here; the
// Rust registry resolves the native resume command in session_open.
export const PROVIDERS: { id: string; label: string; color: string }[] = [
  { id: "claude-code-cli", label: "Claude", color: "var(--accent)" },
  { id: "pi-cli", label: "Pi", color: "var(--brand-pi)" },
  { id: "opencode-desktop", label: "OpenCode Desktop", color: "var(--brand-opencode)" },
  { id: "zcode-desktop", label: "ZCode", color: "var(--brand-zcode)" },
  { id: "codex-desktop", label: "Codex Desktop", color: "var(--brand-codex)" },
];

// Agent registry id (from agentList) → the session-provider id its sessions
// are stored under. Providers here are only those with a resumable agent
// (matches session/provider.rs registrations). Mirrors Rust's default_provider_registry.
export const AGENT_TO_PROVIDER: Record<string, string> = {
  "claude-code-cli": "claude-code-cli",
  "opencode-desktop": "opencode-desktop",
  "pi-cli": "pi-cli",
};

export function providerMeta(id: string) {
  return (
    PROVIDERS.find((p) => p.id === id) ?? {
      id,
      label: id,
      color: "var(--brand-fallback)",
    }
  );
}
