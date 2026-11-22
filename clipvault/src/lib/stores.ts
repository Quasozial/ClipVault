/** Small app-wide stores (health, Moments import, routing) built on useSyncExternalStore. */
import { useEffect, useSyncExternalStore } from "react";
import { api, on, type Health, type ImportProgress, type ImportReport, type MoveState } from "../api";
import { reportError } from "./toast";

function store<T>(initial: T) {
  let value = initial;
  const subs = new Set<() => void>();
  return {
    get: () => value,
    set(next: T) {
      value = next;
      subs.forEach((s) => s());
    },
    subscribe(cb: () => void) {
      subs.add(cb);
      return () => {
        subs.delete(cb);
      };
    },
  };
}

// ----- health -----

const health = store<Health | null>(null);
let healthStarted = false;

/** Recorder health from `get_health`, then kept current by `health-changed`. */
export function useHealth(): Health | null {
  useEffect(() => {
    if (healthStarted) return;
    healthStarted = true;
    on("health-changed", (h) => health.set(h));
    api.getHealth().then(
      (h) => health.set(h),
      (e) => reportError("Could not read recorder status", e),
    );
  }, []);
  return useSyncExternalStore(health.subscribe, health.get);
}

// ----- Moments import (survives page changes) -----

export interface ImportState {
  running: boolean;
  progress: ImportProgress | null;
  report: ImportReport | null;
  error: string | null;
  /** Bumped after every finished import so the library re-fetches. */
  generation: number;
}

const importState = store<ImportState>({ running: false, progress: null, report: null, error: null, generation: 0 });

export function useImport(): ImportState {
  return useSyncExternalStore(importState.subscribe, importState.get);
}

// ----- moving clips to another folder (runs in the backend, survives page changes) -----

const moveState = store<MoveState | null>(null);
let moveStarted = false;

/** The running or last finished clip move, kept current by `move-status`. */
export function useMove(): MoveState | null {
  useEffect(() => {
    if (moveStarted) return;
    moveStarted = true;
    on("move-status", (m) => moveState.set(m));
    api.moveStatus().then(
      (m) => {
        // An event may have come in first; it is newer.
        if (!moveState.get()) moveState.set(m);
      },
      (e) => reportError("Could not read the clip move status", e),
    );
  }, []);
  return useSyncExternalStore(moveState.subscribe, moveState.get);
}

/** Starts moving the library clips in `from` to `to`; progress arrives via `useMove`. */
export async function startMove(from: string, to: string): Promise<void> {
  const before = moveState.get();
  // Shown at once; the backend's `move-status` events replace it.
  moveState.set({ running: true, from, to, progress: null, report: null, error: null });
  try {
    await api.moveClips(from, to);
  } catch (e) {
    moveState.set(before);
    throw e;
  }
}

/** True while a clip move runs. */
export const moveRunning = () => moveState.get()?.running === true;

/** Hides a finished move's summary (the next move shows again). */
export function dismissMove() {
  const m = moveState.get();
  if (m && !m.running) moveState.set(null);
}

/**
 * Runs the Moments import without generating thumbnails (the library grid makes
 * them lazily as cards scroll into view). Resolves when done.
 */
export async function startImport(): Promise<void> {
  const s = importState.get();
  if (s.running) return;
  importState.set({ ...s, running: true, progress: null, report: null, error: null });
  const off = on("import-progress", (p) => importState.set({ ...importState.get(), progress: p }));
  try {
    const report = await api.importMoments(false);
    importState.set({ ...importState.get(), running: false, report, generation: importState.get().generation + 1 });
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    importState.set({ ...importState.get(), running: false, error: msg });
    reportError("Import from Moments failed", e);
  } finally {
    off();
  }
}

// ----- hash routing -----

export type Route = { page: "library" } | { page: "clip"; id: string } | { page: "settings"; section?: string };

export function parseRoute(hash: string): Route {
  const h = hash.replace(/^#\/?/, "");
  const [head, ...rest] = h.split("/");
  if (head === "clip" && rest[0]) return { page: "clip", id: decodeURIComponent(rest.join("/")) };
  if (head === "settings") return { page: "settings", section: rest[0] };
  return { page: "library" };
}

export function href(r: Route): string {
  switch (r.page) {
    case "clip":
      return `#/clip/${encodeURIComponent(r.id)}`;
    case "settings":
      return r.section ? `#/settings/${r.section}` : "#/settings";
    default:
      return "#/";
  }
}

export function navigate(r: Route) {
  window.location.hash = href(r);
}

const subscribeHash = (cb: () => void) => {
  window.addEventListener("hashchange", cb);
  return () => window.removeEventListener("hashchange", cb);
};

export function useRoute(): Route {
  const hash = useSyncExternalStore(subscribeHash, () => window.location.hash);
  return parseRoute(hash);
}
