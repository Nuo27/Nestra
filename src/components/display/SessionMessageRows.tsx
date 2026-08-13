import { useTranslation } from "react-i18next";
import type { SessionMessage } from "../../ipc";
import { formatTime } from "../../lib/format";
import { isErrorMeta, type RenderItem } from "../../lib/sessionMessages";
import { SectionLabel } from "../layout/PageHeader";
import { Badge } from "../ui/badge";
import { MessageCard } from "./MessageCard";
import { CodeBlock } from "./CodeBlock";

// Small badges for the few metadata keys we currently surface. Unknown keys
// stay in the metadata blob and are not visualized (lossless preservation).
function MetadataBadges({ meta }: { meta: string }) {
  const { t } = useTranslation();
  let parsed: Record<string, unknown> | null = null;
  try {
    const v = JSON.parse(meta);
    if (v && typeof v === "object") parsed = v as Record<string, unknown>;
  } catch {
    return null;
  }
  if (!parsed) return null;
  return (
    <span className="ml-1 inline-flex gap-1">
      {parsed.is_error === true && (
        <Badge tone="danger" variant="soft">{t("common.error")}</Badge>
      )}
      {typeof parsed.provider_kind === "string" && (
        <Badge tone="neutral" variant="soft">{parsed.provider_kind}</Badge>
      )}
    </span>
  );
}

function ThinkingRow({ m }: { m: SessionMessage }) {
  const { t } = useTranslation();
  const text = m.thinking ?? m.content_text;
  return (
    <MessageCard
      header={
        <>
          <SectionLabel inline>{t("sessions.reasoning")}</SectionLabel>
          <MetadataBadges meta={m.provider_metadata_json} />
        </>
      }
      trailing={m.timestamp ? formatTime(m.timestamp) : ""}
      defaultOpen
      body={
        text ? (
          <CodeBlock variant="bare" maxH="max-h-80" tone="muted" italic pad="px-3 py-2">
            {text}
          </CodeBlock>
        ) : undefined
      }
    />
  );
}

function ToolPairRow({
  use,
  result,
}: {
  use: SessionMessage;
  result?: SessionMessage;
}) {
  const { t } = useTranslation();
  const isError = isErrorMeta(result?.provider_metadata_json);
  const label = use.tool_name ?? "tool";
  return (
    <MessageCard
      borderTone={isError ? "danger" : "default"}
      chevron="warning"
      defaultOpen={false}
      header={
        <>
          <span className="text-xs text-muted">{label}</span>
          {!result && <span className="text-xs text-warning">{t("sessions.noResult")}</span>}
          {isError && <Badge tone="danger" variant="soft">{t("common.error")}</Badge>}
          <MetadataBadges meta={(result ?? use).provider_metadata_json} />
        </>
      }
      trailing={use.timestamp ? formatTime(use.timestamp) : ""}
      body={
        <div>
          {use.tool_input && (
            <div>
              <SectionLabel inline className="px-3 py-1">{t("sessions.input")}</SectionLabel>
              <CodeBlock variant="bare" maxH="max-h-60" tone="muted" pad="px-3 pb-2">
                {use.tool_input}
              </CodeBlock>
            </div>
          )}
          {result?.tool_output && (
            <div className="border-t border-border">
              <SectionLabel inline className="px-3 py-1">{t("sessions.output")}</SectionLabel>
              <CodeBlock variant="bare" maxH="max-h-80" tone="muted" pad="px-3 pb-2">
                {result.tool_output}
              </CodeBlock>
            </div>
          )}
        </div>
      }
    />
  );
}

function ToolRow({ m }: { m: SessionMessage }) {
  const { t } = useTranslation();
  return (
    <MessageCard
      chevron="warning"
      defaultOpen={false}
      header={
        <>
          <span className="text-xs text-muted">
            TOOL{m.tool_name ? ` · ${m.tool_name}` : ""}
            {!m.tool_call_id && t("sessions.toolNoId")}
          </span>
          <MetadataBadges meta={m.provider_metadata_json} />
        </>
      }
      trailing={m.timestamp ? formatTime(m.timestamp) : ""}
      body={
        <CodeBlock variant="bare" maxH="max-h-80" tone="muted" pad="px-3 py-2">
          {m.tool_input ?? m.tool_output ?? m.content_text}
        </CodeBlock>
      }
    />
  );
}

function MessageRow({ m }: { m: SessionMessage }) {
  const label = m.role.toUpperCase();
  return (
    <MessageCard
      plain
      header={
        <>
          <span className={"text-xs " + (m.role === "assistant" ? "text-accent" : "text-muted")}>
            {label}
          </span>
          <MetadataBadges meta={m.provider_metadata_json} />
        </>
      }
      trailing={m.timestamp ? formatTime(m.timestamp) : ""}
      body={
        <CodeBlock variant="bare" maxH="max-h-none" size="sm" pad="">
          {m.content_text}
        </CodeBlock>
      }
    />
  );
}

/** The session message viewer: one row component per render-item kind. */
export function SessionMessageRows({ items }: { items: RenderItem[] }) {
  return (
    <>
      {items.map((item, i) => {
        const key = `${i}`;
        if (item.kind === "single") {
          return <MessageRow key={key} m={item.m} />;
        }
        if (item.kind === "thinking") {
          return <ThinkingRow key={key} m={item.m} />;
        }
        if (item.kind === "tool_pair") {
          return <ToolPairRow key={key} use={item.use} result={item.result} />;
        }
        return <ToolRow key={key} m={item.m} />;
      })}
    </>
  );
}
