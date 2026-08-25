import type { Session } from "../ipc";

// ---- date bucketing for the list ----
// Bucket labels are translation KEYS — the list renders `t(b.labelKey)` so
// the date groups localize with the rest of the page.
type Bucket = { key: string; labelKey: string; sessions: Session[] };
export function bucketByDate(sessions: Session[]): Bucket[] {
  const now = new Date();
  const startOfToday = new Date(now.getFullYear(), now.getMonth(), now.getDate()).getTime();
  const startOfYesterday = startOfToday - 86_400_000;
  const startOfWeek = startOfToday - 6 * 86_400_000;
  const buckets: Bucket[] = [
    { key: "today", labelKey: "sessions.bucketToday", sessions: [] },
    { key: "yesterday", labelKey: "sessions.bucketYesterday", sessions: [] },
    { key: "week", labelKey: "sessions.bucketThisWeek", sessions: [] },
    { key: "older", labelKey: "sessions.bucketOlder", sessions: [] },
  ];
  for (const s of sessions) {
    const t = s.updated_at;
    if (t >= startOfToday) buckets[0].sessions.push(s);
    else if (t >= startOfYesterday) buckets[1].sessions.push(s);
    else if (t >= startOfWeek) buckets[2].sessions.push(s);
    else buckets[3].sessions.push(s);
  }
  return buckets.filter((b) => b.sessions.length > 0);
}
