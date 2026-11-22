//! Tauri commands for the library, games, settings and Rocket League pages.
//!
//! Thin wrappers: the logic lives in `library.rs` as plain functions. Every
//! command returns `Result<T, String>` with a human-readable message. Work that
//! touches disk or FFmpeg runs on blocking threads, never on the UI thread.
//! (JS passes arguments camelCase: `trimIn`, `trimOut`, `generateThumbnails`.)
use crate::app::tauri_ui::TauriUi;
use crate::app::AppCore;
use crate::capture::display::DisplayInfo;
use crate::audio::sources::AudioDevice;
use crate::db::clips::Clip;
use crate::db::games::Game;
use crate::hotkeys::HotkeyError;
use crate::library::{self, ListFilter, RlStatsInfo, RunningProcess};
use crate::settings::Settings;
use crate::{rl_stats, FfmpegPath};
use anyhow::Context as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tauri::{AppHandle, Manager, State};

type Core<'a> = State<'a, Arc<AppCore>>;

/// Runs `f` on a blocking thread, flattening errors to display strings.
async fn blocking<T, F>(f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| format!("background task failed: {e}"))?
        .map_err(|e| format!("{e:#}"))
}

fn ffmpeg_of(f: &FfmpegPath) -> Result<PathBuf, String> {
    f.0.clone().ok_or_else(|| "FFmpeg was not found".to_string())
}

/// Lets the webview load clip videos and thumbnails (`convertFileSrc`): the
/// capture folder, the thumbnails folder and every folder that holds a library
/// clip (imported clips live elsewhere). The scope only grows while running.
pub fn refresh_asset_scope(app: &AppHandle, core: &AppCore) {
    let paths: Vec<PathBuf> = {
        let conn = core.db();
        let conn = crate::locks::lock(&conn);
        let rows = conn.prepare("SELECT path FROM clips").and_then(|mut st| {
            st.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()
        });
        match rows {
            Ok(v) => v.into_iter().map(PathBuf::from).collect(),
            Err(e) => {
                tracing::warn!("asset scope: listing clip paths: {e}");
                Vec::new()
            }
        }
    };
    let scope = app.asset_protocol_scope();
    for (dir, recursive) in library::asset_scope_entries(&core.settings(), &core.thumbs_dir(), paths) {
        if let Err(e) = scope.allow_directory(&dir, recursive) {
            tracing::warn!("asset scope: allowing {}: {e}", dir.display());
        }
    }
}

// ----- clips -----

/// `filter`: `{ game?: string, search?: string, sort?: "newest"|"oldest" }` -> `Clip[]`.
#[tauri::command]
pub async fn list_clips(core: Core<'_>, filter: ListFilter) -> Result<Vec<Clip>, String> {
    let db = core.db();
    blocking(move || library::list_clips(&crate::locks::lock(&db), &filter)).await
}

#[tauri::command]
pub async fn get_clip(core: Core<'_>, id: String) -> Result<Clip, String> {
    let db = core.db();
    blocking(move || library::get_clip(&crate::locks::lock(&db), &id)).await
}

/// Returns the updated clip.
#[tauri::command]
pub async fn update_clip_meta(
    core: Core<'_>,
    id: String,
    title: String,
    description: String,
    tags: Vec<String>,
) -> Result<Clip, String> {
    let db = core.db();
    blocking(move || library::update_clip_meta(&crate::locks::lock(&db), &id, &title, &description, &tags)).await
}

/// Stores the trim range (`null` = clip start/end; both `null` clears). Returns the updated clip.
#[tauri::command]
pub async fn set_trim(
    core: Core<'_>,
    id: String,
    trim_in: Option<f64>,
    trim_out: Option<f64>,
) -> Result<Clip, String> {
    let (db, thumbs) = (core.db(), core.thumbs_dir());
    blocking(move || {
        let clip = library::set_trim(&crate::locks::lock(&db), &id, trim_in, trim_out)?;
        library::forget_strip(&thumbs, &id);
        Ok(clip)
    })
    .await
}

/// Cuts the trim range into a new `<name>__trim.mp4` and adds it as a clip.
/// Returns the new file's path. Can take a while when `accurate` (re-encode).
#[tauri::command]
pub async fn export_trimmed(
    core: Core<'_>,
    ffmpeg: State<'_, FfmpegPath>,
    id: String,
    accurate: bool,
) -> Result<String, String> {
    let (db, thumbs, ff, s) = (core.db(), core.thumbs_dir(), ffmpeg_of(&ffmpeg)?, core.settings());
    let reencode = accurate.then_some(s.encoder);
    blocking(move || library::export_trimmed(&db, &ff, &thumbs, &s.capture_dir, &id, reencode)).await
}

/// Re-encodes the clip (trim range if set) for sharing into `<capture dir>/Exports`.
/// Returns the new file's path. Not added to the library.
#[tauri::command]
pub async fn export_clip(
    core: Core<'_>,
    ffmpeg: State<'_, FfmpegPath>,
    id: String,
    options: crate::export::ExportOptions,
) -> Result<String, String> {
    let (db, ff, s) = (core.db(), ffmpeg_of(&ffmpeg)?, core.settings());
    blocking(move || {
        let clip = library::get_clip(&crate::locks::lock(&db), &id)?;
        let out = crate::export::export_clip(&ff, &clip, &s.capture_dir, s.encoder, &options)?;
        Ok(out.to_string_lossy().into_owned())
    })
    .await
}

/// Opens Explorer with the file selected.
/// Only files ClipVault made or knows (see [`library::revealable`]).
#[tauri::command]
pub async fn reveal_in_folder(core: Core<'_>, path: String) -> Result<(), String> {
    let (db, capture_dir) = (core.db(), core.settings().capture_dir);
    let file = blocking(move || {
        let conn = crate::locks::lock(&db);
        let mut st = conn.prepare("SELECT path FROM clips")?;
        let clips = st.query_map([], |r| r.get::<_, String>(0))?.map(|r| r.map(PathBuf::from)).collect::<rusqlite::Result<Vec<_>>>()?;
        library::revealable(Path::new(&path), &capture_dir, &clips)
    })
    .await?;
    tauri_plugin_opener::reveal_item_in_dir(&file).map_err(|e| format!("could not open the folder: {e}"))
}

#[tauri::command]
pub async fn set_starred(core: Core<'_>, id: String, starred: bool) -> Result<Clip, String> {
    let db = core.db();
    blocking(move || library::set_starred(&crate::locks::lock(&db), &id, starred)).await
}

/// Stars or unstars many clips at once (library bulk bar); returns the changed clips.
#[tauri::command]
pub async fn set_starred_many(core: Core<'_>, ids: Vec<String>, starred: bool) -> Result<Vec<Clip>, String> {
    let db = core.db();
    blocking(move || library::set_starred_many(&crate::locks::lock(&db), &ids, starred)).await
}

/// Adds tags to many clips at once (library bulk bar); returns the changed clips.
#[tauri::command]
pub async fn add_tags_many(core: Core<'_>, ids: Vec<String>, tags: Vec<String>) -> Result<Vec<Clip>, String> {
    let db = core.db();
    blocking(move || library::add_tags_many(&crate::locks::lock(&db), &ids, &tags)).await
}

/// Recycle Bin, then the row. A file that is already gone still removes the row.
#[tauri::command]
pub async fn delete_clip(core: Core<'_>, id: String) -> Result<(), String> {
    let (db, thumbs) = (core.db(), core.thumbs_dir());
    let cache = crate::waveform::cache_dir(&thumbs);
    blocking(move || {
        library::delete_clip(&crate::locks::lock(&db), &id)?;
        crate::waveform::forget(&cache, &id);
        library::forget_strip(&thumbs, &id);
        Ok(())
    })
    .await
}

/// Every audio track of the clip as its own playable file plus a waveform
/// envelope. Extracted with FFmpeg on first use, then cached.
#[tauri::command]
pub async fn audio_tracks(
    core: Core<'_>,
    ffmpeg: State<'_, FfmpegPath>,
    id: String,
) -> Result<Vec<crate::waveform::AudioTrack>, String> {
    let (db, cache, ff) = (core.db(), crate::waveform::cache_dir(&core.thumbs_dir()), ffmpeg_of(&ffmpeg)?);
    blocking(move || {
        let clip = library::get_clip(&crate::locks::lock(&db), &id)?;
        crate::waveform::audio_tracks(&ff, &clip.path, &cache, &id)
    })
    .await
}

/// Thumbnail path of the clip, generated on demand (one FFmpeg at a time);
/// `null` when it cannot be made (the grid then shows a placeholder).
#[tauri::command]
pub async fn ensure_thumbnail(core: Core<'_>, ffmpeg: State<'_, FfmpegPath>, id: String) -> Result<Option<String>, String> {
    let (db, thumbs) = (core.db(), core.thumbs_dir());
    let Some(ff) = ffmpeg.0.clone() else { return Ok(None) };
    let key = id.clone();
    Ok(library::THUMBS.run(&key, move || library::ensure_thumbnail(&db, &ff, &thumbs, &id)).await)
}

/// Hover-scrub strip of the clip (one JPEG, frames side by side); made on
/// first use, queued behind thumbnails. `None` without FFmpeg or on failure.
#[tauri::command]
pub async fn ensure_scrub_strip(core: Core<'_>, ffmpeg: State<'_, FfmpegPath>, id: String) -> Result<Option<String>, String> {
    let (db, thumbs) = (core.db(), core.thumbs_dir());
    let Some(ff) = ffmpeg.0.clone() else { return Ok(None) };
    let key = format!("strip:{id}");
    Ok(library::THUMBS.run(&key, move || library::ensure_scrub_strip(&db, &ff, &thumbs, &id)).await)
}

// ----- games -----

#[tauri::command]
pub async fn list_games(core: Core<'_>) -> Result<Vec<Game>, String> {
    let db = core.db();
    blocking(move || crate::db::games::list(&crate::locks::lock(&db))).await
}

#[tauri::command]
pub async fn set_game_enabled(core: Core<'_>, id: i64, enabled: bool) -> Result<(), String> {
    let db = core.db();
    blocking(move || crate::db::games::set_enabled(&crate::locks::lock(&db), id, enabled)).await
}

/// Programs with a visible window (one per exe), sorted by name.
#[tauri::command]
pub async fn list_running_processes() -> Result<Vec<RunningProcess>, String> {
    blocking(|| Ok(library::list_running_processes())).await
}

/// Adds the running process `pid` as a game (name = exe without `.exe`).
#[tauri::command]
pub async fn add_game_from_process(core: Core<'_>, pid: u32) -> Result<Game, String> {
    let db = core.db();
    blocking(move || library::add_game_from_process(&crate::locks::lock(&db), pid)).await
}

// ----- settings & devices -----

#[tauri::command]
pub fn get_settings(core: Core<'_>) -> Settings {
    core.settings()
}

/// Validates, persists and applies the settings: capture restarts only when a
/// capture-relevant field changed; hotkeys are re-registered and "start with
/// Windows" synced. Returns the hotkeys that could not be registered (the
/// settings are saved regardless). The file, DB and registry work runs on a
/// blocking thread so the main thread (which receives the save hotkey) never
/// waits on it; only the hotkey registration is sent to the main thread.
#[tauri::command]
pub async fn save_settings(app: AppHandle, core: Core<'_>, settings: Settings) -> Result<Vec<HotkeyError>, String> {
    library::validate_settings(&settings)?;
    let core: Arc<AppCore> = core.inner().clone();
    let (a, c) = (app.clone(), core.clone());
    blocking(move || {
        c.apply_settings(settings).context("could not save settings")?;
        crate::sync_autostart(&a, c.settings().start_with_windows);
        refresh_asset_scope(&a, &c);
        Ok(())
    })
    .await?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    let (a, c) = (app.clone(), core.clone());
    app.run_on_main_thread(move || {
        let ui = TauriUi { app: a.clone() };
        let _ = tx.send(crate::apply_hotkeys(&a, &c, &ui));
    })
    .map_err(|e| format!("could not register hotkeys: {e}"))?;
    rx.await.map_err(|_| "could not register hotkeys".to_string())
}

/// One encoder choice for the Settings page.
#[derive(serde::Serialize)]
pub struct EncoderOption {
    pub id: crate::settings::Encoder,
    pub label: &'static str,
    pub hardware: bool,
}

/// `{ available, auto }`: encoders this PC can run (best first) and what
/// "Automatic" picks. Empty `available` = unknown (probe pending or failed).
#[derive(serde::Serialize)]
pub struct EncoderList {
    pub available: Vec<EncoderOption>,
    pub auto: EncoderOption,
}

#[tauri::command]
pub fn list_encoders(core: Core<'_>) -> EncoderList {
    let avail = core.available_encoders();
    let opt = |e: crate::settings::Encoder| EncoderOption { id: e, label: e.label(), hardware: e.is_hardware() };
    let (auto, _) = crate::capture::encoders::resolve(crate::settings::Encoder::Auto, &avail);
    EncoderList { available: avail.iter().filter(|e| e.can_record()).map(|&e| opt(e)).collect(), auto: opt(auto) }
}

/// Queues the clip for background AV1 compression; `clip-updated` follows.
#[tauri::command]
pub fn compress_clip(core: Core<'_>, id: String) {
    core.compressor().enqueue_next(&id);
}

/// Whether the clip is waiting for or in background compression, and where.
#[tauri::command]
pub fn compress_status(core: Core<'_>, id: String) -> Option<crate::compress::Stage> {
    core.compressor().stage(&id)
}

#[tauri::command]
pub async fn list_displays() -> Result<Vec<DisplayInfo>, String> {
    blocking(crate::capture::display::list_displays).await
}

#[tauri::command]
pub async fn list_audio_devices() -> Result<Vec<AudioDevice>, String> {
    blocking(|| Ok(crate::audio::sources::list_audio_devices())).await
}

// ----- Rocket League Stats API -----

fn rl_install_of(core: &AppCore) -> anyhow::Result<Option<PathBuf>> {
    let games = crate::db::games::list(&crate::locks::lock(&core.db()))?;
    Ok(rl_stats::find_rl_install(&games))
}

/// `{ status: "enabled"|"disabled"|"not_found", ini_path, port, listener_connected, rl_install, note }`
#[tauri::command]
pub async fn rl_stats_status(core: Core<'_>) -> Result<RlStatsInfo, String> {
    let core = core.inner().clone();
    blocking(move || {
        let install = rl_install_of(&core)?;
        Ok(library::rl_info(dirs::document_dir().as_deref(), install.as_deref(), core.rl_listener_connected()))
    })
    .await
}

/// Edits the Stats API ini (backing it up first); the UI confirms before calling.
/// Returns the new status with `note` asking to restart Rocket League.
#[tauri::command]
pub async fn rl_enable_stats(core: Core<'_>) -> Result<RlStatsInfo, String> {
    let core = core.inner().clone();
    blocking(move || {
        let install = rl_install_of(&core)?;
        rl_stats::enable_stats_api(install.as_deref())?;
        let mut info =
            library::rl_info(dirs::document_dir().as_deref(), install.as_deref(), core.rl_listener_connected());
        info.note = Some("Restart Rocket League for the change to take effect.".into());
        Ok(info)
    })
    .await
}
