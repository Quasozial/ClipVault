const pad = (n: number) => String(n).padStart(2, "0");

/** `dd.MM.yyyy HH:mm` in local time; "–" when the date cannot be read. */
export function formatTakenAt(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return "–";
  return `${pad(d.getDate())}.${pad(d.getMonth() + 1)}.${d.getFullYear()} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/** `m:ss` (minutes are not wrapped into hours). Negative/NaN read as 0. */
export function formatDuration(seconds: number): string {
  const s = Number.isFinite(seconds) && seconds > 0 ? Math.floor(seconds) : 0;
  return `${Math.floor(s / 60)}:${pad(s % 60)}`;
}

/** Playhead time with tenths, e.g. `1:05.3`. */
export function formatClock(seconds: number): string {
  const s = Number.isFinite(seconds) && seconds > 0 ? seconds : 0;
  const tenths = Math.floor((s * 10) % 10);
  return `${formatDuration(s)}.${tenths}`;
}

export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let v = bytes / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(1)} ${units[i]}`;
}
