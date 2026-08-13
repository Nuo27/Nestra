import type { ReactNode } from "react"

/** Keyboard keycap `⌘K`. Used for shortcut hints. */
export function Kbd({
  children,
  className,
}: {
  children: ReactNode
  className?: string
}) {
  return (
    <kbd
      className={`inline-flex items-center justify-center px-0.5 text-[10px] font-medium text-muted tabular ${className ?? ""}`}
    >
      {children}
    </kbd>
  )
}
