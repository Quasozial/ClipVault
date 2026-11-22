import { describe, expect, it } from "vitest";
import { groupByDate, groupLabel } from "./dateGroups";

const monday = new Date(2026, 10, 23, 10); // Mon 23 Nov 2026
const wednesday = new Date(2026, 10, 25, 10);

describe("groupLabel", () => {
  it("puts today and the future under Today", () => {
    expect(groupLabel(new Date(2026, 10, 23, 0, 0), monday)).toBe("Today");
    expect(groupLabel(new Date(2026, 10, 24, 9), monday)).toBe("Today");
  });
  it("prefers Yesterday over Last week on a Monday", () => {
    expect(groupLabel(new Date(2026, 10, 22, 23, 59), monday)).toBe("Yesterday");
  });
  it("splits this week from last week at Monday 00:00", () => {
    expect(groupLabel(new Date(2026, 10, 23, 0, 0), wednesday)).toBe("This week");
    expect(groupLabel(new Date(2026, 10, 22, 23, 59), wednesday)).toBe("Last week");
    expect(groupLabel(new Date(2026, 10, 16, 0, 0), wednesday)).toBe("Last week");
  });
  it("uses the month for older dates this year and the year before that", () => {
    expect(groupLabel(new Date(2026, 10, 15, 23), wednesday)).toBe("November 2026");
    expect(groupLabel(new Date(2026, 9, 3), wednesday)).toBe("October 2026");
    expect(groupLabel(new Date(2025, 11, 31, 23), wednesday)).toBe("2025");
  });
});

describe("groupByDate", () => {
  it("keeps the input order and merges equal neighbours", () => {
    const items = [new Date(2025, 0, 1), new Date(2025, 5, 1), new Date(2026, 9, 3), new Date(2026, 10, 25, 8)];
    const groups = groupByDate(items, (d) => d, wednesday);
    expect(groups.map((g) => [g.label, g.items.length])).toEqual([
      ["2025", 2],
      ["October 2026", 1],
      ["Today", 1],
    ]);
  });
  it("returns no groups for no items", () => {
    expect(groupByDate([], (d: Date) => d, wednesday)).toEqual([]);
  });
});
