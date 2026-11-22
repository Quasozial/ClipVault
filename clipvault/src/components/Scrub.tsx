import { useEffect, useState } from "react";
import { api, fileSrc, type Clip } from "../api";

/** Frames per strip (keep in sync with `thumbs::STRIP_FRAMES`). */
export const STRIP_FRAMES = 8;
/** How long the pointer must rest on a card before a strip is asked for. */
const REST_MS = 250;

/** Strip URLs by `id:trim_in:trim_out` (`null` = could not be made). Lives for the session. */
const cache = new Map<string, string | null>();

const keyOf = (c: Clip) => `${c.id}:${c.trim_in}:${c.trim_out}`;

/**
 * The clip's hover-scrub strip as a URL, once the pointer has rested on the
 * card for a moment; `null` until then, or when no strip can be made (the
 * card then keeps its static thumbnail).
 */
export function useScrubStrip(clip: Clip, hovering: boolean): string | null {
  const key = keyOf(clip);
  const [url, setUrl] = useState<string | null>(() => cache.get(key) ?? null);

  useEffect(() => {
    setUrl(cache.get(key) ?? null);
    if (!hovering || cache.has(key)) return;
    let cancelled = false;
    const t = setTimeout(() => {
      api.ensureScrubStrip(clip.id).then(
        (p) => {
          const u = p ? fileSrc(p) : null;
          cache.set(key, u);
          if (!cancelled) setUrl(u);
        },
        () => {
          // Silent: the static thumbnail stays.
          cache.set(key, null);
        },
      );
    }, REST_MS);
    return () => {
      cancelled = true;
      clearTimeout(t);
    };
  }, [key, hovering, clip.id]);

  return url;
}

/** Strip frame for a pointer at `x` of `width` (0..STRIP_FRAMES-1). */
export function frameAt(x: number, width: number): number {
  if (width <= 0) return 0;
  return Math.max(0, Math.min(STRIP_FRAMES - 1, Math.floor((x / width) * STRIP_FRAMES)));
}
