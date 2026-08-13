import { useEffect, useRef } from "react";

/// Monotonic generation guard for ad-hoc async IPC that bypasses TanStack
/// Query (debounced search, one-shot fetches into local state). Detects when a
/// late-resolving older call has been superseded so its result is discarded
/// instead of overwriting the newer one.
///
/// TanStack Query already dedupes + serializes per-key useQuery paths — do
/// NOT wrap those. This is only for fire-and-await sequences that write local
/// component state, where a newer call (or unmount) must invalidate an
/// in-flight older one (decision #5: old requests must not overwrite new).
export function makeGuard() {
  let gen = 0;
  return {
    /// Reserve the next generation; pass it to `isCurrent` after `await`.
    start: () => ++gen,
    /// True if `g` is still the latest generation (no newer call started).
    isCurrent: (g: number) => g === gen,
    /// Bump so every in-flight generation becomes stale. Call on unmount, or
    /// when superseding an in-flight call with a fresh one.
    supersede: () => {
      gen += 1;
    },
  };
}
export type Guard = ReturnType<typeof makeGuard>;

/// Run `fn`; resolve `{ value }` if still the current generation when it
/// lands, otherwise `{ stale: true }`.
///
/// Errors are swallowed and reported as `{ stale: true }` — this is for
/// best-effort auxiliary fetches (palette search, presets) where a failure
/// just means "keep prior state". That also guarantees cancelled/superseded
/// calls never surface as unhandled rejections or error toasts. If you need
/// to surface a genuine failure, use a useMutation with onError instead.
export async function cancellableInvoke<T>(
  guard: Guard,
  fn: () => Promise<T>,
): Promise<{ stale: false; value: T } | { stale: true; error?: unknown }> {
  const g = guard.start();
  try {
    const value = await fn();
    if (guard.isCurrent(g)) return { stale: false, value };
  } catch (e) {
    // Distinguish a genuine failure from a superseded call: both return
    // "no result", but a real error is worth a debug log (the old code
    // collapsed them silently, making failures undiagnosable).
    if (guard.isCurrent(g)) {
      console.debug("[guard] cancellable invoke failed:", e);
      return { stale: true, error: e };
    }
  }
  return { stale: true };
}

/// Per-component guard that supersedes on unmount. One guard per mount;
/// re-renders return the same instance.
export function useGuard(): Guard {
  const ref = useRef<Guard | null>(null);
  if (ref.current === null) ref.current = makeGuard();
  useEffect(() => () => ref.current!.supersede(), []);
  return ref.current;
}
