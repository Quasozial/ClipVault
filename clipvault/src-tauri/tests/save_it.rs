//! SaveService integration tests against a real FFmpeg.
//!
//! Set `CLIPVAULT_FFMPEG` to an ffmpeg.exe with h264_nvenc (ffprobe.exe must sit
//! beside it); without it the FFmpeg tests print a skip message and pass.
//! Each test runs its own NVENC capture, so use `--test-threads=2` (or fewer).
//! All working folders live under a directory whose name contains a space and an
//! apostrophe, as real user paths do.
//!
//! Timings are scaled down from the spec to keep wall time sane while keeping the
//! proportions: `save_60s` saves 10 s after ~14 s of buffer (spec: 60 s after 70 s).

mod save {
    use clipvault_lib::capture::args::{AudioInput, CaptureSpec, VideoSource};
    use clipvault_lib::capture::segments::Segment;
    use clipvault_lib::capture::supervisor::{Health, Supervisor, SupervisorEvent};
    use clipvault_lib::db::clips::{self, ClipFilter, MarkerKind};
    use clipvault_lib::events::{BusEvent, EventBus};
    use clipvault_lib::save::{CaptureInfo, SaveConfig, SaveOutcome, SaveService};
    use clipvault_lib::settings::Encoder;
    use rusqlite::Connection;
    use std::io::Read;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, SystemTime};
    use tokio::sync::broadcast;
    use tokio::time::{sleep, timeout};

    const TITLES: [&str; 3] = ["Mic", "Discord", "Game/System"];

    fn ffmpeg() -> Option<PathBuf> {
        match std::env::var_os("CLIPVAULT_FFMPEG") {
            Some(p) => Some(PathBuf::from(p)),
            None => {
                eprintln!("SKIP: CLIPVAULT_FFMPEG not set; save integration test skipped");
                None
            }
        }
    }

    fn ffprobe(ff: &Path) -> PathBuf {
        ff.with_file_name("ffprobe.exe")
    }

    struct Rig {
        _root: tempfile::TempDir,
        base: PathBuf,
        sup: Arc<Supervisor>,
        svc: SaveService,
        db: Arc<Mutex<Connection>>,
        bus: Arc<EventBus>,
        rx: broadcast::Receiver<SupervisorEvent>,
    }

    impl Rig {
        fn clips_dir(&self) -> PathBuf {
            self.base.join("Game Clips")
        }
        fn temp_root(&self) -> PathBuf {
            self.base.join("tmp root")
        }
        fn config(&self, ff: &Path, save_length: Duration) -> SaveConfig {
            SaveConfig {
                ffmpeg: ff.to_path_buf(),
                capture_dir: self.clips_dir(),
                thumbs_dir: self.base.join("thumbs"),
                temp_root: self.temp_root(),
                save_length,
            }
        }
        fn temp_leftovers(&self) -> Vec<PathBuf> {
            std::fs::read_dir(self.temp_root())
                .map(|rd| rd.map(|e| e.unwrap().path()).collect())
                .unwrap_or_default()
        }
        fn row_count(&self) -> usize {
            clips::list(&self.db.lock().unwrap(), ClipFilter::default()).unwrap().len()
        }
    }

    fn rig(ff: &Path, save_length: Duration) -> Rig {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("O'Neil clip vault");
        std::fs::create_dir_all(base.join("tmp root")).unwrap();
        let (tx, rx) = broadcast::channel(1024);
        let sup = Arc::new(Supervisor::new(ff.to_path_buf(), Duration::from_secs(600), tx));
        let db = Arc::new(Mutex::new(clipvault_lib::db::open(&base.join("library.db")).unwrap()));
        let bus = Arc::new(EventBus::new(Duration::from_secs(120)));
        let info = CaptureInfo {
            fps: 30,
            width: 320,
            height: 180,
            track_titles: TITLES.iter().map(|s| s.to_string()).collect(),
            label: "Rocket League".into(),
        };
        let cfg = SaveConfig {
            ffmpeg: ff.to_path_buf(),
            capture_dir: base.join("Game Clips"),
            thumbs_dir: base.join("thumbs"),
            temp_root: base.join("tmp root"),
            save_length,
        };
        let svc = SaveService::new(cfg, info, sup.clone(), db.clone(), bus.clone());
        Rig { _root: root, base, sup, svc, db, bus, rx }
    }

    fn spec(dir: &Path) -> CaptureSpec {
        CaptureSpec {
            output_idx: 0,
            fps: 30,
            out_w: 320,
            out_h: 180,
            hdr_sdr_white: None,
            encoder: Encoder::H264Nvenc,
            bitrate_kbps: 1000,
            audio_inputs: TITLES.iter().map(|t| AudioInput::Silence { title: t }).collect(),
            segment_dir: dir.to_path_buf(),
            segment_prefix: "ignored_".into(),
            video_source: VideoSource::TestSrc,
            wallclock_origin_us: 0, // set per run by the caller / supervisor
        }
    }

    async fn wait_event(
        rx: &mut broadcast::Receiver<SupervisorEvent>,
        within: Duration,
        pred: impl Fn(&SupervisorEvent) -> bool,
    ) -> SupervisorEvent {
        timeout(within, async {
            loop {
                match rx.recv().await {
                    Ok(e) if pred(&e) => return e,
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(e) => panic!("event channel: {e}"),
                }
            }
        })
        .await
        .expect("timed out waiting for supervisor event")
    }

    async fn start_recording(r: &mut Rig) {
        let buf = r.base.join("seg buffer");
        std::fs::create_dir_all(&buf).unwrap();
        r.sup.start(spec(&buf)).await;
        wait_event(&mut r.rx, Duration::from_secs(10), |e| {
            matches!(e, SupervisorEvent::HealthChanged(Health::Recording))
        })
        .await;
    }

    struct Probe {
        duration: f64,
        streams: Vec<(String, Option<String>, Option<u64>)>, // (codec_type, track name, width)
    }

    fn probe(ff: &Path, file: &Path) -> Probe {
        let out = Command::new(ffprobe(ff))
            .args(["-v", "error", "-show_entries", "format=duration:stream=codec_type,width:stream_tags=name,handler_name", "-of", "json"])
            .arg(file)
            .output()
            .unwrap();
        assert!(out.status.success(), "ffprobe failed: {}", String::from_utf8_lossy(&out.stderr));
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        let duration = v["format"]["duration"].as_str().map(|s| s.parse().unwrap()).unwrap_or(0.0);
        let streams = v["streams"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| {
                (
                    s["codec_type"].as_str().unwrap().to_string(),
                    track_name(&s["tags"]),
                    s["width"].as_u64(),
                )
            })
            .collect();
        Probe { duration, streams }
    }

    /// The mp4 muxer stores a stream `title` as `name`; `handler_name` must match it.
    fn track_name(tags: &serde_json::Value) -> Option<String> {
        let name = tags["name"].as_str()?;
        let handler = tags["handler_name"].as_str();
        (handler == Some(name)).then(|| name.to_string())
    }

    /// Top-level MP4 atom types in file order.
    fn top_atoms(file: &Path) -> Vec<String> {
        let mut f = std::fs::File::open(file).unwrap();
        let len = f.metadata().unwrap().len();
        let mut pos = 0u64;
        let mut atoms = Vec::new();
        while pos + 8 <= len {
            use std::io::Seek;
            f.seek(std::io::SeekFrom::Start(pos)).unwrap();
            let mut h = [0u8; 16];
            f.read_exact(&mut h[..8]).unwrap();
            let mut size = u32::from_be_bytes(h[0..4].try_into().unwrap()) as u64;
            let kind = String::from_utf8_lossy(&h[4..8]).to_string();
            if size == 1 {
                f.read_exact(&mut h[8..16]).unwrap();
                size = u64::from_be_bytes(h[8..16].try_into().unwrap());
            } else if size == 0 {
                size = len - pos;
            }
            atoms.push(kind);
            pos += size.max(8);
        }
        atoms
    }

    /// Decodes the whole file; returns FFmpeg's error output.
    fn decode_errors(ff: &Path, file: &Path) -> (bool, String) {
        let out = Command::new(ff).args(["-v", "error", "-i"]).arg(file).args(["-f", "null", "-"]).output().unwrap();
        (out.status.success(), String::from_utf8_lossy(&out.stderr).into_owned())
    }

    fn assert_streams(p: &Probe) {
        let video = p.streams.iter().filter(|s| s.0 == "video").count();
        let audio: Vec<Option<&str>> =
            p.streams.iter().filter(|s| s.0 == "audio").map(|s| s.1.as_deref()).collect();
        assert_eq!(video, 1, "{:?}", p.streams);
        assert_eq!(audio, vec![Some("Mic"), Some("Discord"), Some("Game/System")]);
    }

    fn unwrap_saved(o: SaveOutcome) -> (clips::Clip, bool, bool) {
        match o {
            SaveOutcome::Saved { clip, gap, short } => (clip, gap, short),
            other => panic!("expected Saved, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn save_60s() {
        let Some(ff) = ffmpeg() else { return };
        let mut r = rig(&ff, Duration::from_secs(10));
        start_recording(&mut r).await;
        sleep(Duration::from_secs(12)).await; // ~14 s of buffer incl. the first segment

        let now = SystemTime::now();
        r.bus.push(BusEvent { at: now - Duration::from_secs(3), kind: MarkerKind::Goal, label: "G".into() });
        r.bus.push(BusEvent { at: now - Duration::from_secs(30), kind: MarkerKind::Goal, label: "old".into() });
        let (clip, gap, short) = unwrap_saved(r.svc.save(now, "Rocket League").await);
        r.sup.stop().await;

        assert!(!gap && !short, "gap={gap} short={short}");
        assert!((9.0..=12.5).contains(&clip.duration_s), "duration {}", clip.duration_s);
        assert!(clip.path.exists());
        assert!(clip.path.starts_with(r.clips_dir()));
        let name = clip.path.file_name().unwrap().to_str().unwrap().to_string();
        assert!(name.starts_with("Rocket-League__") && name.ends_with(".mp4"), "{name}");
        let files: Vec<_> = std::fs::read_dir(r.clips_dir()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(files.len(), 1, "stray files: {files:?}");
        assert!(r.temp_leftovers().is_empty(), "{:?}", r.temp_leftovers());

        let p = probe(&ff, &clip.path);
        eprintln!("ffprobe duration {} vs clip.duration_s {}; streams {:?}", p.duration, clip.duration_s, p.streams);
        assert!((p.duration - clip.duration_s).abs() < 0.6, "probe {} vs {}", p.duration, clip.duration_s);
        assert_streams(&p);
        let atoms = top_atoms(&clip.path);
        eprintln!("top-level atoms: {atoms:?}");
        let moov = atoms.iter().position(|a| a == "moov").expect("no moov");
        let mdat = atoms.iter().position(|a| a == "mdat").expect("no mdat");
        assert!(moov < mdat, "moov after mdat: {atoms:?}");
        let (ok, errs) = decode_errors(&ff, &clip.path);
        assert!(ok, "decode failed: {errs}");

        // Row metadata.
        assert_eq!(clip.game, "Rocket League");
        assert_eq!(clip.source, "native");
        assert_eq!((clip.fps, clip.width, clip.height), (30, 320, 180));
        assert_eq!(clip.size, std::fs::metadata(&clip.path).unwrap().len());
        assert!(clip.title.starts_with("Rocket League "), "{}", clip.title);
        let thumb = clip.thumbnail_path.clone().expect("thumbnail");
        assert!(thumb.exists());
        let tp = probe(&ff, &thumb);
        assert_eq!(tp.streams[0].2, Some(480), "thumbnail width");

        // Markers: the in-window goal ~3 s before the press, the saved marker, not the old goal.
        let kinds: Vec<MarkerKind> = clip.markers.iter().map(|m| m.kind).collect();
        assert_eq!(kinds, vec![MarkerKind::Goal, MarkerKind::Saved], "{:?}", clip.markers);
        let (goal, saved) = (clip.markers[0].t_s, clip.markers[1].t_s);
        assert!((saved - goal - 3.0).abs() < 0.01, "goal {goal} saved {saved}");
        assert!(saved <= clip.duration_s + 1e-6 && saved > clip.duration_s - 2.5, "saved {saved} / {}", clip.duration_s);

        let row = clips::get(&r.db.lock().unwrap(), &clip.id).unwrap().expect("row");
        assert_eq!(row, clip);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn save_short_buffer() {
        let Some(ff) = ffmpeg() else { return };
        let mut r = rig(&ff, Duration::from_secs(60));
        start_recording(&mut r).await;
        sleep(Duration::from_secs(8)).await; // ~10 s of buffer
        let (clip, gap, short) = unwrap_saved(r.svc.save(SystemTime::now(), "Rocket League").await);
        r.sup.stop().await;
        assert!(short, "expected short");
        assert!(!gap);
        assert!((7.0..=13.0).contains(&clip.duration_s), "duration {}", clip.duration_s);
        let p = probe(&ff, &clip.path);
        eprintln!("short: ffprobe duration {} vs clip.duration_s {}", p.duration, clip.duration_s);
        assert!((p.duration - clip.duration_s).abs() < 0.6);
        assert_streams(&p);
    }

    /// The game ended and capture stopped: its footage is still saved.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn save_after_capture_stopped() {
        let Some(ff) = ffmpeg() else { return };
        let mut r = rig(&ff, Duration::from_secs(30));
        start_recording(&mut r).await;
        sleep(Duration::from_secs(6)).await;
        r.sup.stop().await;
        assert_eq!(r.sup.health(), Health::Idle);
        let (clip, _gap, _short) = unwrap_saved(r.svc.save(SystemTime::now(), "Rocket League").await);
        assert!(clip.duration_s >= 4.0, "duration {}", clip.duration_s);
        assert_streams(&probe(&ff, &clip.path));
    }

    /// A capture restart (new target, monitor move, settings change) keeps the
    /// buffer: one save covers footage from before and after it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn save_spans_a_capture_restart() {
        let Some(ff) = ffmpeg() else { return };
        let mut r = rig(&ff, Duration::from_secs(60));
        start_recording(&mut r).await;
        sleep(Duration::from_secs(6)).await;
        start_recording(&mut r).await;
        sleep(Duration::from_secs(5)).await;
        let (clip, gap, _short) = unwrap_saved(r.svc.save(SystemTime::now(), "Rocket League").await);
        r.sup.stop().await;
        assert!(clip.duration_s >= 8.0, "footage before the restart was lost: {} s", clip.duration_s);
        assert!(gap, "the restart leaves a hole");
        let p = probe(&ff, &clip.path);
        assert!((p.duration - clip.duration_s).abs() < 0.6, "ffprobe {} vs {}", p.duration, clip.duration_s);
        assert_streams(&p);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn save_across_restart_gap() {
        let Some(ff) = ffmpeg() else { return };
        let mut r = rig(&ff, Duration::from_secs(30));
        start_recording(&mut r).await;
        sleep(Duration::from_secs(4)).await;
        // Kill mid-segment so the lost partial segment makes a real hole.
        wait_event(&mut r.rx, Duration::from_secs(5), |e| matches!(e, SupervisorEvent::SegmentClosed(_))).await;
        sleep(Duration::from_millis(1000)).await;
        r.sup.kill_child_for_test().await;
        wait_event(&mut r.rx, Duration::from_secs(5), |e| {
            matches!(e, SupervisorEvent::HealthChanged(Health::Restarting { .. }))
        })
        .await;
        wait_event(&mut r.rx, Duration::from_secs(10), |e| {
            matches!(e, SupervisorEvent::HealthChanged(Health::Recording))
        })
        .await;
        sleep(Duration::from_secs(4)).await;

        let (clip, gap, _short) = unwrap_saved(r.svc.save(SystemTime::now(), "Rocket League").await);
        r.sup.stop().await;
        assert!(gap, "expected gap");
        let p = probe(&ff, &clip.path);
        eprintln!("gap: ffprobe duration {} vs clip.duration_s {}", p.duration, clip.duration_s);
        assert!((p.duration - clip.duration_s).abs() < 0.6);
        assert_streams(&p);
        let (ok, errs) = decode_errors(&ff, &clip.path);
        assert!(ok, "decode failed: {errs}");
        eprintln!("gap clip decode stderr: {errs:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn save_while_restarting_uses_existing_segments() {
        let Some(ff) = ffmpeg() else { return };
        let mut r = rig(&ff, Duration::from_secs(30));
        start_recording(&mut r).await;
        sleep(Duration::from_secs(5)).await;
        r.sup.kill_child_for_test().await;
        wait_event(&mut r.rx, Duration::from_secs(5), |e| {
            matches!(e, SupervisorEvent::HealthChanged(Health::Restarting { .. }))
        })
        .await;
        let started = std::time::Instant::now();
        let (clip, _gap, short) = unwrap_saved(r.svc.save(SystemTime::now(), "Rocket League").await);
        r.sup.stop().await;
        assert!(short);
        assert!(clip.duration_s >= 3.9, "duration {}", clip.duration_s);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn save_to_missing_dir_fails_cleanly() {
        let Some(ff) = ffmpeg() else { return };
        if Path::new(r"Z:\").exists() {
            eprintln!("SKIP: drive Z: exists on this machine");
            return;
        }
        let mut r = rig(&ff, Duration::from_secs(10));
        let mut cfg = r.config(&ff, Duration::from_secs(10));
        cfg.capture_dir = PathBuf::from(r"Z:\nope");
        r.svc.set_config(cfg);
        start_recording(&mut r).await;
        sleep(Duration::from_secs(2)).await;
        let out = r.svc.save(SystemTime::now(), "Rocket League").await;
        r.sup.stop().await;
        match out {
            SaveOutcome::Failed { reason } => assert!(reason.contains(r"Z:\nope"), "{reason}"),
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(r.temp_leftovers().is_empty(), "{:?}", r.temp_leftovers());
        assert_eq!(r.row_count(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ffmpeg_failure_leaves_no_tmp() {
        let Some(ff) = ffmpeg() else { return };
        let mut r = rig(&ff, Duration::from_secs(10));
        start_recording(&mut r).await;
        sleep(Duration::from_secs(2)).await;
        // ffprobe.exe rejects the concat command line: stands in for a failing FFmpeg.
        r.svc.set_config(r.config(&ffprobe(&ff), Duration::from_secs(10)));
        let out = r.svc.save(SystemTime::now(), "Rocket League").await;
        r.sup.stop().await;
        match out {
            SaveOutcome::Failed { reason } => {
                eprintln!("ffmpeg failure reason: {reason}");
                assert!(reason.contains("FFmpeg"), "{reason}")
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        let files: Vec<_> = std::fs::read_dir(r.clips_dir()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert!(files.is_empty(), "{files:?}");
        assert!(r.temp_leftovers().is_empty(), "{:?}", r.temp_leftovers());
        assert_eq!(r.row_count(), 0);
    }

    /// Needs no FFmpeg: the supervisor is never started.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn not_recording() {
        let r = rig(Path::new("ffmpeg-not-needed.exe"), Duration::from_secs(10));
        // While idle, only leftover footage older than the save length is ignored.
        let now = SystemTime::now();
        r.sup.segments().lock().unwrap().push(Segment {
            index: 0,
            path: r.base.join("x.ts"),
            start: now - Duration::from_secs(60),
            end: now - Duration::from_secs(58),
            layout: 0,
        });
        fn assert_send<T: Send>(t: T) -> T {
            t
        }
        let out = assert_send(r.svc.save(now, "Rocket League")).await;
        assert_eq!(out, SaveOutcome::NotRecording);
        assert_eq!(r.row_count(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_rapid_saves_make_two_clips() {
        let Some(ff) = ffmpeg() else { return };
        let mut r = rig(&ff, Duration::from_secs(6));
        start_recording(&mut r).await;
        sleep(Duration::from_secs(6)).await;
        let t = SystemTime::now();
        let (a, b) = tokio::join!(
            r.svc.save(t, "Rocket League"),
            r.svc.save(t + Duration::from_millis(50), "Rocket League")
        );
        r.sup.stop().await;
        let (a, _, _) = unwrap_saved(a);
        let (b, _, _) = unwrap_saved(b);
        assert_ne!(a.id, b.id);
        assert_ne!(a.path, b.path);
        assert!(a.path.exists() && b.path.exists());
        // FIFO: the first call got the plain name (both presses are in the same second
        // unless the 50 ms straddles a second boundary).
        let (an, bn) = (a.path.file_stem().unwrap().to_str().unwrap(), b.path.file_stem().unwrap().to_str().unwrap());
        assert!(!an.ends_with("_2"), "{an}");
        if bn.len() > an.len() {
            assert_eq!(bn, format!("{an}_2"));
        }
        assert_eq!(r.row_count(), 2);
        assert!(r.temp_leftovers().is_empty());
    }
}
