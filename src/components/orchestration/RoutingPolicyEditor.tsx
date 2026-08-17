import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Trans } from "react-i18next";
import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { Plus, Trash2 } from "lucide-react";
import {
  endpointList,
  type EndpointInfo,
} from "../../ipc";
import {
  routingPolicyList,
  routingPolicyUpsert,
  routingPolicyDelete,
  detectedRoles,
  type RoutingPolicyRow,
  type RoutingPolicyInput,
} from "../../ipc/orchestration";
import { qk } from "../../lib/queries";
import { useUI } from "../../stores/ui";
import { ErrorBanner } from "../feedback/ErrorBanner";
import { Card } from "../controls/Card";
import { Button } from "../controls/Button";
import { ButtonGroup } from "../controls/ButtonGroup";
import { Field, FieldRow } from "../controls/Field";
import { Input } from "../ui/input";
import { Switch } from "../ui/switch";
import { Badge } from "../ui/badge";
import { Skeleton } from "../ui/skeleton";
import { OrderedChain } from "../controls/OrderedChain";
import { AffinityScope, type AffinityScopeValue } from "./AffinityScope";
import { RoleKey } from "./RoleKey";

/// Per-agent routing-policy editor. Lists every `routing_policy` row for the
/// agent (keyed by role — `*` is the catch-all), and lets the user edit each:
/// preferred + fallback endpoint chains, allowed-model globs, affinity scope,
/// migrate-on-quota, and inject-cache-control. Endpoint options come from the
/// live endpoints list so chains reference real ids.
///
/// Roles are free-form keys (the backend keys them by
/// `SubagentRole::as_policy_key()`); the editor surfaces whatever rows exist
/// plus a way to add a catch-all or per-role row. The router is the real
/// producer of per-role rows; for now the user can author them.
export function RoutingPolicyEditor({ agentId }: { agentId: string }) {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const toast = useUI((s) => s.pushToast);
  const [newRole, setNewRole] = useState("");

  const endpointsQ = useQuery({
    queryKey: qk.endpoints(),
    queryFn: endpointList,
  });
  const policiesQ = useQuery({
    queryKey: qk.routingPolicies(agentId),
    queryFn: () => routingPolicyList(agentId),
  });

  const upsertMut = useMutation({
    mutationFn: (input: RoutingPolicyInput) => routingPolicyUpsert(input),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: qk.routingPolicies(agentId) });
    },
    onError: () => toast(t("routingPolicy.notSaved"), "error"),
  });

  const deleteMut = useMutation({
    mutationFn: ({ role }: { role: string }) => routingPolicyDelete(agentId, role),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: qk.routingPolicies(agentId) });
      toast(t("routingPolicy.deleted"), "success");
    },
    onError: () => toast(t("routingPolicy.notDeleted"), "error"),
  });

  const addMut = useMutation({
    mutationFn: (role: string) =>
      routingPolicyUpsert({
        agent_id: agentId,
        role,
        preferred_endpoints: null,
        fallback_endpoints: null,
        allowed_models: null,
        migrate_on_quota: true,
        inject_cache_control: false,
        affinity_scope: "task",
      }),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: qk.routingPolicies(agentId) });
      setNewRole("");
      toast(t("routingPolicy.created"), "success");
    },
    onError: () => toast(t("routingPolicy.notCreated"), "error"),
  });

  const endpoints = endpointsQ.data ?? [];
  const policies = policiesQ.data ?? [];

  // Guarded role-add: `routingPolicyUpsert` is ON CONFLICT DO UPDATE, so
  // adding a role that already exists would silently OVERWRITE the whole row
  // (preferred/fallback/allowed_models wiped). Refuse instead.
  const addRole = (role: string) => {
    const r = role.trim();
    if (!r) return;
    if (policies.some((p) => p.role === r)) {
      toast(t("routingPolicy.alreadyExists"), "error");
      return;
    }
    addMut.mutate(r);
  };

  if (policiesQ.isLoading || endpointsQ.isLoading) {
    return (
      <div className="space-y-3">
        {[0, 1].map((i) => (
          <Card key={i} padding="md">
            <Skeleton className="h-4 w-32" />
            <Skeleton className="mt-3 h-8 w-full" />
            <Skeleton className="mt-2 h-8 w-full" />
          </Card>
        ))}
      </div>
    );
  }

  if (policiesQ.isError || endpointsQ.isError) {
    // A failed load must not render as "no saved policies" (the user could
    // then author rows the backend can't honor, or see an empty endpoint
    // picker).
    return (
      <ErrorBanner
        onRetry={() => {
          policiesQ.refetch();
          endpointsQ.refetch();
        }}
      >
        {t("routingPolicy.loadFailed")}
      </ErrorBanner>
    );
  }

  if (policies.length === 0) {
    // No rows persisted yet — but routing is NOT idle: the backend serves the
    // synthetic catch-all default (`store::RoutingPolicyRow::default_for`):
    // migrate_on_quota=true, inject_cache_control=false, task affinity. Render
    // that default as an editable preview so the user sees what is ACTUALLY in
    // effect and can save it (persisting a row) or tune it. `onDelete` is
    // omitted because a not-yet-persisted default has nothing to delete.
    const defaultRow: RoutingPolicyRow = {
      agent_id: agentId,
      role: "*",
      preferred_endpoints: null,
      fallback_endpoints: null,
      allowed_models: null,
      migrate_on_quota: true,
      inject_cache_control: false,
      affinity_scope: "task",
      updated_at: 0,
    };
    return (
      <div className="space-y-3">
        <div className="rounded border border-border bg-inset px-3 py-2 font-mono text-2xs text-subtle">
          <Trans
            i18nKey="routingPolicy.noPolicies"
            components={{ mono: <span className="text-fg" /> }}
          />
        </div>
        <PolicyRow
          policy={defaultRow}
          endpoints={endpoints}
          onSave={(input) => upsertMut.mutate(input)}
          pending={upsertMut.isPending}
        />
        <AddRoleRow
          value={newRole}
          onChange={setNewRole}
          onAdd={() => addRole(newRole)}
          pending={addMut.isPending}
        />
        {agentId === "claude-code-cli" && (
          <TierPresets policies={policies} onAdd={addRole} pending={addMut.isPending} />
        )}
        <DetectedRolesStrip
          agentId={agentId}
          policies={policies}
          onAdd={addRole}
          pending={addMut.isPending}
        />
      </div>
    );
  }

  return (
    <div className="space-y-3">
      {policies.map((p) => (
        <PolicyRow
          // Key by role+updated_at: the draft state initializes once, so an
          // external update (saved elsewhere, or a fresh fetch) must remount
          // the row or the stale draft would overwrite it on the next Save.
          key={`${p.role}:${p.updated_at}`}
          policy={p}
          endpoints={endpoints}
          onSave={(input) => upsertMut.mutate(input)}
          onDelete={() => deleteMut.mutate({ role: p.role })}
          pending={upsertMut.isPending}
        />
      ))}

      <AddRoleRow
        value={newRole}
        onChange={setNewRole}
        onAdd={() => addRole(newRole)}
        pending={addMut.isPending}
      />

      {agentId === "claude-code-cli" && (
        <TierPresets policies={policies} onAdd={addRole} pending={addMut.isPending} />
      )}

      <DetectedRolesStrip
        agentId={agentId}
        policies={policies}
        onAdd={addRole}
        pending={addMut.isPending}
      />
    </div>
  );
}

/// Detected-role suggestion strip: subagent roles the gateway has actually
/// observed for this agent (`route_request.subagent_role`), most recent
/// first. Clicking an unconfigured role creates its policy row with defaults
/// (same shape as AddRoleRow); roles that already have a policy are marked
/// "configured" and are not clickable. Hidden when nothing was detected.
function DetectedRolesStrip({
  agentId,
  policies,
  onAdd,
  pending,
}: {
  agentId: string;
  policies: RoutingPolicyRow[];
  onAdd: (role: string) => void;
  pending: boolean;
}) {
  const { t } = useTranslation();
  const q = useQuery({
    queryKey: qk.detectedRoles(agentId),
    queryFn: () => detectedRoles(agentId),
  });
  const roles = q.data ?? [];
  if (roles.length === 0) return null;
  const configured = new Set(policies.map((p) => p.role));
  return (
    <div className="flex flex-wrap items-center gap-1.5">
      <span className="font-mono text-2xs text-subtle">{t("routingPolicy.detected")}</span>
      {roles.map((r) => {
        const done = configured.has(r.role);
        return (
          <button
            key={r.role}
            type="button"
            disabled={done || pending}
            onClick={() => !done && onAdd(r.role)}
            title={
              done
                ? t("routingPolicy.configuredTip")
                : t("routingPolicy.createTip", { role: r.role })
            }
            className={
              "inline-flex items-center gap-1.5 rounded border border-border bg-inset px-1.5 py-0.5 font-mono text-2xs transition-colors duration-fast " +
              (done
                ? "cursor-default text-muted"
                : "text-fg hover:border-accent/50 hover:bg-raised disabled:opacity-50")
            }
          >
            <RoleKey roleKey={r.role} />
            <span className="text-subtle tabular">×{r.request_count}</span>
            {done && <span className="text-accent">{t("routingPolicy.configured")}</span>}
          </button>
        );
      })}
    </div>
  );
}

/// Budget-tier preset strip. Claude Code sends each of its model env slots
/// (haiku/sonnet/opus) as a distinct id, so the gateway can classify a
/// request's tier and match a `tier:*` policy row — e.g. steer background
/// haiku-tier traffic to a cheaper endpoint. Only rendered for agents whose
/// requests classify (Claude Code); the lookup order is exact role → tier →
/// `*` catch-all.
function TierPresets({
  policies,
  onAdd,
  pending,
}: {
  policies: RoutingPolicyRow[];
  onAdd: (role: string) => void;
  pending: boolean;
}) {
  const { t } = useTranslation();
  const configured = new Set(policies.map((p) => p.role));
  return (
    <div className="flex flex-wrap items-center gap-1.5">
      <span className="font-mono text-2xs text-subtle">{t("routingPolicy.tierPresets")}</span>
      {(["haiku", "sonnet", "opus"] as const).map((tier) => {
        const role = `tier:${tier}`;
        const done = configured.has(role);
        return (
          <button
            key={role}
            type="button"
            disabled={done || pending}
            onClick={() => !done && onAdd(role)}
            title={t("routingPolicy.tierTip", { role })}
            className={
              "inline-flex items-center gap-1.5 rounded border border-border bg-inset px-1.5 py-0.5 font-mono text-2xs transition-colors duration-fast " +
              (done
                ? "cursor-default text-muted"
                : "text-fg hover:border-accent/50 hover:bg-raised disabled:opacity-50")
            }
          >
            <RoleKey roleKey={role} />
            {done && <span className="text-accent">{t("routingPolicy.configured")}</span>}
          </button>
        );
      })}
    </div>
  );
}

function AddRoleRow({
  value,
  onChange,
  onAdd,
  pending,
}: {
  value: string;
  onChange: (v: string) => void;
  onAdd: () => void;
  pending: boolean;
}) {
  const { t } = useTranslation();
  return (
    <Field
      label={t("routingPolicy.addRoleLabel")}
      hint={t("routingPolicy.addRoleHint")}
    >
      <div className="flex items-center gap-2">
        <Input
          value={value}
          onChange={(e) => onChange(e.target.value)}
          placeholder={t("routingPolicy.addPlaceholder")}
          onKeyDown={(e) => {
            if (e.key === "Enter" && value.trim() && !pending) {
              e.preventDefault();
              onAdd();
            }
          }}
        />
        <Button
          variant="secondary"
          size="sm"
          loading={pending}
          disabled={!value.trim()}
          onClick={onAdd}
        >
          <Plus data-icon size={14} />{t("common.add")}</Button>
      </div>
    </Field>
  );
}

function PolicyRow({
  policy,
  endpoints,
  onSave,
  onDelete,
  pending,
}: {
  policy: RoutingPolicyRow;
  endpoints: EndpointInfo[];
  onSave: (input: RoutingPolicyInput) => void;
  onDelete?: () => void;
  pending: boolean;
}) {
  const { t } = useTranslation();
  // Local draft state mirrors the row; "Save" commits via onSave.
  const [preferred, setPreferred] = useState<string[]>(policy.preferred_endpoints ?? []);
  const [fallback, setFallback] = useState<string[]>(policy.fallback_endpoints ?? []);
  const [allowedModels, setAllowedModels] = useState<string[]>(
    policy.allowed_models ?? [],
  );
  const [migrateOnQuota, setMigrateOnQuota] = useState(policy.migrate_on_quota);
  const [injectCache, setInjectCache] = useState(policy.inject_cache_control);
  const [affinity, setAffinity] = useState<AffinityScopeValue>(policy.affinity_scope);

  const isCatchAll = policy.role === "*";
  const dirty =
    JSON.stringify(preferred) !== JSON.stringify(policy.preferred_endpoints ?? []) ||
    JSON.stringify(fallback) !== JSON.stringify(policy.fallback_endpoints ?? []) ||
    JSON.stringify(allowedModels) !== JSON.stringify(policy.allowed_models ?? []) ||
    migrateOnQuota !== policy.migrate_on_quota ||
    injectCache !== policy.inject_cache_control ||
    affinity !== policy.affinity_scope;

  function save() {
    onSave({
      agent_id: policy.agent_id,
      role: policy.role,
      preferred_endpoints: preferred.length > 0 ? preferred : null,
      fallback_endpoints: fallback.length > 0 ? fallback : null,
      allowed_models: allowedModels.length > 0 ? allowedModels : null,
      migrate_on_quota: migrateOnQuota,
      inject_cache_control: injectCache,
      affinity_scope: affinity,
    });
  }

  return (
    <Card padding="md">
      <div className="flex items-center justify-between gap-3">
        <div className="flex items-center gap-2 min-w-0">
          {isCatchAll ? (
            <Badge tone="accent" variant="soft">
              {t("routingPolicy.catchAll")}
            </Badge>
          ) : null}
          <RoleKey roleKey={policy.role} />
        </div>
        <ButtonGroup className="shrink-0" space="loose">
          <Button variant="primary" size="sm" loading={pending} disabled={!dirty} onClick={save}>{t("common.save")}</Button>
          {onDelete && (
            <Button variant="ghost" size="sm" onClick={onDelete}>
              <Trash2 data-icon size={13} />
            </Button>
          )}
        </ButtonGroup>
      </div>

      <div className="mt-4 grid grid-cols-1 gap-4 sm:grid-cols-2">
        <Field
          label={t("routingPolicy.preferred")}
          hint={t("routingPolicy.preferredHint")}
        >
          <EndpointChainPicker
            value={preferred}
            onChange={setPreferred}
            endpoints={endpoints}
            placeholder={t("routingPolicy.preferredPlaceholder")}
          />
        </Field>
        <Field
          label={t("routingPolicy.fallback")}
          hint={t("routingPolicy.fallbackHint")}
        >
          <EndpointChainPicker
            value={fallback}
            onChange={setFallback}
            endpoints={endpoints}
            placeholder={t("routingPolicy.fallbackPlaceholder")}
          />
        </Field>
      </div>

      <div className="mt-4">
        <Field
          label={t("routingPolicy.allowedModels")}
          hint={t("routingPolicy.allowedModelsHint")}
        >
          <GlobEditor value={allowedModels} onChange={setAllowedModels} />
        </Field>
      </div>

      <div className="mt-4 flex flex-wrap items-center justify-between gap-3 border-t border-border pt-3">
        <FieldRow label={t("routingPolicy.affinityScope")} align="center">
          <AffinityScope value={affinity} onChange={setAffinity} />
        </FieldRow>
        <FieldRow
          label={t("routingPolicy.migrateOnQuota")}
          description={t("routingPolicy.migrateOnQuotaDesc")}
          divider={false}
        >
          <Switch checked={migrateOnQuota} onCheckedChange={setMigrateOnQuota} />
        </FieldRow>
        <FieldRow
          label={t("routingPolicy.injectCache")}
          description={t("routingPolicy.injectCacheDesc")}
          divider={false}
        >
          <Switch checked={injectCache} onCheckedChange={setInjectCache} />
        </FieldRow>
      </div>
    </Card>
  );
}

/// Ordered multi-select over the live providers list. Each chosen provider
/// renders as a removable chip in priority order; unchosen ones appear in the
/// add dropdown. Preserves order (the priority of the preferred/fallback
/// chain). The provider's DEFAULT model rides along (dropdown hint + row
/// tooltip) — routing serves exactly that model, so it must be visible at
/// pick time. Thin wrapper over the shared `OrderedChain`.
function EndpointChainPicker({
  value,
  onChange,
  endpoints,
  placeholder,
}: {
  value: string[];
  onChange: (next: string[]) => void;
  endpoints: EndpointInfo[];
  placeholder: string;
}) {
  const { t } = useTranslation();
  const labelFor = (id: string) =>
    endpoints.find((e) => e.id === id)?.display_name ?? id;
  // The model the router will actually serve from this provider — its
  // models_json default.
  const defaultModel = (id: string) =>
    endpoints.find((e) => e.id === id)?.models?.default ?? "";
  const titleFor = (id: string) => {
    const model = defaultModel(id);
    return model
      ? t("routingPolicy.defaultModelTip", { model })
      : t("routingPolicy.defaultModelNone");
  };
  return (
    <OrderedChain
      ids={value}
      labelFor={labelFor}
      titleFor={titleFor}
      onMove={(from, to) => {
        const next = [...value];
        const [item] = next.splice(from, 1);
        next.splice(to, 0, item);
        onChange(next);
      }}
      onRemove={(id) => onChange(value.filter((x) => x !== id))}
      onAdd={(id) => onChange([...value, id])}
      addChoices={endpoints
        .filter((e) => !value.includes(e.id))
        .map((e) => ({
          id: e.id,
          label: e.display_name,
          hint: e.models?.default || undefined,
        }))}
      emptyHint={placeholder}
      surface
    />
  );
}

/// Comma-separated glob editor for allowed_models. The parent owns the list;
/// this keeps a LOCAL text state so typing `a, ` (a trailing comma) doesn't
/// round-trip into `a` and swallow the separator — the parsed list is only
/// reported on change, and external updates reset the text. `key` remounts
/// the editor when the parent's list identity changes (external load).
function GlobEditor({
  value,
  onChange,
}: {
  value: string[];
  onChange: (next: string[]) => void;
}) {
  const { t } = useTranslation();
  const [text, setText] = useState(value.join(", "));
  // External load (policy switched / draft reset): re-seed the text.
  useEffect(() => {
    setText(value.join(", "));
  }, [value]);
  return (
    <Input
      value={text}
      onChange={(e) => {
        setText(e.target.value);
        onChange(
          e.target.value
            .split(",")
            .map((s) => s.trim())
            .filter(Boolean),
        );
      }}
      placeholder={t("routingPolicy.globPlaceholder")}
      className="font-mono"
    />
  );
}
