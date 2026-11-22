//! Named-pipe server that feeds raw PCM to FFmpeg.
//!
//! FFmpeg opens `\\.\pipe\{name}` as a client when it starts and is killed and
//! respawned by the supervisor, so the server must outlive any one client:
//!
//! * A background thread always keeps one *listening* instance (overlapped
//!   `ConnectNamedPipe`). When a client connects, that instance becomes the
//!   *active* one, any previous active instance is closed (newest client wins)
//!   and a fresh listening instance is created at once, so a respawned FFmpeg
//!   never finds the name missing, even before the old pipe is noticed broken.
//! * Audio written while no client is connected is dropped, not queued, so the
//!   stream is real-time as soon as FFmpeg (re)connects.
//! * [`PipeWriter::write`] never blocks: samples go into a 500 ms queue that
//!   drops the oldest frames on overflow. The thread drains it into the pipe with
//!   overlapped writes; a write that fails (broken pipe / no data) closes the
//!   active instance.
//! * The pipe's DACL grants access only to the owner (the creating user, or the
//!   Administrators group when elevated) and SYSTEM, since it carries mic audio.

use anyhow::{anyhow, Context, Result};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows::core::{HRESULT, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, LocalFree, ERROR_IO_INCOMPLETE, ERROR_IO_PENDING, ERROR_PIPE_CONNECTED, HANDLE, HLOCAL,
    WIN32_ERROR,
};
use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{
    WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_OUTBOUND,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

/// Pipe format: interleaved f32 little-endian, 48 kHz, stereo.
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
/// `write()` queue bound: 500 ms.
pub const QUEUE_FRAMES: usize = SAMPLE_RATE as usize / 2;
const BYTES_PER_FRAME: usize = CHANNELS * 4;
/// Pipe outbound buffer: about 1 s of audio.
const PIPE_BUFFER_BYTES: u32 = SAMPLE_RATE * BYTES_PER_FRAME as u32;
/// How often the server thread re-checks its stop flag while waiting.
pub(crate) const POLL: Duration = Duration::from_millis(10);
/// Owner and SYSTEM only, protected from inheritance.
const PIPE_SDDL: &str = "D:P(A;;GA;;;OW)(A;;GA;;;SY)";

/// Bounded sample queue that drops the oldest whole frames on overflow.
pub(crate) struct SampleQueue {
    buf: VecDeque<f32>,
    cap_samples: usize,
}

impl SampleQueue {
    pub(crate) fn new(cap_frames: usize) -> SampleQueue {
        let cap_samples = cap_frames * CHANNELS;
        SampleQueue { buf: VecDeque::with_capacity(cap_samples), cap_samples }
    }

    /// Append samples (a trailing partial frame is ignored); returns the number
    /// of frames dropped to stay within the bound.
    pub(crate) fn push(&mut self, samples: &[f32]) -> u64 {
        let samples = &samples[..samples.len() - samples.len() % CHANNELS];
        let mut dropped = 0;
        let samples = if samples.len() > self.cap_samples {
            let skip = samples.len() - self.cap_samples;
            dropped += skip;
            &samples[skip..]
        } else {
            samples
        };
        let over = (self.buf.len() + samples.len()).saturating_sub(self.cap_samples);
        self.buf.drain(..over);
        dropped += over;
        self.buf.extend(samples);
        (dropped / CHANNELS) as u64
    }

    pub(crate) fn take_all(&mut self) -> Vec<f32> {
        self.buf.drain(..).collect()
    }

    pub(crate) fn clear(&mut self) {
        self.buf.clear();
    }

    pub(crate) fn len_frames(&self) -> usize {
        self.buf.len() / CHANNELS
    }
}

struct Shared {
    queue: Mutex<SampleQueue>,
    data_ready: Condvar,
    connected: AtomicBool,
    dropped: AtomicU64,
    stop: AtomicBool,
    last_error: Mutex<Option<String>>,
}

impl Shared {
    /// Record and log a server failure (logged once per distinct message).
    fn set_error(&self, name: &str, e: &anyhow::Error) {
        let msg = format!("{e:#}");
        let mut last = self.last_error.lock().unwrap();
        if last.as_deref() != Some(msg.as_str()) {
            tracing::warn!("[audio pipe {name}] {msg}");
        }
        *last = Some(msg);
    }
}

struct Inner {
    name: String,
    shared: Arc<Shared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.shared.data_ready.notify_all();
        if let Some(t) = self.thread.lock().unwrap().take() {
            let _ = t.join();
        }
    }
}

/// Server end of `\\.\pipe\{name}`. Cheap to clone; the server stops (and the
/// name is released) when the last clone is dropped.
#[derive(Clone)]
pub struct PipeWriter {
    inner: Arc<Inner>,
}

impl PipeWriter {
    /// Create the pipe (failing if the name is already in use) and start serving.
    /// The pipe is accepting clients when this returns.
    pub fn new(pipe_name: &str) -> Result<PipeWriter> {
        let wide: Vec<u16> = format!(r"\\.\pipe\{pipe_name}").encode_utf16().chain([0]).collect();
        let mut first = Instance::create(&wide, true, PIPE_BUFFER_BYTES).with_context(|| format!("creating pipe {pipe_name}"))?;
        first.begin_connect().with_context(|| format!("listening on pipe {pipe_name}"))?;
        let shared = Arc::new(Shared {
            queue: Mutex::new(SampleQueue::new(QUEUE_FRAMES)),
            data_ready: Condvar::new(),
            connected: AtomicBool::new(false),
            dropped: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            last_error: Mutex::new(None),
        });
        let (sh, name) = (shared.clone(), pipe_name.to_string());
        let thread = std::thread::Builder::new()
            .name(format!("pipe-{pipe_name}"))
            .spawn(move || serve(&name, wide, first, &sh))?;
        Ok(PipeWriter {
            inner: Arc::new(Inner { name: pipe_name.to_string(), shared, thread: Mutex::new(Some(thread)) }),
        })
    }

    pub fn pipe_name(&self) -> &str {
        &self.inner.name
    }

    /// Queue interleaved stereo f32 samples. Never blocks; while no client is
    /// connected the samples are discarded. Discarded and overflowed frames are
    /// counted in [`dropped_frames`](Self::dropped_frames).
    pub fn write(&self, samples: &[f32]) {
        let sh = &self.inner.shared;
        if !sh.connected.load(Ordering::SeqCst) {
            sh.dropped.fetch_add((samples.len() / CHANNELS) as u64, Ordering::Relaxed);
            return;
        }
        let dropped = sh.queue.lock().unwrap().push(samples);
        if dropped > 0 {
            sh.dropped.fetch_add(dropped, Ordering::Relaxed);
        }
        sh.data_ready.notify_one();
    }

    /// Frames not delivered: written while disconnected, or pushed out of the queue.
    pub fn dropped_frames(&self) -> u64 {
        self.inner.shared.dropped.load(Ordering::Relaxed)
    }

    /// A client (FFmpeg) is connected and no write to it has failed yet.
    pub fn is_connected(&self) -> bool {
        self.inner.shared.connected.load(Ordering::SeqCst)
    }

    /// Last server-side failure (creating/listening on an instance); cleared once
    /// a new instance is listening. Client disconnects are normal, not errors.
    pub fn last_error(&self) -> Option<String> {
        self.inner.shared.last_error.lock().unwrap().clone()
    }
}

fn hr(code: WIN32_ERROR) -> HRESULT {
    HRESULT::from_win32(code.0)
}

/// One pipe instance with its own overlapped state. Also used by the video
/// frame pipe (`capture::hdr_grab`).
pub(crate) struct Instance {
    pipe: HANDLE,
    event: HANDLE,
    /// Boxed: the kernel holds a pointer to it while an operation is pending.
    ov: Box<OVERLAPPED>,
    pending: bool,
}

// SAFETY: plain kernel handles, used by one thread at a time.
unsafe impl Send for Instance {}

impl Instance {
    pub(crate) fn create(wide_name: &[u16], first: bool, buffer_bytes: u32) -> Result<Instance> {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        let sddl: Vec<u16> = PIPE_SDDL.encode_utf16().chain([0]).collect();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(PCWSTR(sddl.as_ptr()), SDDL_REVISION_1, &mut sd, None)
                .context("building pipe security descriptor")?;
        }
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: false.into(),
        };
        let mut open_mode = PIPE_ACCESS_OUTBOUND | FILE_FLAG_OVERLAPPED;
        if first {
            open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
        }
        let pipe = unsafe {
            CreateNamedPipeW(
                PCWSTR(wide_name.as_ptr()),
                open_mode,
                PIPE_TYPE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                buffer_bytes,
                0,
                0,
                Some(&sa),
            )
        };
        let create_err = windows::core::Error::from_thread();
        unsafe {
            let _ = LocalFree(Some(HLOCAL(sd.0)));
        }
        if pipe.is_invalid() {
            return Err(anyhow!(create_err).context("CreateNamedPipeW"));
        }
        let event = match unsafe { CreateEventW(None, true, false, PCWSTR::null()) } {
            Ok(e) => e,
            Err(e) => {
                unsafe {
                    let _ = CloseHandle(pipe);
                }
                return Err(anyhow!(e).context("CreateEventW"));
            }
        };
        Ok(Instance { pipe, event, ov: Box::new(OVERLAPPED::default()), pending: false })
    }

    fn reset_ov(&mut self) {
        *self.ov = OVERLAPPED { hEvent: self.event, ..Default::default() };
    }

    /// Start listening. Returns true if a client was already connected.
    pub(crate) fn begin_connect(&mut self) -> Result<bool> {
        self.reset_ov();
        match unsafe { ConnectNamedPipe(self.pipe, Some(&mut *self.ov)) } {
            Ok(()) => Ok(true),
            Err(e) if e.code() == hr(ERROR_PIPE_CONNECTED) => Ok(true),
            Err(e) if e.code() == hr(ERROR_IO_PENDING) => {
                self.pending = true;
                Ok(false)
            }
            Err(e) => Err(anyhow!(e).context("ConnectNamedPipe")),
        }
    }

    /// Wait up to `timeout` for the pending operation. `Some(bytes)` once it has
    /// completed (or if nothing was pending), `None` while still in progress.
    pub(crate) fn poll(&mut self, timeout: Duration) -> Result<Option<u32>> {
        if !self.pending {
            return Ok(Some(0));
        }
        unsafe { WaitForSingleObject(self.event, timeout.as_millis() as u32) };
        let mut n = 0u32;
        match unsafe { GetOverlappedResult(self.pipe, &*self.ov, &mut n, false) } {
            Ok(()) => {
                self.pending = false;
                Ok(Some(n))
            }
            Err(e) if e.code() == hr(ERROR_IO_INCOMPLETE) => Ok(None),
            Err(e) => {
                self.pending = false;
                Err(anyhow!(e))
            }
        }
    }

    /// Write all of `bytes`, waiting as long as needed unless `stop` is set.
    pub(crate) fn write_all(&mut self, mut bytes: &[u8], stop: &AtomicBool) -> Result<()> {
        while !bytes.is_empty() {
            self.reset_ov();
            self.pending = true;
            match unsafe { WriteFile(self.pipe, Some(bytes), None, Some(&mut *self.ov)) } {
                Ok(()) => {}
                Err(e) if e.code() == hr(ERROR_IO_PENDING) => {}
                Err(e) => {
                    self.pending = false;
                    return Err(anyhow!(e));
                }
            }
            // Completed synchronously or not, the result comes from the OVERLAPPED.
            let n = loop {
                if let Some(n) = self.poll(POLL)? {
                    break n;
                }
                if stop.load(Ordering::SeqCst) {
                    return Err(anyhow!("stopping"));
                }
            };
            bytes = &bytes[n as usize..];
        }
        Ok(())
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        unsafe {
            if self.pending {
                // The OVERLAPPED must stay alive until the cancelled operation completes.
                let _ = CancelIoEx(self.pipe, Some(&*self.ov));
                let mut n = 0u32;
                let _ = GetOverlappedResult(self.pipe, &*self.ov, &mut n, true);
            }
            let _ = CloseHandle(self.pipe);
            let _ = CloseHandle(self.event);
        }
    }
}

/// Server thread: see the module docs.
fn serve(name: &str, wide: Vec<u16>, first: Instance, sh: &Shared) {
    let mut listening = Some(first);
    let mut active: Option<Instance> = None;
    let mut retry_at = Instant::now();
    let mut bytes = Vec::new();
    while !sh.stop.load(Ordering::SeqCst) {
        // 1. Keep one listening instance; promote it when a client connects.
        if listening.is_none() && Instant::now() >= retry_at {
            match Instance::create(&wide, false, PIPE_BUFFER_BYTES).and_then(|mut i| i.begin_connect().map(|_| i)) {
                Ok(i) => {
                    *sh.last_error.lock().unwrap() = None;
                    listening = Some(i);
                }
                Err(e) => {
                    sh.set_error(name, &e);
                    retry_at = Instant::now() + Duration::from_millis(200);
                }
            }
        }
        // Block on the listener only while nothing is connected.
        let wait = if active.is_some() { Duration::ZERO } else { POLL };
        if let Some(l) = listening.as_mut() {
            match l.poll(wait) {
                Ok(Some(_)) => {
                    active = listening.take(); // closes any previous client
                    sh.queue.lock().unwrap().clear();
                    sh.connected.store(true, Ordering::SeqCst);
                }
                Ok(None) => {}
                Err(e) => {
                    sh.set_error(name, &e.context("waiting for client"));
                    listening = None;
                }
            }
        } else if active.is_none() {
            std::thread::sleep(POLL);
        }

        // 2. Stream queued audio to the active client.
        let Some(a) = active.as_mut() else { continue };
        let samples = {
            let mut q = sh.queue.lock().unwrap();
            if q.len_frames() == 0 {
                q = sh.data_ready.wait_timeout(q, POLL).unwrap().0;
            }
            q.take_all()
        };
        if samples.is_empty() {
            continue;
        }
        bytes.clear();
        bytes.extend(samples.iter().flat_map(|s| s.to_le_bytes()));
        if a.write_all(&bytes, &sh.stop).is_err() {
            // Broken pipe / no data: the client went away. Normal during restarts.
            sh.connected.store(false, Ordering::SeqCst);
            active = None;
            sh.queue.lock().unwrap().clear();
        }
    }
    sh.connected.store(false, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::time::{Duration, Instant};

    fn unique_name() -> String {
        format!("clipvault_test_{}", uuid::Uuid::new_v4().simple())
    }

    fn open_client(name: &str) -> std::fs::File {
        let path = format!(r"\\.\pipe\{name}");
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match std::fs::File::open(&path) {
                Ok(f) => return f,
                Err(e) if Instant::now() < deadline => {
                    let _ = e;
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => panic!("open {path}: {e}"),
            }
        }
    }

    fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !f() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn read_f32s(f: &mut std::fs::File, n: usize) -> Vec<f32> {
        let mut bytes = vec![0u8; n * 4];
        f.read_exact(&mut bytes).unwrap();
        bytes.chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect()
    }

    #[test]
    fn queue_drops_oldest_whole_frames() {
        let mut q = SampleQueue::new(4);
        assert_eq!(q.push(&[1.0, 1.0, 2.0, 2.0, 3.0, 3.0]), 0);
        assert_eq!(q.push(&[4.0, 4.0, 5.0, 5.0]), 1);
        assert_eq!(q.len_frames(), 4);
        assert_eq!(q.take_all(), vec![2.0, 2.0, 3.0, 3.0, 4.0, 4.0, 5.0, 5.0]);
        assert_eq!(q.len_frames(), 0);
        // A single push larger than the bound keeps only its newest frames.
        let big: Vec<f32> = (0..20).map(|i| (i / 2) as f32).collect();
        assert_eq!(q.push(&big), 6);
        assert_eq!(q.take_all(), vec![6.0, 6.0, 7.0, 7.0, 8.0, 8.0, 9.0, 9.0]);
    }

    #[test]
    fn writer_never_blocks_without_reader() {
        let w = PipeWriter::new(&unique_name()).unwrap();
        let chunk = vec![0.1f32; 480 * CHANNELS]; // 10 ms
        let t = Instant::now();
        for _ in 0..500 {
            w.write(&chunk); // 5 s of audio
        }
        let took = t.elapsed();
        assert!(took < Duration::from_millis(50), "write took {took:?}");
        assert!(!w.is_connected());
        assert_eq!(w.dropped_frames(), 500 * 480);
    }

    #[test]
    fn listening_when_new_returns() {
        let name = unique_name();
        let w = PipeWriter::new(&name).unwrap();
        // No retry: the instance must exist as soon as `new` returns.
        std::fs::File::open(format!(r"\\.\pipe\{name}")).expect("pipe is listening");
        wait_until("connected", || w.is_connected());
        assert_eq!(w.pipe_name(), name);
    }

    #[test]
    fn duplicate_pipe_name_rejected() {
        let name = unique_name();
        let _w = PipeWriter::new(&name).unwrap();
        assert!(PipeWriter::new(&name).is_err());
    }

    #[test]
    fn streams_samples_to_reader() {
        let name = unique_name();
        let w = PipeWriter::new(&name).unwrap();
        let mut client = open_client(&name);
        wait_until("connected", || w.is_connected());
        let data: Vec<f32> = (0..4800 * CHANNELS).map(|i| i as f32 * 1e-4).collect();
        w.write(&data);
        assert_eq!(read_f32s(&mut client, data.len()), data);
        assert_eq!(w.dropped_frames(), 0);
    }

    #[test]
    fn reconnects_after_reader_goes_away() {
        let name = unique_name();
        let w = PipeWriter::new(&name).unwrap();
        let tick = vec![0.5f32; 480 * CHANNELS];
        for round in 0..3 {
            let mut client = open_client(&name);
            wait_until("connected", || w.is_connected());
            let marker = vec![round as f32; 480 * CHANNELS];
            w.write(&marker);
            assert_eq!(read_f32s(&mut client, marker.len()), marker, "round {round}");
            drop(client);
            // The server notices the broken pipe on its next write.
            wait_until("disconnected", || {
                w.write(&tick);
                !w.is_connected()
            });
        }
        assert!(w.last_error().is_none(), "{:?}", w.last_error());
    }

    #[test]
    fn drop_stops_server() {
        let name = unique_name();
        let w = PipeWriter::new(&name).unwrap();
        let t = Instant::now();
        drop(w);
        assert!(t.elapsed() < Duration::from_millis(500));
        // The name is free again.
        let _w2 = PipeWriter::new(&name).unwrap();
    }
}
