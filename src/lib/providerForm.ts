import type { EndpointInfo, ModelAbilities } from "../ipc";

export class ValidationError extends Error {}

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
  const adv: Record<string, string> = {};
  for (const [k, v] of Object.entries(e.advanced_env ?? {})) {
    adv[k] = typeof v === "string" ? v : JSON.stringify(v);
  }
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
    advanced_env: adv,
    model_abilities: e.model_abilities ?? {},
  };
}

export function isDirty(f: FormState, e: EndpointInfo): boolean {
  if (f.display_name.trim() !== e.display_name) return true;
  const curProtos = f.protocols
    .filter((x) => x.protocol.trim())
    .map((x) => ({ protocol: x.protocol.trim(), base_url: x.base_url.trim() }));
  if (JSON.stringify(curProtos) !== JSON.stringify(e.protocols)) return true;
  if ((e.models?.haiku ?? "") !== f.models_haiku) return true;
  if ((e.models?.sonnet ?? "") !== f.models_sonnet) return true;
  if ((e.models?.opus ?? "") !== f.models_opus) return true;
  if ((e.models?.default ?? "") !== f.models_default) return true;
  const curEnv: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(e.advanced_env ?? {})) {
    curEnv[k] = typeof v === "string" ? v : JSON.stringify(v);
  }
  if (JSON.stringify(f.advanced_env) !== JSON.stringify(curEnv)) return true;
  if (JSON.stringify(f.model_abilities) !== JSON.stringify(e.model_abilities ?? {})) return true;
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
