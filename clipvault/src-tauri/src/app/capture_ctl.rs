//! Capture lifecycle for one target: display selection, audio layout, the
//! FFmpeg supervisor and the Rocket League stats listener.
//!
//! Owned by the control loop (one task), so no locking. Start order per
//! (re)start: resolve display ⇒ plan audio (pipes + sources only recreated when
//! the layout changes; pipe servers exist before FFmpeg spawns) ⇒
//! `SaveService::set_capture_info` ⇒ `Supervisor::start`.
use super::plan::{is_rocket_league, output_size, target_name};
use crate::audio::pipes::PipeWriter;
use crate::audio::sources::{plan_sources, process_loopback_supported, AudioSource, LevelCallback, SourceKind};
use crate::capture::args::{layout_key, AudioInput, AudioPipe, CaptureSpec, VideoSource};
use crate::capture::display::{list_displays, resolve_display, window_monitor};
use crate::capture::supervisor::{Health, Supervisor};
use crate::db::games::Game;
use crate::events::EventBus;
use crate::game_watcher::CaptureTarget;
use crate::notify::{Ui, APP_TITLE};
use crate::rl_stats;
use crate::save::{CaptureInfo, SaveService};
use crate::settings::{Encoder, Settings};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// One mic level window: linear RMS and window start.
pub type Level = (f32, SystemTime);

struct AudioLayout {
    plan: Vec<(SourceKind, &'static str)>,
    pipes: Vec<AudioPipe>,
    /// Dropped (threads joined) when the layout changes or capture stops.
    _sources: Vec<AudioSource>,
    /// Keep the pipe servers alive as long as the layout.
    _writers: Vec<PipeWriter>,
}

static PIPE_GEN: AtomicU64 = AtomicU64::new(0);

impl AudioLayout {
    fn create(plan: Vec<(SourceKind, &'static str)>, levels: &mpsc::Sender<Level>) -> anyhow::Result<AudioLayout> {
        let generation = PIPE_GEN.fetch_add(1, Ordering::Relaxed);
        let (mut pipes, mut writers, mut sources) = (Vec::new(), Vec::new(), Vec::new());
        for (i, (kind, title)) in plan.iter().enumerate() {
            let name = format!("clipvault-{}-{generation}-{i}", std::process::id());
            let w = PipeWriter::new(&name)?;
            let on_level: Option<LevelCallback> = (*title == "Mic").then(|| {
                let tx = levels.clone();
                // Capture thread: never block, drop the window if the consumer lags.
                Box::new(move |rms: f32, at: SystemTime| {
                    let _ = tx.try_send((rms, at));
                }) as LevelCallback
            });
            sources.push(AudioSource::start(kind.clone(), w.clone(), on_level)?);
            pipes.push(AudioPipe { pipe_name: name, title });
            writers.push(w);
        }
        Ok(AudioLayout { plan, pipes, _sources: sources, _writers: writers })
    }
}

pub struct CaptureController {
    pub(super) supervisor: Arc<Supervisor>,
    pub(super) save: Arc<SaveService>,
    pub(super) bus: Arc<EventBus>,
    pub(super) ui: Arc<dyn Ui>,
    pub(super) levels: mpsc::Sender<Level>,
    pub(super) video_source: VideoSource,
    pub(super) ffmpeg_found: bool,
    /// Shared with the UI: the stats listener has a socket to Rocket League.
    rl_connected: Arc<AtomicBool>,
    audio: Option<AudioLayout>,
    rl: Option<CancellationToken>,
    target: CaptureTarget,
    /// ddagrab output in use by the running capture.
    output_idx: Option<u32>,
    /// Set after FFmpeg could not open the selected output; cleared by a
    /// capture-relevant settings change.
    force_primary: bool,
    /// Video encoders this PC can run, best first (empty: unknown).
    encoders: Vec<Encoder>,
    /// Encoder notices already shown (once per process each).
    notices_shown: Vec<String>,
    /// The app's player can decode HEVC (else Auto avoids it).
    hevc_playable: bool,
}

impl CaptureController {
    pub fn new(
        supervisor: Arc<Supervisor>,
        save: Arc<SaveService>,
        bus: Arc<EventBus>,
        ui: Arc<dyn Ui>,
        levels: mpsc::Sender<Level>,
        video_source: VideoSource,
        ffmpeg_found: bool,
        rl_connected: Arc<AtomicBool>,
        encoders: Vec<Encoder>,
        hevc_playable: bool,
    ) -> Self {
        Self {
            supervisor,
            save,
            bus,
            ui,
            levels,
            video_source,
            ffmpeg_found,
            rl_connected,
            audio: None,
            rl: None,
            target: CaptureTarget::None,
            output_idx: None,
            force_primary: false,
            encoders,
            notices_shown: Vec::new(),
            hevc_playable,
        }
    }

    /// The player's HEVC support changed: restart a running capture whose
    /// encoder pick changes with it.
    pub async fn set_hevc_playable(&mut self, ok: bool, s: &Settings) {
        let pick = |c: &Self| crate::capture::encoders::resolve_for_playback(s.encoder, &c.encoders, c.hevc_playable).0;
        let before = pick(self);
        self.hevc_playable = ok;
        if pick(self) != before && self.target != CaptureTarget::None {
            tracing::info!("player HEVC support changed: restarting capture with {:?}", pick(self));
            self.start(s).await;
        }
    }

    /// The encoder recording uses for `wanted`, toasting (once each) when it is
    /// not the one picked or when it is the CPU.
    fn pick_encoder(&mut self, wanted: Encoder) -> Encoder {
        let (enc, notice) = crate::capture::encoders::resolve_for_playback(wanted, &self.encoders, self.hevc_playable);
        let cpu = (enc == Encoder::X264 && wanted != Encoder::X264 && !self.encoders.is_empty())
            .then(|| crate::capture::encoders::CPU_NOTICE.to_string());
        for n in [notice, cpu].into_iter().flatten() {
            if !self.notices_shown.contains(&n) {
                tracing::warn!("{n}");
                self.ui.toast(APP_TITLE, &n);
                self.notices_shown.push(n);
            }
        }
        enc
    }

    /// SDR white in scRGB units for the native FP16 HDR grabber
    /// (`capture::hdr_grab`), or `None` for ddagrab's 8-bit path (SDR monitor).
    fn hdr_sdr_white(&self, display: &crate::capture::display::DisplayInfo) -> Option<f32> {
        if !display.hdr || self.video_source != VideoSource::Ddagrab {
            return None;
        }
        // Unknown level: Windows' default SDR content brightness (200 nits).
        let level = if display.sdr_white_level > 0 { display.sdr_white_level } else { 2500 };
        Some(level as f32 / 1000.0)
    }

    pub fn target(&self) -> &CaptureTarget {
        &self.target
    }

    /// New target from the game watcher.
    pub async fn set_target(&mut self, t: CaptureTarget, s: &Settings, games: &[Game]) {
        self.target = t.clone();
        self.stop_rl();
        if t == CaptureTarget::None {
            self.stop().await;
            return;
        }
        if is_rocket_league(&t, games) {
            self.start_rl(games);
        }
        self.start(s).await;
    }

    /// Restart for a capture-relevant settings change (no-op while idle).
    pub async fn settings_changed(&mut self, s: &Settings) {
        self.force_primary = false;
        if self.target != CaptureTarget::None {
            self.start(s).await;
        }
    }

    /// Game window moved to another monitor (or appeared after capture
    /// started): restart on that monitor. Called on every watcher tick.
    pub async fn recheck_game_monitor(&mut self, s: &Settings) {
        let CaptureTarget::Game { pid, .. } = &self.target else { return };
        if self.force_primary {
            return;
        }
        let (Some(current), Some(id)) = (self.output_idx, window_monitor(*pid)) else { return };
        let Ok(displays) = list_displays() else { return };
        let Some(d) = displays.iter().find(|d| d.id.eq_ignore_ascii_case(&id)) else { return };
        if d.output_idx != current {
            tracing::info!("game window on {id} (output {}), was output {current}: restarting capture", d.output_idx);
            self.start(s).await;
        }
    }

    /// FFmpeg reported that the selected output cannot be enumerated: fall back
    /// to the primary monitor once (toast) and restart.
    pub async fn output_missing(&mut self, s: &Settings) {
        if self.target == CaptureTarget::None || self.force_primary {
            return;
        }
        let primary = primary_output();
        if self.output_idx == Some(primary) {
            return; // already on the primary; the watchdog keeps retrying
        }
        tracing::warn!("output {:?} not available; falling back to primary output {primary}", self.output_idx);
        self.force_primary = true;
        self.ui.toast(APP_TITLE, "Selected monitor not available – recording primary monitor");
        self.start(s).await;
    }

    async fn start(&mut self, s: &Settings) {
        if !self.ffmpeg_found {
            let h = Health::Failing { reason: "FFmpeg not found".into() };
            self.ui.health_changed(&h, None);
            return;
        }
        // a. Display.
        let displays = list_displays().unwrap_or_else(|e| {
            tracing::warn!("listing displays: {e:#}");
            Vec::new()
        });
        // Games record the monitor their window is on; the configured monitor
        // applies to desktop recording only.
        let (wanted, desktop) = match &self.target {
            CaptureTarget::Game { pid, .. } => (window_monitor(*pid), false),
            _ => (s.monitor_id.clone(), true),
        };
        let (display, fell_back) = resolve_display(&displays, wanted.as_deref());
        let output_idx = if self.force_primary { primary_output() } else { display.output_idx };
        // The monitor actually captured (differs from `display` when forced to primary).
        let display = displays.iter().find(|d| d.output_idx == output_idx).cloned().unwrap_or(display);
        if fell_back && desktop && !self.force_primary {
            self.ui.toast(APP_TITLE, "Selected monitor not found – recording primary");
        }
        // b. Audio: keep pipes and sources while the layout is unchanged.
        let plan = plan_sources(s, process_loopback_supported());
        if audio_setup_needed(self.audio.as_ref().map(|a| a.plan.as_slice()), &plan) {
            self.drop_audio().await;
            match AudioLayout::create(plan, &self.levels) {
                Ok(a) => self.audio = Some(a),
                Err(e) => {
                    // FFmpeg still starts, without audio inputs: this capture
                    // records video only. `self.audio` stays `None`, so the next
                    // capture start (new target, settings change, monitor
                    // fallback) tries the audio setup again. Not on a watchdog
                    // restart of FFmpeg: that keeps the buffer, and segments
                    // with and without audio tracks cannot be concatenated
                    // into one clip.
                    tracing::error!("audio setup failed: {e:#}; recording video only until the next capture start");
                    self.ui.toast(APP_TITLE, &format!("Audio capture could not start ({e}) – recording video only"));
                }
            }
        }
        let pipes: Vec<AudioPipe> = self.audio.as_ref().map(|a| a.pipes.clone()).unwrap_or_default();
        // c. Spec + capture info, before every start.
        let hdr_sdr_white = self.hdr_sdr_white(&display);
        let (mut out_w, out_h) = output_size(s.output_height);
        if hdr_sdr_white.is_some() {
            out_w = out_w.div_ceil(4) * 4; // the HDR shader writes 4 pixels per store
        }
        let encoder = self.pick_encoder(s.encoder);
        let spec = CaptureSpec {
            output_idx,
            fps: s.fps,
            out_w,
            out_h,
            hdr_sdr_white,
            encoder,
            bitrate_kbps: s.bitrate_kbps,
            audio_inputs: pipes.iter().cloned().map(AudioInput::Pipe).collect(),
            segment_dir: s.segment_dir(),
            segment_prefix: String::new(),
            video_source: self.video_source,
            wallclock_origin_us: 0,
        };
        // Keyed by stream layout: a later save of this footage (after the game
        // ended, or after a restart with another layout) still gets its info.
        self.save.set_capture_info(
            layout_key(&spec),
            CaptureInfo {
                fps: s.fps,
                width: out_w,
                height: out_h,
                track_titles: pipes.iter().map(|p| p.title.to_string()).collect(),
                label: target_name(&self.target).unwrap_or_else(|| "Desktop".into()),
            },
        );
        let (dw, dh, dhz, hdr) = (display.width, display.height, display.refresh_hz, display.hdr);
        let white = spec.hdr_sdr_white;
        tracing::info!(
            "capture start: {:?} on output {output_idx} ({dw}x{dh}@{dhz} hdr={hdr} sdr_white={white:?}), {out_w}x{out_h}@{} {:?}, tracks {:?}",
            self.target,
            s.fps,
            encoder,
            pipes.iter().map(|p| p.title).collect::<Vec<_>>()
        );
        self.output_idx = Some(output_idx);
        // d. Go.
        self.supervisor.start(spec).await;
    }

    pub async fn stop(&mut self) {
        self.stop_rl();
        self.supervisor.stop().await;
        self.output_idx = None;
        self.drop_audio().await;
    }

    async fn drop_audio(&mut self) {
        if let Some(a) = self.audio.take() {
            // Dropping joins the capture threads: keep that off the async workers.
            let _ = tokio::task::spawn_blocking(move || drop(a)).await;
        }
    }

    fn start_rl(&mut self, games: &[Game]) {
        let port = rl_stats::configured_port(rl_stats::find_rl_install(games).as_deref());
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let token = CancellationToken::new();
        tracing::info!("Rocket League active: stats listener on {addr}");
        tokio::spawn(rl_stats::run_listener_flagged(addr, self.bus.clone(), token.clone(), self.rl_connected.clone()));
        self.rl = Some(token);
    }

    fn stop_rl(&mut self) {
        if let Some(t) = self.rl.take() {
            tracing::info!("stats listener stopped");
            t.cancel();
        }
    }
}

/// Whether pipes and audio sources must be (re)created for `plan`: the layout
/// changed, or there is none because the last setup failed (so a failed setup
/// is retried on every capture start).
fn audio_setup_needed(current: Option<&[(SourceKind, &'static str)]>, plan: &[(SourceKind, &'static str)]) -> bool {
    current != Some(plan)
}

/// ddagrab output of the primary monitor (0 if unknown).
fn primary_output() -> u32 {
    list_displays().ok().and_then(|d| d.into_iter().find(|d| d.primary).map(|d| d.output_idx)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_audio_setup_is_retried_on_the_next_start() {
        let plan = vec![(SourceKind::Mic { device: None }, "Mic"), (SourceKind::Silence, "Discord")];
        assert!(audio_setup_needed(None, &plan), "no layout (setup failed) => set up again");
        assert!(!audio_setup_needed(Some(&plan), &plan), "same layout => keep pipes and sources");
        let other = vec![(SourceKind::Silence, "Mic"), (SourceKind::Silence, "Discord")];
        assert!(audio_setup_needed(Some(&other), &plan), "layout changed => recreate");
    }
}
