import tailwindcssAnimate from "tailwindcss-animate";

/** @type {import('tailwindcss').Config}
 *
 * Token-driven theme. Canonical color keys are flat (bg-canvas, text-fg,
 * bg-accent, text-success, …). No legacy aliases — every page now reads
 * from the canonical token set.
 */
export default {
  content: ["./index.html", "./src/**/*.{ts,tsx}"],
  // The app toggles themes via `<html data-theme>` (CSS variables), not the
  // Tailwind `dark:` variant. `class` strategy keeps any `dark:` usage tied
  // to a class on the root element rather than the OS media query.
  darkMode: "class",
  theme: {
    extend: {
      fontFamily: {
        // Phosphor terminal — mono-dominant. `sans` repointed to the mono
        // stack so Tailwind preflight (html element) and the `font-sans`
        // utility also render mono. Geist is exposed via `font-prose` for
        // longer descriptive copy ONLY (subtitles, descriptions, hints,
        // empty-states, dialog body); it never becomes the body default.
        // Sarasa Mono SC carries CJK glyphs only (its @font-face is
        // unicode-range-scoped), so listing it never affects Latin.
        sans: [
          "JetBrains Mono",
          "Sarasa Mono SC",
          "ui-monospace",
          "SF Mono",
          "Consolas",
          "monospace",
        ],
        mono: [
          "JetBrains Mono",
          "Sarasa Mono SC",
          "ui-monospace",
          "SF Mono",
          "Consolas",
          "monospace",
        ],
        prose: [
          "Geist",
          "Sarasa Mono SC",
          "ui-sans-serif",
          "system-ui",
          "-apple-system",
          "Segoe UI",
          "sans-serif",
        ],
      },
      colors: {
        // surfaces — canonical
        canvas: "var(--bg-canvas)",
        surface: "var(--bg-surface)",
        raised: "var(--bg-raised)",
        overlay: "var(--bg-overlay)",
        inset: "var(--bg-inset)",
        // borders
        border: "var(--border)",
        "border-strong": "var(--border-strong)",
        // text — canonical
        fg: "var(--fg)",
        muted: "var(--fg-muted)",
        subtle: "var(--fg-subtle)",
        // accent
        accent: {
          DEFAULT: "var(--accent)",
          hover: "var(--accent-hover)",
          soft: "var(--accent-soft)",
          border: "var(--accent-border)",
        },
        // semantic
        success: {
          DEFAULT: "var(--success)",
          soft: "var(--success-soft)",
          border: "var(--success-border)",
        },
        warning: {
          DEFAULT: "var(--warning)",
          soft: "var(--warning-soft)",
          border: "var(--warning-border)",
        },
        danger: {
          DEFAULT: "var(--danger)",
          soft: "var(--danger-soft)",
          border: "var(--danger-border)",
        },
      },
      boxShadow: {
        focus: "var(--shadow-focus)",
      },
      transitionDuration: {
        fast: "120ms",
        DEFAULT: "150ms",
        slow: "220ms",
      },
      transitionTimingFunction: {
        out: "var(--ease-out)",
        DEFAULT: "var(--ease-standard)",
        standard: "var(--ease-standard)",
        spring: "var(--ease-spring)",
      },
      keyframes: {
        // `fade-in` was never referenced by an animation mapping (the
        // `animate-in fade-in-0` classes come from tailwindcss-animate).
        shimmer: {
          "100%": { transform: "translateX(100%)" },
        },
      },
      spacing: {
        "0.5": "2px",
        "1": "4px",
        "2": "8px",
        "3": "12px",
        "4": "16px",
        "5": "24px",
        "6": "32px",
        "7": "48px",
      },
      fontSize: {
        "2xs": ["10px", { lineHeight: "1.4" }],
        xs: ["11px", { lineHeight: "1.45" }],
        sm: ["12px", { lineHeight: "1.45" }],
        md: ["13px", { lineHeight: "1.5" }],
        lg: ["15px", { lineHeight: "1.35" }],
        xl: ["18px", { lineHeight: "1.25", letterSpacing: "-0.01em" }],
      },
    },
  },
  plugins: [tailwindcssAnimate],
};
