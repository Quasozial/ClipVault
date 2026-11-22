//! On-screen "Clip saved" banner: a small borderless, transparent,
//! always-on-top window in the top-right corner of the monitor the cursor is
//! on (the game's monitor), shown for a few seconds after each save request.
//!
//! Windows toasts are swallowed by Focus Assist while a game runs, so this is
//! the visible confirmation in game. The window never takes focus
//! (`WS_EX_NOACTIVATE`), ignores the mouse and is hidden, not destroyed,
//! between banners. Exclusive-fullscreen games draw over every window, so
//! there only the sound confirms the save.
use crate::notify::BannerKind;
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Manager, PhysicalPosition, PhysicalSize, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

pub const LABEL: &str = "overlay";
/// Logical size of the banner window (the page centres its card inside).
const WIDTH: f64 = 380.0;
const HEIGHT: f64 = 96.0;
/// Logical gap to the monitor's top-right corner.
const MARGIN: f64 = 16.0;
const VISIBLE_FOR: Duration = Duration::from_millis(3200);

#[derive(Debug, Clone, Serialize)]
pub struct Banner {
    pub kind: BannerKind,
    pub title: String,
    pub detail: String,
    /// Bumped per banner so the page restarts its entry animation.
    pub seq: u64,
}

static SEQ: AtomicU64 = AtomicU64::new(0);
/// Serialises create/show/hide so two quick saves cannot race the window.
static LOCK: Mutex<()> = Mutex::new(());

/// Shows `title`/`detail` as a banner. Returns at once: building the webview
/// must not run on (or block) the thread that handled the hotkey.
pub fn show(app: &AppHandle, kind: BannerKind, title: impl Into<String>, detail: impl Into<String>) {
    let seq = SEQ.fetch_add(1, Ordering::SeqCst) + 1;
    let banner = Banner { kind, title: title.into(), detail: detail.into(), seq };
    let app = app.clone();
    std::thread::spawn(move || {
        if let Err(e) = show_blocking(&app, &banner) {
            tracing::warn!("overlay banner failed: {e}");
            return;
        }
        std::thread::sleep(VISIBLE_FOR);
        // Only the newest banner hides the window.
        if SEQ.load(Ordering::SeqCst) == banner.seq {
            let _guard = LOCK.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(w) = app.get_webview_window(LABEL) {
                let _ = w.hide();
            }
        }
    });
}

fn show_blocking(app: &AppHandle, banner: &Banner) -> tauri::Result<()> {
    let _guard = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let json = serde_json::to_string(banner).unwrap_or_else(|_| "null".into());
    let w = match app.get_webview_window(LABEL) {
        Some(w) => {
            w.eval(format!("window.__showBanner && window.__showBanner({json})"))?;
            w
        }
        None => build(app, &json)?,
    };
    place(app, &w)?;
    let _ = w.set_always_on_top(true);
    w.show()?;
    Ok(())
}

fn build(app: &AppHandle, first_json: &str) -> tauri::Result<WebviewWindow> {
    let w = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("overlay.html".into()))
        .title("ClipVault banner")
        .inner_size(WIDTH, HEIGHT)
        .resizable(false)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .focused(false)
        .focusable(false)
        .visible(false)
        // Runs before the page script, so the first banner needs no event.
        .initialization_script(format!("window.__initialBanner = {first_json};"))
        .build()?;
    let _ = w.set_ignore_cursor_events(true);
    Ok(w)
}

/// Top-right corner of the monitor under the cursor (in a game that is the
/// game's monitor), else the primary one.
fn place(app: &AppHandle, w: &WebviewWindow) -> tauri::Result<()> {
    let monitor = app
        .cursor_position()
        .ok()
        .and_then(|p| app.monitor_from_point(p.x, p.y).ok().flatten())
        .or_else(|| app.primary_monitor().ok().flatten());
    let Some(m) = monitor else { return Ok(()) };
    let scale = m.scale_factor();
    let size = PhysicalSize::new((WIDTH * scale).round() as u32, (HEIGHT * scale).round() as u32);
    let margin = (MARGIN * scale).round() as i32;
    let pos = PhysicalPosition::new(
        m.position().x + m.size().width as i32 - size.width as i32 - margin,
        m.position().y + margin,
    );
    w.set_size(size)?;
    w.set_position(pos)?;
    Ok(())
}
