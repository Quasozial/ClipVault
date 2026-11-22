import { describe, expect, it } from "vitest";
import type { Clip, Marker } from "../api";
import { applyView, EMPTY_FILTER, facetCounts, isFiltered, markerCount, matchesLibraryFilter, sortClips, type LibraryFilter } from "./libraryView";

let n = 0;
const clip = (extra: Partial<Clip> = {}): Clip => ({
  id: `c${++n}`,
  title: "Clip",
  description: "",
  path: "E:\\GameClips\\x.mp4",
  game: "Rocket League",
  recorded_at: "2026-11-20T10:00:00+01:00",
  duration_s: 60,
  fps: 60,
  width: 1920,
  height: 1080,
  size: 100,
  trim_in: null,
  trim_out: null,
  thumbnail_path: null,
  source: "native",
  source_id: null,
  tags: [],
  markers: [],
  starred: false,
  compressed: false,
  ...extra,
});
const mk = (kind: Marker["kind"], label = ""): Marker => ({ t_s: 1, kind, label });
const f = (p: Partial<LibraryFilter>): LibraryFilter => ({ ...EMPTY_FILTER, ...p });

describe("matchesLibraryFilter", () => {
  it("matches the game exactly", () => {
    expect(matchesLibraryFilter(clip({ game: "Valorant" }), f({ game: "Valorant" }))).toBe(true);
    expect(matchesLibraryFilter(clip({ game: "Valorant" }), f({ game: "Rocket League" }))).toBe(false);
  });
  it("matches any of the selected marker kinds", () => {
    const c = clip({ markers: [mk("laugh")] });
    expect(matchesLibraryFilter(c, f({ markers: ["goal", "laugh"] }))).toBe(true);
    expect(matchesLibraryFilter(c, f({ markers: ["goal"] }))).toBe(false);
  });
  it("needs all selected tags, ignoring case", () => {
    const c = clip({ tags: ["Clutch", "ranked"] });
    expect(matchesLibraryFilter(c, f({ tags: ["clutch", "ranked"] }))).toBe(true);
    expect(matchesLibraryFilter(c, f({ tags: ["clutch", "funny"] }))).toBe(false);
  });
  it("honours starred", () => {
    expect(matchesLibraryFilter(clip(), f({ starred: true }))).toBe(false);
    expect(matchesLibraryFilter(clip({ starred: true }), f({ starred: true }))).toBe(true);
  });
  it("searches marker kinds like the backend", () => {
    expect(matchesLibraryFilter(clip({ markers: [mk("goal")] }), f({ search: "goal" }))).toBe(true);
    expect(matchesLibraryFilter(clip(), f({ search: "goal" }))).toBe(false);
  });
  it("skips only the ignored facet", () => {
    const c = clip({ game: "Valorant" });
    expect(matchesLibraryFilter(c, f({ game: "Rocket League" }), "game")).toBe(true);
    expect(matchesLibraryFilter(c, f({ game: "Rocket League", starred: true }), "game")).toBe(false);
  });
});

describe("isFiltered", () => {
  it("is false only for the empty filter", () => {
    expect(isFiltered(EMPTY_FILTER)).toBe(false);
    expect(isFiltered(f({ search: "  " }))).toBe(false);
    expect(isFiltered(f({ tags: ["x"] }))).toBe(true);
  });
});

describe("markerCount", () => {
  it("does not count saved markers", () => {
    expect(markerCount(clip({ markers: [mk("saved"), mk("saved"), mk("goal")] }))).toBe(1);
  });
});

describe("sortClips", () => {
  const old = clip({ recorded_at: "2026-11-01T10:00:00+01:00", duration_s: 30, size: 500 });
  const mid = clip({ recorded_at: "2026-11-10T10:00:00+01:00", duration_s: 90, size: 100, markers: [mk("saved"), mk("saved"), mk("saved")] });
  const neu = clip({ recorded_at: "2026-11-20T10:00:00+01:00", duration_s: 30, size: 100, markers: [mk("goal")] });
  const all = [old, mid, neu];
  it("orders by date both ways", () => {
    expect(sortClips(all, "newest")).toEqual([neu, mid, old]);
    expect(sortClips(all, "oldest")).toEqual([old, mid, neu]);
  });
  it("orders by length, then newest", () => {
    expect(sortClips(all, "longest")).toEqual([mid, neu, old]);
  });
  it("orders by size, then newest", () => {
    expect(sortClips(all, "largest")).toEqual([old, neu, mid]);
  });
  it("orders by markers without saved ones, then newest", () => {
    expect(sortClips(all, "markers")).toEqual([neu, mid, old]);
  });
  it("does not change the input", () => {
    const copy = all.slice();
    sortClips(all, "oldest");
    expect(all).toEqual(copy);
  });
});

describe("applyView", () => {
  it("places a live-inserted matching clip by sort and drops a non-matching one", () => {
    const a = clip({ recorded_at: "2026-11-01T10:00:00+01:00" });
    const fresh = clip({ recorded_at: "2026-11-22T10:00:00+01:00" });
    const other = clip({ recorded_at: "2026-11-22T11:00:00+01:00", game: "Valorant" });
    expect(applyView([a, fresh, other], f({ game: "Rocket League" }), "newest")).toEqual([fresh, a]);
  });
});

describe("facetCounts", () => {
  const rl1 = clip({ title: "flip", markers: [mk("goal")], tags: ["ranked"] });
  const rl2 = clip({ title: "save", starred: true, tags: ["ranked", "funny"] });
  const val = clip({ title: "flip", game: "Valorant", markers: [mk("laugh")] });
  const all = [rl1, rl2, val];
  it("counts games ignoring the game selection but honouring search", () => {
    const fc = facetCounts(all, f({ game: "Valorant", search: "flip" }));
    expect(fc.games).toEqual([
      ["Rocket League", 1],
      ["Valorant", 1],
    ]);
  });
  it("lists imported only when some clip has an imported marker", () => {
    expect(facetCounts(all, EMPTY_FILTER).markers.map(([k]) => k)).toEqual(["goal", "laugh", "manual"]);
    const withImp = [...all, clip({ markers: [mk("imported")] })];
    expect(facetCounts(withImp, EMPTY_FILTER).markers.map(([k]) => k)).toEqual(["goal", "laugh", "manual", "imported"]);
  });
  it("counts marker kinds within the other filters", () => {
    expect(facetCounts(all, f({ game: "Rocket League" })).markers).toEqual([
      ["goal", 1],
      ["laugh", 0],
      ["manual", 0],
    ]);
  });
  it("keeps the top tags by count, ties alphabetical", () => {
    const many = Array.from({ length: 10 }, (_, i) => clip({ tags: [`t${i}`] }));
    const fc = facetCounts([...all, ...many], EMPTY_FILTER, 3);
    expect(fc.tags).toEqual([
      ["ranked", 2],
      ["funny", 1],
      ["t0", 1],
    ]);
  });
  it("keeps selected tags listed even when outside the top", () => {
    const many = Array.from({ length: 10 }, (_, i) => clip({ tags: [`t${i}`] }));
    const fc = facetCounts([...all, ...many], f({ tags: ["t9"] }), 2);
    expect(fc.tags.map(([t]) => t)).toContain("t9");
  });
  it("counts starred within the other filters", () => {
    expect(facetCounts(all, f({ game: "Rocket League" })).starred).toBe(1);
    expect(facetCounts(all, f({ game: "Valorant" })).starred).toBe(0);
  });
});
