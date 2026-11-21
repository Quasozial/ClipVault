import { describe, expect, it } from "vitest";
import { acceleratorFromKey } from "./hotkey";

type Mods = Partial<{ ctrlKey: boolean; shiftKey: boolean; altKey: boolean; metaKey: boolean }>;
const ev = (code: string, mods: Mods = {}) => ({
  code,
  ctrlKey: false,
  shiftKey: false,
  altKey: false,
  metaKey: false,
  ...mods,
});

describe("acceleratorFromKey", () => {
  it("builds Ctrl+Shift+F10", () => {
    expect(acceleratorFromKey(ev("F10", { ctrlKey: true, shiftKey: true }))).toBe("Ctrl+Shift+F10");
  });
  it("orders modifiers Ctrl, Alt, Shift, Super", () => {
    expect(acceleratorFromKey(ev("KeyK", { shiftKey: true, metaKey: true, altKey: true, ctrlKey: true }))).toBe(
      "Ctrl+Alt+Shift+Super+K",
    );
  });
  it("maps digits, numpad, arrows and punctuation", () => {
    expect(acceleratorFromKey(ev("Digit3", { altKey: true }))).toBe("Alt+3");
    expect(acceleratorFromKey(ev("Numpad5", { ctrlKey: true }))).toBe("Ctrl+Numpad5");
    expect(acceleratorFromKey(ev("ArrowUp", { ctrlKey: true }))).toBe("Ctrl+Up");
    expect(acceleratorFromKey(ev("Minus", { ctrlKey: true }))).toBe("Ctrl+Minus");
    expect(acceleratorFromKey(ev("Space", { ctrlKey: true }))).toBe("Ctrl+Space");
  });
  it("is incomplete while only modifiers are held", () => {
    expect(acceleratorFromKey(ev("ShiftLeft", { shiftKey: true }))).toBeNull();
    expect(acceleratorFromKey(ev("ControlRight", { ctrlKey: true }))).toBeNull();
  });
  it("allows bare function keys but needs a modifier for anything else", () => {
    expect(acceleratorFromKey(ev("F9"))).toBe("F9");
    expect(acceleratorFromKey(ev("KeyA"))).toBeNull();
  });
  it("ignores keys a global hotkey cannot use", () => {
    expect(acceleratorFromKey(ev("Escape", { ctrlKey: true }))).toBeNull();
    expect(acceleratorFromKey(ev("", { ctrlKey: true }))).toBeNull();
  });
});
