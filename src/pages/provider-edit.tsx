import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { useNavigate } from "@tanstack/react-router";
import { Trash2 } from "lucide-react";
import {
  endpointAddProtocol,
  endpointClearApiKey,
  endpointDelete,
  endpointGet,
  endpointRemoveProtocol,
  endpointSetAdvancedEnv,
  endpointSetApiKey,
  endpointSetModelAbilities,
  endpointSetModels,
  endpointSetName,
} from "../ipc";
import { Button } from "../components/controls/Button";
import { Card } from "../components/controls/Card";
import { ConfigPreview } from "../components/display/ConfigPreview";
import { Page } from "../components/layout/Page";
import { PageHeader, SectionLabel, BackLink } from "../components/layout/PageHeader";
import { ErrorBanner } from "../components/feedback/ErrorBanner";
import { Skeleton } from "../components/ui/skeleton";
import { confirmDialog } from "../components/controls/ConfirmDialog";
import { extractError } from "../ipc/errors";
import { qk } from "../lib/queries";
import { useUI } from "../stores/ui";
import {
  emptyToForm,
  isDirty,
  serializeAdvancedEnv,
  stableJson,
  ValidationError,
  type FormState,
} from "../lib/providerForm";
import { NameField } from "../components/providers/NameField";
import { ProtocolsCard } from "../components/providers/ProtocolsCard";
import { ProviderKeyCard } from "../components/providers/ProviderKeyCard";
import { ProviderModelsCard } from "../components/providers/ProviderModelsCard";
import { ProviderAdvancedEnvCard } from "../components/providers/ProviderAdvancedEnvCard";

export function ProviderEditPage({ id }: { id: string }) {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const navigate = useNavigate();
  const toast = useUI((s) => s.pushToast);
  const q = useQuery({
    queryKey: qk.endpoint(id),
    queryFn: () => endpointGet(id),
  });

  const [form, setForm] = useState<FormState | null>(null);
  const [formSnapshot, setFormSnapshot] = useState<{
    id: string;
    last_validated_at: number | null;
    models_fetched_at: number | null;
  } | null>(null);

  const e = q.data;
  const dirty = !!form && !!e && isDirty(form, e);

  const saveMut = useMutation({
    mutationFn: async () => {
      if (!form || !e) return;
      const payload = {
        display_name: form.display_name.trim() || e.id,
        models: {
          default: form.models_default,
          haiku: form.models_haiku,
          sonnet: form.models_sonnet,
          opus: form.models_opus,
          available: form.models_available,
        },
        // Empty env keys are transient editor rows — never persist them.
        advanced_env: Object.fromEntries(
          Object.entries(form.advanced_env).filter(([k]) => k.trim() !== ""),
        ),
      };

      if (payload.display_name !== e.display_name) {
        await endpointSetName(id, payload.display_name);
      }
      if (stableJson(payload.models) !== stableJson(e.models ?? {})) {
        await endpointSetModels(id, payload.models);
      }
      if (stableJson(payload.advanced_env) !== stableJson(serializeAdvancedEnv(e.advanced_env))) {
        await endpointSetAdvancedEnv(id, payload.advanced_env);
      }
      if (stableJson(form.model_abilities) !== stableJson(e.model_abilities ?? {})) {
        await endpointSetModelAbilities(id, form.model_abilities);
      }
      // Protocols: diff against the stored set, apply adds/removes on Save.
      const oldProtos = new Map(e.protocols.map((p) => [p.protocol, p.base_url]));
      const newProtos = new Map(
        form.protocols
          .filter((x) => x.protocol.trim())
          .map((x) => [x.protocol.trim(), x.base_url.trim()] as [string, string]),
      );
      for (const proto of oldProtos.keys()) {
        if (!newProtos.has(proto)) await endpointRemoveProtocol(id, proto);
      }
      for (const [proto, url] of newProtos) {
        if (oldProtos.get(proto) !== url) await endpointAddProtocol(id, proto, url);
      }
      if (form.clear_key) {
        await endpointClearApiKey(id);
      } else if (form.api_key.trim()) {
        const r = await endpointSetApiKey(id, form.api_key.trim());
        if (!r.ok) {
          throw new ValidationError(r.message ?? t("providerEdit.invalidKey"));
        }
      }
    },
    onSuccess: () => {
      setForm((f) => (f ? { ...f, api_key: "", clear_key: false } : f));
      qc.invalidateQueries({ queryKey: qk.endpoint(id) });
      qc.invalidateQueries({ queryKey: qk.endpoints() });
      toast(t("providerEdit.saved"), "success");
    },
    onError: () => {
      // The save is a sequence of independent IPC writes — a mid-sequence
      // failure may have persisted SOME of them. Refresh the cached endpoint
      // so the next retry diffs against the real server state instead of a
      // stale snapshot (protocol add/removes are set-diffs).
      qc.invalidateQueries({ queryKey: qk.endpoint(id) });
    },
  });

  const deleteMut = useMutation({
    mutationFn: () => endpointDelete(id),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: qk.endpoints() });
      toast(t("providerEdit.deleted"), "success");
      navigate({ to: "/providers" });
    },
    onError: (err) => {
      toast(
        t("providerEdit.deleteFailed", { err: extractError(err) ?? t("common.error") }),
        "error",
      );
    },
  });

  useEffect(() => {
    if (!q.data) return;
    const data = q.data;
    const sig = {
      id: data.id,
      last_validated_at: data.last_validated_at,
      models_fetched_at: data.models_fetched_at,
    };
    const sigChanged =
      !formSnapshot ||
      formSnapshot.id !== sig.id ||
      formSnapshot.last_validated_at !== sig.last_validated_at ||
      formSnapshot.models_fetched_at !== sig.models_fetched_at ||
      form === null;
    if (sigChanged) {
      // Never clobber an in-progress edit: a background refetch that bumps
      // last_validated_at/models_fetched_at (e.g. after key validation)
      // must not wipe what the user is typing. Refresh the snapshot only.
      if (form && dirty) {
        setFormSnapshot(sig);
      } else {
        setForm(emptyToForm(data));
        setFormSnapshot(sig);
      }
    }
  }, [q.data, form, formSnapshot]);

  if (q.isLoading || !form) {
    return (
      <Page>
        <Skeleton className="h-8 w-1/3" />
        <div className="mt-4 space-y-3">
          <Skeleton className="h-24 w-full" />
          <Skeleton className="h-24 w-full" />
          <Skeleton className="h-24 w-full" />
        </div>
      </Page>
    );
  }
  if (q.isError && !q.data) {
    // A failed INITIAL fetch must not hang on the skeleton forever. A failed
    // background refetch (data still present) keeps the editor open —
    // replacing a form mid-edit with a banner would discard UI state.
    return (
      <Page>
        <ErrorBanner onRetry={() => q.refetch()}>
          {t("providerEdit.loadFailed")}
        </ErrorBanner>
      </Page>
    );
  }
  if (!q.data) {
    return (
      <Page>
        <div className="text-sm text-danger">{t("providerEdit.notFound")}</div>
      </Page>
    );
  }
  const data = q.data;

  const goBack = () => navigate({ to: "/providers" });
  const validationError = extractError(saveMut.error);

  return (
    <Page>
      <PageHeader
        back={<BackLink to="/providers">{t("nav.providers")}</BackLink>}
        title={form.display_name || data.id}
        subtitle={<SectionLabel inline>{t("common.edit")}</SectionLabel>}
        sticky
        action={
          <>
            <Button variant="ghost" size="sm" onClick={goBack} disabled={saveMut.isPending}>{t("common.cancel")}</Button>
            <Button
              variant="primary"
              size="sm"
              loading={saveMut.isPending}
              disabled={!dirty}
              onClick={() => saveMut.mutate()}
            >{t("common.save")}</Button>
          </>
        }
      />

      {validationError && (
        <ErrorBanner severity="error">
          {t("providerEdit.validationError", { err: validationError })}
        </ErrorBanner>
      )}

      <NameField
        form={form}
        onChange={(v) => setForm({ ...form, display_name: v })}
      />
      <ProtocolsCard
        form={form}
        onPatch={(patch) => setForm({ ...form, ...patch })}
      />
      <ProviderKeyCard
        endpoint={data}
        form={form}
        onPatch={(patch) => setForm({ ...form, ...patch })}
      />
      <ProviderModelsCard
        endpoint={data}
        form={form}
        onPatch={(patch) => setForm({ ...form, ...patch })}
      />
      <ProviderAdvancedEnvCard
        form={form}
        onChange={(env) => setForm({ ...form, advanced_env: env })}
      />
      <ConfigPreview endpoint={data} />
      <Card
        title={t("providerEdit.dangerZone")}
        hint={t("providerEdit.dangerHint")}
        tone="danger"
      >
        <div className="flex items-center justify-between gap-3">
          <div className="text-sm text-muted">
            {t("providerEdit.deleteLine", { name: data.display_name })}
          </div>
          <Button
            variant="danger"
            size="sm"
            loading={deleteMut.isPending}
            onClick={async () => {
              const msg = data.has_api_key
                ? t("providerEdit.deleteConfirmHasKey")
                : t("providerEdit.deleteConfirmNoKey");
              const ok = await confirmDialog({
                title: t("providerEdit.deleteConfirmTitle", { name: data.display_name }),
                body: msg,
                confirmLabel: t("providerEdit.deleteConfirmLabel"),
              });
              if (ok) deleteMut.mutate();
            }}
          >
            <Trash2 data-icon size={13} />{t("providerEdit.deleteBtn")}</Button>
        </div>
      </Card>
    </Page>
  );
}
