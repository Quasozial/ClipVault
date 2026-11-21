//! [`Ui`] for the running app: Windows toasts, sounds, tray, events to the UI
//! (`health-changed`, `clip-saved`, `mic-level`, `settings-changed`).
use super::health::tray_state;
use crate::capture::supervisor::Health;
use crate::db::clips::Clip;
use crate::notify::{play_sound, Sound, Ui};
use crate::tray;
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tauri_plugin_notification::NotificationExt;

#[derive(Debug, Clone, Serialize)]
pub struct HealthPayload {
    /// `idle` | `recording` | `restarting` | `failing`
    pub state: &'static str,
    pub reason: Option<String>,
    pub target: Option<String>,
}

impl HealthPayload {
    pub fn new(h: &Health, target: Option<&str>) -> Self {
        let (state, reason) = match h {
            Health::Idle => ("idle", None),
            Health::Recording => ("recording", None),
            Health::Restarting { reason } => ("restarting", Some(reason.clone())),
            Health::Failing { reason } => ("failing", Some(reason.clone())),
        };
        HealthPayload { state, reason, target: target.map(str::to_string) }
    }

    pub fn tooltip(&self) -> String {
        let mut t = match (self.state, &self.target) {
            ("recording", Some(g)) => format!("ClipVault – recording {g}"),
            ("idle", _) => "ClipVault – idle".to_string(),
            (s, _) => format!("ClipVault – {s}"),
        };
        if let Some(r) = &self.reason {
            t.push_str(": ");
            t.push_str(r);
        }
        // Windows tray tooltips are limited to 127 characters.
        t.chars().take(127).collect()
    }
}

pub struct TauriUi {
    pub app: AppHandle,
}

impl Ui for TauriUi {
    fn toast(&self, title: &str, body: &str) {
        if let Err(e) = self.app.notification().builder().title(title).body(body).show() {
            tracing::warn!("toast failed ({e}): {body}");
        }
    }

    fn play(&self, sound: Sound) {
        play_sound(sound);
    }

    fn health_changed(&self, health: &Health, target: Option<&str>) {
        let p = HealthPayload::new(health, target);
        tray::set_state(&self.app, tray_state(health), &p.tooltip());
        let _ = self.app.emit("health-changed", p);
    }

    fn clip_saved(&self, clip: &Clip) {
        let _ = self.app.emit("clip-saved", clip);
    }

    fn mic_level(&self, dbfs: f32) {
        #[derive(Serialize, Clone)]
        struct MicLevel {
            dbfs: f32,
        }
        let _ = self.app.emit("mic-level", MicLevel { dbfs });
    }

    fn desktop_capture_changed(&self, on: bool) {
        tray::set_desktop_checked(&self.app, on);
    }

    fn settings_changed(&self) {
        let _ = self.app.emit("settings-changed", ());
    }
}
