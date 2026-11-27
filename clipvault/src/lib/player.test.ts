import { describe, expect, it } from "vitest";
import type { Marker } from "../api";
import { allTracksMixed, canDecodeHevc, EXPORT_PRESETS, exportOptions, exportTracks, frameStep, hasNoPicture, markerJump, stepSpeed } from "./player";

describe("stepSpeed", () => {
  it("steps through the list and stops at the ends", () => {
    expect(stepSpeed(1, 1)).toBe(1.5);
    expect(stepSpeed(1, -1)).toBe(0.5);
    expect(stepSpeed(2, 1)).toBe(2);
    expect(stepSpeed(0.1, -1)).toBe(0.1);
    expect(stepSpeed(0.7, 1)).toBe(1); // unknown rate resets
  });
});

describe("frameStep", () => {
  it("moves exactly one frame and lands mid-frame", () => {
    // t = 1.0 at 60 fps shows frame 60.
    const t1 = frameStep(1.0, 60, 1);
    expect(t1).toBeCloseTo(61.5 / 60, 6);
    expect(frameStep(t1, 60, 1)).toBeCloseTo(62.5 / 60, 6);
    expect(frameStep(t1, 60, -1)).toBeCloseTo(60.5 / 60, 6);
    expect(frameStep(1.0, 60, -1)).toBeCloseTo(59.5 / 60, 6);
  });
  it("does not drift over many steps", () => {
    let t = 0;
    for (let i = 0; i < 300; i++) t = frameStep(t, 30, 1);
    expect(t).toBeCloseTo(300.5 / 30, 6);
  });
  it("clamps at 0 and defaults fps", () => {
    expect(frameStep(0, 60, -1)).toBe(0);
    expect(frameStep(0, 0, 1)).toBeCloseTo(1.5 / 60, 6);
  });
});

describe("markerJump", () => {
  const m = (t_s: number): Marker => ({ t_s, kind: "goal", label: "" });
  const ms = [m(30), m(10), m(20)];
  it("finds next and previous, skipping the one under the playhead", () => {
    expect(markerJump(ms, 0, 1)).toBe(10);
    expect(markerJump(ms, 10, 1)).toBe(20);
    expect(markerJump(ms, 25, -1)).toBe(20);
    expect(markerJump(ms, 20.1, -1)).toBe(10);
    expect(markerJump(ms, 30, 1)).toBeNull();
    expect(markerJump(ms, 5, -1)).toBeNull();
    expect(markerJump([], 5, 1)).toBeNull();
  });
});

describe("export options", () => {
  const tracks = [
    { index: 0, key: "game-0" },
    { index: 1, key: "mic-1" },
    { index: 2, key: "voice-2" },
  ];
  it("drops muted and silent tracks, keeps volumes", () => {
    expect(exportTracks(tracks, { "game-0": 0.8, "mic-1": 0 }, { "voice-2": true })).toEqual([{ index: 0, volume: 0.8 }]);
    expect(exportTracks(tracks, {}, {})).toHaveLength(3);
  });
  it("carries the preset", () => {
    const o = exportOptions(EXPORT_PRESETS[0], [{ index: 0, volume: 1 }]);
    expect(o).toEqual({ label: "discord", max_mb: 10, height: 720, tracks: [{ index: 0, volume: 1 }] });
  });
});

describe("allTracksMixed", () => {
  it("keeps every track at full volume", () => {
    expect(allTracksMixed([{ index: 0 }, { index: 2 }])).toEqual([
      { index: 0, volume: 1 },
      { index: 2, volume: 1 },
    ]);
    expect(allTracksMixed([])).toEqual([]);
  });
});

describe("canDecodeHevc", () => {
  it("trusts MediaSource first, then canPlayType", () => {
    expect(canDecodeHevc(() => true, () => "")).toBe(true);
    expect(canDecodeHevc(() => false, () => "")).toBe(false);
    expect(canDecodeHevc(undefined, () => "probably")).toBe(true);
    expect(canDecodeHevc(undefined, () => "maybe")).toBe(true);
    expect(canDecodeHevc(undefined, () => "")).toBe(false);
  });

  it("asks about the hvc1 tag in MP4", () => {
    const asked: string[] = [];
    canDecodeHevc((t) => (asked.push(t), false), undefined);
    expect(asked[0]).toMatch(/^video\/mp4; codecs="hvc1\./);
  });

  it("assumes support when the page cannot tell", () => {
    expect(canDecodeHevc(undefined, undefined)).toBe(true);
  });
});

describe("hasNoPicture", () => {
  it("is true when the video track was dropped (no decoder)", () => {
    expect(hasNoPicture({ videoWidth: 0, videoHeight: 0 })).toBe(true);
    expect(hasNoPicture({ videoWidth: 1920, videoHeight: 1080 })).toBe(false);
  });
});
