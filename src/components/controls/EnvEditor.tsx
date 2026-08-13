import { useTranslation } from "react-i18next"
import { Plus, X } from "lucide-react"
import { Button } from "./Button"
import { Input } from "../ui/input"

/// Canonical editable key/value list. `pairs` is the source map; edits call
/// `onChange` with a new map. Used for MCP global env, per-CLI env overrides,
/// and provider advanced env. The single replacement for mcp's local
/// EnvEditor + provider-edit's AdvancedEnvCard rows.
export function EnvEditor({
  title,
  pairs,
  onChange,
  addLabel,
  keyPlaceholder,
}: {
  title: string
  pairs: Record<string, string>
  onChange: (next: Record<string, string>) => void
  addLabel?: string
  keyPlaceholder?: string
}) {
  const { t } = useTranslation()
  const addLabelT = addLabel ?? t("env.addVar")
  const keyPh = keyPlaceholder ?? t("env.keyPlaceholder")
  const entries = Object.entries(pairs)
  const setKey = (oldKey: string, newKey: string) => {
    // Prototype-pollution guard: these keys trigger object-prototype
    // setters and can never be saved by the backend anyway.
    if (newKey === "__proto__" || newKey === "constructor" || newKey === "prototype") return
    // Empty keys would merge onto "" — refuse.
    if (!newKey.trim()) return
    // Renaming onto an existing key would silently overwrite that row.
    if (newKey !== oldKey && Object.prototype.hasOwnProperty.call(pairs, newKey)) return
    const next = { ...pairs }
    const v = next[oldKey]
    delete next[oldKey]
    next[newKey] = v ?? ""
    onChange(next)
  }
  const setValue = (key: string, value: string) => {
    onChange({ ...pairs, [key]: value })
  }
  const remove = (key: string) => {
    const next = { ...pairs }
    delete next[key]
    onChange(next)
  }
  const add = () => {
    let k = "NEW_KEY"
    let i = 1
    while (pairs[k] !== undefined) k = `NEW_KEY_${++i}`
    onChange({ ...pairs, [k]: "" })
  }

  return (
    <div className="space-y-1.5">
      <div className="flex items-center justify-between">
        <span className="text-2xs font-semibold uppercase tracking-[0.08em] text-subtle">
          {title}
        </span>
        <Button
          variant="subtle"
          size="xs"
          onClick={add}
          title={addLabelT}
          aria-label={addLabelT}
        >
          <Plus data-icon size={12} />
        </Button>
      </div>
      {entries.length === 0 && (
        <div className="text-xs italic text-subtle">{t("env.none")}</div>
      )}
      {/* Stable index key: keying by the editable name would REMOUNT the row
          on every rename keystroke and drop focus. All inputs here are
          controlled, so index keys are safe (no internal state to confuse). */}
      {entries.map(([k, v], i) => (
        <div key={i} className="flex gap-1.5">
          <Input
            size="sm"
            value={k}
            onChange={(e) => setKey(k, e.target.value)}
            placeholder={keyPh}
            className="flex-1 font-mono"
          />
          <Input
            size="sm"
            value={v}
            onChange={(e) => setValue(k, e.target.value)}
            placeholder={t("env.valuePlaceholder")}
            className="flex-1 font-mono"
          />
          <Button type="button" variant="ghost" size="sm" onClick={() => remove(k)} title={t("env.removeVar")} aria-label={t("env.removeVar")}>
            <X data-icon size={13} />
          </Button>
        </div>
      ))}
    </div>
  )
}
