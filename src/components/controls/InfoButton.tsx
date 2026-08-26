import type { ReactNode } from "react"
import { Info } from "lucide-react"
import { useTranslation } from "react-i18next"
import { Tip } from "../ui/tooltip"

/// Small info icon that reveals explanatory text as a tooltip on hover/focus.
/// The entry point for explanatory copy: long descriptions live here (as a
/// `Tip` tooltip, content from the locale file via `t("…help")`) instead of
/// cluttering the header inline. One shared affordance for every surface.
export function InfoButton({
  content,
  tooltip,
}: {
  content: ReactNode
  /** Override the default localized "How this works" aria-label. */
  tooltip?: string
}) {
  const { t } = useTranslation()
  // Default is LOCALIZED ("How this works"): the old hardcoded English
  // default reached every non-English user's screen reader.
  const label = tooltip ?? t("common.howItWorks")
  return (
    <Tip content={content}>
      <button
        type="button"
        aria-label={label}
        // `muted`, not `subtle`: this is an interactive affordance, and
        // subtle (~2.6:1 on card surfaces) reads as invisible decoration.
        className="inline-flex shrink-0 items-center text-muted transition-colors duration-fast hover:text-fg focus-visible:outline-none focus-visible:shadow-focus"
      >
        <Info data-icon size={14} />
      </button>
    </Tip>
  )
}
