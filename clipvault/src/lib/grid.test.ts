import { describe, expect, it } from "vitest";
import { buildLayout, columnsFor, headingAt, moveFocus, visibleRange, visibleRows } from "./grid";

describe("columnsFor", () => {
  it("fits as many min-width cards as possible, at least one", () => {
    expect(columnsFor(1000, 240, 16)).toBe(3); // 4 cards need 4*240 + 3*16 = 1008
    expect(columnsFor(1008, 240, 16)).toBe(4);
    expect(columnsFor(100, 240, 16)).toBe(1);
    expect(columnsFor(3000, 240, 16, 4)).toBe(4);
  });
});

describe("visibleRows", () => {
  it("covers the viewport plus overscan", () => {
    expect(visibleRows({ scrollTop: 0, viewport: 600, rowHeight: 200, rowCount: 100, overscan: 1 })).toEqual({
      first: 0,
      last: 3,
    });
    expect(visibleRows({ scrollTop: 1000, viewport: 600, rowHeight: 200, rowCount: 100, overscan: 1 })).toEqual({
      first: 4,
      last: 8,
    });
  });
  it("clamps to the row count", () => {
    expect(visibleRows({ scrollTop: 0, viewport: 5000, rowHeight: 200, rowCount: 3, overscan: 2 })).toEqual({
      first: 0,
      last: 2,
    });
    expect(visibleRows({ scrollTop: 0, viewport: 600, rowHeight: 200, rowCount: 0, overscan: 2 })).toEqual({
      first: 0,
      last: -1,
    });
  });
});

describe("buildLayout", () => {
  const groups = [
    { label: "Today", items: ["a", "b", "c", "d", "e"] },
    { label: "Yesterday", items: ["f", "g"] },
  ];
  const o = { cols: 4, cardRowH: 200, headingH: 40, headings: true };
  it("interleaves heading rows and card rows with cumulative tops", () => {
    const { rows, height } = buildLayout(groups, o);
    expect(rows.map((r) => [r.kind, r.top, r.height])).toEqual([
      ["heading", 0, 40],
      ["cards", 40, 200],
      ["cards", 240, 200],
      ["heading", 440, 40],
      ["cards", 480, 200],
    ]);
    expect(height).toBe(680);
    const cards = rows.filter((r) => r.kind === "cards");
    expect(cards.map((r) => (r.kind === "cards" ? [r.firstIndex, r.items] : null))).toEqual([
      [0, ["a", "b", "c", "d"]],
      [4, ["e"]],
      [5, ["f", "g"]],
    ]);
  });
  it("leaves out headings and packs rows across groups when headings are off", () => {
    const { rows } = buildLayout(groups, { ...o, headings: false });
    expect(rows.map((r) => r.kind)).toEqual(["cards", "cards"]);
    expect(rows[1].kind === "cards" && rows[1].items).toEqual(["e", "f", "g"]);
  });
  it("counts items on heading rows", () => {
    const { rows } = buildLayout(groups, o);
    expect(rows[3].kind === "heading" && rows[3].count).toBe(2);
  });
});

describe("visibleRange", () => {
  it("agrees with fixed-height windowing for equal rows", () => {
    const rows = Array.from({ length: 100 }, (_, i) => ({ top: i * 200, height: 200 }));
    expect(visibleRange(rows, 1000, 600, 1)).toEqual(visibleRows({ scrollTop: 1000, viewport: 600, rowHeight: 200, rowCount: 100, overscan: 1 }));
    expect(visibleRange(rows, 0, 600, 1)).toEqual({ first: 0, last: 3 });
  });
  it("handles mixed heights", () => {
    const rows = [
      { top: 0, height: 40 },
      { top: 40, height: 200 },
      { top: 240, height: 200 },
      { top: 440, height: 40 },
      { top: 480, height: 200 },
    ];
    expect(visibleRange(rows, 250, 200, 0)).toEqual({ first: 2, last: 3 });
    expect(visibleRange(rows, 250, 240, 0)).toEqual({ first: 2, last: 4 });
    expect(visibleRange([], 0, 600, 2)).toEqual({ first: 0, last: -1 });
  });
});

describe("headingAt", () => {
  const { rows } = buildLayout(
    [
      { label: "Today", items: [1, 2, 3, 4, 5] },
      { label: "Yesterday", items: [6] },
    ],
    { cols: 4, cardRowH: 200, headingH: 40, headings: true },
  );
  it("names the group under the top edge", () => {
    expect(headingAt(rows, 0)).toBe("Today");
    expect(headingAt(rows, 439)).toBe("Today");
    expect(headingAt(rows, 440)).toBe("Yesterday");
    expect(headingAt(rows, 600)).toBe("Yesterday");
  });
  it("is null without headings", () => {
    expect(headingAt(buildLayout([{ label: "", items: [1] }], { cols: 4, cardRowH: 200, headingH: 40, headings: false }).rows, 0)).toBe(null);
  });
});

describe("moveFocus", () => {
  const { rows } = buildLayout(
    [
      { label: "A", items: [0, 1, 2, 3, 4] },
      { label: "B", items: [5, 6, 7] },
    ],
    { cols: 4, cardRowH: 200, headingH: 40, headings: true },
  );
  it("moves left and right in the flat order, clamped", () => {
    expect(moveFocus(rows, 0, "ArrowLeft")).toBe(0);
    expect(moveFocus(rows, 3, "ArrowRight")).toBe(4);
    expect(moveFocus(rows, 7, "ArrowRight")).toBe(7);
  });
  it("moves down to the same column, clamped to a short row", () => {
    expect(moveFocus(rows, 3, "ArrowDown")).toBe(4);
    expect(moveFocus(rows, 4, "ArrowDown")).toBe(5);
    expect(moveFocus(rows, 1, "ArrowDown")).toBe(4);
  });
  it("moves up across a heading and stops at the top", () => {
    expect(moveFocus(rows, 6, "ArrowUp")).toBe(4);
    expect(moveFocus(rows, 2, "ArrowUp")).toBe(2);
  });
});
