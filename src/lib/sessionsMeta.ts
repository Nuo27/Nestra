// Provider display metadata ONLY — pure chrome (dot color + label) with a
// graceful fallback for unknown providers. No resume logic lives here; the
// Rust registry resolves the native resume command in session_open.
export const PROVIDERS: { id: string; label: string; color: string }[] = [
  { id: "claude-code", label: "Claude", color: "var(--accent)" },
  { id: "pi", label: "Pi", color: "var(--brand-pi)" },
  { id: "opencode-desktop", label: "OpenCode Desktop", color: "var(--brand-opencode)" },
];

// Agent registry id (from agentList) → the session-provider id its sessions
// are stored under. Providers here are only those with a resumable agent
// (matches session/provider.rs registrations). Mirrors Rust's default_provider_registry.
export const AGENT_TO_PROVIDER: Record<string, string> = {
  "claude-code": "claude-code",
  "opencode-desktop": "opencode-desktop",
  pi: "pi",
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
