import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { useTranslation } from "react-i18next";
import { useNavigate } from "@tanstack/react-router";
import { Plus } from "lucide-react";
import {
  endpointAddProtocol,
  endpointCreate,
  endpointCreateWithPreset,
  endpointList,
  type BuiltinKind,
  type CreateWithPresetResult,
} from "../ipc";
import { extractError } from "../ipc/errors";
import { useUI } from "../stores/ui";
import { qk } from "../lib/queries";
import { Button } from "../components/controls/Button";
import { Card } from "../components/controls/Card";
import { Page } from "../components/layout/Page";
import { PageHeader } from "../components/layout/PageHeader";
import { SyncIndicator } from "../components/feedback/SyncIndicator";
import { EmptyState } from "../components/feedback/EmptyState";
import { Skeleton } from "../components/ui/skeleton";
import { EndpointRow } from "../components/providers/EndpointRow";
import { MasonryGrid } from "../components/layout/MasonryGrid";
import {
  CreateProviderDialog,
  slugify,
  type CreateInput,
} from "../components/providers/CreateProviderDialog";

function NewProviderButton({ onClick }: { onClick: () => void }) {
  const { t } = useTranslation();
  return (
    <Button variant="primary" size="sm" onClick={onClick}>
      <Plus data-icon size={14} />{t("providers.newProvider")}</Button>
  );
}

export function ProvidersPage() {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const navigate = useNavigate();
  const q = useQuery({ queryKey: qk.endpoints(), queryFn: endpointList });
  const endpoints = q.data ?? [];
  const [creating, setCreating] = useState(false);
  const toast = useUI((s) => s.pushToast);

  const createMut = useMutation({
    mutationFn: async (input: { displayName: string; id: string }) =>
      endpointCreate({ id: input.id, displayName: input.displayName }),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: qk.endpoints() });
    },
    onError: () => toast(t("providers.createFailed"), "error"),
  });

  /// Single-step create + validate-key flow. Returns the validation result
  /// so the dialog can stay open on failure and route to the edit page; the
  /// endpoint IS created either way (protocols persisted), only the key may
  /// have been rejected.
  const createWithKeyMut = useMutation({
    mutationFn: async (input: {
      id: string;
      displayName: string;
      protocols: { protocol: string; base_url: string }[];
      apiKey: string;
      quotaQuery: BuiltinKind | null;
    }) => endpointCreateWithPreset(input),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: qk.endpoints() });
    },
    onError: () => toast(t("providers.createFailed"), "error"),
  });

  /// The create orchestration: slugify + collision-avoid, then branch on
  /// preset+key (single-step create+validate) vs custom (create + parallel
  /// protocol adds). Lives here, not in the dialog — it owns the two
  /// mutations and the navigation.
  const handleCreate = async (input: CreateInput) => {
    const baseId = slugify(input.display_name) || "provider";
    let finalId = baseId;
    // Avoid collision with existing ids.
    if (endpoints.some((e) => e.id === baseId)) {
      for (let i = 2; i < 1000; i++) {
        const candidate = `${baseId}-${i}`;
        if (!endpoints.some((e) => e.id === candidate)) {
          finalId = candidate;
          break;
        }
      }
    }
    try {
      // Preset + key → single-step create + validate. The endpoint is
      // created with protocols regardless; only the key may be
      // rejected, in which case we route to the edit page to fix it.
      if (input.apiKey && input.protocols.length > 0) {
        const res: CreateWithPresetResult = await createWithKeyMut.mutateAsync({
          id: finalId,
          displayName: input.display_name,
          protocols: input.protocols,
          apiKey: input.apiKey,
          quotaQuery: input.quota_query,
        });
        if (res.validation.ok) {
          toast(t("providers.createdValidated"), "success");
        } else {
          toast(
            res.validation.message
              ? t("providers.keyRejectedWith", { msg: res.validation.message })
              : t("providers.keyRejected"),
            "error",
          );
        }
        setCreating(false);
        navigate({ to: "/providers/$id", params: { id: res.id } });
        return;
      }
      // Custom (or no key) → legacy create + add-protocols path; the
      // user finishes on the edit page.
      const created = await createMut.mutateAsync({
        id: finalId,
        displayName: input.display_name,
      });
      // Parallel protocol adds — the old serial await loop wasted a
      // round-trip per protocol.
      const results = await Promise.allSettled(
        input.protocols.map((proto) =>
          endpointAddProtocol(created.id, proto.protocol, proto.base_url),
        ),
      );
      const protoFailed = results.filter((r) => r.status === "rejected").length;
      if (protoFailed > 0) {
        toast(t("providers.protocolsFailed", { n: protoFailed }), "error");
      } else {
        toast(t("providers.created"), "success");
      }
      setCreating(false);
      navigate({ to: "/providers/$id", params: { id: created.id } });
    } catch {
      // error displayed in dialog
    }
  };

  return (
    <Page width="wide">
      <PageHeader
        title={t("providers.title")}
        info={t("providers.help")}
        action={
          <div className="flex items-center gap-3">
            <SyncIndicator query={q} />
            <NewProviderButton onClick={() => setCreating(true)} />
          </div>
        }
      />

      {q.isLoading && (
        <MasonryGrid>
          {[
            { h1: "h-5", h2: "h-16" },
            { h1: "h-5", h2: "h-10" },
            { h1: "h-5", h2: "h-24" },
          ].map((s, i) => (
            <Card key={i} padding="sm">
              <Skeleton className={`${s.h1} w-full`} />
              <Skeleton className={`mt-2 ${s.h2} w-full`} />
            </Card>
          ))}
        </MasonryGrid>
      )}

      {!q.isLoading && endpoints.length === 0 && (
        <EmptyState
          title={t("providers.none.title")}
          hint={t("providers.none.hint")}
          action={<NewProviderButton onClick={() => setCreating(true)} />}
        />
      )}

      {/* Masonry: round-robin into flex columns — left-to-right reading
          order with gap-free vertical packing (cards of different heights
          stack tightly, no empty space below shorter cards). */}
      <MasonryGrid>
        {endpoints.map((e) => (
          <EndpointRow key={e.id} endpoint={e} />
        ))}
      </MasonryGrid>

      {creating && (
        <CreateProviderDialog
          onCancel={() => setCreating(false)}
          existingIds={endpoints.map((e) => e.id)}
          onSubmit={handleCreate}
          pending={createMut.isPending || createWithKeyMut.isPending}
          error={extractError(createMut.error) ?? extractError(createWithKeyMut.error)}
        />
      )}
    </Page>
  );
}
