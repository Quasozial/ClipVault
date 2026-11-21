import { describe, expect, it } from "vitest";
import { columnsFor, visibleRows } from "./grid";

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
