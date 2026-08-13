import { useEffect, useRef, useState } from "react"

/// Shared clipboard hook. Falls back to a hidden textarea when the async
/// Clipboard API is unavailable (e.g. insecure context). Returns [copied, copy]
/// where `copied` flips back to false after 1.5s.
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
      // Fallback: hidden textarea + execCommand. Remove it in `finally` so a
      // throwing execCommand can't leak the node into the DOM.
      const ta = document.createElement("textarea")
      try {
        ta.value = text
        ta.style.position = "fixed"
        ta.style.opacity = "0"
        document.body.appendChild(ta)
        ta.select()
        ok = document.execCommand("copy")
      } catch {
        ok = false
      } finally {
        document.body.removeChild(ta)
      }
    }
    if (ok) {
      setCopied(true)
      if (timer.current !== undefined) window.clearTimeout(timer.current)
      timer.current = window.setTimeout(() => setCopied(false), 1500)
    }
  }

  return [copied, copy]
}
