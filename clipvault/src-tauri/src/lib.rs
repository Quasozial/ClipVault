pub mod app;
pub mod audio;
pub mod capture;
pub mod commands;
pub mod compress;
pub mod db;
pub mod events;
pub mod export;
pub mod game_watcher;
pub mod hotkeys;
pub mod import_moments;
pub mod library;
pub mod locks;
pub mod logging;
pub mod notify;
pub mod rescan;
pub mod rl_stats;
pub mod save;
pub mod settings;
pub mod thumbs;
pub mod tray;
pub mod waveform;

use app::tauri_ui::{HealthPayload, TauriUi};
use app::{AppCore, CoreConfig};
use hotkeys::tauri_backend::{register_all, HotkeyErrors, HotkeyMap};
use hotkeys::{HotkeyAction, HotkeyError};
use notify::{Ui, APP_TITLE};
use std::sync::Arc;
use std::time::SystemTime;
use tauri::{AppHandle, Emitter, Manager, RunEvent, State, WindowEvent};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_global_shortcut::ShortcutState;

/// Registers the hotkeys from `core`'s settings; failures become toasts and
/// are kept for the Settings UI.
pub fn apply_hotkeys(app: &AppHandle, core: &AppCore, ui: &dyn Ui) -> Vec<HotkeyError> {
    let errors = register_all(app, &core.settings());
    for e in &errors {
        ui.toast(APP_TITLE, &e.to_string());
    }
    errors
}

/// Command-line flag of the "start with Windows" registry entry: the app then
/// starts in the tray without showing its window.
pub const AUTOSTART_ARG: &str = "--autostart";

/// Whether this process was launched by the "start with Windows" entry.
pub fn started_by_autostart<S: AsRef<str>>(args: impl IntoIterator<Item = S>) -> bool {
    args.into_iter().any(|a| a.as_ref() == AUTOSTART_ARG)
}

/// What [`sync_autostart`] writes: `Some(true)` = (re-)register,
/// `Some(false)` = remove, `None` = leave the registry alone.
pub fn autostart_action(want: bool, current: Result<bool, String>) -> Option<bool> {
    match current {
        // Rewrite even when already registered: older builds registered the
        // entry without AUTOSTART_ARG (the window would open at sign-in).
        _ if want => Some(true),
        Ok(false) => None,
        _ => Some(false),
    }
}

/// Matches the Windows "start with Windows" entry (with [`AUTOSTART_ARG`]) to
/// the setting.
pub fn sync_autostart(app: &AppHandle, enabled: bool) {
    let al = app.autolaunch();
    let Some(on) = autostart_action(enabled, al.is_enabled().map_err(|e| e.to_string())) else { return };
    let r = if on { al.enable() } else { al.disable() };
    match r {
        Ok(()) => tracing::info!("start with Windows: {on}"),
        Err(e) => tracing::error!("start with Windows ({on}): {e}"),
    }
}

#[tauri::command]
fn get_health(core: State<'_, Arc<AppCore>>) -> HealthPayload {
    let target = app::plan::target_name(&core.target());
    HealthPayload::new(&core.health(), target.as_deref())
}

#[tauri::command]
fn get_hotkey_errors(errors: State<'_, HotkeyErrors>) -> Vec<HotkeyError> {
    errors.0.lock().unwrap().clone()
}

#[cfg(test)]
mod startup_tests {
    use super::*;

    #[test]
    fn autostart_flag_detected() {
        assert!(started_by_autostart(["clipvault.exe", "--autostart"]));
        assert!(!started_by_autostart(["clipvault.exe"]));
        assert!(!started_by_autostart(Vec::<String>::new()));
    }

    /// I8: an entry registered by an older build has no `--autostart`, so an
    /// enabled entry is always rewritten; a disabled one is only removed.
    #[test]
    fn autostart_entry_rewritten_when_enabled() {
        assert_eq!(autostart_action(true, Ok(true)), Some(true));
        assert_eq!(autostart_action(true, Ok(false)), Some(true));
        assert_eq!(autostart_action(true, Err("x".into())), Some(true));
        assert_eq!(autostart_action(false, Ok(false)), None);
        assert_eq!(autostart_action(false, Ok(true)), Some(false));
        assert_eq!(autostart_action(false, Err("x".into())), Some(false));
    }
}

/// The Settings page reports whether it is open (mic level meter).
#[tauri::command]
fn set_settings_open(core: State<'_, Arc<AppCore>>, open: bool) {
    core.set_settings_open(open);
}

/// FFmpeg path for commands that need it (Moments import thumbnails).
pub struct FfmpegPath(pub Option<std::path::PathBuf>);

/// Whether GG's Moments database exists on this machine (first-run prompt).
#[tauri::command]
fn moments_db_available() -> bool {
    import_moments::moments_db_available()
}

#[derive(Clone, serde::Serialize)]
struct ImportProgress {
    done: u32,
    total: u32,
}

/// Imports the Moments library. Runs on a blocking thread with its own library
/// connection (so saves are not held up), emitting `import-progress`.
#[tauri::command]
async fn import_moments(
    app: AppHandle,
    core: State<'_, Arc<AppCore>>,
    ffmpeg: State<'_, FfmpegPath>,
    generate_thumbnails: Option<bool>,
) -> Result<import_moments::ImportReport, String> {
    let running = import_moments::begin_import().ok_or("An import is already running")?;
    let core = core.inner().clone();
    let ffmpeg = ffmpeg.0.clone();
    let generate = generate_thumbnails.unwrap_or(true) && ffmpeg.is_some();
    tauri::async_runtime::spawn_blocking(move || -> anyhow::Result<import_moments::ImportReport> {
        let _running = running;
        let dir = app::app_data_dir();
        let conn = db::open(&dir.join("library.db"))?;
        let mut settings = core.settings(); // scratch copy for the importer; not applied
        let options = import_moments::ImportOptions {
            generate_thumbnails: generate,
            thumbs_dir: dir.join("thumbs"),
            thumbnail_limit: None,
        };
        let report = import_moments::import_moments(
            &import_moments::default_gg_db(),
            &conn,
            &mut settings,
            ffmpeg.as_deref().unwrap_or_else(|| std::path::Path::new("ffmpeg.exe")),
            &options,
            |done, total| {
                let _ = app.emit("import-progress", ImportProgress { done, total });
            },
        )?;
        // The import can take minutes: merge onto the settings as they are now,
        // so changes made meanwhile are kept. A failure must not lose the report.
        if let Some(merged) = import_moments::merge_imported_settings(&core.settings(), &report.imported_settings) {
            if let Err(e) = core.apply_settings(merged) {
                tracing::error!("[moments] could not apply imported settings: {e:#}");
            }
        }
        // Imported clips may live in folders the webview may not read yet.
        commands::refresh_asset_scope(&app, &core);
        Ok(report)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("{e:#}"))
}

fn on_hotkey(app: &AppHandle, id: u32, t_press: SystemTime) {
    let Some(action) = app.state::<HotkeyMap>().0.lock().unwrap().get(&id).copied() else { return };
    let Some(core) = app.try_state::<Arc<AppCore>>() else { return };
    tracing::info!("hotkey: {}", action.label());
    match action {
        HotkeyAction::SaveClip => core.request_save(t_press),
        HotkeyAction::Mark => core.mark(t_press),
        HotkeyAction::ToggleDesktop => {
            core.toggle_desktop();
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Dropped (flushed) at RunEvent::Exit: Tauri ends the process without
    // returning from run(), so a guard local to this function never flushes.
    let log_guard = std::sync::Mutex::new(logging::init(&app::app_data_dir().join("logs")));
    logging::install_panic_hook();
    let autostarted = started_by_autostart(std::env::args());
    tracing::info!("ClipVault {} starting{}", env!("CARGO_PKG_VERSION"), if autostarted { " (autostart)" } else { "" });
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf()));
    let ffmpeg = app::plan::resolve_ffmpeg(
        std::env::var_os("CLIPVAULT_FFMPEG"),
        exe_dir.as_deref(),
        std::env::var_os("PATH"),
    );
    tracing::info!("FFmpeg: {ffmpeg:?}");

    let shortcuts = tauri_plugin_global_shortcut::Builder::new()
        .with_handler(|app, shortcut, event| {
            // First thing: the press time a save is measured from.
            let t_press = SystemTime::now();
            if event.state == ShortcutState::Pressed {
                on_hotkey(app, shortcut.id(), t_press);
            }
        })
        .build();

    let app = tauri::Builder::default()
        // First plugin (its docs): a second ClipVault exits right here, before
        // it could wipe the buffer, grab the hotkeys or start a second capture;
        // the running one shows its window instead.
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            tracing::info!("second instance started ({args:?})");
            if !started_by_autostart(&args) {
                tray::show_main_window(app);
            }
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec![AUTOSTART_ARG]),
        ))
        .plugin(shortcuts)
        .manage(HotkeyMap::default())
        .manage(HotkeyErrors::default())
        .manage(tray::TrayHandle::default())
        .manage(FfmpegPath(ffmpeg.clone()))
        .setup(move |app| {
            let handle = app.handle().clone();
            let ui = Arc::new(TauriUi { app: handle.clone() });
            let core = AppCore::new(CoreConfig::for_app(ffmpeg), ui.clone())?;
            app.manage(core.clone());
            {
                let h = handle.clone();
                core.compressor().set_on_update(Arc::new(move |clip| {
                    let _ = h.emit("clip-updated", clip);
                }));
            }
            commands::refresh_asset_scope(&handle, &core);
            tray::build(&handle, core.clone())?;
            apply_hotkeys(&handle, &core, ui.as_ref());
            sync_autostart(&handle, core.settings().start_with_windows);
            let c = core.clone();
            tauri::async_runtime::spawn(async move {
                c.start(Box::new(game_watcher::SysinfoProcesses::new()));
            });
            // The window is created hidden (tauri.conf.json); started with
            // Windows it stays in the tray.
            if !autostarted {
                tray::show_main_window(&handle);
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // Tray app: closing the window hides it; Quit is in the tray menu.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
                // A hidden page cannot tell (WebView2 stays "visible"): stop the
                // Settings mic meter here and let the UI know (tray::show_main_window sends `true`).
                if let Some(core) = window.app_handle().try_state::<Arc<AppCore>>() {
                    core.set_settings_open(false);
                }
                let _ = window.app_handle().emit("window-visibility", tray::WindowVisibility { visible: false });
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_health,
            get_hotkey_errors,
            set_settings_open,
            moments_db_available,
            import_moments,
            commands::list_clips,
            commands::get_clip,
            commands::update_clip_meta,
            commands::set_trim,
            commands::export_trimmed,
            commands::delete_clip,
            commands::set_starred,
            commands::export_clip,
            commands::reveal_in_folder,
            commands::ensure_thumbnail,
            commands::audio_tracks,
            commands::list_games,
            commands::set_game_enabled,
            commands::list_running_processes,
            commands::add_game_from_process,
            commands::get_settings,
            commands::save_settings,
            commands::list_displays,
            commands::list_encoders,
            commands::compress_clip,
            commands::list_audio_devices,
            commands::rl_stats_status,
            commands::rl_enable_stats
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(move |app, event| {
        if let RunEvent::Exit = event {
            if let Some(core) = app.try_state::<Arc<AppCore>>() {
                tauri::async_runtime::block_on(core.shutdown());
            }
            tracing::info!("ClipVault stopped");
            drop(locks::lock(&log_guard).take());
        }
    });
}
