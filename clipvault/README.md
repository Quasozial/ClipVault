# Tauri + React + Typescript

This template should help get you started developing with Tauri, React and Typescript in Vite.

## Recommended IDE Setup

- [VS Code](https://code.visualstudio.com/) + [Tauri](https://marketplace.visualstudio.com/items?itemName=tauri-apps.tauri-vscode) + [rust-analyzer](https://marketplace.visualstudio.com/items?itemName=rust-lang.rust-analyzer)

## Packaging

1. `scripts\fetch-ffmpeg.ps1` copies an FFmpeg build (ddagrab + NVENC required) to `src-tauri/binaries/` (git-ignored); it is bundled as a Tauri sidecar and installed next to the exe as `ffmpeg.exe`.
2. `CARGO_TARGET_DIR=C:/cvt npm run tauri build` produces `C:/cvt/release/bundle/nsis/ClipVault_0.1.0_x64-setup.exe`.

The bundled FFmpeg is a **GPL** build: fine for personal use; redistributing the installer carries GPL obligations (source offer, licence text). ffprobe is not bundled (unused at runtime). Manual verification steps: `docs/VERIFY.md`.
