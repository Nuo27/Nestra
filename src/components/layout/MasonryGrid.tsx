import { Children, type ReactNode, useEffect, useMemo, useState } from "react";

/// Responsive masonry grid: distributes children into N flex columns
/// (round-robin) so items read left-to-right while packing tightly per
/// column — no gaps between cards of different heights.
///
/// Unlike CSS `columns` (which fills top-to-bottom per column, breaking
/// reading order) or CSS Grid (which aligns row heights, leaving gaps
/// below shorter cards), this approach gives both correct left-to-right
/// order and gap-free vertical packing — the same technique used by
/// react-masonry-css and Pinterest-style layouts.
///
/// Column counts mirror Tailwind's sm/xl breakpoints (1 / 2 / 3).
export function MasonryGrid({ children }: { children: ReactNode }) {
  const cols = useResponsiveCols();
  const items = Children.toArray(children);
  const columns = useMemo(() => {
    const arr: ReactNode[][] = Array.from({ length: cols }, () => []);
    items.forEach((item, i) => arr[i % cols].push(item));
    return arr;
  }, [items, cols]);

  return (
    <div className="flex w-full gap-3">
      {columns.map((col, ci) => (
        <div key={ci} className="flex min-w-0 flex-1 flex-col gap-3">
          {col}
        </div>
      ))}
    </div>
  );
}

/// Column count matching Tailwind breakpoints: 1 below 640 px, 2 at sm,
/// 3 at xl. Uses a lazy initialiser so the first paint is already correct
/// (no flash), then updates on resize.
function useResponsiveCols() {
  const [cols, setCols] = useState(() => {
    if (typeof window === "undefined") return 2;
    const w = window.innerWidth;
    return w < 640 ? 1 : w < 1280 ? 2 : 3;
  });
  useEffect(() => {
    const update = () => {
      const w = window.innerWidth;
      setCols(w < 640 ? 1 : w < 1280 ? 2 : 3);
    };
    window.addEventListener("resize", update);
    return () => window.removeEventListener("resize", update);
  }, []);
  return cols;
}
