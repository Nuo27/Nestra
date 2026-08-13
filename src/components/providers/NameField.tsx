import { useTranslation } from "react-i18next";
import type { FormState } from "../../lib/providerForm";
import { Card } from "../controls/Card";
import { Input } from "../ui/input";

export function NameField({
  form,
  onChange,
}: {
  form: FormState;
  onChange: (v: string) => void;
}) {
  const { t } = useTranslation();
  return (
    <Card title={t("providerEdit.name")} hint={t("providerEdit.nameHint")}>
      <Input
        value={form.display_name}
        onChange={(e) => onChange(e.target.value)}
      />
    </Card>
  );
}
