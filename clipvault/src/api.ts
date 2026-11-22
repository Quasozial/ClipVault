/**
 * Typed wrappers for every ClipVault Tauri command and event.
 *
 * Commands reject with an `Error` whose message is the backend's
 * human-readable string; callers surface it (toast or inline) — see
 * `lib/toast.ts`. In dev, `?mock` in the URL (or `VITE_MOCK=1`) swaps the
 * Tauri IPC for in-memory fixtures (`mock/backend.ts`) so the UI can be
 * previewed in a plain browser; the mock is never part of a production build.
 */
import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { reportError } from "./lib/toast";

// ----- payload types (snake_case, as serialized by Rust) -----

export type MarkerKind = "saved" | "goal" | "laugh" | "manual" | "imported";

export interface Marker {
  t_s: number;
  kind: MarkerKind;
  label: string;
}

export interface Clip {
  id: string;
  title: string;
  description: string;
  path: string;
  game: string;
  /** RFC3339 with offset. */
  recorded_at: string;
  duration_s: number;
  fps: number;
  width: number;
  height: number;
  size: number;
  trim_in: number | null;
  trim_out: number | null;
  thumbnail_path: string | null;
  source: "native" | "moments" | string;
  source_id: string | null;
  tags: string[];
  markers: Marker[];
  starred: boolean;
  /** Re-encoded to AV1 by background compression (or tried: it was not smaller). */
  compressed: boolean;
  /** Size before compression replaced the file; null if it never did (or did before this was recorded). */
  original_size: number | null;
}

export interface AudioTrack {
  /** Audio stream index in the file. */
  index: number;
  /** Title stored in the file (`Game`, `Mic`, ...). */
  title: string;
  /** Display name (`Desktop audio`, `Microphone`, ...). */
  label: string;
  kind: "game" | "mic" | "voice" | "other";
  /** This track alone, as an .m4a. */
  path: string;
  /** Peak level 0..1 per 1/peaks_per_sec s. */
  peaks: number[];
  peaks_per_sec: number;
}

export interface ExportTrack {
  /** `AudioTrack.index`. */
  index: number;
  /** 0..2, 1 = unchanged. */
  volume: number;
}

export interface ExportOptions {
  /** File name suffix. */
  label: string;
  /** Size cap in MB (decimal); null = quality-based. */
  max_mb: number | null;
  /** Output height; null = source. */
  height: number | null;
  /** Tracks to mix into the one output track; muted ones left out. */
  tracks: ExportTrack[];
}

export type SortOrder = "newest" | "oldest";

export interface ClipFilter {
  game?: string;
  /** Words that must all match title, description, tags, game, date (2026-11-20) or a marker. */
  search?: string;
  starred?: boolean;
  sort?: SortOrder;
}

export interface Game {
  id: number;
  name: string;
  exe_names: string[];
  path: string | null;
  enabled: boolean;
}

export interface RunningProcess {
  pid: number;
  name: string;
  exe: string;
  path: string | null;
}

export type Encoder =
  | "Auto"
  | "H264Nvenc"
  | "HevcNvenc"
  | "Av1Nvenc"
  | "H264Amf"
  | "HevcAmf"
  | "Av1Amf"
  | "H264Qsv"
  | "HevcQsv"
  | "Av1Qsv"
  | "X264";

export interface EncoderOption {
  id: Encoder;
  label: string;
  hardware: boolean;
}

/** Encoders this PC can run (best first; empty = not known yet) and what "Automatic" picks. */
export interface EncoderList {
  available: EncoderOption[];
  auto: EncoderOption;
}

export interface Settings {
  fps: number;
  save_length_s: number;
  max_save_length_s: number;
  encoder: Encoder;
  bitrate_kbps: number;
  output_height: number;
  monitor_id: string | null;
  desktop_capture: boolean;
  buffer_dir: string;
  capture_dir: string;
  mic_device: string | null;
  output_device: string | null;
  mic_enabled: boolean;
  discord_split: boolean;
  laugh_enabled: boolean;
  laugh_threshold_dbfs: number;
  hotkey_save: string;
  hotkey_mark: string;
  hotkey_toggle_desktop: string;
  start_with_windows: boolean;
  /** Re-encode saved clips to AV1 in the background. */
  compress_clips: boolean;
}

export type HotkeyAction = "SaveClip" | "Mark" | "ToggleDesktop";

export interface HotkeyError {
  action: HotkeyAction;
  accelerator: string;
  reason: string;
}

export interface DisplayInfo {
  id: string;
  output_idx: number;
  width: number;
  height: number;
  refresh_hz: number;
  primary: boolean;
  hdr: boolean;
}

export interface AudioDevice {
  id: string;
  name: string;
  is_input: boolean;
  is_default: boolean;
}

export interface RlStatsInfo {
  status: "enabled" | "disabled" | "not_found";
  ini_path: string | null;
  port: number;
  listener_connected: boolean;
  rl_install: string | null;
  note: string | null;
}

export interface Health {
  state: "idle" | "recording" | "restarting" | "failing";
  reason: string | null;
  target: string | null;
}

export interface ImportReport {
  imported: number;
  skipped_existing: number;
  missing_files: string[];
  games_imported: number;
  /** Known to be unreliable: re-fetch settings after an import instead. */
  settings_changed: boolean;
  imported_settings: { capture_dir: string | null; fps: number | null };
}

export interface ImportProgress {
  done: number;
  total: number;
}

/** Where a clip is in background compression. */
export type CompressStage = "queued" | "encoding" | "swapping";

export interface EventMap {
  "health-changed": Health;
  "clip-saved": Clip;
  /** A clip changed in the background (compression finished). */
  "clip-updated": Clip;
  /** A clip's compression stage changed (`null`: finished or given up). */
  "compress-status": { id: string; stage: CompressStage | null };
  "mic-level": { dbfs: number };
  "import-progress": ImportProgress;
  /** Main window hidden to the tray (close button) or shown again from the tray. */
  "window-visibility": { visible: boolean };
  /** Stored settings changed (any source: this page, tray / hotkey desktop toggle, Moments import). */
  "settings-changed": null;
}

// ----- backend seam (real Tauri IPC or dev mock) -----

export interface Backend {
  invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T>;
  listen<E extends keyof EventMap>(event: E, cb: (payload: EventMap[E]) => void): Promise<() => void>;
  fileSrc(path: string): string;
}

const tauriBackend: Backend = {
  invoke: (cmd, args) => invoke(cmd, args),
  listen: (event, cb) => listen(event, (e) => cb(e.payload as never)),
  fileSrc: (path) => convertFileSrc(path),
};

let backend: Backend = tauriBackend;
export let isMock = false;

/** Call once before rendering: picks the dev mock when asked for. */
export async function initApi(): Promise<void> {
  if (import.meta.env.DEV) {
    const wanted = import.meta.env.VITE_MOCK === "1" || new URLSearchParams(window.location.search).has("mock");
    if (wanted) {
      const { createMockBackend } = await import("./mock/backend");
      backend = createMockBackend();
      isMock = true;
    }
  }
}

/** Normalises whatever the IPC rejects with into an Error with a readable message. */
function asError(e: unknown): Error {
  if (e instanceof Error) return e;
  if (typeof e === "string") return new Error(e);
  try {
    return new Error(JSON.stringify(e));
  } catch {
    return new Error(String(e));
  }
}

async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  try {
    return await backend.invoke<T>(cmd, args);
  } catch (e) {
    throw asError(e);
  }
}

export const fileSrc = (path: string) => backend.fileSrc(path);

// ----- commands -----

export const api = {
  // health & misc
  getHealth: () => call<Health>("get_health"),
  getHotkeyErrors: () => call<HotkeyError[]>("get_hotkey_errors"),
  setSettingsOpen: (open: boolean) => call<null>("set_settings_open", { open }),
  momentsDbAvailable: () => call<boolean>("moments_db_available"),
  importMoments: (generateThumbnails: boolean) => call<ImportReport>("import_moments", { generateThumbnails }),

  // clips
  listClips: (filter: ClipFilter) => call<Clip[]>("list_clips", { filter }),
  getClip: (id: string) => call<Clip>("get_clip", { id }),
  updateClipMeta: (id: string, title: string, description: string, tags: string[]) =>
    call<Clip>("update_clip_meta", { id, title, description, tags }),
  setTrim: (id: string, trimIn: number | null, trimOut: number | null) =>
    call<Clip>("set_trim", { id, trimIn, trimOut }),
  exportTrimmed: (id: string, accurate: boolean) => call<string>("export_trimmed", { id, accurate }),
  deleteClip: (id: string) => call<null>("delete_clip", { id }),
  setStarred: (id: string, starred: boolean) => call<Clip>("set_starred", { id, starred }),
  /** Re-encodes for sharing into `<capture dir>/Exports`; returns the new file's path. */
  exportClip: (id: string, options: ExportOptions) => call<string>("export_clip", { id, options }),
  revealInFolder: (path: string) => call<null>("reveal_in_folder", { path }),
  ensureThumbnail: (id: string) => call<string | null>("ensure_thumbnail", { id }),
  /** Hover-scrub strip: 8 frames side by side in one JPEG; `null` when it cannot be made. */
  ensureScrubStrip: (id: string) => call<string | null>("ensure_scrub_strip", { id }),
  audioTracks: (id: string) => call<AudioTrack[]>("audio_tracks", { id }),
  /** Queues background AV1 compression; `clip-updated` follows when done. */
  compressClip: (id: string) => call<null>("compress_clip", { id }),
  /** `null`: the clip is not waiting for or in compression. */
  compressStatus: (id: string) => call<CompressStage | null>("compress_status", { id }),

  // games
  listGames: () => call<Game[]>("list_games"),
  setGameEnabled: (id: number, enabled: boolean) => call<null>("set_game_enabled", { id, enabled }),
  listRunningProcesses: () => call<RunningProcess[]>("list_running_processes"),
  addGameFromProcess: (pid: number) => call<Game>("add_game_from_process", { pid }),

  // settings & devices
  getSettings: () => call<Settings>("get_settings"),
  saveSettings: (settings: Settings) => call<HotkeyError[]>("save_settings", { settings }),
  listDisplays: () => call<DisplayInfo[]>("list_displays"),
  listEncoders: () => call<EncoderList>("list_encoders"),
  listAudioDevices: () => call<AudioDevice[]>("list_audio_devices"),

  // Rocket League
  rlStatsStatus: () => call<RlStatsInfo>("rl_stats_status"),
  rlEnableStats: () => call<RlStatsInfo>("rl_enable_stats"),
};

// ----- events -----

/** Subscribes to a backend event; returns the unsubscribe function. */
export function on<E extends keyof EventMap>(event: E, cb: (payload: EventMap[E]) => void): () => void {
  let unlisten: (() => void) | null = null;
  let cancelled = false;
  backend
    .listen(event, cb)
    .then((u) => {
      if (cancelled) u();
      else unlisten = u;
    })
    .catch((e) => reportError(`Could not subscribe to ${event}`, e));
  return () => {
    cancelled = true;
    unlisten?.();
  };
}
