import { useState } from "react";
import { useTranslation } from "react-i18next";
import { Check, Copy } from "lucide-react";
import type { EndpointInfo } from "../../ipc";
import { Card } from "../controls/Card";
import { Button } from "../controls/Button";
import { useCopy } from "../../lib/useCopy";
import { Tabs } from "../controls/Tabs";

const KEY_PLACEHOLDER = "<your api key>";

type Models = {
  haiku?: string;
  sonnet?: string;
  opus?: string;
  default?: string;
  available?: string[];
};

/** Live preview of the config block Nestra writes for each compatible CLI.
 * Mirrors the Rust ConfigWriter output (cli/*.rs). Redacts the key. */
export function ConfigPreview({ endpoint }: { endpoint: EndpointInfo }) {
  const { t } = useTranslation();
  const previews = buildPreviews(endpoint);
  const [open, setOpen] = useState(previews[0]?.id ?? "");
  const active = previews.find((p) => p.id === open);
  const [copied, copy] = useCopy();
  if (previews.length === 0) return null;

  return (
    <Card
      title={t("providerEdit.configPreview")}
      description={t("providerEdit.configPreviewDesc")}
      action={
        active && (
          <Button
            variant="ghost"
            size="sm"
            onClick={() => copy(active.body)}
            aria-label={t("providerEdit.configPreviewAria")}
          >
            {copied ? <Check data-icon size={13} /> : <Copy data-icon size={13} />}
            {copied ? t("common.copied") : t("common.copy")}
          </Button>
        )
      }
    >
      <Tabs
        size="sm"
        className="mb-3"
        value={open}
        onChange={setOpen}
        items={previews.map((p) => ({ id: p.id, label: p.label }))}
      />

      {active && (
        <div>
          <div className="text-xs text-subtle mb-1 font-mono">{active.path}</div>
          <pre className="bg-inset border border-border p-3 text-xs overflow-auto max-h-80 scroll">
            {active.body}
          </pre>
        </div>
      )}
    </Card>
  );
}

type Preview = { id: string; label: string; path: string; body: string };

function pickUrl(e: EndpointInfo, ...kinds: string[]): string {
  for (const k of kinds) {
    const row = e.protocols.find((p) => p.protocol === k);
    if (row) return row.base_url;
  }
  return e.protocols[0]?.base_url ?? "";
}

/** opencode's @ai-sdk/anthropic posts to `${baseURL}/messages`, so an Anthropic
 * base must carry the `/v1` version root (e.g. `.../anthropic` → `.../anthropic/v1`). */
function sdkBaseUrl(url: string, anthropic: boolean): string {
  if (!anthropic) return url;
  const t = url.replace(/\/+$/, "");
  return t.endsWith("/v1") ? t : `${t}/v1`;
}

function buildPreviews(e: EndpointInfo): Preview[] {
  const m: Models = e.models ?? {};
  const adv = envEntries(e);
  const out: Preview[] = [];

  const hasAnthropic = e.protocols.some((p) => p.protocol === "anthropic");
  const hasOpenaiComp = e.protocols.some(
    (p) => p.protocol === "openai-comp" || p.protocol === "custom",
  );
  const hasResponses = e.protocols.some((p) => p.protocol === "response-api");

  if (hasAnthropic) out.push(claudeCodePreview(e, m, adv));
  // ZCode binds both wire families (anthropic / openai-compatible).
  if (hasAnthropic || hasOpenaiComp) out.push(zcodePreview(e, m, hasAnthropic));
  if (hasOpenaiComp) {
    out.push(openCodePreview(e, m, hasAnthropic));
    out.push(piPreview(e, m, hasAnthropic));
  }
  // Codex speaks only the Responses wire.
  if (hasResponses) out.push(codexPreview(e, m));
  return out;
}

function envEntries(e: EndpointInfo): Record<string, string> {
  const obj: Record<string, string> = {};
  if (e.advanced_env) {
    for (const [k, v] of Object.entries(e.advanced_env)) obj[k] = String(v);
  }
  return obj;
}

function tiers(m: Models): { haiku: string; sonnet: string; opus: string } {
  const d = m.default ?? "";
  return {
    haiku: m.haiku || d,
    sonnet: m.sonnet || d,
    opus: m.opus || d,
  };
}

function claudeCodePreview(e: EndpointInfo, m: Models, adv: Record<string, string>): Preview {
  const t = tiers(m);
  const reserved = new Set([
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
  ]);
  const env: Record<string, string> = {
    ANTHROPIC_BASE_URL: pickUrl(e, "anthropic"),
    ANTHROPIC_AUTH_TOKEN: KEY_PLACEHOLDER,
    ANTHROPIC_MODEL: m.default ?? "",
    ANTHROPIC_DEFAULT_HAIKU_MODEL: t.haiku,
    ANTHROPIC_DEFAULT_SONNET_MODEL: t.sonnet,
    ANTHROPIC_DEFAULT_OPUS_MODEL: t.opus,
  };
  for (const [k, v] of Object.entries(adv)) {
    if (!reserved.has(k)) env[k] = v;
  }
  return {
    id: "claude-code-cli",
    label: "Claude Code", // i18n: agent display names come from the registry
    path: "~/.claude/settings.json",
    body: JSON.stringify({ env }, null, 2),
  };
}

/** Mirrors agents/zcode.rs — one `nestra-<endpoint>` entry in ZCode's
 * `~/.zcode/v2/config.json` top-level `provider` map. ZCode supports both
 * wire families: `anthropic` (base + `/v1/messages`) and
 * `openai-compatible` (base + `/chat/completions`, base keeps its `/v1`).
 * Model limits mirror the adapter's models.dev fallback. */
function zcodePreview(e: EndpointInfo, m: Models, hasAnthropic: boolean): Preview {
  const key = `nestra-${e.id}`;
  const kind = hasAnthropic ? "anthropic" : "openai-compatible";
  const baseURL = hasAnthropic ? pickUrl(e, "anthropic") : pickUrl(e, "openai-comp", "custom");
  const ids = (m.available?.length ? m.available : m.default ? [m.default] : []).filter(Boolean);
  const models: Record<string, { limit: { context: number; output: number }; modalities: { input: string[]; output: string[] } }> = {};
  for (const id of ids) {
    models[id] = {
      limit: { context: 200_000, output: 128_000 },
      modalities: { input: ["text"], output: ["text"] },
    };
  }
  const entry = {
    name: `${e.display_name} (via Nestra)`,
    kind,
    options: { baseURL, apiKey: KEY_PLACEHOLDER },
    enabled: true,
    source: "custom",
    models,
  };
  return {
    id: "zcode-desktop",
    label: "ZCode", // i18n: agent display names come from the registry
    path: "~/.zcode/v2/config.json",
    body: JSON.stringify({ provider: { [key]: entry } }, null, 2),
  };
}

function openCodePreview(e: EndpointInfo, m: Models, hasAnthropic: boolean): Preview {
  const key = `nestra-${e.id}`;
  const npm = hasAnthropic ? "@ai-sdk/anthropic" : "@ai-sdk/openai-compatible";
  const ids =
    hasAnthropic
      ? [m.haiku, m.sonnet, m.opus].filter(Boolean) as string[]
      : m.available?.length
        ? m.available
        : m.default
          ? [m.default]
          : [];
  const models: Record<string, { name: string }> = {};
  for (const id of ids) models[id] = { name: id };
  const block = {
    npm,
    name: `${e.display_name} (via Nestra)`,
    options: { baseURL: sdkBaseUrl(pickUrl(e, hasAnthropic ? "anthropic" : "openai-comp", "custom"), hasAnthropic), apiKey: KEY_PLACEHOLDER },
    models,
  };
  return {
    id: "opencode",
    label: "OpenCode Desktop",
    path: "~/.config/opencode/opencode.json",
    body: JSON.stringify({ provider: { [key]: block } }, null, 2),
  };
}

function piPreview(e: EndpointInfo, m: Models, hasAnthropic: boolean): Preview {
  const key = `nestra-${e.id}`;
  const api = hasAnthropic ? "anthropic" : "openai-comp";
  const ids =
    hasAnthropic
      ? [m.haiku, m.sonnet, m.opus].filter(Boolean) as string[]
      : m.available?.length
        ? m.available
        : m.default
          ? [m.default]
          : [];
  const models = ids.map((id) => ({ id: `${key}:${id}`, provider: key, name: id }));
  return {
    id: "pi-cli",
    label: "Pi",
    path: "~/.pi/agent/models-store.json",
    body: JSON.stringify(
      { providers: { [key]: { baseUrl: pickUrl(e, hasAnthropic ? "anthropic" : "openai-comp", "custom"), apiKey: KEY_PLACEHOLDER, api } }, models },
      null,
      2,
    ),
  };
}

/** Mirrors agents/codex/config.rs — `[model_providers.nestra-<id>]` +
 * selection keys in `~/.codex/config.toml`. Codex appends `/responses` to
 * base_url itself, so the preview shows the version root (strip the
 * `/responses` tail from the endpoint's full Responses URL). */
function codexPreview(e: EndpointInfo, m: Models): Preview {
  const key = `nestra-${e.id}`;
  const url = pickUrl(e, "response-api").replace(/\/+$/, "");
  const base = url.endsWith("/responses") ? url.slice(0, -"/responses".length) : url;
  const model = m.default ?? m.available?.[0] ?? "";
  const body = [
    `model = ${JSON.stringify(model)}`,
    `model_provider = ${JSON.stringify(key)}`,
    "",
    `[model_providers.${key}]`,
    `name = ${JSON.stringify(`${e.display_name} (via Nestra)`)}`,
    `wire_api = "responses"`,
    `base_url = ${JSON.stringify(base)}`,
    `requires_openai_auth = true`,
    `experimental_bearer_token = ${JSON.stringify(KEY_PLACEHOLDER)}`,
  ].join("\n");
  return {
    id: "codex-desktop",
    label: "Codex Desktop", // i18n: agent display names come from the registry
    path: "~/.codex/config.toml",
    body,
  };
}