//! Supervisor integration tests against a real FFmpeg.
//!
//! Set `CLIPVAULT_FFMPEG` to an ffmpeg.exe with h264_nvenc; without it every
//! test prints a skip message and passes. Tests live in `mod supervisor` so
//! `cargo test supervisor` selects them.

mod supervisor {
    use clipvault_lib::capture::args::{AudioInput, CaptureSpec, VideoSource};
    use clipvault_lib::capture::segments::SegmentIndex;
    use clipvault_lib::capture::supervisor::{Health, Supervisor, SupervisorEvent};
    use clipvault_lib::settings::Encoder;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, SystemTime};
    use tokio::sync::broadcast;
    use tokio::time::{sleep, timeout};

    fn ffmpeg() -> Option<PathBuf> {
        match std::env::var_os("CLIPVAULT_FFMPEG") {
            Some(p) => Some(PathBuf::from(p)),
            None => {
                eprintln!("SKIP: CLIPVAULT_FFMPEG not set; supervisor integration test skipped");
                None
            }
        }
    }

    fn spec(dir: &Path) -> CaptureSpec {
        CaptureSpec {
            output_idx: 0,
            fps: 30,
            out_w: 320,
            out_h: 180,
            hdr_sdr_white: None,
            ddagrab_fallback: false,
            encoder: Encoder::H264Nvenc,
            bitrate_kbps: 1000,
            audio_inputs: vec![
                AudioInput::Silence { title: "Mic" },
                AudioInput::Silence { title: "Discord" },
                AudioInput::Silence { title: "Game/System" },
            ],
            segment_dir: dir.to_path_buf(),
            segment_prefix: "ignored_".into(),
            video_source: VideoSource::TestSrc,
            wallclock_origin_us: 0, // set per run by the caller / supervisor
        }
    }

    fn files_ending(dir: &Path, suffix: &str) -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.to_string_lossy().ends_with(suffix))
            .collect()
    }

    /// PIDs of ffmpeg.exe processes whose parent is this test process.
    fn ffmpeg_children() -> Vec<u32> {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
        };
        let me = std::process::id();
        let mut out = Vec::new();
        // SAFETY: standard Toolhelp snapshot walk.
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).unwrap();
            let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
            let mut ok = Process32FirstW(snap, &mut e).is_ok();
            while ok {
                let len = e.szExeFile.iter().position(|c| *c == 0).unwrap_or(e.szExeFile.len());
                let name = String::from_utf16_lossy(&e.szExeFile[..len]);
                if e.th32ParentProcessID == me && name.eq_ignore_ascii_case("ffmpeg.exe") {
                    out.push(e.th32ProcessID);
                }
                ok = Process32NextW(snap, &mut e).is_ok();
            }
            let _ = CloseHandle(snap);
        }
        out
    }

    fn in_job(pid: u32) -> bool {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
        // SAFETY: open, query, close.
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).unwrap();
            let r = clipvault_lib::capture::job::in_kill_job(h.0);
            let _ = CloseHandle(h);
            r
        }
    }

    fn all_paths(idx: &Arc<Mutex<SegmentIndex>>) -> Vec<PathBuf> {
        let far = SystemTime::now() + Duration::from_secs(3600);
        idx.lock().unwrap().select(SystemTime::UNIX_EPOCH, far).segments.into_iter().map(|s| s.path).collect()
    }

    /// Waits until a `HealthChanged` event matching `pred` arrives.
    async fn wait_health(rx: &mut broadcast::Receiver<SupervisorEvent>, within: Duration, pred: impl Fn(&Health) -> bool) -> Health {
        timeout(within, async {
            loop {
                match rx.recv().await {
                    Ok(SupervisorEvent::HealthChanged(h)) if pred(&h) => return h,
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(e) => panic!("event channel: {e}"),
                }
            }
        })
        .await
        .expect("timed out waiting for health change")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn produces_segments() {
        let Some(ff) = ffmpeg() else { return };
        let dir = tempfile::tempdir().unwrap();
        // Leftovers from a previous session must be wiped; unrelated files kept
        // (also ones that merely end like ours, C2).
        std::fs::write(dir.path().join("s1a2b-1_r9_seg_00000000.ts"), b"old").unwrap();
        std::fs::write(dir.path().join("s1a2b-1_r9_list.csv"), b"old").unwrap();
        std::fs::write(dir.path().join("s1a2b-1_r9_ffmpeg.log"), b"old").unwrap();
        std::fs::write(dir.path().join("keep.txt"), b"mine").unwrap();
        std::fs::write(dir.path().join("video.ts"), b"mine").unwrap();
        std::fs::write(dir.path().join("shopping list.csv"), b"mine").unwrap();

        let (tx, _rx) = broadcast::channel(256);
        let sup = Supervisor::new(ff, Duration::from_secs(600), tx);
        sup.start(spec(dir.path())).await;
        assert!(!dir.path().join("s1a2b-1_r9_seg_00000000.ts").exists());
        assert!(!dir.path().join("s1a2b-1_r9_list.csv").exists());
        assert!(!dir.path().join("s1a2b-1_r9_ffmpeg.log").exists());
        assert!(dir.path().join("keep.txt").exists());
        assert!(dir.path().join("video.ts").exists());
        assert!(dir.path().join("shopping list.csv").exists());

        sleep(Duration::from_secs(7)).await;
        let paths = all_paths(&sup.segments());
        assert!(paths.len() >= 3, "only {} segments: {paths:?}", paths.len());
        // Wall-clock pacing (`-re`): ~3 segments in 7 s, and segment times track real time.
        assert!(paths.len() <= 5, "too many segments, source not realtime: {paths:?}");
        let latest = sup.segments().lock().unwrap().latest_end().unwrap();
        let lag = SystemTime::now().duration_since(latest).unwrap_or_default();
        assert!(lag < Duration::from_secs(4), "latest segment end lags wall clock by {lag:?}");
        for p in &paths {
            assert!(p.exists(), "{p:?} missing");
            let name = p.file_name().unwrap().to_str().unwrap();
            assert!(name.starts_with('s') && name.contains("_r1_seg_"), "{p:?}");
        }
        assert_eq!(sup.health(), Health::Recording);
        assert_eq!(files_ending(dir.path(), "_r1_ffmpeg.log").len(), 1);
        // The FFmpeg child is in the kill-on-close job (dies with us on a crash).
        let kids = ffmpeg_children();
        assert!(!kids.is_empty(), "no FFmpeg child found");
        for pid in kids {
            assert!(in_job(pid), "FFmpeg {pid} not in the kill-on-close job");
        }

        sup.stop().await;
        assert_eq!(sup.health(), Health::Idle);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn kill_then_recovers() {
        let Some(ff) = ffmpeg() else { return };
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = broadcast::channel(256);
        let sup = Supervisor::new(ff, Duration::from_secs(600), tx);
        sup.start(spec(dir.path())).await;
        wait_health(&mut rx, Duration::from_secs(8), |h| *h == Health::Recording).await;
        sleep(Duration::from_secs(3)).await;
        let before = all_paths(&sup.segments());
        assert!(!before.is_empty());

        sup.kill_child_for_test().await;
        let h = wait_health(&mut rx, Duration::from_secs(3), |h| *h != Health::Recording).await;
        assert!(matches!(h, Health::Restarting { .. }), "{h:?}");
        wait_health(&mut rx, Duration::from_secs(8), |h| *h == Health::Recording).await;

        for p in &before {
            assert!(p.exists(), "pre-kill segment {p:?} lost");
        }
        let after = all_paths(&sup.segments());
        assert!(after.iter().any(|p| p.file_name().unwrap().to_str().unwrap().contains("_r2_seg_")), "{after:?}");
        assert!(before.iter().all(|p| after.contains(p)), "index dropped old segments");
        sup.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retention_deletes_old() {
        let Some(ff) = ffmpeg() else { return };
        let dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let sup = Supervisor::new(ff, Duration::from_secs(6), tx);
        sup.start(spec(dir.path())).await;
        sleep(Duration::from_secs(16)).await;

        let oldest = [files_ending(dir.path(), "_r1_seg_00000000.ts"), files_ending(dir.path(), "_r1_seg_00000001.ts")];
        assert!(oldest.iter().all(|v| v.is_empty()), "oldest segments not deleted: {oldest:?}");
        let paths = all_paths(&sup.segments());
        assert!(!paths.iter().any(|p| p.to_string_lossy().ends_with("_r1_seg_00000000.ts")));
        assert!(!paths.is_empty() && paths.len() <= 5, "{paths:?}");
        for p in &paths {
            assert!(p.exists(), "{p:?} missing");
        }
        sup.stop().await;
    }
}
