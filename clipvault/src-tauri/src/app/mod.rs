//! AppCore: owns the services and wires them together, independent of Tauri
//! (feedback goes through [`Ui`]), so the whole pipeline runs in tests.
//!
//! Tasks started by [`AppCore::start`]:
//! * control loop (`control.rs`): owns the [`GameWatcher`], the process source
//!   and the [`CaptureController`]; ticks every 2 s and handles commands
//!   (re-tick, settings restart, missing output, shutdown);
//! * health loop: supervisor events ⇒ UI/tray, deduped toasts, monitor fallback;
//! * levels loop: mic level windows ⇒ laugh markers and the `mic-level` event.
//!
//! Module map: `plan` (pure decisions), `health` (pure toast/tray logic),
//! `capture_ctl` (capture lifecycle), `control` (the loops), `tauri_ui` (the
//! Tauri implementation of [`Ui`]).
pub mod capture_ctl;
mod control;
pub mod health;
pub mod plan;
pub mod tauri_ui;

use crate::audio::pipes::PipeWriter;
use crate::audio::sources::{AudioSource, LevelCallback, SourceKind};
use crate::capture::args::VideoSource;
use crate::capture::kick::DesktopKicker;
use crate::capture::supervisor::{Health, Supervisor, SupervisorEvent};
use crate::db;
use crate::db::clips::MarkerKind;
use crate::events::{BusEvent, EventBus};
use crate::locks;
use crate::game_watcher::{CaptureTarget, ProcessSource};
use crate::notify::{self, Sound, Ui, APP_TITLE};
use crate::save::{CaptureInfo, SaveConfig, SaveOutcome, SaveService};
use crate::settings::{Encoder, Settings};
use anyhow::Context;
use capture_ctl::Level;
use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, SystemTime};
use tokio::runtime::Handle;
use tokio::sync::{broadcast, mpsc, oneshot};

/// How long Quit waits for a running save (concat normally takes under 1 s).
const SHUTDOWN_SAVE_WAIT: Duration = Duration::from_secs(15);
/// How long Quit waits for capture to stop. The control loop can be stuck
/// (startup probes, a WASAPI thread on a removed device); FFmpeg dies with
/// the process anyway through its kill-on-close job.
const SHUTDOWN_CAPTURE_WAIT: Duration = Duration::from_secs(10);

pub struct CoreConfig {
    /// `%LOCALAPPDATA%\ClipVault`: settings.json, library.db, thumbs\.
    pub data_dir: PathBuf,
    /// `None` ⇒ health `Failing("FFmpeg not found")`, saves fail.
    pub ffmpeg: Option<PathBuf>,
    /// Parent of per-save temp dirs. `None` (the app): `<buffer_dir>\ClipVault-save`,
    /// on the segments' volume so they are hard-linked rather than copied.
    pub temp_root: Option<PathBuf>,
    pub video_source: VideoSource,
    /// Install the desktop kick (ddagrab on a static desktop, ruling R8).
    pub desktop_kick: bool,
}

impl CoreConfig {
    /// The real app: `%LOCALAPPDATA%\ClipVault`, ddagrab, desktop kick.
    pub fn for_app(ffmpeg: Option<PathBuf>) -> Self {
        CoreConfig {
            data_dir: app_data_dir(),
            ffmpeg,
            temp_root: None,
            video_source: VideoSource::Ddagrab,
            desktop_kick: true,
        }
    }
}

pub fn app_data_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("ClipVault")
}

/// Mic-only capture for the Settings level meter while idle.
struct MicProbe {
    device: Option<String>,
    _source: AudioSource,
}

impl MicProbe {
    fn start(device: Option<String>, ui: Arc<dyn Ui>) -> anyhow::Result<MicProbe> {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        // A pipe nobody connects to: the source just discards what it writes.
        let name = format!("clipvault-micprobe-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed));
        let writer = PipeWriter::new(&name)?;
        let on_level: LevelCallback = Box::new(move |rms, _at| ui.mic_level(control::rms_to_dbfs(rms)));
        let source = AudioSource::start(SourceKind::Mic { device: device.clone() }, writer, Some(on_level))?;
        Ok(MicProbe { device, _source: source })
    }
}

pub(crate) enum Cmd {
    Retick,
    SettingsChanged,
    OutputMissing,
    Shutdown(oneshot::Sender<()>),
}

pub struct AppCore {
    cfg: CoreConfig,
    settings_path: PathBuf,
    settings: RwLock<Settings>,
    db: Arc<Mutex<Connection>>,
    bus: Arc<EventBus>,
    supervisor: Arc<Supervisor>,
    sup_events: broadcast::Sender<SupervisorEvent>,
    save: Arc<SaveService>,
    ui: Arc<dyn Ui>,
    target: RwLock<CaptureTarget>,
    health: RwLock<Health>,
    settings_open: AtomicBool,
    /// Mic-only audio source feeding `mic-level` while Settings is open and
    /// nothing is being captured (see [`AppCore::refresh_mic_probe`]).
    mic_probe: Mutex<Option<MicProbe>>,
    /// The Rocket League stats listener has a socket to the game.
    rl_connected: Arc<AtomicBool>,
    cmd_tx: mpsc::UnboundedSender<Cmd>,
    cmd_rx: Mutex<Option<mpsc::UnboundedReceiver<Cmd>>>,
    level_tx: mpsc::Sender<Level>,
    level_rx: Mutex<Option<mpsc::Receiver<Level>>>,
    rt: OnceLock<Handle>,
    /// Video encoders this PC can run, best first; set once by the control
    /// loop before the first capture (see `capture::encoders`).
    encoders: OnceLock<Vec<Encoder>>,
    /// Background AV1 re-encoding of saved clips.
    compressor: Arc<crate::compress::Compressor>,
    /// Set when the library was restored or recreated at open: the capture
    /// folder is rescanned on start for clips the library lost (see `rescan`).
    /// Inner value: time of the restored backup (`None`: no usable backup).
    rescan_after_restore: Mutex<Option<Option<SystemTime>>>,
    _kicker: Option<Arc<DesktopKicker>>,
}

impl AppCore {
    /// Loads settings (a corrupt file is reset with a toast), opens the library
    /// and builds the services. Nothing runs until [`AppCore::start`].
    pub fn new(cfg: CoreConfig, ui: Arc<dyn Ui>) -> anyhow::Result<Arc<AppCore>> {
        let settings_path = cfg.data_dir.join("settings.json");
        let (settings, notice) = Settings::load_or_reset(&settings_path);
        if let Some(n) = notice {
            ui.toast(APP_TITLE, &n);
        }
        let db_path = cfg.data_dir.join("library.db");
        let backups = cfg.data_dir.join("backups");
        let (conn, notice) =
            db::open_library(&db_path, &backups).with_context(|| format!("opening library {}", db_path.display()))?;
        // Before today's backup is written: the restored backup's own time.
        let rescan_after_restore = notice.as_ref().map(|_| {
            let empty = conn.query_row("SELECT COUNT(*) FROM clips", [], |r| r.get::<_, i64>(0)).unwrap_or(0) == 0;
            if empty { None } else { db::newest_backup_time(&backups) }
        });
        if let Some(n) = notice {
            ui.toast(APP_TITLE, &n);
        }
        db::backup_daily(&conn, &backups);
        // No save runs yet: leftovers of saves cut short by a crash or power
        // loss (also in %TEMP%, where older versions put them).
        for root in [save_temp_root(&cfg, &settings), std::env::temp_dir()] {
            let n = crate::save::clean_stale_save_dirs(&root);
            if n > 0 {
                tracing::info!("removed {n} stale save temp folder(s) in {}", root.display());
            }
        }
        let db = Arc::new(Mutex::new(conn));
        let max = Duration::from_secs(settings.max_save_length_s as u64);
        let bus = Arc::new(EventBus::new(max));
        let (sup_events, _) = broadcast::channel(256);
        let ffmpeg = cfg.ffmpeg.clone().unwrap_or_else(|| PathBuf::from("ffmpeg.exe"));
        let supervisor = Arc::new(Supervisor::new(ffmpeg, segment_retention(&settings), sup_events.clone()));
        let kicker = if cfg.desktop_kick && cfg.video_source == VideoSource::Ddagrab {
            let k = DesktopKicker::start();
            if let Some(k) = &k {
                let k = k.clone();
                supervisor.set_kick(Arc::new(move |idx| k.kick(idx)));
            }
            k
        } else {
            None
        };
        let save = Arc::new(SaveService::new(
            save_config(&cfg, &settings),
            CaptureInfo { fps: settings.fps, width: 0, height: 0, track_titles: Vec::new(), label: "Desktop".into() },
            supervisor.clone(),
            db.clone(),
            bus.clone(),
        ));
        let compressor = crate::compress::Compressor::new(cfg.ffmpeg.clone(), db.clone());
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (level_tx, level_rx) = mpsc::channel(64);
        tracing::info!("settings {}, library {}", settings_path.display(), db_path.display());
        Ok(Arc::new(AppCore {
            cfg,
            settings_path,
            settings: RwLock::new(settings),
            db,
            bus,
            supervisor,
            sup_events,
            save,
            ui,
            target: RwLock::new(CaptureTarget::None),
            health: RwLock::new(Health::Idle),
            settings_open: AtomicBool::new(false),
            mic_probe: Mutex::new(None),
            rl_connected: Arc::new(AtomicBool::new(false)),
            cmd_tx,
            cmd_rx: Mutex::new(Some(cmd_rx)),
            level_tx,
            level_rx: Mutex::new(Some(level_rx)),
            rt: OnceLock::new(),
            encoders: OnceLock::new(),
            compressor,
            rescan_after_restore: Mutex::new(rescan_after_restore),
            _kicker: kicker,
        }))
    }

    /// Starts the control, health and levels loops on the current tokio
    /// runtime. `procs` is owned by the control loop (it may be `!Sync`).
    pub fn start(self: &Arc<Self>, procs: Box<dyn ProcessSource + Send>) {
        let rt = self.rt.get_or_init(Handle::current).clone();
        if self.cfg.ffmpeg.is_none() {
            let h = Health::Failing { reason: "FFmpeg not found".into() };
            *locks::write(&self.health) = h.clone();
            self.ui.health_changed(&h, None);
            self.ui.toast(APP_TITLE, "Capture failing: FFmpeg not found");
            tracing::error!("FFmpeg not found (CLIPVAULT_FFMPEG, sidecar, PATH)");
        }
        let cmd_rx = locks::lock(&self.cmd_rx).take().expect("AppCore::start called twice");
        let level_rx = locks::lock(&self.level_rx).take().expect("AppCore::start called twice");
        rt.spawn(control::control_loop(self.clone(), procs, cmd_rx));
        rt.spawn(control::health_loop(self.clone(), self.sup_events.subscribe()));
        rt.spawn(control::levels_loop(self.clone(), level_rx));
        rt.spawn(backup_loop(self.db.clone(), self.cfg.data_dir.join("backups")));
        if let Some(since) = locks::lock(&self.rescan_after_restore).take() {
            self.spawn_rescan(since);
        }
    }

    /// Re-adds clips in the capture folder the restored library lost.
    fn spawn_rescan(self: &Arc<Self>, since: Option<SystemTime>) {
        let Some(ffmpeg) = self.cfg.ffmpeg.clone() else { return };
        let me = self.clone();
        let dir = self.settings().capture_dir.clone();
        self.spawn(async move {
            let db = me.db.clone();
            let found = tokio::task::spawn_blocking(move || crate::rescan::rescan(&db, &ffmpeg, &dir, since, since.is_some()))
                .await;
            match found {
                Ok(Ok(0)) => tracing::info!("[rescan] no clips missing from the restored library"),
                Ok(Ok(n)) => {
                    tracing::info!("[rescan] {n} clip(s) re-added");
                    me.ui.toast(APP_TITLE, &format!("{n} clip(s) saved after the backup were found and added back to the library."));
                }
                Ok(Err(e)) => tracing::error!("[rescan] failed: {e:#}"),
                Err(e) => tracing::error!("[rescan] task failed: {e}"),
            }
        });
    }

    /// Lets running saves finish (bounded), then stops capture (FFmpeg,
    /// audio, RL listener) and the control loop.
    pub async fn shutdown(&self) {
        if !self.save.drain(SHUTDOWN_SAVE_WAIT).await {
            tracing::warn!("quitting while a save is still running (waited {} s)", SHUTDOWN_SAVE_WAIT.as_secs());
        }
        let (tx, rx) = oneshot::channel();
        let stopped = if self.cmd_tx.send(Cmd::Shutdown(tx)).is_ok() {
            tokio::time::timeout(SHUTDOWN_CAPTURE_WAIT, async {
                let _ = rx.await;
            })
            .await
        } else {
            tokio::time::timeout(SHUTDOWN_CAPTURE_WAIT, self.supervisor.stop()).await
        };
        if stopped.is_err() {
            tracing::warn!("capture did not stop within {} s; quitting anyway", SHUTDOWN_CAPTURE_WAIT.as_secs());
        }
    }

    fn spawn<F: std::future::Future<Output = ()> + Send + 'static>(&self, f: F) {
        match self.rt.get() {
            Some(rt) => drop(rt.spawn(f)),
            None => drop(tauri::async_runtime::spawn(f)),
        }
    }

    // ----- hotkey / tray actions -----

    /// Save hotkey: `t_press` was taken first thing in the callback. Never blocks.
    /// The press always ends in exactly one notification, even if the save
    /// path panics (see [`AppCore::save_guarded`]).
    pub fn request_save(self: &Arc<Self>, t_press: SystemTime) {
        let me = self.clone();
        let game = self.game_name();
        self.spawn(async move {
            me.save_guarded(t_press, game).await;
        });
    }

    /// Runs the save as its own task and watches it: a panic anywhere in the
    /// save path becomes an error toast + sound instead of a silent press.
    /// `None` when the save task panicked (or was cancelled).
    async fn save_guarded(self: Arc<Self>, t_press: SystemTime, game: String) -> Option<SaveOutcome> {
        let me = self.clone();
        match tokio::spawn(async move { me.save_as(t_press, &game).await }).await {
            Ok(outcome) => Some(outcome),
            Err(e) => {
                tracing::error!("save task failed: {e}");
                notify::error(self.ui.as_ref(), "internal error while saving – see log");
                None
            }
        }
    }

    /// The save path behind the hotkey: save, then exactly one notification.
    pub async fn save_and_notify(&self, t_press: SystemTime) -> SaveOutcome {
        let game = self.game_name();
        self.save_as(t_press, &game).await
    }

    async fn save_as(&self, t_press: SystemTime, game: &str) -> SaveOutcome {
        let ms = t_press.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_millis());
        tracing::info!("save requested: {game}, t_press={ms} ms");
        let outcome = if self.cfg.ffmpeg.is_none() {
            SaveOutcome::Failed { reason: "FFmpeg not found".into() }
        } else {
            self.save.save(t_press, game).await
        };
        let ui = self.ui.as_ref();
        match &outcome {
            SaveOutcome::Saved { clip, gap, short } => {
                tracing::info!("save ok: {} ({:.1} s, gap={gap}, short={short})", clip.path.display(), clip.duration_s);
                notify::saved(ui, clip, *gap, *short);
                if self.settings().compress_clips {
                    self.compressor.enqueue(&clip.id);
                }
            }
            SaveOutcome::NotRecording => {
                tracing::info!("save: not recording");
                notify::not_recording(ui);
            }
            SaveOutcome::Failed { reason } => {
                tracing::error!("save FAILED: {reason}");
                notify::error(ui, reason);
            }
        }
        outcome
    }

    /// Mark hotkey: a manual marker at `at` plus a short tick.
    pub fn mark(&self, at: SystemTime) {
        self.bus.push(BusEvent { at, kind: MarkerKind::Manual, label: "Marker".into() });
        self.ui.play(Sound::Mark);
        tracing::info!("manual marker");
    }

    /// Toggle-desktop hotkey / tray item. Returns the new state.
    pub fn toggle_desktop(&self) -> bool {
        let on = {
            let mut s = locks::write(&self.settings);
            s.desktop_capture = !s.desktop_capture;
            if let Err(e) = s.save(&self.settings_path) {
                tracing::error!("saving settings: {e:#}");
            }
            s.desktop_capture
        };
        tracing::info!("desktop capture {}", if on { "on" } else { "off" });
        self.ui.toast(APP_TITLE, if on { "Desktop capture on" } else { "Desktop capture off" });
        self.ui.desktop_capture_changed(on);
        self.ui.settings_changed();
        let _ = self.cmd_tx.send(Cmd::Retick);
        on
    }

    // ----- settings -----

    pub fn settings(&self) -> Settings {
        locks::read(&self.settings).clone()
    }

    /// Persists `new` and applies it: SaveService config live; capture restarts
    /// only for capture-relevant changes; desktop toggle on the next tick.
    /// (Hotkeys and autostart are re-applied by the Tauri layer.)
    pub fn apply_settings(&self, new: Settings) -> anyhow::Result<()> {
        self.update_settings(|_| Ok(Some(new)))
    }

    /// Read-modify-write of the settings: `f` gets the current settings and
    /// returns the new ones (`None` = no change). The file and the in-memory
    /// copy are updated under one write lock, so concurrent writers (Settings
    /// page, desktop hotkey, Moments merge) can neither lose an update nor
    /// leave file and memory disagreeing.
    pub fn update_settings(&self, f: impl FnOnce(&Settings) -> anyhow::Result<Option<Settings>>) -> anyhow::Result<()> {
        let (old, new) = {
            let mut cur = locks::write(&self.settings);
            let Some(new) = f(&cur)? else { return Ok(()) };
            new.save(&self.settings_path)?;
            (std::mem::replace(&mut *cur, new.clone()), new)
        };
        self.save.set_config(save_config(&self.cfg, &new));
        if old.max_save_length_s != new.max_save_length_s {
            // Live: a longer maximum keeps more buffer from now on (no restart,
            // no wiped buffer); a shorter one prunes at the next segment.
            tracing::info!("max save length {} s -> {} s", old.max_save_length_s, new.max_save_length_s);
            self.supervisor.set_retention(segment_retention(&new));
            self.bus.set_max_save_length(Duration::from_secs(new.max_save_length_s as u64));
        }
        if old.capture_differs(&new) {
            tracing::info!("capture-relevant settings changed: restarting capture");
            let _ = self.cmd_tx.send(Cmd::SettingsChanged);
        }
        if old.desktop_capture != new.desktop_capture {
            self.ui.desktop_capture_changed(new.desktop_capture);
            let _ = self.cmd_tx.send(Cmd::Retick);
        }
        if new.compress_clips && !old.compress_clips {
            self.compressor.enqueue_pending();
        }
        self.refresh_mic_probe(); // the mic device may have changed
        self.ui.settings_changed();
        Ok(())
    }

    /// While the Settings page is open, mic levels are emitted (≤ 10 Hz). When
    /// nothing is being captured a temporary mic-only source supplies them.
    pub fn set_settings_open(&self, open: bool) {
        self.settings_open.store(open, Ordering::Relaxed);
        self.refresh_mic_probe();
    }

    /// Makes the temporary mic source match the situation: running iff the
    /// Settings page is open, capture is idle (no target) and the mic is
    /// enabled; restarted when the mic device changes. Levels go straight to
    /// the UI (no pipe client, no FFmpeg, no laugh detection). Never blocks on
    /// joining the old source's thread.
    pub(crate) fn refresh_mic_probe(&self) {
        let s = self.settings();
        let want = self.settings_open.load(Ordering::Relaxed) && self.target() == CaptureTarget::None && s.mic_enabled;
        let mut slot = locks::lock(&self.mic_probe);
        if want && slot.as_ref().is_some_and(|p| p.device == s.mic_device) {
            return;
        }
        if let Some(old) = slot.take() {
            std::thread::spawn(move || drop(old));
        }
        if want {
            match MicProbe::start(s.mic_device.clone(), self.ui.clone()) {
                Ok(p) => *slot = Some(p),
                Err(e) => tracing::warn!("mic level meter could not start: {e:#}"),
            }
        }
    }

    /// Whether the Rocket League stats listener is connected to the game.
    pub fn rl_listener_connected(&self) -> bool {
        self.rl_connected.load(Ordering::Relaxed)
    }

    /// The thumbnails folder inside the data dir.
    pub fn thumbs_dir(&self) -> PathBuf {
        self.cfg.data_dir.join("thumbs")
    }

    // ----- state -----

    pub fn health(&self) -> Health {
        locks::read(&self.health).clone()
    }

    pub fn target(&self) -> CaptureTarget {
        locks::read(&self.target).clone()
    }

    /// Clip game name for the current target (`Desktop` when idle, unused then).
    pub fn game_name(&self) -> String {
        plan::target_name(&self.target()).unwrap_or_else(|| "Desktop".into())
    }

    pub fn db(&self) -> Arc<Mutex<Connection>> {
        self.db.clone()
    }

    pub fn bus(&self) -> Arc<EventBus> {
        self.bus.clone()
    }

    pub fn supervisor(&self) -> Arc<Supervisor> {
        self.supervisor.clone()
    }

    /// Video encoders this PC can run, best first (empty until probed, or when
    /// FFmpeg cannot run).
    pub fn available_encoders(&self) -> Vec<Encoder> {
        self.encoders.get().cloned().unwrap_or_default()
    }

    /// Probes the encoders once (blocking thread); later calls return the result.
    pub(crate) async fn probe_encoders(&self) -> Vec<Encoder> {
        if let Some(e) = self.encoders.get() {
            return e.clone();
        }
        let found = match self.cfg.ffmpeg.clone() {
            Some(ff) => tokio::task::spawn_blocking(move || crate::capture::encoders::probe_all(&ff))
                .await
                .unwrap_or_default(),
            None => Vec::new(),
        };
        self.encoders.get_or_init(|| found).clone()
    }

    pub fn compressor(&self) -> Arc<crate::compress::Compressor> {
        self.compressor.clone()
    }
}

/// Segments are kept for `max_save_length + 10 s`.
fn segment_retention(s: &Settings) -> Duration {
    Duration::from_secs(s.max_save_length_s as u64 + 10)
}

/// Folder for per-save temp dirs (see [`CoreConfig::temp_root`]).
fn save_temp_root(cfg: &CoreConfig, s: &Settings) -> PathBuf {
    cfg.temp_root.clone().unwrap_or_else(|| s.buffer_dir.join(crate::settings::SAVE_TEMP_SUBDIR))
}

fn save_config(cfg: &CoreConfig, s: &Settings) -> SaveConfig {
    SaveConfig {
        ffmpeg: cfg.ffmpeg.clone().unwrap_or_else(|| PathBuf::from("ffmpeg.exe")),
        capture_dir: s.capture_dir.clone(),
        thumbs_dir: cfg.data_dir.join("thumbs"),
        temp_root: save_temp_root(cfg, s),
        save_length: Duration::from_secs(s.effective_save_length_s() as u64),
    }
}

/// How often the backup loop checks for a missing daily backup.
const BACKUP_CHECK: Duration = Duration::from_secs(3600);

/// Writes the daily library backup while the app keeps running (a tray app
/// may run for weeks; the startup backup alone would go stale). One per day:
/// [`db::backup_daily`] skips a day that has one.
async fn backup_loop(db: Arc<Mutex<Connection>>, dir: PathBuf) {
    let mut tick = tokio::time::interval(BACKUP_CHECK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await; // the first tick is immediate; startup already backed up
    loop {
        tick.tick().await;
        let (db, dir) = (db.clone(), dir.clone());
        let _ = tokio::task::spawn_blocking(move || db::backup_daily(&locks::lock(&db), &dir)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    pub(crate) struct RecUi(pub Mutex<Vec<String>>);

    impl Ui for RecUi {
        fn toast(&self, _title: &str, body: &str) {
            self.0.lock().unwrap().push(format!("toast: {body}"));
        }
        fn play(&self, sound: Sound) {
            self.0.lock().unwrap().push(format!("sound: {sound:?}"));
        }
        fn health_changed(&self, _health: &Health, _target: Option<&str>) {}
        fn clip_saved(&self, _clip: &crate::db::clips::Clip) {}
        fn mic_level(&self, _dbfs: f32) {}
        fn desktop_capture_changed(&self, _on: bool) {}
        fn settings_changed(&self) {
            self.0.lock().unwrap().push("settings-changed".into());
        }
    }

    /// I9: every settings change outside the page (tray / hotkey toggle,
    /// import merge via apply_settings) tells the UI.
    #[test]
    fn settings_changes_are_announced() {
        let dir = tempfile::tempdir().unwrap();
        let (core, ui) = test_core(dir.path());
        core.toggle_desktop();
        assert!(ui.0.lock().unwrap().contains(&"settings-changed".to_string()));
        ui.0.lock().unwrap().clear();
        let mut s = core.settings();
        s.capture_dir = dir.path().join("imported");
        core.apply_settings(s).unwrap();
        assert_eq!(ui.0.lock().unwrap().clone(), ["settings-changed"]);
    }

    /// An AppCore on a temp dir (never the real data dir or clip folder),
    /// without starting its loops. FFmpeg "exists" but is never run.
    pub(crate) fn test_core(dir: &std::path::Path) -> (Arc<AppCore>, Arc<RecUi>) {
        let data = dir.join("data");
        let mut s = Settings::default();
        s.buffer_dir = dir.join("buffer");
        s.capture_dir = dir.join("clips");
        s.save(&data.join("settings.json")).unwrap();
        let ui = Arc::new(RecUi::default());
        let cfg = CoreConfig {
            data_dir: data,
            ffmpeg: Some(dir.join("ffmpeg.exe")),
            temp_root: Some(dir.join("tmp")),
            video_source: VideoSource::TestSrc,
            desktop_kick: false,
        };
        (AppCore::new(cfg, ui.clone()).unwrap(), ui)
    }

    /// I6: raising the maximum save length takes effect without a restart.
    #[test]
    fn max_save_length_applies_live() {
        let dir = tempfile::tempdir().unwrap();
        let (core, _) = test_core(dir.path());
        let secs = Duration::from_secs;
        assert_eq!(core.supervisor.retention(), secs(130));
        let mut s = core.settings();
        s.max_save_length_s = 300;
        core.apply_settings(s).unwrap();
        assert_eq!(core.supervisor.retention(), secs(310));
        assert_eq!(locks::lock(&core.supervisor.segments()).retention(), secs(310));
        assert_eq!(core.bus.retention(), secs(310));
    }

    /// C1: a panic in the save path still ends in a loud error, and the next
    /// save works.
    #[tokio::test(flavor = "multi_thread")]
    async fn panic_in_save_is_reported_and_next_save_works() {
        let dir = tempfile::tempdir().unwrap();
        let (core, ui) = test_core(dir.path());
        core.supervisor.set_health_for_test(Health::Restarting { reason: "test".into() });
        core.save.panic_next_save.store(true, Ordering::SeqCst);
        let out = tokio::spawn(core.clone().save_guarded(SystemTime::now(), "G".into())).await;
        assert!(matches!(out, Ok(None)), "panic must be caught: {out:?}");
        assert_eq!(
            ui.0.lock().unwrap().clone(),
            ["toast: Clip NOT saved: internal error while saving – see log", "sound: Error"]
        );
        ui.0.lock().unwrap().clear();
        match core.clone().save_guarded(SystemTime::now(), "G".into()).await {
            Some(SaveOutcome::Failed { reason }) => assert!(reason.contains("no recorded video"), "{reason}"),
            o => panic!("{o:?}"),
        }
        assert_eq!(ui.0.lock().unwrap().len(), 2, "exactly one toast + sound");
    }
}
