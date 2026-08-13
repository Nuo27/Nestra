import { useTranslation } from "react-i18next";
import type { FormState } from "../../lib/providerForm";
import { Card } from "../controls/Card";
import { EnvEditor } from "../controls/EnvEditor";

export function ProviderAdvancedEnvCard({
  form,
  onChange,
}: {
  form: FormState;
  onChange: (env: Record<string, string>) => void;
}) {
  const { t } = useTranslation();
  return (
    <Card
      title={t("providerEdit.advancedEnv")}
      hint={t("providerEdit.advancedEnvHint")}
    >
      <EnvEditor
        title={t("providerEdit.envVariables")}
        pairs={form.advanced_env}
        onChange={onChange}
      />
    </Card>
  );
}
