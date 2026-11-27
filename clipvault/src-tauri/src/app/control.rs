//! The AppCore background loops.
use super::capture_ctl::{CaptureController, Level};
use super::health::{is_missing_output, HealthToaster};
use super::{AppCore, Cmd};
use crate::audio::laugh::LaughDetector;
use crate::capture::supervisor::SupervisorEvent;
use crate::db;
use crate::db::clips::MarkerKind;
use crate::db::games::Game;
use crate::events::BusEvent;
use crate::locks;
use crate::game_watcher::{CaptureTarget, GameWatcher, ProcessSource};
use crate::notify::APP_TITLE;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};
use tokio::time::{interval, MissedTickBehavior};

/// Game detection poll.
pub const WATCH_INTERVAL: Duration = Duration::from_secs(2);
const HEALTH_POLL: Duration = Duration::from_millis(250);
/// Level reported for digital silence (log10(0) = -inf).
const SILENCE_DBFS: f32 = -120.0;

/// Linear RMS (0..1) to dBFS; digital silence is -120.
pub(super) fn rms_to_dbfs(rms: f32) -> f32 {
    if rms > 0.0 { 20.0 * rms.log10() } else { SILENCE_DBFS }
}

fn games(core: &AppCore) -> Vec<Game> {
    db::games::list(&locks::lock(&core.db)).unwrap_or_else(|e| {
        tracing::error!("listing games: {e:#}");
        Vec::new()
    })
}

/// Synchronous so the `!Sync` process source is never held across an await.
fn watch(core: &AppCore, w: &mut GameWatcher, procs: &dyn ProcessSource, games: &[Game]) -> Option<CaptureTarget> {
    let desktop = locks::read(&core.settings).desktop_capture;
    w.tick(Instant::now(), games, desktop, procs)
}

pub(super) async fn control_loop(
    core: Arc<AppCore>,
    procs: Box<dyn ProcessSource + Send>,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
) {
    // Before any save or compression: leftovers of an interrupted one (quit, crash).
    let capture_dir = core.settings().capture_dir;
    let db = core.db();
    let _ = tokio::task::spawn_blocking(move || {
        let clip_paths = crate::db::clips::all_paths(&locks::lock(&db)).unwrap_or_else(|e| {
            tracing::warn!("listing clip folders for temp cleanup: {e:#}");
            Vec::new()
        });
        for dir in crate::compress::temp_dirs(&capture_dir, &clip_paths) {
            crate::compress::clean_stale_temps(&dir);
        }
    })
    .await;
    // Before the first capture: which video encoders this PC can run.
    let encoders = core.probe_encoders().await;
    core.compressor.start(encoders.clone(), {
        let c = core.clone();
        // Re-encoding waits while a game is recorded, so it never costs frames.
        Arc::new(move || matches!(c.target(), CaptureTarget::Game { .. }))
    });
    if core.settings().compress_clips {
        core.compressor.enqueue_pending();
    }
    let mut ctl = CaptureController::new(
        core.supervisor.clone(),
        core.save.clone(),
        core.bus.clone(),
        core.ui.clone(),
        core.level_tx.clone(),
        core.cfg.video_source,
        core.cfg.ffmpeg.is_some(),
        core.rl_connected.clone(),
        encoders,
        core.hevc_playable(),
    );
    let mut watcher = GameWatcher::new();
    let mut tick = interval(WATCH_INTERVAL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        let retick = tokio::select! {
            _ = tick.tick() => true,
            cmd = rx.recv() => match cmd {
                Some(Cmd::Retick) => true,
                Some(Cmd::SettingsChanged) => {
                    let s = core.settings();
                    ctl.settings_changed(&s).await;
                    false
                }
                Some(Cmd::HevcPlayable(ok)) => {
                    let s = core.settings();
                    ctl.set_hevc_playable(ok, &s).await;
                    false
                }
                Some(Cmd::OutputMissing) => {
                    let s = core.settings();
                    ctl.output_missing(&s).await;
                    false
                }
                Some(Cmd::Shutdown(done)) => {
                    tracing::info!("shutting down capture");
                    ctl.stop().await;
                    let _ = done.send(());
                    return;
                }
                None => {
                    ctl.stop().await;
                    return;
                }
            },
        };
        if !retick {
            continue;
        }
        let games = games(&core);
        if let Some(t) = watch(&core, &mut watcher, procs.as_ref(), &games) {
            tracing::info!("capture target: {t:?}");
            *locks::write(&core.target) = t.clone();
            let idle = t == CaptureTarget::None;
            if !idle {
                core.refresh_mic_probe(); // capture's own mic source supplies levels now
            }
            let s = core.settings();
            ctl.set_target(t, &s, &games).await;
            if idle {
                core.refresh_mic_probe(); // Settings may still be open
            }
        } else {
            ctl.recheck_game_monitor(&core.settings()).await;
        }
    }
}

pub(super) async fn health_loop(core: Arc<AppCore>, mut rx: broadcast::Receiver<SupervisorEvent>) {
    let mut toaster = HealthToaster::default();
    let mut poll = interval(HEALTH_POLL);
    poll.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Ok(SupervisorEvent::HealthChanged(h)) => {
                    tracing::info!("health: {h:?}");
                    *locks::write(&core.health) = h.clone();
                    let target = super::plan::target_name(&core.target());
                    core.ui.health_changed(&h, target.as_deref());
                    if let Some(t) = toaster.on_health(&h, Instant::now()) {
                        tracing::warn!("{}", t.text());
                        core.ui.toast(APP_TITLE, &t.text());
                    }
                    if is_missing_output(&h) {
                        let _ = core.cmd_tx.send(Cmd::OutputMissing);
                    }
                }
                Ok(SupervisorEvent::SegmentClosed(_)) => {}
                Err(broadcast::error::RecvError::Lagged(n)) => tracing::warn!("health loop lagged {n} events"),
                Err(broadcast::error::RecvError::Closed) => return,
            },
            _ = poll.tick() => {
                if let Some(t) = toaster.poll(Instant::now()) {
                    tracing::info!("{}", t.text());
                    core.ui.toast(APP_TITLE, &t.text());
                }
            }
        }
    }
}

pub(super) async fn levels_loop(core: Arc<AppCore>, mut rx: mpsc::Receiver<Level>) {
    let mut det = LaughDetector::new(core.settings().laugh_threshold_dbfs);
    while let Some((rms, at)) = rx.recv().await {
        let dbfs = rms_to_dbfs(rms);
        let (enabled, threshold) = {
            let s = locks::read(&core.settings);
            (s.laugh_enabled, s.laugh_threshold_dbfs)
        };
        det.set_threshold(threshold);
        if enabled {
            if let Some(t) = det.feed_window(dbfs, at) {
                tracing::info!("laugh marker");
                core.bus.push(BusEvent { at: t, kind: MarkerKind::Laugh, label: "Laugh".into() });
            }
        }
        if core.settings_open.load(Ordering::Relaxed) {
            core.ui.mic_level(dbfs);
        }
    }
}
