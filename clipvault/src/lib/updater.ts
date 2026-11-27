/**
 * App updates through tauri-plugin-updater: checks the `latest.json` on the
 * newest GitHub release (endpoint and signing key in tauri.conf.json), and
 * downloads, verifies and installs it when the user agrees.
 *
 * Checked a little after start and then every few hours; a failed automatic
 * check (offline, GitHub down) stays quiet, a failed manual one is shown.
 */
import { invoke } from "@tauri-apps/api/core";
import { relaunch } from "@tauri-apps/plugin-process";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { useSyncExternalStore } from "react";
import { isMock } from "../api";
import { toast } from "./toast";

export type UpdatePhase =
  | { phase: "idle" }
  | { phase: "checking" }
  | { phase: "available"; update: Update }
  | { phase: "downloading"; update: Update; done: number; total: number | null }
  | { phase: "installing"; update: Update }
  | { phase: "error"; update: Update | null; message: string; captureStopped: boolean };

export interface UpdaterState {
  status: UpdatePhase;
  /** Whether the update dialog is showing. */
  open: boolean;
  /** When the last check finished without finding an update. */
  upToDateAt: Date | null;
}

const FIRST_CHECK_MS = 15_000;
const CHECK_EVERY_MS = 6 * 60 * 60 * 1000;

let state: UpdaterState = { status: { phase: "idle" }, open: false, upToDateAt: null };
/** Versions the user put off with "Later": not shown again by automatic checks this session. */
const postponed = new Set<string>();
const subs = new Set<() => void>();

function set(next: Partial<UpdaterState>) {
  state = { ...state, ...next };
  subs.forEach((s) => s());
}

export function useUpdater(): UpdaterState {
  return useSyncExternalStore(
    (cb) => {
      subs.add(cb);
      return () => subs.delete(cb);
    },
    () => state,
  );
}

const busy = () => ["checking", "downloading", "installing"].includes(state.status.phase);

/** Looks for a newer release. `manual`: the user asked, so always answer. */
export async function checkForUpdates(manual = false): Promise<void> {
  if (isMock || busy()) return;
  set({ status: { phase: "checking" } });
  try {
    const update = await check();
    if (update) {
      const show = manual || !postponed.has(update.version);
      set({ status: { phase: "available", update }, open: show || state.open });
    } else {
      set({ status: { phase: "idle" }, upToDateAt: new Date() });
      if (manual) toast.info("ClipVault is up to date.");
    }
  } catch (e) {
    const message = e instanceof Error ? e.message : String(e);
    console.warn("update check failed", e);
    set({ status: { phase: "idle" } });
    if (manual) toast.error(`Could not check for updates: ${message}`);
  }
}

/** Downloads and verifies the update, stops capture, then runs the installer and restarts. */
export async function installUpdate(): Promise<void> {
  const s = state.status;
  if (s.phase !== "available" && !(s.phase === "error" && s.update && !s.captureStopped)) return;
  const update = s.update!;
  let captureStopped = false;
  try {
    let total: number | null = null;
    let done = 0;
    set({ status: { phase: "downloading", update, done, total } });
    // Download first (capture keeps running), so the gap in recording is only the install itself.
    await update.download((ev) => {
      if (ev.event === "Started") total = ev.data.contentLength ?? null;
      else if (ev.event === "Progress") done += ev.data.chunkLength;
      set({ status: { phase: "downloading", update, done, total } });
    });
    set({ status: { phase: "installing", update } });
    await invoke("prepare_update");
    captureStopped = true;
    // On Windows this starts the installer and exits ClipVault; it restarts it when done.
    await update.install();
    await relaunch();
  } catch (e) {
    const message = e instanceof Error ? e.message : String(e);
    console.error("update failed", e);
    set({ status: { phase: "error", update, message, captureStopped }, open: true });
  }
}

/** "Later": closes the dialog; the update stays offered in Settings. */
export function dismissUpdate() {
  const s = state.status;
  if (s.phase === "downloading" || s.phase === "installing") return;
  if (s.phase === "available") postponed.add(s.update.version);
  set({ open: false });
}

export function openUpdateDialog() {
  set({ open: true });
}

let started = false;

/** Call once at startup: first check shortly after launch, then periodically. */
export function startUpdateChecks() {
  if (started || isMock || import.meta.env.DEV) return;
  started = true;
  setTimeout(() => void checkForUpdates(), FIRST_CHECK_MS);
  setInterval(() => void checkForUpdates(), CHECK_EVERY_MS);
}
