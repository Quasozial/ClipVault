import { describe, expect, it } from "vitest";
import type { Settings } from "../api";
import { bitrateFor, clipSizeEstimate, diskShares, qualityOf, withFormat, withLength } from "./clipStorage";

const base = { fps: 60, output_height: 1080, bitrate_kbps: 40000, save_length_s: 60, max_save_length_s: 120 } as Settings;

describe("clip storage", () => {
  it("default settings are the High quality", () => {
    expect(bitrateFor("high", 1080, 60)).toBe(40000);
    expect(qualityOf(40000, 1080, 60)).toBe("high");
    expect(qualityOf(41000, 1080, 60)).toBeNull();
  });

  it("bitrate grows with resolution and frame rate, within 2..150 Mbit/s", () => {
    expect(bitrateFor("high", 1440, 60)).toBeGreaterThan(40000);
    expect(bitrateFor("high", 1080, 30)).toBeLessThan(40000);
    expect(bitrateFor("very-high", 2160, 60)).toBeLessThanOrEqual(150000);
    expect(bitrateFor("low", 720, 30)).toBeGreaterThanOrEqual(2000);
  });

  it("a quality bitrate follows a format change, a hand-set one stays", () => {
    const s = withFormat(base, 1440, 60);
    expect(s.output_height).toBe(1440);
    expect(qualityOf(s.bitrate_kbps, 1440, 60)).toBe("high");
    const custom = withFormat({ ...base, bitrate_kbps: 33000 }, 1440, 30);
    expect(custom.bitrate_kbps).toBe(33000);
    expect(custom.fps).toBe(30);
  });

  it("a longer clip raises the longest-clip limit", () => {
    expect(withLength(base, 300)).toMatchObject({ save_length_s: 300, max_save_length_s: 300 });
    expect(withLength(base, 30)).toMatchObject({ save_length_s: 30, max_save_length_s: 120 });
  });

  it("estimates the largest clip size from bitrate and length", () => {
    expect(clipSizeEstimate(base)).toBe(300_000_000);
    expect(clipSizeEstimate({ ...base, save_length_s: 300 })).toBe(600_000_000);
  });

  it("splits used space into clips and the rest of the PC", () => {
    expect(diskShares(100, 60, 10)).toEqual({ clips: 0.1, other: 0.3, used: 0.4 });
    expect(diskShares(100, 60, 80)).toEqual({ clips: 0.4, other: 0, used: 0.4 });
    expect(diskShares(0, 0, 5)).toEqual({ clips: 0, other: 0, used: 0 });
  });
});
