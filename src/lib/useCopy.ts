import { useEffect, useRef, useState } from "react"

/// Shared clipboard hook (async Clipboard API — Tauri's WebView always runs
/// in a secure context, so no fallback path is needed). Returns [copied,
/// copy] where `copied` flips back to false after 1.5s.
export function useCopy(): [boolean, (text: string) => Promise<void>] {
  const [copied, setCopied] = useState(false)
  // The reset timer lives in a ref so a copy after unmount can't setState
  // (and rapid copies re-arm instead of fighting over one timeout).
  const timer = useRef<number | undefined>(undefined)
  useEffect(
    () => () => {
      if (timer.current !== undefined) window.clearTimeout(timer.current)
    },
    [],
  )

  async function copy(text: string) {
    let ok = false
    try {
      await navigator.clipboard.writeText(text)
      ok = true
    } catch {
      ok = false
    }
    if (ok) {
      setCopied(true)
      if (timer.current !== undefined) window.clearTimeout(timer.current)
      timer.current = window.setTimeout(() => setCopied(false), 1500)
    }
  }

  return [copied, copy]
}
