import { useEffect, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Trans, useTranslation } from "react-i18next";
import {
  opencodeGetCreds,
  opencodeSetCreds,
  type OpencodeCredsStatus,
  type RefreshSettings,
} from "../../ipc";
import { extractError } from "../../ipc/errors";
import { qk } from "../../lib/queries";
import { FieldRow } from "./Field";
import { Input } from "../ui/input";
import { Button } from "./Button";

/// OpenCode Go dashboard credentials editor. The usage query scrapes the
/// authenticated dashboard, so it needs the browser `auth` session cookie
/// (from opencode.ai) + the workspace ID. The cookie is stored encrypted
/// (never read back here — only `has_cookie`); the workspace ID is non-secret.
/// Saving clears the verified state so a fetch must re-confirm data.
export function OpencodeCredsFields({ endpointId }: { endpointId: string }) {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const credsQ = useQuery({
    queryKey: qk.opencodeCreds(endpointId),
    queryFn: () => opencodeGetCreds(endpointId),
    // This is an editor: once the user types, a silent background refetch
    // (window focus / reconnect — the cookie workflow requires alt-tabbing
    // to the browser) would clobber the edited field and re-lock Save with
    // no error anywhere. Keep refetches explicit (invalidation after save)
    // only.
    refetchOnWindowFocus: false,
    refetchOnReconnect: false,
  });
  const status: OpencodeCredsStatus = credsQ.data ?? {
    workspace_id: null,
    has_cookie: false,
  };
  const [workspaceId, setWorkspaceId] = useState("");
  const [cookie, setCookie] = useState("");
  // Whether the user has edited either field since load. While untouched the
  // loaded status may seed the workspace field; once touched, user input
  // always wins over refetched server state.
  const [touched, setTouched] = useState(false);
  const [saved, setSaved] = useState(false);
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);

  // Seed the workspace field from the loaded status, but only until the user
  // edits it (see `touched` above).
  useEffect(() => {
    if (!touched && status.workspace_id != null) setWorkspaceId(status.workspace_id);
  }, [status.workspace_id, touched]);

  // Compare against the server's trimmed form so a trailing space on the
  // user's input doesn't leave Save permanently armed after a successful save.
  const dirty =
    cookie.trim().length > 0 || workspaceId.trim() !== (status.workspace_id ?? "");

  const save = async () => {
    setSaving(true);
    setSaveError(null);
    try {
      // Empty cookie keeps the existing one (server side); only overwrite
      // when the user typed a new value.
      await opencodeSetCreds(endpointId, cookie, workspaceId);
      setCookie("");
      setSaved(true);
      // Mirror the new workspace ID into the quota-settings cache BEFORE the
      // refetch lands: `save()`/`patch()` on this page rewrite the whole
      // blob from the cache, so a stale cache here would let an unrelated
      // settings write re-erase the value we just persisted.
      qc.setQueryData<RefreshSettings>(qk.quotaRefresh(), (old) => {
        const base: RefreshSettings = old ?? { endpoints: {} };
        const ep = base.endpoints[endpointId] ?? {};
        return {
          ...base,
          endpoints: {
            ...base.endpoints,
            [endpointId]: {
              ...ep,
              opencode_workspace_id: workspaceId.trim() || null,
              provisioned: false,
            },
          },
        };
      });
      await qc.invalidateQueries({ queryKey: qk.opencodeCreds(endpointId) });
      await qc.invalidateQueries({ queryKey: qk.quotaRefresh() });
      await qc.invalidateQueries({ queryKey: qk.endpointQuota(endpointId) });
    } catch (e) {
      // A rejected save must surface as user feedback, never be swallowed.
      setSaveError(extractError(e) ?? String(e));
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="border-t border-border pt-3 space-y-2.5">
      <FieldRow label={t("quota.workspaceId")}>
        <Input
          size="sm"
          className="w-64 font-mono"
          value={workspaceId}
          placeholder={t("quota.workspacePlaceholder")}
          onChange={(e) => {
            setWorkspaceId(e.target.value);
            setTouched(true);
            setSaved(false);
          }}
        />
      </FieldRow>
      <FieldRow label={t("quota.authCookie")}>
        <Input
          size="sm"
          type="password"
          className="w-96 font-mono"
          value={cookie}
          placeholder={status.has_cookie ? t("quota.cookieSetPlaceholder") : t("quota.cookiePlaceholder")}
          onChange={(e) => {
            setCookie(e.target.value);
            setTouched(true);
            setSaved(false);
          }}
        />
      </FieldRow>
      <div className="flex items-center justify-end gap-3">
        <Button
          variant="primary"
          size="sm"
          disabled={saving || !dirty}
          loading={saving}
          onClick={save}
        >
          {saving ? t("quota.saving") : t("common.save")}
        </Button>
        {saved && !dirty && (
          <span className="font-mono text-xs text-success">{t("quota.savedVerify")}</span>
        )}
        {saveError && (
          <span className="font-mono text-xs text-danger">{saveError}</span>
        )}
      </div>
      <p className="prose text-xs text-subtle leading-relaxed">
        <Trans i18nKey="quota.credsHint" components={{ code: <code className="font-mono" /> }} />
      </p>
    </div>
  );
}
