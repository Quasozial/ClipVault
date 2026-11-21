import { describe, expect, it } from "vitest";
import { formatBytes, formatDuration, formatTakenAt } from "./format";

describe("formatTakenAt", () => {
  it("renders dd.MM.yyyy HH:mm in local time", () => {
    const iso = new Date(2025, 0, 5, 9, 7, 42).toISOString();
    expect(formatTakenAt(iso)).toBe("05.01.2025 09:07");
  });
  it("accepts an RFC3339 string with offset", () => {
    const d = new Date("2025-03-14T21:30:00+01:00");
    const pad = (n: number) => String(n).padStart(2, "0");
    const want = `${pad(d.getDate())}.${pad(d.getMonth() + 1)}.${d.getFullYear()} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
    expect(formatTakenAt("2025-03-14T21:30:00+01:00")).toBe(want);
  });
  it("shows a dash for an unparsable date", () => {
    expect(formatTakenAt("nope")).toBe("–");
  });
});

describe("formatDuration", () => {
  it("renders m:ss", () => {
    expect(formatDuration(0)).toBe("0:00");
    expect(formatDuration(5.9)).toBe("0:05");
    expect(formatDuration(65)).toBe("1:05");
    expect(formatDuration(600)).toBe("10:00");
    expect(formatDuration(3725)).toBe("62:05");
  });
  it("treats negative or NaN as zero", () => {
    expect(formatDuration(-3)).toBe("0:00");
    expect(formatDuration(NaN)).toBe("0:00");
  });
});

describe("formatBytes", () => {
  it("uses MB/GB with one decimal", () => {
    expect(formatBytes(512)).toBe("512 B");
    expect(formatBytes(1536 * 1024)).toBe("1.5 MB");
    expect(formatBytes(2.25 * 1024 ** 3)).toBe("2.3 GB");
  });
});
