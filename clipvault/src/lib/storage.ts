import type { Clip } from "../api";

/**
 * Disk use of the library. `saved` counts only clips whose original size is
 * known; `lowerBound` says some compressed clips predate that record, so the
 * real saving is at least `saved`.
 */
export function storageSummary(clips: Clip[]): { total: number; saved: number; lowerBound: boolean } {
  let total = 0;
  let saved = 0;
  let lowerBound = false;
  for (const c of clips) {
    total += c.size;
    if (c.original_size !== null && c.original_size > c.size) saved += c.original_size - c.size;
    else if (c.compressed && c.original_size === null) lowerBound = true;
  }
  return { total, saved, lowerBound };
}
