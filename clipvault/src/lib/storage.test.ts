import { describe, expect, it } from "vitest";
import type { Clip } from "../api";
import { storageSummary } from "./storage";

const c = (size: number, compressed = false, original_size: number | null = null) => ({ size, compressed, original_size }) as Clip;

describe("storageSummary", () => {
  it("sums sizes and what compression saved", () => {
    expect(storageSummary([c(100), c(40, true, 100), c(50, true, 80)])).toEqual({ total: 190, saved: 90, lowerBound: false });
  });
  it("is a lower bound when a compressed clip has no original size", () => {
    expect(storageSummary([c(40, true, 100), c(70, true, null)])).toEqual({ total: 110, saved: 60, lowerBound: true });
  });
  it("ignores an original size that is not larger", () => {
    expect(storageSummary([c(100, true, 100)]).saved).toBe(0);
  });
  it("is zero for no clips", () => {
    expect(storageSummary([])).toEqual({ total: 0, saved: 0, lowerBound: false });
  });
});
