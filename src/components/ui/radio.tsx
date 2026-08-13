import * as React from "react"

/// Native radio with token accent — mirrors `ui/checkbox.tsx` grammar: the
/// real <input> keeps full a11y and the label hit-target for free; styling
/// comes from `accent-color`. The single replacement for the raw browser
/// radio in agents.tsx's "None (factory config)" row.
const Radio = React.forwardRef<
  HTMLInputElement,
  {
    checked: boolean
    onCheckedChange: (checked: boolean) => void
    disabled?: boolean
    id?: string
    name?: string
    value?: string
    label?: React.ReactNode
    className?: string
  }
>(({ checked, onCheckedChange, disabled, id, name, value, label, className }, ref) => {
  const fallbackId = React.useId()
  const input = (
    <input
      ref={ref}
      id={id ?? fallbackId}
      type="radio"
      name={name}
      value={value}
      checked={checked}
      disabled={disabled}
      onChange={(e) => onCheckedChange(e.target.checked)}
      className="size-3.5 accent-accent"
    />
  )
  if (!label) return input
  return (
    <span className={`inline-flex items-center gap-1.5 ${className ?? ""}`}>
      {input}
      <label htmlFor={id ?? fallbackId} className="cursor-pointer text-sm text-fg">
        {label}
      </label>
    </span>
  )
})
Radio.displayName = "Radio"

export { Radio }
