import type { ReactNode } from "react"
import { useTranslation } from "react-i18next"
import { GripVertical } from "lucide-react"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "../ui/select"
import { ButtonGroup } from "./ButtonGroup"

/// Ordered list of ids with priority markers (index = priority) and optional
/// reorder/remove/add affordances. The shared merge of the read-only
/// FallbackChain and the editable EndpointChainPicker:
///   • `onMove`   → show ▲▼ move buttons (drag-free reorder)
///   • `onRemove` → show ✕ remove buttons
///   • `onAdd`    → show the "+ add provider…" dropdown over `addChoices`
/// `titleFor` puts a native tooltip on each chosen row (e.g. the provider's
/// default model); `addChoices[].hint` shows secondary text inside the
/// dropdown items for the same purpose at pick time.
export function OrderedChain({
  ids,
  labelFor = (id) => id,
  titleFor,
  onMove,
  onRemove,
  onAdd,
  addChoices = [],
  emptyHint,
  surface = false,
}: {
  ids: string[]
  labelFor?: (id: string) => string
  titleFor?: (id: string) => string
  onMove?: (from: number, to: number) => void
  onRemove?: (id: string) => void
  onAdd?: (id: string) => void
  addChoices?: { id: string; label: string; hint?: string }[]
  emptyHint?: ReactNode
  surface?: boolean
}) {
  const { t } = useTranslation()
  const editable = onMove || onRemove
  const move = (from: number, to: number) => {
    if (!onMove || to < 0 || to >= ids.length) return
    onMove(from, to)
  }

  return (
    <div className={surface ? "surface-field flex flex-col gap-1 p-2" : "flex flex-col"}>
      {ids.length === 0 && emptyHint && (
        <div className="prose px-1 py-0.5 text-xs text-subtle">{emptyHint}</div>
      )}
      {ids.length > 0 && (
        <ol className="flex flex-col">
          {ids.map((id, i) => (
            <li
              key={id}
              className="flex items-center gap-2 border-b border-border px-2 py-1.5 last:border-b-0"
            >
              <span className="w-4 shrink-0 text-right font-mono text-2xs text-subtle tabular">
                {i + 1}
              </span>
              <span
                className="min-w-0 flex-1 truncate font-mono text-xs text-fg"
                title={titleFor ? titleFor(id) : undefined}
              >
                {labelFor(id)}
              </span>
              {editable && (
                <ButtonGroup className="text-subtle">
                  {onMove && (
                    <>
                      <button
                        type="button"
                        aria-label={t("common.moveUp", { name: labelFor(id) })}
                        onClick={() => move(i, i - 1)}
                        disabled={i === 0}
                        className="brackets-state px-0.5 text-2xs hover:text-fg disabled:pointer-events-none disabled:opacity-30"
                      >
                        ▲
                      </button>
                      <button
                        type="button"
                        aria-label={t("common.moveDown", { name: labelFor(id) })}
                        onClick={() => move(i, i + 1)}
                        disabled={i === ids.length - 1}
                        className="brackets-state px-0.5 text-2xs hover:text-fg disabled:pointer-events-none disabled:opacity-30"
                      >
                        ▼
                      </button>
                    </>
                  )}
                  {onRemove && (
                    <button
                      type="button"
                      aria-label={t("common.removeItem", { name: labelFor(id) })}
                      onClick={() => onRemove(id)}
                      className="px-0.5 text-2xs hover:text-danger"
                    >
                      ✕
                    </button>
                  )}
                  {onMove && !onRemove && (
                    <GripVertical data-icon size={12} className="opacity-30" />
                  )}
                </ButtonGroup>
              )}
            </li>
          ))}
        </ol>
      )}
      {onAdd && addChoices.length > 0 && (
        // key = choice count so the select remounts (resetting its value) when
        // the option set changes after an add/remove.
        <Select key={addChoices.length} value="" onValueChange={onAdd}>
          <SelectTrigger
            size="sm"
            className="mt-1 h-6 w-auto border-0 bg-transparent px-1 text-xs text-muted shadow-none hover:border-0 [&_[data-icon]]:size-3"
          >
            <SelectValue placeholder={t("common.addEndpoint")} />
          </SelectTrigger>
          <SelectContent>
            {addChoices.map((c) => (
              <SelectItem key={c.id} value={c.id}>
                {c.label}
                {c.hint && (
                  <span className="ml-2 font-mono text-2xs text-subtle">{c.hint}</span>
                )}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      )}
    </div>
  )
}
