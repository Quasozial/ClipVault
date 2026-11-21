/**
 * Dev-only in-memory stand-in for the Tauri backend (`?mock` in the URL).
 * Loaded through a dynamic import guarded by `import.meta.env.DEV`, so it and
 * its sample video never reach a production build.
 *
 * `?mock=empty` starts with an empty library (shows the first-run import banner).
 */
import type {
  AudioDevice,
  Backend,
  Clip,
  ClipFilter,
  DisplayInfo,
  ExportOptions,
  EventMap,
  Game,
  Health,
  HotkeyError,
  ImportReport,
  Marker,
  RlStatsInfo,
  RunningProcess,
  Settings,
} from "../api";
import sampleUrl from "./sample.mp4?url";
import { matchesFilter } from "../lib/clips";

const TITLES = [
  "Ceiling shot in overtime",
  "Flip reset, finally",
  "Double tap off the backboard",
  "Kickoff goal in 4 seconds",
  "Musty flick vs. Champ 2",
  "Own goal, do not watch",
  "Air dribble bump",
  "Save of the match",
  "Demo chain into open net",
  "Pinch from the corner",
  "Redirect at 0:01",
  "3v3 comeback, 0–3 to 4–3",
  "Wavedash ground pinch",
  "Team pinch",
  "Psycho attempt",
  "Whiff into goal",
  "Rumble spikes goal",
  "Ranked promo match point",
  "Breezi flick",
  "Ball cam off goal",
];
const TAGS = ["ranked", "2v2", "3v3", "freestyle", "fail", "with Jonas", "overtime", "tournament"];

function rng(seed: number) {
  return () => {
    seed = (seed * 1664525 + 1013904223) % 4294967296;
    return seed / 4294967296;
  };
}

function isoAt(d: Date): string {
  // RFC3339 with local offset, like chrono's DateTime<FixedOffset>.
  const off = -d.getTimezoneOffset();
  const sign = off >= 0 ? "+" : "-";
  const pad = (n: number) => String(Math.floor(Math.abs(n))).padStart(2, "0");
  return (
    `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}` +
    `${sign}${pad(off / 60)}:${pad(off % 60)}`
  );
}

function makeClips(n: number): Clip[] {
  const r = rng(7);
  const now = Date.now();
  const out: Clip[] = [];
  for (let i = 0; i < n; i++) {
    const game = i % 9 === 4 ? "Valorant" : i % 13 === 7 ? "Desktop" : "Rocket League";
    const duration = [60, 60, 45, 30, 58.4, 60, 42][i % 7];
    const markers: Marker[] = [{ t_s: duration - 0.2, kind: "saved", label: "" }];
    if (game === "Rocket League") {
      const goals = Math.floor(r() * 3);
      for (let g = 0; g < goals; g++) markers.push({ t_s: 8 + r() * (duration - 14), kind: "goal", label: g === 0 ? "Goal" : "Assist" });
    }
    if (r() > 0.6) markers.push({ t_s: 5 + r() * (duration - 10), kind: "laugh", label: "" });
    if (r() > 0.75) markers.push({ t_s: 3 + r() * (duration - 6), kind: "manual", label: "" });
    const imported = i >= 22;
    if (imported) markers.push({ t_s: duration / 2, kind: "imported", label: "Highlight" });
    markers.sort((a, b) => a.t_s - b.t_s);
    const recorded = new Date(now - i * 1.9 * 86400_000 - r() * 40_000_000);
    const tags = TAGS.filter(() => r() > 0.78);
    out.push({
      id: `mock-${i}`,
      title: `${TITLES[i % TITLES.length]}${i >= TITLES.length ? ` (${Math.floor(i / TITLES.length) + 1})` : ""}`,
      description: i % 3 === 0 ? "Diamond lobby, last game of the night." : "",
      path: `E:\\GameClips\\${game}\\${game.replace(/ /g, "_")}_${String(i).padStart(4, "0")}.mp4`,
      game,
      recorded_at: isoAt(recorded),
      duration_s: duration,
      fps: 60,
      width: 1920,
      height: 1080,
      size: Math.round(duration * 5.1 * 1024 * 1024),
      trim_in: i % 5 === 1 ? 12 : null,
      trim_out: i % 5 === 1 ? 38 : null,
      thumbnail_path: null,
      source: imported ? "moments" : "native",
      source_id: imported ? `uuid-${i}` : null,
      tags,
      markers,
      starred: i % 6 === 2,
      compressed: i % 4 === 1,
    });
  }
  return out;
}

const HUES: Record<string, number> = { "Rocket League": 205, Valorant: 352, Desktop: 220 };

function thumbSvg(clip: Clip | undefined, seed: number): string {
  const r = rng(seed + 11);
  const hue = HUES[clip?.game ?? ""] ?? 200;
  const field = clip?.game === "Rocket League";
  const ballX = 40 + r() * 240;
  const ballY = 50 + r() * 70;
  const svg = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 320 180">
<defs><linearGradient id="g" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="hsl(${hue} 35% ${28 + r() * 12}%)"/><stop offset="1" stop-color="hsl(${hue} 25% 12%)"/></linearGradient></defs>
<rect width="320" height="180" fill="url(#g)"/>
${field ? `<path d="M0 120 L320 104 L320 180 L0 180Z" fill="hsl(${110 + r() * 30} 30% 24%)"/><path d="M0 150 L320 136" stroke="#fff" stroke-opacity=".25" stroke-width="2"/><circle cx="${ballX}" cy="${ballY}" r="${7 + r() * 6}" fill="#d9dde3"/>` : `<rect x="${r() * 200}" y="40" width="${60 + r() * 60}" height="90" fill="hsl(${hue} 20% 30%)"/>`}
</svg>`;
  return `data:image/svg+xml;utf8,${encodeURIComponent(svg)}`;
}

export function createMockBackend(): Backend {
  const empty = new URLSearchParams(window.location.search).get("mock") === "empty";
  let clips = empty ? [] : makeClips(40);
  let nextClip = 100;
  let games: Game[] = [
    { id: 1, name: "Rocket League", exe_names: ["RocketLeague.exe"], path: "C:\\Program Files\\Epic Games\\rocketleague\\Binaries\\Win64\\RocketLeague.exe", enabled: true },
    { id: 2, name: "Valorant", exe_names: ["VALORANT-Win64-Shipping.exe"], path: null, enabled: true },
    { id: 3, name: "Elden Ring", exe_names: ["eldenring.exe"], path: null, enabled: false },
  ];
  let settings: Settings = {
    fps: 60,
    save_length_s: 60,
    max_save_length_s: 120,
    encoder: "Auto",
    bitrate_kbps: 40000,
    output_height: 1080,
    monitor_id: null,
    desktop_capture: false,
    buffer_dir: "C:\\Users\\player\\AppData\\Local\\ClipVault\\buffer",
    capture_dir: "E:\\GameClips",
    mic_device: null,
    output_device: null,
    mic_enabled: true,
    discord_split: true,
    laugh_enabled: true,
    laugh_threshold_dbfs: -20,
    hotkey_save: "Ctrl+Shift+F10",
    hotkey_mark: "Ctrl+Shift+F9",
    hotkey_toggle_desktop: "Ctrl+Shift+F8",
    start_with_windows: false,
    compress_clips: false,
  };
  let rl: RlStatsInfo = {
    status: "disabled",
    ini_path: "C:\\Program Files\\Epic Games\\rocketleague\\TAGame\\Config\\DefaultStatsAPI.ini",
    port: 49123,
    listener_connected: false,
    rl_install: "C:\\Program Files\\Epic Games\\rocketleague",
    note: null,
  };
  let health: Health = { state: "idle", reason: null, target: null };
  let settingsOpen = false;
  const thumbIds = new Map<string, number>();

  const listeners = new Map<string, Set<(p: unknown) => void>>();
  const emit = <E extends keyof EventMap>(event: E, payload: EventMap[E]) =>
    listeners.get(event)?.forEach((cb) => cb(payload));

  const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
  const find = (id: string) => {
    const c = clips.find((c) => c.id === id);
    if (!c) throw `clip ${id} not found`;
    return c;
  };
  const setHealth = (h: Health) => {
    health = h;
    emit("health-changed", h);
  };

  // Health story: idle -> recording RL -> one restart -> recording; a clip is saved meanwhile.
  setTimeout(() => setHealth({ state: "recording", reason: null, target: "Rocket League" }), 2500);
  setTimeout(() => {
    if (empty) return;
    const base = makeClips(1)[0];
    const c: Clip = { ...base, id: `mock-live-${nextClip++}`, title: "Just saved: open-net miss", recorded_at: isoAt(new Date()) };
    clips = [c, ...clips];
    emit("clip-saved", c);
  }, 9000);
  setTimeout(() => setHealth({ state: "failing", reason: "FFmpeg exited (code 1); restarting capture", target: "Rocket League" }), 30000);
  setTimeout(() => setHealth({ state: "recording", reason: null, target: "Rocket League" }), 36000);

  // Mic meter while Settings is open.
  let level = -40;
  setInterval(() => {
    if (!settingsOpen) return;
    level += (Math.random() - 0.5) * 14;
    if (Math.random() > 0.96) level = -8 - Math.random() * 8; // a laugh
    level = Math.max(-70, Math.min(-2, level * 0.9 - 4));
    emit("mic-level", { dbfs: level });
  }, 60);

  const validate = (s: Settings) => {
    if (s.fps !== 30 && s.fps !== 60) throw "fps must be 30 or 60";
    if (s.max_save_length_s > 600) throw "max save length must be at most 600 s";
    if (s.save_length_s < 5 || s.save_length_s > s.max_save_length_s)
      throw `save length must be between 5 and ${s.max_save_length_s} s`;
    if (s.bitrate_kbps < 2000 || s.bitrate_kbps > 150000) throw "bitrate must be between 2000 and 150000 kbps";
    if (!s.capture_dir.trim()) throw "capture folder must not be empty";
  };

  const handlers: Record<string, (a: any) => unknown> = {
    get_health: () => health,
    get_hotkey_errors: (): HotkeyError[] => [],
    set_settings_open: ({ open }) => {
      settingsOpen = open;
      return null;
    },
    moments_db_available: () => true,
    import_moments: async (): Promise<ImportReport> => {
      const total = 120;
      for (let done = 0; done <= total; done += 6) {
        emit("import-progress", { done, total });
        await sleep(120);
      }
      const added = makeClips(30).slice(22).map((c, i) => ({ ...c, id: `mock-imp-${nextClip++}-${i}` }));
      const known = new Set(clips.map((c) => c.source_id));
      const fresh = added.filter((c) => !known.has(c.source_id));
      clips = [...clips, ...fresh];
      return {
        imported: fresh.length,
        skipped_existing: added.length - fresh.length,
        missing_files: ["E:\\GameClips\\Rocket League\\old_2023_01.mp4", "E:\\GameClips\\Rocket League\\old_2023_02.mp4"],
        games_imported: 1,
        settings_changed: false,
        imported_settings: { capture_dir: "E:\\GameClips", fps: 60 },
      };
    },
    list_clips: async ({ filter }: { filter: ClipFilter }) => {
      await sleep(80);
      let v = clips.filter((c) => matchesFilter(c, filter));
      v = v.slice().sort((a, b) => +new Date(b.recorded_at) - +new Date(a.recorded_at));
      if (filter.sort === "oldest") v.reverse();
      return v;
    },
    get_clip: ({ id }) => find(id),
    update_clip_meta: ({ id, title, description, tags }) => {
      if (!String(title).trim()) throw "title must not be empty";
      const c = find(id);
      const seen = new Set<string>();
      const clean = (tags as string[])
        .map((t) => t.trim())
        .filter((t) => t && !seen.has(t.toLowerCase()) && seen.add(t.toLowerCase()));
      Object.assign(c, { title: String(title).trim(), description, tags: clean });
      return { ...c };
    },
    set_trim: ({ id, trimIn, trimOut }) => {
      const c = find(id);
      const a = trimIn ?? 0;
      const b = trimOut ?? c.duration_s;
      if (a < 0 || a >= b || a >= c.duration_s || b > c.duration_s) throw "invalid trim range";
      Object.assign(c, { trim_in: trimIn, trim_out: trimOut });
      return { ...c };
    },
    export_trimmed: async ({ id, accurate }) => {
      const c = find(id);
      if (c.trim_in === null && c.trim_out === null) throw "Set a trim range first";
      await sleep(accurate ? 2500 : 900);
      const tin = c.trim_in ?? 0;
      const tout = c.trim_out ?? c.duration_s;
      const n: Clip = {
        ...c,
        id: `mock-exp-${nextClip++}`,
        title: `${c.title} (trim)`,
        path: c.path.replace(/\.mp4$/, "__trim.mp4"),
        duration_s: tout - tin,
        trim_in: null,
        trim_out: null,
        source: "native",
        markers: c.markers.filter((m) => m.t_s >= tin && m.t_s <= tout).map((m) => ({ ...m, t_s: m.t_s - tin })),
      };
      clips = [n, ...clips];
      return n.path;
    },
    set_starred: ({ id, starred }) => {
      const c = find(id);
      c.starred = Boolean(starred);
      return { ...c };
    },
    export_clip: async ({ id, options }) => {
      const c = find(id);
      const o = options as ExportOptions;
      const dur = (c.trim_out ?? c.duration_s) - (c.trim_in ?? 0);
      if (o.max_mb !== null && dur > (o.max_mb * 8000 * 0.92) / 396) throw `${Math.round(dur)} s is too long for ${o.max_mb} MB. Trim it to about ${Math.floor((o.max_mb * 8000 * 0.92) / 396)} s or less, or pick a bigger preset.`;
      await sleep(1800);
      const name = c.path.split("\\").pop()!.replace(/\.mp4$/, "");
      return `E:\\GameClips\\Exports\\${name}__${o.label}.mp4`;
    },
    reveal_in_folder: () => null,
    delete_clip: ({ id }) => {
      find(id);
      clips = clips.filter((c) => c.id !== id);
      return null;
    },
    audio_tracks: async ({ id }) => {
      await sleep(300);
      const c = find(id);
      const n = Math.round(c.duration_s * 50);
      const wave = (seed: number, speech: boolean) =>
        Array.from({ length: n }, (_, i) => {
          const t = i / 50;
          const env = speech ? (Math.sin(t * 0.9 + seed) > 0.55 ? 0.6 : 0.02) : 0.35 + 0.25 * Math.sin(t * 0.37 + seed);
          return Math.min(1, Math.abs(env * (0.6 + 0.4 * Math.sin(i * 1.7 + seed * 3) * Math.cos(i * 0.31))));
        });
      return [
        { index: 0, title: "Game", label: "Desktop audio", kind: "game", path: c.path, peaks: wave(1, false), peaks_per_sec: 50 },
        { index: 1, title: "Mic", label: "Microphone", kind: "mic", path: c.path, peaks: wave(2, true), peaks_per_sec: 50 },
      ];
    },
    ensure_thumbnail: async ({ id }) => {
      await sleep(120 + Math.random() * 400);
      const idx = clips.findIndex((c) => c.id === id);
      if (idx < 0 || /-(7|19)$/.test(id)) return null; // a couple of failed thumbnails
      if (!thumbIds.has(id)) thumbIds.set(id, thumbIds.size + 1);
      return `mock-thumb:${id}`;
    },
    list_games: () => games.map((g) => ({ ...g })),
    set_game_enabled: ({ id, enabled }) => {
      games = games.map((g) => (g.id === id ? { ...g, enabled } : g));
      return null;
    },
    list_running_processes: async (): Promise<RunningProcess[]> => {
      await sleep(300);
      return [
        { pid: 4120, name: "Discord", exe: "Discord.exe", path: "C:\\Users\\player\\AppData\\Local\\Discord\\app-1.0\\Discord.exe" },
        { pid: 8812, name: "Firefox", exe: "firefox.exe", path: "C:\\Program Files\\Mozilla Firefox\\firefox.exe" },
        { pid: 9920, name: "RocketLeague", exe: "RocketLeague.exe", path: games[0].path },
        { pid: 10432, name: "Spotify", exe: "Spotify.exe", path: null },
      ];
    },
    add_game_from_process: ({ pid }) => {
      const names: Record<number, string> = { 4120: "Discord", 8812: "Firefox", 9920: "RocketLeague", 10432: "Spotify" };
      const g: Game = { id: games.length + 10, name: names[pid] ?? "Game", exe_names: [`${names[pid]}.exe`], path: null, enabled: true };
      games = [...games, g];
      return g;
    },
    get_settings: () => ({ ...settings }),
    save_settings: ({ settings: s }: { settings: Settings }): HotkeyError[] => {
      validate(s);
      settings = { ...s };
      queueMicrotask(() => emit("settings-changed", null));
      const errs: HotkeyError[] = [];
      const check = (action: HotkeyError["action"], acc: string) => {
        if (/PrintScreen|F12/.test(acc)) errs.push({ action, accelerator: acc, reason: "already registered by another program" });
      };
      check("SaveClip", s.hotkey_save);
      check("Mark", s.hotkey_mark);
      check("ToggleDesktop", s.hotkey_toggle_desktop);
      return errs;
    },
    list_encoders: () => ({
      available: [
        { id: "Av1Nvenc", label: "NVIDIA AV1", hardware: true },
        { id: "HevcNvenc", label: "NVIDIA HEVC", hardware: true },
        { id: "H264Nvenc", label: "NVIDIA H.264", hardware: true },
        { id: "X264", label: "CPU H.264 (x264)", hardware: false },
      ],
      auto: { id: "Av1Nvenc", label: "NVIDIA AV1", hardware: true },
    }),
    compress_clip: ({ id }) => {
      setTimeout(() => {
        const c = clips.find((x) => x.id === id);
        if (!c) return;
        c.size = Math.round(c.size / 3);
        c.compressed = true;
        emit("clip-updated", c);
      }, 1500);
      return null;
    },
    list_displays: (): DisplayInfo[] => [
      { id: "\\\\.\\DISPLAY1", output_idx: 0, width: 2560, height: 1440, refresh_hz: 165, primary: true, hdr: true },
      { id: "\\\\.\\DISPLAY2", output_idx: 1, width: 1920, height: 1080, refresh_hz: 60, primary: false, hdr: false },
    ],
    list_audio_devices: (): AudioDevice[] => [
      { id: "out-1", name: "Speakers (Realtek Audio)", is_input: false, is_default: true },
      { id: "out-2", name: "Headset (Arctis Nova 7)", is_input: false, is_default: false },
      { id: "in-1", name: "Microphone (Arctis Nova 7)", is_input: true, is_default: true },
      { id: "in-2", name: "Microphone (USB Audio)", is_input: true, is_default: false },
    ],
    rl_stats_status: async () => {
      await sleep(200);
      return { ...rl, note: null };
    },
    rl_enable_stats: async () => {
      await sleep(400);
      rl = { ...rl, status: "enabled" };
      return { ...rl, note: "Restart Rocket League for the change to take effect." };
    },
  };

  return {
    async invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
      const h = handlers[cmd];
      if (!h) throw `mock: unknown command ${cmd}`;
      await sleep(10);
      return (await h(args ?? {})) as T;
    },
    async listen(event, cb) {
      let set = listeners.get(event);
      if (!set) listeners.set(event, (set = new Set()));
      const f = cb as (p: unknown) => void;
      set.add(f);
      return () => set!.delete(f);
    },
    fileSrc(path: string) {
      if (path.startsWith("mock-thumb:")) {
        const id = path.slice("mock-thumb:".length);
        return thumbSvg(
          clips.find((c) => c.id === id),
          thumbIds.get(id) ?? 1,
        );
      }
      return sampleUrl;
    },
  };
}
