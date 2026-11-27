# Game window capture (OBS-style) — design

Date: 2026-11-27. Status: draft for approval.

## Intent

With desktop capture off, clips of a game (e.g. Rocket League) still contain
desktop footage. Cause: recording is whole-monitor (Desktop Duplication in the
native grabber `capture::hdr_grab`, ddagrab as SDR fallback). Game detection
(`game_watcher`) only decides *when* to record — from process start, through
alt-tab and minimize, until 10 s after exit — not *what*. Everything on that
monitor lands in the buffer: launcher before the game window exists, desktop
while alt-tabbed, pop-ups over the game, the desktop after quitting.

The user wants it "like OBS": while a game is the target, the recording shows
the game and nothing else.

## Goals

- Game target records the game window's content only (Windows.Graphics.Capture,
  WGC), independent of what covers it on screen.
- Black frames whenever the game shows nothing to record: window not created
  yet (launch), minimized (incl. exclusive-fullscreen alt-tab), closed (grace
  period after exit), window being recreated.
- No yellow capture border (where Windows allows turning it off).
- Cursor only when over the game window (WGC draws it).
- Client area only (no title bar / borders of a windowed game), like OBS's
  default "client area" option.
- Non-16:9 window: fit into the recording size with black bars, never stretched.
- FFmpeg keeps running across alt-tab / window recreation: continuous frames,
  no restart gap, no watchdog trips.
- Same quality/perf path as today: frame stays on GPU, same compute shader,
  HDR handling unchanged.

- Applies regardless of the `desktop_capture` setting (user decision): a
  running game always records as its window; `desktop_capture` only governs
  the time when no game runs.

## Non-goals

- Desktop target (desktop capture on, no game) — stays whole-monitor, unchanged.
- Capturing game overlays drawn as separate windows (Discord pop-ups, Steam
  notifications) — intentionally excluded; in-game overlays drawn by the game
  itself (Discord in-game overlay) still appear, as in OBS.
- Per-game capture settings, picking a specific window by title.
- Game audio isolation changes (already per-process).

## Anti-goals

- No DLL injection / game hooking (anti-cheat risk: Easy Anti-Cheat in Rocket
  League; OBS is whitelisted, ClipVault is not).
- Never fall back to recording the monitor silently while a game is the target
  in a way that reintroduces the desktop footage — fallback only when window
  capture is impossible on this PC, and it is logged + toasted once.
- No added per-frame CPU copies or frame-rate loss compared with today.

## Constraints

- Windows 10 1903+ for WGC; `IsBorderRequired` needs Windows 11 (or
  Windows 10 build 20348+). Unpackaged desktop app: border-free access via
  `GraphicsCaptureAccess::RequestAccessAsync(Borderless)`; where unsupported or
  denied, the border shows (log once, no failure).
- `windows` crate 0.62; needs extra features: `Graphics_Capture`,
  `Graphics_DirectX`, `Graphics_DirectX_Direct3D11`, `Foundation`,
  `Foundation_Metadata` (ApiInformation), `Win32_System_WinRT`,
  `Win32_System_WinRT_Direct3D11`, `Win32_System_WinRT_Graphics_Capture`.
- D3D11 device for WGC must be wrapped as `IDirect3DDevice`
  (`CreateDirect3D11DeviceFromDXGIDevice`); create it with
  `D3D11_CREATE_DEVICE_BGRA_SUPPORT`. Adapter 0 as today (multi-GPU: WGC copies
  across adapters itself).
- Frame pool: `Direct3D11CaptureFramePool::CreateFreeThreaded`, pixel format
  `R16G16B16A16Float` (scRGB, same as the FP16 Desktop Duplication path, so
  `hdr_nv12.hlsl` and the SDR-white division apply unchanged). Grabber thread
  initialises WinRT as MTA.
- Uncommitted work in progress at time of writing (native grabber used on every
  monitor, `ddagrab_fallback`) is the base; build on it.

## Approaches considered

- **A. WGC as a second frame source inside the native grabber — chosen.**
  Reuses GPU conversion, pipe, wall-clock timing, buffer. Full control over
  black frames, crop, letterbox, border.
- B. FFmpeg's `gfxcapture` filter (present in the bundled N-127043 build).
  No new capture code, but its D3D11 frames cannot be scaled/converted on the
  GPU by this FFmpeg on NVIDIA (same problem documented for ddagrab in
  `args.rs` / `hdr_grab.rs`): CPU round-trip, ~36–50 fps at 4K measured; no
  black-while-minimized control, no HDR handling. Rejected.
- C. OBS-style game hook (inject into the game's swapchain). Exactly OBS game
  capture, but anti-cheat risk and very large. Rejected (anti-goal).

## Design

### Frame source

`HdrGrabConfig` gains `source: GrabSource`:

```rust
pub enum GrabSource {
    Monitor { output_idx: u32 },   // today's Desktop Duplication
    Window { exes: Vec<String> },  // WGC on the game's main window
}
```

The grabber loop is unchanged in shape (tick, read back previous, take newest
frame, dispatch). Only "take newest frame" differs per source. Each tick the
window source yields one of:

- `New` — a fresh frame copied into our FP16 texture,
- `Same` — no new frame, window still showing: re-send last picture,
- `Black` — nothing to show (no window yet, minimized, closed, recreated).

Black is produced by the shader (a flag in `Params`, or dispatching with the
destination rectangle empty) so the NV12 output is exact limited-range black
(Y=16, UV=128).

### Window tracking (inside the grabber thread)

- Find the game window: reuse `display.rs`'s window search, factored into
  `game_window(exes) -> Option<HWND>` (largest visible, unowned top-level
  window of any process with a matching exe). Unlike `window_monitor`, minimized windows count for
  tracking (we must know it is minimized to send black, and keep the item).
- No window: black; retry the search every 500 ms.
- Window found: `IGraphicsCaptureItemInterop::CreateForWindow`, create frame
  pool + session (`IsCursorCaptureEnabled = true`, `IsBorderRequired = false`
  when the API exists), `StartCapture`.
- `IsIconic(hwnd)` ⇒ black (WGC delivers nothing useful while minimized).
- Item `Closed` event or `IsWindow(hwnd)` false ⇒ drop session, black, search
  again (game recreating its window on a display-mode change, or exiting).
- Window search is by **exe name, not pid** (my call, from the builder
  check): `GrabSource::Window { exes: Vec<String> }` carries the game's
  matched exe names (`game_watcher::game_exes`, incl. the EAC mapping), and the
  grabber picks the largest visible, unowned top-level window of *any* running
  process with one of those names. Reason: the watcher may hand over the Easy
  Anti-Cheat bootstrap's pid (`rocketleague_eac.exe`, no game window), and a
  relaunch within the grace changes the pid without a target change. Searching
  by exe covers both without controller restarts.
- Per-window capture failure where WGC itself is supported
  (`CreateForWindow` / `StartCapture` error: elevated or protected window,
  device lost) ⇒ black frames, retry every 2 s, warn log once per distinct
  error. **No** fallback to the monitor (anti-goal). If the game window has
  been found but not captured for 10 s, toast once per capture: "Can't capture
  the game window – switch Game capture to Whole monitor in Settings"
  (my call). Monitor fallback stays reserved for `IsSupported() == false`.

### Frame size, client crop, letterbox

- `frame.ContentSize()` changes when the window resizes: recreate the frame
  pool (`Recreate`) at the new size; recreate our copy texture.
- Client area: per tick (cheap) compute the client rectangle inside the
  captured frame: `DwmGetWindowAttribute(DWMWA_EXTENDED_FRAME_BOUNDS)` (what
  WGC captures) vs `GetClientRect` + `ClientToScreen`; offset/size clamped to
  the frame. Borderless/fullscreen games: client == frame.
- The shader samples a source sub-rectangle (`src_origin`, `src_size`) and
  writes into a destination rectangle inside the output (letterbox / pillarbox,
  aspect preserved, centred); pixels outside are black. Today's whole-frame
  path is the special case "source = full frame, destination = full output".
  New `Params` fields; keep 16-byte row layout.

### HDR / SDR white

Unchanged: scRGB FP16 frames, divided by the SDR white of the monitor the game
window is on (the controller already resolves that monitor via
`window_monitor(pid)` and restarts on monitor change — keep that; with WGC the
monitor only affects SDR white).

### Controller / supervisor wiring

- `CaptureSpec` gains `game_window: Option<Vec<String>>` (the game's exe
  names; set for `CaptureTarget::Game` when the setting is on; `None` for
  Desktop). `hdr_grab_config` maps it to
  `GrabSource::Window`, else `Monitor { output_idx }`.
- `layout_key` unaffected (stream layout is the same).
- ddagrab fallback: only for `Monitor`. If the window source cannot start at
  all (WGC unsupported: `GraphicsCaptureSession::IsSupported()` false), the
  grabber falls back to `Monitor` for this capture with a warn log and a
  one-time toast "Window capture not supported – recording the monitor".
  A window that does not exist yet is **not** a failure (black frames).
- Watchdog / desktop kick: unchanged; the window source produces frames every
  tick, so segments keep flowing.
- Health/label: unchanged ("Rocket League").

### Setting

`Settings::game_capture: GameCapture { Window (default), Monitor }` — escape
hatch for a game where WGC shows black (rare: some old DX9 exclusive-fullscreen
titles). Capture-relevant (restarts capture). UI: one select in the capture
settings, "Game capture: Game window (like OBS) / Whole monitor".

### Logging

- Start: `native capture: window of rocketleague.exe (pid N, hwnd …, WxH client) -> 1920x1080@60`.
- State changes (window found / minimized / restored / closed / border not
  removable) at info, once per change — no per-tick spam.
- 60 s stats line gains black-frame count.

## Testing

- Unit (pure functions): letterbox rectangle math (16:9, 21:9, 4:3, portrait,
  odd sizes, never exceeding output, even alignment for NV12); client-crop
  math (frame bounds vs client rect, clamping); window-state → `New/Same/Black`
  decision.
- Shader: extend the existing e2e (`hdr_grab_ffmpeg_e2e`) or add one that
  checks a letterboxed output has black bars (Y=16) and a black frame is
  exactly Y=16/UV=128.
- Live `#[ignore]` test: spawn a window of our own (a plain Win32 window
  painted a solid colour), capture it 2 s via `GrabSource::Window`, check
  colour in the centre and black bars; minimize it, check black frames.
- Manual (docs/VERIFY.md): Rocket League — launch (black until the game
  window), alt-tab in borderless and in fullscreen, Discord pop-up over the
  game (absent), quit (black during grace), windowed mode (no title bar).

## Left to the builder

- Exact WinRT plumbing (event handler vs polling `TryGetNextFrame` each tick —
  polling preferred: fits the tick loop, no extra thread).
- How black is encoded in the shader (flag vs empty destination rect).
- Whether to rename `hdr_grab` (it now handles SDR and windows); a rename is
  fine but separate commit.
