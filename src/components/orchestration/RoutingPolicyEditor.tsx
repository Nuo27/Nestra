import { useState } from "react";
import { useTranslation } from "react-i18next";
import { Trans } from "react-i18next";
import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { Plus, Trash2 } from "lucide-react";
import {
  agentList,
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
import { ROLE_CHIP_ACTIVE, ROLE_CHIP_BASE, ROLE_CHIP_DONE } from "./RoleChip";
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
import { AffinityScope, type AffinityScopeValue } from "./AffinityScope";
import { RoleKey } from "./RoleKey";
import { TargetChain } from "./TargetChain";

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
  // Tier presets (haiku/sonnet/opus) only apply to anthropic-tier model
  // selection — derived from the registry flag, not an agent-id literal, so
  // any future tiered agent gets them automatically. Shared qk.agents()
  // cache; undefined while loading hides the presets one frame at most.
  const tiersQ = useQuery({ queryKey: qk.agents(), queryFn: agentList });
  const tiered =
    (tiersQ.data ?? []).find((a) => a.id === agentId)?.model_selection === "anthropic_tiers";
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
  // (targets wiped). Refuse instead — against a FRESH fetch, not the possibly
  // stale cache snapshot (another window may have created the role since).
  const addRole = async (role: string) => {
    const r = role.trim();
    if (!r) return;
    const exists = (list: { role: string }[]) => list.some((p) => p.role === r);
    if (exists(policies)) {
      toast(t("routingPolicy.alreadyExists"), "error");
      return;
    }
    try {
      const latest = await qc.fetchQuery({
        queryKey: qk.routingPolicies(agentId),
        queryFn: () => routingPolicyList(agentId),
      });
      if (exists(latest)) {
        toast(t("routingPolicy.alreadyExists"), "error");
        return;
      }
    } catch {
      // Refetch failed — fall through to the local check above rather than
      // blocking the add; the backend's upsert stays the last line of defense.
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
        <div className="border border-border bg-inset px-3 py-2 font-mono text-2xs text-subtle">
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
        {tiered && (
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

      {tiered && (
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
              ROLE_CHIP_BASE +
              (done ? ROLE_CHIP_DONE : ROLE_CHIP_ACTIVE)
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
              ROLE_CHIP_BASE +
              (done ? ROLE_CHIP_DONE : ROLE_CHIP_ACTIVE)
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
