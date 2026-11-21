//! End-to-end run of AppCore without the GUI: game detection (fake process
//! list reporting `notepad.exe`) ⇒ capture ⇒ the same save path the hotkey
//! uses ⇒ clip + library row + notifications; FFmpeg killed mid-run ⇒
//! recovery ⇒ save with the gap flag; shutdown ⇒ "not recording".
//!
//! Opt-in: `CLIPVAULT_FFMPEG` (ffmpeg.exe with NVENC, ffprobe.exe beside it)
//! and `CLIPVAULT_E2E=1`. Uses real ddagrab and real audio devices unless
//! `CLIPVAULT_E2E_VIDEO=testsrc` (synthetic video, e.g. when the display is
//! off: Desktop Duplication then delivers no frames at all).
//! `static_desktop_keeps_recording` (ruling R8) additionally needs
//! `CLIPVAULT_E2E_R8=1` and an awake, untouched display.
//!
//! Privacy: everything is written under a temp dir that is deleted at the end;
//! only stream layout, durations and counts are inspected.

mod e2e {
    use clipvault_lib::app::{AppCore, CoreConfig};
    use clipvault_lib::capture::args::VideoSource;
    use clipvault_lib::capture::supervisor::Health;
    use clipvault_lib::db;
    use clipvault_lib::db::clips::{Clip, ClipFilter, MarkerKind};
    use clipvault_lib::game_watcher::ProcessSource;
    use clipvault_lib::notify::{Sound, Ui};
    use clipvault_lib::save::SaveOutcome;
    use clipvault_lib::settings::Settings;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant, SystemTime};

    #[derive(Debug, Clone, PartialEq)]
    enum Ev {
        Toast(String),
        Sound(Sound),
        Health(Health),
        ClipSaved(String),
    }

    #[derive(Default)]
    struct RecUi(Mutex<Vec<(Instant, Ev)>>);

    impl RecUi {
        fn push(&self, e: Ev) {
            eprintln!("[ui] {e:?}");
            self.0.lock().unwrap().push((Instant::now(), e));
        }
        fn events(&self) -> Vec<Ev> {
            self.0.lock().unwrap().iter().map(|(_, e)| e.clone()).collect()
        }
        fn toasts(&self) -> Vec<String> {
            self.events().into_iter().filter_map(|e| if let Ev::Toast(t) = e { Some(t) } else { None }).collect()
        }
        fn health_since(&self, t: Instant) -> Vec<Health> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter(|(at, _)| *at >= t)
                .filter_map(|(_, e)| if let Ev::Health(h) = e { Some(h.clone()) } else { None })
                .collect()
        }
    }

    impl Ui for RecUi {
        fn toast(&self, _title: &str, body: &str) {
            self.push(Ev::Toast(body.into()));
        }
        fn play(&self, sound: Sound) {
            self.push(Ev::Sound(sound));
        }
        fn health_changed(&self, health: &Health, _target: Option<&str>) {
            self.push(Ev::Health(health.clone()));
        }
        fn clip_saved(&self, clip: &Clip) {
            self.push(Ev::ClipSaved(clip.id.clone()));
        }
        fn mic_level(&self, _dbfs: f32) {}
        fn desktop_capture_changed(&self, _on: bool) {}
    }

    struct FakeProcs;
    impl ProcessSource for FakeProcs {
        fn running_exes(&self) -> Vec<(u32, String)> {
            vec![(4242, "notepad.exe".into()), (1, "explorer.exe".into())]
        }
    }

    fn ffmpeg() -> Option<PathBuf> {
        let ff = std::env::var_os("CLIPVAULT_FFMPEG").map(PathBuf::from);
        let on = std::env::var("CLIPVAULT_E2E").is_ok_and(|v| v == "1");
        match (ff, on) {
            (Some(ff), true) => Some(ff),
            _ => {
                eprintln!("SKIP: set CLIPVAULT_FFMPEG and CLIPVAULT_E2E=1 to run the AppCore end-to-end test");
                None
            }
        }
    }

    fn video_source() -> VideoSource {
        match std::env::var("CLIPVAULT_E2E_VIDEO").as_deref() {
            Ok("testsrc") => VideoSource::TestSrc,
            _ => VideoSource::Ddagrab,
        }
    }

    struct Rig {
        _tmp: tempfile::TempDir,
        base: PathBuf,
        core: Arc<AppCore>,
        ui: Arc<RecUi>,
    }

    fn rig(ff: &Path, video: VideoSource, save_length_s: u32) -> Rig {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("Pat's e2e");
        let data = base.join("data");
        let mut s = Settings::default();
        s.buffer_dir = base.join("buffer");
        s.capture_dir = base.join("Game Clips");
        s.save_length_s = save_length_s;
        s.max_save_length_s = 30;
        s.output_height = 720;
        s.bitrate_kbps = 8000;
        s.save(&data.join("settings.json")).unwrap();
        {
            let conn = db::open(&data.join("library.db")).unwrap();
            db::games::upsert_by_name(&conn, "Notepad", &["notepad.exe".into()], None).unwrap();
        }
        let ui = Arc::new(RecUi::default());
        let cfg = CoreConfig {
            data_dir: data,
            ffmpeg: Some(ff.to_path_buf()),
            temp_root: Some(base.join("tmp")),
            video_source: video,
            desktop_kick: true,
        };
        let core = AppCore::new(cfg, ui.clone()).unwrap();
        core.start(Box::new(FakeProcs));
        Rig { _tmp: tmp, base, core, ui }
    }

    async fn wait_for(core: &AppCore, want: &Health, timeout: Duration) -> bool {
        let end = Instant::now() + timeout;
        while Instant::now() < end {
            if core.health() == *want {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    }

    fn streams(ff: &Path, file: &Path) -> Vec<String> {
        let o = Command::new(ff.with_file_name("ffprobe.exe"))
            .args(["-v", "error", "-show_entries", "stream=codec_name,codec_type:stream_tags=name", "-of", "csv=p=0"])
            .arg(file)
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect()
    }

    fn duration(ff: &Path, file: &Path) -> f64 {
        let o = Command::new(ff.with_file_name("ffprobe.exe"))
            .args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"])
            .arg(file)
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).trim().parse().unwrap_or(0.0)
    }

    fn segment_count(dir: &Path) -> usize {
        std::fs::read_dir(dir)
            .map(|rd| rd.filter_map(|e| e.ok()).filter(|e| e.file_name().to_string_lossy().ends_with(".ts")).count())
            .unwrap_or(0)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn e2e_detect_save_kill_recover_gap() {
        let Some(ff) = ffmpeg() else { return };
        let video = video_source();
        eprintln!("video source: {video:?}");
        let r = rig(&ff, video, 10);
        let core = &r.core;

        // Detection (2 s poll) ⇒ capture start ⇒ first segment.
        assert!(wait_for(core, &Health::Recording, Duration::from_secs(20)).await, "never recording: {:?}", r.ui.events());
        assert_eq!(core.game_name(), "Notepad");
        tokio::time::sleep(Duration::from_secs(12)).await;

        // Save through the hotkey path.
        core.mark(SystemTime::now() - Duration::from_secs(3));
        let out = core.save_and_notify(SystemTime::now()).await;
        let SaveOutcome::Saved { clip, gap, short } = out else { panic!("first save: {out:?}") };
        assert!(!gap, "no gap expected");
        assert!(!short, "{} s", clip.duration_s);
        assert_eq!(clip.game, "Notepad");
        assert!(clip.path.starts_with(r.base.join("Game Clips")));
        let st = streams(&ff, &clip.path);
        eprintln!("streams: {st:?}, duration {:.2} s", duration(&ff, &clip.path));
        let audio: Vec<&String> = st.iter().filter(|s| s.contains("audio")).collect();
        assert_eq!(st.iter().filter(|s| s.contains("video")).count(), 1, "{st:?}");
        let loopback = clipvault_lib::audio::sources::process_loopback_supported();
        assert_eq!(audio.len(), if loopback { 3 } else { 2 }, "{st:?}");
        assert_eq!(st.len(), if loopback { 4 } else { 3 });
        for (a, title) in audio.iter().zip(["Mic", "Discord", "Game/System"]) {
            if loopback || title != "Discord" {
                assert!(a.ends_with(title), "{a} vs {title}");
            }
        }
        let d = duration(&ff, &clip.path);
        assert!((8.0..=14.0).contains(&d), "duration {d}");
        assert!(clip.markers.iter().any(|m| m.kind == MarkerKind::Manual), "{:?}", clip.markers);
        let rows = clipvault_lib::db::clips::list(&core.db().lock().unwrap(), ClipFilter::default()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, clip.id);
        let toasts = r.ui.toasts();
        assert!(toasts.iter().any(|t| t.starts_with("Clip saved (") && t.contains("Notepad)")), "{toasts:?}");
        assert!(r.ui.events().contains(&Ev::Sound(Sound::Saved)));
        assert!(r.ui.events().contains(&Ev::ClipSaved(clip.id.clone())));

        // Kill FFmpeg ⇒ watchdog restart ⇒ recording again.
        let killed_at = Instant::now();
        core.supervisor().kill_child_for_test().await;
        let mut restarted = false;
        let end = Instant::now() + Duration::from_secs(20);
        while Instant::now() < end {
            let hs = r.ui.health_since(killed_at);
            restarted = hs.iter().any(|h| matches!(h, Health::Restarting { .. }));
            if restarted && core.health() == Health::Recording {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let recovered_in = killed_at.elapsed();
        let hs = r.ui.health_since(killed_at);
        eprintln!("after kill: {hs:?}; recovered in {recovered_in:?}");
        assert!(restarted && core.health() == Health::Recording, "no recovery: {hs:?}");
        let reason = hs.iter().find_map(|h| if let Health::Restarting { reason } = h { Some(reason.clone()) } else { None });
        let reason = reason.unwrap();
        assert!(reason.contains("FFmpeg exited") && reason.contains("log: "), "{reason}");
        tokio::time::sleep(Duration::from_secs(5)).await;

        let out = core.save_and_notify(SystemTime::now()).await;
        let SaveOutcome::Saved { clip: clip2, gap, .. } = out else { panic!("second save: {out:?}") };
        assert!(gap, "save across the restart must flag a gap");
        assert_eq!(streams(&ff, &clip2.path).len(), st.len());
        assert!(r.ui.toasts().iter().any(|t| t.contains("– contains gap")), "{:?}", r.ui.toasts());

        // Shutdown ⇒ idle ⇒ a press says "not recording".
        core.shutdown().await;
        assert_eq!(core.supervisor().health(), Health::Idle);
        assert_eq!(core.save_and_notify(SystemTime::now()).await, SaveOutcome::NotRecording);
        assert!(r.ui.toasts().iter().any(|t| t.starts_with("Not recording")));
        // No failing toast in a healthy run with one forced restart.
        assert!(!r.ui.toasts().iter().any(|t| t.starts_with("Capture failing")), "{:?}", r.ui.toasts());
    }

    /// Ruling R8: on an untouched display, capture must start within ~3 s of
    /// the first segment deadline and keep producing segments for ≥ 30 s
    /// without a restart (desktop kick + ddagrab dup_frames).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn static_desktop_keeps_recording() {
        let Some(ff) = ffmpeg() else { return };
        if !std::env::var("CLIPVAULT_E2E_R8").is_ok_and(|v| v == "1") {
            eprintln!("SKIP: set CLIPVAULT_E2E_R8=1 (awake, untouched display) for the static desktop test");
            return;
        }
        let r = rig(&ff, VideoSource::Ddagrab, 10);
        let core = &r.core;
        let t0 = Instant::now();
        let ok = wait_for(core, &Health::Recording, Duration::from_secs(15)).await;
        let first = t0.elapsed();
        eprintln!("R8: first segment after {first:?} (includes up to 2 s detection poll)");
        assert!(ok, "no segment on a static desktop: {:?}", r.ui.events());
        let buffer = r.base.join("buffer");
        let n0 = segment_count(&buffer);
        let since = Instant::now();
        for i in 1..=6 {
            tokio::time::sleep(Duration::from_secs(5)).await;
            eprintln!("R8: +{} s: {} segments, health {:?}", i * 5, segment_count(&buffer), core.health());
        }
        let n1 = segment_count(&buffer);
        let hs = r.ui.health_since(since);
        eprintln!("R8: segments {n0} -> {n1} over 30 s; health changes {hs:?}");
        core.shutdown().await;
        assert!(hs.is_empty(), "health changed during 30 s idle: {hs:?}");
        assert!(n1 >= n0 + 13, "segments did not keep coming: {n0} -> {n1}");
    }
}
