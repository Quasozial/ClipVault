# Game Window Capture Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** While a game is the capture target, record only the game window (Windows.Graphics.Capture) — black when there is nothing to show — instead of the whole monitor.

**Architecture:** The native grabber (`capture/hdr_grab.rs`) gets a second frame source: a WGC session on the game's window, found by exe name. Each tick yields New / Same / Black; the existing compute shader gains a source crop (client area), a destination rectangle (letterbox) and a black flag. Controller → `CaptureSpec.game_window` → supervisor → grabber config; a new `GameCapture` setting is the escape hatch.

**Tech Stack:** Rust, `windows` 0.62 (WinRT + Win32), D3D11 compute (HLSL cs_5_0), FFmpeg via named pipe, React/TS settings page.

**Spec:** `docs/superpowers/specs/2026-11-27-game-window-capture-design.md`

## Global Constraints

- Build on the uncommitted working-tree changes (native grabber on every monitor, `CaptureSpec::ddagrab_fallback`); never revert them.
- No DLL injection / hooking of the game process.
- Desktop target (`CaptureTarget::Desktop`) stays whole-monitor; unchanged.
- A running game records window-only regardless of `desktop_capture`.
- Monitor fallback while a game is the target only when `GraphicsCaptureSession::IsSupported()` is false (log warn + toast once: `Window capture not supported – recording the monitor`).
- Per-window capture failure ⇒ black frames, retry every 2 s; after 10 s with the window found but not captured, toast once per capture: `Can't capture the game window – switch Game capture to Whole monitor in Settings`.
- Black output is exact NV12 limited-range black: Y = 16, U = V = 128.
- Output width % 4 == 0, height % 2 == 0 (existing shader rule); destination rectangle origin and size even.
- Frame pool format `R16G16B16A16Float`; device created with `D3D11_CREATE_DEVICE_BGRA_SUPPORT`; grabber thread WinRT MTA.
- Rust tests: `cd clipvault/src-tauri; $env:CARGO_TARGET_DIR='C:/cvt'; cargo test ...` (target dir as in `docs/VERIFY.md`). Frontend: `cd clipvault; npm test; npx tsc --noEmit`.
- Log state changes once per change (info), never per tick.

## Review Focus

1. **Game recreates its window** (fullscreen ↔ borderless switch, or EAC bootstrap then real game): expect ≤ ~1 s black, then the new window — never a frozen frame or a dead session. Test: Task 4 live test `window_recreated_is_found_again`.
2. **Window resized while recording** (windowed game dragged larger): expect frame pool recreated, letterbox recomputed, no stretch, no crash. Tests: Task 1 `letterbox_*` cases; Task 4 live test `resize_keeps_aspect`.
3. **Several windows of the game's exes** (splash/launcher then main window, EAC bootstrap with no window): expect the largest visible unowned window, switching when a larger one appears. Test: Task 3 `picks_largest_and_switches`.
4. **Minimize then restore**: expect black while minimized and no flash of the pre-minimize frame after restore (black until a fresh frame). Test: Task 1 `restore_waits_for_fresh_frame`.
5. **HDR monitor brightness**: window capture of SDR content on an HDR desktop must match today's monitor capture brightness (same SDR-white division). Test: Task 4 live test checks the solid colour's Y within ±3 on the dev PC (HDR); manual check in Task 6 VERIFY entry.

---

### Task 1: Geometry and picture decision (pure functions)

**Files:**
- Create: `clipvault/src-tauri/src/capture/window_grab.rs` (pure part, no `cfg`)
- Modify: `clipvault/src-tauri/src/capture/mod.rs` (add `pub mod window_grab;`)

**Interfaces:**
- Produces:
  - `pub struct Rect { pub x: u32, pub y: u32, pub w: u32, pub h: u32 }` (derive Debug, Clone, Copy, PartialEq, Eq)
  - `pub fn letterbox(src_w: u32, src_h: u32, out_w: u32, out_h: u32) -> Rect` — fit preserving aspect, centred; w and h rounded to the nearest even number (min 2), x and y even.
  - `pub fn client_crop(frame_w: u32, frame_h: u32, bounds: (i32, i32, i32, i32), client: (i32, i32, i32, i32)) -> Rect` — `bounds` = DWM extended frame bounds (left, top, right, bottom, screen px), `client` = client rect in screen px. Result = client relative to bounds, clamped to the frame; empty/invalid ⇒ whole frame.
  - `pub enum WindowPicture { New, Same, Black }`
  - `pub struct PictureState { pub found: bool, pub minimized: bool, pub capturing: bool, pub new_frame: bool, pub have_last: bool }`
  - `pub fn decide(s: &PictureState) -> WindowPicture` — Black if `!found || minimized || !capturing`; else New if `new_frame`; else Same if `have_last`; else Black. Caller clears `have_last` whenever Black is decided (documented on the fn).

- [ ] **Step 1: Write the failing tests** in `window_grab.rs` `mod tests`:

```rust
#[test] fn letterbox_same_aspect() { assert_eq!(letterbox(3840, 2160, 1920, 1080), Rect { x: 0, y: 0, w: 1920, h: 1080 }); }
#[test] fn letterbox_ultrawide() { assert_eq!(letterbox(3440, 1440, 1920, 1080), Rect { x: 0, y: 138, w: 1920, h: 804 }); }
#[test] fn letterbox_4_3() { assert_eq!(letterbox(1600, 1200, 1920, 1080), Rect { x: 240, y: 0, w: 1440, h: 1080 }); }
#[test] fn letterbox_portrait() { assert_eq!(letterbox(1080, 1920, 1920, 1080), Rect { x: 656, y: 0, w: 608, h: 1080 }); }
#[test] fn letterbox_tiny_source() { let r = letterbox(1, 1, 1920, 1080); assert!(r.w >= 2 && r.h >= 2 && r.x % 2 == 0 && r.y % 2 == 0); }
#[test] fn letterbox_never_exceeds_output() {
    for (w, h) in [(1281, 721), (1366, 768), (2559, 1079), (7, 3000)] {
        let r = letterbox(w, h, 1920, 1080);
        assert!(r.x + r.w <= 1920 && r.y + r.h <= 1080, "{w}x{h}: {r:?}");
        assert!(r.w % 2 == 0 && r.h % 2 == 0 && r.x % 2 == 0 && r.y % 2 == 0, "{w}x{h}: {r:?}");
    }
}
#[test] fn client_crop_windowed() {
    assert_eq!(client_crop(1280, 720, (100, 100, 1380, 820), (108, 131, 1372, 812)), Rect { x: 8, y: 31, w: 1264, h: 681 });
}
#[test] fn client_crop_borderless_is_full() {
    assert_eq!(client_crop(1920, 1080, (0, 0, 1920, 1080), (0, 0, 1920, 1080)), Rect { x: 0, y: 0, w: 1920, h: 1080 });
}
#[test] fn client_crop_clamped_and_empty() {
    assert_eq!(client_crop(800, 600, (0, 0, 800, 600), (-50, -50, 900, 700)), Rect { x: 0, y: 0, w: 800, h: 600 });
    assert_eq!(client_crop(800, 600, (0, 0, 800, 600), (10, 10, 10, 10)), Rect { x: 0, y: 0, w: 800, h: 600 });
}
#[test] fn decide_cases() {
    let base = PictureState { found: true, minimized: false, capturing: true, new_frame: false, have_last: true };
    assert_eq!(decide(&PictureState { found: false, ..base }), WindowPicture::Black);
    assert_eq!(decide(&PictureState { minimized: true, new_frame: true, ..base }), WindowPicture::Black);
    assert_eq!(decide(&PictureState { capturing: false, ..base }), WindowPicture::Black);
    assert_eq!(decide(&PictureState { new_frame: true, ..base }), WindowPicture::New);
    assert_eq!(decide(&base), WindowPicture::Same);
}
#[test] fn restore_waits_for_fresh_frame() {
    // After a Black (minimized), the caller cleared have_last: restored window with no new frame stays black.
    let s = PictureState { found: true, minimized: false, capturing: true, new_frame: false, have_last: false };
    assert_eq!(decide(&s), WindowPicture::Black);
}
```

(`WindowPicture` derives Debug, Clone, Copy, PartialEq, Eq; `PictureState` derives Clone, Copy.)

- [ ] **Step 2: Run** `cargo test --lib capture::window_grab` — Expected: FAIL (items not defined).
- [ ] **Step 3: Implement** the items above in `window_grab.rs`. Letterbox: scale = min(out_w/src_w, out_h/src_h) in f64; `even(v) = max(2, 2 * round(v / 2))` capped at the output dimension; x = (out_w − w)/2 rounded down to even, same for y.
- [ ] **Step 4: Run** `cargo test --lib capture::window_grab` — Expected: PASS.
- [ ] **Step 5: Commit** `feat(capture): window capture geometry and picture decision`.

---

### Task 2: Shader crop, letterbox and black

**Files:**
- Modify: `clipvault/src-tauri/src/capture/hdr_nv12.hlsl`
- Modify: `clipvault/src-tauri/src/capture/hdr_grab.rs` (`Params`, `Gpu::dispatch`)

**Interfaces:**
- Consumes: `window_grab::Rect`.
- Produces: `Gpu::dispatch(&mut self, cfg: &HdrGrabConfig, staging: usize, view: &View)` with
  `pub(crate) struct View { pub src: Rect, pub dst: Rect, pub black: bool }`, defined in `hdr_grab.rs`. `View::full(src_w, src_h, out_w, out_h)` = whole source into whole output, not black (today's behaviour, used by the monitor path).

New `Params` layout (16-byte rows, must match the cbuffer exactly):

```
row0: out_size uint2, scale float2          // scale = src.w/dst.w, src.h/dst.h
row1: src_inv float2, inv_white f32, cursor_mode u32
row2: cursor_pos int2, cursor_size uint2
row3: src_origin float2, dst_origin uint2
row4: dst_size uint2, black u32, _pad u32
```

Shader `pixel(o)`: if `black != 0` or `o` outside `[dst_origin, dst_origin + dst_size)` ⇒ return float3(0,0,0) (encodes to Y=16, UV=128). Else `center = src_origin + (float2(o - dst_origin) + 0.5) * scale`; cursor position stays relative to the source frame (monitor path only).

- [ ] **Step 1: Write the failing test** in `hdr_grab.rs` tests:

```rust
#[test] fn params_match_cbuffer_layout() { assert_eq!(std::mem::size_of::<imp::Params>(), 80); }
#[test] fn full_view_is_identity() {
    let v = View::full(3840, 2160, 1920, 1080);
    assert_eq!(v.src, Rect { x: 0, y: 0, w: 3840, h: 2160 });
    assert_eq!(v.dst, Rect { x: 0, y: 0, w: 1920, h: 1080 });
    assert!(!v.black);
}
```

(make `imp::Params` `pub(super)` for the test; the test is `#[cfg(windows)]`.)

- [ ] **Step 2: Run** `cargo test --lib capture::hdr_grab` — Expected: FAIL.
- [ ] **Step 3: Implement** `View`, new `Params`, shader changes; the monitor loop calls `dispatch` with `View::full(frame_w, frame_h, out_w, out_h)`.
- [ ] **Step 4: Run** `cargo test --lib capture::hdr_grab` — PASS; then the hardware tests `cargo test --lib capture::hdr_grab -- --ignored hdr_grab_ffmpeg_e2e hdr_grab_live` — Expected: PASS as before (monitor picture unchanged).
- [ ] **Step 5: Commit** `feat(capture): shader source crop, letterbox and black frames`.

---

### Task 3: Find the game window by exe name

**Files:**
- Modify: `clipvault/src-tauri/src/capture/display.rs`
- Modify: `clipvault/src-tauri/src/game_watcher.rs` (make `game_exes` `pub`)

**Interfaces:**
- Produces:
  - `pub struct WindowCandidate { pub hwnd: isize, pub exe: String, pub area: i64, pub minimized: bool }`
  - `pub fn pick_game_window(cands: &[WindowCandidate], exes: &[String]) -> Option<&WindowCandidate>` — pure: candidates whose `exe` (lowercase file name) is in `exes`; prefer non-minimized, then largest `area`; ties ⇒ first.
  - `#[cfg(windows)] pub fn game_window(exes: &[String]) -> Option<(HWND, bool /*minimized*/)>` — `EnumWindows` over visible, unowned top-level windows (minimized ones included, using their restored size from `GetWindowPlacement` for `area`), exe via `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` + `QueryFullProcessImageNameW` (cache per pid within one call), then `pick_game_window`.
  - `pub fn game_exes(g: &Game) -> Vec<String>` (now public, unchanged).

- [ ] **Step 1: Write the failing tests** in `display.rs` tests:

```rust
fn c(h: isize, exe: &str, area: i64, min: bool) -> WindowCandidate { WindowCandidate { hwnd: h, exe: exe.into(), area, minimized: min } }
#[test] fn picks_largest_and_switches() {
    let exes = vec!["rocketleague.exe".to_string(), "rocketleague_eac.exe".to_string()];
    let splash = c(1, "rocketleague.exe", 600 * 400, false);
    assert_eq!(pick_game_window(&[splash.clone(), c(9, "explorer.exe", 9_999_999, false)], &exes).unwrap().hwnd, 1);
    let main = c(2, "rocketleague.exe", 3840 * 2160, false);
    assert_eq!(pick_game_window(&[splash, main], &exes).unwrap().hwnd, 2);
}
#[test] fn prefers_visible_over_minimized() {
    let exes = vec!["a.exe".to_string()];
    assert_eq!(pick_game_window(&[c(1, "a.exe", 100, true), c(2, "a.exe", 10, false)], &exes).unwrap().hwnd, 2);
    assert_eq!(pick_game_window(&[c(1, "a.exe", 100, true)], &exes).unwrap().hwnd, 1);
}
#[test] fn no_matching_window() { assert!(pick_game_window(&[c(1, "b.exe", 5, false)], &["a.exe".into()]).is_none()); }
```

- [ ] **Step 2: Run** `cargo test --lib capture::display` — Expected: FAIL.
- [ ] **Step 3: Implement** `WindowCandidate`, `pick_game_window`, `game_window`; add `Win32_System_ProcessStatus` only if `QueryFullProcessImageNameW` needs it (it lives in `Win32_System_Threading`, already enabled).
- [ ] **Step 4: Run** `cargo test --lib capture::display game_watcher` — PASS.
- [ ] **Step 5: Commit** `feat(capture): find a game's main window by exe name`.

---

### Task 4: WGC window source in the grabber

**Files:**
- Modify: `clipvault/src-tauri/Cargo.toml` (`windows` features: `Foundation`, `Foundation_Metadata`, `Graphics`, `Graphics_Capture`, `Graphics_DirectX`, `Graphics_DirectX_Direct3D11`, `Security_Authorization_AppCapabilityAccess`, `Win32_System_WinRT`, `Win32_System_WinRT_Direct3D11`, `Win32_System_WinRT_Graphics_Capture`)
- Modify: `clipvault/src-tauri/src/capture/window_grab.rs` (Windows part: `WindowSource`)
- Modify: `clipvault/src-tauri/src/capture/hdr_grab.rs` (config, `Gpu::new`, tick loop, `HdrGrabber::start`)

**Interfaces:**
- Consumes: Task 1 (`letterbox`, `client_crop`, `decide`), Task 2 (`View`, `dispatch`), Task 3 (`game_window`).
- Produces:
  - `HdrGrabConfig` gains `pub window_exes: Option<Vec<String>>` (`Some` ⇒ window source; `output_idx` kept for the monitor fallback).
  - `pub type Notice = Arc<dyn Fn(String) + Send + Sync>`; `HdrGrabber::start(cfg: HdrGrabConfig, notice: Notice) -> Result<HdrGrabber>`.
  - `#[cfg(windows)] pub(crate) struct WindowSource` with `fn new(device: &ID3D11Device, exes: Vec<String>, notice: Notice) -> Result<WindowSource>` and `fn poll(&mut self, now: Instant) -> (WindowPicture, Option<(ID3D11Texture2D, Rect /*client crop*/)>)`.

Behaviour of `WindowSource::poll` (per tick):
- No session: every 500 ms `game_window(exes)`; found ⇒ `IGraphicsCaptureItemInterop::CreateForWindow`, `Direct3D11CaptureFramePool::CreateFreeThreaded(device, R16G16B16A16Float, 2, size)`, session with `SetIsCursorCaptureEnabled(true)`, `SetIsBorderRequired(false)` when `ApiInformation::IsPropertyPresent("Windows.Graphics.Capture.GraphicsCaptureSession", "IsBorderRequired")` (first time: `GraphicsCaptureAccess::RequestAccessAsync(Borderless)?.get()`; failure ⇒ info log once, keep border), `StartCapture`. Errors ⇒ warn once per distinct message, retry after 2 s, start the 10 s toast timer.
- Session: every 2 s re-run `game_window`; different hwnd ⇒ drop session (switch to the larger window). `IsWindow` false or item `Closed` (checked via a flag set by the `Closed` handler) ⇒ drop session.
- Drain `TryGetNextFrame` to the newest; `ContentSize` ≠ pool size ⇒ `Recreate` the pool. Texture via `IDirect3DDxgiInterfaceAccess::GetInterface`.
- Client crop each new frame: `DwmGetWindowAttribute(DWMWA_EXTENDED_FRAME_BOUNDS)` + `GetClientRect` + `ClientToScreen` ⇒ `client_crop`.
- `decide(...)` gives the picture; on Black clear the last-frame flag.

Grabber changes:
- `Gpu::new`: create the device with `D3D11_CREATE_DEVICE_BGRA_SUPPORT`; `output: Option<IDXGIOutput5>`; skip `EnumOutputs`/`duplicate` for the window source.
- Thread start: `RoInitialize(RO_INIT_MULTITHREADED)`.
- `HdrGrabber::start` with `window_exes = Some` and `GraphicsCaptureSession::IsSupported()` false ⇒ `notice("Window capture not supported – recording the monitor")`, warn log, run as monitor source.
- Tick: window source New ⇒ copy texture into `gpu.frame` (factor the copy out of `take_frame` into `copy_frame(&ID3D11Texture2D)`), `View { src: crop, dst: letterbox(crop.w, crop.h, out_w, out_h), black: false }`; Same ⇒ last view; Black ⇒ `View { black: true, .. }` (dispatch even with no frame yet — bind any 1×1 texture created at startup as `src`). Cursor mode 0 for the window source.
- 60 s stats line adds `{black} black`.

- [ ] **Step 1: Write the failing live tests** (`#[ignore]`, in `window_grab.rs`, Windows only). Helper `TestWindow::open(w, h, rgb)` creates a Win32 window in this process (solid `hbrBackground`), pumps messages on its own thread, and offers `minimize()`, `restore()`, `resize(w, h)`, `close()`. Helper `read_frames(pipe_name, n) -> Vec<Vec<u8>>` opens `\\.\pipe\<name>` as a client and reads `nv12_size` chunks. Grabber config: `window_exes = Some(vec![current_exe_file_name_lowercase])`, 1920×1080@30, `sdr_white` from the display under the window (`window_monitor(std::process::id())` → `list_displays` → `sdr_white` rule of `capture_ctl`).

```rust
#[test] #[ignore] fn window_capture_shows_window_only() {
    // 400x400 red window ⇒ pillarbox: centre is red, (100, 540) is black.
    let win = TestWindow::open(400, 400, (255, 0, 0));
    let f = last_frame_after(&win, Duration::from_secs(2));
    assert_near(y_at(&f, 960, 540), 63, 3); assert_near(v_at(&f, 960, 540), 240, 3);
    assert_eq!(y_at(&f, 100, 540), 16);
}
#[test] #[ignore] fn minimized_window_is_black() {
    // minimize ⇒ every Y 16, every UV 128 within 1 s; restore ⇒ red again within 1 s.
}
#[test] #[ignore] fn window_recreated_is_found_again() {
    // close the window, open a new one (green) ⇒ black, then green centre within 2 s.
}
#[test] #[ignore] fn resize_keeps_aspect() {
    // resize 400x400 ⇒ 800x400 (client): before, (100, 540) black (pillarbox); after, letterbox 1920x960 at y 60:
    // (100, 540) red, (960, 30) black.
}
#[test] #[ignore] fn no_window_yet_is_black() {
    // exes = ["clipvault-no-such-game.exe"] ⇒ frames flow and are all black.
}
```

(Fill the commented bodies with the same helpers; expected values are in the comments. Y/U/V for pure red in BT.709 limited: Y 63, U 102, V 240; green: Y 173, U 42, V 26.)

- [ ] **Step 2: Run** `cargo test --lib capture::window_grab -- --ignored` — Expected: FAIL (no window source).
- [ ] **Step 3: Implement** Cargo features, `WindowSource`, grabber changes above.
- [ ] **Step 4: Run** `cargo test --lib capture::window_grab -- --ignored` — PASS; `cargo test --lib capture` — PASS; `cargo test --lib capture::hdr_grab -- --ignored` — PASS (monitor path unchanged).
- [ ] **Step 5: Commit** `feat(capture): record the game window with Windows.Graphics.Capture`.

---

### Task 5: Wiring — spec, setting, controller, supervisor, toast

**Files:**
- Modify: `clipvault/src-tauri/src/settings.rs`
- Modify: `clipvault/src-tauri/src/capture/args.rs` (`CaptureSpec`, test `spec()`)
- Modify: `clipvault/src-tauri/src/capture/supervisor.rs` (`hdr_grab_config`, `SupervisorEvent`, `test_spec`)
- Modify: `clipvault/src-tauri/src/app/capture_ctl.rs`
- Modify: `clipvault/src-tauri/src/app/control.rs` (toast on notice)
- Modify: `clipvault/src-tauri/src/capture/hdr_grab.rs` (e2e test spec), `clipvault/src-tauri/tests/{audio_smoke_it,av_sync_it,save_it,supervisor_it}.rs` (add `game_window: None`)

**Interfaces:**
- Consumes: Task 4 `HdrGrabConfig::window_exes`, `HdrGrabber::start(cfg, notice)`; Task 3 `game_exes`.
- Produces:
  - `#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)] pub enum GameCapture { #[default] Window, Monitor }`; `Settings::game_capture: GameCapture`, part of `capture_differs`.
  - `CaptureSpec::game_window: Option<Vec<String>>` (not part of `layout_key`).
  - `SupervisorEvent::Notice(String)`; supervisor passes `Arc::new(move |m| { let _ = events.send(SupervisorEvent::Notice(m)); })` as the grabber's `Notice`.
  - `fn hdr_grab_config(spec: &CaptureSpec) -> Option<HdrGrabConfig>` copies `game_window` into `window_exes`.
  - Controller: `set_target` stores `self.game_exes: Option<Vec<String>>` = `game_exes` of the `Game` whose `name` equals the target's name (None for Desktop/None); `start` sets `game_window = self.game_exes.clone().filter(|_| s.game_capture == GameCapture::Window)`.

- [ ] **Step 1: Write the failing tests:**

```rust
// settings.rs
#[test] fn game_capture_defaults_to_window_and_restarts_capture() {
    assert_eq!(Settings::default().game_capture, GameCapture::Window);
    let old: Settings = serde_json::from_str("{}").unwrap();
    assert_eq!(old.game_capture, GameCapture::Window, "settings files without the field");
    assert!(changed(&|s| s.game_capture = GameCapture::Monitor));
}
// supervisor.rs
#[test] fn window_spec_maps_to_window_grabber() {
    let mut s = test_spec(Path::new("x")); s.video_source = VideoSource::Ddagrab; s.hdr_sdr_white = Some(1.0);
    s.game_window = Some(vec!["rocketleague.exe".into()]);
    assert_eq!(hdr_grab_config(&s).unwrap().window_exes, Some(vec!["rocketleague.exe".to_string()]));
    s.game_window = None;
    assert_eq!(hdr_grab_config(&s).unwrap().window_exes, None);
}
// capture_ctl.rs
#[test] fn window_exes_only_for_games_with_window_setting() {
    // pure helper: fn game_window_for(target: &CaptureTarget, games: &[Game], mode: GameCapture) -> Option<Vec<String>>
    let games = [Game { id: 1, name: "Rocket League".into(), exe_names: vec!["RocketLeague_EAC.exe".into()], path: None, enabled: true }];
    let rl = CaptureTarget::Game { name: "Rocket League".into(), pid: 7 };
    assert_eq!(game_window_for(&rl, &games, GameCapture::Window), Some(vec!["rocketleague_eac.exe".into(), "rocketleague.exe".into()]));
    assert_eq!(game_window_for(&rl, &games, GameCapture::Monitor), None);
    assert_eq!(game_window_for(&CaptureTarget::Desktop, &games, GameCapture::Window), None);
}
```

- [ ] **Step 2: Run** `cargo test --lib settings capture::supervisor app::capture_ctl` — Expected: FAIL.
- [ ] **Step 3: Implement** the interfaces above. Monitor resolution for a game (`start`, `recheck_game_monitor`) stays on `window_monitor(pid)`; with the window source it only picks the SDR white, so a bootstrap pid with no window (fallback to the configured monitor) is acceptable. `control.rs` handles `SupervisorEvent::Notice(m)` with `core.ui.toast(APP_TITLE, &m)`; the capture-start log line adds `window={game_window:?}`.
- [ ] **Step 4: Run** `cargo test` (whole crate incl. `tests/`) — Expected: PASS.
- [ ] **Step 5: Commit** `feat(capture): record games as their window, Game capture setting`.

---

### Task 6: Settings UI and manual verification

**Files:**
- Modify: `clipvault/src/api.ts` (`game_capture: "Window" | "Monitor"` in the settings type)
- Modify: `clipvault/src/mock/backend.ts` (default `game_capture: "Window"`)
- Modify: `clipvault/src/pages/Settings.tsx` (row after "Monitor")
- Modify: `docs/VERIFY.md` (new section)

**Interfaces:**
- Consumes: Task 5 `Settings::game_capture` (serde names `"Window"`, `"Monitor"`).

Row: label `Game capture`, select id `gcap`, options `Window` → `Game window (like OBS)`, `Monitor` → `Whole monitor`; hint `Game window records only the game: no desktop, no pop-ups, black while it is minimized. Whole monitor records everything on the game's screen.` Monitor row hint becomes `Desktop recording, and games when Game capture is Whole monitor.`

- [ ] **Step 1: Implement** the type, mock default and row (same `Row`/`select` pattern as the Monitor row).
- [ ] **Step 2: Run** `cd clipvault; npx tsc --noEmit; npm test` — Expected: no type errors, all tests pass.
- [ ] **Step 3: Add** `docs/VERIFY.md` section "Game window capture" with checkboxes: Rocket League launch (black until game window, never the launcher/desktop); alt-tab in borderless (game keeps showing) and in fullscreen (black); Discord/notification pop-up over the game absent from the clip; quit ⇒ black during the 10 s grace; windowed mode ⇒ no title bar, black bars when not 16:9; no yellow border on screen; brightness on the HDR monitor equal to a "Whole monitor" clip of the same scene; switching the setting to Whole monitor restores today's behaviour.
- [ ] **Step 4: Commit** `feat(settings): Game capture option; verification checklist`.
