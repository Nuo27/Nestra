import { invoke } from "@tauri-apps/api/core";
import type { BuiltinKind } from "./quota";
import type { ModelAbilities } from "./index";

// ============================================================
// Provider surface — the LLM endpoint + key.
// Agent/CLI types (AgentInfo, AgentProvider, …) live in agent.ts.
// ============================================================

export interface ProtocolInfo {
  protocol: string;
  base_url: string;
}

export interface ProviderPreset {
  id: string;
  display_name: string;
  protocols: ProtocolInfo[];
  default_model: string | null;
  /** Built-in quota query this preset carries (z.ai / minimax / openrouter),
   *  or null when the preset has no built-in fetcher. Endpoints created from
   *  the preset inherit this as their query plan. */
  quota_query: BuiltinKind | null;
}

export interface ValidationResult {
  ok: boolean;
  error_code: string | null;
  message: string | null;
}

export interface EndpointInfo {
  id: string;
  display_name: string;
  has_api_key: boolean;
  status: "valid" | "invalid" | "unvalidated";
  models: {
    haiku?: string;
    sonnet?: string;
    opus?: string;
    default?: string;
    available?: string[];
  } | null;
  advanced_env: Record<string, unknown> | null;
  /** User-saved per-model ability overrides, keyed by model id. */
  model_abilities: Record<string, ModelAbilities>;
  /** models.dev-derived defaults for the endpoint's selected model ids.
   *  The Capabilities disclosure pre-populates from this and shows a
   *  "default from models.dev" hint per field. */
  model_abilities_defaults: Record<string, ModelAbilities>;
  last_validated_at: number | null;
  models_fetched_at: number | null;
  protocols: ProtocolInfo[];
}

// ---- Provider (endpoint) ----
export const endpointList = () => invoke<EndpointInfo[]>("endpoint_list");
export const endpointGet = (id: string) => invoke<EndpointInfo>("endpoint_get", { id });
export const endpointCreate = (input: { id: string; displayName: string }) =>
  invoke<EndpointInfo>("endpoint_create", {
    id: input.id,
    displayName: input.displayName,
  });
export const endpointDelete = (id: string) => invoke<void>("endpoint_delete", { id });
export const endpointAddProtocol = (id: string, protocol: string, baseUrl: string) =>
  invoke<EndpointInfo>("endpoint_add_protocol", { id, protocol, baseUrl });
export const endpointRemoveProtocol = (id: string, protocol: string) =>
  invoke<EndpointInfo>("endpoint_remove_protocol", { id, protocol });
export const providerPresets = () => invoke<ProviderPreset[]>("provider_presets");
export const endpointSetName = (id: string, displayName: string) =>
  invoke<void>("endpoint_set_name", { id, displayName });
export const endpointSetModels = (id: string, models: unknown) =>
  invoke<void>("endpoint_set_models", { id, models });
export const endpointSetAdvancedEnv = (id: string, env: unknown) =>
  invoke<void>("endpoint_set_advanced_env", { id, env });
export const endpointSetModelAbilities = (
  id: string,
  abilities: Record<string, ModelAbilities>,
) => invoke<void>("endpoint_set_model_abilities", { id, abilities });
export const endpointSetApiKey = (id: string, key: string) =>
  invoke<ValidationResult>("endpoint_set_api_key", { id, key });
export const endpointClearApiKey = (id: string) =>
  invoke<void>("endpoint_clear_api_key", { id });

/// Result of the single-step create-with-preset flow. The endpoint always
/// exists on return; `validation.ok = false` means the key was rejected
/// (protocols were still persisted) — the caller routes to the edit page to
/// fix the key.
export interface CreateWithPresetResult {
  id: string;
  validation: ValidationResult;
}
export const endpointCreateWithPreset = (input: {
  id: string;
  displayName: string;
  protocols: { protocol: string; base_url: string }[];
  apiKey: string;
  /** Built-in quota query inherited from the preset, if any. Stamped as the
   *  new endpoint's query plan so the Quota page + keep-alive work without
   *  extra configuration. */
  quotaQuery: BuiltinKind | null;
}) =>
  invoke<CreateWithPresetResult>("endpoint_create_with_preset", {
    id: input.id,
    displayName: input.displayName,
    protocols: input.protocols,
    apiKey: input.apiKey,
    quotaQuery: input.quotaQuery,
  });
/** Result of the "Fetch models" button. `resolved` = abilities the local
 *  models.dev chain already covers (display-only fallback until saved);
 *  `hints` = provider-declared fields for models the local chain can't
 *  resolve — merged into the override draft so Save persists them. */
export interface FetchedModels {
  models: string[];
  resolved: Record<string, ModelAbilities>;
  hints: Record<string, ModelAbilities>;
}
export const endpointFetchModels = (id: string) =>
  invoke<FetchedModels>("endpoint_fetch_models", { id });
