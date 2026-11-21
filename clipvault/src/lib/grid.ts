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
