import * as React from "react"
import * as DialogPrimitive from "@radix-ui/react-dialog"
import { useTranslation } from "react-i18next"
import { X } from "lucide-react"

const Dialog = DialogPrimitive.Root
const DialogPortal = DialogPrimitive.Portal

const DialogOverlay = React.forwardRef<
  React.ElementRef<typeof DialogPrimitive.Overlay>,
  React.ComponentPropsWithoutRef<typeof DialogPrimitive.Overlay>
>(({ className, ...props }, ref) => (
  <DialogPrimitive.Overlay
    ref={ref}
    className={`fixed inset-0 z-50 surface-scrim data-[state=open]:animate-in data-[state=closed]:animate-out data-[state=closed]:fade-out-0 data-[state=open]:fade-in-0 duration-fast${className ? ` ${className}` : ""}`}
    {...props}
  />
))
DialogOverlay.displayName = DialogPrimitive.Overlay.displayName

type Size = "sm" | "md" | "lg" | "xl"
const SIZE_MAX: Record<Size, string> = {
  sm: "max-w-sm",
  md: "max-w-lg",
  lg: "max-w-xl",
  xl: "max-w-3xl",
}

const DialogContent = React.forwardRef<
  React.ElementRef<typeof DialogPrimitive.Content>,
  React.ComponentPropsWithoutRef<typeof DialogPrimitive.Content> & {
    size?: Size
  }
>(({ className, children, size = "md", ...props }, ref) => (
  <DialogPortal>
    <DialogOverlay />
    <DialogPrimitive.Content
      ref={ref}
      className={`flex flex-col fixed left-[50%] top-[50%] z-50 w-[calc(100%-2rem)] ${SIZE_MAX[size]} translate-x-[-50%] translate-y-[-50%] gap-0 border border-border bg-surface data-[state=open]:animate-in data-[state=closed]:animate-out data-[state=closed]:fade-out-0 data-[state=open]:fade-in-0 data-[state=closed]:zoom-out-95 data-[state=open]:zoom-in-95 data-[state=closed]:slide-out-to-left-1/2 data-[state=closed]:slide-out-to-top-[48%] data-[state=open]:slide-in-from-left-1/2 data-[state=open]:slide-in-from-top-[48%] duration-fast${className ? ` ${className}` : ""}`}
      {...props}
    >
      {children}
      <DialogCloseLabel />
    </DialogPrimitive.Content>
  </DialogPortal>
))
DialogContent.displayName = DialogPrimitive.Content.displayName

/// The dialog's sr-only close label — localized (a hardcoded "Close" was
/// invisible to the i18n key check and wrong for non-English users).
function DialogCloseLabel() {
  const { t } = useTranslation()
  return (
    <DialogPrimitive.Close className="absolute right-4 top-4 flex h-7 w-7 items-center justify-center text-muted transition-colors duration-fast hover:text-danger focus:outline-none focus-visible:shadow-focus disabled:pointer-events-none">
      <X className="h-4 w-4" />
      <span className="sr-only">{t("common.close")}</span>
    </DialogPrimitive.Close>
  )
}

/// Scrollable dialog body region. Replaces the ad-hoc
/// `max-h-[70vh] overflow-y-auto pr-1` wrapper. `flex-1` lets the footer
/// pin at the bottom of `DialogContent`.
const DialogBody = ({
  className,
  ...props
}: React.HTMLAttributes<HTMLDivElement>) => (
  <div className={`flex-1 max-h-[70vh] overflow-y-auto scroll px-5 py-4 -mr-2 pr-4${className ? ` ${className}` : ""}`} {...props} />
)
DialogBody.displayName = "DialogBody"

const DialogHeader = ({
  className,
  ...props
}: React.HTMLAttributes<HTMLDivElement>) => (
  <div
    className={`flex flex-col gap-1 border-b border-border px-5 py-3 text-left${className ? ` ${className}` : ""}`}
    {...props}
  />
)
DialogHeader.displayName = "DialogHeader"

const DialogFooter = ({
  className,
  ...props
}: React.HTMLAttributes<HTMLDivElement>) => (
  <div
    className={`flex gap-2 justify-end border-t border-border px-5 py-3${className ? ` ${className}` : ""}`}
    {...props}
  />
)
DialogFooter.displayName = "DialogFooter"

const DialogTitle = React.forwardRef<
  React.ElementRef<typeof DialogPrimitive.Title>,
  React.ComponentPropsWithoutRef<typeof DialogPrimitive.Title>
>(({ className, ...props }, ref) => (
  <DialogPrimitive.Title
    ref={ref}
    className={`text-lg font-semibold leading-tight tracking-[-0.01em]${className ? ` ${className}` : ""}`}
    {...props}
  />
))
DialogTitle.displayName = DialogPrimitive.Title.displayName

const DialogDescription = React.forwardRef<
  React.ElementRef<typeof DialogPrimitive.Description>,
  React.ComponentPropsWithoutRef<typeof DialogPrimitive.Description>
>(({ className, ...props }, ref) => (
  <DialogPrimitive.Description
    ref={ref}
    className={`prose text-sm text-muted leading-relaxed${className ? ` ${className}` : ""}`}
    {...props}
  />
))
DialogDescription.displayName = DialogPrimitive.Description.displayName

export {
  Dialog,
  DialogBody,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
}
