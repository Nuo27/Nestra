import { useEffect, useState } from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import {
  agentApplyProviderSelection,
  agentClearProvider,
  type AgentInfo,
  type EndpointInfo,
} from "../../ipc";
import { extractError } from "../../ipc/errors";
import { qk } from "../../lib/queries";
import { useUI } from "../../stores/ui";
import { Button } from "../controls/Button";
import { ButtonGroup } from "../controls/ButtonGroup";
import { SegmentedControl } from "../controls/SegmentedControl";
import { Note } from "../feedback/Note";
import { Radio } from "../ui/radio";
import { Checkbox } from "../ui/checkbox";
import { AgentConfigDialog } from "./AgentConfigDialog";

// ---- Provider configuration ----
//
// Single abstraction for every agent: pick which providers go into the
// config and which one is the default. Single-slot agents (Claude Code)
// enforce `selected.length <= 1`; multi-slot agents (Pi, OpenCode) accept
// any subset. Save ships the full selection to the backend in one IPC, so
// the on-disk config + bindings are always written atomically.

export type ProviderOption = {
  id: string;
  name: string;
  protocols: string[];
  /** Protocol rows this endpoint carries that the agent accepts (the picker
   * surface — only shown when ≥2, i.e. a genuine wire choice like OpenRouter's
   * anthropic vs openai). */
  accepted: string[];
};

/// Direct-mode provider binding editor (shared by the agent detail page).
/// Pick which providers go into the config and which is the default.
export function ProviderConfigPanel({
  agent,
  endpoints,
}: {
  agent: AgentInfo;
  endpoints: EndpointInfo[];
}) {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const toast = useUI((s) => s.pushToast);
  const [previewOpen, setPreviewOpen] = useState(false);

  const multi = agent.capability.supports_multiple_providers;
  const boundIds = new Set(agent.providers.map((p) => p.provider_id));
  const options: ProviderOption[] = endpoints
    .filter(
      (e) =>
        e.has_api_key &&
        e.protocols.some((p) => agent.supported_protocols.includes(p.protocol)),
    )
    .map((e) => {
      const accepted = e.protocols
        .map((p) => p.protocol)
        .filter((p) => agent.supported_protocols.includes(p));
      return {
        id: e.id,
        name: e.display_name,
        protocols: e.protocols.map((p) => p.protocol),
        accepted,
      };
    })
    .sort((a, b) => a.name.localeCompare(b.name));

  // Single-slot agents bind exactly one provider. Their `selected` set is the
  // active provider only — never the full binding list. A single-slot row can
  // accumulate stale extra bindings in the DB (older builds added without
  // clearing); seeding `selected` from all of them made the radio no-op guard
  // fire on every already-bound row, so the Save button never enabled.
  const initialSelected = multi
    ? new Set(boundIds)
    : new Set(agent.active_provider_id ? [agent.active_provider_id] : []);
  const [selected, setSelected] = useState<Set<string>>(() => initialSelected);
  const [defaultId, setDefaultId] = useState<string | null>(
    agent.active_provider_id ?? null,
  );
  // Per-binding wire override (the protocol picker). Seeded from each bound
  // provider's resolved protocol; `null` means "no explicit choice" and is sent
  // as-is so the backend resolves the default (first accepted).
  const [protoChoice, setProtoChoice] = useState<Record<string, string | null>>(
    () => Object.fromEntries(agent.providers.map((p) => [p.provider_id, p.protocol || null])),
  );

  // Re-sync local edit state when the backend reports a new selection
  // (e.g. after an enable/disable, or another tab applied a change). Deps are
  // SCALARS only — `agent.providers` is a fresh array reference per refetch,
  // so depending on it reset unsaved panel edits on every background poll.
  useEffect(() => {
    const next = multi
      ? new Set(boundIds)
      : new Set(agent.active_provider_id ? [agent.active_provider_id] : []);
    setSelected(next);
    const active = agent.active_provider_id;
    if (active && next.has(active)) {
      setDefaultId(active);
    } else if (active === null) {
      setDefaultId(null);
    }
    // Merge (don't overwrite) protoChoice: only seed entries the user hasn't
    // touched yet, and bail when nothing was added so an in-progress picker
    // edit survives background re-syncs. Returning the same ref lets React skip
    // the re-render.
    setProtoChoice((prev) => {
      let changed = false;
      const merged = { ...prev };
      for (const p of agent.providers) {
        if (!(p.provider_id in merged)) {
          merged[p.provider_id] = p.protocol || null;
          changed = true;
        }
      }
      return changed ? merged : prev;
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [agent.id, agent.active_provider_id, boundIds, multi]);

  const dirty =
    selected.size !== boundIds.size ||
    ![...selected].every((id) => boundIds.has(id)) ||
    defaultId !== agent.active_provider_id ||
    // A per-binding wire change alone (no selection change) still counts.
    [...selected].some((id) => {
      const backend =
        agent.providers.find((p) => p.provider_id === id)?.protocol ?? null;
      return (protoChoice[id] ?? null) !== backend;
    });

  const applyMut = useMutation({
    mutationFn: async () => {
      // Map the selection to ProviderSelection entries, carrying each row's
      // per-binding wire override (null = resolve the default upstream).
      const list = [...selected].map((id) => ({
        provider_id: id,
        protocol: protoChoice[id] ?? null,
      }));
      // Single-slot agent: exactly one provider (or none). Route through
      // apply_provider_selection so replace_bindings wipes any stale extra
      // bindings older builds left behind — switch_provider only upserts and
      // would keep accumulating them.
      if (!multi) {
        if (list.length === 0) {
          await agentClearProvider(agent.id);
          return;
        }
        await agentApplyProviderSelection(agent.id, list, list[0].provider_id);
        return;
      }
      if (list.length === 0 || defaultId === null) {
        await agentApplyProviderSelection(agent.id, [], defaultId ?? "");
        return;
      }
      await agentApplyProviderSelection(agent.id, list, defaultId);
    },
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: qk.agents() });
      qc.invalidateQueries({ queryKey: qk.agentConfig(agent.id) });
      toast(t("agents.updatedToast", { name: agent.display_name }), "success");
    },
    onError: (e: unknown) =>
      toast(t("agents.updateFailed", { name: agent.display_name, err: extractError(e) ?? String(e) }), "error"),
  });

  const toggle = (id: string) => {
    if (!multi) {
      // Single-slot radio: clicking the active item is a no-op.
      if (selected.has(id)) return;
      setSelected(new Set([id]));
      setDefaultId(id);
      return;
    }
    // Multi-slot: toggle membership — computed from current state, never
    // inside a state updater (side effects in updaters break under React
    // strict mode double-invoke).
    const next = new Set(selected);
    if (next.has(id)) {
      next.delete(id);
      setSelected(next);
      if (defaultId === id) {
        setDefaultId(next.size > 0 ? [...next][0] : null);
      }
    } else {
      next.add(id);
      setSelected(next);
      if (defaultId === null) setDefaultId(id);
    }
  };

  const clearSelection = () => {
    setSelected(new Set());
    setDefaultId(null);
  };

  const pickProto = (id: string, proto: string) => {
    setProtoChoice((prev) => ({ ...prev, [id]: proto }));
  };

  return (
    <div className="mt-3 space-y-2">
      {options.length === 0 ? (
        <Note>
          {t("agents.noCompatProviders")}
        </Note>
      ) : (
        <>
          <ProviderList
            multi={multi}
            options={options}
            selected={selected}
            defaultId={defaultId}
            protoChoice={protoChoice}
            onToggle={toggle}
            onSetDefault={(id) => setDefaultId(id)}
            onClear={clearSelection}
            onPickProto={pickProto}
          />
          <ButtonGroup space="loose" justify="end">
            <Button
              size="sm"
              variant="primary"
              disabled={!dirty || applyMut.isPending}
              loading={applyMut.isPending}
              onClick={() => applyMut.mutate()}
              title={t("agents.writeSelection")}
            >{t("agents.saveSelection")}</Button>
            <Button size="sm" variant="ghost" onClick={() => setPreviewOpen(true)}>{t("agents.configPreview")}</Button>
          </ButtonGroup>
        </>
      )}
      {previewOpen && <AgentConfigDialog agent={agent} onClose={() => setPreviewOpen(false)} />}
    </div>
  );
}

export function ProviderList({
  multi,
  options,
  selected,
  defaultId,
  protoChoice,
  onToggle,
  onSetDefault,
  onClear,
  onPickProto,
}: {
  multi: boolean;
  options: ProviderOption[];
  selected: Set<string>;
  defaultId: string | null;
  protoChoice: Record<string, string | null>;
  onToggle: (id: string) => void;
  onSetDefault: (id: string) => void;
  onClear: () => void;
  onPickProto: (id: string, proto: string) => void;
}) {
  const { t } = useTranslation();
  const noneSelected = selected.size === 0;

  const protoLabel = (p: string) =>
    p === "anthropic"
      ? t("agents.protoAnthropic")
      : p === "openai-comp"
        ? t("agents.protoOpenai")
        : p === "response-api"
          ? t("agents.protoResponses")
          : t("agents.protoCustom");

  /// For a selected option that offers ≥2 accepted wires, render a compact
  /// picker (the per-binding protocol override). Otherwise the read-only
  /// comma-joined protocol list. The picker is the only place a dual-protocol
  /// endpoint (e.g. OpenRouter anthropic vs openai) is steered per agent.
  const wireFor = (o: ProviderOption) => {
    if (selected.has(o.id) && o.accepted.length >= 2) {
      const cur = protoChoice[o.id] ?? o.accepted[0];
      const value = o.accepted.includes(cur) ? cur : o.accepted[0];
      return (
        <SegmentedControl
          size="sm"
          ariaLabel={t("agents.wirePickerTitle")}
          value={value}
          onChange={(next) => onPickProto(o.id, next)}
          items={o.accepted.map((p) => ({ value: p, label: protoLabel(p) }))}
        />
      );
    }
    return o.protocols.length > 0 ? (
      <span className="shrink-0 text-xs text-muted">{o.protocols.join(", ")}</span>
    ) : null;
  };

  // Single-slot agents (radio semantics): compact borderless text rows —
  // [radio][name · wire] on one line. No boxes, no full-width rows.
  if (!multi) {
    return (
      <div className="space-y-0.5">
        {options.map((o) => (
          <div key={o.id} className="flex items-center gap-2 py-0.5">
            <Radio
              name="provider-default"
              checked={selected.has(o.id)}
              onCheckedChange={() => onToggle(o.id)}
            />
            <span className="min-w-0 flex-1 truncate text-sm">{o.name}</span>
            {wireFor(o)}
          </div>
        ))}
        <div className="flex items-center gap-2 py-0.5">
          <Radio
            name="provider-default"
            checked={noneSelected}
            onCheckedChange={onClear}
          />
          <span className="min-w-0 flex-1 truncate text-sm text-muted">
            {t("agents.noneFactory")}
          </span>
          {noneSelected && (
            <span className="shrink-0 text-xs text-success">{t("agents.restored")}</span>
          )}
        </div>
      </div>
    );
  }
  // Multi-slot agents (checkbox semantics): compact borderless rows with a
  // text-only "default" marker on the chosen row.
  return (
    <div className="space-y-0.5">
      {options.map((o) => {
        const checked = selected.has(o.id);
        const isDefault = defaultId === o.id && checked;
        return (
          <div key={o.id} className="flex items-center gap-2 py-0.5">
            <Checkbox
              checked={checked}
              onCheckedChange={() => onToggle(o.id)}
            />
            <span className="min-w-0 flex-1 truncate text-sm">{o.name}</span>
            {wireFor(o)}
            {checked &&
              (isDefault ? (
                <span className="shrink-0 text-xs text-success">{t("agents.defaultMarker")}</span>
              ) : (
                <Button
                  variant="ghost"
                  size="xs"
                  onClick={() => onSetDefault(o.id)}
                  title={t("agents.makeDefaultTitle", { name: o.name })}
                >
                  {t("agents.setDefault")}
                </Button>
              ))}
          </div>
        );
      })}
    </div>
  );
}
