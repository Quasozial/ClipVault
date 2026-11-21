import { describe, expect, it } from "vitest";
import { addTags } from "./tags";
import { settingsOpenSync } from "./settingsOpen";

describe("addTags", () => {
  it("adds every complete comma-separated tag and keeps the unfinished rest", () => {
    expect(addTags(["a"], "b,c,")).toEqual({ tags: ["a", "b", "c"], rest: "" });
    expect(addTags([], "x, y,partial")).toEqual({ tags: ["x", "y"], rest: "partial" });
  });
  it("commits the rest too when asked (Enter / blur)", () => {
    expect(addTags(["a"], " last ", true)).toEqual({ tags: ["a", "last"], rest: "" });
  });
  it("skips blanks and case-insensitive duplicates", () => {
    expect(addTags(["Ranked"], "ranked,, ,new,NEW,", false)).toEqual({ tags: ["Ranked", "new"], rest: "" });
  });
});

describe("settingsOpenSync", () => {
  it("reports open while visible, closed while hidden, without repeats", () => {
    const sent: boolean[] = [];
    const s = settingsOpenSync((v) => sent.push(v));
    s.update(false); // visible on mount
    s.update(false);
    s.update(true); // window hidden to the tray
    s.update(true);
    s.update(false); // shown again
    expect(sent).toEqual([true, false, true]);
  });
  it("sends false on dispose and ignores later updates", () => {
    const sent: boolean[] = [];
    const s = settingsOpenSync((v) => sent.push(v));
    s.update(false);
    s.dispose();
    s.update(false);
    expect(sent).toEqual([true, false]);
  });
  it("does not send a second false when disposed while hidden", () => {
    const sent: boolean[] = [];
    const s = settingsOpenSync((v) => sent.push(v));
    s.update(true);
    s.dispose();
    expect(sent).toEqual([false]);
  });
});
