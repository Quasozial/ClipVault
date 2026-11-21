/**
 * Tells the backend whether the Settings page is actually visible (it runs the
 * idle mic meter only then). Closing the window only hides it to the tray, so
 * visibility matters, not just whether the page is mounted. Sends only
 * changes; `dispose` sends a final `false` (unless already closed) and stops.
 */
export function settingsOpenSync(send: (open: boolean) => void) {
  let last: boolean | null = null;
  let disposed = false;
  const set = (open: boolean) => {
    if (open === last) return;
    last = open;
    send(open);
  };
  return {
    update(hidden: boolean) {
      if (!disposed) set(!hidden);
    },
    dispose() {
      if (disposed) return;
      disposed = true;
      set(false);
    },
  };
}
