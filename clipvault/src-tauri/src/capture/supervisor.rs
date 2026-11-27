//! Capture supervisor: runs FFmpeg, tails its segment list into the shared
//! [`SegmentIndex`], prunes old segments and restarts FFmpeg when it exits or
//! stalls, without touching segments that are already on disk.
//!
//! Design notes:
//! * Every (re)start is a new *run* with prefix `s{session}_r{run}_` (session =
//!   process id + launch time + start counter, see [`session_token`]), so a
//!   file left by another process or start never matches the current run;
//!   segment files, the CSV list and the stderr log (`{prefix}ffmpeg.log`) of
//!   older runs are never overwritten. Logs sit next to the segments so the
//!   startup wipe cleans them too. The wipe deletes only names of exactly that
//!   shape ([`is_own_buffer_file`]); the app also points `segment_dir` at a
//!   dedicated `ClipVault-buffer` subfolder ([`Settings::segment_dir`]).
//!
//! [`Settings::segment_dir`]: crate::settings::Settings::segment_dir
//! * Every input of a run is timed against `run_start`, the `SystemTime` taken
//!   just before spawning FFmpeg (`CaptureSpec::wallclock_origin_us`, see
//!   args.rs), so segment times in the CSV are `wall time - run_start` and
//!   `run_start + t` is the capture wall time. Only the first segment's start
//!   is reported as 0 rather than its first frame (a few tens of ms later).
//! * Pruning only happens when a new segment arrives. While capture is broken
//!   the last footage is therefore kept rather than aged out.
//! * All restart decisions live in the pure [`WatchdogState::on_tick`]: restart
//!   when FFmpeg exited or no segment closed for 5 s (measured from the later of
//!   the last segment and the process start, giving every start a grace
//!   period); more than 3 restarts in a sliding 60 s window ⇒ `Fail`. After a
//!   `Fail` the watchdog keeps retrying, but only every 10 s, and health stays
//!   `Failing` until a segment from a fresh process arrives.
//! * Health is `Restarting { reason: "starting" }` from `start` until the first
//!   segment closes; it becomes `Recording` on every segment arrival.
//! * Desktop kick (ddagrab only): Desktop Duplication delivers no frame until
//!   the screen changes, so on a static desktop FFmpeg would sit waiting and
//!   the watchdog would restart it forever. Every watchdog tick calls the
//!   installed [`Kick`] for the captured output while a fresh process has had
//!   no segment for [`KICK_AFTER`], or, once it has, after
//!   [`KICK_AFTER_SEGMENT`] without one (never at the normal 2 s cadence).
//!   At most [`MAX_KICKS_PER_PROCESS`] kicks per FFmpeg process and none while
//!   the watchdog is failing ([`KickBudget`]).
//! * The supervision task is itself watched (`supervise_guarded`): a panic
//!   sets `Failing { "internal error – restarting capture" }` and restarts it,
//!   bounded like FFmpeg restarts ([`panic_restart_delay`]).
//! * FFmpeg runs in a kill-on-close Job Object (`capture::job`), so it dies
//!   with ClipVault even on a crash or force-kill.
//! * A restart/fail reason carries the root cause: spawn error, exit status,
//!   FFmpeg's first error line and last line, and the run's log path.
//! * When a run ends, its list CSV and any segment file that never made it
//!   into the index (the partial segment of a killed process) are deleted
//!   ([`clean_ended_run`]); only the newest [`KEEP_RUN_LOGS`] FFmpeg logs of a
//!   session are kept. A watchdog failing for days therefore leaves a bounded
//!   number of files, not one set per retry.
use crate::capture::args::{build_capture_args, hdr_pipe_name, layout_key, CaptureSpec, VideoSource};
use crate::capture::hdr_grab::{HdrGrabConfig, HdrGrabber};
use crate::capture::segments::{parse_segment_list_line, Segment, SegmentIndex};
use crate::locks;
use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime};
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, Notify};
use tokio::task::JoinHandle;
use tokio::time::{interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

/// No closed segment for this long ⇒ FFmpeg is considered stalled.
pub const STALL_TIMEOUT: Duration = Duration::from_secs(5);
/// Sliding window for counting restarts.
pub const RESTART_WINDOW: Duration = Duration::from_secs(60);
/// More than this many restarts inside [`RESTART_WINDOW`] ⇒ `Fail`.
pub const MAX_RESTARTS_IN_WINDOW: usize = 3;
/// Delay between restart attempts while failing.
pub const FAIL_BACKOFF: Duration = Duration::from_secs(10);

const LIST_POLL: Duration = Duration::from_millis(250);
/// FFmpeg logs of ended runs kept per session (the newest), for diagnosis.
pub const KEEP_RUN_LOGS: usize = 10;
const WATCHDOG_TICK: Duration = Duration::from_secs(1);
/// `CREATE_NO_WINDOW`: no console window flashes up for FFmpeg.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    Idle,
    Recording,
    Restarting { reason: String },
    Failing { reason: String },
}

#[derive(Debug, Clone)]
pub enum SupervisorEvent {
    HealthChanged(Health),
    SegmentClosed(Segment),
    /// Message from the window grabber for the user (e.g. fallback to the monitor).
    Notice(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchdogAction {
    None,
    /// Kill (if needed) and respawn FFmpeg now; health `Restarting`.
    Restart(String),
    /// Kill (if needed) and respawn FFmpeg now; health `Failing`. Further
    /// attempts are spaced by [`FAIL_BACKOFF`] until a segment arrives.
    Fail(String),
}

/// Pure watchdog decision logic. Assumes the caller acts on every
/// `Restart`/`Fail` immediately: the returned action marks a new process start.
#[derive(Debug)]
pub struct WatchdogState {
    process_started: Instant,
    restarts: VecDeque<Instant>,
    failing: bool,
}

impl WatchdogState {
    pub fn new(process_started: Instant) -> Self {
        Self { process_started, restarts: VecDeque::new(), failing: false }
    }

    pub fn is_failing(&self) -> bool {
        self.failing
    }

    pub fn on_tick(&mut self, now: Instant, last_segment: Option<Instant>, process_alive: bool) -> WatchdogAction {
        if self.failing && last_segment.is_some_and(|t| t > self.process_started) {
            self.failing = false;
        }

        let reference = last_segment.map_or(self.process_started, |t| t.max(self.process_started));
        let idle = now.saturating_duration_since(reference);
        let reason = if !process_alive {
            "FFmpeg exited".to_string()
        } else if idle >= STALL_TIMEOUT {
            format!("no new segment for {} s", idle.as_secs())
        } else {
            return WatchdogAction::None;
        };

        if self.failing {
            if now.saturating_duration_since(self.process_started) < FAIL_BACKOFF {
                return WatchdogAction::None;
            }
            self.process_started = now;
            return WatchdogAction::Fail(reason);
        }

        while self.restarts.front().is_some_and(|&t| now.saturating_duration_since(t) >= RESTART_WINDOW) {
            self.restarts.pop_front();
        }
        self.restarts.push_back(now);
        self.process_started = now;
        if self.restarts.len() > MAX_RESTARTS_IN_WINDOW {
            self.failing = true;
            WatchdogAction::Fail(format!("{reason} ({} restarts within 60 s)", self.restarts.len()))
        } else {
            WatchdogAction::Restart(reason)
        }
    }
}

/// A new FFmpeg process that has produced no segment after this long gets a
/// desktop kick every watchdog tick: ddagrab delivers nothing until the screen
/// changes.
pub const KICK_AFTER: Duration = Duration::from_secs(2);
/// Once this process has produced a segment, kick only after this long without
/// one: above the 2 s segment period plus 250 ms poll jitter, below the 5 s stall.
pub const KICK_AFTER_SEGMENT: Duration = Duration::from_millis(3500);

/// Whether to kick the desktop on this watchdog tick.
pub fn kick_due(now: Instant, process_started: Instant, last_segment: Option<Instant>, alive: bool) -> bool {
    if !alive {
        return false;
    }
    match last_segment.filter(|&t| t > process_started) {
        // This process has produced nothing yet: ddagrab may be waiting for a first frame.
        None => now.saturating_duration_since(process_started) >= KICK_AFTER,
        // Recording: only a gap well beyond the segment period + poll jitter.
        Some(t) => now.saturating_duration_since(t) >= KICK_AFTER_SEGMENT,
    }
}

/// At most this many desktop kicks per FFmpeg process.
pub const MAX_KICKS_PER_PROCESS: u32 = 3;

/// What to do when [`kick_due`] says a kick is due.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KickDecision {
    /// Kick; `first` = first kick of this process (logged).
    Kick { first: bool },
    /// The cap was just reached: log it once, no kick.
    CapReached,
    Skip,
}

/// Per-process kick budget (I7): at most [`MAX_KICKS_PER_PROCESS`] kicks, none
/// while the watchdog is failing (repeated kicks would flash the kick window
/// over a game that capture cannot see anyway).
#[derive(Debug, Default)]
pub struct KickBudget {
    kicks: u32,
    cap_logged: bool,
}

impl KickBudget {
    pub fn kicks(&self) -> u32 {
        self.kicks
    }

    pub fn on_due(&mut self, failing: bool) -> KickDecision {
        if failing {
            return KickDecision::Skip;
        }
        if self.kicks >= MAX_KICKS_PER_PROCESS {
            if self.cap_logged {
                return KickDecision::Skip;
            }
            self.cap_logged = true;
            return KickDecision::CapReached;
        }
        self.kicks += 1;
        KickDecision::Kick { first: self.kicks == 1 }
    }
}

/// Normal delay before supervision restarts after a panic.
pub const PANIC_RESTART_DELAY: Duration = Duration::from_secs(1);

/// Delay before supervision restarts after it panicked (I4), with the same
/// bound as FFmpeg restarts: more than [`MAX_RESTARTS_IN_WINDOW`] panics in
/// [`RESTART_WINDOW`] ⇒ [`FAIL_BACKOFF`] between attempts. `history` holds
/// the earlier panic times.
pub fn panic_restart_delay(history: &mut VecDeque<Instant>, now: Instant) -> Duration {
    while history.front().is_some_and(|&t| now.saturating_duration_since(t) >= RESTART_WINDOW) {
        history.pop_front();
    }
    history.push_back(now);
    if history.len() > MAX_RESTARTS_IN_WINDOW { FAIL_BACKOFF } else { PANIC_RESTART_DELAY }
}

/// Health reason while supervision restarts after a panic.
pub const SUPERVISION_PANIC_REASON: &str = "internal error – restarting capture";

/// Per-`start` session token: launch (process id + start time) and start
/// counter, so files of an older process or start never match the current run.
pub fn session_token(start_no: u64) -> String {
    static LAUNCH: OnceLock<String> = OnceLock::new();
    let launch = LAUNCH.get_or_init(|| {
        let ms = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_millis());
        format!("{:x}{:x}", std::process::id(), ms)
    });
    format!("{launch}-{start_no}")
}

/// File prefix of run `run` in session `session`.
pub fn run_prefix(session: &str, run: u64) -> String {
    format!("s{session}_r{run}_")
}

/// The FFmpeg line that best explains a failure: the first line mentioning an
/// error (the root cause, e.g. `Failed to enumerate DXGI output 1`), with the
/// `[filter @ 0x...] ` prefix stripped; else the last non-empty line.
pub fn root_cause_line(stderr: &str) -> Option<String> {
    let strip = |l: &str| -> String {
        let l = l.trim();
        match (l.starts_with('['), l.find("] ")) {
            (true, Some(i)) => l[i + 2..].trim().to_string(),
            _ => l.to_string(),
        }
    };
    let lines: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
    let is_error = |l: &&&str| {
        let l = l.to_ascii_lowercase();
        ["error", "failed", "invalid", "cannot", "could not", "unable"].iter().any(|k| l.contains(k))
    };
    lines.iter().find(is_error).or(lines.last()).map(|l| strip(l))
}

/// Last non-empty line of `stderr`, prefix stripped like [`root_cause_line`].
fn last_line(stderr: &str) -> Option<String> {
    stderr.lines().rev().find(|l| !l.trim().is_empty()).and_then(root_cause_line)
}

/// Root-cause details appended to a watchdog reason: spawn error, exit status,
/// FFmpeg's own explanation and where its log is.
pub fn failure_detail(spawn_error: Option<&str>, exit: Option<&str>, stderr: &str, log: &Path) -> String {
    if let Some(e) = spawn_error {
        return format!("could not start FFmpeg: {e}");
    }
    let mut parts: Vec<String> = Vec::new();
    if let Some(x) = exit {
        parts.push(x.to_string());
    }
    if let Some(root) = root_cause_line(stderr) {
        match last_line(stderr) {
            Some(last) if last != root => parts.push(format!("FFmpeg: {root} | {last}")),
            _ => parts.push(format!("FFmpeg: {root}")),
        }
    }
    parts.push(format!("log: {}", log.display()));
    parts.join("; ")
}

/// Incrementally reads complete lines appended to a file that may not exist yet.
pub struct ListTailer {
    path: PathBuf,
    offset: u64,
    partial: Vec<u8>,
}

impl ListTailer {
    pub fn new(path: PathBuf) -> Self {
        Self { path, offset: 0, partial: Vec::new() }
    }

    /// Complete new lines (without line terminators, empty lines skipped).
    /// I/O errors (e.g. file not created yet) yield no lines.
    pub fn poll(&mut self) -> Vec<String> {
        let Ok(mut f) = std::fs::File::open(&self.path) else { return Vec::new() };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if len < self.offset {
            // Truncated/rewritten: start over.
            self.offset = 0;
            self.partial.clear();
        }
        if f.seek(SeekFrom::Start(self.offset)).is_err() {
            return Vec::new();
        }
        let mut buf = Vec::new();
        let Ok(n) = f.read_to_end(&mut buf) else { return Vec::new() };
        self.offset += n as u64;
        self.partial.extend_from_slice(&buf);

        let mut lines = Vec::new();
        while let Some(pos) = self.partial.iter().position(|&b| b == b'\n') {
            let raw: Vec<u8> = self.partial.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&raw).trim_end_matches(['\r', '\n']).to_string();
            if !line.is_empty() {
                lines.push(line);
            }
        }
        lines
    }
}

/// Whether `name` is a file a capture run writes: `{prefix}seg_NNNNNNNN.ts`,
/// `{prefix}list.csv` or `{prefix}ffmpeg.log` with `prefix` =
/// `s{hex}-{n}_r{n}_` (see [`run_prefix`] / [`session_token`]).
pub fn is_own_buffer_file(name: &str) -> bool {
    let Some(rest) = name.strip_prefix('s') else { return false };
    let Some((session, rest)) = rest.split_once("_r") else { return false };
    let Some((launch, start_no)) = session.split_once('-') else { return false };
    let hex = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit());
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !hex(launch) || !digits(start_no) {
        return false;
    }
    let Some((run, file)) = rest.split_once('_') else { return false };
    if !digits(run) {
        return false;
    }
    match file {
        "list.csv" | "ffmpeg.log" => true,
        f => f.strip_prefix("seg_").and_then(|f| f.strip_suffix(".ts")).is_some_and(digits),
    }
}

/// Deletes our own run files (see [`is_own_buffer_file`]) directly inside
/// `dir`, creating `dir` if missing. Never recurses, never removes the
/// directory or any other file.
fn wipe_buffer_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        if is_own_buffer_file(&entry.file_name().to_string_lossy()) {
            if let Err(e) = std::fs::remove_file(entry.path()) {
                tracing::warn!("could not remove {:?}: {e}", entry.path());
            }
        }
    }
    Ok(())
}

/// Removes what an ended run leaves that nothing will read again: its list
/// CSV and its segment files not in the index (`indexed`), i.e. the partial
/// segment of a killed process. Indexed segments are the buffer and stay.
/// Call after the run's final ingest.
fn clean_ended_run(dir: &Path, prefix: &str, indexed: &dyn Fn(&Path) -> bool) {
    let list = dir.join(format!("{prefix}list.csv"));
    let _ = delete_segment(&list);
    let seg_prefix = format!("{prefix}seg_");
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.starts_with(&seg_prefix) || !is_own_buffer_file(&name) {
            continue;
        }
        let path = e.path();
        if !indexed(&path) {
            let _ = delete_segment(&path);
        }
    }
}

/// Remembers an ended run's log and deletes the oldest beyond [`KEEP_RUN_LOGS`].
fn rotate_run_logs(logs: &mut VecDeque<PathBuf>, ended: PathBuf) {
    logs.push_back(ended);
    while logs.len() > KEEP_RUN_LOGS {
        if let Some(old) = logs.pop_front() {
            let _ = delete_segment(&old);
        }
    }
}

fn unix_micros(t: SystemTime) -> i64 {
    t.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_micros() as i64)
}

fn spawn_ffmpeg(ffmpeg: &Path, spec: &CaptureSpec) -> std::io::Result<Child> {
    let log_path = spec.segment_dir.join(format!("{}ffmpeg.log", spec.segment_prefix));
    let log = std::fs::File::create(&log_path)?;
    let mut cmd = Command::new(ffmpeg);
    cmd.args(build_capture_args(spec))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let child = cmd.spawn()?;
    // Dies with ClipVault even on a crash or force-kill (see capture::job).
    #[cfg(windows)]
    if let Some(h) = child.raw_handle() {
        if let Err(e) = crate::capture::job::assign(h) {
            tracing::warn!("FFmpeg not in kill-on-close job: {e}");
        }
    }
    Ok(child)
}

/// Desktop kick: makes the captured monitor (ddagrab `output_idx`) produce a
/// new frame. Must return quickly (it runs on the supervision task).
pub type Kick = Arc<dyn Fn(u32) + Send + Sync>;

/// At most this much of the end of an FFmpeg log is read to explain a failure.
const LOG_TAIL_BYTES: u64 = 16 * 1024;

fn read_log_tail(path: &Path) -> String {
    let Ok(mut f) = std::fs::File::open(path) else { return String::new() };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = f.seek(SeekFrom::Start(len.saturating_sub(LOG_TAIL_BYTES)));
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

struct Inner {
    kick: RwLock<Option<Kick>>,
    ffmpeg: PathBuf,
    retention: Mutex<Duration>,
    segments: Arc<Mutex<SegmentIndex>>,
    /// Expired segment files that could not be deleted yet (sharing
    /// violation); retried on every ingest.
    pending_delete: Mutex<Vec<PathBuf>>,
    health: RwLock<Health>,
    events: broadcast::Sender<SupervisorEvent>,
    kill_for_test: Notify,
    /// Test hook: the supervision task panics on its next watchdog tick.
    #[cfg(test)]
    panic_for_test: std::sync::atomic::AtomicBool,
}

impl Inner {
    fn set_health(&self, h: Health) {
        let mut cur = locks::write(&self.health);
        if *cur != h {
            *cur = h.clone();
            drop(cur);
            let _ = self.events.send(SupervisorEvent::HealthChanged(h));
        }
    }

    /// Reads new list lines, indexes the segments (tagged with `layout`),
    /// prunes and deletes expired files. Returns whether any segment arrived.
    fn ingest(&self, tailer: &mut ListTailer, run_start: SystemTime, dir: &Path, layout: u64) -> bool {
        let new: Vec<Segment> = tailer
            .poll()
            .iter()
            .filter_map(|l| parse_segment_list_line(l, run_start, dir))
            .map(|s| Segment { layout, ..s })
            .collect();
        if new.is_empty() {
            return false;
        }
        let removed = {
            let mut idx = locks::lock(&self.segments);
            for s in &new {
                idx.push(s.clone());
            }
            idx.prune(SystemTime::now())
        };
        let mut doomed = std::mem::take(&mut *locks::lock(&self.pending_delete));
        doomed.extend(removed);
        let failed: Vec<PathBuf> = doomed.into_iter().filter(|p| !delete_segment(p)).collect();
        locks::lock(&self.pending_delete).extend(failed);
        for s in new {
            let _ = self.events.send(SupervisorEvent::SegmentClosed(s));
        }
        self.set_health(Health::Recording);
        true
    }
}

/// Deletes an expired segment file; false if it is still there (retry later).
fn delete_segment(p: &Path) -> bool {
    match std::fs::remove_file(p) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => {
            tracing::warn!("could not delete expired {p:?} (retried later): {e}");
            false
        }
    }
}

async fn kill(child: &mut Option<Child>) {
    if let Some(mut c) = child.take() {
        let _ = c.kill().await;
    }
}

/// Runs [`supervise`] and restarts it if it panics: health becomes
/// `Failing { SUPERVISION_PANIC_REASON }` (so the UI never shows a stale
/// `Recording`), restarts are spaced by [`panic_restart_delay`]. Segments
/// already indexed stay (the index is shared); the new supervision starts a
/// new session prefix.
async fn supervise_guarded(inner: Arc<Inner>, spec: CaptureSpec, stop: CancellationToken) {
    let mut panics: VecDeque<Instant> = VecDeque::new();
    loop {
        let run = tokio::spawn(supervise(inner.clone(), spec.clone(), stop.clone()));
        match run.await {
            Ok(()) => return,
            Err(e) => {
                if stop.is_cancelled() {
                    return;
                }
                let delay = panic_restart_delay(&mut panics, Instant::now());
                tracing::error!("capture supervision crashed ({e}); restarting in {} s", delay.as_secs());
                inner.set_health(Health::Failing { reason: SUPERVISION_PANIC_REASON.into() });
                tokio::select! {
                    _ = stop.cancelled() => return,
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        }
    }
}

async fn supervise(inner: Arc<Inner>, mut spec: CaptureSpec, stop: CancellationToken) {
    static STARTS: AtomicU64 = AtomicU64::new(0);
    let session = session_token(STARTS.fetch_add(1, Ordering::Relaxed) + 1);
    let mut run: u64 = 0;
    let mut last_segment: Option<Instant> = None;
    let mut poll = interval(LIST_POLL);
    poll.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut tick = interval(WATCHDOG_TICK);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut watchdog = WatchdogState::new(Instant::now());
    let layout = layout_key(&spec);
    let mut ended_logs: VecDeque<PathBuf> = VecDeque::new();
    let indexed = |p: &Path| locks::lock(&inner.segments).contains(p);

    loop {
        run += 1;
        spec.segment_prefix = run_prefix(&session, run);
        let mut tailer = ListTailer::new(spec.segment_dir.join(format!("{}list.csv", spec.segment_prefix)));
        let run_start = SystemTime::now();
        // Every input of this run is timed against run_start (see args.rs).
        spec.wallclock_origin_us = unix_micros(run_start);
        let log_path = spec.segment_dir.join(format!("{}ffmpeg.log", spec.segment_prefix));
        let process_started = Instant::now();
        let mut spawn_error: Option<String> = None;
        let mut exit: Option<String> = None;
        let mut kicks = KickBudget::default();
        // Our own grabber feeds FFmpeg; its pipe must listen before FFmpeg
        // opens it. It never needs a desktop kick.
        let mut grabber: Option<HdrGrabber> = None;
        if let Some(cfg) = hdr_grab_config(&spec) {
            let events = inner.events.clone();
            let notice: crate::capture::hdr_grab::Notice = Arc::new(move |m| {
                let _ = events.send(SupervisorEvent::Notice(m));
            });
            let failed = match tokio::task::spawn_blocking(move || HdrGrabber::start(cfg, notice)).await {
                Ok(Ok(g)) => {
                    grabber = Some(g);
                    None
                }
                Ok(Err(e)) => Some(format!("{e:#}")),
                Err(e) => Some(e.to_string()),
            };
            if let Some(e) = failed {
                if spec.ddagrab_fallback {
                    // SDR desktop: ddagrab gives the same picture. Stay on it
                    // for this capture rather than failing again every run.
                    tracing::warn!("native capture could not start ({e}); recording with ddagrab");
                    spec.hdr_sdr_white = None;
                } else {
                    tracing::error!("HDR capture could not start: {e}");
                    spawn_error = Some(format!("HDR capture: {e}"));
                }
            }
        }
        let uses_ddagrab = spec.video_source == VideoSource::Ddagrab && spec.hdr_sdr_white.is_none();
        let mut child = if spawn_error.is_some() {
            None
        } else {
            match spawn_ffmpeg(&inner.ffmpeg, &spec) {
                Ok(c) => Some(c),
                Err(e) => {
                    tracing::error!("failed to spawn {:?}: {e}", inner.ffmpeg);
                    spawn_error = Some(format!("{} ({e})", inner.ffmpeg.display()));
                    None
                }
            }
        };

        let next_health = loop {
            tokio::select! {
                _ = stop.cancelled() => {
                    kill(&mut child).await;
                    drop_grabber(grabber.take()).await;
                    inner.ingest(&mut tailer, run_start, &spec.segment_dir, layout);
                    clean_ended_run(&spec.segment_dir, &spec.segment_prefix, &indexed);
                    return;
                }
                _ = inner.kill_for_test.notified() => {
                    if let Some(c) = child.as_mut() {
                        let _ = c.start_kill();
                    }
                }
                _ = poll.tick() => {
                    if inner.ingest(&mut tailer, run_start, &spec.segment_dir, layout) {
                        last_segment = Some(Instant::now());
                    }
                }
                _ = tick.tick() => {
                    #[cfg(test)]
                    if inner.panic_for_test.swap(false, Ordering::SeqCst) {
                        panic!("injected supervision panic (test)");
                    }
                    let alive = match child.as_mut().map(|c| c.try_wait()) {
                        Some(Ok(None)) => true,
                        Some(Ok(Some(status))) => {
                            exit.get_or_insert_with(|| status.to_string());
                            false
                        }
                        Some(Err(e)) => {
                            exit.get_or_insert_with(|| format!("status unknown ({e})"));
                            false
                        }
                        None => false,
                    };
                    let now = Instant::now();
                    if uses_ddagrab && kick_due(now, process_started, last_segment, alive) {
                        let kick = locks::read(&inner.kick).clone();
                        if let Some(kick) = kick {
                            match kicks.on_due(watchdog.is_failing()) {
                                KickDecision::Kick { first } => {
                                    if first {
                                        tracing::info!("run {run}: no segment yet; kicking output {}", spec.output_idx);
                                    }
                                    kick(spec.output_idx);
                                }
                                KickDecision::CapReached => tracing::warn!(
                                    "run {run}: still no segment after {MAX_KICKS_PER_PROCESS} kicks; not kicking this FFmpeg process again"
                                ),
                                KickDecision::Skip => {}
                            }
                        }
                    }
                    match watchdog.on_tick(now, last_segment, alive) {
                        WatchdogAction::None => {}
                        WatchdogAction::Restart(reason) => break Health::Restarting { reason },
                        WatchdogAction::Fail(reason) => break Health::Failing { reason },
                    }
                }
            }
        };
        kill(&mut child).await;
        drop_grabber(grabber.take()).await;
        // Root cause for the UI and the log: spawn error, exit status, FFmpeg's stderr.
        let detail = failure_detail(spawn_error.as_deref(), exit.as_deref(), &read_log_tail(&log_path), &log_path);
        let next_health = match next_health {
            Health::Restarting { reason } => Health::Restarting { reason: format!("{reason}; {detail}") },
            Health::Failing { reason } => Health::Failing { reason: format!("{reason}; {detail}") },
            h => h,
        };
        tracing::warn!("run {run} ended ({} kicks): {next_health:?}", kicks.kicks());
        // Segments closed just before the process died are still indexed.
        inner.ingest(&mut tailer, run_start, &spec.segment_dir, layout);
        clean_ended_run(&spec.segment_dir, &spec.segment_prefix, &indexed);
        rotate_run_logs(&mut ended_logs, log_path);
        inner.set_health(next_health);
    }
}

/// Grabber settings for an HDR run, `None` when FFmpeg captures by itself.
fn hdr_grab_config(spec: &CaptureSpec) -> Option<HdrGrabConfig> {
    let white = spec.hdr_sdr_white.filter(|_| spec.video_source == VideoSource::Ddagrab)?;
    Some(HdrGrabConfig {
        output_idx: spec.output_idx,
        fps: spec.fps,
        out_w: spec.out_w,
        out_h: spec.out_h,
        sdr_white: white,
        pipe_name: hdr_pipe_name(spec),
        window_exes: spec.game_window.clone(),
    })
}

/// Stops a grabber (joins its threads) off the async workers.
async fn drop_grabber(g: Option<HdrGrabber>) {
    if let Some(g) = g {
        let dropped = g.dropped_frames();
        if dropped > 0 {
            tracing::warn!("native capture: {dropped} frames dropped because FFmpeg read too slowly");
        }
        let _ = tokio::task::spawn_blocking(move || drop(g)).await;
    }
}

/// Owns the FFmpeg capture process. Cheap to share: all methods take `&self`.
pub struct Supervisor {
    inner: Arc<Inner>,
    /// Segment folder of the last start: a start in the same folder keeps the
    /// buffer, a new folder is wiped first (and the old one cleaned).
    buffer_dir: Mutex<Option<PathBuf>>,
    task: tokio::sync::Mutex<Option<(CancellationToken, JoinHandle<()>)>>,
}

impl Supervisor {
    pub fn new(ffmpeg: PathBuf, retention: Duration, events: broadcast::Sender<SupervisorEvent>) -> Self {
        Self {
            inner: Arc::new(Inner {
                kick: RwLock::new(None),
                ffmpeg,
                retention: Mutex::new(retention),
                segments: Arc::new(Mutex::new(SegmentIndex::new(retention))),
                pending_delete: Mutex::new(Vec::new()),
                health: RwLock::new(Health::Idle),
                events,
                kill_for_test: Notify::new(),
                #[cfg(test)]
                panic_for_test: std::sync::atomic::AtomicBool::new(false),
            }),
            buffer_dir: Mutex::new(None),
            task: tokio::sync::Mutex::new(None),
        }
    }

    /// (Re)starts capture: stops any running capture and launches the
    /// supervision task. The buffer survives restarts in the same folder (a
    /// game that just ended, a monitor move or a settings change must not
    /// delete footage; [`SegmentIndex::select`] keeps layouts apart). The first
    /// start in a folder (app start, buffer folder changed) wipes our leftover
    /// files there, cleans the previous folder and clears the index.
    /// `spec.segment_prefix` is ignored (replaced by `s{session}_r{run}_`).
    pub async fn start(&self, spec: CaptureSpec) {
        let mut task = self.task.lock().await;
        Self::shutdown(&mut task).await;
        let previous = locks::lock(&self.buffer_dir).replace(spec.segment_dir.clone());
        if previous.as_deref() != Some(spec.segment_dir.as_path()) {
            for dir in previous.iter().chain(std::iter::once(&spec.segment_dir)) {
                if let Err(e) = wipe_buffer_dir(dir) {
                    tracing::error!("buffer dir {dir:?}: {e}");
                }
            }
            *locks::lock(&self.inner.segments) = SegmentIndex::new(self.retention());
            locks::lock(&self.inner.pending_delete).clear();
        }
        self.inner.set_health(Health::Restarting { reason: "starting".into() });
        let stop = CancellationToken::new();
        let handle = tokio::spawn(supervise_guarded(self.inner.clone(), spec, stop.clone()));
        *task = Some((stop, handle));
    }

    /// Kills FFmpeg and stops supervision. Indexed segments stay on disk and in
    /// the index (a save can still use them).
    pub async fn stop(&self) {
        let mut task = self.task.lock().await;
        Self::shutdown(&mut task).await;
        self.inner.set_health(Health::Idle);
    }

    async fn shutdown(task: &mut Option<(CancellationToken, JoinHandle<()>)>) {
        if let Some((stop, handle)) = task.take() {
            stop.cancel();
            let _ = handle.await;
        }
    }

    /// How long segments are kept.
    pub fn retention(&self) -> Duration {
        *locks::lock(&self.inner.retention)
    }

    /// Changes how long segments are kept, live: the running index prunes
    /// with it from the next segment on, and later starts use it too.
    pub fn set_retention(&self, retention: Duration) {
        *locks::lock(&self.inner.retention) = retention;
        locks::lock(&self.inner.segments).set_retention(retention);
    }

    /// Installs the desktop kick (see [`KICK_AFTER`]); used for ddagrab only.
    pub fn set_kick(&self, kick: Kick) {
        *locks::write(&self.inner.kick) = Some(kick);
    }

    pub fn segments(&self) -> Arc<Mutex<SegmentIndex>> {
        self.inner.segments.clone()
    }

    pub fn health(&self) -> Health {
        locks::read(&self.inner.health).clone()
    }

    /// Test hook: forces the health value (no event if unchanged).
    #[cfg(test)]
    pub(crate) fn set_health_for_test(&self, h: Health) {
        self.inner.set_health(h);
    }

    /// Test hook: kills the current FFmpeg child, as if it crashed.
    #[doc(hidden)]
    pub async fn kill_child_for_test(&self) {
        self.inner.kill_for_test.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn s(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn restart_when_no_segment_5s() {
        let t0 = Instant::now();
        let mut w = WatchdogState::new(t0);
        let seg = Some(t0 + s(2));
        assert_eq!(w.on_tick(t0 + s(6), seg, true), WatchdogAction::None);
        assert!(matches!(w.on_tick(t0 + s(7), seg, true), WatchdogAction::Restart(_)));
    }

    #[test]
    fn fresh_start_gets_grace_period() {
        let t0 = Instant::now();
        let mut w = WatchdogState::new(t0);
        // An old segment from a previous run must not trigger an instant restart.
        let old = Some(t0 - s(30));
        assert_eq!(w.on_tick(t0 + s(4), old, true), WatchdogAction::None);
        assert_eq!(w.on_tick(t0 + s(4), None, true), WatchdogAction::None);
        assert!(matches!(w.on_tick(t0 + s(5), None, true), WatchdogAction::Restart(_)));
        // The restart resets the grace period.
        assert_eq!(w.on_tick(t0 + s(9), None, true), WatchdogAction::None);
    }

    #[test]
    fn restart_when_process_dead() {
        let t0 = Instant::now();
        let mut w = WatchdogState::new(t0);
        match w.on_tick(t0 + s(1), None, false) {
            WatchdogAction::Restart(r) => assert!(r.contains("exited"), "{r}"),
            a => panic!("{a:?}"),
        }
    }

    #[test]
    fn fail_after_4th_restart_in_60s() {
        let t0 = Instant::now();
        let mut w = WatchdogState::new(t0);
        for i in 1..=3 {
            assert!(matches!(w.on_tick(t0 + s(i * 10), None, false), WatchdogAction::Restart(_)), "restart {i}");
        }
        assert!(matches!(w.on_tick(t0 + s(40), None, false), WatchdogAction::Fail(_)));
        assert!(w.is_failing());
    }

    #[test]
    fn restart_counter_window_slides() {
        let t0 = Instant::now();
        let mut w = WatchdogState::new(t0);
        for i in 1..=3 {
            assert!(matches!(w.on_tick(t0 + s(i * 10), None, false), WatchdogAction::Restart(_)));
        }
        // Restart at t=10 has left the 60 s window by t=71: only 3 in window.
        assert!(matches!(w.on_tick(t0 + s(71), None, false), WatchdogAction::Restart(_)));
        // t=20,30,71 in window at t=75 -> 4th restart in window fails.
        assert!(matches!(w.on_tick(t0 + s(75), None, false), WatchdogAction::Fail(_)));
    }

    #[test]
    fn failing_retries_with_backoff_and_recovers() {
        let t0 = Instant::now();
        let mut w = WatchdogState::new(t0);
        for i in 1..=3 {
            w.on_tick(t0 + s(i), None, false);
        }
        assert!(matches!(w.on_tick(t0 + s(4), None, false), WatchdogAction::Fail(_)));
        // Backoff: nothing for 10 s after the last attempt, then another attempt.
        for i in 5..14 {
            assert_eq!(w.on_tick(t0 + s(i), None, false), WatchdogAction::None, "t={i}");
        }
        assert!(matches!(w.on_tick(t0 + s(14), None, false), WatchdogAction::Fail(_)));
        // A segment after that attempt ends failing mode.
        assert_eq!(w.on_tick(t0 + s(17), Some(t0 + s(16)), true), WatchdogAction::None);
        assert!(!w.is_failing());
    }

    #[test]
    fn kick_after_2s_without_segment_while_alive() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        assert!(!kick_due(t0 + ms(1900), t0, None, true));
        assert!(kick_due(t0 + ms(2000), t0, None, true));
        assert!(kick_due(t0 + ms(4000), t0, None, true), "every tick until a segment arrives");
        assert!(!kick_due(t0 + ms(4000), t0, None, false), "dead process: restart, not kick");
        assert!(!kick_due(t0 + ms(4000), t0, Some(t0 + ms(3000)), true));
        assert!(!kick_due(t0 + ms(5100), t0, Some(t0 + ms(3000)), true), "2.1 s is normal cadence");
        assert!(kick_due(t0 + ms(6500), t0, Some(t0 + ms(3000)), true));
        // A segment from before this process started does not count.
        assert!(kick_due(t0 + ms(2500), t0, Some(t0 - s(10)), true));
    }

    #[test]
    fn no_kick_at_normal_segment_cadence() {
        // Segments every 2 s, seen by the 250 ms list poll after 2.0 / 2.25 s;
        // the 1 s watchdog tick may land right at a 2.0 or 2.25 s gap.
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut last = t0 + ms(3000); // first segment
        for gap in [2000u64, 2250, 2000, 2250, 2000, 2000, 2250] {
            for probe in [0u64, 250, 500, 1000, 1500, 2000, gap] {
                assert!(!kick_due(last + ms(probe), t0, Some(last), true), "kick at +{probe} ms (gap {gap})");
            }
            last += ms(gap);
        }
        // A real stall still kicks before the 5 s watchdog restart.
        assert!(kick_due(last + ms(3500), t0, Some(last), true));
        assert!(!kick_due(last + ms(3500), t0, Some(last), false));
    }

    #[test]
    fn kicks_capped_per_process_and_none_while_failing() {
        let mut b = KickBudget::default();
        let seq: Vec<KickDecision> = (0..6).map(|_| b.on_due(false)).collect();
        assert_eq!(
            seq,
            [
                KickDecision::Kick { first: true },
                KickDecision::Kick { first: false },
                KickDecision::Kick { first: false },
                KickDecision::CapReached,
                KickDecision::Skip,
                KickDecision::Skip,
            ]
        );
        assert_eq!(b.kicks(), 3);
        let mut b = KickBudget::default();
        assert_eq!(b.on_due(true), KickDecision::Skip, "no kick while failing");
        assert_eq!(b.on_due(false), KickDecision::Kick { first: true });
        assert_eq!(b.on_due(true), KickDecision::Skip);
        assert_eq!(b.kicks(), 1);
    }

    #[test]
    fn panic_restarts_are_bounded_by_the_restart_window() {
        let t0 = Instant::now();
        let mut h = VecDeque::new();
        for i in 0..3 {
            assert_eq!(panic_restart_delay(&mut h, t0 + s(i)), PANIC_RESTART_DELAY, "restart {i}");
        }
        assert_eq!(panic_restart_delay(&mut h, t0 + s(3)), FAIL_BACKOFF, "4th within 60 s backs off");
        // The window slides: 60 s later only this attempt is in the window.
        assert_eq!(panic_restart_delay(&mut h, t0 + s(70)), PANIC_RESTART_DELAY);
    }

    #[test]
    fn window_spec_maps_to_window_grabber() {
        let mut s = test_spec(Path::new("x"));
        s.video_source = VideoSource::Ddagrab;
        s.hdr_sdr_white = Some(1.0);
        s.game_window = Some(vec!["rocketleague.exe".into()]);
        assert_eq!(hdr_grab_config(&s).unwrap().window_exes, Some(vec!["rocketleague.exe".to_string()]));
        s.game_window = None;
        assert_eq!(hdr_grab_config(&s).unwrap().window_exes, None);
    }

    fn test_spec(dir: &Path) -> CaptureSpec {
        use crate::capture::args::AudioInput;
        CaptureSpec {
            output_idx: 0,
            fps: 30,
            out_w: 320,
            out_h: 180,
            hdr_sdr_white: None,
            ddagrab_fallback: false,
            encoder: crate::settings::Encoder::H264Nvenc,
            bitrate_kbps: 1000,
            audio_inputs: vec![AudioInput::Silence { title: "Mic" }],
            segment_dir: dir.to_path_buf(),
            segment_prefix: String::new(),
            video_source: VideoSource::TestSrc,
            wallclock_origin_us: 0,
            game_window: None,
        }
    }

    async fn next(rx: &mut broadcast::Receiver<SupervisorEvent>) -> Health {
        loop {
            if let SupervisorEvent::HealthChanged(h) = rx.recv().await.unwrap() {
                return h;
            }
        }
    }

    /// I4: a panic in the supervision task must not leave health stuck; it
    /// reports Failing and supervision restarts (here: the watchdog acts
    /// again on the missing FFmpeg).
    #[tokio::test(flavor = "multi_thread")]
    async fn supervision_panic_sets_failing_and_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = broadcast::channel(256);
        let sup = Supervisor::new(dir.path().join("no-ffmpeg.exe"), s(30), tx);
        sup.start(test_spec(dir.path())).await;
        sup.inner.panic_for_test.store(true, Ordering::SeqCst);
        let failing = tokio::time::timeout(s(5), async {
            loop {
                if let Health::Failing { reason } = next(&mut rx).await {
                    if reason == SUPERVISION_PANIC_REASON {
                        return;
                    }
                }
            }
        })
        .await;
        assert!(failing.is_ok(), "no Failing({SUPERVISION_PANIC_REASON}) after the panic");
        assert_eq!(sup.health(), Health::Failing { reason: SUPERVISION_PANIC_REASON.into() });
        // Restarted supervision: its watchdog reports the missing FFmpeg again.
        let after = tokio::time::timeout(s(5), next(&mut rx)).await.expect("supervision did not restart");
        assert!(matches!(&after, Health::Restarting { reason } if reason.contains("could not start FFmpeg")), "{after:?}");
        tokio::time::timeout(s(5), sup.stop()).await.expect("stop hangs after a panic restart");
        assert_eq!(sup.health(), Health::Idle);
    }

    /// A capture restart (new target, monitor move, settings change) keeps
    /// the buffer, so the game just played can still be saved. Only the first
    /// start in a folder wipes leftovers of earlier app runs; switching folders
    /// cleans the old one.
    #[tokio::test(flavor = "multi_thread")]
    async fn restart_keeps_buffer_and_folder_switch_cleans_the_old_one() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let sup = Supervisor::new(a.path().join("no-ffmpeg.exe"), s(600), tx);
        let leftover = a.path().join(format!("{}seg_00000001.ts", run_prefix("ff-1", 1)));
        std::fs::write(&leftover, b"old").unwrap();
        sup.start(test_spec(a.path())).await;
        assert!(!leftover.exists(), "first start wipes leftovers of earlier runs");

        let kept = a.path().join(format!("{}seg_00000001.ts", run_prefix("ff-2", 1)));
        std::fs::write(&kept, b"x").unwrap();
        let now = SystemTime::now();
        sup.segments().lock().unwrap().push(Segment { index: 1, path: kept.clone(), start: now - s(2), end: now, layout: 3 });
        sup.start(test_spec(a.path())).await;
        assert!(kept.exists(), "restart in the same folder keeps the buffer");
        assert_eq!(sup.segments().lock().unwrap().latest_end(), Some(now));

        sup.start(test_spec(b.path())).await;
        assert!(!kept.exists(), "switching folders cleans the old one");
        assert_eq!(sup.segments().lock().unwrap().latest_end(), None);
        sup.stop().await;
    }

    /// Segments are tagged with the layout of the capture that wrote them.
    #[test]
    fn ingest_tags_segments_with_the_layout() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let sup = Supervisor::new(dir.path().join("no-ffmpeg.exe"), s(600), tx);
        let list = dir.path().join("sab-1_r1_list.csv");
        std::fs::write(&list, "sab-1_r1_seg_00000000.ts,0.0,2.0\n").unwrap();
        let mut tailer = ListTailer::new(list);
        assert!(sup.inner.ingest(&mut tailer, SystemTime::now(), dir.path(), 42));
        let sel = sup.segments().lock().unwrap().select(SystemTime::UNIX_EPOCH, SystemTime::now() + s(10));
        assert_eq!(sel.layout, 42);
        assert_eq!(sel.segments.len(), 1);
    }

    /// An expired segment that cannot be deleted yet (antivirus, indexer, a
    /// save still copying it) is deleted on a later ingest instead of leaking.
    #[cfg(windows)]
    #[test]
    fn locked_expired_segment_is_deleted_later() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let sup = Supervisor::new(dir.path().join("no-ffmpeg.exe"), s(1), tx);
        let old = dir.path().join("sab-1_r1_seg_00000099.ts");
        std::fs::write(&old, b"old").unwrap();
        let now = SystemTime::now();
        sup.segments().lock().unwrap().push(Segment { index: 99, path: old.clone(), start: now - s(100), end: now - s(98), layout: 0 });
        let lock = std::fs::OpenOptions::new().read(true).share_mode(0).open(&old).unwrap();
        let list = dir.path().join("sab-1_r1_list.csv");
        std::fs::write(&list, "sab-1_r1_seg_00000000.ts,0.0,2.0\n").unwrap();
        let mut tailer = ListTailer::new(list.clone());
        assert!(sup.inner.ingest(&mut tailer, now, dir.path(), 0));
        assert!(old.exists(), "locked file cannot be deleted yet");
        drop(lock);
        let mut f = std::fs::OpenOptions::new().append(true).open(&list).unwrap();
        f.write_all(b"sab-1_r1_seg_00000001.ts,2.0,4.0\n").unwrap();
        drop(f);
        assert!(sup.inner.ingest(&mut tailer, now, dir.path(), 0));
        assert!(!old.exists(), "retried once the lock is gone");
    }

    /// A run that ends leaves only its indexed segments behind: its list CSV
    /// and the partial segment of the killed process go, other runs' files and
    /// foreign files stay.
    #[test]
    fn ended_run_leaves_only_indexed_segments() {
        let dir = tempfile::tempdir().unwrap();
        let p = run_prefix(&session_token(1), 7);
        let other = run_prefix(&session_token(1), 8);
        let kept = dir.path().join(format!("{p}seg_00000001.ts"));
        let partial = dir.path().join(format!("{p}seg_00000002.ts"));
        let list = dir.path().join(format!("{p}list.csv"));
        let log = dir.path().join(format!("{p}ffmpeg.log"));
        let other_seg = dir.path().join(format!("{other}seg_00000001.ts"));
        let foreign = dir.path().join("video.ts");
        for f in [&kept, &partial, &list, &log, &other_seg, &foreign] {
            std::fs::write(f, b"x").unwrap();
        }
        clean_ended_run(dir.path(), &p, &|q| q == kept);
        assert!(kept.exists() && log.exists() && other_seg.exists() && foreign.exists());
        assert!(!partial.exists(), "partial segment of the killed run");
        assert!(!list.exists(), "list CSV of the ended run");
    }

    #[test]
    fn only_the_newest_run_logs_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let mut logs = VecDeque::new();
        let paths: Vec<PathBuf> = (1..=KEEP_RUN_LOGS as u64 + 5).map(|r| dir.path().join(format!("{}ffmpeg.log", run_prefix("ab-1", r)))).collect();
        for p in &paths {
            std::fs::write(p, b"log").unwrap();
            rotate_run_logs(&mut logs, p.clone());
        }
        assert_eq!(logs.len(), KEEP_RUN_LOGS);
        assert!(paths[..5].iter().all(|p| !p.exists()), "oldest deleted");
        assert!(paths[5..].iter().all(|p| p.exists()), "newest kept");
    }

    #[test]
    fn run_prefix_is_unique_per_session() {
        let a = run_prefix("1a2b-1", 1);
        assert_eq!(a, "s1a2b-1_r1_");
        assert_ne!(a, run_prefix("1a2b-2", 1));
        let t = session_token(7);
        assert!(t.starts_with(&format!("{:x}", std::process::id())), "{t}");
        assert!(t.ends_with("-7"), "{t}");
        assert_ne!(session_token(7), session_token(8));
    }

    /// C2: the startup wipe removes only ClipVault's own run files, never a
    /// user's `video.ts` or `shopping list.csv` in the same folder.
    #[test]
    fn wipe_removes_only_own_run_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = run_prefix(&session_token(3), 12);
        let own = [format!("{p}seg_00000001.ts"), format!("{p}seg_00012345.ts"), format!("{p}list.csv"), format!("{p}ffmpeg.log")];
        let foreign = [
            "video.ts",
            "notes.ts",
            "shopping list.csv",
            "playlist.csv",
            "my ffmpeg.log",
            "r3_seg_00000001.ts",
            "sxyz-1_r1_seg_00000001.ts",
            "s1a-1_r1_seg_0000x001.ts",
            "s1a-1_r1_other.ts",
            "s1a-1_rx_list.csv",
        ];
        for n in own.iter().map(String::as_str).chain(foreign) {
            std::fs::write(dir.path().join(n), b"x").unwrap();
        }
        std::fs::create_dir(dir.path().join(format!("{p}seg_00000002.ts"))).unwrap();
        wipe_buffer_dir(dir.path()).unwrap();
        for n in &own {
            assert!(!dir.path().join(n).exists(), "own file {n} must be removed");
        }
        for n in foreign {
            assert!(dir.path().join(n).exists(), "foreign file {n} must survive");
        }
        assert!(dir.path().join(format!("{p}seg_00000002.ts")).is_dir(), "directories are never touched");
    }

    #[test]
    fn root_cause_prefers_first_error_line() {
        let log = "[Parsed_ddagrab_0 @ 0000017af2de7940] Failed to enumerate DXGI output 1\n\
                   [Parsed_ddagrab_0 @ 0000017af2de7940] Failed to configure output pad on Parsed_ddagrab_0\n\
                   Error opening input files: Generic error in an external library\n";
        assert_eq!(root_cause_line(log).as_deref(), Some("Failed to enumerate DXGI output 1"));
        assert_eq!(root_cause_line("frame=1\nsomething odd\n\n").as_deref(), Some("something odd"));
        assert_eq!(root_cause_line("  \n"), None);
    }

    #[test]
    fn failure_detail_carries_root_cause() {
        let log = Path::new(r"C:\buf\r3_ffmpeg.log");
        let stderr = "[x @ 01] Failed to enumerate DXGI output 1\nError opening input files\n";
        assert_eq!(
            failure_detail(None, Some("exit code: 0xfffffff3"), stderr, log),
            r"exit code: 0xfffffff3; FFmpeg: Failed to enumerate DXGI output 1 | Error opening input files; log: C:\buf\r3_ffmpeg.log"
        );
        assert_eq!(failure_detail(Some("program not found"), None, "", log), "could not start FFmpeg: program not found");
        assert_eq!(failure_detail(None, None, "", log), r"log: C:\buf\r3_ffmpeg.log");
    }

    #[test]
    fn tailer_handles_missing_file_and_partial_lines() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("r1_list.csv");
        let mut t = ListTailer::new(p.clone());
        assert!(t.poll().is_empty());
        let mut f = std::fs::File::create(&p).unwrap();
        write!(f, "r1_seg_00000000.ts,0.0,2.0\nr1_seg_0000").unwrap();
        f.flush().unwrap();
        assert_eq!(t.poll(), vec!["r1_seg_00000000.ts,0.0,2.0".to_string()]);
        assert!(t.poll().is_empty());
        write!(f, "0001.ts,2.0,4.0\r\n").unwrap();
        f.flush().unwrap();
        assert_eq!(t.poll(), vec!["r1_seg_00000001.ts,2.0,4.0".to_string()]);
    }
}
