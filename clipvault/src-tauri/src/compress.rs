//! Background compression: re-encodes saved clips to AV1 to save disk space.
//!
//! One clip at a time on its own thread, FFmpeg at idle CPU priority, and it
//! waits while a game is being recorded, so it never costs frames: a game
//! starting mid-encode stops the encode, which is redone afterwards. FFmpeg is
//! in the kill-on-close job (it dies with ClipVault) and has a timeout. Audio and
//! metadata are copied untouched; only the video is re-encoded. The result
//! replaces the original file only when it is clearly smaller; either way the
//! clip is marked `compressed` and not tried again.
//!
//! Encoder: NVIDIA AV1 when this PC has it (fast), otherwise SVT-AV1 on the
//! CPU, which every PC can run (slower, but at idle priority). AMD/Intel AV1
//! are left out: their quality scales differ between drivers.
use crate::db::clips::{self, Clip};
use crate::locks;
use crate::settings::Encoder;
use anyhow::{bail, Result};
use rusqlite::Connection;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// `CREATE_NO_WINDOW | IDLE_PRIORITY_CLASS`: no console window, and FFmpeg only
/// gets CPU time nothing else wants.
#[cfg(windows)]
const CREATION_FLAGS: u32 = 0x0800_0000 | 0x0000_0040;

/// The result must be at most this share of the original, or it is discarded.
const KEEP_RATIO: f64 = 0.9;
/// Replacing a clip that is open in the player fails; retried for ~10 min.
const RENAME_ATTEMPTS: u32 = 30;
const RENAME_RETRY: Duration = Duration::from_secs(20);
/// How often a waiting job checks whether the game has ended.
const BUSY_POLL: Duration = Duration::from_secs(15);
/// How often a running encode checks whether a game started / it timed out.
const RUN_POLL: Duration = Duration::from_millis(250);
/// Encode timeout: this much, plus `TIMEOUT_PER_CLIP_S` seconds per clip second.
const TIMEOUT_BASE: Duration = Duration::from_secs(600);
const TIMEOUT_PER_CLIP_S: f64 = 30.0;

/// Called after a clip changed (new size / compressed flag), for the UI.
pub type OnUpdate = Arc<dyn Fn(&Clip) + Send + Sync>;
/// True while compression should wait (a game is being recorded).
pub type IsBusy = Arc<dyn Fn() -> bool + Send + Sync>;

pub struct Compressor {
    ffmpeg: Option<PathBuf>,
    db: Arc<Mutex<Connection>>,
    tx: mpsc::Sender<String>,
    rx: Mutex<Option<mpsc::Receiver<String>>>,
    /// Ids waiting or running, so a clip is never queued twice.
    queued: Arc<Mutex<HashSet<String>>>,
    on_update: Arc<Mutex<Option<OnUpdate>>>,
}

/// The encoder for compression given the recording encoders this PC can run.
pub fn compress_encoder(available: &[Encoder]) -> &'static str {
    if available.contains(&Encoder::Av1Nvenc) { "av1_nvenc" } else { "libsvtav1" }
}

/// FFmpeg arguments: every stream copied, video re-encoded to AV1, mp4 with
/// faststart (the file is written as `out`, hence the explicit `-f mp4`).
pub fn compress_args(input: &Path, out: &Path, encoder: &str) -> Vec<String> {
    let mut a: Vec<String> = ["-hide_banner", "-loglevel", "error", "-y", "-i"].iter().map(|s| s.to_string()).collect();
    a.push(input.display().to_string());
    let video: &[&str] = if encoder == "av1_nvenc" {
        &["-c:v", "av1_nvenc", "-preset", "p6", "-tune", "hq", "-rc", "vbr", "-cq", "34", "-b:v", "0", "-bf", "3", "-spatial-aq", "1", "-temporal-aq", "1"]
    } else {
        &["-c:v", "libsvtav1", "-preset", "8", "-crf", "34", "-pix_fmt", "yuv420p"]
    };
    a.extend(["-map", "0", "-c", "copy"].iter().map(|s| s.to_string()));
    a.extend(video.iter().map(|s| s.to_string()));
    a.extend(["-map_metadata", "0", "-movflags", "+faststart", "-f", "mp4"].iter().map(|s| s.to_string()));
    a.push(out.display().to_string());
    a
}

/// Temporary output next to the clip (same volume, so the swap is a rename).
pub fn temp_path(clip: &Path) -> PathBuf {
    let mut name = clip.file_name().unwrap_or_default().to_os_string();
    name.push(".compress.tmp");
    clip.with_file_name(name)
}

/// Whether the compressed file is worth keeping.
pub fn worth_keeping(original: u64, compressed: u64) -> bool {
    compressed > 0 && (compressed as f64) <= original as f64 * KEEP_RATIO
}

impl Compressor {
    pub fn new(ffmpeg: Option<PathBuf>, db: Arc<Mutex<Connection>>) -> Arc<Compressor> {
        let (tx, rx) = mpsc::channel();
        Arc::new(Compressor {
            ffmpeg,
            db,
            tx,
            rx: Mutex::new(Some(rx)),
            queued: Arc::new(Mutex::new(HashSet::new())),
            on_update: Arc::new(Mutex::new(None)),
        })
    }

    pub fn set_on_update(&self, f: OnUpdate) {
        *locks::lock(&self.on_update) = Some(f);
    }

    /// Queues one clip (no-op if already queued). Runs once [`Compressor::start`] was called.
    pub fn enqueue(&self, id: &str) {
        if locks::lock(&self.queued).insert(id.to_string()) {
            let _ = self.tx.send(id.to_string());
        }
    }

    /// Queues every native clip not compressed yet, oldest first.
    pub fn enqueue_pending(&self) {
        let ids = clips::uncompressed_ids(&locks::lock(&self.db));
        match ids {
            Ok(ids) => {
                if !ids.is_empty() {
                    tracing::info!("[compress] {} clip(s) queued", ids.len());
                }
                for id in ids {
                    self.enqueue(&id);
                }
            }
            Err(e) => tracing::error!("[compress] listing clips: {e:#}"),
        }
    }

    /// Starts the worker thread (once). `available`: recording encoders this PC
    /// can run (picks the AV1 encoder); `busy`: wait while it returns true.
    pub fn start(&self, available: Vec<Encoder>, busy: IsBusy) {
        let Some(rx) = locks::lock(&self.rx).take() else { return };
        let Some(ffmpeg) = self.ffmpeg.clone() else { return };
        let encoder = compress_encoder(&available);
        let (db, queued, on_update) = (self.db.clone(), self.queued.clone(), self.on_update.clone());
        tracing::info!("[compress] worker ready, encoder {encoder}");
        let spawned = std::thread::Builder::new().name("compress".into()).spawn(move || {
            for id in rx {
                // Panics stay inside this job: the worker and the queue live on.
                let result = loop {
                    while busy() {
                        std::thread::sleep(BUSY_POLL);
                    }
                    let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        compress_clip(&ffmpeg, &db, &id, encoder, &*busy)
                    }));
                    match run {
                        Ok(Ok(Outcome::Interrupted)) => continue,
                        Ok(r) => break r,
                        Err(_) => break Err(anyhow::anyhow!("internal error (panic) while compressing")),
                    }
                };
                match result {
                    Ok(Outcome::Done(Some(clip))) => {
                        if let Some(f) = locks::lock(&on_update).clone() {
                            f(&clip);
                        }
                    }
                    Ok(_) => {}
                    Err(e) => tracing::error!("[compress] clip {id}: {e:#}"),
                }
                locks::lock(&queued).remove(&id);
            }
        });
        if let Err(e) = spawned {
            tracing::error!("[compress] cannot start worker: {e}");
        }
    }
}

/// How a watched FFmpeg run ended.
#[derive(Debug)]
pub enum Run {
    Finished { success: bool, stderr: String },
    /// `busy` turned true: the process was killed.
    Busy,
    /// Still running after the timeout: the process was killed.
    TimedOut,
}

pub enum Outcome {
    /// `None`: nothing to do (gone, foreign, done).
    Done(Option<Clip>),
    /// A game started; try again once it ended.
    Interrupted,
}

/// Runs `program` in the kill-on-close job and waits for it, killing (and
/// reaping) it when `busy()` turns true or after `timeout`. `on_spawn` gets the
/// pid and whether it is in the job (for tests). Spawn errors are `Finished`
/// with `success == false`.
pub fn run_watched(
    program: &Path,
    args: &[String],
    timeout: Duration,
    busy: &dyn Fn() -> bool,
    on_spawn: impl FnOnce(u32, bool),
) -> Run {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    cmd.creation_flags(CREATION_FLAGS);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Run::Finished { success: false, stderr: format!("starting {}: {e}", program.display()) },
    };
    #[cfg(windows)]
    let in_job = {
        use std::os::windows::io::AsRawHandle;
        let h = child.as_raw_handle();
        if let Err(e) = crate::capture::job::assign(h) {
            tracing::warn!("[compress] FFmpeg not in kill-on-close job: {e}");
        }
        crate::capture::job::in_kill_job(h)
    };
    #[cfg(not(windows))]
    let in_job = false;
    on_spawn(child.id(), in_job);
    // Drain stderr on its own thread so a chatty FFmpeg never blocks on a full pipe.
    let stderr = child.stderr.take().map(|mut s| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = std::io::Read::read_to_string(&mut s, &mut buf);
            buf
        })
    });
    let deadline = std::time::Instant::now() + timeout;
    let stopped = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stderr = stderr.and_then(|t| t.join().ok()).unwrap_or_default();
                return Run::Finished { success: status.success(), stderr };
            }
            Ok(None) => {}
            Err(e) => break Run::Finished { success: false, stderr: format!("waiting for FFmpeg: {e}") },
        }
        if busy() {
            break Run::Busy;
        }
        if std::time::Instant::now() >= deadline {
            break Run::TimedOut;
        }
        std::thread::sleep(RUN_POLL);
    };
    let _ = child.kill();
    let _ = child.wait();
    stopped
}

/// Removes leftovers of interrupted saves (`*.mp4.tmp`) and compressions
/// (`*.mp4.compress.tmp`) directly in `dir`; returns how many. Call before
/// any save or compression can run.
pub fn clean_stale_temps(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    let mut removed = 0;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_lowercase();
        let stale = name.ends_with(".mp4.tmp") || name.ends_with(".mp4.compress.tmp");
        if stale && e.file_type().is_ok_and(|t| t.is_file()) {
            match std::fs::remove_file(e.path()) {
                Ok(()) => {
                    tracing::info!("removed stale temp file {:?}", e.path());
                    removed += 1;
                }
                Err(err) => tracing::warn!("could not remove stale temp file {:?}: {err}", e.path()),
            }
        }
    }
    removed
}

/// Compresses one clip. `Ok(None)`: nothing to do (gone, foreign, done).
pub fn compress_clip(ffmpeg: &Path, db: &Mutex<Connection>, id: &str, encoder: &str, busy: &dyn Fn() -> bool) -> Result<Outcome> {
    let Some(clip) = clips::get(&locks::lock(db), id)? else { return Ok(Outcome::Done(None)) };
    if clip.compressed || clip.source != "native" || !clip.path.is_file() {
        return Ok(Outcome::Done(None));
    }
    let original = std::fs::metadata(&clip.path)?.len();
    let tmp = temp_path(&clip.path);
    let started = std::time::Instant::now();
    let timeout = TIMEOUT_BASE + Duration::from_secs_f64(clip.duration_s.max(0.0) * TIMEOUT_PER_CLIP_S);
    match run_watched(ffmpeg, &compress_args(&clip.path, &tmp, encoder), timeout, busy, |_, _| {}) {
        Run::Finished { success: true, .. } => {}
        Run::Finished { success: false, stderr } => {
            let _ = std::fs::remove_file(&tmp);
            // Mark it anyway: a clip FFmpeg cannot read would fail on every start.
            clips::set_compressed(&locks::lock(db), id, original)?;
            bail!("FFmpeg failed: {}", stderr.lines().last().unwrap_or("").trim());
        }
        Run::Busy => {
            let _ = std::fs::remove_file(&tmp);
            tracing::info!("[compress] {}: a game started; stopped, redone later", clip.path.display());
            return Ok(Outcome::Interrupted);
        }
        Run::TimedOut => {
            let _ = std::fs::remove_file(&tmp);
            // Marked so a clip that hangs FFmpeg is not retried on every start.
            clips::set_compressed(&locks::lock(db), id, original)?;
            bail!("FFmpeg still running after {} s; stopped", timeout.as_secs());
        }
    }
    let new = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
    let keep = worth_keeping(original, new);
    let mut attempt = 0;
    let (conn, size) = loop {
        let conn = locks::lock(db);
        // Re-check under the lock: the clip may have been deleted meanwhile,
        // and the rename must not bring its file back.
        let still = clips::get(&conn, id)?.filter(|c| c.path == clip.path && c.path.is_file());
        if still.is_none() {
            let _ = std::fs::remove_file(&tmp);
            return Ok(Outcome::Done(None));
        }
        if !keep {
            let _ = std::fs::remove_file(&tmp);
            break (conn, original);
        }
        match std::fs::rename(&tmp, &clip.path) {
            Ok(()) => break (conn, new),
            // Typically the clip is open in the player: wait for it to close.
            Err(_) if attempt < RENAME_ATTEMPTS => {
                drop(conn);
                attempt += 1;
                std::thread::sleep(RENAME_RETRY);
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                bail!("replacing {}: {e} (tried again on the next start)", clip.path.display());
            }
        }
    };
    clips::set_compressed(&conn, id, size)?;
    tracing::info!(
        "[compress] {}: {:.1} MB -> {:.1} MB in {:.0} s{}",
        clip.path.display(),
        original as f64 / 1e6,
        size as f64 / 1e6,
        started.elapsed().as_secs_f64(),
        if size == original { " (kept original: not smaller)" } else { "" }
    );
    Ok(Outcome::Done(clips::get(&conn, id)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvidia_av1_preferred_else_cpu() {
        assert_eq!(compress_encoder(&[Encoder::Av1Nvenc, Encoder::X264]), "av1_nvenc");
        assert_eq!(compress_encoder(&[Encoder::HevcNvenc, Encoder::X264]), "libsvtav1");
        assert_eq!(compress_encoder(&[Encoder::Av1Amf]), "libsvtav1");
        assert_eq!(compress_encoder(&[]), "libsvtav1");
    }

    #[test]
    fn args_copy_audio_and_reencode_video() {
        let a = compress_args(Path::new(r"C:\c\a.mp4"), Path::new(r"C:\c\a.mp4.compress.tmp"), "libsvtav1").join(" ");
        assert!(a.contains(r"-i C:\c\a.mp4 -map 0 -c copy -c:v libsvtav1 -preset 8 -crf 34"), "{a}");
        assert!(a.ends_with(r"-map_metadata 0 -movflags +faststart -f mp4 C:\c\a.mp4.compress.tmp"), "{a}");
        let n = compress_args(Path::new("a.mp4"), Path::new("b"), "av1_nvenc").join(" ");
        assert!(n.contains("-c:v av1_nvenc -preset p6 -tune hq -rc vbr -cq 34 -b:v 0"), "{n}");
    }

    #[test]
    fn keeps_only_clearly_smaller_results() {
        assert!(worth_keeping(100, 50));
        assert!(worth_keeping(100, 90));
        assert!(!worth_keeping(100, 91));
        assert!(!worth_keeping(100, 0));
    }

    #[test]
    fn temp_file_sits_next_to_the_clip() {
        assert_eq!(temp_path(Path::new(r"E:\Clips\x.mp4")), PathBuf::from(r"E:\Clips\x.mp4.compress.tmp"));
    }

    #[cfg(windows)]
    fn ping() -> (PathBuf, Vec<String>) {
        let ping = PathBuf::from(std::env::var("SystemRoot").unwrap()).join(r"System32\PING.EXE");
        (ping, ["-n", "30", "127.0.0.1"].iter().map(|s| s.to_string()).collect())
    }

    #[cfg(windows)]
    fn gone(pid: u32) -> bool {
        let out = std::process::Command::new("tasklist").args(["/FI", &format!("PID eq {pid}"), "/NH"]).output().unwrap();
        !String::from_utf8_lossy(&out.stdout).contains(&pid.to_string())
    }

    /// A game starting mid-encode stops the encode at once (it would share
    /// NVENC / CPU with the recorder); the process is in the kill-on-close job
    /// and is gone afterwards.
    #[cfg(windows)]
    #[test]
    fn encode_stops_when_a_game_starts() {
        let (prog, args) = ping();
        let started = std::time::Instant::now();
        let busy = move || started.elapsed() > Duration::from_millis(300);
        let mut seen = None;
        let r = run_watched(&prog, &args, Duration::from_secs(60), &busy, |pid, in_job| seen = Some((pid, in_job)));
        assert!(matches!(r, Run::Busy), "{r:?}");
        assert!(started.elapsed() < Duration::from_secs(10));
        let (pid, in_job) = seen.unwrap();
        assert!(in_job, "compression FFmpeg must die with ClipVault");
        assert!(gone(pid), "process {pid} still running");
    }

    #[cfg(windows)]
    #[test]
    fn hung_encode_times_out_and_is_killed() {
        let (prog, args) = ping();
        let mut pid = 0;
        let r = run_watched(&prog, &args, Duration::from_millis(500), &|| false, |p, _| pid = p);
        assert!(matches!(r, Run::TimedOut), "{r:?}");
        assert!(gone(pid), "process {pid} still running");
    }

    /// Leftovers of an interrupted save or compression (quit, crash, power
    /// loss) are removed on start; nothing else is touched.
    #[test]
    fn stale_temp_files_are_cleaned() {
        let dir = tempfile::tempdir().unwrap();
        let stale = ["a.mp4.compress.tmp", "RL__2026-11-18__21-37-14.mp4.tmp"];
        let keep = ["a.mp4", "notes.tmp", "b.mp4.tmp.mp4"];
        for n in stale.iter().chain(&keep) {
            std::fs::write(dir.path().join(n), b"x").unwrap();
        }
        assert_eq!(clean_stale_temps(dir.path()), 2);
        for n in stale {
            assert!(!dir.path().join(n).exists(), "{n}");
        }
        for n in keep {
            assert!(dir.path().join(n).exists(), "{n}");
        }
        assert_eq!(clean_stale_temps(&dir.path().join("missing")), 0);
    }

    #[test]
    fn skips_missing_foreign_and_done_clips() {
        let db = Mutex::new(crate::db::open_memory().unwrap());
        let r = compress_clip(Path::new("ffmpeg.exe"), &db, "nope", "libsvtav1", &|| false).unwrap();
        assert!(matches!(r, Outcome::Done(None)));
    }
}
