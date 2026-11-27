//! Native screen capture: Desktop Duplication in FP16, converted to SDR NV12
//! at the recording size on the GPU, streamed to FFmpeg as raw video over a
//! named pipe. Used on every monitor: an SDR desktop duplicated as FP16
//! arrives as linear sRGB with white at 1.0 (`sdr_white` 1.0).
//!
//! Also why on SDR desktops: ddagrab ends its stream on
//! DXGI_ERROR_ACCESS_LOST (887a0026: fullscreen games switching mode, a window
//! over exclusive fullscreen), which cost a FFmpeg restart and seconds of
//! video each time; here the duplication is recreated within ~100 ms while the
//! last picture keeps going out. It also never needs a desktop kick.
//!
//! Why: ddagrab's frames cannot be scaled or converted on the GPU by this
//! FFmpeg (scale_d3d11 fails to allocate its pool on NVIDIA; mapping D3D11
//! frames to Vulkan/CUDA/OpenCL is not implemented), so the old HDR graph
//! downloaded every 4K FP16 frame (66 MB), uploaded it again to Vulkan for
//! libplacebo and downloaded the result: ~50 fps on an idle desktop and ~36 fps
//! next to a game (measured 2026-11-22, RTX 5090, 3840x2160@240), with FFmpeg
//! duplicating frames to fill the gaps. Here the frame never leaves the GPU
//! until it is a ~3 MB NV12 picture.
//!
//! Per tick (`fps` per second, aligned to the wall clock, see [`next_tick`]):
//! 1. take the newest desktop frame if there is one (`AcquireNextFrame(0)`),
//!    copy it into our own FP16 texture and release it at once; otherwise keep
//!    the last one (a static desktop still yields frames: no desktop kick),
//! 2. read back the NV12 picture dispatched on the previous tick (it had a
//!    whole tick to finish behind the game's GPU work) and queue it for the pipe,
//! 3. dispatch the compute shader (`hdr_nv12.hlsl`: downscale, divide by SDR
//!    white, clip, sRGB, cursor, BT.709 limited-range NV12) for this tick.
//!
//! Timing: FFmpeg stamps each frame with its wall-clock read time
//! (`-use_wallclock_as_timestamps`, quantised to 1/fps), like the audio pipes.
//! Frames are written just before a 1/fps boundary of the Unix epoch, so the
//! rounding is stable; the one-tick readback delay is subtracted in the filter
//! ([`PIPE_LATENCY_FRAMES`]).
//!
//! The pipe works like the audio pipes (`audio::pipes`): frames written while
//! FFmpeg is not connected are dropped, the queue is bounded (oldest dropped),
//! a respawned FFmpeg reconnects to a fresh listening instance.
use crate::audio::pipes::{Instance, POLL};
use crate::capture::window_grab::Rect;
use anyhow::{anyhow, bail, Context, Result};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

/// Frames between capture and pipe write (the readback is one tick late).
pub const PIPE_LATENCY_FRAMES: u32 = 1;
/// Frames are written this long before their tick's epoch boundary, so the
/// read time FFmpeg stamps rounds to that boundary even with a few ms of jitter.
const WRITE_LEAD: Duration = Duration::from_millis(3);
/// Pipe queue bound (~0.5 s at 60 fps).
const QUEUE_FRAMES: usize = 30;

#[derive(Debug, Clone)]
pub struct HdrGrabConfig {
    /// Output index on DXGI adapter 0 (same numbering as ddagrab).
    pub output_idx: u32,
    pub fps: u32,
    /// Recording size; width a multiple of 4, height a multiple of 2.
    pub out_w: u32,
    pub out_h: u32,
    /// SDR white in scRGB units (e.g. 3.0 for 240 nits).
    pub sdr_white: f32,
    pub pipe_name: String,
    /// `Some`: record the main window of a running process with one of these
    /// exe names (lowercase file names) with Windows.Graphics.Capture;
    /// `output_idx` is then only the fallback when WGC is not supported.
    pub window_exes: Option<Vec<String>>,
}

/// One-time user-facing message from the grabber (shown as a toast).
pub type Notice = Arc<dyn Fn(String) + Send + Sync>;

/// What the shader draws: the `src` rectangle of the captured frame (source
/// pixels) scaled into the `dst` rectangle of the output picture, black
/// around it; `black` draws an all-black picture (Y 16, UV 128).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) struct View {
    pub src: Rect,
    pub dst: Rect,
    pub black: bool,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl View {
    /// The whole `src_w` x `src_h` frame stretched over the whole
    /// `out_w` x `out_h` output (the monitor path).
    pub fn full(src_w: u32, src_h: u32, out_w: u32, out_h: u32) -> View {
        View {
            src: Rect { x: 0, y: 0, w: src_w, h: src_h },
            dst: Rect { x: 0, y: 0, w: out_w, h: out_h },
            black: false,
        }
    }
}

/// NV12 frame size in bytes.
pub fn nv12_size(w: u32, h: u32) -> usize {
    (w * h * 3 / 2) as usize
}

/// Wall-clock instant (as seconds since the epoch) of the next frame write
/// after `now_s`: `WRITE_LEAD` before a multiple of 1/fps.
fn next_tick(now_s: f64, fps: u32) -> f64 {
    let lead = WRITE_LEAD.as_secs_f64();
    let n = ((now_s + lead) * fps as f64).floor() + 1.0;
    n / fps as f64 - lead
}

fn epoch_secs() -> f64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

// ---------------------------------------------------------------------------
// Frame pipe
// ---------------------------------------------------------------------------

struct PipeShared {
    queue: Mutex<VecDeque<Arc<Vec<u8>>>>,
    ready: Condvar,
    connected: AtomicBool,
    stop: AtomicBool,
    dropped: AtomicU64,
}

/// Server end of `\\.\pipe\{name}` carrying whole NV12 frames.
struct FramePipe {
    shared: Arc<PipeShared>,
    thread: Option<JoinHandle<()>>,
}

impl FramePipe {
    fn new(name: &str, frame_bytes: usize) -> Result<FramePipe> {
        let wide: Vec<u16> = format!(r"\\.\pipe\{name}").encode_utf16().chain([0]).collect();
        let buffer = frame_bytes as u32;
        let mut first = Instance::create(&wide, true, buffer).with_context(|| format!("creating pipe {name}"))?;
        first.begin_connect().with_context(|| format!("listening on pipe {name}"))?;
        let shared = Arc::new(PipeShared {
            queue: Mutex::new(VecDeque::new()),
            ready: Condvar::new(),
            connected: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            dropped: AtomicU64::new(0),
        });
        let sh = shared.clone();
        let label = name.to_string();
        let thread = std::thread::Builder::new()
            .name(format!("pipe-{name}"))
            .spawn(move || serve_frames(&label, wide, buffer, first, &sh))?;
        Ok(FramePipe { shared, thread: Some(thread) })
    }

    /// Queue a frame; dropped while no client is connected. Never blocks.
    fn push(&self, frame: Arc<Vec<u8>>) {
        let sh = &self.shared;
        if !sh.connected.load(Ordering::SeqCst) {
            return;
        }
        let mut q = crate::locks::lock(&sh.queue);
        if q.len() >= QUEUE_FRAMES {
            q.pop_front();
            sh.dropped.fetch_add(1, Ordering::Relaxed);
        }
        q.push_back(frame);
        drop(q);
        sh.ready.notify_one();
    }
}

impl Drop for FramePipe {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.shared.ready.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn serve_frames(name: &str, wide: Vec<u16>, buffer: u32, first: Instance, sh: &PipeShared) {
    let mut listening = Some(first);
    let mut active: Option<Instance> = None;
    let mut retry_at = Instant::now();
    let mut last_error: Option<String> = None;
    while !sh.stop.load(Ordering::SeqCst) {
        if listening.is_none() && Instant::now() >= retry_at {
            match Instance::create(&wide, false, buffer).and_then(|mut i| i.begin_connect().map(|_| i)) {
                Ok(i) => listening = Some(i),
                Err(e) => {
                    let msg = format!("{e:#}");
                    if last_error.as_deref() != Some(&msg) {
                        tracing::warn!("[video pipe {name}] {msg}");
                    }
                    last_error = Some(msg);
                    retry_at = Instant::now() + Duration::from_millis(200);
                }
            }
        }
        let wait = if active.is_some() { Duration::ZERO } else { POLL };
        if let Some(l) = listening.as_mut() {
            match l.poll(wait) {
                Ok(Some(_)) => {
                    active = listening.take();
                    crate::locks::lock(&sh.queue).clear();
                    sh.connected.store(true, Ordering::SeqCst);
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!("[video pipe {name}] waiting for client: {e:#}");
                    listening = None;
                }
            }
        } else if active.is_none() {
            std::thread::sleep(POLL);
        }

        let Some(a) = active.as_mut() else { continue };
        let frame = {
            let mut q = crate::locks::lock(&sh.queue);
            if q.is_empty() {
                q = crate::locks::wait_timeout(&sh.ready, q, POLL);
            }
            q.pop_front()
        };
        let Some(frame) = frame else { continue };
        if a.write_all(&frame, &sh.stop).is_err() {
            sh.connected.store(false, Ordering::SeqCst);
            active = None;
            crate::locks::lock(&sh.queue).clear();
        }
    }
    sh.connected.store(false, Ordering::SeqCst);
}

// ---------------------------------------------------------------------------
// Grabber
// ---------------------------------------------------------------------------

/// Running HDR capture feeding one pipe. Stops (threads joined) on drop.
pub struct HdrGrabber {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    pipe: Option<Arc<FramePipe>>,
}

impl HdrGrabber {
    /// Open the output and start capturing. Fails (e.g. `Failed to enumerate
    /// DXGI output 2`) before returning if the GPU side cannot be set up; the
    /// pipe is listening when this returns.
    pub fn start(cfg: HdrGrabConfig, notice: Notice) -> Result<HdrGrabber> {
        if cfg.out_w == 0 || cfg.out_h == 0 || cfg.out_w % 4 != 0 || cfg.out_h % 2 != 0 || cfg.fps == 0 {
            bail!("unsupported HDR capture size {}x{}@{}", cfg.out_w, cfg.out_h, cfg.fps);
        }
        let pipe = Arc::new(FramePipe::new(&cfg.pipe_name, nv12_size(cfg.out_w, cfg.out_h))?);
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
        let (st, p) = (stop.clone(), pipe.clone());
        let thread = std::thread::Builder::new()
            .name("hdr-grab".into())
            .spawn(move || imp::run(cfg, notice, &p, &st, ready_tx))?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(HdrGrabber { stop, thread: Some(thread), pipe: Some(pipe) }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err(anyhow!("native capture thread exited during setup"))
            }
        }
    }

    /// Frames dropped because FFmpeg read too slowly.
    pub fn dropped_frames(&self) -> u64 {
        self.pipe.as_ref().map_or(0, |p| p.shared.dropped.load(Ordering::Relaxed))
    }
}

impl Drop for HdrGrabber {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        self.pipe.take();
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use crate::capture::window_grab::{fit, letterbox, WindowPicture, WindowSource};
    use windows::core::{s, Interface};
    use windows::Graphics::Capture::GraphicsCaptureSession;
    use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
    use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_R32_TYPELESS, DXGI_FORMAT_R8G8B8A8_UNORM,
        DXGI_SAMPLE_DESC,
    };
    use windows::Win32::Graphics::Dxgi::*;
    use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST};
    use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};

    const SHADER: &str = include_str!("hdr_nv12.hlsl");
    /// A duplication that delivered no frame for this long is recreated.
    const DUP_FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(2);

    /// Constant buffer, laid out like `Params` in the shader (16-byte rows).
    #[repr(C)]
    #[derive(Clone, Copy, Default, PartialEq)]
    pub(super) struct Params {
        out_size: [u32; 2],
        /// Source pixels per output pixel inside the destination rectangle.
        scale: [f32; 2],
        src_inv: [f32; 2],
        inv_white: f32,
        cursor_mode: u32,
        cursor_pos: [i32; 2],
        cursor_size: [u32; 2],
        src_origin: [f32; 2],
        dst_origin: [u32; 2],
        dst_size: [u32; 2],
        black: u32,
        _pad: u32,
    }

    struct Cursor {
        srv: ID3D11ShaderResourceView,
        mode: u32,
        w: u32,
        h: u32,
    }

    struct Gpu {
        device: ID3D11Device,
        ctx: ID3D11DeviceContext,
        /// The duplicated monitor; `None` for the window source.
        output: Option<IDXGIOutput5>,
        dup: Option<IDXGIOutputDuplication>,
        /// When `dup` was created and whether it has delivered anything since.
        /// A new duplication hands out the current desktop at once; one that
        /// stays silent is wedged (seen after access lost next to a fullscreen
        /// game) and is recreated after [`DUP_FIRST_FRAME_TIMEOUT`].
        dup_since: Instant,
        dup_delivered: bool,
        /// 1x1 texture bound as the source of black pictures before any frame.
        blank: ID3D11ShaderResourceView,
        cs: ID3D11ComputeShader,
        cbuf: ID3D11Buffer,
        sampler: ID3D11SamplerState,
        out_uav: ID3D11UnorderedAccessView,
        out_buf: ID3D11Buffer,
        staging: [ID3D11Buffer; 2],
        /// Our copy of the latest desktop frame (+ SRV), sized like the desktop.
        frame: Option<(ID3D11Texture2D, ID3D11ShaderResourceView, u32, u32)>,
        cursor: Option<Cursor>,
        cursor_visible: bool,
        cursor_pos: [i32; 2],
        shape_buf: Vec<u8>,
        params: Params,
    }

    pub(super) fn run(
        cfg: HdrGrabConfig,
        notice: Notice,
        pipe: &FramePipe,
        stop: &AtomicBool,
        ready: std::sync::mpsc::Sender<Result<()>>,
    ) {
        // Windows.Graphics.Capture objects live on this thread. The MTA is
        // kept alive for the whole process: grabber threads come and go, and
        // windows-rs caches activation factories (e.g. for `IsSupported`)
        // process-wide; tearing the MTA down with the last grabber thread
        // left those dangling (access violation on the next start).
        static MTA: std::sync::Once = std::sync::Once::new();
        MTA.call_once(|| unsafe {
            if let Err(e) = windows::Win32::System::Com::CoIncrementMTAUsage() {
                tracing::warn!("native capture: CoIncrementMTAUsage: {e}");
            }
        });
        if let Err(e) = unsafe { RoInitialize(RO_INIT_MULTITHREADED) } {
            tracing::warn!("native capture: RoInitialize: {e}");
        }
        let mut cfg = cfg;
        if cfg.window_exes.is_some() && !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
            tracing::warn!("native capture: window capture not supported, recording output {}", cfg.output_idx);
            notice("Window capture not supported – recording the monitor".into());
            cfg.window_exes = None;
        }
        let setup = Gpu::new(&cfg, cfg.window_exes.is_some()).and_then(|gpu| {
            let source = match &cfg.window_exes {
                Some(exes) => Some(WindowSource::new(&gpu.device, exes.clone(), notice.clone())?),
                None => None,
            };
            Ok((gpu, source))
        });
        let (mut gpu, mut source) = match setup {
            Ok(g) => g,
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
        let _ = ready.send(Ok(()));
        unsafe {
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
        }
        let size = nv12_size(cfg.out_w, cfg.out_h);
        let mut pending: Option<usize> = None;
        let mut next_staging = 0usize;
        let mut last_error: Option<String> = None;
        let mut retry_dup_at = Instant::now();
        let (mut frames, mut new_frames, mut black, mut stats_at) = (0u64, 0u64, 0u64, Instant::now());
        // Window source: the picture re-sent while the window shows no new frame.
        let mut last_view: Option<View> = None;
        let black_view = View {
            src: Rect { x: 0, y: 0, w: 1, h: 1 },
            dst: Rect { x: 0, y: 0, w: cfg.out_w, h: cfg.out_h },
            black: true,
        };
        match &cfg.window_exes {
            Some(exes) => tracing::info!(
                "native capture: window of {} -> {}x{}@{} NV12, SDR white {}",
                exes.join("/"),
                cfg.out_w,
                cfg.out_h,
                cfg.fps,
                cfg.sdr_white
            ),
            None => tracing::info!(
                "native capture on output {} -> {}x{}@{} NV12, SDR white {}",
                cfg.output_idx,
                cfg.out_w,
                cfg.out_h,
                cfg.fps,
                cfg.sdr_white
            ),
        }
        while !stop.load(Ordering::SeqCst) {
            // Sleep until just before the next epoch-aligned frame boundary.
            let now = epoch_secs();
            let wait = next_tick(now, cfg.fps) - now;
            if wait > 0.0 {
                std::thread::sleep(Duration::from_secs_f64(wait));
            }

            // 2. Previous tick's picture -> pipe.
            if let Some(i) = pending.take() {
                match gpu.read_back(i, size) {
                    Ok(buf) => {
                        pipe.push(Arc::new(buf));
                        frames += 1;
                    }
                    Err(e) => report(&mut last_error, &e.context("reading back frame")),
                }
            }

            // 1+3 for the window source: newest window frame, letterboxed.
            if let Some(src) = source.as_mut() {
                let view = match src.poll(Instant::now()) {
                    (WindowPicture::New, Some((tex, crop))) => match gpu.copy_frame(&tex) {
                        Ok(()) => {
                            new_frames += 1;
                            let (fw, fh) = gpu.frame.as_ref().map_or((1, 1), |f| (f.2, f.3));
                            let crop = fit(crop, fw, fh);
                            let v = View { src: crop, dst: letterbox(crop.w, crop.h, cfg.out_w, cfg.out_h), black: false };
                            last_view = Some(v);
                            v
                        }
                        Err(e) => {
                            report(&mut last_error, &e.context("copying window frame"));
                            last_view.unwrap_or(black_view)
                        }
                    },
                    (WindowPicture::Same, _) => last_view.unwrap_or(black_view),
                    _ => {
                        last_view = None;
                        black_view
                    }
                };
                if view.black {
                    black += 1;
                }
                gpu.dispatch(&cfg, next_staging, &view);
                pending = Some(next_staging);
                next_staging ^= 1;
                log_stats(pipe, &mut frames, &mut new_frames, &mut black, &mut stats_at);
                continue;
            }

            // 1. Newest desktop frame, if any.
            if gpu.dup.is_none() && Instant::now() >= retry_dup_at {
                match gpu.duplicate() {
                    Ok(()) => last_error = None,
                    Err(e) => {
                        report(&mut last_error, &e);
                        retry_dup_at = Instant::now() + Duration::from_millis(500);
                    }
                }
            }
            match gpu.acquire() {
                Ok(true) => new_frames += 1,
                Ok(false) => {}
                Err(e) => {
                    // Access lost (mode change, secure desktop, fullscreen
                    // switch): keep sending the last picture, re-duplicate.
                    report(&mut last_error, &e);
                    gpu.dup = None;
                    retry_dup_at = Instant::now() + Duration::from_millis(100);
                }
            }

            // 3. Convert for the next tick. Black until the first desktop
            // frame: FFmpeg must get video (and cut segments) even while the
            // duplication delivers nothing, or the watchdog restarts it forever.
            let view = match gpu.frame.as_ref() {
                Some((_, _, fw, fh)) => View::full(*fw, *fh, cfg.out_w, cfg.out_h),
                None => {
                    black += 1;
                    black_view
                }
            };
            gpu.dispatch(&cfg, next_staging, &view);
            pending = Some(next_staging);
            next_staging ^= 1;

            log_stats(pipe, &mut frames, &mut new_frames, &mut black, &mut stats_at);
        }
    }

    fn log_stats(pipe: &FramePipe, frames: &mut u64, new_frames: &mut u64, black: &mut u64, at: &mut Instant) {
        if at.elapsed() >= Duration::from_secs(60) {
            tracing::debug!(
                "native capture: {frames} frames sent, {new_frames} new frames, {black} black, {} dropped at pipe",
                pipe.shared.dropped.load(Ordering::Relaxed)
            );
            (*frames, *new_frames, *black, *at) = (0, 0, 0, Instant::now());
        }
    }

    fn report(last: &mut Option<String>, e: &anyhow::Error) {
        let msg = format!("{e:#}");
        if last.as_deref() != Some(&msg) {
            tracing::warn!("native capture: {msg}");
        }
        *last = Some(msg);
    }

    impl Gpu {
        /// `window`: the window source (no monitor to duplicate).
        fn new(cfg: &HdrGrabConfig, window: bool) -> Result<Gpu> {
            unsafe {
                // Desktop Duplication (DuplicateOutput1) needs per-monitor DPI
                // awareness, and so do the window geometry calls (physical
                // pixels); the app already has it (tao), tests do not.
                let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

                let factory: IDXGIFactory1 = CreateDXGIFactory1().context("CreateDXGIFactory1")?;
                let adapter = factory.EnumAdapters1(0).context("no DXGI adapter 0")?;
                let output = if window {
                    None
                } else {
                    let output = adapter
                        .EnumOutputs(cfg.output_idx)
                        .map_err(|_| anyhow!("Failed to enumerate DXGI output {}", cfg.output_idx))?;
                    Some(output.cast::<IDXGIOutput5>().context("IDXGIOutput5 not supported")?)
                };

                let mut device = None;
                let mut ctx = None;
                // BGRA support: required by Windows.Graphics.Capture.
                D3D11CreateDevice(
                    &adapter,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    Some(&[D3D_FEATURE_LEVEL_11_0]),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut ctx),
                )
                .context("D3D11CreateDevice")?;
                let device: ID3D11Device = device.context("no D3D11 device")?;
                let ctx: ID3D11DeviceContext = ctx.context("no D3D11 context")?;
                // Our few dispatches should not wait behind a whole game frame;
                // raising the priority may need a privilege, so failing is fine.
                if let Ok(dxgi_dev) = device.cast::<IDXGIDevice>() {
                    let _ = dxgi_dev.SetGPUThreadPriority(7);
                }

                let cs = compile_shader(&device)?;

                let mut cbuf = None;
                device.CreateBuffer(
                    &D3D11_BUFFER_DESC {
                        ByteWidth: std::mem::size_of::<Params>() as u32,
                        Usage: D3D11_USAGE_DEFAULT,
                        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                        ..Default::default()
                    },
                    None,
                    Some(&mut cbuf),
                )?;

                let mut sampler = None;
                device.CreateSamplerState(
                    &D3D11_SAMPLER_DESC {
                        Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                        AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                        AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                        AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                        MaxLOD: f32::MAX,
                        ..Default::default()
                    },
                    Some(&mut sampler),
                )?;

                let bytes = nv12_size(cfg.out_w, cfg.out_h) as u32; // multiple of 4: w % 4 == 0, h % 2 == 0
                let mut out_buf = None;
                device.CreateBuffer(
                    &D3D11_BUFFER_DESC {
                        ByteWidth: bytes,
                        Usage: D3D11_USAGE_DEFAULT,
                        BindFlags: D3D11_BIND_UNORDERED_ACCESS.0 as u32,
                        MiscFlags: D3D11_RESOURCE_MISC_BUFFER_ALLOW_RAW_VIEWS.0 as u32,
                        ..Default::default()
                    },
                    None,
                    Some(&mut out_buf),
                )?;
                let out_buf: ID3D11Buffer = out_buf.context("output buffer")?;
                let mut out_uav = None;
                device.CreateUnorderedAccessView(
                    &out_buf,
                    Some(&D3D11_UNORDERED_ACCESS_VIEW_DESC {
                        Format: DXGI_FORMAT_R32_TYPELESS,
                        ViewDimension: D3D11_UAV_DIMENSION_BUFFER,
                        Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
                            Buffer: D3D11_BUFFER_UAV {
                                FirstElement: 0,
                                NumElements: bytes / 4,
                                Flags: D3D11_BUFFER_UAV_FLAG_RAW.0 as u32,
                            },
                        },
                    }),
                    Some(&mut out_uav),
                )?;
                let staging_desc = D3D11_BUFFER_DESC {
                    ByteWidth: bytes,
                    Usage: D3D11_USAGE_STAGING,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    ..Default::default()
                };
                let (mut s0, mut s1) = (None, None);
                device.CreateBuffer(&staging_desc, None, Some(&mut s0))?;
                device.CreateBuffer(&staging_desc, None, Some(&mut s1))?;

                let zero = [0u16; 4];
                let mut blank_tex = None;
                device.CreateTexture2D(
                    &D3D11_TEXTURE2D_DESC {
                        Width: 1,
                        Height: 1,
                        MipLevels: 1,
                        ArraySize: 1,
                        Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
                        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                        Usage: D3D11_USAGE_IMMUTABLE,
                        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                        ..Default::default()
                    },
                    Some(&D3D11_SUBRESOURCE_DATA { pSysMem: zero.as_ptr().cast(), SysMemPitch: 8, SysMemSlicePitch: 0 }),
                    Some(&mut blank_tex),
                )?;
                let blank_tex: ID3D11Texture2D = blank_tex.context("blank texture")?;
                let mut blank = None;
                device.CreateShaderResourceView(&blank_tex, None, Some(&mut blank))?;

                let mut gpu = Gpu {
                    device,
                    ctx,
                    output,
                    dup: None,
                    dup_since: Instant::now(),
                    dup_delivered: false,
                    blank: blank.context("blank view")?,
                    cs,
                    cbuf: cbuf.context("constant buffer")?,
                    sampler: sampler.context("sampler")?,
                    out_uav: out_uav.context("output view")?,
                    out_buf,
                    staging: [s0.context("staging")?, s1.context("staging")?],
                    frame: None,
                    cursor: None,
                    cursor_visible: false,
                    cursor_pos: [0, 0],
                    shape_buf: Vec::new(),
                    params: Params::default(),
                };
                if gpu.output.is_some() {
                    gpu.duplicate()?;
                }
                Ok(gpu)
            }
        }

        fn duplicate(&mut self) -> Result<()> {
            let output = self.output.as_ref().context("no output to duplicate")?;
            let dup = unsafe { output.DuplicateOutput1(&self.device, 0, &[DXGI_FORMAT_R16G16B16A16_FLOAT]) }
                .context("DuplicateOutput1 (FP16)")?;
            self.dup = Some(dup);
            self.dup_since = Instant::now();
            self.dup_delivered = false;
            Ok(())
        }

        /// Copy the newest desktop frame, if there is one. `Ok(true)` when the
        /// picture changed.
        fn acquire(&mut self) -> Result<bool> {
            let Some(dup) = self.dup.clone() else { return Ok(false) };
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut res: Option<IDXGIResource> = None;
            match unsafe { dup.AcquireNextFrame(0, &mut info, &mut res) } {
                Ok(()) => self.dup_delivered = true,
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                    if !self.dup_delivered && self.dup_since.elapsed() >= DUP_FIRST_FRAME_TIMEOUT {
                        bail!("duplication delivered no frame in {} s", DUP_FIRST_FRAME_TIMEOUT.as_secs());
                    }
                    return Ok(false);
                }
                Err(e) => return Err(anyhow!(e).context("AcquireNextFrame")),
            }
            let result = self.take_frame(&dup, &info, res);
            unsafe {
                let _ = dup.ReleaseFrame();
            }
            result
        }

        fn take_frame(
            &mut self,
            dup: &IDXGIOutputDuplication,
            info: &DXGI_OUTDUPL_FRAME_INFO,
            res: Option<IDXGIResource>,
        ) -> Result<bool> {
            if info.LastMouseUpdateTime != 0 {
                self.cursor_visible = info.PointerPosition.Visible.as_bool();
                self.cursor_pos = [info.PointerPosition.Position.x, info.PointerPosition.Position.y];
            }
            if info.PointerShapeBufferSize > 0 {
                if let Err(e) = self.update_cursor(dup, info.PointerShapeBufferSize) {
                    tracing::debug!("native capture: cursor shape: {e:#}");
                }
            }
            if info.LastPresentTime == 0 && self.frame.is_some() {
                return Ok(false); // mouse-only update
            }
            let tex: ID3D11Texture2D = res.context("no frame resource")?.cast()?;
            self.copy_frame(&tex)?;
            Ok(true)
        }

        /// Copies `tex` into our frame texture, (re)created at its size.
        fn copy_frame(&mut self, tex: &ID3D11Texture2D) -> Result<()> {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { tex.GetDesc(&mut desc) };
            let resize = !matches!(&self.frame, Some((_, _, w, h)) if *w == desc.Width && *h == desc.Height);
            if resize {
                let mut copy = None;
                unsafe {
                    self.device.CreateTexture2D(
                        &D3D11_TEXTURE2D_DESC {
                            Width: desc.Width,
                            Height: desc.Height,
                            MipLevels: 1,
                            ArraySize: 1,
                            Format: desc.Format,
                            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                            Usage: D3D11_USAGE_DEFAULT,
                            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                            ..Default::default()
                        },
                        None,
                        Some(&mut copy),
                    )?;
                }
                let copy: ID3D11Texture2D = copy.context("frame copy")?;
                let mut srv = None;
                unsafe { self.device.CreateShaderResourceView(&copy, None, Some(&mut srv))? };
                self.frame = Some((copy, srv.context("frame view")?, desc.Width, desc.Height));
            }
            let (copy, ..) = self.frame.as_ref().unwrap();
            unsafe { self.ctx.CopyResource(copy, tex) };
            Ok(())
        }

        fn update_cursor(&mut self, dup: &IDXGIOutputDuplication, size: u32) -> Result<()> {
            self.shape_buf.resize(size as usize, 0);
            let mut needed = 0u32;
            let mut shape = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
            unsafe {
                dup.GetFramePointerShape(size, self.shape_buf.as_mut_ptr().cast(), &mut needed, &mut shape)?;
            }
            let Some((rgba, w, h, mode)) = cursor_rgba(&shape, &self.shape_buf) else {
                self.cursor = None;
                return Ok(());
            };
            let mut tex = None;
            unsafe {
                self.device.CreateTexture2D(
                    &D3D11_TEXTURE2D_DESC {
                        Width: w,
                        Height: h,
                        MipLevels: 1,
                        ArraySize: 1,
                        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                        Usage: D3D11_USAGE_IMMUTABLE,
                        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                        ..Default::default()
                    },
                    Some(&D3D11_SUBRESOURCE_DATA {
                        pSysMem: rgba.as_ptr().cast(),
                        SysMemPitch: w * 4,
                        SysMemSlicePitch: 0,
                    }),
                    Some(&mut tex),
                )?;
            }
            let tex: ID3D11Texture2D = tex.context("cursor texture")?;
            let mut srv = None;
            unsafe { self.device.CreateShaderResourceView(&tex, None, Some(&mut srv))? };
            self.cursor = Some(Cursor { srv: srv.context("cursor view")?, mode, w, h });
            Ok(())
        }

        /// Converts the current frame into staging buffer `staging`, drawing
        /// `view.src` (frame pixels) into `view.dst` (output pixels).
        fn dispatch(&mut self, cfg: &HdrGrabConfig, staging: usize, view: &View) {
            let (srv, sw, sh) = match self.frame.clone() {
                Some((_, srv, w, h)) => (srv, w, h),
                None if view.black => (self.blank.clone(), 1, 1),
                None => return,
            };
            // The window source has the cursor in its frames (WGC draws it).
            let cursor = self.cursor.as_ref().filter(|_| self.cursor_visible && self.output.is_some());
            let (src, dst) = (view.src, view.dst);
            let p = Params {
                out_size: [cfg.out_w, cfg.out_h],
                scale: [src.w as f32 / dst.w.max(1) as f32, src.h as f32 / dst.h.max(1) as f32],
                src_inv: [1.0 / sw as f32, 1.0 / sh as f32],
                inv_white: 1.0 / cfg.sdr_white.max(1.0),
                cursor_mode: cursor.map_or(0, |c| c.mode),
                cursor_pos: self.cursor_pos,
                cursor_size: cursor.map_or([0, 0], |c| [c.w, c.h]),
                src_origin: [src.x as f32, src.y as f32],
                dst_origin: [dst.x, dst.y],
                dst_size: [dst.w, dst.h],
                black: view.black as u32,
                _pad: 0,
            };
            unsafe {
                if p != self.params {
                    self.ctx.UpdateSubresource(&self.cbuf, 0, None, (&p as *const Params).cast(), 0, 0);
                    self.params = p;
                }
                self.ctx.CSSetShader(&self.cs, None);
                self.ctx.CSSetConstantBuffers(0, Some(&[Some(self.cbuf.clone())]));
                let cursor_srv = cursor.map(|c| c.srv.clone());
                self.ctx.CSSetShaderResources(0, Some(&[Some(srv), cursor_srv]));
                self.ctx.CSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
                self.ctx.CSSetUnorderedAccessViews(0, 1, Some(&Some(self.out_uav.clone())), None);
                self.ctx.Dispatch(cfg.out_w.div_ceil(4 * 8), cfg.out_h.div_ceil(2 * 8), 1);
                // Unbind so the frame copy can be written again next tick.
                self.ctx.CSSetShaderResources(0, Some(&[None, None]));
                self.ctx.CSSetUnorderedAccessViews(0, 1, Some(&None), None);
                self.ctx.CopyResource(&self.staging[staging], &self.out_buf);
                self.ctx.Flush();
            }
        }

        fn read_back(&self, staging: usize, size: usize) -> Result<Vec<u8>> {
            let buf = &self.staging[staging];
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            unsafe { self.ctx.Map(buf, 0, D3D11_MAP_READ, 0, Some(&mut mapped))? };
            let mut out = vec![0u8; size];
            unsafe {
                std::ptr::copy_nonoverlapping(mapped.pData as *const u8, out.as_mut_ptr(), size);
                self.ctx.Unmap(buf, 0);
            }
            Ok(out)
        }
    }

    fn compile_shader(device: &ID3D11Device) -> Result<ID3D11ComputeShader> {
        let mut blob: Option<ID3DBlob> = None;
        let mut errors: Option<ID3DBlob> = None;
        let r = unsafe {
            D3DCompile(
                SHADER.as_ptr().cast(),
                SHADER.len(),
                s!("hdr_nv12.hlsl"),
                None,
                None,
                s!("main"),
                s!("cs_5_0"),
                D3DCOMPILE_OPTIMIZATION_LEVEL3,
                0,
                &mut blob,
                Some(&mut errors),
            )
        };
        if let Err(e) = r {
            let msg = errors
                .map(|b| unsafe {
                    let s = std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize());
                    String::from_utf8_lossy(s).into_owned()
                })
                .unwrap_or_default();
            bail!("compiling HDR shader: {e} {msg}");
        }
        let blob = blob.context("no shader blob")?;
        let code = unsafe { std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize()) };
        let mut cs = None;
        unsafe { device.CreateComputeShader(code, None, Some(&mut cs))? };
        cs.context("no compute shader")
    }

    /// Pointer shape as RGBA8 plus the shader's cursor mode (1 alpha, 2 mask).
    fn cursor_rgba(shape: &DXGI_OUTDUPL_POINTER_SHAPE_INFO, buf: &[u8]) -> Option<(Vec<u8>, u32, u32, u32)> {
        super::cursor_rgba(shape.Type, shape.Width, shape.Height, shape.Pitch, buf)
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;
    pub(super) fn run(_: HdrGrabConfig, _: Notice, _: &FramePipe, _: &AtomicBool, ready: std::sync::mpsc::Sender<Result<()>>) {
        let _ = ready.send(Err(anyhow!("native capture needs Windows")));
    }
}

const SHAPE_MONOCHROME: u32 = 1;
const SHAPE_COLOR: u32 = 2;
const SHAPE_MASKED_COLOR: u32 = 4;

/// Desktop Duplication pointer shape -> RGBA8 for the shader, with its mode:
/// 1 = straight alpha (colour cursors), 2 = mask (alpha 255 opaque, 128
/// invert the screen, 0 transparent) for monochrome and masked-colour cursors.
fn cursor_rgba(kind: u32, w: u32, h: u32, pitch: u32, buf: &[u8]) -> Option<(Vec<u8>, u32, u32, u32)> {
    let (w, pitch) = (w as usize, pitch as usize);
    match kind {
        SHAPE_COLOR | SHAPE_MASKED_COLOR => {
            let h = h as usize;
            if w == 0 || h == 0 || buf.len() < pitch * (h - 1) + w * 4 {
                return None;
            }
            let mut out = Vec::with_capacity(w * h * 4);
            for y in 0..h {
                for x in 0..w {
                    let p = &buf[y * pitch + x * 4..][..4]; // BGRA
                    let (r, g, b, a) = (p[2], p[1], p[0], p[3]);
                    if kind == SHAPE_COLOR {
                        out.extend([r, g, b, a]);
                    } else if a == 0 {
                        out.extend([r, g, b, 255]); // replace
                    } else if r == 0 && g == 0 && b == 0 {
                        out.extend([0, 0, 0, 0]); // XOR with black: unchanged
                    } else {
                        out.extend([0, 0, 0, 128]); // XOR: approximated as invert
                    }
                }
            }
            Some((out, w as u32, h as u32, if kind == SHAPE_COLOR { 1 } else { 2 }))
        }
        SHAPE_MONOCHROME => {
            let h = (h / 2) as usize; // AND mask on top, XOR mask below
            if w == 0 || h == 0 || buf.len() < pitch * h * 2 {
                return None;
            }
            let bit = |row: usize, x: usize| (buf[row * pitch + x / 8] >> (7 - x % 8)) & 1;
            let mut out = Vec::with_capacity(w * h * 4);
            for y in 0..h {
                for x in 0..w {
                    out.extend(match (bit(y, x), bit(y + h, x)) {
                        (0, 0) => [0, 0, 0, 255],
                        (0, _) => [255, 255, 255, 255],
                        (_, 0) => [0, 0, 0, 0],
                        _ => [0, 0, 0, 128],
                    });
                }
            }
            Some((out, w as u32, h as u32, 2))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn params_match_cbuffer_layout() {
        assert_eq!(std::mem::size_of::<imp::Params>(), 80);
    }

    #[test]
    fn full_view_is_identity() {
        let v = View::full(3840, 2160, 1920, 1080);
        assert_eq!(v.src, Rect { x: 0, y: 0, w: 3840, h: 2160 });
        assert_eq!(v.dst, Rect { x: 0, y: 0, w: 1920, h: 1080 });
        assert!(!v.black);
    }

    #[test]
    fn ticks_land_just_before_epoch_frame_boundaries() {
        let lead = WRITE_LEAD.as_secs_f64();
        for fps in [30u32, 60, 120] {
            for now in [1_795_000_000.0f64, 1_795_000_000.004, 1_795_000_000.0165, 1_795_000_000.9999] {
                let t = next_tick(now, fps);
                assert!(t > now, "{fps} {now} -> {t}");
                assert!(t - now <= 1.0 / fps as f64 + 1e-9, "{fps} {now} -> {t}");
                let slots = (t + lead) * fps as f64;
                assert!((slots - slots.round()).abs() < 1e-3, "{fps} {now} -> {t} not on a boundary");
            }
        }
    }

    #[test]
    fn monochrome_cursor_decodes_and_xor_masks() {
        // 8x1 cursor: AND row then XOR row, one byte each (pitch 1).
        // px0: and0 xor0 black, px1: and0 xor1 white, px2: and1 xor0 clear, px3: and1 xor1 invert.
        let buf = [0b0011_0000u8, 0b0101_0000u8];
        let (rgba, w, h, mode) = cursor_rgba(SHAPE_MONOCHROME, 8, 2, 1, &buf).unwrap();
        assert_eq!((w, h, mode), (8, 1, 2));
        assert_eq!(&rgba[0..16], &[0, 0, 0, 255, 255, 255, 255, 255, 0, 0, 0, 0, 0, 0, 0, 128]);
    }

    #[test]
    fn color_cursor_swaps_bgra() {
        let buf = [10u8, 20, 30, 40];
        let (rgba, _, _, mode) = cursor_rgba(SHAPE_COLOR, 1, 1, 4, &buf).unwrap();
        assert_eq!((rgba, mode), (vec![30, 20, 10, 40], 1));
        assert!(cursor_rgba(SHAPE_COLOR, 2, 2, 8, &buf).is_none(), "short buffer rejected");
    }

    /// Grabber + real FFmpeg (NVENC) writing segments like the app, for 8 s.
    /// Needs an HDR desktop on output 0; FFmpeg from `CV_FFMPEG`. Prints the
    /// segment folder for inspection.
    #[test]
    #[ignore]
    fn hdr_grab_ffmpeg_e2e() {
        use crate::capture::args::{build_capture_args, hdr_pipe_name, CaptureSpec, VideoSource};
        let ffmpeg = std::env::var("CV_FFMPEG").expect("CV_FFMPEG");
        let dir = std::env::temp_dir().join(format!("cv_hdr_e2e_{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut spec = CaptureSpec {
            output_idx: 0,
            fps: 60,
            out_w: 1920,
            out_h: 1080,
            hdr_sdr_white: Some(3.0),
            ddagrab_fallback: false,
            encoder: crate::settings::Encoder::H264Nvenc,
            bitrate_kbps: 40000,
            audio_inputs: vec![],
            segment_dir: dir.clone(),
            segment_prefix: "r1_".into(),
            video_source: VideoSource::Ddagrab,
            wallclock_origin_us: 0,
            game_window: None,
        };
        spec.wallclock_origin_us =
            SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_micros() as i64;
        let g = HdrGrabber::start(HdrGrabConfig {
            output_idx: 0,
            fps: 60,
            out_w: 1920,
            out_h: 1080,
            sdr_white: 3.0,
            pipe_name: hdr_pipe_name(&spec),
            window_exes: None,
        }, Arc::new(|_| {}))
        .unwrap();
        let log = std::fs::File::create(dir.join("ffmpeg.log")).unwrap();
        let mut child = std::process::Command::new(ffmpeg)
            .args(build_capture_args(&spec))
            .stderr(log)
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_secs(8));
        let _ = child.kill();
        let _ = child.wait();
        println!("segments in {} (dropped {})", dir.display(), g.dropped_frames());
    }

    /// Real capture of output 0 for 3 s. Needs an HDR desktop on output 0 and
    /// FFmpeg; run with `cargo test hdr_grab_live -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn hdr_grab_live() {
        let name = format!("clipvault_test_{}", uuid::Uuid::new_v4().simple());
        let cfg = HdrGrabConfig { output_idx: 0, fps: 60, out_w: 1920, out_h: 1080, sdr_white: 3.0, pipe_name: name.clone(), window_exes: None };
        let g = HdrGrabber::start(cfg, Arc::new(|_| {})).expect("start");
        let mut f = std::fs::File::open(format!(r"\\.\pipe\{name}")).unwrap();
        let size = nv12_size(1920, 1080);
        let mut buf = vec![0u8; size];
        let t = Instant::now();
        let mut n = 0;
        use std::io::Read;
        while t.elapsed() < Duration::from_secs(3) {
            f.read_exact(&mut buf).unwrap();
            n += 1;
        }
        let fps = n as f64 / t.elapsed().as_secs_f64();
        let y_avg = buf[..1920 * 1080].iter().map(|&b| b as u64).sum::<u64>() / (1920 * 1080);
        if let Some(p) = std::env::var_os("CV_HDR_DUMP") {
            std::fs::write(p, &buf).unwrap();
        }
        println!("{n} frames, {fps:.1} fps, mean luma {y_avg}, dropped {}", g.dropped_frames());
        assert!(fps > 55.0, "{fps}");
        assert!((16..=235).contains(&(y_avg as u32)));
    }
}
