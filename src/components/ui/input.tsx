import * as React from "react"

type Size = "sm" | "md"

const SIZE_CLASS: Record<Size, string> = {
  sm: "h-7 text-xs px-2",
  md: "h-8 text-sm px-2.5",
}

type InputProps = Omit<React.ComponentProps<"input">, "size" | "prefix" | "suffix"> & {
  size?: Size
  error?: boolean
  prefix?: React.ReactNode
  suffix?: React.ReactNode
}

/** Single text input. Adds `size`, an `error` state, and `prefix`/`suffix`
 * slots (search icon, Kbd hint, unit). Prefix/suffix wrap the field in a
 * group so the focus ring wraps the whole control. */
const Input = React.forwardRef<HTMLInputElement, InputProps>(
  (
    { className, type, size = "md", error, prefix, suffix, disabled, ...props },
    ref,
  ) => {
    if (!prefix && !suffix) {
      return (
        <input
          type={type}
          ref={ref}
          disabled={disabled}
          className={`surface-field flex w-full ${SIZE_CLASS[size]} ${
            error ? "!border-danger" : ""
          } text-fg transition-[border-color,box-shadow] duration-fast placeholder:text-subtle hover:border-border-strong focus-visible:outline-none focus-visible:shadow-focus disabled:cursor-not-allowed disabled:opacity-50 file:border-0 file:bg-transparent file:text-sm file:font-medium file:disabled:opacity-50${
            className ? ` ${className}` : ""
          }`}
          {...props}
        />
      )
    }
    return (
      <div
        className={`surface-field flex items-center w-full ${SIZE_CLASS[size]} ${
          error ? "!border-danger" : ""
        } gap-1.5 text-fg transition-[border-color,box-shadow] duration-fast hover:border-border-strong focus-within:shadow-focus ${
          // `disabled:opacity-50` on the wrapper never applies (pseudo-classes
          // need a form control) — gate on the prop directly.
          disabled ? "opacity-50" : ""
        }${className ? ` ${className}` : ""}`}
      >
        {prefix && (
          // Decorative — hidden from screen readers.
          <span aria-hidden="true" className="flex items-center text-subtle [&_svg]:size-3.5 pl-1">
            {prefix}
          </span>
        )}
        <input
          type={type}
          ref={ref}
          disabled={disabled}
          className="min-w-0 flex-1 bg-transparent border-0 outline-none placeholder:text-subtle disabled:cursor-not-allowed"
          {...props}
        />
        {suffix && (
          <span aria-hidden="true" className="flex items-center text-subtle pr-1.5">
            {suffix}
          </span>
        )}
      </div>
    )
  },
)
Input.displayName = "Input"

export { Input }
