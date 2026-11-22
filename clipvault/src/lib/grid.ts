import type { Group } from "./dateGroups";

/** How many cards of at least `minWidth` fit in `width` with `gap` between them (1..maxCols). */
export function columnsFor(width: number, minWidth: number, gap: number, maxCols = Infinity): number {
  return Math.max(1, Math.min(maxCols, Math.floor((width + gap) / (minWidth + gap))));
}

export interface RowWindow {
  scrollTop: number;
  viewport: number;
  rowHeight: number;
  rowCount: number;
  overscan: number;
}

/** Inclusive range of rows to render; `last < first` when there are none. */
export function visibleRows({ scrollTop, viewport, rowHeight, rowCount, overscan }: RowWindow): {
  first: number;
  last: number;
} {
  if (rowCount <= 0 || rowHeight <= 0) return { first: 0, last: -1 };
  const first = Math.max(0, Math.floor(scrollTop / rowHeight) - overscan);
  const last = Math.min(rowCount - 1, Math.ceil((scrollTop + viewport) / rowHeight) - 1 + overscan);
  return { first, last };
}


export type LayoutRow<T> =
  | { kind: "heading"; label: string; count: number; top: number; height: number }
  | { kind: "cards"; items: T[]; firstIndex: number; top: number; height: number };

export interface Layout<T> {
  rows: LayoutRow<T>[];
  height: number;
}

/**
 * Rows for the virtualised grid. With `headings`, each group starts with a
 * heading row and its own card rows; without, all items pack into one run.
 * `cardRowH` includes the gap below a card row.
 */
export function buildLayout<T>(
  groups: Group<T>[],
  o: { cols: number; cardRowH: number; headingH: number; headings: boolean },
): Layout<T> {
  const rows: LayoutRow<T>[] = [];
  let top = 0;
  let index = 0;
  const runs = o.headings ? groups : [{ label: "", items: groups.flatMap((g) => g.items) }];
  for (const g of runs) {
    if (g.items.length === 0) continue;
    if (o.headings) {
      rows.push({ kind: "heading", label: g.label, count: g.items.length, top, height: o.headingH });
      top += o.headingH;
    }
    for (let i = 0; i < g.items.length; i += o.cols) {
      const items = g.items.slice(i, i + o.cols);
      rows.push({ kind: "cards", items, firstIndex: index, top, height: o.cardRowH });
      index += items.length;
      top += o.cardRowH;
    }
  }
  return { rows, height: top };
}

/** Inclusive range of rows that intersect the viewport, plus `overscan` rows each side. */
export function visibleRange(rows: { top: number; height: number }[], scrollTop: number, viewport: number, overscan: number): { first: number; last: number } {
  if (rows.length === 0) return { first: 0, last: -1 };
  // First row whose bottom is below scrollTop.
  let lo = 0;
  let hi = rows.length - 1;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (rows[mid].top + rows[mid].height > scrollTop) hi = mid;
    else lo = mid + 1;
  }
  let last = lo;
  while (last + 1 < rows.length && rows[last + 1].top < scrollTop + viewport) last++;
  return { first: Math.max(0, lo - overscan), last: Math.min(rows.length - 1, last + overscan) };
}

/** Label of the group the top edge at `scrollTop` is in; null when there are no headings. */
export function headingAt<T>(rows: LayoutRow<T>[], scrollTop: number): string | null {
  let label: string | null = null;
  for (const r of rows) {
    if (r.top > scrollTop) break;
    if (r.kind === "heading") label = r.label;
  }
  return label;
}

/** Grid-aware keyboard move over the flat item order; headings are skipped. */
export function moveFocus<T>(rows: LayoutRow<T>[], index: number, key: "ArrowLeft" | "ArrowRight" | "ArrowUp" | "ArrowDown"): number {
  const cards = rows.filter((r): r is Extract<LayoutRow<T>, { kind: "cards" }> => r.kind === "cards");
  if (cards.length === 0) return index;
  const total = cards[cards.length - 1].firstIndex + cards[cards.length - 1].items.length;
  if (key === "ArrowLeft") return Math.max(0, index - 1);
  if (key === "ArrowRight") return Math.min(total - 1, index + 1);
  const r = cards.findIndex((c) => index >= c.firstIndex && index < c.firstIndex + c.items.length);
  if (r < 0) return index;
  const target = cards[key === "ArrowUp" ? r - 1 : r + 1];
  if (!target) return index;
  return target.firstIndex + Math.min(index - cards[r].firstIndex, target.items.length - 1);
}
