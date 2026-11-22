import type { ExportOptions, ExportTrack, Marker } from "../api";

/** Playback speeds offered, slowest first. */
export const SPEEDS = [0.1, 0.25, 0.5, 1, 1.5, 2] as const;

/** The next speed up (`dir` 1) or down (-1) from `rate`, staying in range. */
export function stepSpeed(rate: number, dir: 1 | -1): number {
  const i = SPEEDS.findIndex((s) => Math.abs(s - rate) < 1e-6);
  if (i < 0) return 1;
  return SPEEDS[Math.min(SPEEDS.length - 1, Math.max(0, i + dir))];
}

export function formatSpeed(rate: number): string {
  return `${rate}×`;
}

/** Time one frame before/after `t`, snapped to the frame grid so repeated steps don't drift. */
export function frameStep(t: number, fps: number, dir: 1 | -1): number {
  const f = fps > 0 ? fps : 60;
  // Frame n covers [n/f, (n+1)/f); landing at its middle avoids boundary rounding.
  const frame = Math.floor(t * f + 1e-6) + dir;
  return Math.max(0, (frame + 0.5) / f);
}

/**
 * Time of the next (`dir` 1) or previous (-1) marker from `t`. Markers within
 * `gap` s of `t` are skipped, so pressing again while playing moves on.
 * `null` when there is none in that direction.
 */
export function markerJump(markers: Marker[], t: number, dir: 1 | -1, gap = 0.35): number | null {
  const times = markers.map((m) => m.t_s).sort((a, b) => a - b);
  if (dir === 1) return times.find((x) => x > t + gap) ?? null;
  for (let i = times.length - 1; i >= 0; i--) if (times[i] < t - gap) return times[i];
  return null;
}

export interface ExportPreset {
  id: string;
  name: string;
  hint: string;
  max_mb: number | null;
  height: number | null;
}

export const EXPORT_PRESETS: ExportPreset[] = [
  { id: "discord", name: "Discord (10 MB)", hint: "Free Discord upload limit. 720p.", max_mb: 10, height: 720 },
  { id: "discord-50", name: "Discord Nitro Basic (50 MB)", hint: "1080p.", max_mb: 50, height: 1080 },
  { id: "discord-500", name: "Discord Nitro (500 MB)", hint: "Full resolution.", max_mb: 500, height: null },
  { id: "hq", name: "High quality", hint: "No size limit, full resolution.", max_mb: null, height: null },
];

/** Tracks to send to `export_clip`: muted (or zero-volume) ones are left out. */
export function exportTracks(tracks: { index: number; key: string }[], volumes: Record<string, number>, muted: Record<string, boolean>): ExportTrack[] {
  return tracks
    .map((t) => ({ index: t.index, volume: muted[t.key] ? 0 : (volumes[t.key] ?? 1) }))
    .filter((t) => t.volume > 0.001);
}

/** Bulk export from the library: every audio track mixed in at full volume. */
export function allTracksMixed(tracks: { index: number }[]): ExportTrack[] {
  return tracks.map((t) => ({ index: t.index, volume: 1 }));
}

export function exportOptions(preset: ExportPreset, tracks: ExportTrack[]): ExportOptions {
  return { label: preset.id, max_mb: preset.max_mb, height: preset.height, tracks };
}
