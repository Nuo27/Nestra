import i18n from "../i18n";

export function formatTime(ts: number | null): string {
  if (!ts) return "";
  const d = new Date(ts);
  const now = new Date();
  const sameDay = d.toDateString() === now.toDateString();
  if (sameDay) {
    // Pass the app language so a Chinese UI doesn't show English AM/PM
    // formats (the system locale may differ from the selected language).
    return d.toLocaleTimeString(i18n.language, {
      hour: "2-digit",
      minute: "2-digit",
      hour12: false,
    });
  }
  const yesterday = new Date(now);
  yesterday.setDate(now.getDate() - 1);
  if (d.toDateString() === yesterday.toDateString()) {
    return i18n.t("time.yesterday");
  }
  const diffMs = now.getTime() - d.getTime();
  const days = Math.floor(diffMs / 86_400_000);
  if (days < 7) return i18n.t("time.daysAgo", { n: days });
  return d.toLocaleDateString(i18n.language, { month: "short", day: "numeric" });
}

export function formatRelative(ts: number | null): string {
  if (!ts) return "";
  // Clamp: a future timestamp (clock skew, misrecorded data) must not render
  // as "-3s ago" — the diff is treated as "just now".
  const diffMs = Math.max(0, Date.now() - ts);
  const sec = Math.floor(diffMs / 1000);
  if (sec < 60) return i18n.t("time.secAgo", { n: sec });
  const min = Math.floor(sec / 60);
  if (min < 60) return i18n.t("time.minAgo", { n: min });
  const hr = Math.floor(min / 60);
  if (hr < 24) return i18n.t("time.hrAgo", { n: hr });
  const day = Math.floor(hr / 24);
  if (day < 30) return i18n.t("time.dayAgo", { n: day });
  const mon = Math.floor(day / 30);
  if (mon < 12) return i18n.t("time.moAgo", { n: mon });
  return i18n.t("time.yrAgo", { n: Math.floor(mon / 12) });
}

/// Money formatting for balance-based quota items. Known units get a symbol
/// prefix ($ USD, ¥ CNY); unknown units are appended space-separated. The
/// sign rides BEFORE the symbol ("-$5.00", not "$-5.00").
export function fmtMoney(n: number, unit: string | null): string {
  const negative = n < 0;
  const amt = Math.abs(n).toFixed(2);
  const signed = `${negative ? "-" : ""}${amt}`;
  switch (unit) {
    case "USD":
      return `${negative ? "-$" : "$"}${amt}`;
    case "CNY":
      return `${negative ? "-¥" : "¥"}${amt}`;
    case null:
    case undefined:
      return signed;
    default:
      return `${signed} ${unit}`;
  }
}

/// Compact count formatting for quota windows: 1.5M / 12.4k / 900.
export function fmtNum(n: number): string {
  const a = Math.abs(n);
  if (a >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (a >= 1_000) return `${(n / 1_000).toFixed(1)}k`;
  return `${n}`;
}
