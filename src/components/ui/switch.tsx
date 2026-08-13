import { useId } from "react"

/// Canonical binary toggle — the classic skewed "TGL" switch: a visually
/// hidden checkbox + sibling label that slides ON/OFF inside a skewed box.
/// Native checkbox keeps keyboard a11y; the CSS lives in styles.css.
export function Switch({
  checked,
  onCheckedChange,
  disabled = false,
  title,
  className,
  onLabel = "ON",
  offLabel = "OFF",
}: {
  checked: boolean
  onCheckedChange: (v: boolean) => void
  disabled?: boolean
  title?: string
  className?: string
  onLabel?: string
  offLabel?: string
}) {
  const id = useId()
  return (
    <span className={`checkbox-wrapper-8${className ? ` ${className}` : ""}`}>
      <input
        id={id}
        type="checkbox"
        className="tgl tgl-skewed"
        checked={checked}
        onChange={(e) => onCheckedChange(e.target.checked)}
        disabled={disabled}
        aria-label={title}
      />
      <label
        htmlFor={id}
        className="tgl-btn"
        data-tg-off={offLabel}
        data-tg-on={onLabel}
        title={title}
        // The checkbox is visually hidden (1px + clip-path). Focusing it on
        // mouse click makes Chromium scroll to its (empty) geometry, which
        // falls back to the document bottom — the page jumps on every
        // toggle. Swallow both mousedown and click: mousedown stops focus
        // transfer, click stops the label's native activation (which would
        // still focus the input) and flips the checked state through React
        // instead. Keyboard activation (Tab + space on the input itself)
        // is untouched.
        onMouseDown={(e) => e.preventDefault()}
        onClick={(e) => {
          e.preventDefault()
          // The label's click path bypasses the (disabled) input — a
          // disabled switch must stay inert here too.
          if (disabled) return
          onCheckedChange(!checked)
        }}
      />
    </span>
  )
}