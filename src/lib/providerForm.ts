import type { EndpointInfo, ModelAbilities } from "../ipc";

export class ValidationError extends Error {}

/// Key-order-insensitive JSON serialization for dirty checks — backend
/// objects can arrive with any key order and form edits reorder keys, so a
/// plain JSON.stringify comparison would report phantom diffs.
export function stableJson(value: unknown): string {
  return JSON.stringify(sortKeys(value));
}

function sortKeys(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(sortKeys);
  if (value && typeof value === "object") {
    return Object.fromEntries(
      Object.entries(value as Record<string, unknown>)
        .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))
        .map(([k, v]) => [k, sortKeys(v)]),
    );
  }
  return value;
}

/// Form representation of `advanced_env`: non-string values serialize to
/// JSON — shared by `emptyToForm`, `isDirty`, and the save diff so every
/// side normalizes identically.
export function serializeAdvancedEnv(
  env: Record<string, unknown> | null | undefined,
): Record<string, string> {
  const out: Record<string, string> = {};
  for (const [k, v] of Object.entries(env ?? {})) {
    out[k] = typeof v === "string" ? v : JSON.stringify(v);
  }
  return out;
}

export interface FormState {
  display_name: string;
  protocols: { protocol: string; base_url: string }[];
  api_key: string;
  clear_key: boolean;
  reveal_key: boolean;
  models_haiku: string;
  models_sonnet: string;
  models_opus: string;
  models_default: string;
  models_available: string[];
  advanced_env: Record<string, string>;
  /// Per-model ability overrides. Mirrors the persisted
  /// `provider_endpoint.model_abilities_json` shape: a flat map keyed by
  /// model id. Empty fields inherit from the upstream default
  /// (`model_abilities_defaults`) — that's what "Reset" clears to.
  model_abilities: Record<string, ModelAbilities>;
}

export function emptyToForm(e: EndpointInfo): FormState {
  const m = e.models ?? {};
  return {
    display_name: e.display_name,
    protocols: e.protocols.map((p) => ({ protocol: p.protocol, base_url: p.base_url })),
    api_key: "",
    clear_key: false,
    reveal_key: false,
    models_haiku: m.haiku ?? "",
    models_sonnet: m.sonnet ?? "",
    models_opus: m.opus ?? "",
    models_default: m.default ?? "",
    models_available: m.available ?? [],
    advanced_env: serializeAdvancedEnv(e.advanced_env),
    model_abilities: e.model_abilities ?? {},
  };
}

export function isDirty(f: FormState, e: EndpointInfo): boolean {
  if (f.display_name.trim() !== e.display_name.trim()) return true;
  const curProtos = f.protocols
    .filter((x) => x.protocol.trim())
    .map((x) => ({ protocol: x.protocol.trim(), base_url: x.base_url.trim() }));
  if (stableJson(curProtos) !== stableJson(e.protocols)) return true;
  if ((e.models?.haiku ?? "") !== f.models_haiku) return true;
  if ((e.models?.sonnet ?? "") !== f.models_sonnet) return true;
  if ((e.models?.opus ?? "") !== f.models_opus) return true;
  if ((e.models?.default ?? "") !== f.models_default) return true;
  if (
    JSON.stringify([...f.models_available].sort()) !==
    JSON.stringify([...(e.models?.available ?? [])].sort())
  )
    return true;
  if (stableJson(f.advanced_env) !== stableJson(serializeAdvancedEnv(e.advanced_env)))
    return true;
  if (stableJson(f.model_abilities) !== stableJson(e.model_abilities ?? {})) return true;
  if (f.api_key.trim() !== "") return true;
  if (f.clear_key) return true;
  return false;
}

/** Wire-protocol catalog — label + one-line description shown in the
 *  protocol dropdown and under each row. Mirrors `ProviderKind`s the backend
 *  accepts for `endpoint_add_protocol`. Labels/descriptions are translation
 *  KEYS ("providerEdit.proto*") — rendered with `t()` at use sites. */
export const PROTOCOL_META: Record<string, { labelKey: string; descKey: string }> = {
  anthropic: {
    labelKey: "providerEdit.protoAnthropic",
    descKey: "providerEdit.protoAnthropicDesc",
  },
  "openai-comp": {
    labelKey: "providerEdit.protoOpenai",
    descKey: "providerEdit.protoOpenaiDesc",
  },
  "response-api": {
    labelKey: "providerEdit.protoResponses",
    descKey: "providerEdit.protoResponsesDesc",
  },
  custom: {
    labelKey: "providerEdit.protoCustom",
    descKey: "providerEdit.protoCustomDesc",
  },
};
