import type { Settings } from "../api";

/** Clip length choices (seconds) on the Clips & storage page. */
export const LENGTHS: [number, string][] = [
  [15, "15 seconds"],
  [30, "30 seconds"],
  [60, "1 minute (default)"],
  [120, "2 minutes"],
  [180, "3 minutes"],
  [300, "5 minutes"],
  [600, "10 minutes"],
];

export type Quality = "low" | "medium" | "high" | "very-high";

/** Max bitrate (Mbit/s) of each quality at 1080p 60 fps. */
const BASE_MBIT: Record<Quality, number> = { low: 15, medium: 25, high: 40, "very-high": 70 };

export const QUALITIES: [Quality, string][] = [
  ["low", "Low"],
  ["medium", "Medium"],
  ["high", "High (default)"],
  ["very-high", "Very high"],
];

export const RESOLUTIONS = [720, 1080, 1440, 2160];
export const FRAME_RATES = [30, 60];

/**
 * Max bitrate in kbit/s for a quality at a resolution and frame rate: the
 * 1080p60 value scaled sub-linearly with pixels per second (more pixels need
 * more bits, but not proportionally more), clamped to what Settings accepts.
 */
export function bitrateFor(q: Quality, height: number, fps: number): number {
  const scale = Math.pow(((height / 1080) ** 2 * fps) / 60, 0.75);
  const mbit = Math.min(150, Math.max(2, Math.round(BASE_MBIT[q] * scale)));
  return mbit * 1000;
}

/** The quality whose bitrate this is at that resolution and frame rate, or `null` (set by hand). */
export function qualityOf(bitrateKbps: number, height: number, fps: number): Quality | null {
  return QUALITIES.find(([q]) => bitrateFor(q, height, fps) === bitrateKbps)?.[0] ?? null;
}

/**
 * Settings after the resolution or frame rate changed: a bitrate that came
 * from a quality choice follows it to the new resolution / frame rate; one
 * set by hand stays.
 */
export function withFormat(s: Settings, height: number, fps: number): Settings {
  const q = qualityOf(s.bitrate_kbps, s.output_height, s.fps);
  return { ...s, output_height: height, fps, bitrate_kbps: q ? bitrateFor(q, height, fps) : s.bitrate_kbps };
}

/** Settings with a new clip length; the longest-clip limit grows with it when needed. */
export function withLength(s: Settings, seconds: number): Settings {
  return { ...s, save_length_s: seconds, max_save_length_s: Math.max(s.max_save_length_s, seconds) };
}

/**
 * Largest size a saved clip can have: recording is quality-based and stays
 * below the max bitrate, so real clips are usually smaller.
 */
export function clipSizeEstimate(s: Settings): number {
  const seconds = Math.min(s.save_length_s, s.max_save_length_s);
  return Math.round((s.bitrate_kbps * 1000 * seconds) / 8);
}

/** Disk bar shares (0..1) of the clip folder's drive. */
export function diskShares(total: number, free: number, clipsBytes: number) {
  if (total <= 0) return { clips: 0, other: 0, used: 0 };
  const used = Math.min(total, Math.max(0, total - free));
  const clips = Math.min(used, clipsBytes);
  return { clips: clips / total, other: (used - clips) / total, used: used / total };
}
