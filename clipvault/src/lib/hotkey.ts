/** The parts of a KeyboardEvent the accelerator builder needs. */
export interface KeyLike {
  code: string;
  ctrlKey: boolean;
  shiftKey: boolean;
  altKey: boolean;
  metaKey: boolean;
}

const MODIFIER_CODES = /^(Control|Shift|Alt|Meta|OS)(Left|Right)?$/;

/** Named physical keys the global-shortcut parser accepts, by `KeyboardEvent.code`. */
const NAMED: Record<string, string> = {
  Space: "Space",
  Enter: "Enter",
  Tab: "Tab",
  Backspace: "Backspace",
  Delete: "Delete",
  Insert: "Insert",
  Home: "Home",
  End: "End",
  PageUp: "PageUp",
  PageDown: "PageDown",
  ArrowUp: "Up",
  ArrowDown: "Down",
  ArrowLeft: "Left",
  ArrowRight: "Right",
  Minus: "Minus",
  Equal: "Equal",
  BracketLeft: "BracketLeft",
  BracketRight: "BracketRight",
  Backslash: "Backslash",
  Semicolon: "Semicolon",
  Quote: "Quote",
  Comma: "Comma",
  Period: "Period",
  Slash: "Slash",
  Backquote: "Backquote",
  PrintScreen: "PrintScreen",
  ScrollLock: "ScrollLock",
  Pause: "Pause",
  NumpadAdd: "NumpadAdd",
  NumpadSubtract: "NumpadSubtract",
  NumpadMultiply: "NumpadMultiply",
  NumpadDivide: "NumpadDivide",
  NumpadDecimal: "NumpadDecimal",
  NumpadEnter: "NumpadEnter",
};

function keyName(code: string): string | null {
  let m: RegExpMatchArray | null;
  if ((m = code.match(/^Key([A-Z])$/))) return m[1];
  if ((m = code.match(/^Digit(\d)$/))) return m[1];
  if (/^Numpad\d$/.test(code)) return code;
  if (/^F([1-9]|1\d|2[0-4])$/.test(code)) return code;
  return NAMED[code] ?? null;
}

/**
 * Accelerator string for a key press, e.g. `Ctrl+Shift+F10` (format of the
 * Tauri global-shortcut plugin). `null` while only modifiers are held, for keys
 * a global hotkey cannot use, and for non-function keys without a modifier
 * (a bare letter would swallow normal typing in every program).
 */
export function acceleratorFromKey(e: KeyLike): string | null {
  if (!e.code || MODIFIER_CODES.test(e.code)) return null;
  const key = keyName(e.code);
  if (!key) return null;
  const mods: string[] = [];
  if (e.ctrlKey) mods.push("Ctrl");
  if (e.altKey) mods.push("Alt");
  if (e.shiftKey) mods.push("Shift");
  if (e.metaKey) mods.push("Super");
  if (mods.length === 0 && !/^F\d+$/.test(key)) return null;
  return [...mods, key].join("+");
}

/** Modifiers currently held, for the live "Ctrl+Shift+…" preview while recording. */
export function heldModifiers(e: KeyLike): string {
  const mods: string[] = [];
  if (e.ctrlKey) mods.push("Ctrl");
  if (e.altKey) mods.push("Alt");
  if (e.shiftKey) mods.push("Shift");
  if (e.metaKey) mods.push("Super");
  return mods.join("+");
}
