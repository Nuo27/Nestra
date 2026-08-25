import { useEffect, useState } from "react"
import { useTranslation } from "react-i18next"
import { X } from "lucide-react"
import { useUI, type ToastTone } from "../../stores/ui"
import { ConfirmHost } from "../controls/ConfirmDialog"
import { Button } from "../controls/Button"

const TONE: Record<ToastTone, { glyph: string; accent: string }> = {
  success: { glyph: "✓", accent: "text-success" },
  error: { glyph: "!", accent: "text-danger" },
  default: { glyph: "i", accent: "text-muted" },
}

// Toast exit transition is the same 150ms `ease-out` as the enter; the
// toast element stays mounted for that long after dismissal so the
// `animate-out` utility can run. (DESIGN.md §11.)
const EXIT_MS = 150

/// Renders the toast stack (bottom-right) and the singleton confirm host.
/// Mounted once at the app root. Terminal style: flat, single-glyph prefix.
export function Toaster() {
  const { t } = useTranslation()
  const toasts = useUI((s) => s.toasts)
  const dismiss = useUI((s) => s.dismissToast)
  // Track which toasts are currently playing their exit animation.
  const [leaving, setLeaving] = useState<Set<number>>(new Set())
  // When a toast leaves the upstream list but we still want to animate it
  // out, snapshot it here so its final frame can play.
  const [exiting, setExiting] = useState<typeof toasts>([])

  useEffect(() => {
    const liveIds = new Set(toasts.map((x) => x.id))
    // Newly dismissed toasts (in exiting snapshot but no longer live): start
    // the exit animation, then prune after EXIT_MS.
    const newlyDismissed = exiting.filter((x) => !liveIds.has(x.id))
    if (newlyDismissed.length === 0) {
      setExiting(toasts)
      return
    }
    setLeaving((prev) => {
      const next = new Set(prev)
      for (const x of newlyDismissed) next.add(x.id)
      return next
    })
    const timer = window.setTimeout(() => {
      setLeaving((prev) => {
        if (prev.size === 0) return prev
        const next = new Set(prev)
        for (const x of newlyDismissed) next.delete(x.id)
        return next
      })
      setExiting(toasts)
    }, EXIT_MS)
    return () => window.clearTimeout(timer)
  }, [toasts, exiting])

  const visible = exiting
  return (
    <>
      <ConfirmHost />
      {visible.length > 0 && (
        <div className="pointer-events-none fixed bottom-4 right-4 z-[60] flex flex-col gap-2">
          {visible.map((toast) => {
            const { glyph, accent } = TONE[toast.tone]
            const isLeaving = leaving.has(toast.id)
            return (
              <div
                key={toast.id}
                role={toast.tone === "error" ? "alert" : "status"}
                className={`pointer-events-auto flex items-start gap-2 border border-border bg-overlay px-3 py-2 text-sm text-fg duration-fast ease-out ${
                  isLeaving
                    ? "animate-out fade-out slide-out-to-right-4 fill-mode-forwards"
                    : "animate-in slide-in-from-right-4 fade-in-0"
                }`}
              >
                <span className={`mt-0.5 shrink-0 text-xs leading-relaxed ${accent}`}>
                  {glyph}
                </span>
                <div className="min-w-0 max-w-[20rem] break-words leading-relaxed">
                  {toast.message}
                </div>
                <Button
                  onClick={() => dismiss(toast.id)}
                  variant="ghost"
                  size="xs"
                  className="-mr-1 -mt-1 shrink-0"
                  aria-label={t("common.dismiss")}
                >
                  <X data-icon size={14} />
                </Button>
              </div>
            )
          })}
        </div>
      )}
    </>
  )
}