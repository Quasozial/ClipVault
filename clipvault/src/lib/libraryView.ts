import type { Clip } from "../api";
import { matchesSearch } from "./clips";

/**
 * Library view model: the whole library is loaded once and searched, filtered,
 * counted and sorted here, so chips can show counts and filtering is instant.
 */

export type LibrarySort = "newest" | "oldest" | "longest" | "largest" | "markers";
export type MarkerFacet = "goal" | "laugh" | "manual" | "imported";
type Facet = "game" | "markers" | "tags" | "starred";

export interface LibraryFilter {
  search: string;
  game: string | null;
  markers: MarkerFacet[];
  tags: string[];
  starred: boolean;
}

export const EMPTY_FILTER: LibraryFilter = { search: "", game: null, markers: [], tags: [], starred: false };

export interface Facets {
  games: [string, number][];
  markers: [MarkerFacet, number][];
  tags: [string, number][];
  starred: number;
}

export function isFiltered(f: LibraryFilter): boolean {
  return Boolean(f.search.trim() || f.game || f.markers.length || f.tags.length || f.starred);
}

/** Markers shown on the card and used by the "Most markers" sort (`saved` excluded). */
export function markerCount(c: Clip): number {
  return c.markers.filter((m) => m.kind !== "saved").length;
}

/** `ignore`: leave that facet out (for counting its own chips). */
export function matchesLibraryFilter(c: Clip, f: LibraryFilter, ignore?: Facet): boolean {
  if (ignore !== "game" && f.game && c.game !== f.game) return false;
  if (ignore !== "starred" && f.starred && !c.starred) return false;
  if (ignore !== "markers" && f.markers.length && !c.markers.some((m) => (f.markers as string[]).includes(m.kind))) return false;
  if (ignore !== "tags" && f.tags.length) {
    const have = new Set(c.tags.map((t) => t.toLowerCase()));
    if (!f.tags.every((t) => have.has(t.toLowerCase()))) return false;
  }
  return matchesSearch(c, f.search);
}

const time = (c: Clip) => new Date(c.recorded_at).getTime();

/** New array; ties in non-date sorts go newest first. */
export function sortClips(clips: Clip[], sort: LibrarySort): Clip[] {
  const newest = (a: Clip, b: Clip) => time(b) - time(a);
  const by: Record<LibrarySort, (a: Clip, b: Clip) => number> = {
    newest,
    oldest: (a, b) => time(a) - time(b),
    longest: (a, b) => b.duration_s - a.duration_s || newest(a, b),
    largest: (a, b) => b.size - a.size || newest(a, b),
    markers: (a, b) => markerCount(b) - markerCount(a) || newest(a, b),
  };
  return clips.slice().sort(by[sort]);
}

export function applyView(all: Clip[], f: LibraryFilter, sort: LibrarySort): Clip[] {
  return sortClips(
    all.filter((c) => matchesLibraryFilter(c, f)),
    sort,
  );
}

const BASE_MARKERS: MarkerFacet[] = ["goal", "laugh", "manual"];

export function facetCounts(all: Clip[], f: LibraryFilter, topTagCount = 8): Facets {
  const games = new Map<string, number>();
  const markers = new Map<MarkerFacet, number>(BASE_MARKERS.map((k) => [k, 0]));
  const tagCounts = new Map<string, number>();
  let starred = 0;
  let anyImported = false;
  for (const c of all) {
    if (!games.has(c.game)) games.set(c.game, 0);
    if (matchesLibraryFilter(c, f, "game")) games.set(c.game, games.get(c.game)! + 1);
    const kinds = new Set(c.markers.map((m) => m.kind));
    if (kinds.has("imported")) anyImported = true;
    if (matchesLibraryFilter(c, f, "markers")) {
      for (const k of kinds) if (k !== "saved") markers.set(k as MarkerFacet, (markers.get(k as MarkerFacet) ?? 0) + 1);
    }
    if (matchesLibraryFilter(c, f, "tags")) {
      for (const t of new Set(c.tags.map((t) => t.toLowerCase()))) tagCounts.set(t, (tagCounts.get(t) ?? 0) + 1);
    }
    if (c.starred && matchesLibraryFilter(c, f, "starred")) starred++;
  }
  const markerList = [...BASE_MARKERS, ...(anyImported ? (["imported"] as MarkerFacet[]) : [])];
  const ranked = [...tagCounts.entries()].sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]));
  const tags = ranked.slice(0, topTagCount);
  for (const sel of f.tags.map((t) => t.toLowerCase())) {
    if (!tags.some(([t]) => t === sel)) tags.push([sel, tagCounts.get(sel) ?? 0]);
  }
  return {
    games: [...games.entries()].sort((a, b) => a[0].localeCompare(b[0])),
    markers: markerList.map((k) => [k, markers.get(k) ?? 0]),
    tags,
    starred,
  };
}
