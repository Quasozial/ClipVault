/**
 * Settings the page should show or send, given that the backend's settings
 * may have changed since the page loaded them (tray / hotkey desktop toggle,
 * Moments import merge): every field the user edited (`draft` differs from
 * `base`, the backend values the draft started from) keeps the user's value,
 * every other field takes the backend's current value (`latest`).
 */
export function rebaseSettings<T extends object>(draft: T, base: T, latest: T): T {
  const out = { ...latest };
  for (const k of Object.keys(draft) as (keyof T)[]) {
    if (JSON.stringify(draft[k]) !== JSON.stringify(base[k])) out[k] = draft[k];
  }
  return out;
}
