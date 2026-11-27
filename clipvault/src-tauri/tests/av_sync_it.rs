//! A/V sync measurement against a real FFmpeg, with a synthetic pipe source
//! (no audio devices): a PipeWriter is fed real-time silence with one
//! full-scale impulse written at a known wall time Tw. With every input on the
//! shared origin T0 (`CaptureSpec::wallclock_origin_us`), the impulse must land
//! at output time Tw - T0 and the first video frame at (its wall time - T0),
//! both within one frame.
//!
//! Opt-in like the audio smoke test: set `CLIPVAULT_FFMPEG` (ffmpeg.exe with
//! h264_nvenc, ffprobe.exe beside it) and `CLIPVAULT_AUDIO_SMOKE=1`. The ddagrab
//! case records the screen into a temp dir that is deleted afterwards; only
//! timestamps are read from it.

mod av_sync {
    use clipvault_lib::audio::pipes::PipeWriter;
    use clipvault_lib::capture::args::{build_capture_args, AudioInput, AudioPipe, CaptureSpec, VideoSource};
    use clipvault_lib::settings::Encoder;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant, SystemTime};

    const FPS: u32 = 60;
    const FRAME: f64 = 1.0 / FPS as f64;
    /// The impulse is written this long after T0.
    const IMPULSE_AFTER: Duration = Duration::from_millis(3300);

    fn config() -> Option<PathBuf> {
        let ff = std::env::var_os("CLIPVAULT_FFMPEG").map(PathBuf::from);
        let on = std::env::var("CLIPVAULT_AUDIO_SMOKE").is_ok_and(|v| v == "1");
        match (ff, on) {
            (Some(ff), true) => Some(ff),
            _ => {
                eprintln!("SKIP: set CLIPVAULT_FFMPEG and CLIPVAULT_AUDIO_SMOKE=1 to run the A/V sync measurement");
                None
            }
        }
    }

    fn unix_us(t: SystemTime) -> i64 {
        t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_micros() as i64
    }

    fn ffprobe(ff: &Path) -> PathBuf {
        ff.with_file_name("ffprobe.exe")
    }

    fn packet_pts(ff: &Path, file: &Path, stream: &str) -> Vec<f64> {
        let o = Command::new(ffprobe(ff))
            .args(["-v", "error", "-select_streams", stream, "-show_entries", "packet=pts_time", "-of", "csv=p=0"])
            .arg(file)
            .output()
            .unwrap();
        let mut v: Vec<f64> =
            String::from_utf8_lossy(&o.stdout).lines().filter_map(|l| l.trim().trim_end_matches(',').parse().ok()).collect();
        v.sort_by(f64::total_cmp);
        v
    }

    /// Decoded mono samples of the first audio stream.
    fn decode_audio(ff: &Path, file: &Path) -> Vec<f32> {
        let o = Command::new(ff)
            .args(["-v", "error", "-i"])
            .arg(file)
            .args(["-map", "0:a:0", "-ac", "1", "-f", "f32le", "-"])
            .output()
            .unwrap();
        o.stdout.chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect()
    }

    struct Measured {
        video_first: f64,
        video_bounds: (f64, f64),
        impulse: f64,
        impulse_expected: f64,
    }

    fn measure(ff: &Path, video_source: VideoSource) -> Measured {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let name = format!("clipvault_avsync_{}", uuid::Uuid::new_v4().simple());
        let writer = PipeWriter::new(&name).unwrap();

        let t0 = SystemTime::now();
        let t0_us = unix_us(t0);
        let spec = CaptureSpec {
            output_idx: 0,
            fps: FPS,
            out_w: 320,
            out_h: 180,
            hdr_sdr_white: None,
            ddagrab_fallback: false,
            encoder: Encoder::H264Nvenc,
            bitrate_kbps: 1000,
            audio_inputs: vec![AudioInput::Pipe(AudioPipe { pipe_name: name.clone(), title: "Sync" })],
            segment_dir: dir.to_path_buf(),
            segment_prefix: "s_".into(),
            video_source,
            wallclock_origin_us: t0_us,
            game_window: None,
        };

        // Synthetic real-time source: 10 ms chunks paced by wall clock; one chunk
        // starts with a full-scale impulse at Tw.
        let stop = Arc::new(AtomicBool::new(false));
        let tw_us = Arc::new(AtomicI64::new(0));
        let feeder = {
            let (w, stop, tw_us) = (writer.clone(), stop.clone(), tw_us.clone());
            std::thread::spawn(move || {
                let start = Instant::now();
                let mut sent = 0u32;
                while !stop.load(Ordering::SeqCst) {
                    let due = start + Duration::from_millis(10) * sent;
                    if let Some(d) = due.checked_duration_since(Instant::now()) {
                        std::thread::sleep(d);
                    }
                    let mut chunk = vec![0.0f32; 480 * 2];
                    if tw_us.load(Ordering::SeqCst) == 0 && start.elapsed() >= IMPULSE_AFTER {
                        chunk[0] = 1.0;
                        chunk[1] = 1.0;
                        tw_us.store(unix_us(SystemTime::now()), Ordering::SeqCst);
                    }
                    w.write(&chunk);
                    sent += 1;
                }
            })
        };

        let log = std::fs::File::create(dir.join("ffmpeg.log")).unwrap();
        let spawned_us = unix_us(SystemTime::now());
        let mut child = Command::new(ff)
            .args(build_capture_args(&spec))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap();
        let t = Instant::now();
        let mut connected_us = None;
        while t.elapsed() < Duration::from_millis(6500) {
            if connected_us.is_none() && writer.is_connected() {
                connected_us = Some(unix_us(SystemTime::now()));
            }
            assert!(child.try_wait().unwrap().is_none(), "ffmpeg exited: {}", std::fs::read_to_string(dir.join("ffmpeg.log")).unwrap());
            std::thread::sleep(Duration::from_millis(1));
        }
        child.kill().unwrap();
        child.wait().unwrap();
        stop.store(true, Ordering::SeqCst);
        feeder.join().unwrap();
        let tw = tw_us.load(Ordering::SeqCst);
        assert!(tw > 0, "impulse written");

        let log = std::fs::read_to_string(dir.join("ffmpeg.log")).unwrap();
        let csv = std::fs::read_to_string(dir.join("s_list.csv")).unwrap_or_default();
        eprintln!("{video_source:?}: segment list:\n{csv}");
        let rows: Vec<(PathBuf, f64)> = csv
            .lines()
            .filter_map(|l| {
                let mut p = l.split(',');
                Some((dir.join(p.next()?), p.next()?.parse().ok()?))
            })
            .collect();
        assert!(rows.len() >= 2, "segments: {rows:?}; log:\n{log}");
        let segs: Vec<PathBuf> = rows.iter().map(|r| r.0.clone()).collect();
        // The mpegts muxer adds a constant offset to every timestamp in the files;
        // the segment list reports the un-offset (T0-relative) start of segment 1.
        let shift = packet_pts(ff, &segs[1], "v:0")[0] - rows[1].1;

        let video_first = packet_pts(ff, &segs[0], "v:0")[0] - shift;
        // Find the impulse: the loudest decoded sample across all segments.
        let mut best = (0.0f32, 0.0f64);
        for seg in &segs {
            let first_audio = packet_pts(ff, seg, "a:0")[0] - shift;
            let samples = decode_audio(ff, seg);
            if let Some((i, v)) = samples.iter().enumerate().max_by(|a, b| a.1.abs().total_cmp(&b.1.abs())) {
                if v.abs() > best.0 {
                    best = (v.abs(), first_audio + i as f64 / 48_000.0);
                }
            }
        }
        assert!(best.0 > 0.3, "impulse not found (peak {})", best.0);
        let s = |us: i64| (us - t0_us) as f64 / 1e6;
        let m = Measured {
            video_first,
            // Frame 0 is produced while FFmpeg opens its first input, i.e. after the
            // spawn and before it opens the pipe.
            video_bounds: (s(spawned_us), s(connected_us.expect("pipe connected"))),
            impulse: best.1,
            impulse_expected: s(tw),
        };
        let warnings = log;
        eprintln!(
            "{video_source:?}: mpegts shift {shift:.3}s; first video pts {:.4}s (spawn {:.4}s, pipe connected {:.4}s); \
             impulse at {:.4}s, written at {:.4}s => error {:+.1} ms; ffmpeg log:\n{warnings}",
            m.video_first,
            m.video_bounds.0,
            m.video_bounds.1,
            m.impulse,
            m.impulse_expected,
            (m.impulse - m.impulse_expected) * 1000.0
        );
        assert!(!warnings.to_ascii_lowercase().contains("error"), "{warnings}");
        drop(writer);
        tmp.close().unwrap();
        m
    }

    fn check(m: &Measured) {
        assert!(
            (m.impulse - m.impulse_expected).abs() <= FRAME,
            "impulse at {} vs written at {} (> 1 frame)",
            m.impulse,
            m.impulse_expected
        );
        assert!(
            m.video_first >= m.video_bounds.0 - FRAME && m.video_first <= m.video_bounds.1 + FRAME,
            "first video frame at {} outside its production window {:?}",
            m.video_first,
            m.video_bounds
        );
    }

    #[test]
    fn testsrc_and_pipe_share_wallclock_origin() {
        let Some(ff) = config() else { return };
        check(&measure(&ff, VideoSource::TestSrc));
    }

    #[test]
    fn ddagrab_and_pipe_share_wallclock_origin() {
        let Some(ff) = config() else { return };
        if !ddagrab_delivers_frames(&ff) {
            eprintln!("SKIP: ddagrab delivered no frame within 8 s (display off/asleep?); ddagrab sync not measured");
            return;
        }
        check(&measure(&ff, VideoSource::Ddagrab));
    }

    /// Desktop Duplication only produces frames while a display is active.
    fn ddagrab_delivers_frames(ff: &Path) -> bool {
        let mut child = Command::new(ff)
            .args(["-v", "error", "-f", "lavfi", "-i", "ddagrab=output_idx=0,hwdownload,format=bgra"])
            .args(["-frames:v", "1", "-f", "null", "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(8) {
            if let Some(st) = child.try_wait().unwrap() {
                return st.success();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
        false
    }
}
