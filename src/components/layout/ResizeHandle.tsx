import { useEffect, useRef, type MouseEvent as ReactMouseEvent } from "react"

/// Vertical resize handle pinned to the right edge of a panel. Drags update
/// `onResize` with the new width, clamped to `min`/`max`. `.no-transition` is
/// applied for the drag so CSS width transitions can't fight the pointer.
export function ResizeHandle({
  width,
  onResize,
  min = 220,
  max = 640,
}: {
  width: number
  onResize: (next: number) => void
  min?: number
  max?: number
}) {
  // The window listeners are removed on unmount too — the old code cleaned
  // them only on mouseup, so an unmount mid-drag leaked the listeners and
  // left `.no-transition` stuck on <html>.
  const dragging = useRef(false)
  useEffect(
    () => () => {
      if (dragging.current) {
        document.documentElement.classList.remove("no-transition")
      }
    },
    [],
  )

  const start = (e: ReactMouseEvent) => {
    e.preventDefault()
    dragging.current = true
    const startX = e.clientX
    const startW = width
    const move = (ev: MouseEvent) => {
      onResize(Math.min(Math.max(startW + (ev.clientX - startX), min), max))
    }
    const up = () => {
      dragging.current = false
      document.documentElement.classList.remove("no-transition")
      window.removeEventListener("mousemove", move)
      window.removeEventListener("mouseup", up)
    }
    document.documentElement.classList.add("no-transition")
    window.addEventListener("mousemove", move)
    window.addEventListener("mouseup", up)
  }

  return (
    <div
      role="separator"
      aria-orientation="vertical"
      // `w-1.5` (6px): a 4px strip was effectively invisible and hard to hit —
      // the hover accent only reveals itself once the cursor is already on it.
      className="absolute right-0 top-0 z-10 h-full w-1.5 cursor-col-resize transition-colors duration-fast hover:bg-accent-border"
      onMouseDown={start}
      title="Drag to resize"
    />
  )
}
