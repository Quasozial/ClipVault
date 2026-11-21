import type { Clip, ClipFilter, SortOrder } from "../api";

/** Client-side mirror of the backend filter (`db::clips::list`), for live-inserted clips. */
export function matchesFilter(clip: Clip, filter: ClipFilter): boolean {
  if (filter.game && clip.game !== filter.game) return false;
  if (filter.starred && !clip.starred) return false;
  const words = (filter.search ?? "").toLowerCase().split(/\s+/).filter(Boolean);
  if (words.length === 0) return true;
  const fields = [
    clip.title,
    clip.description,
    ...clip.tags,
    clip.game,
    clip.recorded_at,
    ...clip.markers.flatMap((m) => [m.label, m.kind]),
  ].map((f) => f.toLowerCase());
  return words.every((w) => fields.some((f) => f.includes(w)));
}

const time = (c: Clip) => new Date(c.recorded_at).getTime();

/** Inserts (or replaces, by id) `clip` keeping the list sorted by date. */
export function insertClip(list: Clip[], clip: Clip, sort: SortOrder): Clip[] {
  const existing = list.findIndex((c) => c.id === clip.id);
  if (existing >= 0) {
    const out = list.slice();
    out[existing] = clip;
    return out;
  }
  const t = time(clip);
  const idx = list.findIndex((c) => (sort === "newest" ? time(c) < t : time(c) > t));
  const out = list.slice();
  out.splice(idx < 0 ? out.length : idx, 0, clip);
  return out;
}
