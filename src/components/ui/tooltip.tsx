import * as React from "react"
import * as TooltipPrimitive from "@radix-ui/react-tooltip"

const TooltipProvider = TooltipPrimitive.Provider
const Tooltip = TooltipPrimitive.Root
const TooltipTrigger = TooltipPrimitive.Trigger

const TooltipContent = React.forwardRef<
  React.ElementRef<typeof TooltipPrimitive.Content>,
  React.ComponentPropsWithoutRef<typeof TooltipPrimitive.Content>
>(({ className, sideOffset = 6, ...props }, ref) => (
  <TooltipPrimitive.Portal>
    <TooltipPrimitive.Content
      ref={ref}
      sideOffset={sideOffset}
      className={`z-[70] max-w-[18rem] border border-border bg-overlay px-2.5 py-1.5 text-xs leading-relaxed text-fg-muted animate-in fade-in-0 zoom-in-95 data-[state=closed]:animate-out data-[state=closed]:fade-out-0 data-[state=closed]:zoom-out-95 duration-fast${className ? ` ${className}` : ""}`}
      {...props}
    />
  </TooltipPrimitive.Portal>
))
TooltipContent.displayName = TooltipPrimitive.Content.displayName

export { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger }

/// Compound convenience: wraps a single trigger element and shows `content`
/// on hover/focus. `<Tip content="Refresh"><IconButton…/></Tip>`. Provider is
/// mounted once at the RootShell so no nesting is needed.
export function Tip({
  content,
  children,
  side,
  align,
  disabled,
}: {
  content: React.ReactNode
  children: React.ReactElement
  side?: "top" | "right" | "bottom" | "left"
  align?: "start" | "center" | "end"
  disabled?: boolean
}) {
  return (
    // Force-close the tooltip on disabled triggers via controlled `open`.
    <Tooltip delayDuration={200} open={disabled ? false : undefined}>
      <TooltipTrigger asChild>{children}</TooltipTrigger>
      {!disabled && (
        <TooltipContent side={side} align={align}>
          {content}
        </TooltipContent>
      )}
    </Tooltip>
  )
}
