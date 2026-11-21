# ClipVault — Design

Date: 2026-11-20
Status: draft, awaiting approval
Working name "ClipVault" is a placeholder (my call); rename freely.

## 1. Intent

A Windows desktop app that replaces SteelSeries GG Moments for game clipping.

**Why:** Moments' clipping engine is unreliable — it freezes and then silently fails to save clips. The new app's core value is a clipping engine that *always* saves when the hotkey is pressed, and says loudly when it can't. Feature parity with Moments (library, markers, metadata, per-game capture) comes second.

**User:** one person, gamer, mostly Rocket League, 1080p60, RTX 5090, existing library of 630 Moments clips (~139 GB) in `E:\GameClips`.

**Success:** user uninstalls/disables Moments and never loses a clip; existing clips and their metadata show up in the new library.

## 2. Goals

1. **Rolling replay buffer**, saved by a configurable global hotkey. Save length configurable (30 s, 60 s, 120 s, custom).
2. **30 or 60 fps** capture, hardware encoded.
3. **Per-game capture**: buffer runs only while an enabled game process is running.
4. **Desktop capture** toggle: when on, buffer also runs with no game; monitor selectable.
5. **Separate audio tracks**: everything audible is recorded, split into three tracks — (1) microphone, (2) Discord, (3) game + all other system sound.
6. **Markers** on the clip timeline:
   - "saved" — the moment the save hotkey was pressed (Moments shows this too)
   - Rocket League goals (Stats API)
   - Big laugh / loud mic spike (adjustable threshold)
   - Manual mark hotkey
7. **Library**: clips grouped/filterable by game, showing date+time taken, duration, thumbnail; player with markers on timeline.
8. **Metadata editing**: title, description, tags. Trim. Delete.
9. **One-time Moments import**: clips, metadata, markers, games list.
10. **Reliability visible**: save confirmation (sound + toast), engine health indicator, logs.

## 3. Non-goals (for now)

- Auto-clipping (saving clips automatically on events). Markers only.
- Upload / sharing / cloud.
- Video effects, overlays, cropping, montage editing.
- Full-session recording (only replay buffer).
- macOS/Linux.

## 4. Anti-goals (failure even if it "works")

- A hotkey press that produces no clip **and no error**. Silent failure is the bug being replaced.
- Noticeable in-game performance loss (target: < 3 % FPS drop in Rocket League at 1080p).
- Writing to, moving, or deleting anything owned by SteelSeries GG (its DB or its clip files) during import.
- Unbounded disk use by the buffer.

## 5. Constraints and facts found

- Displays: three 2560×1440 monitors at 164 Hz (`\\.\DISPLAY8` primary, DISPLAY6 left, DISPLAY7 right). Moments recorded at 1920×1080. Windows HDR state unknown to the user ⇒ must be detected.
- SteelSeries Moments will be **turned off** when ClipVault is adopted: no hotkey sharing, no parallel recording, import is effectively one-time.
- Windows 10/11 only. NVIDIA RTX 5090 present (NVENC available); AMD AMF / Intel QSV should work via encoder selection but are not primary.
- Tooling present: Rust/cargo 1.99, Node 26, FFmpeg build (N-127043) available locally; app ships its own FFmpeg as a Tauri sidecar.
- Moments DB: `C:\ProgramData\SteelSeries\GG\apps\moments\db\database.db` (SQLite). Relevant tables:
  - `moments_clips` (630 rows): `id` UUID, `name`, `description`, `path`, `trimmed_clip_path`, `clip_start_point`, `clip_end_point`, `full_length`, `last_game_name`, `library_game_unique_id`, `recording_timestamp` (ISO 8601 with offset), `framerate`, `resolution_width/height`, `filesize`, `thumbnail_path`, `gamesense_events` (JSON array; may be the string `null`), `tags` (JSON), `is_deleted`, `trigger_type`.
  - `gamesense_events` entries look like `{"localized_name_key":"moments.timelineEvents.savedClip","clip_timestamp":34.24,...}`.
  - `moments_games` (116 rows): `name`, `path`, `launcher_executables` (JSON), `library_game_unique_id`, `capture_enabled`, `recording_mode`.
  - `moments_settings`: `captureDir=E:\GameClips`, `videoFps=60`, `selectedMonitor=\\.\DISPLAY1`, audio device IDs.
- Rocket League Stats API: local TCP socket on `127.0.0.1:49123`, enabled by setting `PacketSendRate` > 0 in `<RL install>\TAGame\Config\DefaultStatsAPI.ini`. Emits JSON events including goal scored.

## 6. Approaches considered

| | Approach | Trade-off |
|---|---|---|
| A | Fully native Rust (Windows Graphics Capture + Media Foundation encoder, encoded-packet ring in RAM) | Most control, lowest overhead; most work; RAM buffer dies with a hung process — same failure mode as Moments. |
| **B** | **Tauri app supervising an FFmpeg process that writes 2 s segments to a rolling disk buffer** | Robust: segments survive an encoder hang; watchdog restarts FFmpeg; save = stream-copy concat (< 1 s). Costs: ~100 MB FFmpeg sidecar; clip start snaps to segment boundary (≤ 2 s extra). |
| C | Tauri app over OBS replay buffer via obs-websocket | Fastest to build, proven capture; requires OBS installed and running, awkward per-game logic. |

**Chosen: B** (user's choice). Reason: the disk-segment design directly targets the freeze-and-lose-clip failure.

## 7. Design

### 7.1 Stack (my calls)

- Tauri 2, Rust backend, React + TypeScript + Vite frontend.
- SQLite (rusqlite) for the library, in the app's data dir.
- FFmpeg bundled as a Tauri sidecar (GPL build with `ddagrab`, `h264_nvenc`, `hevc_nvenc`, `hevc_amf`, `h264_qsv`).
- App lives in the system tray, starts with Windows (toggle), main window opens the library.

### 7.2 Architecture

```
            ┌──────────── Tauri (Rust core) ─────────────┐
 hotkeys ──►│ HotkeyService ─► SaveService ─► Library DB  │◄── UI (React)
            │      │               ▲                     │
 processes ►│ GameWatcher ─► CaptureSupervisor ─► ffmpeg ─┼─► buffer dir (2 s segments)
            │                    │  watchdog             │
 WASAPI  ──►│ AudioCapture ──────┘ (PCM via named pipes) │
 RL :49123 ►│ EventBus ◄─ RlStatsListener, LaughDetector, │
            │             manual mark, save press        │
            └────────────────────────────────────────────┘
```

### 7.3 Capture pipeline

- **Video**: FFmpeg `ddagrab` on the selected monitor (output_idx from monitor selection), framerate 30/60, GPU encode. Keyframe every segment boundary (GOP = fps × 2, forced keyframes) so every segment is independently decodable.
- **Resolution** (user decision): clips are saved at **1920×1080**; the 1440p desktop is scaled down on the GPU (`scale_d3d11` or equivalent, no CPU round-trip). Output resolution is a setting with 1080p default (native as an option is cheap to add, my call).
- **HDR**: detect Windows advanced-color state of the captured monitor at capture start (DXGI output desc / `DisplayConfigGetDeviceInfo`). If HDR is on, tone-map to SDR BT.709 on the GPU before encoding so clips don't look washed out. If HDR state changes mid-session, restart the pipeline.
- **Audio** (user decision: record everything, split into tracks): Rust captures three sources and writes raw PCM into three named pipes that FFmpeg reads as inputs; FFmpeg muxes video + 3 AAC tracks into the same segments, so A/V sync lives in one muxer.
  1. **Mic** — chosen input device (WASAPI capture). Can be muted in settings.
  2. **Discord** — Windows process-loopback capture (`ActivateAudioInterfaceAsync` with `AUDIOCLIENT_PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE`) on the Discord process tree. When Discord isn't running, the track carries silence; reattach when it starts.
  3. **Game + system** — process loopback with `..._EXCLUDE_TARGET_PROCESS_TREE` on Discord (i.e. everything except Discord). If Discord isn't running, plain loopback of the output device.
  - Requires Windows 10 2004+ (process loopback). Fallback if unavailable: 2 tracks (system loopback incl. Discord + mic), with a warning in settings.
  - Track names in the MP4: "Mic", "Discord", "Game/System". Requirement: A/V drift < 1 frame over a 2-hour session; sync mechanism is the builder's call (wall-clock timestamps, aresample async, etc.).
- **Segments**: FFmpeg `segment` muxer, 2 s, MPEG-TS (survives abrupt process death; each file self-contained), into a buffer dir (configurable, default `%LOCALAPPDATA%\ClipVault\buffer`). Segment filenames carry a monotonic index; Rust records each segment's wall-clock start time when it appears.
- **Retention**: keep segments covering `max_save_length + 10 s`; delete older. Buffer dir is wiped on startup.
- **Defaults** (my call): 1080p native, 60 fps, H.264 NVENC, CBR 40 Mbps (≈ 300 MB/min); HEVC selectable. Bitrate and encoder in settings.

### 7.4 Watchdog / reliability

- Supervisor considers FFmpeg unhealthy if: process exited, or no new segment for > 5 s while capture should be running.
- Unhealthy ⇒ kill, restart with same args; existing segments are kept. Restarts are logged with reason; > 3 restarts in 1 min ⇒ tray icon turns red + toast "Capture failing: <reason>".
- Tray icon states: idle (no game), recording (buffer live), error.
- Save path is independent of FFmpeg health: it only reads finished segments on disk.
- Logs: rolling log files in app data dir, plus FFmpeg stderr captured per run.

### 7.5 Saving a clip

1. Hotkey press ⇒ record `t_press` (wall clock) immediately, play confirmation sound only after success (step 6).
2. Wait for the segment currently being written to close (≤ 2 s) so the press moment is included.
3. Select segments covering `[t_press − save_length, t_press]`; hardlink (or copy) them into a temp dir so retention can't delete them mid-save.
4. FFmpeg concat demuxer, `-c copy`, `+faststart`, write to `<capture folder>\<Game>__YYYY-MM-DD__HH-MM-SS.mp4.tmp`, then rename to `.mp4` (same naming as Moments).
5. Generate thumbnail; insert library row; convert EventBus events in the window to clip-relative markers, plus a "saved" marker at `t_press`.
6. Success ⇒ sound + toast "Clip saved (58 s, Rocket League)". Failure at any step ⇒ error sound + toast with reason; logged. Never silent.
- Saves are queued; pressing twice rapidly yields two clips.
- If no buffer is running (no game, desktop off) ⇒ toast "Not recording".

### 7.6 Game detection

- Games table: name, exe path/name(s), enabled flag. Seeded from Moments import; user can add a game by picking a running process or an exe.
- Poll processes every 2 s (my call); when an enabled game exe is running ⇒ buffer on (capture its monitor — the selected monitor in settings). Game exits ⇒ buffer off after 10 s grace.
- Desktop toggle on ⇒ buffer runs whenever no game is running too (clips tagged game "Desktop").
- Active game name is attached to clips and used for folder naming.

### 7.7 Markers / EventBus

All event sources push `{wall_clock_time, type, label}` into an in-memory ring covering the buffer window.

- **RL goal**: RlStatsListener connects to `127.0.0.1:49123` while Rocket League runs, reconnects on drop, emits on goal events (label: scorer/team if available). Settings page shows Stats API status and offers to enable it in `DefaultStatsAPI.ini` (with backup of the file, user-confirmed).
- **Big laugh**: mic RMS over 100 ms windows; threshold (dB, slider) exceeded for ≥ 300 ms ⇒ marker; 5 s cooldown. Off by default? — **my call: on**, threshold tuned by user via a live level meter in settings.
- **Manual mark**: separate global hotkey.
- **Saved**: implicit at save press.

### 7.8 Hotkeys

- Global shortcuts (tauri global-shortcut / RegisterHotKey): Save clip, Manual mark, Toggle desktop capture. All rebindable in settings with a "press keys" recorder; conflict detection.
- Mouse-button hotkeys: nice-to-have, builder's call.
- Moments is turned off, so exclusive `RegisterHotKey` is fine; if registration fails (key held by another app), show it in settings and in a toast — never silently.

### 7.9 Library UI

- Grid of clip cards: thumbnail, title, game, date + time taken, duration, marker icons. Filter by game, search title/tags, sort by date.
- Clip view: video player (HTML5 via Tauri asset protocol), timeline with markers (click to seek), title / description / tags editable inline, trim handles, delete.
- **Trim** (my call): non-destructive (stores in/out points, player respects them); "Export trimmed" writes a new file (stream copy, or re-encode for frame accuracy — toggle).
- **Delete**: moves file to Recycle Bin, removes row.
- Player: per-track mute toggles (Mic / Discord / Game) — nice-to-have; "Export with selected tracks" mixes the chosen tracks into one stereo track for sharing (my call, nice-to-have).

### 7.10 Moments import

- Triggered from settings ("Import from SteelSeries Moments"), and offered on first run. Re-runnable and idempotent keyed on Moments clip UUID: a re-run **only adds clips not yet imported** and never overwrites ClipVault-side edits (my call).
- After import, imported clips are ordinary ClipVault clips: edit, trim and delete (to Recycle Bin) act on the files in `E:\GameClips` (Moments is turned off, so there is no conflict). GG's DB itself is never written.
- Opens GG DB **read-only** (copy to temp first to avoid locks).
- Imports clips where `is_deleted = 0` and file exists: title=`name`, description, path (file stays in place), game=`last_game_name`, recorded_at=`recording_timestamp`, duration, fps, resolution, size, trim from `clip_start_point/clip_end_point`, `trimmed_clip_path` if set, tags, markers from `gamesense_events` (`savedClip` ⇒ "saved"; others ⇒ their localized key / display name). Missing files are reported, not imported.
- Imports `moments_games` (name, path, launcher exes, enabled) into games table.
- Imports settings as defaults: capture dir, fps, monitor.
- Thumbnails regenerated by ClipVault (don't depend on GG's thumbnail cache).

### 7.11 Settings

Capture: fps, save length, encoder, bitrate, monitor, desktop toggle, buffer dir, capture folder.
Audio: output device, mic device, mic on/off, Discord split on/off, laugh threshold + meter.
Video: output resolution (default 1080p), HDR status display.
Hotkeys. Games list. Rocket League Stats API status. Start with Windows. Import.

### 7.12 Data model (sketch)

- `clips(id, title, description, path, game_id, recorded_at, duration, fps, width, height, size, trim_in, trim_out, thumbnail_path, source['native'|'moments'], source_id, tags_json, created_at, updated_at)`
- `markers(id, clip_id, t_seconds, type['saved'|'goal'|'laugh'|'manual'|'imported'], label)`
- `games(id, name, exe_names_json, path, enabled)`
- `settings(key, value)`

## 8. Verification

- Soak test: 4 h with Rocket League running, save every ~2 min via scripted hotkey; 100 % of saves produce a playable clip with correct length and markers.
- Fault tests: kill FFmpeg mid-session, then save ⇒ clip saved from existing segments, engine restarts. Suspend FFmpeg (freeze) ⇒ watchdog restarts within 5 s. Fill disk ⇒ clear error toast.
- Performance: in-game FPS delta < 3 % with buffer on.
- Import: all 630 Moments clips (minus missing files) appear with titles, dates, games, markers; GG DB unchanged (hash before/after).

## 9. Left to the builder

- Exact FFmpeg arguments, NVENC preset/tuning, A/V sync technique.
- Crate choices (wasapi, sysinfo, etc.), UI component library, styling.
- Thumbnail strategy, DB migrations, log rotation details.
- Whether to hardlink or copy segments during save.
- Mouse-button hotkeys, per-track audio in player.

## 10. Risks

- `ddagrab` (Desktop Duplication) may not capture some exclusive-fullscreen games; Rocket League in borderless/fullscreen should work — verify early. Fallback: Windows Graphics Capture via a different input (later).
- Named-pipe audio feeding: a stalled pipe can stall FFmpeg; watchdog covers recovery, but audio capture must never block on a slow reader.
- Global hotkeys can be swallowed by some anti-cheat / elevated games; if the game runs elevated, ClipVault may need to run elevated too.
