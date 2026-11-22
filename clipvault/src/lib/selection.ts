/** Library multi-select helpers. Sets are never mutated; each returns a new one. */

export function toggle(sel: ReadonlySet<string>, id: string): Set<string> {
  const out = new Set(sel);
  if (out.has(id)) out.delete(id);
  else out.add(id);
  return out;
}

/** Shift-click: adds every id between `anchor` and `id` (inclusive, in `order`). A missing anchor toggles `id` alone. */
export function selectRange(sel: ReadonlySet<string>, order: string[], anchor: string | null, id: string): Set<string> {
  const a = anchor === null ? -1 : order.indexOf(anchor);
  const b = order.indexOf(id);
  if (a < 0 || b < 0) return toggle(sel, id);
  const out = new Set(sel);
  for (let i = Math.min(a, b); i <= Math.max(a, b); i++) out.add(order[i]);
  return out;
}

/** Keeps only ids still visible (after a filter change). */
export function prune(sel: ReadonlySet<string>, visible: string[]): Set<string> {
  const v = new Set(visible);
  return new Set([...sel].filter((id) => v.has(id)));
}

export interface BulkResult {
  ok: string[];
  failed: { id: string; error: string }[];
}

/** Runs `op` for each id one after another; failures are collected, not thrown. Stops before the next id once `signal` is aborted. */
export async function runBulk<T>(
  ids: string[],
  op: (id: string) => Promise<T>,
  opts: { signal?: AbortSignal; onProgress?(done: number): void } = {},
): Promise<BulkResult> {
  const out: BulkResult = { ok: [], failed: [] };
  let done = 0;
  for (const id of ids) {
    if (opts.signal?.aborted) break;
    try {
      await op(id);
      out.ok.push(id);
    } catch (e) {
      out.failed.push({ id, error: e instanceof Error ? e.message : String(e) });
    }
    opts.onProgress?.(++done);
  }
  return out;
}
