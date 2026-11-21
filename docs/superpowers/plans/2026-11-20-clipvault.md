# ClipVault Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Windows tray app that keeps a crash-proof rolling replay buffer of the game/desktop and saves the last N seconds on a hotkey, with a clip library, markers, metadata editing and a SteelSeries Moments import.

**Architecture:** Tauri 2 app. Rust core supervises an FFmpeg sidecar that captures with `ddagrab`, scales/tonemaps on GPU, encodes with NVENC and writes 2 s MPEG-TS segments to a disk buffer; three audio sources are captured in Rust (WASAPI + process loopback) and fed to FFmpeg through named pipes. Saving = stream-copy concat of finished segments. React UI for library and settings; SQLite for the library.

**Tech Stack:** Rust (stable 1.99), Tauri 2, `tauri-plugin-global-shortcut`, `tauri-plugin-notification`, `rusqlite` (bundled), `wasapi` crate (process loopback), `windows` crate (DXGI), `sysinfo`, `tokio`, `serde`, `tracing` + `tracing-appender`, `trash`; React 18 + TypeScript + Vite + Vitest; FFmpeg GPL build (needs `ddagrab`, `scale_d3d11`, `libplacebo`, `h264_nvenc`, `hevc_nvenc`, `h264_amf`, `h264_qsv`) shipped as sidecar.

**Spec:** `docs/superpowers/specs/2026-11-20-clipvault-design.md`

## Global Constraints

- Windows 10 2004+ / Windows 11 only.
- A save request never ends silently: every save ends in a success toast+sound or an error toast+sound with a reason, and a log line.
- Segments: 2 s, MPEG-TS, GOP = fps × 2 with forced keyframes at segment boundaries.
- Retention: keep segments covering `max_save_length + 10 s`; wipe buffer dir on startup.
- Watchdog: unhealthy if FFmpeg exited or no new segment for > 5 s while capture should run; > 3 restarts within 60 s ⇒ tray error state + toast "Capture failing: <reason>".
- Output resolution default 1920×1080 (setting), GPU scaling only, no CPU frame round-trip on the SDR path.
- HDR monitor ⇒ tonemap to SDR BT.709 before encode.
- Defaults: 60 fps, H.264 NVENC, CBR 40 Mbps, save length 60 s, capture folder `E:\GameClips`, buffer dir `%LOCALAPPDATA%\ClipVault\buffer`.
- Audio tracks, in this order and with these MP4 titles: `Mic`, `Discord`, `Game/System`. 48 kHz stereo AAC.
- Clip filename: `<Game>__YYYY-MM-DD__HH-MM-SS.mp4` (local time; game name sanitized), written as `.tmp` then renamed.
- Game detection poll 2 s; buffer off 10 s after game exit. Desktop toggle ⇒ game name `Desktop`.
- Laugh detector: 100 ms RMS windows, threshold (dBFS, setting) held ≥ 300 ms, 5 s cooldown, on by default.
- Rocket League Stats API: `127.0.0.1:49123`; enabling edits `<RL>\TAGame\Config\DefaultStatsAPI.ini` only after user confirms, with a `.bak` copy.
- Moments DB `C:\ProgramData\SteelSeries\GG\apps\moments\db\database.db` is only ever opened from a temp copy, read-only. Never written.
- Delete = Recycle Bin. Trim is non-destructive; "Export trimmed" writes a new file.

## Review Focus

1. **Save pressed before the buffer holds `save_length` seconds** (game just started) ⇒ save whatever exists; toast states real length. Test in Task 6.
2. **Gap in segments** (FFmpeg restarted inside the save window) ⇒ clip still saved from all available segments in the window; toast notes "gap". Test in Task 6.
3. **Capture folder unavailable or disk full** (E: missing, no space) ⇒ error toast with path/reason, no partial `.mp4` left. Test in Task 6.
4. **Game names with characters illegal in filenames** (`:`, `?`, `"`, trailing dots) ⇒ sanitized filename, original name kept in DB. Test in Task 6.
5. **Selected monitor disappears or changes resolution** ⇒ pipeline restarts on the primary monitor with a toast, not a crash loop. Test in Task 4.

## File Structure

```
clipvault/
  package.json, vite.config.ts, tsconfig.json, index.html
  src/                         React UI
    api.ts                     typed wrappers over Tauri commands/events
    App.tsx                    routes: Library, Clip, Settings
    pages/Library.tsx  pages/ClipView.tsx  pages/Settings.tsx
    components/Timeline.tsx    markers + trim handles
    components/Timeline.test.ts
  src-tauri/
    tauri.conf.json            sidecar binaries/ffmpeg, asset protocol scope
    binaries/ffmpeg-x86_64-pc-windows-msvc.exe
    src/
      main.rs  lib.rs          Tauri setup, command registration
      app.rs                   AppCore: owns services, wires events
      settings.rs              Settings struct, load/save JSON
      db/mod.rs db/schema.sql  migrations, connection
      db/clips.rs db/games.rs  repositories
      capture/display.rs       DXGI outputs, HDR detection
      capture/args.rs          FFmpeg argument builder (pure)
      capture/segments.rs      segment index, retention, window selection (pure)
      capture/supervisor.rs    FFmpeg process + watchdog
      audio/pipes.rs           named pipe server, non-blocking writer
      audio/sources.rs         mic / Discord / game-system capture
      audio/laugh.rs           laugh detector (pure)
      events.rs                EventBus ring
      rl_stats.rs              Rocket League Stats API client + ini enabler
      games.rs                 GameWatcher
      save.rs                  SaveService
      thumbs.rs                thumbnail generation
      import_moments.rs        Moments import
      hotkeys.rs  tray.rs  notify.rs
    tests/                     integration tests (need ffmpeg on disk)
      fixtures/moments_schema.sql
  scripts/soak.ps1
```

---

### Task 1: Scaffold, settings, database

**Files:**
- Create: whole `clipvault/` skeleton via `npm create tauri-app@latest` (React + TS template), `src-tauri/src/settings.rs`, `src-tauri/src/db/{mod.rs,schema.sql,clips.rs,games.rs}`
- Test: unit tests in those files

**Interfaces:**
- Produces:
  - `struct Settings { fps: u32, save_length_s: u32, max_save_length_s: u32, encoder: Encoder, bitrate_kbps: u32, output_height: u32, monitor_id: Option<String>, desktop_capture: bool, buffer_dir: PathBuf, capture_dir: PathBuf, mic_device: Option<String>, output_device: Option<String>, mic_enabled: bool, discord_split: bool, laugh_enabled: bool, laugh_threshold_dbfs: f32, hotkey_save: String, hotkey_mark: String, hotkey_toggle_desktop: String, start_with_windows: bool }` with `Default` = Global Constraints values (`max_save_length_s` 120, hotkeys `Alt+F10`, `Alt+F9`, `Alt+F8`).
  - `enum Encoder { H264Nvenc, HevcNvenc, H264Amf, H264Qsv }`
  - `Settings::load(path) -> Result<Settings>` (missing file ⇒ default), `Settings::save(&self, path) -> Result<()>`
  - `db::open(path) -> Result<Connection>` runs migrations (`PRAGMA user_version`).
  - Tables exactly as spec §7.12; `clips.source` CHECK in (`native`,`moments`); `UNIQUE(source, source_id)`.
  - `struct Clip { id: String, title, description, path: PathBuf, game: String, recorded_at: DateTime<FixedOffset>, duration_s: f64, fps: u32, width: u32, height: u32, size: u64, trim_in: Option<f64>, trim_out: Option<f64>, thumbnail_path: Option<PathBuf>, source: String, source_id: Option<String>, tags: Vec<String>, markers: Vec<Marker> }`
  - `struct Marker { t_s: f64, kind: MarkerKind, label: String }`, `enum MarkerKind { Saved, Goal, Laugh, Manual, Imported }`
  - `clips::insert(&Connection, &Clip) -> Result<()>`, `clips::list(&Connection, ClipFilter) -> Result<Vec<Clip>>`, `clips::get(&Connection, &str) -> Result<Option<Clip>>`, `clips::update_meta(&Connection, id, title, description, tags) -> Result<()>`, `clips::set_trim(&Connection, id, Option<f64>, Option<f64>)`, `clips::delete(&Connection, id)`, `clips::exists_source(&Connection, source, source_id) -> Result<bool>`
  - `struct Game { id: i64, name: String, exe_names: Vec<String>, path: Option<PathBuf>, enabled: bool }`; `games::list`, `games::upsert_by_name`, `games::set_enabled`.

- [ ] **Step 1:** `git init`; scaffold Tauri 2 React-TS app in `clipvault/`; `cargo build` and `npm run tauri dev` open an empty window. Commit.
- [ ] **Step 2: Failing tests**: `settings_roundtrip` (save then load equals), `settings_missing_file_is_default` (`fps==60`, `bitrate_kbps==40000`, `capture_dir=="E:\\GameClips"`), `db_migrates_fresh` (tables `clips`,`markers`,`games`,`settings` exist, `user_version==1`), `clip_insert_get_with_markers`, `clip_update_meta`, `clip_unique_source_id` (second insert with same `("moments", id)` errors).
- [ ] **Step 3:** Run `cargo test -p clipvault` ⇒ FAIL.
- [ ] **Step 4:** Implement the interfaces above. Tags stored as JSON text.
- [ ] **Step 5:** `cargo test` ⇒ PASS. Commit `feat: settings and library database`.

### Task 2: Segment index and window selection (pure)

**Files:** Create `src-tauri/src/capture/segments.rs`

**Interfaces:**
- Produces:
  - `struct Segment { index: u64, path: PathBuf, start: SystemTime, end: SystemTime }`
  - `struct SegmentIndex` with `new(retention: Duration)`, `push(&mut self, Segment)`, `prune(&mut self, now: SystemTime) -> Vec<PathBuf>` (returns files to delete: all whose `end < now - retention`), `latest_end(&self) -> Option<SystemTime>`, `select(&self, from: SystemTime, to: SystemTime) -> Selection`
  - `struct Selection { segments: Vec<Segment>, covered: Duration, has_gap: bool }` — segments overlapping `[from, to]`, sorted; `has_gap` true if any consecutive pair has `next.start - prev.end > 500 ms`.
  - `fn parse_segment_list_line(line: &str, run_start: SystemTime, dir: &Path) -> Option<Segment>` parsing FFmpeg `segment_list_type csv` lines `seg_00000042.ts,84.000000,86.000000`.

- [ ] **Step 1: Failing tests**: `select_full_window` (40 contiguous 2 s segments, select last 60 s ⇒ 30 segments, `covered` 60 s, no gap), `select_short_buffer` (5 segments, ask 60 s ⇒ 5 segments, `covered` 10 s), `select_detects_gap` (2 runs with 6 s hole ⇒ `has_gap`), `prune_respects_retention` (retention 130 s), `parse_csv_line` (line above with run_start T ⇒ start T+84 s, end T+86 s).
- [ ] **Step 2:** `cargo test segments` ⇒ FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** PASS. Commit.

### Task 3: FFmpeg argument builder (pure)

**Files:** Create `src-tauri/src/capture/args.rs`

**Interfaces:**
- Consumes: `Settings`, `Encoder` (Task 1).
- Produces:
  - `struct CaptureSpec { output_idx: u32, fps: u32, out_w: u32, out_h: u32, hdr: bool, encoder: Encoder, bitrate_kbps: u32, audio_pipes: Vec<AudioPipe>, segment_dir: PathBuf, video_source: VideoSource }`
  - `enum VideoSource { Ddagrab, TestSrc }` (TestSrc = `testsrc2` lavfi, used by integration tests)
  - `struct AudioPipe { pipe_name: String, title: &'static str }` (format `f32le`, 48000 Hz, 2 ch)
  - `fn build_capture_args(spec: &CaptureSpec) -> Vec<String>`
  - `fn build_concat_args(list_file: &Path, out: &Path, track_titles: &[&str]) -> Vec<String>`
- Pinned values:
  - SDR video: `-f lavfi -i ddagrab=output_idx={i}:framerate={fps}:draw_mouse=1,scale_d3d11=w={w}:h={h}:format=nv12`
  - HDR video: `ddagrab=...:output_fmt=10bit` → `hwmap=derive_device=vulkan` → `libplacebo=w={w}:h={h}:format=nv12:colorspace=bt709:color_primaries=bt709:color_trc=bt709:tonemapping=bt.2390` → `hwmap=derive_device=cuda` (NVENC) — the implementer verifies this chain in Task 4 and may adjust the mapping steps; tonemap must stay on GPU.
  - Audio inputs: `-use_wallclock_as_timestamps 1 -f f32le -ar 48000 -ac 2 -i \\.\pipe\{name}`, each mapped, `-filter:a:{n} asetpts=PTS-NB_SAMPLES/SR/TB-{T0}/TB,aresample=async=1000`, `-c:a aac -b:a 160k`, `-metadata:s:a:{n} title={title}`. (Amended in Task 7: FFmpeg N-127043 rejects input `-thread_queue_size`; pipe timestamps must be rebased.)
  - Shared clock (Task 7): global `-copyts`; `T0 = CaptureSpec::wallclock_origin_us` (set by the supervisor per run to `run_start`, rendered in seconds with µs). lavfi sources (ddagrab/testsrc2 video, anullsrc) are followed directly (before hwdownload/scale/setparams) by `settb=AVTB,setpts='if(eq(N,0),st(0,(time(0)-{T0})/TB));PTS-STARTPTS+ld(0)'` (`asettb`/`asetpts` for audio), so every output timestamp is capture wall time − T0.
  - Video encode: `-c:v h264_nvenc -preset p4 -tune ll -rc cbr -b:v {kbps}k -g {fps*2} -forced-idr 1 -force_key_frames expr:if(isnan(prev_forced_t),1,gte(t+0.001,prev_forced_t+2))` (amended in Task 7: keyframes counted from the first frame, which `-copyts` puts at its wall time − T0).
  - Output: `-f segment -segment_time 2 -segment_time_delta 0.001 -segment_format mpegts -segment_list {dir}\list.csv -segment_list_type csv -segment_list_flags live {dir}\seg_%08d.ts`.
  - Concat: `-f concat -safe 0 -i {list} -map 0 -c copy -movflags +faststart -f mp4 {out}` plus per-track `title`.

- [ ] **Step 1: Failing tests**: `sdr_args_contain_ddagrab_scale_nvenc`, `hdr_args_use_libplacebo`, `three_audio_pipes_mapped_in_order` (titles `Mic`,`Discord`,`Game/System` at `s:a:0..2`), `gop_is_two_seconds` (`-g 120` at 60 fps, `-g 60` at 30), `testsrc_replaces_ddagrab`, `concat_args_faststart_mp4`.
- [ ] **Step 2:** FAIL. **Step 3:** Implement. **Step 4:** PASS. Commit.

### Task 4: Displays and HDR detection

**Files:** Create `src-tauri/src/capture/display.rs`

**Interfaces:**
- Produces:
  - `struct DisplayInfo { id: String /* "\\\\.\\DISPLAY8" */, output_idx: u32 /* DXGI enumeration index used by ddagrab */, width: u32, height: u32, refresh_hz: u32, primary: bool, hdr: bool }`
  - `fn list_displays() -> Result<Vec<DisplayInfo>>` (DXGI `IDXGIOutput6::GetDesc1`: `DeviceName`, `ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020` ⇒ hdr; index = order ddagrab uses)
  - `fn resolve_display(displays: &[DisplayInfo], wanted: Option<&str>) -> (DisplayInfo, bool /* fell_back */)`

- [ ] **Step 1: Failing tests**: `resolve_picks_wanted`, `resolve_falls_back_to_primary_when_missing` (Review Focus 5: wanted `\\.\DISPLAY9` absent ⇒ primary, `fell_back=true`), `list_displays_smoke` (`#[ignore]`-free on Windows: ≥ 1 display, primary present).
- [ ] **Step 2:** FAIL. **Step 3:** Implement.
- [ ] **Step 4: Manual spike** with Task 3 args against local FFmpeg: run 10 s capture of the primary monitor (SDR path) and, if any monitor reports `hdr=true`, the HDR path; confirm output plays, 1920×1080, colours not washed out. Record the final working HDR filter chain back into `args.rs` and its test.
- [ ] **Step 5:** PASS. Commit.

### Task 5: Capture supervisor and watchdog

**Files:** Create `src-tauri/src/capture/supervisor.rs`; Test: `src-tauri/tests/supervisor_it.rs`

**Interfaces:**
- Consumes: `CaptureSpec`, `build_capture_args` (Task 3), `SegmentIndex`, `parse_segment_list_line` (Task 2).
- Produces:
  - `enum Health { Idle, Recording, Restarting { reason: String }, Failing { reason: String } }`
  - `struct Supervisor` with `new(ffmpeg: PathBuf, retention: Duration, events: tokio::sync::broadcast::Sender<SupervisorEvent>)`, `async fn start(&self, spec: CaptureSpec)`, `async fn stop(&self)`, `fn segments(&self) -> Arc<Mutex<SegmentIndex>>`, `fn health(&self) -> Health`
  - `enum SupervisorEvent { HealthChanged(Health), SegmentClosed(Segment) }`
  - Pure core for testing: `struct WatchdogState`, `fn on_tick(&mut self, now: Instant, last_segment: Option<Instant>, process_alive: bool) -> WatchdogAction` with `enum WatchdogAction { None, Restart(String), Fail(String) }`.
- Behaviour: tails `list.csv`; on each new line pushes segment and prunes (deleting files); stderr to a per-run log file; stop kills process. On start, wipe buffer dir. On restart keep existing segments (new run's list file + new run_start; segment filenames get a run prefix `r{run}_seg_%08d.ts` to avoid collisions).

- [ ] **Step 1: Failing unit tests** for `WatchdogState`: `restart_when_no_segment_5s`, `restart_when_process_dead`, `fail_after_4th_restart_in_60s`, `restart_counter_window_slides`.
- [ ] **Step 2: Failing integration tests** (TestSrc source, real FFmpeg from `CLIPVAULT_FFMPEG` env): `produces_segments` (after 7 s ≥ 3 segments in index), `kill_then_recovers` (kill process, within 8 s health back to `Recording` and old segments still on disk), `retention_deletes_old` (retention 6 s, after 16 s oldest files gone).
- [ ] **Step 3:** `cargo test supervisor` ⇒ FAIL. **Step 4:** Implement. **Step 5:** PASS. Commit.

### Task 6: EventBus and SaveService

**Files:** Create `src-tauri/src/events.rs`, `src-tauri/src/save.rs`, `src-tauri/src/thumbs.rs`; Test: `src-tauri/tests/save_it.rs`

**Interfaces:**
- Consumes: `SegmentIndex::select` (Task 2), `build_concat_args` (Task 3), `clips::insert` (Task 1), `Supervisor::segments` (Task 5).
- Produces:
  - `struct BusEvent { at: SystemTime, kind: MarkerKind, label: String }`; `struct EventBus` with `push(BusEvent)`, `between(from, to) -> Vec<BusEvent>`, keeps last `max_save_length + 10 s`.
  - `fn sanitize_filename(name: &str) -> String` (replace `<>:"/\|?*` and control chars with `_`, trim trailing dots/spaces, empty ⇒ `Unknown`).
  - `fn clip_filename(game: &str, at: DateTime<Local>) -> String` ⇒ `Rocket-League__2026-11-18__21-37-14.mp4` (spaces ⇒ `-`, as Moments).
  - `enum SaveOutcome { Saved { clip: Clip, gap: bool, short: bool }, NotRecording, Failed { reason: String } }`
  - `struct SaveService` with `async fn save(&self, t_press: SystemTime, game: &str) -> SaveOutcome`; internal FIFO queue so concurrent calls run in order.
  - `fn generate_thumbnail(ffmpeg: &Path, video: &Path, out: &Path, at_s: f64) -> Result<()>` (JPEG 480 px wide).
- Behaviour (spec §7.5): wait until `latest_end >= t_press` (max 3 s); select `[t_press - save_length, t_press]`; hardlink (fallback copy) into `%TEMP%\clipvault-save-{uuid}`; concat to `<capture_dir>\<name>.mp4.tmp`; rename; thumbnail into `%LOCALAPPDATA%\ClipVault\thumbs\{id}.jpg`; markers = bus events in window mapped to clip-relative seconds + `Saved` at `t_press`; insert row; clean temp dir; on any error delete `.tmp`.

- [ ] **Step 1: Failing unit tests**: `bus_between_filters`, `sanitize_illegal_chars` (`Tom Clancy's: Siege?` ⇒ `Tom Clancy's_ Siege_`; `"..."` ⇒ `Unknown`), `clip_filename_format`, `markers_relative_to_clip_start`.
- [ ] **Step 2: Failing integration tests** (TestSrc + 3 silent pipes or `anullsrc` stand-in): `save_60s` (after 70 s of buffer, clip duration 59–62 s, 1 video + 3 audio streams titled `Mic`,`Discord`,`Game/System`, `moov` before `mdat`), `save_short_buffer` (after 10 s ⇒ `Saved{short:true}`, duration ≈ 10 s), `save_across_restart_gap` (kill FFmpeg mid-window ⇒ `Saved{gap:true}`, file plays), `save_to_missing_dir_fails_cleanly` (capture_dir `Z:\nope` ⇒ `Failed`, no `.tmp` left), `not_recording` (no segments ⇒ `NotRecording`), `two_rapid_saves_make_two_clips`.
- [ ] **Step 3:** FAIL. **Step 4:** Implement. **Step 5:** PASS. Commit.

### Task 7: Audio capture into named pipes

**Files:** Create `src-tauri/src/audio/pipes.rs`, `src-tauri/src/audio/sources.rs`

**Interfaces:**
- Consumes: `AudioPipe` (Task 3).
- Produces:
  - `struct PipeWriter` — creates `\\.\pipe\{name}` server, accepts FFmpeg, writes f32le 48 kHz stereo; `fn write(&self, frames: &[f32])` never blocks: bounded queue (500 ms), drops oldest on overflow and counts drops.
  - `enum SourceKind { Mic { device: Option<String> }, ProcessInclude { process: String /* "Discord.exe" */ }, ProcessExclude { process: String }, SystemLoopback { device: Option<String> } }`
  - `struct AudioSource` with `start(kind, writer: PipeWriter, on_level: Option<Box<dyn Fn(f32) + Send>>) -> Result<AudioSource>`, `stop()`. Emits silence at real-time rate whenever the underlying source is absent (Discord not running) and re-attaches when it appears (check every 2 s).
  - `fn plan_sources(settings: &Settings, process_loopback_supported: bool) -> Vec<(SourceKind, &'static str)>` ⇒ `[Mic, ProcessInclude Discord, ProcessExclude Discord]` titled `Mic`,`Discord`,`Game/System`; if unsupported or `discord_split=false` ⇒ `[Mic, SystemLoopback]` titled `Mic`,`Game/System`.
- Resample/convert to 48 kHz stereo f32 in Rust when the device format differs.

- [ ] **Step 1: Failing tests**: `writer_never_blocks_without_reader` (write 5 s of audio with no client connected returns within 50 ms total, drops counted), `plan_three_tracks_when_supported`, `plan_two_tracks_fallback`, `silence_when_process_absent` (ProcessInclude for `nonexistent.exe` yields ≈ 48000 frames/s of zeros over 2 s).
- [ ] **Step 2:** FAIL. **Step 3:** Implement with `wasapi` crate (`AudioClient::new_application_loopback_client(pid, include_tree)`).
- [ ] **Step 4: Manual check:** 30 s capture with Discord call + game + mic; play in VLC/mpv and switch tracks: voices only on `Discord`, game only on `Game/System`, own voice only on `Mic`; lip-sync of a clap visible at start and end (drift < 1 frame).
- [ ] **Step 5:** PASS. Commit.

### Task 8: Laugh detector

**Files:** Create `src-tauri/src/audio/laugh.rs`

**Interfaces:**
- Produces: `struct LaughDetector` with `new(threshold_dbfs: f32)`, `set_threshold(f32)`, `fn feed(&mut self, samples: &[f32], at: SystemTime) -> Option<SystemTime>` (returns the time the spike started when a marker should fire). Constants: window 100 ms at 48 kHz stereo, hold 300 ms, cooldown 5 s. Also `fn rms_dbfs(samples: &[f32]) -> f32` for the settings meter.

- [ ] **Step 1: Failing tests**: `fires_after_300ms_above`, `no_fire_for_200ms_burst`, `cooldown_5s`, `rms_of_full_scale_sine_is_minus_3db` (±0.1).
- [ ] **Step 2:** FAIL. **Step 3:** Implement. **Step 4:** PASS. Commit.

### Task 9: Rocket League Stats API

**Files:** Create `src-tauri/src/rl_stats.rs`; Test fixture inline.

**Interfaces:**
- Consumes: `EventBus::push` (Task 6).
- Produces:
  - `fn parse_message(raw: &str) -> Option<BusEvent-like (kind, label)>` — accepts `{"Event":"GoalScored","Data":...}` where `Data` may be an object or a JSON-encoded string; label `"Goal – {Scorer.Name}"` when present, else `"Goal"`.
  - `async fn run_listener(addr: SocketAddr, bus: Arc<EventBus>, cancel: CancellationToken)` — connect, read stream (messages may arrive concatenated; split by JSON object boundaries), reconnect every 3 s.
  - `fn stats_api_status(rl_install: &Path) -> StatsApiStatus { Enabled, Disabled, NotFound }` and `fn enable_stats_api(rl_install: &Path) -> Result<()>` (writes `PacketSendRate=30` under `[TAGame.MatchStatsExporter_TA]`, creates `.bak` first).
  - RL install path: from games table entry `Rocket League` path, else Epic/Steam default locations.

- [ ] **Step 1: Failing tests**: `parse_goal_object_data`, `parse_goal_string_data`, `ignores_other_events`, `listener_reads_concatenated_messages` (local TCP fixture sends two messages in one write ⇒ two bus events), `listener_reconnects` (fixture closes, reopens), `enable_writes_ini_and_backup` (temp dir with ini `PacketSendRate=0`).
- [ ] **Step 2:** FAIL. **Step 3:** Implement. **Step 4:** PASS. Commit.

### Task 10: Game watcher

**Files:** Create `src-tauri/src/games.rs`

**Interfaces:**
- Consumes: `games::list` (Task 1).
- Produces:
  - `trait ProcessSource { fn running_exes(&self) -> Vec<(u32, String /* lowercase exe name */)> }` with `SysinfoProcesses` impl.
  - `enum CaptureTarget { None, Game { name: String, pid: u32 }, Desktop }`
  - `struct GameWatcher` with `fn tick(&mut self, now: Instant, games: &[Game], desktop: bool, procs: &dyn ProcessSource) -> Option<CaptureTarget>` (returns `Some` only on change). Rules: enabled game running ⇒ Game; none ⇒ after 10 s grace ⇒ Desktop if `desktop` else None; first matching game wins if several.

- [ ] **Step 1: Failing tests**: `detects_enabled_game`, `ignores_disabled_game`, `grace_10s_before_stop`, `desktop_when_no_game_and_enabled`, `game_overrides_desktop`, `exe_match_case_insensitive`.
- [ ] **Step 2:** FAIL. **Step 3:** Implement. **Step 4:** PASS. Commit.

### Task 11: AppCore wiring, hotkeys, tray, notifications

**Files:** Create `src-tauri/src/app.rs`, `hotkeys.rs`, `tray.rs`, `notify.rs`; Modify `lib.rs`; add `src-tauri/sounds/{saved.wav,error.wav}`.

**Interfaces:**
- Consumes: everything from Tasks 1–10.
- Produces:
  - `struct AppCore` (in Tauri state) owning Settings, DB, Supervisor, AudioSources, EventBus, SaveService, GameWatcher, RL listener, LaughDetector.
  - Loop (every 2 s): GameWatcher tick ⇒ on change start/stop Supervisor + audio + RL listener (only while Rocket League); resolve display each start (fallback toast per Review Focus 5); HDR from `DisplayInfo`.
  - Hotkeys (`hotkeys.rs`): `fn register_all(app, &Settings) -> Vec<HotkeyError>`; errors shown as toast and returned to Settings UI. Save hotkey ⇒ `t_press = now` captured in the callback before anything else ⇒ `SaveService::save` ⇒ notify. Mark hotkey ⇒ `EventBus::push(Manual)` + short tick sound. Toggle desktop ⇒ flips setting, toast.
  - `notify::saved(&Clip, gap, short)` ⇒ toast "Clip saved (58 s, Rocket League)" (+ " – started late" / " – contains gap") + `saved.wav`; `notify::error(reason)` ⇒ toast + `error.wav`; `notify::not_recording()`.
  - Tray: icon states idle/recording/error from `Health`; menu: Open library, Save clip, Desktop capture ☑, Quit.
  - Tauri events to UI: `health-changed`, `clip-saved`, `mic-level` (10 Hz while Settings open).
  - Autostart via `tauri-plugin-autostart` when `start_with_windows`.
  - Mic samples from the `Mic` AudioSource also feed `LaughDetector` (when `laugh_enabled`) ⇒ `EventBus::push(Laugh)`.
  - Logging: `tracing` to `%LOCALAPPDATA%\ClipVault\logs\clipvault.log`, daily rotation, keep 7; every save request logs start + outcome.
- [ ] **Step 1: Failing test**: `hotkey_conflict_reported` (register same accelerator twice via a test double ⇒ one `HotkeyError` with the accelerator text).
- [ ] **Step 2:** FAIL. **Step 3:** Implement.
- [ ] **Step 4: Manual check**: launch app, start Rocket League (or add `notepad.exe` as a game) ⇒ tray goes "recording"; press `Alt+F10` ⇒ toast + sound + file in capture dir; kill `ffmpeg.exe` in Task Manager ⇒ tray recovers within ~8 s; press save ⇒ clip with gap toast.
- [ ] **Step 5:** PASS. Commit.

### Task 12: Moments import

**Files:** Create `src-tauri/src/import_moments.rs`, `src-tauri/tests/fixtures/moments_schema.sql` (copy of the `CREATE TABLE` statements for `moments_clips`, `moments_games`, `moments_settings` from spec §5 / the real DB).

**Interfaces:**
- Consumes: `clips::insert`, `clips::exists_source`, `games::upsert_by_name`, `generate_thumbnail`, `Settings`.
- Produces:
  - `struct ImportReport { imported: u32, skipped_existing: u32, missing_files: Vec<PathBuf>, games_imported: u32 }`
  - `fn import_moments(gg_db: &Path, conn: &Connection, settings: &mut Settings, ffmpeg: &Path) -> Result<ImportReport>`; copies `database.db` (+ `-wal`, `-shm` if present) to temp, opens with `SQLITE_OPEN_READ_ONLY`.
  - Mapping per spec §7.10: `name`→title, `description`, `path` (or `trimmed_clip_path` if `path` missing and trimmed exists), `last_game_name`→game, `recording_timestamp`→recorded_at, `full_length`→duration, `framerate`, `resolution_*`, `filesize`, `clip_start_point/clip_end_point` → trim only when not `0..full_length`, `tags` JSON (array of objects ⇒ take `name`/`label` string fields), `gamesense_events` (`null` string or `[]` ⇒ none; `localized_name_key == "moments.timelineEvents.savedClip"` ⇒ `Saved`; else `Imported` with label from `unlocalized_display_name` or key) at `clip_timestamp`. `source="moments"`, `source_id=id`.
  - Games: `moments_games` name, `path` file name + `launcher_executables` ⇒ `exe_names`, `capture_enabled` ⇒ enabled.
  - Settings: `captureDir`, `videoFps` only if user hasn't changed defaults.
  - UI command `import_moments` + first-run prompt.

- [ ] **Step 1: Failing tests** (fixture DB built in temp from schema + inserted rows, clip files as empty temp files): `imports_live_clips_only` (`is_deleted=1` skipped), `missing_file_reported`, `null_gamesense_events_ok`, `saved_marker_mapped` (34.24 s), `idempotent_second_run` (second run ⇒ `imported 0`, `skipped_existing` = previous count, and a title edited after first run is unchanged), `source_db_unchanged` (SHA-256 of fixture DB equal before/after), `games_imported_with_exes`.
- [ ] **Step 2:** FAIL. **Step 3:** Implement. **Step 4:** PASS.
- [ ] **Step 5: Real-data check:** run import against a copy of the real GG DB into a scratch ClipVault DB ⇒ report shows ≈ 630 imported minus missing; spot-check the 3 newest Rocket League clips (titles, 21:37:14 timestamp, saved marker). Commit.

### Task 13: Library commands

**Files:** Modify `src-tauri/src/lib.rs` (commands), Create `src-tauri/src/commands.rs`

**Interfaces:**
- Consumes: `db::clips`, `db::games`, `Settings`, `generate_thumbnail`.
- Produces Tauri commands (all `Result<T, String>`):
  - `list_clips(filter: { game?: string, search?: string, sort: "newest"|"oldest" }) -> Clip[]`
  - `get_clip(id) -> Clip`, `update_clip_meta(id, title, description, tags)`, `set_trim(id, trim_in?, trim_out?)`
  - `export_trimmed(id, accurate: bool) -> string /* new path */` — stream copy (`-ss/-to -c copy`) or NVENC re-encode when `accurate`; new file `<orig>__trim.mp4`, inserted as new clip.
  - `delete_clip(id)` — `trash::delete` then DB row removed; if trash fails, row kept and error returned.
  - `list_games()`, `set_game_enabled(id, bool)`, `add_game_from_process(pid)`, `list_running_processes()`
  - `get_settings()`, `save_settings(s)` (re-registers hotkeys, restarts capture if capture-relevant fields changed), `list_displays()`, `list_audio_devices()`, `rl_stats_status()`, `rl_enable_stats()`.
  - Asset protocol scope allows capture dir and thumbs dir.

- [ ] **Step 1: Failing tests** (call command fns directly with temp DB/files): `search_matches_title_and_tags`, `filter_by_game`, `delete_moves_to_trash_and_removes_row` (temp file no longer at path), `export_trimmed_creates_new_clip` (TestSrc clip 20 s, trim 5–10 s ⇒ new file 4.5–6 s with stream copy, exactly 5 s ±1 frame accurate).
- [ ] **Step 2:** FAIL. **Step 3:** Implement. **Step 4:** PASS. Commit.

### Task 14: Frontend — Library, Clip view, Settings

**Files:** Create `src/api.ts`, `src/App.tsx`, `src/pages/{Library,ClipView,Settings}.tsx`, `src/components/Timeline.tsx`, `src/components/Timeline.test.ts`

**Interfaces:**
- Consumes: commands and events from Tasks 11 and 13.
- Produces:
  - `Timeline` props: `{ duration: number, markers: Marker[], trimIn?: number, trimOut?: number, current: number, onSeek(t), onTrimChange(in, out) }`; exported pure helper `markerPositions(duration, markers, widthPx) -> {x, kind}[]`.
  - Library: grid cards (thumbnail, title, game, `dd.MM.yyyy HH:mm` taken time, duration `m:ss`, marker icons), game filter dropdown, search box, sort; live-inserts on `clip-saved`; header shows health (Idle / Recording – game / Error – reason).
  - ClipView: `<video>` via `convertFileSrc`, playback limited to trim range, Timeline with markers (Saved, Goal, Laugh, Manual, Imported — distinct icons, tooltip label), inline editable title/description/tags (save on blur), trim handles, Export trimmed (accurate toggle), Delete (confirm dialog).
  - Settings: sections per spec §7.11 incl. hotkey recorder (shows registration errors), monitor dropdown with HDR badge, audio devices, Discord split toggle, mic level meter with threshold slider, games list with enable toggles + "Add from running process", RL Stats API status + Enable button (confirm), Import from Moments button showing `ImportReport`.
- [ ] **Step 1: Failing tests** (Vitest): `markerPositions_scales_to_width` (duration 60, marker 30 s, width 600 ⇒ x 300), `markerPositions_drops_out_of_range`.
- [ ] **Step 2:** `npm test` ⇒ FAIL. **Step 3:** Implement components and pages.
- [ ] **Step 4:** `npm test` ⇒ PASS; `npm run tauri dev` manual pass through every page with imported library. Commit.

### Task 15: Verification and packaging

**Files:** Create `scripts/soak.ps1`; Modify `tauri.conf.json` (bundle NSIS installer, sidecar).

- [ ] **Step 1:** `scripts/soak.ps1 -Minutes 240 -IntervalSec 120` sends the save hotkey (`SendKeys`/`keybd_event`) on interval and afterwards checks: every press produced an `.mp4` that `ffprobe` reads with duration ≥ 55 s and 4 streams; prints PASS/FAIL table. Run with Rocket League (spec §8).
- [ ] **Step 2: Fault run:** during soak, suspend FFmpeg (`Suspend-Process`/pssuspend) once and kill it once; both saves still produce clips; log shows restarts.
- [ ] **Step 3: Performance:** Rocket League same replay/training map 2 min with ClipVault off vs on; FPS delta < 3 %.
- [ ] **Step 4: Import:** real import; GG DB SHA-256 unchanged.
- [ ] **Step 5:** `npm run tauri build` ⇒ installer in `src-tauri/target/release/bundle/nsis/`. Commit.
