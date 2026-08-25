import { invoke } from "@tauri-apps/api/core";

export interface SkillMeta {
  id: string;
  name: string;
  description: string | null;
  source: string;
  path: string;
  skill_type: string;
  managed: boolean;
  enabled_agents: string[];
  builtin: boolean;
}

// ---- Skills ----
export const skillsList = () => invoke<SkillMeta[]>("skills_list");
export const skillsReveal = (path: string) => invoke<void>("skills_reveal", { path });
export const skillsUninstall = (id: string) => invoke<void>("skills_uninstall", { id });
export const skillsToggle = (id: string, agentId: string, enabled: boolean) =>
  invoke<SkillMeta>("skills_toggle", { id, agentId, enabled });
export const skillsImportOne = (path: string, agentId: string) =>
  invoke<SkillMeta>("skills_import_one", { path, agentId });
/** Stop managing a skill: drop the DB row + SSOT, leave agent-dir copies. */
export const skillsUnmanage = (id: string) => invoke<void>("skills_unmanage", { id });
