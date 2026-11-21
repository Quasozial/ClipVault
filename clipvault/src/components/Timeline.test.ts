import { describe, expect, it } from "vitest";
import { clampTrim, markerPositions, resamplePeaks, tickStep, timeFromX } from "./Timeline";
import type { Marker } from "../api";

const m = (t_s: number, kind: Marker["kind"] = "goal"): Marker => ({ t_s, kind, label: "" });

describe("markerPositions", () => {
  it("markerPositions_scales_to_width", () => {
    expect(markerPositions(60, [m(30)], 600)).toEqual([{ x: 300, kind: "goal" }]);
  });

  it("markerPositions_drops_out_of_range", () => {
    const got = markerPositions(60, [m(-1), m(0, "saved"), m(60, "laugh"), m(61), m(NaN)], 600);
    expect(got).toEqual([
      { x: 0, kind: "saved" },
      { x: 600, kind: "laugh" },
    ]);
  });

  it("returns nothing for a zero or unknown duration", () => {
    expect(markerPositions(0, [m(0)], 600)).toEqual([]);
    expect(markerPositions(NaN, [m(1)], 600)).toEqual([]);
  });
});

describe("tickStep", () => {
  it("picks a round step giving at most ~8 ticks", () => {
    expect(tickStep(10)).toBe(2);
    expect(tickStep(60)).toBe(10);
    expect(tickStep(120)).toBe(15);
    expect(tickStep(600)).toBe(120);
    expect(tickStep(0)).toBe(1);
  });
});

describe("clampTrim", () => {
  it("keeps in before out with a minimum gap, inside the clip", () => {
    expect(clampTrim(-2, 70, 60)).toEqual([0, 60]);
    const [a, b] = clampTrim(30, 30.1, 60, "in"); // the dragged handle yields
    expect(a).toBeCloseTo(29.6);
    expect(b).toBeCloseTo(30.1);
    expect(clampTrim(30, 30.1, 60, "out")).toEqual([30, 30.5]);
    expect(clampTrim(59.9, 60, 60, "in")).toEqual([59.5, 60]);
    expect(clampTrim(0, 0.2, 60, "out")).toEqual([0, 0.5]);
  });
});

describe("timeFromX", () => {
  it("maps pixels back to seconds, clamped to the clip", () => {
    expect(timeFromX(150, 600, 60)).toBe(15);
    expect(timeFromX(-20, 600, 60)).toBe(0);
    expect(timeFromX(700, 600, 60)).toBe(60);
    expect(timeFromX(10, 0, 60)).toBe(0);
  });
});

describe("resamplePeaks", () => {
  it("takes the max of each column's slice", () => {
    // 4 s at 2 peaks/s -> 8 peaks, 4 columns of 2
    expect(resamplePeaks([0.1, 0.5, 0.2, 0.2, 0, 0.9, 0.3, 0.1], 2, 4, 4)).toEqual([0.5, 0.2, 0.9, 0.3]);
  });

  it("pads with silence past the end of the peaks", () => {
    expect(resamplePeaks([0.4], 1, 2, 2)).toEqual([0.4, 0]);
  });

  it("is all zero for no data or no duration", () => {
    expect(resamplePeaks([], 50, 10, 3)).toEqual([0, 0, 0]);
    expect(resamplePeaks([1], 50, 0, 2)).toEqual([0, 0]);
  });
});
