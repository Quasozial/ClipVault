//! Audio sources: WASAPI capture into a [`PipeWriter`] as 48 kHz stereo f32.
//!
//! Each [`AudioSource`] owns one thread (COM MTA) that:
//! * every [`RECHECK`] (2 s) resolves its [`SourceKind`] to a concrete target
//!   (device ID, Discord root PID, or silence) and (re)opens the capture when the
//!   target changed or the previous attempt failed. Default devices are resolved
//!   each time, so a change of the Windows default device is followed;
//! * captures with `AUTOCONVERTPCM | SRC_DEFAULT_QUALITY` requesting 48 kHz
//!   stereo f32, falling back to the device mix format plus [`Converter`] if a
//!   device refuses;
//! * keeps the pipe fed in real time: whenever the capture delivers nothing for
//!   [`IDLE`] (source absent, failed, or WASAPI loopback idle because nothing
//!   plays) it writes 10 ms chunks of zeros paced by wall clock (see [`Pacer`]);
//! * reports the RMS level (linear, 0..1, over both channels) of every 100 ms
//!   written to the pipe, silence included, through `on_level`.
//!
//! Failures (device lost, client creation) are logged once per distinct message,
//! kept in [`AudioSource::last_error`], and retried every 2 s while silence flows.

use super::convert::{decode, Converter, SampleFormat};
use super::pipes::{PipeWriter, CHANNELS, SAMPLE_RATE};
use crate::settings::Settings;
use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};
use wasapi::{AudioCaptureClient, AudioClient, Device, DeviceEnumerator, Direction, Handle, SampleType, StreamMode, WaveFormat};
use windows::Wdk::System::SystemServices::RtlGetVersion;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::SystemInformation::OSVERSIONINFOW;

pub const DISCORD_EXE: &str = "Discord.exe";
/// Interval for re-resolving targets and retrying failed captures.
pub const RECHECK: Duration = Duration::from_secs(2);
/// Silence is written in chunks of this many frames (10 ms).
const CHUNK_FRAMES: u64 = SAMPLE_RATE as u64 / 100;
/// The capture counts as idle after delivering nothing for this long.
const IDLE: Duration = Duration::from_millis(50);
/// Level reports cover this many frames (100 ms).
const LEVEL_WINDOW_FRAMES: usize = SAMPLE_RATE as usize / 10;
/// WASAPI shared-mode buffer: 100 ms, in 100 ns units.
const BUFFER_HNS: i64 = 1_000_000;
/// First Windows build with process loopback capture (Windows 10 2004).
const PROCESS_LOOPBACK_MIN_BUILD: u32 = 19041;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceKind {
    Mic { device: Option<String> },
    /// The process tree rooted at the first `process` whose parent is not `process`.
    ProcessInclude { process: String },
    /// Everything except that tree; plain loopback of the default output while absent.
    ProcessExclude { process: String },
    SystemLoopback { device: Option<String> },
    /// Real-time silence. Used for the Mic track when the mic is disabled, so the
    /// track layout (and FFmpeg's inputs) stay the same either way.
    Silence,
}

/// Sources and track titles in FFmpeg input order. Devices are WASAPI endpoint
/// IDs (`None` = Windows default).
pub fn plan_sources(settings: &Settings, process_loopback_supported: bool) -> Vec<(SourceKind, &'static str)> {
    let mic = if settings.mic_enabled {
        SourceKind::Mic { device: settings.mic_device.clone() }
    } else {
        SourceKind::Silence
    };
    if process_loopback_supported && settings.discord_split {
        vec![
            (mic, "Mic"),
            (SourceKind::ProcessInclude { process: DISCORD_EXE.into() }, "Discord"),
            (SourceKind::ProcessExclude { process: DISCORD_EXE.into() }, "Game/System"),
        ]
    } else {
        vec![(mic, "Mic"), (SourceKind::SystemLoopback { device: settings.output_device.clone() }, "Game/System")]
    }
}

/// Windows build number via `RtlGetVersion` (not subject to manifest lies); 0 on failure.
pub fn windows_build() -> u32 {
    let mut v = OSVERSIONINFOW { dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32, ..Default::default() };
    let status = unsafe { RtlGetVersion(&mut v) };
    if status.0 >= 0 { v.dwBuildNumber } else { 0 }
}

pub fn process_loopback_supported() -> bool {
    windows_build() >= PROCESS_LOOPBACK_MIN_BUILD
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AudioDevice {
    /// WASAPI endpoint ID, e.g. `{0.0.1.00000000}.{guid}`; stored in settings.
    pub id: String,
    pub name: String,
    pub is_input: bool,
    pub is_default: bool,
}

/// Active capture and render endpoints, for the settings UI. Errors are logged
/// and yield an empty (or partial) list.
pub fn list_audio_devices() -> Vec<AudioDevice> {
    // Own thread: COM is initialised as MTA there, whatever the caller's apartment.
    let res = std::thread::spawn(|| {
        let _ = wasapi::initialize_mta();
        let mut out = Vec::new();
        let res = (|| -> Result<()> {
            let en = DeviceEnumerator::new()?;
            for dir in [Direction::Capture, Direction::Render] {
                let default_id = en.get_default_device(dir).and_then(|d| d.get_id()).ok();
                for dev in &en.get_device_collection(dir)? {
                    let dev = dev?;
                    let id = dev.get_id()?;
                    out.push(AudioDevice {
                        name: dev.get_friendlyname().unwrap_or_else(|_| id.clone()),
                        is_input: dir == Direction::Capture,
                        is_default: default_id.as_deref() == Some(id.as_str()),
                        id,
                    });
                }
            }
            Ok(())
        })();
        (out, res)
    })
    .join();
    match res {
        Ok((out, Ok(()))) => out,
        Ok((out, Err(e))) => {
            tracing::warn!("[audio] listing devices: {e:#}");
            out
        }
        Err(_) => {
            tracing::warn!("[audio] listing devices panicked");
            Vec::new()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcInfo {
    pub pid: u32,
    pub parent: u32,
    pub name: String,
}

/// The root of `exe`'s process tree: a process named `exe` whose parent is not
/// also `exe` (lowest PID if there are several). Case-insensitive.
pub(crate) fn find_root_pid(procs: &[ProcInfo], exe: &str) -> Option<u32> {
    let is_exe = |pid: u32| procs.iter().any(|p| p.pid == pid && p.name.eq_ignore_ascii_case(exe));
    procs
        .iter()
        .filter(|p| p.name.eq_ignore_ascii_case(exe) && !is_exe(p.parent))
        .map(|p| p.pid)
        .min()
}

fn snapshot_processes() -> Result<Vec<ProcInfo>> {
    let mut out = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).context("CreateToolhelp32Snapshot")?;
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut ok = Process32FirstW(snap, &mut e).is_ok();
        while ok {
            let len = e.szExeFile.iter().position(|c| *c == 0).unwrap_or(e.szExeFile.len());
            out.push(ProcInfo {
                pid: e.th32ProcessID,
                parent: e.th32ParentProcessID,
                name: String::from_utf16_lossy(&e.szExeFile[..len]),
            });
            ok = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    Ok(out)
}

fn frames_in(d: Duration) -> u64 {
    (d.as_nanos() * SAMPLE_RATE as u128 / 1_000_000_000) as u64
}

/// Wall-clock pacing of the output. Tracks frames written against the time
/// since `anchor`; when the source runs ahead the anchor follows it, so silence
/// after a source stops resumes from where the source left off.
pub(crate) struct Pacer {
    anchor: Instant,
    frames: u64,
    last_real: Option<Instant>,
}

impl Pacer {
    pub(crate) fn new(now: Instant) -> Pacer {
        Pacer { anchor: now, frames: 0, last_real: None }
    }

    fn due(&self, now: Instant) -> u64 {
        frames_in(now.saturating_duration_since(self.anchor))
    }

    /// Record `frames` of captured audio written at `now`.
    pub(crate) fn on_real(&mut self, now: Instant, frames: u64) {
        self.frames += frames;
        self.last_real = Some(now);
        if self.frames > self.due(now) {
            let span = Duration::from_nanos(self.frames * 1_000_000_000 / SAMPLE_RATE as u64);
            if let Some(a) = now.checked_sub(span) {
                self.anchor = a;
            }
        }
    }

    /// Frames of silence (a multiple of 10 ms) to write now; counted as written.
    pub(crate) fn silence_due(&mut self, now: Instant) -> u64 {
        if self.last_real.is_some_and(|t| now.saturating_duration_since(t) < IDLE) {
            return 0;
        }
        let behind = self.due(now).saturating_sub(self.frames);
        let n = behind - behind % CHUNK_FRAMES;
        self.frames += n;
        n
    }
}

/// RMS over fixed 100 ms windows of interleaved stereo samples.
pub(crate) struct LevelMeter {
    sum_sq: f64,
    samples: usize,
}

impl LevelMeter {
    pub(crate) fn new() -> LevelMeter {
        LevelMeter { sum_sq: 0.0, samples: 0 }
    }

    /// Feed samples whose last sample was written at `end`; `emit` gets the
    /// linear RMS (0..1) of each completed window and the wall time of the
    /// window's first sample (derived from `end` and the sample count).
    pub(crate) fn push(&mut self, samples: &[f32], end: SystemTime, emit: &mut dyn FnMut(f32, SystemTime)) {
        const WINDOW: usize = LEVEL_WINDOW_FRAMES * CHANNELS;
        let total_frames = samples.len() / CHANNELS;
        for (i, s) in samples.iter().enumerate() {
            self.sum_sq += (*s as f64) * (*s as f64);
            self.samples += 1;
            if self.samples == WINDOW {
                // Frames after this window's last one, within this chunk.
                let after = total_frames.saturating_sub(i / CHANNELS + 1) as u64;
                let span = Duration::from_nanos((after + LEVEL_WINDOW_FRAMES as u64) * 1_000_000_000 / SAMPLE_RATE as u64);
                let start = end.checked_sub(span).unwrap_or(end);
                emit((self.sum_sq / WINDOW as f64).sqrt() as f32, start);
                self.sum_sq = 0.0;
                self.samples = 0;
            }
        }
    }
}

/// What a [`SourceKind`] currently resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    Silence,
    Device { id: String, loopback: bool },
    Process { pid: u32, include: bool },
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Target::Silence => write!(f, "silence"),
            Target::Device { id, loopback: false } => write!(f, "capture device {id}"),
            Target::Device { id, loopback: true } => write!(f, "loopback of {id}"),
            Target::Process { pid, include: true } => write!(f, "process tree {pid}"),
            Target::Process { pid, include: false } => write!(f, "everything except process tree {pid}"),
        }
    }
}

fn device_or_default(device: &Option<String>, dir: Direction) -> Result<String> {
    match device {
        Some(id) => Ok(id.clone()),
        None => Ok(DeviceEnumerator::new()?
            .get_default_device(dir)
            .with_context(|| format!("no default {dir} device"))?
            .get_id()?),
    }
}

fn resolve_target(kind: &SourceKind) -> Result<Target> {
    Ok(match kind {
        SourceKind::Silence => Target::Silence,
        SourceKind::Mic { device } => Target::Device { id: device_or_default(device, Direction::Capture)?, loopback: false },
        SourceKind::SystemLoopback { device } => {
            Target::Device { id: device_or_default(device, Direction::Render)?, loopback: true }
        }
        SourceKind::ProcessInclude { process } => match find_root_pid(&snapshot_processes()?, process) {
            Some(pid) => Target::Process { pid, include: true },
            None => Target::Silence,
        },
        SourceKind::ProcessExclude { process } => match find_root_pid(&snapshot_processes()?, process) {
            Some(pid) => Target::Process { pid, include: false },
            None => Target::Device { id: device_or_default(&None, Direction::Render)?, loopback: true },
        },
    })
}

fn pipe_format() -> WaveFormat {
    WaveFormat::new(32, 32, SampleType::Float, SAMPLE_RATE as usize, CHANNELS, None)
}

fn shared_mode(autoconvert: bool) -> StreamMode {
    StreamMode::EventsShared { autoconvert, buffer_duration_hns: BUFFER_HNS }
}

/// A running WASAPI capture stream.
struct Capture {
    client: AudioClient,
    capture: AudioCaptureClient,
    event: Handle,
    format: SampleFormat,
    /// `None` when WASAPI already delivers the pipe format.
    converter: Option<Converter>,
    bytes: VecDeque<u8>,
    decoded: Vec<f32>,
}

impl Capture {
    fn open(target: &Target) -> Result<Capture> {
        match target {
            Target::Silence => Err(anyhow!("silence has no capture")),
            Target::Device { id, .. } => {
                let dev = DeviceEnumerator::new()?.get_device(id).with_context(|| format!("device {id}"))?;
                Self::open_device(&dev)
            }
            Target::Process { pid, include } => {
                // Process-loopback clients accept any requested format.
                let mut client = AudioClient::new_application_loopback_client(*pid, *include)
                    .with_context(|| format!("process loopback client for pid {pid}"))?;
                client.initialize_client(&pipe_format(), Direction::Capture, shared_mode(true), None)?;
                Self::start(client, SampleFormat::F32, None)
            }
        }
    }

    /// Capture from a device (render devices are captured as loopback).
    fn open_device(dev: &Device) -> Result<Capture> {
        let mut client = dev.get_iaudioclient()?;
        let refused = match client.initialize_client(&pipe_format(), Direction::Capture, shared_mode(true), None) {
            Ok(()) => return Self::start(client, SampleFormat::F32, None),
            Err(e) => e,
        };
        // Fallback: the device's own mix format, converted in Rust.
        let mut client = dev.get_iaudioclient()?;
        let mix = client.get_mixformat()?;
        let format = match (mix.get_subformat()?, mix.get_bitspersample()) {
            (SampleType::Float, 32) => SampleFormat::F32,
            (SampleType::Int, 16) => SampleFormat::I16,
            (SampleType::Int, 24) => SampleFormat::I24,
            (SampleType::Int, 32) => SampleFormat::I32,
            (t, b) => return Err(anyhow!("48 kHz f32 refused ({refused}); unsupported mix format {t} {b}-bit")),
        };
        client
            .initialize_client(&mix, Direction::Capture, shared_mode(false), None)
            .with_context(|| format!("48 kHz f32 refused ({refused}); mix format also failed"))?;
        let conv = Converter::new(mix.get_samplespersec(), mix.get_nchannels() as usize);
        tracing::info!(
            "[audio] converting in software from {} Hz, {} ch ({refused})",
            mix.get_samplespersec(),
            mix.get_nchannels()
        );
        Self::start(client, format, Some(conv))
    }

    fn start(client: AudioClient, format: SampleFormat, converter: Option<Converter>) -> Result<Capture> {
        let event = client.set_get_eventhandle()?;
        let capture = client.get_audiocaptureclient()?;
        client.start_stream()?;
        Ok(Capture { client, capture, event, format, converter, bytes: VecDeque::new(), decoded: Vec::new() })
    }

    /// Wait up to 10 ms for data, then append everything available (pipe format) to `out`.
    fn read(&mut self, out: &mut Vec<f32>) -> Result<()> {
        // A timeout is fine: packets are polled either way (loopback streams may not signal).
        let _ = self.event.wait_for_event(10);
        while self.capture.get_next_packet_size()?.unwrap_or(0) > 0 {
            self.bytes.clear();
            self.capture.read_from_device_to_deque(&mut self.bytes)?;
            let (a, b) = self.bytes.as_slices();
            match self.converter.as_mut() {
                None => {
                    decode(a, self.format, out);
                    decode(b, self.format, out);
                }
                Some(conv) => {
                    self.decoded.clear();
                    decode(a, self.format, &mut self.decoded);
                    decode(b, self.format, &mut self.decoded);
                    conv.process(&self.decoded, out);
                }
            }
        }
        Ok(())
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.client.stop_stream();
    }
}

struct SourceShared {
    stop: AtomicBool,
    last_error: Mutex<Option<String>>,
    status: Mutex<String>,
}

impl SourceShared {
    fn set_error(&self, who: &str, msg: String) {
        let mut last = self.last_error.lock().unwrap();
        if last.as_deref() != Some(msg.as_str()) {
            tracing::warn!("[audio {who}] {msg}; emitting silence, retrying every {}s", RECHECK.as_secs());
        }
        *last = Some(msg);
    }

    fn clear_error(&self) {
        *self.last_error.lock().unwrap() = None;
    }
}

/// Level callback: linear RMS (0..1) of a 100 ms window and the wall time of
/// its first sample. Runs on the capture thread: must not block.
pub type LevelCallback = Box<dyn Fn(f32, SystemTime) + Send>;

/// Everything written to the pipe goes through here (pacing + level meter).
struct Output {
    writer: PipeWriter,
    pacer: Pacer,
    meter: LevelMeter,
    on_level: Option<LevelCallback>,
    zeros: Vec<f32>,
}

impl Output {
    fn emit(&mut self, samples: &[f32]) {
        self.writer.write(samples);
        if let Some(cb) = &self.on_level {
            self.meter.push(samples, SystemTime::now(), &mut |l, at| cb(l, at));
        }
    }

    fn real(&mut self, now: Instant, samples: &[f32]) {
        self.pacer.on_real(now, (samples.len() / CHANNELS) as u64);
        self.emit(samples);
    }

    fn fill_silence(&mut self, now: Instant) {
        let n = self.pacer.silence_due(now) as usize;
        if n > 0 {
            let mut zeros = std::mem::take(&mut self.zeros);
            zeros.resize(n * CHANNELS, 0.0);
            self.emit(&zeros);
            self.zeros = zeros;
        }
    }
}

/// One audio track's capture thread; see the module docs.
pub struct AudioSource {
    kind: SourceKind,
    writer: PipeWriter,
    shared: Arc<SourceShared>,
    thread: Option<JoinHandle<()>>,
}

impl AudioSource {
    /// Start capturing `kind` into `writer`. `on_level` receives the linear RMS
    /// and the window's start (wall clock)
    /// (0..1) of each 100 ms written, on the capture thread; keep it cheap.
    pub fn start(kind: SourceKind, writer: PipeWriter, on_level: Option<LevelCallback>) -> Result<AudioSource> {
        let shared = Arc::new(SourceShared {
            stop: AtomicBool::new(false),
            last_error: Mutex::new(None),
            status: Mutex::new("starting".into()),
        });
        let out = Output { writer: writer.clone(), pacer: Pacer::new(Instant::now()), meter: LevelMeter::new(), on_level, zeros: Vec::new() };
        let (k, sh) = (kind.clone(), shared.clone());
        let thread = std::thread::Builder::new()
            .name(format!("audio-{}", writer.pipe_name()))
            .spawn(move || {
                let com = wasapi::initialize_mta();
                run(&k, out, &sh);
                if com.is_ok() {
                    wasapi::deinitialize();
                }
            })
            .context("spawning audio thread")?;
        Ok(AudioSource { kind, writer, shared, thread: Some(thread) })
    }

    /// Stop capturing and join the thread. The writer (pipe) lives on in any clones.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }

    /// Last capture failure (cleared when a capture opens or the target is silence).
    pub fn last_error(&self) -> Option<String> {
        self.shared.last_error.lock().unwrap().clone()
    }

    /// Human-readable current target, e.g. `process tree 1234` or `silence`.
    pub fn status(&self) -> String {
        self.shared.status.lock().unwrap().clone()
    }

    pub fn kind(&self) -> &SourceKind {
        &self.kind
    }

    pub fn writer(&self) -> &PipeWriter {
        &self.writer
    }
}

impl Drop for AudioSource {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run(kind: &SourceKind, mut out: Output, sh: &SourceShared) {
    let who = out.writer.pipe_name().to_string();
    let mut cap: Option<Capture> = None;
    let mut current: Option<Target> = None;
    let mut next_check = Instant::now();
    let mut buf = Vec::new();
    while !sh.stop.load(Ordering::SeqCst) {
        let now = Instant::now();
        if now >= next_check {
            next_check = now + RECHECK;
            match resolve_target(kind) {
                Ok(t) => {
                    let changed = current.as_ref() != Some(&t);
                    if changed || (cap.is_none() && t != Target::Silence) {
                        cap = None;
                        if t == Target::Silence {
                            sh.clear_error();
                        } else {
                            match Capture::open(&t) {
                                Ok(c) => {
                                    cap = Some(c);
                                    sh.clear_error();
                                    tracing::info!("[audio {who}] capturing {t}");
                                }
                                Err(e) => sh.set_error(&who, format!("{t}: {e:#}")),
                            }
                        }
                        let live = if cap.is_some() || t == Target::Silence { "" } else { " (failed; silence)" };
                        *sh.status.lock().unwrap() = format!("{t}{live}");
                        current = Some(t);
                    }
                }
                Err(e) => {
                    cap = None;
                    current = None;
                    sh.set_error(&who, format!("resolving {kind:?}: {e:#}"));
                    *sh.status.lock().unwrap() = "unresolved (silence)".into();
                }
            }
        }
        match cap.as_mut() {
            Some(c) => {
                buf.clear();
                match c.read(&mut buf) {
                    Ok(()) if !buf.is_empty() => out.real(Instant::now(), &buf),
                    Ok(()) => {}
                    Err(e) => {
                        let t = current.as_ref().map(|t| t.to_string()).unwrap_or_default();
                        sh.set_error(&who, format!("{t}: capture failed: {e:#}"));
                        *sh.status.lock().unwrap() = format!("{t} (lost; silence)");
                        cap = None;
                        next_check = Instant::now() + RECHECK;
                    }
                }
            }
            None => std::thread::sleep(Duration::from_millis(5)),
        }
        out.fill_silence(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::time::Duration;

    #[test]
    fn plan_three_tracks_when_supported() {
        let s = Settings::default();
        assert_eq!(
            plan_sources(&s, true),
            vec![
                (SourceKind::Mic { device: None }, "Mic"),
                (SourceKind::ProcessInclude { process: DISCORD_EXE.into() }, "Discord"),
                (SourceKind::ProcessExclude { process: DISCORD_EXE.into() }, "Game/System"),
            ]
        );
    }

    #[test]
    fn plan_two_tracks_fallback() {
        let mut s = Settings::default();
        s.output_device = Some("{0.0.0.00000000}.{out}".into());
        let want = vec![
            (SourceKind::Mic { device: None }, "Mic"),
            (SourceKind::SystemLoopback { device: Some("{0.0.0.00000000}.{out}".into()) }, "Game/System"),
        ];
        assert_eq!(plan_sources(&s, false), want);
        s.discord_split = false;
        assert_eq!(plan_sources(&s, true), want);
    }

    #[test]
    fn plan_uses_mic_device_and_silence_when_disabled() {
        let mut s = Settings::default();
        s.mic_device = Some("{0.0.1.00000000}.{mic}".into());
        assert_eq!(plan_sources(&s, true)[0], (SourceKind::Mic { device: Some("{0.0.1.00000000}.{mic}".into()) }, "Mic"));
        s.mic_enabled = false;
        for supported in [true, false] {
            let p = plan_sources(&s, supported);
            assert_eq!(p[0], (SourceKind::Silence, "Mic"), "track layout stays stable");
        }
        assert_eq!(plan_sources(&s, true).len(), 3);
    }

    #[test]
    fn process_loopback_supported_on_this_machine() {
        // Development machine is Windows 11 (build >= 22000).
        assert!(process_loopback_supported());
    }

    fn p(pid: u32, parent: u32, name: &str) -> ProcInfo {
        ProcInfo { pid, parent, name: name.into() }
    }

    #[test]
    fn root_pid_is_the_process_whose_parent_is_not_the_same_exe() {
        let procs = vec![
            p(4, 0, "System"),
            p(500, 1, "explorer.exe"),
            p(812, 900, "Discord.exe"),
            p(900, 500, "Discord.exe"),
            p(913, 900, "Discord.exe"),
            p(1000, 812, "Discord.exe"),
        ];
        assert_eq!(find_root_pid(&procs, "Discord.exe"), Some(900));
        assert_eq!(find_root_pid(&procs, "discord.EXE"), Some(900), "case-insensitive");
        assert_eq!(find_root_pid(&procs, "nonexistent.exe"), None);
    }

    #[test]
    fn root_pid_prefers_lowest_when_several_roots() {
        let procs = vec![p(70, 1, "Discord.exe"), p(30, 2, "Discord.exe"), p(31, 30, "Discord.exe")];
        assert_eq!(find_root_pid(&procs, "Discord.exe"), Some(30));
    }

    #[test]
    fn pacer_emits_realtime_silence_without_source() {
        let t0 = Instant::now();
        let mut pc = Pacer::new(t0);
        let mut total = 0;
        // Irregular ticks (sleep jitter) must not accumulate drift.
        let mut t = 0u64;
        for step in [7u64, 15, 16, 3, 31, 15, 16, 9].iter().cycle().take(200) {
            t += step;
            let n = pc.silence_due(t0 + Duration::from_millis(t));
            assert_eq!(n % 480, 0, "10 ms chunks");
            total += n;
        }
        let want = t * 48;
        assert!(total <= want && want - total < 480, "{total} vs {want}");
    }

    #[test]
    fn pacer_no_silence_while_source_delivers() {
        let t0 = Instant::now();
        let mut pc = Pacer::new(t0);
        for i in 1..=100u64 {
            let now = t0 + Duration::from_millis(i * 10);
            pc.on_real(now, 480);
            assert_eq!(pc.silence_due(now + Duration::from_millis(5)), 0);
        }
    }

    #[test]
    fn pacer_fills_gap_after_source_goes_idle() {
        let t0 = Instant::now();
        let mut pc = Pacer::new(t0);
        for i in 1..=100u64 {
            pc.on_real(t0 + Duration::from_millis(i * 10), 480);
        }
        // Source stops at 1 s; at 2 s the silence brings us back to wall clock.
        let n = pc.silence_due(t0 + Duration::from_secs(2));
        assert!((47_520..=48_000).contains(&n), "{n}");
    }

    #[test]
    fn pacer_follows_source_running_ahead() {
        let t0 = Instant::now();
        let mut pc = Pacer::new(t0);
        // Source delivers 1.1 s of audio within 1 s; afterwards it goes idle.
        pc.on_real(t0 + Duration::from_secs(1), 52_800);
        // Idle for 0.5 s: silence resumes from where the source left off, no double count.
        let n = pc.silence_due(t0 + Duration::from_millis(1500));
        assert!((23_520..=24_000).contains(&n), "{n}");
    }

    #[test]
    fn level_meter_rms_every_100ms() {
        let mut m = LevelMeter::new();
        let mut levels = Vec::new();
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let ms = Duration::from_millis;
        let half = vec![0.5f32; 2400 * CHANNELS];
        m.push(&half, t, &mut |l, at| levels.push((l, at)));
        assert!(levels.is_empty());
        m.push(&half, t + ms(50), &mut |l, at| levels.push((l, at)));
        // 150 ms in one chunk ending at t + 200 ms: one window [t+50, t+150] completes.
        m.push(&vec![0.0f32; 7200 * CHANNELS], t + ms(200), &mut |l, at| levels.push((l, at)));
        assert_eq!(levels.len(), 2);
        assert!((levels[0].0 - 0.5).abs() < 1e-6, "{levels:?}");
        assert_eq!(levels[0].1, t - ms(50), "window = the two 50 ms chunks");
        assert_eq!(levels[1], (0.0, t + ms(50)));
    }

    #[test]
    fn silence_when_process_absent() {
        let name = format!("clipvault_test_{}", uuid::Uuid::new_v4().simple());
        let w = PipeWriter::new(&name).unwrap();
        let levels = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let l2 = levels.clone();
        let src = AudioSource::start(
            SourceKind::ProcessInclude { process: "nonexistent.exe".into() },
            w,
            Some(Box::new(move |l, _| l2.lock().unwrap().push(l))),
        )
        .unwrap();
        let mut client = std::fs::File::open(format!(r"\\.\pipe\{name}")).unwrap();
        let mut buf = vec![0u8; 64 * 1024];
        // Skip whatever was buffered at connect time, then measure 2 s.
        let n = client.read(&mut buf).unwrap();
        assert!(n > 0);
        let t0 = Instant::now();
        let mut bytes = 0usize;
        while t0.elapsed() < Duration::from_secs(2) {
            let n = client.read(&mut buf).unwrap();
            assert!(buf[..n].iter().all(|b| *b == 0), "silence must be zeros");
            bytes += n;
        }
        let secs = t0.elapsed().as_secs_f64();
        let rate = (bytes / (4 * CHANNELS)) as f64 / secs;
        assert!((rate - SAMPLE_RATE as f64).abs() < SAMPLE_RATE as f64 * 0.02, "{rate} frames/s");
        assert!(src.last_error().is_none(), "{:?}", src.last_error());
        src.stop();
        let levels = levels.lock().unwrap();
        assert!(levels.len() >= 15 && levels.iter().all(|l| *l == 0.0), "{levels:?}");
    }
}
