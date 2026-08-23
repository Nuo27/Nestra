import { useState } from "react";
import { useTranslation } from "react-i18next";
import { Trans } from "react-i18next";
import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { ArrowDown, ArrowUp, Plus, Trash2 } from "lucide-react";
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
  type RouteTarget,
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
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "../ui/select";
import { AffinityScope, type AffinityScopeValue } from "./AffinityScope";
import { RoleKey } from "./RoleKey";

/// Per-agent routing-policy editor. Lists every `routing_policy` row for the
/// agent (keyed by role — `*` is the mandatory catch-all), and lets the user
/// edit each: the ORDERED (provider, model) route-target list (the router
/// serves the first healthy entry and failures walk the list), affinity
/// scope, migrate-on-quota, and inject-cache-control.
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
        route_targets: [],
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
  // (targets wiped). Refuse instead.
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
    // No rows persisted yet — routing FAILS CLOSED for this agent until a
    // `*` row with targets exists (the router no longer synthesizes a
    // routing default from bindings). Render the empty catch-all as an
    // editable draft so the user can configure it in place.
    const defaultRow: RoutingPolicyRow = {
      agent_id: agentId,
      role: "*",
      route_targets: [],
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
/// haiku-tier traffic to a cheaper target. Only rendered for agents whose
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
  const [targets, setTargets] = useState<RouteTarget[]>(policy.route_targets);
  const [migrateOnQuota, setMigrateOnQuota] = useState(policy.migrate_on_quota);
  const [injectCache, setInjectCache] = useState(policy.inject_cache_control);
  const [affinity, setAffinity] = useState<AffinityScopeValue>(policy.affinity_scope);

  // The `*` catch-all is the mandatory default policy — it cannot be
  // deleted, only cleared (the backend refuses the delete too).
  const isCatchAll = policy.role === "*";
  const dirty =
    JSON.stringify(targets) !== JSON.stringify(policy.route_targets) ||
    migrateOnQuota !== policy.migrate_on_quota ||
    injectCache !== policy.inject_cache_control ||
    affinity !== policy.affinity_scope;

  function save() {
    onSave({
      agent_id: policy.agent_id,
      role: policy.role,
      route_targets: targets,
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
          {!isCatchAll && onDelete && (
            <Button variant="ghost" size="sm" onClick={onDelete}>
              <Trash2 data-icon size={13} />
            </Button>
          )}
        </ButtonGroup>
      </div>

      <div className="mt-4">
        <Field
          label={t("routingPolicy.targets")}
          hint={t("routingPolicy.targetsHint")}
        >
          <TargetChain value={targets} onChange={setTargets} endpoints={endpoints} />
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

/// The ordered (provider, model) route-target list. Each row is an inline
/// provider select + a model select filtered to that provider's models,
/// with up/down/delete controls; the add row picks any provider (its
/// default model pre-fills). Order is priority — the router serves the
/// first healthy entry and failures walk down.
function TargetChain({
  value,
  onChange,
  endpoints,
}: {
  value: RouteTarget[];
  onChange: (next: RouteTarget[]) => void;
  endpoints: EndpointInfo[];
}) {
  const { t } = useTranslation();
  const modelsFor = (id: string): string[] => {
    const ep = endpoints.find((e) => e.id === id);
    const list = ep?.models?.available ?? [];
    const def = ep?.models?.default;
    // Default model first so the picker opens on the provider's serving
    // default even when the list is alphabetical.
    return def && !list.includes(def) ? [def, ...list] : list;
  };
  const defaultModelFor = (id: string): string =>
    endpoints.find((e) => e.id === id)?.models?.default
      ?? endpoints.find((e) => e.id === id)?.models?.available?.[0]
      ?? "";

  const move = (from: number, to: number) => {
    if (to < 0 || to >= value.length) return;
    const next = [...value];
    const [item] = next.splice(from, 1);
    next.splice(to, 0, item);
    onChange(next);
  };
  const addChoices = endpoints.filter(
    (e) => !value.some((v) => v.endpoint === e.id),
  );

  return (
    <div className="space-y-1.5">
      {value.length === 0 && (
        <div className="rounded border border-dashed border-border bg-inset px-3 py-2 text-2xs text-subtle">
          {t("routingPolicy.targetsEmpty")}
        </div>
      )}
      {value.map((target, i) => (
        <div
          key={`${target.endpoint}:${i}`}
          className="flex items-center gap-1.5 rounded border border-border bg-inset px-1.5 py-1"
        >
          <span className="w-5 shrink-0 text-center font-mono text-2xs text-subtle tabular">
            {i + 1}
          </span>
          <Select
            value={target.endpoint}
            onValueChange={(ep) => {
              const next = [...value];
              // Provider switch keeps a valid model: the provider's default.
              next[i] = { endpoint: ep, model: defaultModelFor(ep) };
              onChange(next);
            }}
          >
            <SelectTrigger className="h-7 flex-1 font-mono text-xs">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {endpoints.map((e) => (
                <SelectItem key={e.id} value={e.id} className="font-mono text-xs">
                  {e.display_name}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <Select
            value={target.model}
            onValueChange={(model) => {
              const next = [...value];
              next[i] = { endpoint: target.endpoint, model };
              onChange(next);
            }}
          >
            <SelectTrigger className="h-7 flex-1 font-mono text-xs">
              <SelectValue placeholder={t("routingPolicy.modelPlaceholder")} />
            </SelectTrigger>
            <SelectContent>
              {modelsFor(target.endpoint).map((m) => (
                <SelectItem key={m} value={m} className="font-mono text-xs">
                  {m}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <div className="flex shrink-0 items-center">
            <Button
              variant="ghost"
              size="sm"
              disabled={i === 0}
              onClick={() => move(i, i - 1)}
              aria-label={t("routingPolicy.moveUp")}
            >
              <ArrowUp data-icon size={13} />
            </Button>
            <Button
              variant="ghost"
              size="sm"
              disabled={i === value.length - 1}
              onClick={() => move(i, i + 1)}
              aria-label={t("routingPolicy.moveDown")}
            >
              <ArrowDown data-icon size={13} />
            </Button>
            <Button
              variant="ghost"
              size="sm"
              onClick={() => onChange(value.filter((_, j) => j !== i))}
              aria-label={t("routingPolicy.removeTarget")}
            >
              <Trash2 data-icon size={13} />
            </Button>
          </div>
        </div>
      ))}
      {addChoices.length > 0 && (
        <div className="flex items-center gap-1.5">
          <Select
            value=""
            onValueChange={(ep) => {
              if (!ep) return;
              onChange([...value, { endpoint: ep, model: defaultModelFor(ep) }]);
            }}
          >
            <SelectTrigger className="h-7 flex-1 font-mono text-xs">
              <SelectValue placeholder={t("routingPolicy.addTarget")} />
            </SelectTrigger>
            <SelectContent>
              {addChoices.map((e) => (
                <SelectItem key={e.id} value={e.id} className="font-mono text-xs">
                  {e.display_name}
                  {e.models?.default ? ` · ${e.models.default}` : ""}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
      )}
    </div>
  );
}
