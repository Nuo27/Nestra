import * as React from "react"

type Size = "sm" | "md"

const SIZE_CLASS: Record<Size, string> = {
  sm: "text-xs px-2 py-1.5",
  md: "text-sm px-2.5 py-2",
}

type TextareaProps = Omit<React.ComponentProps<"textarea">, "size"> & {
  size?: Size
  error?: boolean
}

/// Multi-line text input on the same `surface-field` skin as `Input`. The
/// shared primitive replaces ad-hoc `<textarea>` instances that otherwise
/// drift on padding, error border, and focus-ring colour.
const Textarea = React.forwardRef<HTMLTextAreaElement, TextareaProps>(
  ({ className, size = "md", error, rows = 3, ...props }, ref) => (
    <textarea
      ref={ref}
      rows={rows}
      className={`surface-field block w-full ${SIZE_CLASS[size]} ${
        error ? "!border-danger" : ""
      } text-fg transition-[border-color,box-shadow] duration-fast placeholder:text-subtle hover:border-border-strong focus-visible:outline-none focus-visible:shadow-focus disabled:cursor-not-allowed disabled:opacity-50${
        className ? ` ${className}` : ""
      }`}
      {...props}
    />
  ),
)
Textarea.displayName = "Textarea"

export { Textarea }