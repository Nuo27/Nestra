import { useNavigate } from "@tanstack/react-router";
import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import i18n from "../../i18n";
import { paletteSearch, type PaletteItem } from "../../ipc";
import { useGuard } from "../../lib/guard";
import { useUI } from "../../stores/ui";
import { Input } from "../ui/input";
import { SectionLabel } from "./PageHeader";

export function Palette() {
  const { t } = useTranslation();
  const { paletteQuery, setPaletteQuery, closePalette } = useUI();
  const [items, setItems] = useState<PaletteItem[]>([]);
  const [failed, setFailed] = useState(false);
  const [active, setActive] = useState(0);
  const navigate = useNavigate();
  const guard = useGuard();
  const inputRef = useRef<HTMLInputElement>(null);

  // WebView2 does not reliably honor `autoFocus` on a freshly mounted node;
  // without focus the user's typing goes nowhere. Explicitly grab it.
  useEffect(() => {
    const id = setTimeout(() => inputRef.current?.focus(), 0);
    return () => clearTimeout(id);
  }, []);

  // Debounced search. `cancellableInvoke` handles the generation guard so a
  // late-resolving older response (or an unmount) can never overwrite a newer
  // one — the canonical pattern used for every ad-hoc async into local state
  // in this codebase. Errors are distinguished from supersession: a genuine
  // IPC failure that's still the current generation surfaces as "search
  // failed" so the user isn't left staring at "no matches" with no clue.
  useEffect(() => {
    const id = setTimeout(async () => {
      const g = guard.start();
      try {
        const value = await paletteSearch(paletteQuery);
        if (!guard.isCurrent(g)) return;
        console.debug("[palette]", { query: paletteQuery, count: value.length });
        setItems(value);
        setFailed(false);
        setActive(0);
      } catch {
        if (!guard.isCurrent(g)) return;
        setFailed(true);
      }
    }, 60);
    return () => clearTimeout(id);
  }, [paletteQuery, guard]);

  useEffect(() => {
    function onKey(e: KeyboardEvent) {
      if (e.key === "ArrowDown") {
        e.preventDefault();
        setActive((i) => Math.min(items.length - 1, i + 1));
      } else if (e.key === "ArrowUp") {
        e.preventDefault();
        setActive((i) => Math.max(0, i - 1));
      } else if (e.key === "Enter") {
        // IME composing (Chinese/Japanese input): the Enter that confirms a
        // composition must NOT trigger navigation — it would clear the query
        // and jump the user mid-typing.
        if (e.isComposing) return;
        e.preventDefault();
        const target = items[active]?.target;
        if (target) {
          navigate({ to: target });
          closePalette();
        }
      }
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [items, active, navigate, closePalette]);

  // group items by kind for section headers
  const grouped = items.reduce<Record<string, PaletteItem[]>>((acc, it) => {
    (acc[it.kind] ??= []).push(it);
    return acc;
  }, {});

  const flat: { item: PaletteItem; section: string }[] = [];
  for (const kind of ["nav", "provider", "session", "skill"]) {
    for (const it of grouped[kind] ?? []) flat.push({ item: it, section: kind });
  }

  return (
    <div
      className="fixed inset-0 z-50 flex items-start justify-center pt-[12vh] bg-black/60 backdrop-blur-[2px] animate-in fade-in-0 duration-fast"
      onClick={closePalette}
    >
      <div
        className="w-[560px] max-w-[calc(100vw-2rem)] overflow-hidden border border-border bg-overlay animate-in fade-in-0 zoom-in-95 slide-in-from-top-4 duration-fast"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="border-b border-border px-3.5">
          <Input
            ref={inputRef}
            autoFocus
            size="md"
            value={paletteQuery}
            onChange={(e) => setPaletteQuery(e.target.value)}
            placeholder={t("palette.placeholder")}
            prefix={<span className="text-accent">{">"}</span>}
            className="h-11 border-0 bg-transparent px-0"
          />
        </div>
        <div
          className="max-h-[60vh] overflow-auto scroll py-1.5"
          role="listbox"
          aria-label={t("palette.placeholder")}
        >
          {flat.length === 0 && (
            <div className="px-3.5 py-3 text-sm text-subtle">
              {failed ? t("palette.searchFailed") : t("palette.noMatches")}
            </div>
          )}
          {flat.map((row, i) => {
            const prevSection = i > 0 ? flat[i - 1].section : null;
            const showHeader = row.section !== prevSection;
            const isActive = i === active;
            return (
              <div key={`${row.section}-${i}`}>
                {showHeader && (
                  <SectionLabel className="px-3.5 pt-2 pb-1">
                    {labelFor(row.section)}
                  </SectionLabel>
                )}
                <button
                  type="button"
                  role="option"
                  aria-selected={isActive}
                  onMouseEnter={() => setActive(i)}
                  onClick={() => {
                    navigate({ to: row.item.target });
                    closePalette();
                  }}
                  className={
                    "brackets-state mx-1.5 flex w-[calc(100%-0.75rem)] items-center justify-between px-2.5 py-1.5 text-left text-sm transition-[color,box-shadow] duration-fast focus-visible:shadow-focus " +
                    (isActive ? "text-accent font-medium" : "text-muted hover:text-fg")
                  }
                  data-active={isActive || undefined}
                >
                  <span className="truncate">{row.item.label}</span>
                  {row.item.detail && (
                    <span className="ml-3 shrink-0 text-2xs text-subtle">
                      {row.item.detail}
                    </span>
                  )}
                </button>
              </div>
            );
          })}
        </div>
      </div>
    </div>
  );
}

function labelFor(kind: string): string {
  // Module-level (no hooks) → use the global instance; sections re-render on
  // language change via the parent's useTranslation.
  return (
    {
      nav: i18n.t("palette.navigation"),
      provider: i18n.t("nav.providers"),
      session: i18n.t("nav.sessions"),
      skill: i18n.t("nav.skills"),
    }[kind] ?? kind
  );
}
