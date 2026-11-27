//! End-to-end audio smoke test on this machine's real devices: the planned
//! sources (default mic, Discord process loopback, everything-else loopback)
//! feed three named pipes that a real FFmpeg (TestSrc video) records; the
//! segments are checked with ffprobe. FFmpeg is then killed and respawned on the
//! same pipe names, which must reconnect.
//!
//! Records real mic/system audio, so it is opt-in: set `CLIPVAULT_FFMPEG` (an
//! ffmpeg.exe with h264_nvenc, ffprobe.exe beside it) and `CLIPVAULT_AUDIO_SMOKE=1`.
//! All output goes to a temp dir that is deleted at the end; audio content is
//! never inspected beyond stream durations and RMS levels.

mod audio_smoke {
    use clipvault_lib::audio::pipes::PipeWriter;
    use clipvault_lib::audio::sources::{list_audio_devices, plan_sources, process_loopback_supported, AudioSource};
    use clipvault_lib::capture::args::{build_capture_args, AudioInput, AudioPipe, CaptureSpec, VideoSource};
    use clipvault_lib::settings::{Encoder, Settings};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn config() -> Option<PathBuf> {
        let ff = std::env::var_os("CLIPVAULT_FFMPEG").map(PathBuf::from);
        let on = std::env::var("CLIPVAULT_AUDIO_SMOKE").is_ok_and(|v| v == "1");
        match (ff, on) {
            (Some(ff), true) => Some(ff),
            _ => {
                eprintln!("SKIP: set CLIPVAULT_FFMPEG and CLIPVAULT_AUDIO_SMOKE=1 to run the audio smoke test");
                None
            }
        }
    }

    fn spec(dir: &Path, prefix: &str, pipes: &[AudioPipe]) -> CaptureSpec {
        CaptureSpec {
            output_idx: 0,
            fps: 30,
            out_w: 320,
            out_h: 180,
            hdr_sdr_white: None,
            ddagrab_fallback: false,
            encoder: Encoder::H264Nvenc,
            bitrate_kbps: 1000,
            audio_inputs: pipes.iter().cloned().map(AudioInput::Pipe).collect(),
            segment_dir: dir.to_path_buf(),
            segment_prefix: prefix.into(),
            video_source: VideoSource::TestSrc,
            wallclock_origin_us: 0, // set per run by the caller / supervisor
            game_window: None,
        }
    }

    fn spawn(ff: &Path, spec: &CaptureSpec) -> Child {
        let log = std::fs::File::create(spec.segment_dir.join(format!("{}ffmpeg.log", spec.segment_prefix))).unwrap();
        Command::new(ff)
            .args(build_capture_args(spec))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap()
    }

    /// Closed segments listed in the run's CSV.
    fn segments(dir: &Path, prefix: &str) -> Vec<PathBuf> {
        let csv = std::fs::read_to_string(dir.join(format!("{prefix}list.csv"))).unwrap_or_default();
        csv.lines().filter_map(|l| l.split(',').next()).filter(|f| !f.is_empty()).map(|f| dir.join(f)).collect()
    }

    /// (codec_type, duration from timestamps, duration from packet count) per stream.
    fn probe(ff: &Path, file: &Path) -> Vec<(String, f64, f64)> {
        let out = Command::new(ff.with_file_name("ffprobe.exe"))
            .args(["-v", "error", "-count_packets", "-show_entries", "stream=codec_type,duration,nb_read_packets"])
            .args(["-of", "json"])
            .arg(file)
            .output()
            .unwrap();
        assert!(out.status.success(), "ffprobe: {}", String::from_utf8_lossy(&out.stderr));
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        v["streams"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| {
                let kind = s["codec_type"].as_str().unwrap_or("").to_string();
                let d = s["duration"].as_str().and_then(|d| d.parse().ok()).unwrap_or(-1.0);
                let n: f64 = s["nb_read_packets"].as_str().and_then(|n| n.parse().ok()).unwrap_or(-1.0);
                // 30 fps video; AAC packets carry 1024 samples at 48 kHz.
                let by_packets = if kind == "video" { n / 30.0 } else { n * 1024.0 / 48_000.0 };
                (kind, d, by_packets)
            })
            .collect()
    }

    fn dropped(writers: &[PipeWriter]) -> Vec<u64> {
        writers.iter().map(|w| w.dropped_frames()).collect()
    }

    /// Runs FFmpeg for `secs`, kills it, and checks the pipes and closed segments.
    fn record_and_check(ff: &Path, dir: &Path, prefix: &str, pipes: &[AudioPipe], writers: &[PipeWriter], secs: u64) {
        let s = spec(dir, prefix, pipes);
        let mut child = spawn(ff, &s);
        let t = Instant::now();
        let mut steady: Option<Vec<u64>> = None;
        while t.elapsed() < Duration::from_secs(secs) {
            assert!(child.try_wait().unwrap().is_none(), "{prefix}: ffmpeg exited early; log:
{}", log(dir, prefix));
            if steady.is_none() && t.elapsed() >= Duration::from_secs(2) {
                steady = Some(dropped(writers));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let connected: Vec<bool> = writers.iter().map(|w| w.is_connected()).collect();
        // FFmpeg must keep consuming every pipe in real time: no queue overflow
        // once it is running (frames dropped before it connects are expected).
        let steady_drops: Vec<u64> = dropped(writers).iter().zip(steady.unwrap()).map(|(a, b)| a - b).collect();
        child.kill().unwrap();
        child.wait().unwrap();
        eprintln!("{prefix}: pipes connected while recording: {connected:?}; frames dropped after 2 s: {steady_drops:?}");
        assert!(connected.iter().all(|c| *c), "{prefix}: every pipe must be connected");
        assert!(steady_drops.iter().all(|d| *d == 0), "{prefix}: FFmpeg stopped reading a pipe: {steady_drops:?}");

        let segs = segments(dir, prefix);
        assert!(segs.len() >= 2, "{prefix}: {} closed segments; log:
{}", segs.len(), log(dir, prefix));
        for seg in &segs {
            let streams = probe(ff, seg);
            let video: Vec<(f64, f64)> = streams.iter().filter(|s| s.0 == "video").map(|s| (s.1, s.2)).collect();
            let audio: Vec<(f64, f64)> = streams.iter().filter(|s| s.0 == "audio").map(|s| (s.1, s.2)).collect();
            eprintln!(
                "{prefix}: {} (duration, by packets) video {video:.3?} audio {audio:.3?}",
                seg.file_name().unwrap().to_string_lossy()
            );
            assert_eq!(video.len(), 1);
            assert_eq!(audio.len(), pipes.len(), "one audio stream per pipe");
            for (d, p) in &audio {
                assert!((d - video[0].0).abs() <= 0.2, "{prefix}: audio {d} vs video {:?}", video[0]);
                assert!((p - video[0].1).abs() <= 0.2, "{prefix}: audio packets {p} vs video {:?}", video[0]);
            }
        }
        let log = log(dir, prefix);
        let errors: Vec<&str> = log.lines().filter(|l| l.to_ascii_lowercase().contains("error")).collect();
        assert!(errors.is_empty(), "{prefix}: ffmpeg errors:\n{log}");
        if !log.trim().is_empty() {
            eprintln!("{prefix}: ffmpeg warnings:\n{log}");
        }
    }

    fn log(dir: &Path, prefix: &str) -> String {
        std::fs::read_to_string(dir.join(format!("{prefix}ffmpeg.log"))).unwrap_or_default()
    }

    #[test]
    fn three_pipes_record_and_survive_ffmpeg_restart() {
        let Some(ff) = config() else { return };
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        let devices = list_audio_devices();
        eprintln!(
            "devices: {} inputs, {} outputs",
            devices.iter().filter(|d| d.is_input).count(),
            devices.iter().filter(|d| !d.is_input).count()
        );
        let supported = process_loopback_supported();
        let plan = plan_sources(&Settings::default(), supported);
        eprintln!("process loopback supported: {supported}; plan: {plan:?}");

        let run_id = uuid::Uuid::new_v4().simple().to_string();
        let mut pipes = Vec::new();
        let mut writers = Vec::new();
        let mut sources = Vec::new();
        let mut level_counts = Vec::new();
        for (i, (kind, title)) in plan.into_iter().enumerate() {
            let pipe = AudioPipe { pipe_name: format!("clipvault_smoke_{run_id}_{i}"), title };
            let w = PipeWriter::new(&pipe.pipe_name).unwrap();
            let count = Arc::new(AtomicU32::new(0));
            let c = count.clone();
            let src = AudioSource::start(kind, w.clone(), Some(Box::new(move |_, _| {
                c.fetch_add(1, Ordering::Relaxed);
            })))
            .unwrap();
            pipes.push(pipe);
            writers.push(w);
            sources.push(src);
            level_counts.push(count);
        }
        std::thread::sleep(Duration::from_millis(500));
        for (s, p) in sources.iter().zip(&pipes) {
            eprintln!("source {}: {} (last error: {:?})", p.title, s.status(), s.last_error());
        }

        record_and_check(&ff, dir, "a_", &pipes, &writers, 7);
        // Restart: kill + respawn on the same pipe names must reconnect.
        record_and_check(&ff, dir, "b_", &pipes, &writers, 6);

        for ((s, p), (w, n)) in sources.iter().zip(&pipes).zip(writers.iter().zip(&level_counts)) {
            eprintln!(
                "source {}: {}; last error {:?}; dropped frames {}; level reports {}",
                p.title,
                s.status(),
                s.last_error(),
                w.dropped_frames(),
                n.load(Ordering::Relaxed)
            );
            assert!(s.last_error().is_none(), "{}: {:?}", p.title, s.last_error());
            // ~100 ms reports over >13 s of running.
            assert!(n.load(Ordering::Relaxed) >= 100, "{}: level reports", p.title);
        }
        for s in sources {
            s.stop();
        }
        drop(writers);
        tmp.close().unwrap();
    }
}
