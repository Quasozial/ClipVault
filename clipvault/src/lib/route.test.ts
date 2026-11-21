import { describe, expect, it } from "vitest";
import { href, parseRoute } from "./stores";

describe("routes", () => {
  it("parses library, clip and settings hashes", () => {
    expect(parseRoute("")).toEqual({ page: "library" });
    expect(parseRoute("#/")).toEqual({ page: "library" });
    expect(parseRoute("#/clip/abc-1")).toEqual({ page: "clip", id: "abc-1" });
    expect(parseRoute("#/settings")).toEqual({ page: "settings", section: undefined });
    expect(parseRoute("#/settings/import")).toEqual({ page: "settings", section: "import" });
    expect(parseRoute("#/nonsense")).toEqual({ page: "library" });
  });
  it("round-trips ids with special characters", () => {
    const r = { page: "clip" as const, id: "a b/c#d" };
    expect(parseRoute(href(r))).toEqual(r);
  });
});
