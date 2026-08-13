import { useTranslation } from "react-i18next";

/// Renders a sub-agent routing role as its stable `routing_policy.role` key.
/// Mirrors `SubagentRole::as_policy_key()` (identity.rs:73-80). The key is
/// what `routing_policy` rows are indexed by, so showing it verbatim keeps
/// the UI honest about what's actually persisted:
///
///   main                 → `main`             (default, conservative)
///   ClaudeAgent{name}    → `claude:{name}`
///   PiSubagent{role}     → `pi:{role}`
///   OpenCodeAgent{name}  → `opencode:{name}`
///
/// `roleSource` (identity.rs:99-108) is shown as a honesty hint:
///   native    → derived from agent-native structured metadata
///   heuristic → no metadata; defaulted conservatively to Main
export type RoleSource = "native" | "heuristic";

export function RoleKey({
  roleKey,
  roleSource,
}: {
  roleKey: string;
  roleSource?: RoleSource;
}) {
  const { t } = useTranslation();
  return (
    <span className="inline-flex items-center gap-1.5">
      <span className="font-mono text-xs text-fg">{roleKey}</span>
      {roleSource && (
        <span
          className={`font-mono text-2xs ${
            roleSource === "native" ? "text-success" : "text-subtle"
          }`}
          title={
            roleSource === "native"
              ? t("orchestration.roleNative")
              : t("orchestration.roleHeuristic")
          }
        >
          {roleSource === "native" ? "●" : "○"} {t(`orchestration.roleSource.${roleSource}`)}
        </span>
      )}
    </span>
  );
}
