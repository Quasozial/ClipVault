//! Global hotkeys: save clip, add a manual marker, toggle desktop capture.
//!
//! Registration goes through [`HotkeyBackend`] so the bookkeeping (which
//! accelerator failed, which one maps to which action) is testable without a
//! real `RegisterHotKey`. Failures never abort the others: every binding is
//! tried and each failure comes back as a [`HotkeyError`] (shown as a toast
//! and returned to the Settings UI).
use crate::settings::Settings;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum HotkeyAction {
    SaveClip,
    Mark,
    ToggleDesktop,
}

impl HotkeyAction {
    pub fn label(self) -> &'static str {
        match self {
            HotkeyAction::SaveClip => "Save clip",
            HotkeyAction::Mark => "Add marker",
            HotkeyAction::ToggleDesktop => "Toggle desktop capture",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HotkeyError {
    pub action: HotkeyAction,
    pub accelerator: String,
    pub reason: String,
}

impl std::fmt::Display for HotkeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Hotkey {} ({}) could not be registered: {}", self.accelerator, self.action.label(), self.reason)
    }
}

/// Registers one accelerator system-wide. Returns an id the key-press handler
/// will report for it, or why it failed (invalid text, already taken, ...).
pub trait HotkeyBackend {
    fn register(&mut self, accelerator: &str) -> Result<u32, String>;
}

/// The configured bindings, in registration order. Empty accelerators are
/// skipped (hotkey disabled).
pub fn bindings(s: &Settings) -> Vec<(HotkeyAction, String)> {
    [
        (HotkeyAction::SaveClip, &s.hotkey_save),
        (HotkeyAction::Mark, &s.hotkey_mark),
        (HotkeyAction::ToggleDesktop, &s.hotkey_toggle_desktop),
    ]
    .into_iter()
    .filter(|(_, a)| !a.trim().is_empty())
    .map(|(act, a)| (act, a.trim().to_string()))
    .collect()
}

/// Registers every binding. Returns the (id, action) map of successful ones and
/// one error per binding that failed.
pub fn register_bindings(
    backend: &mut dyn HotkeyBackend,
    bindings: &[(HotkeyAction, String)],
) -> (Vec<(u32, HotkeyAction)>, Vec<HotkeyError>) {
    let mut ok = Vec::new();
    let mut errors = Vec::new();
    for (action, accel) in bindings {
        match backend.register(accel) {
            Ok(id) => ok.push((id, *action)),
            Err(reason) => errors.push(HotkeyError { action: *action, accelerator: accel.clone(), reason }),
        }
    }
    (ok, errors)
}

/// Tauri side: the global-shortcut plugin as a [`HotkeyBackend`], the
/// id ⇒ action map the plugin handler consults, and `register_all`.
pub mod tauri_backend {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use tauri::{AppHandle, Manager, Runtime};
    use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut};

    /// id ⇒ action for the currently registered hotkeys (Tauri state).
    #[derive(Default)]
    pub struct HotkeyMap(pub Mutex<HashMap<u32, HotkeyAction>>);

    /// Last registration errors, for the Settings UI (Tauri state).
    #[derive(Default)]
    pub struct HotkeyErrors(pub Mutex<Vec<HotkeyError>>);

    struct Backend<'a, R: Runtime>(&'a AppHandle<R>);

    impl<R: Runtime> HotkeyBackend for Backend<'_, R> {
        fn register(&mut self, accelerator: &str) -> Result<u32, String> {
            let sc: Shortcut = accelerator.parse().map_err(|e| format!("invalid hotkey: {e}"))?;
            let gs = self.0.global_shortcut();
            if gs.is_registered(sc) {
                return Err("already used by another ClipVault hotkey".into());
            }
            gs.register(sc).map_err(|e| e.to_string())?;
            Ok(sc.id())
        }
    }

    /// Unregisters all ClipVault hotkeys, then registers the configured ones.
    pub fn register_all<R: Runtime>(app: &AppHandle<R>, s: &Settings) -> Vec<HotkeyError> {
        if let Err(e) = app.global_shortcut().unregister_all() {
            tracing::warn!("unregistering hotkeys: {e}");
        }
        let (ok, errors) = register_bindings(&mut Backend(app), &bindings(s));
        for (id, action) in &ok {
            let accel = bindings(s).into_iter().find(|(a, _)| a == action).map(|(_, k)| k).unwrap_or_default();
            tracing::info!("hotkey registered: {accel} => {} (id {id})", action.label());
        }
        for e in &errors {
            tracing::warn!("{e}");
        }
        if let Some(map) = app.try_state::<HotkeyMap>() {
            *map.0.lock().unwrap() = ok.into_iter().collect();
        }
        if let Some(errs) = app.try_state::<HotkeyErrors>() {
            *errs.0.lock().unwrap() = errors.clone();
        }
        errors
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Behaves like RegisterHotKey: an accelerator can be held only once.
    #[derive(Default)]
    struct FakeBackend {
        taken: HashSet<String>,
        next: u32,
    }

    impl HotkeyBackend for FakeBackend {
        fn register(&mut self, accelerator: &str) -> Result<u32, String> {
            if !self.taken.insert(accelerator.to_ascii_lowercase()) {
                return Err("hotkey already registered".into());
            }
            self.next += 1;
            Ok(self.next)
        }
    }

    #[test]
    fn hotkey_conflict_reported() {
        let mut s = Settings::default();
        s.hotkey_save = "Ctrl+Shift+F10".into();
        s.hotkey_mark = "Ctrl+Shift+F10".into();
        let mut b = FakeBackend::default();
        let (ok, errors) = register_bindings(&mut b, &bindings(&s));
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].accelerator, "Ctrl+Shift+F10");
        assert_eq!(errors[0].action, HotkeyAction::Mark);
        assert!(errors[0].to_string().contains("Ctrl+Shift+F10"));
        // The other two still work.
        let acts: Vec<HotkeyAction> = ok.iter().map(|(_, a)| *a).collect();
        assert_eq!(acts, [HotkeyAction::SaveClip, HotkeyAction::ToggleDesktop]);
    }

    #[test]
    fn empty_hotkey_is_disabled_not_an_error() {
        let mut s = Settings::default();
        s.hotkey_mark = "  ".into();
        let b = bindings(&s);
        assert_eq!(b.len(), 2);
        let (ok, errors) = register_bindings(&mut FakeBackend::default(), &b);
        assert!(errors.is_empty());
        assert_eq!(ok.len(), 2);
    }
}
