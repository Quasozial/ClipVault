import { describe, expect, it } from "vitest";
import { rebaseSettings } from "./settingsMerge";

type S = { fps: number; desktop_capture: boolean; capture_dir: string; hotkey_save: string; monitor_id: string | null };

const base: S = { fps: 60, desktop_capture: false, capture_dir: "E:/GameClips", hotkey_save: "Ctrl+Shift+F10", monitor_id: null };

describe("rebaseSettings", () => {
  it("keeps only the fields the user changed and takes everything else from the backend", () => {
    const draft = { ...base, fps: 30 };
    // Meanwhile: tray toggled desktop capture, an import set the clip folder.
    const latest = { ...base, desktop_capture: true, capture_dir: "D:/Moments" };
    expect(rebaseSettings(draft, base, latest)).toEqual({ ...latest, fps: 30 });
  });

  it("an untouched draft becomes the backend's settings", () => {
    const latest = { ...base, hotkey_save: "F10", monitor_id: "\\.\DISPLAY2" };
    expect(rebaseSettings({ ...base }, base, latest)).toEqual(latest);
  });

  it("the user's edit wins over a backend change of the same field", () => {
    const draft = { ...base, desktop_capture: true };
    const latest = { ...base, desktop_capture: false, fps: 30 };
    expect(rebaseSettings(draft, base, latest)).toEqual({ ...latest, desktop_capture: true });
  });

  it("compares null and changed-back values by value", () => {
    const draft = { ...base, monitor_id: null, fps: 60 };
    const latest = { ...base, monitor_id: "X", fps: 30 };
    expect(rebaseSettings(draft, base, latest)).toEqual(latest);
  });
});
