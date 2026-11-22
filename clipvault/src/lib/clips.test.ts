import { describe, expect, it } from "vitest";
import { insertClip, matchesFilter } from "./clips";
import type { Clip } from "../api";

const clip = (id: string, recorded_at: string, extra: Partial<Clip> = {}): Clip => ({
  id,
  title: `Clip ${id}`,
  description: "",
  path: `E:\\GameClips\\${id}.mp4`,
  game: "Rocket League",
  recorded_at,
  duration_s: 60,
  fps: 60,
  width: 1920,
  height: 1080,
  size: 1,
  trim_in: null,
  trim_out: null,
  thumbnail_path: null,
  source: "native",
  source_id: null,
  tags: [],
  markers: [],
  starred: false,
  compressed: false,
  original_size: null,
  ...extra,
});

describe("insertClip", () => {
  const list = [clip("c", "2025-01-03T10:00:00+01:00"), clip("a", "2025-01-01T10:00:00+01:00")];
  it("inserts by date for newest-first", () => {
    expect(insertClip(list, clip("b", "2025-01-02T10:00:00+01:00"), "newest").map((c) => c.id)).toEqual(["c", "b", "a"]);
    expect(insertClip(list, clip("d", "2025-02-01T10:00:00+01:00"), "newest")[0].id).toBe("d");
  });
  it("inserts by date for oldest-first", () => {
    const old = [...list].reverse();
    expect(insertClip(old, clip("d", "2025-02-01T10:00:00+01:00"), "oldest").map((c) => c.id)).toEqual(["a", "c", "d"]);
  });
  it("replaces a clip that is already listed", () => {
    const got = insertClip(list, clip("a", "2025-01-01T10:00:00+01:00", { title: "new" }), "newest");
    expect(got).toHaveLength(2);
    expect(got[1].title).toBe("new");
  });
});

describe("matchesFilter", () => {
  const c = clip("x", "2025-01-01T10:00:00Z", { title: "Ceiling shot", tags: ["Freestyle"], description: "ranked" });
  it("matches game exactly and search over title, description and tags", () => {
    expect(matchesFilter(c, {})).toBe(true);
    expect(matchesFilter(c, { game: "Rocket League" })).toBe(true);
    expect(matchesFilter(c, { game: "Valorant" })).toBe(false);
    expect(matchesFilter(c, { search: "ceiling" })).toBe(true);
    expect(matchesFilter(c, { search: "FREESTYLE" })).toBe(true);
    expect(matchesFilter(c, { search: "rank" })).toBe(true);
    expect(matchesFilter(c, { search: "  " })).toBe(true);
    expect(matchesFilter(c, { search: "whiff" })).toBe(false);
  });
  it("needs every word, and also searches game, date and markers", () => {
    const m = clip("y", "2025-03-04T10:00:00Z", { markers: [{ t_s: 1, kind: "laugh", label: "big one" }] });
    expect(matchesFilter(m, { search: "rocket" })).toBe(true);
    expect(matchesFilter(m, { search: "2025-03-04" })).toBe(true);
    expect(matchesFilter(m, { search: "laugh" })).toBe(true);
    expect(matchesFilter(m, { search: "big rocket" })).toBe(true);
    expect(matchesFilter(m, { search: "big valorant" })).toBe(false);
  });
  it("starred filter", () => {
    expect(matchesFilter(c, { starred: true })).toBe(false);
    expect(matchesFilter({ ...c, starred: true }, { starred: true })).toBe(true);
  });
});
