import { useEffect, useRef, useState } from "react";
import { api, fileSrc, type Clip } from "../api";
import { reportError } from "../lib/toast";

/** Resolved thumbnail paths by clip id (`null` = could not be made). Lives for the session. */
const cache = new Map<string, string | null>();
let reportedFailure = false;

/**
 * Clip thumbnail. Asks the backend (`ensure_thumbnail`, one FFmpeg at a time)
 * only once the card is actually in view; shows a placeholder until then and
 * when no thumbnail can be made.
 */
export function Thumb({ clip, root }: { clip: Clip; root: Element | null }) {
  const ref = useRef<HTMLDivElement>(null);
  const known = cache.has(clip.id) ? cache.get(clip.id)! : clip.thumbnail_path;
  const [path, setPath] = useState<string | null | undefined>(known ?? undefined);
  const [broken, setBroken] = useState(false);
  // A stored path can point at a file that is gone (cache cleared, data moved):
  // the first load error asks the backend once, which regenerates missing files.
  const [retried, setRetried] = useState(false);
  const onImgError = () => {
    if (retried) {
      setBroken(true);
      return;
    }
    setRetried(true);
    cache.delete(clip.id);
    setPath(undefined);
  };

  useEffect(() => {
    if (path !== undefined || cache.has(clip.id)) return;
    const el = ref.current;
    if (!el) return;
    let cancelled = false;
    const io = new IntersectionObserver(
      (entries) => {
        if (!entries.some((e) => e.isIntersecting)) return;
        io.disconnect();
        api.ensureThumbnail(clip.id).then(
          (p) => {
            cache.set(clip.id, p);
            if (!cancelled) setPath(p);
          },
          (e) => {
            cache.set(clip.id, null);
            if (!cancelled) setPath(null);
            // Once per session: every card failing the same way would flood the toasts.
            if (!reportedFailure) {
              reportedFailure = true;
              reportError("Could not load thumbnails", e);
            }
          },
        );
      },
      { root, rootMargin: "100px 0px" },
    );
    io.observe(el);
    return () => {
      cancelled = true;
      io.disconnect();
    };
  }, [clip.id, path, root]);

  const showImg = path && !broken;
  return (
    <div ref={ref} className={`thumb${showImg ? "" : " thumb-empty"}`}>
      {showImg ? (
        <img src={fileSrc(path)} alt="" loading="lazy" decoding="async" draggable={false} onError={onImgError} />
      ) : (
        <svg className="thumb-ph" viewBox="0 0 24 24" width="28" height="28" aria-hidden>
          <rect x="3" y="5" width="18" height="14" rx="2" fill="none" stroke="currentColor" strokeWidth="1.4" />
          <path d="M10 9.5v5l4.2-2.5Z" fill="currentColor" />
        </svg>
      )}
    </div>
  );
}

/** Lets the clip view tell the grid about a regenerated or removed thumbnail. */
export function forgetThumb(id: string) {
  cache.delete(id);
}
