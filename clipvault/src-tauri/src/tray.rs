//! Tray icon: state icon (idle / recording / error, drawn in code), tooltip
//! with the health, and the menu Open library · Save clip · Desktop capture ☑ · Quit.
use crate::app::health::TrayState;
use crate::app::AppCore;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, Wry};

const SIZE: u32 = 32;

/// Handles kept in Tauri state to update the tray later.
#[derive(Clone)]
pub struct Tray {
    icon: TrayIcon<Wry>,
    desktop: CheckMenuItem<Wry>,
}

#[derive(Default)]
pub struct TrayHandle(pub Mutex<Option<Tray>>);

/// A filled circle with a darker ring, RGBA.
fn circle(rgb: [u8; 3]) -> Image<'static> {
    let mut px = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    let c = (SIZE as f32 - 1.0) / 2.0;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let d = ((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt();
            let (r, g, b, a) = if d <= c - 3.0 {
                (rgb[0], rgb[1], rgb[2], 255)
            } else if d <= c {
                (rgb[0] / 2, rgb[1] / 2, rgb[2] / 2, 255)
            } else {
                (0, 0, 0, 0)
            };
            px.extend_from_slice(&[r, g, b, a]);
        }
    }
    Image::new_owned(px, SIZE, SIZE)
}

pub fn icon_for(state: TrayState) -> Image<'static> {
    match state {
        TrayState::Idle => circle([140, 140, 140]),
        TrayState::Recording => circle([220, 40, 40]),
        TrayState::Error => circle([245, 170, 0]),
    }
}

pub fn show_main_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
        let _ = app.emit("window-visibility", WindowVisibility { visible: true });
    }
}

/// `window-visibility` event payload: the main window was hidden to the tray or shown again.
#[derive(Clone, serde::Serialize)]
pub struct WindowVisibility {
    pub visible: bool,
}

pub fn build(app: &AppHandle, core: Arc<AppCore>) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open library", true, None::<&str>)?;
    let save = MenuItem::with_id(app, "save", "Save clip", true, None::<&str>)?;
    let desktop =
        CheckMenuItem::with_id(app, "desktop", "Desktop capture", true, core.settings().desktop_capture, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&open, &save, &desktop, &sep, &quit])?;
    let icon = TrayIconBuilder::with_id("clipvault")
        .icon(icon_for(TrayState::Idle))
        .tooltip("ClipVault – idle")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, ev| match ev.id().as_ref() {
            "open" => show_main_window(app),
            "save" => {
                let t_press = SystemTime::now();
                core.request_save(t_press);
            }
            "desktop" => {
                core.toggle_desktop();
            }
            "quit" => {
                let (core, app) = (core.clone(), app.clone());
                tauri::async_runtime::spawn(async move {
                    core.shutdown().await;
                    app.exit(0);
                });
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, ev| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = ev {
                show_main_window(tray.app_handle());
            }
        })
        .build(app)?;
    if let Some(h) = app.try_state::<TrayHandle>() {
        *crate::locks::lock(&h.0) = Some(Tray { icon, desktop });
    }
    Ok(())
}

/// Clones the handles out of the state mutex; the lock is released before
/// any tray call (those block on the main thread when called from elsewhere).
fn handles(app: &AppHandle) -> Option<Tray> {
    let h = app.try_state::<TrayHandle>()?;
    let t = crate::locks::lock(&h.0).clone();
    t
}

/// Non-blocking from any thread: the update is queued to the main thread,
/// where the tray calls run directly. No lock is held while they run.
pub fn set_state(app: &AppHandle, state: TrayState, tooltip: &str) {
    let Some(t) = handles(app) else { return };
    let tooltip = tooltip.to_string();
    let _ = app.run_on_main_thread(move || {
        let _ = t.icon.set_icon(Some(icon_for(state)));
        let _ = t.icon.set_tooltip(Some(tooltip));
    });
}

pub fn set_desktop_checked(app: &AppHandle, on: bool) {
    let Some(t) = handles(app) else { return };
    let _ = app.run_on_main_thread(move || {
        let _ = t.desktop.set_checked(on);
    });
}
