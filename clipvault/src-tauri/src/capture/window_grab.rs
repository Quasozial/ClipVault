//! Game window capture (Windows.Graphics.Capture): pure geometry and picture
//! decision helpers, platform independent so they can be unit tested.
use std::time::{Duration, Instant};

/// Pixel rectangle inside a frame or output picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Rounds to the nearest even number, at least 2 and at most `cap`.
fn even(v: f64, cap: u32) -> u32 {
    let e = ((2.0 * (v / 2.0).round()) as u32).max(2);
    e.min(cap & !1).max(2)
}

/// Fits a `src_w` x `src_h` picture into `out_w` x `out_h`, preserving the
/// aspect ratio and centring it. Width, height and position are even (NV12).
pub fn letterbox(src_w: u32, src_h: u32, out_w: u32, out_h: u32) -> Rect {
    let scale = (out_w as f64 / src_w.max(1) as f64).min(out_h as f64 / src_h.max(1) as f64);
    let w = even(src_w as f64 * scale, out_w);
    let h = even(src_h as f64 * scale, out_h);
    Rect { x: (out_w.saturating_sub(w) / 2) & !1, y: (out_h.saturating_sub(h) / 2) & !1, w, h }
}

/// Client area inside a captured window frame. `bounds` is the DWM extended
/// frame bounds (left, top, right, bottom) and `client` the client rect, both
/// in screen pixels. The result is the client relative to `bounds`, clamped to
/// the frame; an empty or invalid client yields the whole frame.
pub fn client_crop(
    frame_w: u32,
    frame_h: u32,
    bounds: (i32, i32, i32, i32),
    client: (i32, i32, i32, i32),
) -> Rect {
    let full = Rect { x: 0, y: 0, w: frame_w, h: frame_h };
    let left = (client.0 - bounds.0).clamp(0, frame_w as i32);
    let top = (client.1 - bounds.1).clamp(0, frame_h as i32);
    let right = (client.2 - bounds.0).clamp(0, frame_w as i32);
    let bottom = (client.3 - bounds.1).clamp(0, frame_h as i32);
    if right <= left || bottom <= top {
        return full;
    }
    Rect { x: left as u32, y: top as u32, w: (right - left) as u32, h: (bottom - top) as u32 }
}

/// What a tick sends to the encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowPicture {
    /// The newest captured frame.
    New,
    /// The previous picture again (static window).
    Same,
    /// A black picture.
    Black,
}

/// Inputs of [`decide`].
#[derive(Clone, Copy)]
pub struct PictureState {
    pub found: bool,
    pub minimized: bool,
    pub capturing: bool,
    pub new_frame: bool,
    pub have_last: bool,
}

/// Black while the window is missing, minimized or not captured; otherwise
/// the new frame, else the last picture, else black. The caller must clear
/// `have_last` whenever Black is decided, so a restored window stays black
/// until a fresh frame arrives instead of showing a stale picture.
pub fn decide(s: &PictureState) -> WindowPicture {
    if !s.found || s.minimized || !s.capturing {
        WindowPicture::Black
    } else if s.new_frame {
        WindowPicture::New
    } else if s.have_last {
        WindowPicture::Same
    } else {
        WindowPicture::Black
    }
}

/// `r` shrunk to lie inside a `w` x `h` frame (at least 1x1 when the frame
/// is not empty), so the shader never samples outside the copied frame.
pub fn fit(r: Rect, w: u32, h: u32) -> Rect {
    let x = r.x.min(w.saturating_sub(1));
    let y = r.y.min(h.saturating_sub(1));
    Rect { x, y, w: r.w.min(w - x.min(w)).max(1), h: r.h.min(h - y.min(h)).max(1) }
}

/// No frame at the new size this long after a frame-pool recreate: restart
/// the session (a static window sends nothing after `Recreate`).
pub const RESIZE_WAIT: Duration = Duration::from_millis(250);
/// At most one resize restart per window this often, so a persistent size
/// mismatch cannot churn sessions.
pub const RESTART_GAP: Duration = Duration::from_secs(2);
/// A session without a single frame for this long while the window is shown
/// counts as "found but not captured"; also the toast delay.
pub const STALL: Duration = Duration::from_secs(10);

/// Whether a session whose pool was recreated at `recreated` (and is still
/// waiting for a frame at the new size) should be restarted now, given the
/// last resize restart of the same window.
pub fn resize_restart_due(recreated: Option<Instant>, last_restart: Option<Instant>, now: Instant) -> bool {
    recreated.is_some_and(|t| now.saturating_duration_since(t) >= RESIZE_WAIT)
        && last_restart.is_none_or(|t| now.saturating_duration_since(t) >= RESTART_GAP)
}

/// Whether a session that has never delivered a frame is stalled: the window
/// has been shown (not minimized) since `shown_since` for [`STALL`].
pub fn stalled(got_frame: bool, shown_since: Option<Instant>, now: Instant) -> bool {
    !got_frame && shown_since.is_some_and(|t| now.saturating_duration_since(t) >= STALL)
}

#[cfg(windows)]
pub(crate) use source::WindowSource;

/// The Windows.Graphics.Capture frame source. Lives on the grabber thread
/// (RoInitialize'd multithreaded) and is polled once per tick; it never blocks:
/// the frame pool is free-threaded and drained with `TryGetNextFrame`.
#[cfg(windows)]
mod source {
    use super::*;
    use crate::capture::display::game_window;
    use crate::capture::hdr_grab::Notice;
    use anyhow::{Context, Result};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use windows::core::{Interface, HSTRING};
    use windows::Foundation::Metadata::ApiInformation;
    use windows::Foundation::TypedEventHandler;
    use windows::Graphics::Capture::{
        Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureAccess, GraphicsCaptureAccessKind,
        GraphicsCaptureItem, GraphicsCaptureSession,
    };
    use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
    use windows::Graphics::DirectX::DirectXPixelFormat;
    use windows::Graphics::SizeInt32;
    use windows::Security::Authorization::AppCapabilityAccess::AppCapabilityAccessStatus;
    use windows::Win32::Foundation::{HWND, POINT, RECT};
    use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D, D3D11_TEXTURE2D_DESC};
    use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
    use windows::Win32::Graphics::Dxgi::IDXGIDevice;
    use windows::Win32::Graphics::Gdi::ClientToScreen;
    use windows::Win32::System::WinRT::Direct3D11::{
        CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
    };
    use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
    use windows::Win32::UI::WindowsAndMessaging::{GetClientRect, IsIconic, IsWindow};

    /// Window search while there is no session.
    const SEARCH: Duration = Duration::from_millis(500);
    /// Re-check for a bigger/other game window while capturing.
    const RECHECK: Duration = Duration::from_secs(2);
    /// Retry after a failed `CreateForWindow`/`StartCapture`.
    const RETRY: Duration = Duration::from_secs(2);
    /// Found but not captured this long: tell the user once.
    const TOAST_AFTER: Duration = STALL;
    const FORMAT: DirectXPixelFormat = DirectXPixelFormat::R16G16B16A16Float;
    pub const CANT_CAPTURE: &str = "Can't capture the game window – switch Game capture to Whole monitor in Settings";

    struct Session {
        hwnd: HWND,
        item: GraphicsCaptureItem,
        pool: Direct3D11CaptureFramePool,
        session: GraphicsCaptureSession,
        size: SizeInt32,
        /// Set by the item's `Closed` handler (window gone).
        closed: Arc<AtomicBool>,
        closed_token: i64,
        /// The frame last handed out; kept until the next one so the pool
        /// does not reuse its buffer while our copy may still be pending.
        held: Option<Direct3D11CaptureFrame>,
        /// When the pool was last recreated for a new size, until a frame at
        /// that size arrives.
        recreated: Option<Instant>,
        /// Any frame delivered since the session started.
        got_frame: bool,
        /// Since when the window is shown (not minimized); `None` while minimized.
        shown_since: Option<Instant>,
    }

    impl Drop for Session {
        fn drop(&mut self) {
            if let Some(f) = self.held.take() {
                let _ = f.Close();
            }
            let _ = self.item.RemoveClosed(self.closed_token);
            let _ = self.session.Close();
            let _ = self.pool.Close();
        }
    }

    pub(crate) struct WindowSource {
        device: IDirect3DDevice,
        exes: Vec<String>,
        notice: Notice,
        session: Option<Session>,
        /// Last search result while there is no session.
        found: Option<(HWND, bool)>,
        next_search: Instant,
        next_recheck: Instant,
        /// The window was found but capture failed since then.
        failing_since: Option<Instant>,
        toasted: bool,
        last_error: Option<String>,
        /// `IsBorderRequired` exists (Windows 11 / 10 2104+).
        border_api: bool,
        /// Borderless access granted (`None`: not asked yet).
        borderless: Option<bool>,
        have_last: bool,
        /// (found, minimized, capturing) last logged.
        logged: Option<(bool, bool, bool)>,
        /// Last resize restart: window and time (caps restarts).
        resize_restart: Option<(HWND, Instant)>,
        /// Window of the last session started (repeat starts log at debug).
        last_started: Option<HWND>,
    }

    impl WindowSource {
        pub fn new(device: &ID3D11Device, exes: Vec<String>, notice: Notice) -> Result<WindowSource> {
            let dxgi: IDXGIDevice = device.cast().context("IDXGIDevice")?;
            let device: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
                .context("CreateDirect3D11DeviceFromDXGIDevice")?
                .cast()?;
            let border_api = ApiInformation::IsPropertyPresent(
                &HSTRING::from("Windows.Graphics.Capture.GraphicsCaptureSession"),
                &HSTRING::from("IsBorderRequired"),
            )
            .unwrap_or(false);
            let now = Instant::now();
            Ok(WindowSource {
                device,
                exes,
                notice,
                session: None,
                found: None,
                next_search: now,
                next_recheck: now,
                failing_since: None,
                toasted: false,
                last_error: None,
                border_api,
                borderless: None,
                have_last: false,
                logged: None,
                resize_restart: None,
                last_started: None,
            })
        }

        /// This tick's picture, with the newest frame and its client crop
        /// (inside the frame) when the picture is `New`.
        pub fn poll(&mut self, now: Instant) -> (WindowPicture, Option<(ID3D11Texture2D, Rect)>) {
            self.track(now);
            let (found, minimized) = match &self.session {
                Some(s) => (true, unsafe { IsIconic(s.hwnd) }.as_bool()),
                None => self.found.map_or((false, false), |(_, m)| (true, m)),
            };
            let capturing = self.session.is_some();
            let frame = match self.newest_frame(now) {
                Ok(f) => f,
                Err(e) => {
                    self.warn(&e.context("capture frame"));
                    None
                }
            };
            if frame.is_some() && self.failing_since.take().is_some() {
                self.last_error = None;
            }
            let pic = decide(&PictureState { found, minimized, capturing, new_frame: frame.is_some(), have_last: self.have_last });
            self.log_state(found, minimized, capturing);
            match pic {
                WindowPicture::Black => {
                    self.have_last = false;
                    (pic, None)
                }
                WindowPicture::Same => (pic, None),
                WindowPicture::New => {
                    let (f, tex, crop) = frame.expect("New has a frame");
                    if let Some(old) = self.session.as_mut().and_then(|s| s.held.replace(f)) {
                        let _ = old.Close();
                    }
                    self.have_last = true;
                    (pic, Some((tex, crop)))
                }
            }
        }

        /// Finds, starts, re-checks and drops the capture session.
        fn track(&mut self, now: Instant) {
            if let Some(s) = self.session.as_mut() {
                let hwnd = s.hwnd;
                if unsafe { IsIconic(hwnd) }.as_bool() {
                    s.shown_since = None;
                } else if s.shown_since.is_none() {
                    s.shown_since = Some(now);
                }
                let last_restart = self.resize_restart.filter(|(h, _)| *h == hwnd).map(|(_, t)| t);
                if !unsafe { IsWindow(Some(hwnd)) }.as_bool() || s.closed.load(Ordering::SeqCst) {
                    tracing::info!("window capture: window {:?} closed", hwnd.0);
                    self.drop_session(now);
                } else if stalled(s.got_frame, s.shown_since, now) {
                    // Started, shown, but never a frame: not captured. Retry
                    // like a failed start; not captured since it was shown.
                    let shown = s.shown_since.unwrap_or(now);
                    self.warn(&anyhow::anyhow!("no frames from window {:?} for {}s", hwnd.0, STALL.as_secs()));
                    self.drop_session(now);
                    self.found = Some((hwnd, false));
                    self.next_search = now + RETRY;
                    self.failing(shown, now);
                } else if resize_restart_due(s.recreated, last_restart, now) {
                    // A static window sends no frame after the pool was
                    // recreated at its new size; a new session always does.
                    tracing::debug!("window capture: no frame after resize, restarting the session");
                    self.resize_restart = Some((hwnd, now));
                    self.drop_session(now);
                } else if now >= self.next_recheck {
                    self.next_recheck = now + RECHECK;
                    if let Some((h, _)) = game_window(&self.exes).filter(|(h, _)| *h != hwnd) {
                        tracing::info!("window capture: switching to window {:?}", h.0);
                        self.drop_session(now);
                    }
                }
            }
            if self.session.is_some() || now < self.next_search {
                return;
            }
            self.next_search = now + SEARCH;
            self.found = game_window(&self.exes);
            let Some((hwnd, _)) = self.found else {
                self.failing_since = None;
                return;
            };
            match self.start_session(hwnd) {
                Ok(s) => {
                    self.session = Some(s);
                    self.next_recheck = now + RECHECK;
                }
                Err(e) => {
                    self.warn(&e);
                    self.next_search = now + RETRY;
                    self.failing(now, now);
                }
            }
        }

        /// The window is found but not captured (failed start or no frames):
        /// toast once per capture when that lasts [`TOAST_AFTER`]. Cleared by
        /// the first frame or when the window disappears.
        fn failing(&mut self, since: Instant, now: Instant) {
            let since = *self.failing_since.get_or_insert(since);
            if !self.toasted && now.saturating_duration_since(since) >= TOAST_AFTER {
                self.toasted = true;
                (self.notice)(CANT_CAPTURE.to_string());
            }
        }

        fn drop_session(&mut self, now: Instant) {
            self.session = None;
            self.found = None;
            self.next_search = now;
        }

        fn start_session(&mut self, hwnd: HWND) -> Result<Session> {
            let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
                .context("IGraphicsCaptureItemInterop")?;
            let item: GraphicsCaptureItem =
                unsafe { interop.CreateForWindow(hwnd) }.context("CreateForWindow")?;
            let size = item.Size().context("capture item size")?;
            let size = SizeInt32 { Width: size.Width.max(1), Height: size.Height.max(1) };
            let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&self.device, FORMAT, 2, size)
                .context("CreateFreeThreaded frame pool")?;
            let session = pool.CreateCaptureSession(&item).context("CreateCaptureSession")?;
            if let Err(e) = session.SetIsCursorCaptureEnabled(true) {
                tracing::debug!("window capture: cursor capture not enabled: {e}");
            }
            if self.border_api && self.borderless() {
                let r = session.SetIsBorderRequired(false);
                if let Err(e) = r {
                    tracing::info!("window capture: capture border not removable: {e}");
                }
            }
            let closed = Arc::new(AtomicBool::new(false));
            let flag = closed.clone();
            let closed_token = item
                .Closed(&TypedEventHandler::new(move |_, _| {
                    flag.store(true, Ordering::SeqCst);
                    Ok(())
                }))
                .context("Closed handler")?;
            let s = Session {
                hwnd,
                item,
                pool,
                session,
                size,
                closed,
                closed_token,
                held: None,
                recreated: None,
                got_frame: false,
                shown_since: None,
            };
            s.session.StartCapture().context("StartCapture")?;
            // Restarts of the same window (resize, stall retry) log at debug.
            if self.last_started.replace(hwnd) == Some(hwnd) {
                tracing::debug!("window capture: restarted capture of window {:?}", hwnd.0);
            } else {
                tracing::info!(
                    "window capture: capturing window {:?} ({}x{} frame)",
                    hwnd.0,
                    size.Width,
                    size.Height
                );
            }
            Ok(s)
        }

        /// Asks once for borderless capture; on refusal the yellow border stays.
        fn borderless(&mut self) -> bool {
            *self.borderless.get_or_insert_with(|| {
                let r = GraphicsCaptureAccess::RequestAccessAsync(GraphicsCaptureAccessKind::Borderless)
                    .and_then(|op| op.join());
                match r {
                    Ok(AppCapabilityAccessStatus::Allowed) => true,
                    Ok(s) => {
                        tracing::info!("window capture: borderless capture not allowed ({s:?}), border stays");
                        false
                    }
                    Err(e) => {
                        tracing::info!("window capture: borderless capture not available ({e}), border stays");
                        false
                    }
                }
            })
        }

        /// Drains the pool to its newest frame: the frame, its texture and the
        /// client crop inside it. Recreates the pool when the window resized.
        /// A failed `Recreate` drops the session (it is started again through
        /// the normal search) instead of retrying every tick.
        fn newest_frame(&mut self, now: Instant) -> Result<Option<(Direct3D11CaptureFrame, ID3D11Texture2D, Rect)>> {
            let Some(s) = self.session.as_mut() else { return Ok(None) };
            let mut newest: Option<Direct3D11CaptureFrame> = None;
            while let Ok(f) = s.pool.TryGetNextFrame() {
                if let Some(old) = newest.replace(f) {
                    let _ = old.Close();
                }
            }
            let Some(f) = newest else { return Ok(None) };
            s.got_frame = true;
            let content = f.ContentSize()?;
            if (content.Width, content.Height) != (s.size.Width, s.size.Height) && content.Width > 0 && content.Height > 0
            {
                if let Err(e) = s.pool.Recreate(&self.device, FORMAT, 2, content) {
                    let _ = f.Close();
                    self.drop_session(now);
                    return Err(anyhow::Error::from(e).context("Recreate frame pool, restarting the session"));
                }
                s.size = content;
                s.recreated = Some(now);
            } else {
                s.recreated = None;
            }
            let access: IDirect3DDxgiInterfaceAccess = f.Surface()?.cast()?;
            let tex: ID3D11Texture2D = unsafe { access.GetInterface() }?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { tex.GetDesc(&mut desc) };
            let fw = desc.Width.min(content.Width.max(1) as u32);
            let fh = desc.Height.min(content.Height.max(1) as u32);
            let crop = fit(client_rect(s.hwnd, fw, fh), fw, fh);
            Ok(Some((f, tex, crop)))
        }

        fn warn(&mut self, e: &anyhow::Error) {
            let msg = format!("{e:#}");
            if self.last_error.as_deref() != Some(&msg) {
                tracing::warn!("window capture: {msg}");
            }
            self.last_error = Some(msg);
        }

        fn log_state(&mut self, found: bool, minimized: bool, capturing: bool) {
            let state = (found, minimized, capturing);
            if self.logged == Some(state) {
                return;
            }
            let was = self.logged.replace(state);
            let what = match state {
                (false, ..) => "no game window",
                (true, true, _) => "game window minimized",
                (true, false, false) => "game window found, not captured",
                (true, false, true) if was.is_some_and(|w| w.1) => "game window restored",
                (true, false, true) => "game window captured",
            };
            tracing::info!("window capture: {what}");
        }
    }

    /// Client area of `hwnd` inside its captured `fw` x `fh` frame; the whole
    /// frame if the window cannot be measured.
    fn client_rect(hwnd: HWND, fw: u32, fh: u32) -> Rect {
        let full = Rect { x: 0, y: 0, w: fw, h: fh };
        let mut b = RECT::default();
        let mut c = RECT::default();
        let mut origin = POINT::default();
        unsafe {
            if DwmGetWindowAttribute(
                hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS,
                (&mut b as *mut RECT).cast(),
                std::mem::size_of::<RECT>() as u32,
            )
            .is_err()
                || GetClientRect(hwnd, &mut c).is_err()
                || !ClientToScreen(hwnd, &mut origin).as_bool()
            {
                return full;
            }
        }
        client_crop(
            fw,
            fh,
            (b.left, b.top, b.right, b.bottom),
            (origin.x, origin.y, origin.x + c.right, origin.y + c.bottom),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test] fn resize_restart_waits_then_is_capped() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        assert!(!resize_restart_due(None, None, ms(5000)), "no recreate pending");
        assert!(!resize_restart_due(Some(t0), None, ms(249)), "frame may still come");
        assert!(resize_restart_due(Some(t0), None, ms(250)));
        // Persistent mismatch: restarted at 250 ms, recreated again at 300 ms.
        assert!(!resize_restart_due(Some(ms(300)), Some(ms(250)), ms(600)), "capped");
        assert!(!resize_restart_due(Some(ms(300)), Some(ms(250)), ms(2249)), "capped");
        assert!(resize_restart_due(Some(ms(300)), Some(ms(250)), ms(2250)), "one per 2 s");
    }
    #[test] fn stall_needs_ten_shown_seconds_without_any_frame() {
        let t0 = Instant::now();
        let s = |n| t0 + Duration::from_secs(n);
        assert!(!stalled(false, Some(t0), s(9)));
        assert!(stalled(false, Some(t0), s(10)));
        assert!(!stalled(true, Some(t0), s(60)), "a static window after its first frame is fine");
        assert!(!stalled(false, None, s(60)), "minimized does not count");
        assert!(!stalled(false, Some(s(50)), s(59)), "shown time restarts after a restore");
    }
    #[test] fn fit_keeps_inside_frame() {
        assert_eq!(fit(Rect { x: 8, y: 31, w: 1264, h: 681 }, 1280, 720), Rect { x: 8, y: 31, w: 1264, h: 681 });
        assert_eq!(fit(Rect { x: 8, y: 31, w: 1264, h: 681 }, 800, 600), Rect { x: 8, y: 31, w: 792, h: 569 });
        assert_eq!(fit(Rect { x: 900, y: 700, w: 10, h: 10 }, 800, 600), Rect { x: 799, y: 599, w: 1, h: 1 });
    }

    #[test] fn letterbox_same_aspect() { assert_eq!(letterbox(3840, 2160, 1920, 1080), Rect { x: 0, y: 0, w: 1920, h: 1080 }); }
    #[test] fn letterbox_ultrawide() { assert_eq!(letterbox(3440, 1440, 1920, 1080), Rect { x: 0, y: 138, w: 1920, h: 804 }); }
    #[test] fn letterbox_4_3() { assert_eq!(letterbox(1600, 1200, 1920, 1080), Rect { x: 240, y: 0, w: 1440, h: 1080 }); }
    #[test] fn letterbox_portrait() { assert_eq!(letterbox(1080, 1920, 1920, 1080), Rect { x: 656, y: 0, w: 608, h: 1080 }); }
    #[test] fn letterbox_tiny_source() { let r = letterbox(1, 1, 1920, 1080); assert!(r.w >= 2 && r.h >= 2 && r.x % 2 == 0 && r.y % 2 == 0); }
    #[test] fn letterbox_never_exceeds_output() {
        for (w, h) in [(1281, 721), (1366, 768), (2559, 1079), (7, 3000)] {
            let r = letterbox(w, h, 1920, 1080);
            assert!(r.x + r.w <= 1920 && r.y + r.h <= 1080, "{w}x{h}: {r:?}");
            assert!(r.w % 2 == 0 && r.h % 2 == 0 && r.x % 2 == 0 && r.y % 2 == 0, "{w}x{h}: {r:?}");
        }
    }
    #[test] fn client_crop_windowed() {
        assert_eq!(client_crop(1280, 720, (100, 100, 1380, 820), (108, 131, 1372, 812)), Rect { x: 8, y: 31, w: 1264, h: 681 });
    }
    #[test] fn client_crop_borderless_is_full() {
        assert_eq!(client_crop(1920, 1080, (0, 0, 1920, 1080), (0, 0, 1920, 1080)), Rect { x: 0, y: 0, w: 1920, h: 1080 });
    }
    #[test] fn client_crop_clamped_and_empty() {
        assert_eq!(client_crop(800, 600, (0, 0, 800, 600), (-50, -50, 900, 700)), Rect { x: 0, y: 0, w: 800, h: 600 });
        assert_eq!(client_crop(800, 600, (0, 0, 800, 600), (10, 10, 10, 10)), Rect { x: 0, y: 0, w: 800, h: 600 });
    }
    #[test] fn decide_cases() {
        let base = PictureState { found: true, minimized: false, capturing: true, new_frame: false, have_last: true };
        assert_eq!(decide(&PictureState { found: false, ..base }), WindowPicture::Black);
        assert_eq!(decide(&PictureState { minimized: true, new_frame: true, ..base }), WindowPicture::Black);
        assert_eq!(decide(&PictureState { capturing: false, ..base }), WindowPicture::Black);
        assert_eq!(decide(&PictureState { new_frame: true, ..base }), WindowPicture::New);
        assert_eq!(decide(&base), WindowPicture::Same);
    }
    #[test] fn restore_waits_for_fresh_frame() {
        // After a Black (minimized), the caller cleared have_last: restored window with no new frame stays black.
        let s = PictureState { found: true, minimized: false, capturing: true, new_frame: false, have_last: false };
        assert_eq!(decide(&s), WindowPicture::Black);
    }
}

/// Live window capture: needs an interactive desktop and a D3D11 GPU. Run with
/// `cargo test --lib capture::window_grab -- --ignored`.
#[cfg(all(test, windows))]
mod live {
    use super::test_window::*;
    use crate::capture::hdr_grab::HdrGrabber;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    #[ignore]
    fn window_capture_shows_window_only() {
        // 400x400 red window => pillarbox: centre is red, (100, 540) is black.
        let _l = live_lock();
        let win = TestWindow::open(400, 400, (255, 0, 0));
        let mut rig = Rig::start();
        let f = rig.last_frame_after(Duration::from_secs(2));
        assert_near(y_at(&f, 960, 540), 63, 3);
        assert_near(v_at(&f, 960, 540), 240, 3);
        assert_eq!(y_at(&f, 100, 540), 16);
        // Client edges (picture at x 420..1500, y 0..1080): no title bar or
        // border blended in by the scaler.
        for (x, y) in [(960, 0), (960, 1079), (420, 540), (1499, 540)] {
            assert_near(y_at(&f, x, y), 63, 3);
        }
        drop(win);
    }

    #[test]
    #[ignore]
    fn minimized_window_is_black() {
        // minimize => every Y 16, every UV 128 within 1 s; restore => red again within 1 s.
        let _l = live_lock();
        let win = TestWindow::open(400, 400, (255, 0, 0));
        let mut rig = Rig::start();
        rig.wait_for(Duration::from_secs(2), "red before minimize", red_centre);
        win.minimize();
        rig.wait_for(Duration::from_secs(1), "black while minimized", is_black);
        win.restore();
        rig.wait_for(Duration::from_secs(1), "red after restore", red_centre);
    }

    #[test]
    #[ignore]
    fn window_recreated_is_found_again() {
        // close the window, open a new one (green) => black, then green centre within 2 s.
        let _l = live_lock();
        let win = TestWindow::open(400, 400, (255, 0, 0));
        let mut rig = Rig::start();
        rig.wait_for(Duration::from_secs(2), "red first window", red_centre);
        win.close();
        rig.wait_for(Duration::from_secs(1), "black after close", is_black);
        let _green = TestWindow::open(400, 400, (0, 255, 0));
        rig.wait_for(Duration::from_secs(2), "green second window", green_centre);
    }

    #[test]
    #[ignore]
    fn resize_keeps_aspect() {
        // resize 400x400 => 800x400 (client): before, (100, 540) black (pillarbox);
        // after, letterbox 1920x960 at y 60: (100, 540) red, (960, 30) black.
        let _l = live_lock();
        let win = TestWindow::open(400, 400, (255, 0, 0));
        let mut rig = Rig::start();
        let f = rig.wait_for(Duration::from_secs(2), "red before resize", red_centre);
        assert_eq!(y_at(&f, 100, 540), 16);
        win.resize(800, 400);
        rig.wait_for(Duration::from_secs(2), "letterboxed after resize", |f| {
            near(y_at(f, 100, 540), 63, 3) && y_at(f, 960, 30) == 16 && red_centre(f)
        });
    }

    #[test]
    #[ignore]
    fn no_window_yet_is_black() {
        // exes = ["clipvault-no-such-game.exe"] => frames flow and are all black.
        let _l = live_lock();
        let cfg = config(vec!["clipvault-no-such-game.exe".into()], 1.0);
        let name = cfg.pipe_name.clone();
        let _g = HdrGrabber::start(cfg, Arc::new(|_| {})).expect("start grabber");
        let frames = read_frames(&name, 30);
        assert_eq!(frames.len(), 30);
        for (i, f) in frames.iter().enumerate() {
            assert!(is_black(f), "frame {i} not black");
        }
    }
}

#[cfg(all(test, windows))]
mod test_window;
