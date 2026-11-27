//! Live-test helpers for the window source: a solid-colour Win32 window owned
//! by this (test) process, and a grabber recording it whose NV12 frames are
//! read from its pipe like FFmpeg would.
use crate::capture::hdr_grab::{nv12_size, HdrGrabConfig, HdrGrabber};
use std::fs::File;
use std::io::Read;
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::CreateSolidBrush;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::WindowsAndMessaging::*;

pub const OUT_W: u32 = 1920;
pub const OUT_H: u32 = 1080;

/// Live tests share the desktop and look windows up by this exe's name: one
/// at a time.
pub fn live_lock() -> MutexGuard<'static, ()> {
    static LIVE: Mutex<()> = Mutex::new(());
    LIVE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Lowercase file name of the test binary (what `game_window` matches).
pub fn this_exe() -> String {
    let p = std::env::current_exe().unwrap();
    p.file_name().unwrap().to_string_lossy().to_ascii_lowercase()
}

fn hwnd(v: isize) -> HWND {
    HWND(v as *mut core::ffi::c_void)
}

/// Top-level window filled with one colour, pumped on its own thread.
pub struct TestWindow {
    hwnd: isize,
    thread: Option<JoinHandle<()>>,
}

unsafe extern "system" fn wndproc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if m == WM_DESTROY {
        unsafe { PostQuitMessage(0) };
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(h, m, w, l) }
}

impl TestWindow {
    /// Opens a window whose client area is `w` x `h` physical pixels.
    pub fn open(w: i32, h: i32, rgb: (u8, u8, u8)) -> TestWindow {
        let (tx, rx) = mpsc::channel::<isize>();
        let thread = std::thread::spawn(move || unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let class: Vec<u16> =
                format!("cv_test_{}", uuid::Uuid::new_v4().simple()).encode_utf16().chain([0]).collect();
            let inst = GetModuleHandleW(None).unwrap();
            let colour = rgb.0 as u32 | (rgb.1 as u32) << 8 | (rgb.2 as u32) << 16;
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: inst.into(),
                lpszClassName: PCWSTR(class.as_ptr()),
                hbrBackground: CreateSolidBrush(COLORREF(colour)),
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                ..Default::default()
            };
            assert!(RegisterClassW(&wc) != 0, "RegisterClassW");
            let title: Vec<u16> = "ClipVault test window".encode_utf16().chain([0]).collect();
            let win = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(class.as_ptr()),
                PCWSTR(title.as_ptr()),
                WS_OVERLAPPEDWINDOW,
                120,
                120,
                w,
                h,
                None,
                None,
                Some(inst.into()),
                None,
            )
            .unwrap();
            set_client_size(win, w, h);
            let _ = ShowWindow(win, SW_SHOWNORMAL);
            tx.send(win.0 as isize).unwrap();
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        });
        let hwnd = rx.recv().unwrap();
        TestWindow { hwnd, thread: Some(thread) }
    }

    pub fn minimize(&self) {
        unsafe {
            let _ = ShowWindow(hwnd(self.hwnd), SW_MINIMIZE);
        }
    }

    pub fn restore(&self) {
        unsafe {
            let _ = ShowWindow(hwnd(self.hwnd), SW_RESTORE);
        }
    }

    /// New client size in physical pixels.
    pub fn resize(&self, w: i32, h: i32) {
        unsafe { set_client_size(hwnd(self.hwnd), w, h) };
    }

    pub fn close(mut self) {
        self.shut();
    }

    fn shut(&mut self) {
        if let Some(t) = self.thread.take() {
            unsafe {
                let _ = PostMessageW(Some(hwnd(self.hwnd)), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
            let _ = t.join();
        }
    }
}

impl Drop for TestWindow {
    fn drop(&mut self) {
        self.shut();
    }
}

unsafe fn set_client_size(win: HWND, w: i32, h: i32) {
    let (mut wr, mut cr) = (RECT::default(), RECT::default());
    unsafe {
        let _ = GetWindowRect(win, &mut wr);
        let _ = GetClientRect(win, &mut cr);
        let dw = (wr.right - wr.left) - (cr.right - cr.left);
        let dh = (wr.bottom - wr.top) - (cr.bottom - cr.top);
        let _ = SetWindowPos(win, None, 0, 0, w + dw, h + dh, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

/// SDR white of the monitor showing this process' window (the controller's
/// rule), 1.0 when unknown.
pub fn window_sdr_white() -> f32 {
    use crate::capture::display::{list_displays, window_monitor};
    let Some(mon) = window_monitor(std::process::id()) else { return 1.0 };
    let displays = list_displays().unwrap_or_default();
    displays
        .iter()
        .find(|d| d.id.eq_ignore_ascii_case(&mon))
        .map_or(1.0, |d| crate::app::capture_ctl::sdr_white(d.hdr, d.sdr_white_level))
}

/// Grabber config recording the window source for `exes` at 1920x1080@30.
pub fn config(exes: Vec<String>, sdr_white: f32) -> HdrGrabConfig {
    HdrGrabConfig {
        output_idx: 0,
        fps: 30,
        out_w: OUT_W,
        out_h: OUT_H,
        sdr_white,
        pipe_name: format!("clipvault_wtest_{}", uuid::Uuid::new_v4().simple()),
        window_exes: Some(exes),
    }
}

/// Opens `\\.\pipe\<name>` as a client and reads `n` NV12 frames.
pub fn read_frames(pipe_name: &str, n: usize) -> Vec<Vec<u8>> {
    let mut f = File::open(format!(r"\\.\pipe\{pipe_name}")).unwrap();
    (0..n)
        .map(|_| {
            let mut buf = vec![0u8; nv12_size(OUT_W, OUT_H)];
            f.read_exact(&mut buf).unwrap();
            buf
        })
        .collect()
}

/// A running window-source grabber for this test binary plus its pipe client.
pub struct Rig {
    _grabber: HdrGrabber,
    pipe: File,
}

impl Rig {
    /// Records the windows of this process (open the window first: SDR white
    /// comes from its monitor).
    pub fn start() -> Rig {
        let cfg = config(vec![this_exe()], window_sdr_white());
        let name = cfg.pipe_name.clone();
        let grabber = HdrGrabber::start(cfg, Arc::new(|m| println!("notice: {m}"))).expect("start grabber");
        let pipe = File::open(format!(r"\\.\pipe\{name}")).unwrap();
        Rig { _grabber: grabber, pipe }
    }

    pub fn next(&mut self) -> Vec<u8> {
        let mut buf = vec![0u8; nv12_size(OUT_W, OUT_H)];
        self.pipe.read_exact(&mut buf).unwrap();
        buf
    }

    /// Reads frames for `d` and returns the last one.
    pub fn last_frame_after(&mut self, d: Duration) -> Vec<u8> {
        let t = Instant::now();
        let mut f = self.next();
        while t.elapsed() < d {
            f = self.next();
        }
        f
    }

    /// Reads frames until one satisfies `ok`; panics after `timeout`.
    pub fn wait_for(&mut self, timeout: Duration, what: &str, ok: impl Fn(&[u8]) -> bool) -> Vec<u8> {
        let t = Instant::now();
        loop {
            let f = self.next();
            if ok(&f) {
                return f;
            }
            if t.elapsed() > timeout {
                panic!(
                    "{what}: not within {timeout:?}; last frame centre Y {} U {} V {}, (100,540) Y {}, (960,30) Y {}",
                    y_at(&f, 960, 540),
                    u_at(&f, 960, 540),
                    v_at(&f, 960, 540),
                    y_at(&f, 100, 540),
                    y_at(&f, 960, 30)
                );
            }
        }
    }
}

pub fn y_at(f: &[u8], x: u32, y: u32) -> u8 {
    f[(y * OUT_W + x) as usize]
}

pub fn u_at(f: &[u8], x: u32, y: u32) -> u8 {
    f[(OUT_W * OUT_H + (y / 2) * OUT_W + (x & !1)) as usize]
}

pub fn v_at(f: &[u8], x: u32, y: u32) -> u8 {
    f[(OUT_W * OUT_H + (y / 2) * OUT_W + (x & !1) + 1) as usize]
}

pub fn near(a: u8, b: u8, tol: u8) -> bool {
    a.abs_diff(b) <= tol
}

#[track_caller]
pub fn assert_near(a: u8, b: u8, tol: u8) {
    assert!(near(a, b, tol), "{a} not within {tol} of {b}");
}

/// Every Y 16 and every UV 128.
pub fn is_black(f: &[u8]) -> bool {
    let luma = (OUT_W * OUT_H) as usize;
    f[..luma].iter().all(|&y| y == 16) && f[luma..].iter().all(|&c| c == 128)
}

/// Pure red (BT.709 limited: Y 63, U 102, V 240) at the output centre.
pub fn red_centre(f: &[u8]) -> bool {
    near(y_at(f, 960, 540), 63, 3) && near(u_at(f, 960, 540), 102, 3) && near(v_at(f, 960, 540), 240, 3)
}

/// Pure green (Y 173, U 42, V 26) at the output centre.
pub fn green_centre(f: &[u8]) -> bool {
    near(y_at(f, 960, 540), 173, 3) && near(u_at(f, 960, 540), 42, 3) && near(v_at(f, 960, 540), 26, 3)
}
