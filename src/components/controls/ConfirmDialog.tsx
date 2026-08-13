import { useEffect, useState, type ReactNode } from "react"
import i18n from "../../i18n"
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "../ui/dialog"
import { Button } from "./Button"

/// Unified confirm dialog. Replaces the three-way split that existed before:
/// `window.confirm` (skills, mcp, provider-edit), `alert` (diagnostics), and
/// Tauri's native `ask()` (sessions). One Radix-based themed dialog everywhere.
///
/// `danger` (default) styles the confirm button red; `primary` uses accent.
/// Returns a promise so callers can `await confirm(...)` like the old
/// `window.confirm` / Tauri `ask` calls.
export function confirmDialog(opts: {
  title: string
  body: ReactNode
  confirmLabel?: string
  cancelLabel?: string
  tone?: "danger" | "primary"
}): Promise<boolean> {
  return new Promise((resolve) => {
    pendingConfirm.next({
      title: opts.title,
      body: opts.body,
      confirmLabel: opts.confirmLabel ?? i18n.t("common.confirm"),
      cancelLabel: opts.cancelLabel ?? i18n.t("common.cancel"),
      tone: opts.tone ?? "danger",
      resolve,
    })
  })
}

/// Internal queue holder. Rendered once at the app root by <Toaster/>.
/// Only one confirm at a time is supported — every existing call site is
/// modal anyway, and stacking native confirms was never supported either.
type PendingConfirm = {
  title: string
  body: ReactNode
  confirmLabel: string
  cancelLabel: string
  tone: "danger" | "primary"
  resolve: (v: boolean) => void
}

// Safe default: resolve(false) so a call before ConfirmHost mounts (early
// init, tests) or after it unmounts can never hang forever.
let pendingConfirm: { next: (c: PendingConfirm) => void } = {
  next: (c) => c.resolve(false),
}

export function setConfirmDispatcher(dispatch: (c: PendingConfirm) => void) {
  pendingConfirm = { next: dispatch }
}

export function ConfirmHost() {
  const [current, setCurrent] = useState<PendingConfirm | null>(null)
  // Register the dispatcher in an effect (NOT a useState initializer — that
  // runs during render, fires twice in StrictMode, and never cleans up; a
  // later unmount would leave the dispatcher pointing at a dead component,
  // making every confirmDialog() call hang forever).
  useEffect(() => {
    setConfirmDispatcher((c) => setCurrent(c))
    return () => {
      // Restore a safe default so calls after unmount resolve (false) instead
      // of hanging: never setState on the unmounted host.
      setConfirmDispatcher((c) => c.resolve(false))
    }
  }, [])
  const close = (v: boolean) => {
    current?.resolve(v)
    setCurrent(null)
  }
  if (!current) return null
  const tone =
    current.tone === "danger" ? ("danger" as const) : ("primary" as const)
  return (
    <Dialog open onOpenChange={(o) => !o && close(false)}>
      <DialogContent size="sm">
        <DialogHeader>
          <DialogTitle>{current.title}</DialogTitle>
          {typeof current.body === "string" && (
            <DialogDescription>{current.body}</DialogDescription>
          )}
        </DialogHeader>
        {typeof current.body !== "string" && (
          <DialogBody className="text-sm text-muted leading-relaxed">
            {current.body}
          </DialogBody>
        )}
        <DialogFooter>
          <Button onClick={() => close(false)}>{current.cancelLabel}</Button>
          <Button variant={tone} onClick={() => close(true)} autoFocus>
            {current.confirmLabel}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
