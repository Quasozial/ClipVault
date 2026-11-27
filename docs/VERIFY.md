# ClipVault: human verification checklist

Everything that can be automated has been run (see the Task 15 report). The items below need you at the PC with an
awake display, a game, and your ears. Do them in order; tick each box.

Conventions: `FF` = `C:\Users\pasca\AppData\Local\Microsoft\WinGet\Packages\yt-dlp.FFmpeg_Microsoft.Winget.Source_8wekyb3d8bbwe\ffmpeg-N-127043-g5a54fcf75e-win64-gpl\bin\ffmpeg.exe`
(or whatever `scripts\fetch-ffmpeg.ps1` copied). Repo root = this repository. Audio tracks in a clip are, in order:
**1 Mic, 2 Discord, 3 Game/System** (track titles are written into the file).

## 0. Install and prepare

- [ ] Run `scripts\fetch-ffmpeg.ps1` (checks ddagrab, h264_nvenc, hevc_nvenc), then `cd clipvault; $env:CARGO_TARGET_DIR='C:/cvt'; npm run tauri build`.
  Installer: `C:\cvt\release\bundle\nsis\ClipVault_0.1.0_x64-setup.exe`. Install it normally (per-user).
- [ ] **Turn SteelSeries Moments OFF** before testing (GG -> Moments -> disable recording, ideally quit GG). Moments and ClipVault would fight over
  hotkeys (do not leave both on Ctrl+Shift+F10 / F9 / F8) and over desktop capture and the audio devices.
- [ ] Start ClipVault, open Settings: capture dir `E:\GameClips`, save length 60 s, hotkeys as shown. Keep the main monitor **awake**
  (Windows power plan: turn display off = never for the duration of the tests).
- FFmpeg licence: the bundled FFmpeg is a **GPL** build. Fine for personal use; if you ever redistribute the installer you must
  also provide the FFmpeg source offer and licence text.

## 1. 4 h soak with fault injection (Rocket League)

Why: proves ring buffer, supervisor restarts, concat/save over hours and 120 saves.

1. Start Rocket League (free play / training, leave it running; idle in a match is fine). ClipVault tray = recording.
2. In PowerShell (RL does not need focus for global hotkeys, but keep it running):
   ```powershell
   cd <repo>
   .\scripts\soak.ps1 -Minutes 240 -IntervalSec 120 -SaveLengthSec 60 -FaultInjection
   ```
   (`-DryRun` shows the schedule only. `-Hotkey`, `-CaptureDir` are parameters if you changed them.)
3. Do not touch anything that changes the hotkey or capture settings. Browsing is fine.
4. At the end it prints a PASS/FAIL table and writes a log `%TEMP%\clipvault-soak-*.json`
   (re-verify later with `.\scripts\soak.ps1 -VerifyOnly <that json>`).

Pass criteria
- Exit code 0, `SUMMARY: ... 0 FAIL`.
- 120 presses, each with a clip, duration >= 55 s, `1v+3a`.
- The presses right after the suspend (+81 min) and after the kill (+161 min) are marked `after suspend` / `after kill`; they must
  still produce a clip (duration >= 5 s, `1v+3a`). Expect a "started late" or gap toast and a shorter clip for those two only.
- `%LOCALAPPDATA%\ClipVault\logs\clipvault.<date>.log` shows supervisor `run N ended (...)` lines (the restarts) around those times; soak.ps1 prints their count, and no FFmpeg process is left
  leaking: `Get-Process ffmpeg` shows only the current capture processes, memory not growing (compare at 0 h and 4 h in Task Manager;
  ClipVault < ~300 MB, stable).
- Note: do not delete the soak clips until you looked at one or two of them.

## 2. FPS comparison (Rocket League)

Pass: average FPS drop < 3 % with ClipVault capturing vs. not running.

PresentMon (preferred): `winget install Intel.PresentMon.Console`, then in RL load the **same training map / replay**, do the same
2-minute routine each time (e.g. fixed-camera free play with nothing happening on screen differently).
```powershell
# 1. Quit ClipVault (tray -> Quit). Confirm: Get-Process clipvault, ffmpeg  -> nothing
.\scripts\perf-compare.ps1 -Measure off -Process RocketLeague.exe -Seconds 120
# 2. Start ClipVault, wait until the tray shows recording (~15 s)
.\scripts\perf-compare.ps1 -Measure on  -Process RocketLeague.exe -Seconds 120
# 3.
.\scripts\perf-compare.ps1 -Compare        # prints avg FPS, 1 % low, drop %, PASS/FAIL
```
Without PresentMon: use the in-game FPS counter (RL Settings -> Interface -> Show FPS), note the average over the same 2 minutes
off and on, drop = (off - on) / off. Repeat each condition twice to see run-to-run noise (if noise > 3 % the test is inconclusive;
use a GPU-bound setting or a fixed replay).

## 3. R8: static desktop keeps recording (awake, untouched display)

Why: ddagrab on a static screen must still produce frames (the app kicks the output). Previously failed only because the monitor was off.

1. Make sure the monitor is on and showing a **static** desktop (no video, no animation, clock hidden or at least not changing is ideal).
2. Quit ClipVault (the test starts its own FFmpeg). **Do not touch the mouse or keyboard for ~60 s** after starting:
   ```powershell
   cd clipvault\src-tauri
   $env:CARGO_TARGET_DIR='C:/cvt'
   $env:CLIPVAULT_FFMPEG='<FF>'; $env:CLIPVAULT_E2E='1'; $env:CLIPVAULT_E2E_R8='1'
   cargo test --test app_e2e_it static_desktop -- --nocapture
   ```
Pass: test ok (first segment within 15 s, >= 13 new segments in 30 s, health unchanged).
If it fails: record the output; the fallbacks are in task-11-report (raise the 2x2 kick window alpha, or `RedrawWindow`).

### 3b. Desktop kick over a game (Rocket League, true Fullscreen and Borderless)

Why: the kick window (2x2 px, alpha 1/255) must not keep flashing over a game. It is capped at 3 kicks per FFmpeg process and
skipped while capture is failing.

1. Rocket League -> Settings -> Video -> Display Mode **Fullscreen**. Play ~5 min of free play with ClipVault recording, save a clip.
2. Repeat with Display Mode **Borderless**.
3. Pass (each mode): the clip is fine; in `%LOCALAPPDATA%\ClipVault\logs\clipvault.<date>.log` the lines containing `kicking`
   appear at most once per `run N` (only at capture start, if at all), there is **no** `not kicking this FFmpeg process again`
   line while the game renders, and `run N ended (K kicks)` lines (if any) show K <= 3. No flicker, no focus loss, no stutter
   in the top-left corner of the game.
   ```powershell
   Select-String -Path "$env:LOCALAPPDATA\ClipVault\logs\clipvault.*.log" -Pattern 'kicking|kicks\)'
   ```

## 4. A/V sync with real screen capture (clap test)

1. Open a visible on-screen stopwatch with centisecond display (e.g. https://stopwatch.onlineclock.net/ or any ms stopwatch) on the
   captured monitor, full-screen or large. Mic in front of you, Discord/Game audio silent.
2. Start the stopwatch. Then, with the stopwatch visible, **clap once close to the mic** and note the stopwatch value at the clap (a
   second person or a phone filming the screen+you helps; otherwise use a loud audible click on Game/System too, e.g.
   `[console]::beep(1000,100)` run at a moment you can read on the stopwatch).
3. Press the save hotkey ~10 s later. Open the clip in an editor with frame stepping and waveform (Shotcut, DaVinci Resolve, or `mpv`
   + Audacity for the waveform).
4. Pass: the beep/clap transient in the audio track aligns with the frame where the on-screen event happens within **1 frame
   (16.7 ms at 60 fps)** on Mic and on Game/System.
5. **Drift:** leave ClipVault running for **2 h** (RL or desktop capture on), repeat step 2-4. Pass: still within 1 frame, no drift
   between start and 2 h.
6. Also run the synthetic-plus-real ddagrab variant (monitor awake, otherwise it prints SKIP):
   ```powershell
   cd clipvault\src-tauri
   $env:CARGO_TARGET_DIR='C:/cvt'; $env:CLIPVAULT_FFMPEG='<FF>'; $env:CLIPVAULT_AUDIO_SMOKE='1'
   cargo test --test av_sync_it -- --nocapture
   ```
   Pass: both cases ok and the ddagrab case does **not** print SKIP (prints first video pts and impulse error within tolerance).

## 5. Discord track separation (listening)

Setup: Discord call running (a friend, or a second account/phone talking), a game or a YouTube video playing as game audio, you speaking
into the mic. Record ~30 s, save a clip.

Switch tracks:
- **VLC:** Audio -> Audio Track -> pick Track 1 / 2 / 3 (names show as Mic / Discord / Game/System).
- **mpv:** press `#` to cycle audio tracks (or `mpv --aid=2 clip.mp4`).

Pass
- Track **Mic**: only your own voice (no Discord voices, no game).
- Track **Discord**: only the Discord voices (not your own voice, not the game).
- Track **Game/System**: only the game / system sounds (no Discord, no your voice).
- Spot check with FFmpeg if you like: `& <FF> -i clip.mp4 -map 0:a:1 -f null -` should run without errors.

## 6. Tray, toasts, sounds, Quit

- [ ] Tray icon colors/states: idle (not capturing) -> recording while a game runs / Desktop capture toggled via tray or Ctrl+Shift+F8 ->
  error colour when capture fails (e.g. kill ffmpeg repeatedly or disable the monitor). Tooltip text matches the state.
- [ ] Toasts: saving a clip shows "Clip saved (NN s, Game)" and plays the saved sound; a failed save shows "Clip NOT saved: ...";
  pressing the hotkey while not recording shows "Not recording - nothing to save" (+ sound). Windows Focus Assist off for this test.
- [ ] Tray menu: Open library, Save clip, Desktop capture checkbox, Quit work.
- [ ] **Quit** really exits: after tray -> Quit, `Get-Process clipvault, ffmpeg -ErrorAction SilentlyContinue` is empty within ~3 s.
  Also closing the window only hides it to the tray (process stays).
- [ ] Autostart toggle in Settings works (Task Manager -> Startup lists ClipVault). Sign out and in again: ClipVault starts
  **in the tray only** (no window; `reg query HKCU\Software\Microsoft\Windows\CurrentVersion\Run /v ClipVault` ends in
  `--autostart`).
- [ ] Single instance: with ClipVault running (recording), start it again from the Start menu. Pass: no second process
  (`Get-Process clipvault` shows one), the existing window comes to the front, recording continues without a gap.

## 7. Rocket League goal markers

1. ClipVault Settings -> Rocket League: **Enable Stats API** (the app writes the RL INI setting), then **restart Rocket League**.
2. Free play: score a goal (or several), wait ~5 s, press the save hotkey.
3. Open the clip in the library. Pass: a goal marker appears on the timeline at the moment of the goal (+/- 1 s), the toast/clip list shows
   Rocket League as game. A manual mark (Ctrl+Shift+F9) also shows up as a marker.

## 8. Moments import on the real library

Pre: SteelSeries Moments OFF (or GG quit). Note the DB hash first:
```powershell
Get-FileHash C:\ProgramData\SteelSeries\GG\apps\moments\db\database.db -Algorithm SHA256
```
1. ClipVault Settings -> Import from Moments -> Import. Expect **about 630** clips imported (the report dialog shows
   imported / skipped / missing files). Run it a second time: imported must be 0 (all skipped_existing).
2. Spot-check 5-10 clips: titles, game, dates, Rocket League markers, thumbnails, trim points; open and play a few.
3. Pass: the hash above is **identical** afterwards (the import only reads a copy), library count matches, no crash, the files in
   `E:\GameClips` are untouched.

## 9. Monitor-missing fallback

1. Settings -> Capture monitor: pick a specific (secondary) monitor, save. Start a game/desktop capture: recording works.
2. Unplug/disable that monitor (or set `monitor_id` to a bogus value in `%LOCALAPPDATA%\ClipVault\settings.json` with ClipVault quit).
3. Pass: ClipVault falls back to the primary monitor (log says so), recording continues, no crash loop.

## 10. Game window capture

Settings -> Game capture = **Game window (like OBS)** (default).

- [ ] Launch Rocket League: the buffer is black until the game window exists, and never shows the launcher or the desktop.
- [ ] Alt-tab away in borderless mode: the game keeps showing in the clip. In fullscreen mode: the clip goes black.
- [ ] A Discord/notification pop-up over the game is absent from the saved clip.
- [ ] Quit the game: the clip is black during the 10 s grace period.
- [ ] Windowed mode: no title bar in the clip; black bars when the window is not 16:9.
- [ ] No yellow capture border appears on screen.
- [ ] Brightness on the HDR monitor equals a "Whole monitor" clip of the same scene.
- [ ] Switching the setting to **Whole monitor** restores the previous behaviour (monitor capture).

## Recording results

Write date, build (`ClipVault_0.1.0`), PASS/FAIL per section and any logs (`%LOCALAPPDATA%\ClipVault\logs`) into your notes. If a
check fails, keep the clip and log files and report which step.
