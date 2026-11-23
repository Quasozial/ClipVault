//! Desktop kick (ruling R8): Desktop Duplication (ddagrab) delivers no frame
//! until something on the monitor changes, so FFmpeg started on a static
//! desktop produces nothing and the watchdog would restart it forever.
//!
//! A kick briefly shows a 2x2 px, practically invisible (alpha 1/255),
//! click-through, non-activating topmost tool window in the top-left corner of
//! the captured monitor and changes its alpha once, then hides it again. DWM
//! recomposes that corner, which Desktop Duplication reports as a new frame.
//! The supervisor kicks only when segments are missing: a new FFmpeg process
//! without a first segment after 2 s, or ≥ 3.5 s since the last segment
//! (normal cadence is one every 2 s), at most 3 times per FFmpeg process and
//! never while the watchdog is failing. A game rendering frames keeps segments
//! flowing, so the window does not appear over a running game in normal
//! operation; it can appear briefly while capture is stalling.
//!
//! The window lives on its own thread (window messages need a thread that owns
//! the window); [`DesktopKicker::kick`] only posts the output index and returns.
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long each of the two kick states is shown (≥ 2 frames at 60 Hz).
const HOLD: Duration = Duration::from_millis(34);

pub struct DesktopKicker {
    tx: Mutex<Sender<u32>>,
}

impl DesktopKicker {
    /// Starts the kick thread. `None` if the window could not be created.
    pub fn start() -> Option<Arc<DesktopKicker>> {
        let (tx, rx) = channel::<u32>();
        let (ready_tx, ready_rx) = channel::<bool>();
        std::thread::Builder::new()
            .name("desktop-kick".into())
            .spawn(move || imp::run(rx, ready_tx))
            .ok()?;
        match ready_rx.recv() {
            Ok(true) => Some(Arc::new(DesktopKicker { tx: Mutex::new(tx) })),
            _ => None,
        }
    }

    /// Requests a kick of ddagrab output `output_idx`; never blocks.
    pub fn kick(&self, output_idx: u32) {
        let _ = crate::locks::lock(&self.tx).send(output_idx);
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use windows::core::w;
    use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, PeekMessageW, RegisterClassExW,
        SetLayeredWindowAttributes, SetWindowPos, ShowWindow, TranslateMessage, HWND_TOPMOST, LWA_ALPHA, MSG,
        PM_REMOVE, SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_HIDE, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
        WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
    };

    unsafe extern "system" fn wndproc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        unsafe { DefWindowProcW(h, m, wp, lp) }
    }

    fn pump() {
        let mut msg = MSG::default();
        // SAFETY: plain message pump on the thread that owns the window.
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    /// Desktop rectangle of ddagrab output `idx` (adapter 0, as ddagrab uses).
    fn output_rect(idx: u32) -> Option<RECT> {
        // SAFETY: COM calls on freshly created DXGI objects.
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
            let output = factory.EnumAdapters1(0).ok()?.EnumOutputs(idx).ok()?;
            Some(output.GetDesc().ok()?.DesktopCoordinates)
        }
    }

    fn create_window() -> Option<HWND> {
        // SAFETY: standard window class registration and creation.
        unsafe {
            let hinst = GetModuleHandleW(None).ok()?;
            let class = w!("ClipVaultDesktopKick");
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(wndproc),
                hInstance: hinst.into(),
                lpszClassName: class,
                ..Default::default()
            };
            // Fails harmlessly if already registered (second kicker in one process).
            RegisterClassExW(&wc);
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE,
                class,
                w!(""),
                WS_POPUP,
                0,
                0,
                2,
                2,
                None,
                None,
                Some(hinst.into()),
                None,
            )
            .ok()?;
            SetLayeredWindowAttributes(hwnd, COLORREF(0), 1, LWA_ALPHA).ok()?;
            Some(hwnd)
        }
    }

    fn kick(hwnd: HWND, idx: u32) {
        let (x, y) = output_rect(idx).map_or((0, 0), |r| (r.left, r.top));
        // SAFETY: hwnd is our own window, used on its owning thread.
        unsafe {
            let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, 2, 2, SWP_NOACTIVATE | SWP_SHOWWINDOW);
            pump();
            std::thread::sleep(HOLD);
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 2, LWA_ALPHA);
            pump();
            std::thread::sleep(HOLD);
            let _ = ShowWindow(hwnd, SW_HIDE);
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 1, LWA_ALPHA);
            pump();
        }
    }

    pub(super) fn run(rx: Receiver<u32>, ready: Sender<bool>) {
        let Some(hwnd) = create_window() else {
            tracing::warn!("desktop kick window could not be created; static desktops may not record");
            let _ = ready.send(false);
            return;
        };
        let _ = ready.send(true);
        loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(mut idx) => {
                    // Coalesce queued requests: one kick per burst.
                    while let Ok(i) = rx.try_recv() {
                        idx = i;
                    }
                    kick(hwnd, idx);
                }
                Err(RecvTimeoutError::Timeout) => pump(),
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        // SAFETY: our window, on its owning thread.
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;
    pub(super) fn run(_rx: Receiver<u32>, ready: Sender<bool>) {
        let _ = ready.send(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kicker_starts_and_kicks_without_blocking() {
        let k = DesktopKicker::start().expect("kick window");
        let t = std::time::Instant::now();
        for _ in 0..5 {
            k.kick(0);
        }
        k.kick(99); // nonexistent output: falls back to (0, 0), no panic
        assert!(t.elapsed() < Duration::from_millis(20), "kick must not block");
        std::thread::sleep(Duration::from_millis(300));
    }
}
