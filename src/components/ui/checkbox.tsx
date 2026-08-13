import * as React from "react"

/// Rotated-square slide checkbox. Native `<input type="checkbox">` keeps full
/// a11y + label hit-target for free (no Radix dep); the sibling `.checkbox-fill`
/// is a square rotated 45°, larger than the box, driven by
/// `:checked + .checkbox-fill` in styles.css — it slides diagonally into view
/// on check and back out on uncheck.
const Checkbox = React.forwardRef<
  HTMLInputElement,
  {
    checked: boolean
    onCheckedChange: (checked: boolean) => void
    disabled?: boolean
    id?: string
    name?: string
  }
>(({ checked, onCheckedChange, disabled, id, name }, ref) => {
  const fallbackId = React.useId()
  return (
    <span className="checkbox">
      <input
        ref={ref}
        id={id ?? fallbackId}
        type="checkbox"
        name={name}
        checked={checked}
        disabled={disabled}
        onChange={(e) => onCheckedChange(e.target.checked)}
      />
      <span className="checkbox-fill" aria-hidden="true" />
    </span>
  )
})
Checkbox.displayName = "Checkbox"

export { Checkbox }