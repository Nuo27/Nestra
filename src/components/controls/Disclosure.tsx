import { useId, useState, type ReactNode } from "react"
import { ChevronRight } from "lucide-react"

/// Generic collapsible section — shared header/chevron/animation anatomy.
/// `header` is the toggle button's content; `children` render under it while
/// open. Controlled or uncontrolled (`open`/`onOpenChange` vs `defaultOpen`).
/// Replaces the hand-rolled `raw <button> + ChevronRight + rotate` patterns in
/// provider-edit's ability disclosure and sessions' message rows (MessageCard
/// builds on this).
///
/// Controlledness is decided by `open !== undefined` (NOT by the presence of
/// `onOpenChange`): `onOpenChange` without `open` is the common
/// "parent wants to know, child owns the state" pattern. Either way, the
/// callback fires.
export function Disclosure({
  header,
  children,
  defaultOpen = false,
  open,
  onOpenChange,
  className,
  buttonClassName,
}: {
  header: ReactNode
  children: ReactNode
  defaultOpen?: boolean
  open?: boolean
  onOpenChange?: (v: boolean) => void
  className?: string
  buttonClassName?: string
}) {
  const [internal, setInternal] = useState(defaultOpen)
  const isOpen = open !== undefined ? open : internal
  const toggle = () => {
    const next = !isOpen
    if (open !== undefined) onOpenChange?.(next)
    else {
      setInternal(next)
      onOpenChange?.(next)
    }
  }
  // Link the toggle button to the panel for screen readers.
  const panelId = useId()
  return (
    <div className={className}>
      <button
        type="button"
        aria-expanded={isOpen}
        aria-controls={panelId}
        onClick={toggle}
        className={`brackets-state flex w-full items-center gap-2 px-3 py-1.5 text-left text-sm transition-[color,box-shadow] duration-fast focus-visible:shadow-focus${buttonClassName ? ` ${buttonClassName}` : ""}`}
      >
        <ChevronRight
          data-icon
          size={12}
          className={`shrink-0 text-subtle transition-transform duration-fast ${
            isOpen ? "rotate-90" : ""
          }`}
        />
        {/* div, not span: header children can be block-level (div/section). */}
        <div className="min-w-0 flex-1">{header}</div>
      </button>
      {/*
       * CSS grid `0fr ↔ 1fr` row-template animation is the only CSS-only
       * height transition that doesn't need JS measurement (DESIGN.md §11).
       * The inner panel is always mounted so we get the exit animation;
       * overflow-hidden on the wrapper clips it. `aria-hidden` toggles for
       * assistive tech without affecting the transition.
       */}
      <div
        className="grid transition-[grid-template-rows] duration-150 ease-out"
        style={{ gridTemplateRows: isOpen ? "1fr" : "0fr" }}
      >
        <div
          id={panelId}
          role="region"
          aria-label="disclosure content"
          aria-hidden={!isOpen}
          className="min-h-0 overflow-hidden"
        >
          {children}
        </div>
      </div>
    </div>
  )
}
