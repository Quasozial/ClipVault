import { describe, expect, it } from "vitest";
import { prune, runBulk, selectRange, toggle } from "./selection";

const order = ["a", "b", "c", "d", "e"];

describe("toggle", () => {
  it("adds and removes one id without touching the input", () => {
    const s = new Set(["a"]);
    expect([...toggle(s, "b")]).toEqual(["a", "b"]);
    expect([...toggle(s, "a")]).toEqual([]);
    expect([...s]).toEqual(["a"]);
  });
});

describe("selectRange", () => {
  it("adds the inclusive span forwards and backwards to the selection", () => {
    expect([...selectRange(new Set(["e"]), order, "b", "d")].sort()).toEqual(["b", "c", "d", "e"]);
    expect([...selectRange(new Set(), order, "d", "b")].sort()).toEqual(["b", "c", "d"]);
  });
  it("falls back to a single toggle when the anchor is gone or missing", () => {
    expect([...selectRange(new Set(), order, "zz", "c")]).toEqual(["c"]);
    expect([...selectRange(new Set(), order, null, "c")]).toEqual(["c"]);
  });
});

describe("prune", () => {
  it("drops ids that are no longer visible", () => {
    expect([...prune(new Set(["a", "x", "c"]), order)].sort()).toEqual(["a", "c"]);
  });
});

describe("runBulk", () => {
  it("runs one at a time, collecting failures", async () => {
    let running = 0;
    let maxRunning = 0;
    const progress: number[] = [];
    const r = await runBulk(
      ["a", "b", "c"],
      async (id) => {
        running++;
        maxRunning = Math.max(maxRunning, running);
        await Promise.resolve();
        running--;
        if (id === "b") throw new Error("locked");
        return id;
      },
      { onProgress: (n) => progress.push(n) },
    );
    expect(r).toEqual({ ok: ["a", "c"], failed: [{ id: "b", error: "locked" }] });
    expect(maxRunning).toBe(1);
    expect(progress).toEqual([1, 2, 3]);
  });
  it("stops before the next id once aborted", async () => {
    const ctl = new AbortController();
    const seen: string[] = [];
    const r = await runBulk(["a", "b", "c"], async (id) => {
      seen.push(id);
      ctl.abort();
    }, { signal: ctl.signal });
    expect(seen).toEqual(["a"]);
    expect(r.ok).toEqual(["a"]);
  });
});
