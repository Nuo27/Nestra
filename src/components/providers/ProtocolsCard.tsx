import { useState } from "react";
import { useTranslation } from "react-i18next";
import { Plus, Trash2, Globe } from "lucide-react";
import { PROTOCOL_META, type FormState } from "../../lib/providerForm";
import { Card } from "../controls/Card";
import { Button } from "../controls/Button";
import { Input } from "../ui/input";
import { InsetBlock } from "../display/InsetBlock";
import { SectionLabel } from "../layout/PageHeader";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "../ui/select";

export function ProtocolsCard({
  form,
  onPatch,
}: {
  form: FormState;
  onPatch: (patch: Partial<FormState>) => void;
}) {
  const { t } = useTranslation();
  const [adding, setAdding] = useState(false);
  const [draftProtocol, setDraftProtocol] = useState("");
  const [draftUrl, setDraftUrl] = useState("");
  const KINDS = Object.keys(PROTOCOL_META);

  const usedKinds = new Set(form.protocols.map((p) => p.protocol));
  const unusedKinds = KINDS.filter((k) => !usedKinds.has(k));

  // Add/Remove/Edit only patch local state. Persisted on Save — same flow as
  // the rest of the page (no instant writes while editing).
  const commitAdd = () => {
    const url = draftUrl.trim();
    if (!draftProtocol || !url) return;
    onPatch({
      protocols: [...form.protocols, { protocol: draftProtocol, base_url: url }],
    });
    setDraftProtocol("");
    setDraftUrl("");
    setAdding(false);
  };

  return (
    <Card
      title={t("providerEdit.protocols")}
      hint={t("providerEdit.protocolsHint")}
    >
      <div className="space-y-2">
        {form.protocols.length === 0 && !adding && (
          <div className="text-sm text-subtle">{t("providerEdit.noProtocols")}</div>
        )}
        {form.protocols.map((p, i) => (
          <ProtocolRow
            key={i}
            protocol={p.protocol}
            baseUrl={p.base_url}
            availableKinds={Array.from(
              new Set([p.protocol, ...unusedKinds]),
            )}
            onPatch={(patch) =>
              onPatch({
                protocols: form.protocols.map((x, j) =>
                  j === i ? { ...x, ...patch } : x,
                ),
              })
            }
            onDelete={() =>
              onPatch({ protocols: form.protocols.filter((_, j) => j !== i) })
            }
          />
        ))}
        {adding ? (
          <InsetBlock className="space-y-2">
            <SectionLabel inline>{t("providerEdit.newProtocol")}</SectionLabel>
            <Select value={draftProtocol || undefined} onValueChange={setDraftProtocol}>
              <SelectTrigger size="sm">
                <SelectValue placeholder={t("providerEdit.protocolPlaceholder")} />
              </SelectTrigger>
              <SelectContent>
                {unusedKinds.map((k) => (
                  <SelectItem key={k} value={k}>
                    <ProtocolMeta value={k} />
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            <Input
              size="sm"
              prefix={<Globe data-icon />}
              className="font-mono"
              value={draftUrl}
              onChange={(e) => setDraftUrl(e.target.value)}
              placeholder="https://api.example.com/v1"
              autoFocus
              onKeyDown={(e) => {
                if (e.key === "Enter") {
                  e.preventDefault();
                  commitAdd();
                }
              }}
            />
            {draftProtocol && (
              <p className="prose text-2xs text-subtle">
                {t(PROTOCOL_META[draftProtocol]?.descKey)}
              </p>
            )}
            <div className="flex justify-end gap-2">
              <Button
                size="sm"
                variant="ghost"
                onClick={() => {
                  setAdding(false);
                  setDraftProtocol("");
                  setDraftUrl("");
                }}
              >{t("common.cancel")}</Button>
              <Button
                variant="primary"
                size="sm"
                disabled={!draftProtocol || !draftUrl.trim()}
                onClick={commitAdd}
              >
                <Plus data-icon size={13} />{t("providerEdit.addProtocol")}</Button>
            </div>
          </InsetBlock>
        ) : (
          unusedKinds.length > 0 && (
            <div className="flex justify-end">
              <Button size="sm" variant="ghost" onClick={() => setAdding(true)}>
                <Plus data-icon size={13} />{t("providerEdit.addProtocol")}</Button>
            </div>
          )
        )}
      </div>
    </Card>
  );
}

/** Dropdown option content — protocol label + one-line behaviour hint.
 *  `compact` renders just the label row (used inside the single-line trigger). */
function ProtocolMeta({ value, compact }: { value: string; compact?: boolean }) {
  const { t } = useTranslation();
  const meta = PROTOCOL_META[value];
  return (
    <span className={compact ? "" : "flex flex-col gap-0.5"}>
      <span className="text-sm font-medium">
        <span className="mr-1.5">•</span>
        {meta ? t(meta.labelKey) : value}
      </span>
      {!compact && meta && (
        <span className="truncate text-2xs font-normal text-subtle">
          {t(meta.descKey)}
        </span>
      )}
    </span>
  );
}

function ProtocolRow({
  protocol,
  baseUrl,
  availableKinds,
  onPatch,
  onDelete,
}: {
  protocol: string;
  baseUrl: string;
  availableKinds: string[];
  onPatch: (patch: { protocol?: string; base_url?: string }) => void;
  onDelete: () => void;
}) {
  const { t } = useTranslation();
  const meta = PROTOCOL_META[protocol];
  return (
    <InsetBlock className="transition-colors hover:border-border-strong">
      <div className="flex items-center justify-between gap-2">
        <Select value={protocol} onValueChange={(v) => onPatch({ protocol: v })}>
          <SelectTrigger size="sm" className="w-64">
            <ProtocolMeta value={protocol} compact />
          </SelectTrigger>
          <SelectContent>
            {availableKinds.map((k) => (
              <SelectItem key={k} value={k}>
                <ProtocolMeta value={k} />
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <Button variant="ghost" size="sm" onClick={onDelete} title={t("providerEdit.removeProtocol")}>
          <Trash2 data-icon size={13} />
        </Button>
      </div>
      <div className="mt-2">
        <SectionLabel className="mb-1 block">{t("providerEdit.baseEndpoint")}</SectionLabel>
        <Input
          size="sm"
          prefix={<Globe data-icon />}
          className="font-mono"
          value={baseUrl}
          onChange={(e) => onPatch({ base_url: e.target.value })}
          placeholder="https://api.example.com/v1"
        />
      </div>
      {meta && <p className="prose mt-1.5 text-2xs text-subtle">{t(meta.descKey)}</p>}
    </InsetBlock>
  );
}
