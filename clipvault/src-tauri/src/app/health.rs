//! Pure health presentation logic: which health changes deserve a toast, and
//! which tray state a health maps to.
use crate::capture::supervisor::Health;
use std::time::{Duration, Instant};

/// Recording must hold this long after `Failing` before "recovered" is announced.
pub const RECOVERY_DEBOUNCE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthToast {
    Failing(String),
    Recovered,
}

impl HealthToast {
    pub fn text(&self) -> String {
        match self {
            HealthToast::Failing(r) => format!("Capture failing: {r}"),
            HealthToast::Recovered => "Capture recovered".into(),
        }
    }
}

/// Toast dedupe by health *variant*: one toast when entering `Failing`, one
/// when `Recording` has held for [`RECOVERY_DEBOUNCE`] after it. A
/// Failing → Recording → Failing flicker inside the debounce is silent, so is
/// a change of the failing reason. `Idle` (capture stopped on purpose) clears
/// everything without a toast.
#[derive(Debug, Default)]
pub struct HealthToaster {
    failing_announced: bool,
    recording_since: Option<Instant>,
}

impl HealthToaster {
    pub fn on_health(&mut self, h: &Health, now: Instant) -> Option<HealthToast> {
        match h {
            Health::Failing { reason } => {
                self.recording_since = None;
                if self.failing_announced {
                    return None;
                }
                self.failing_announced = true;
                Some(HealthToast::Failing(reason.clone()))
            }
            Health::Recording => {
                if self.failing_announced && self.recording_since.is_none() {
                    self.recording_since = Some(now);
                }
                None
            }
            Health::Restarting { .. } => {
                self.recording_since = None;
                None
            }
            Health::Idle => {
                *self = Self::default();
                None
            }
        }
    }

    /// Call periodically (≤ 0.5 s) to emit a debounced recovery.
    pub fn poll(&mut self, now: Instant) -> Option<HealthToast> {
        let since = self.recording_since?;
        if now.saturating_duration_since(since) < RECOVERY_DEBOUNCE {
            return None;
        }
        *self = Self::default();
        Some(HealthToast::Recovered)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayState {
    Idle,
    Recording,
    Error,
}

pub fn tray_state(h: &Health) -> TrayState {
    match h {
        Health::Idle => TrayState::Idle,
        Health::Recording => TrayState::Recording,
        Health::Restarting { .. } | Health::Failing { .. } => TrayState::Error,
    }
}

/// FFmpeg could not open the selected monitor (asleep, unplugged, renumbered).
pub fn is_missing_output(h: &Health) -> bool {
    match h {
        Health::Restarting { reason } | Health::Failing { reason } => {
            reason.contains("Failed to enumerate DXGI output")
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failing(r: &str) -> Health {
        Health::Failing { reason: r.into() }
    }
    fn restarting(r: &str) -> Health {
        Health::Restarting { reason: r.into() }
    }

    /// Feeds `(ms, health)` in order, polling every 100 ms in between and
    /// for 5 s after the last one; returns the toasts with their times.
    fn run(seq: &[(u64, Health)]) -> Vec<(u64, HealthToast)> {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut t = HealthToaster::default();
        let mut out = Vec::new();
        let end = seq.last().map_or(0, |(ms, _)| *ms) + 5000;
        let mut i = 0;
        let mut ms = 0;
        while ms <= end {
            while i < seq.len() && seq[i].0 <= ms {
                if let Some(x) = t.on_health(&seq[i].1, at(seq[i].0)) {
                    out.push((seq[i].0, x));
                }
                i += 1;
            }
            if let Some(x) = t.poll(at(ms)) {
                out.push((ms, x));
            }
            ms += 100;
        }
        out
    }

    #[test]
    fn one_toast_entering_failing_and_one_on_recovery() {
        let got = run(&[
            (0, restarting("starting")),
            (2000, Health::Recording),
            (10_000, restarting("FFmpeg exited")),
            (11_000, failing("FFmpeg exited (4 restarts within 60 s)")),
            (21_000, failing("no new segment for 5 s")), // reason change: silent
            (30_000, Health::Recording),
        ]);
        assert_eq!(
            got,
            vec![
                (11_000, HealthToast::Failing("FFmpeg exited (4 restarts within 60 s)".into())),
                (32_000, HealthToast::Recovered),
            ]
        );
    }

    #[test]
    fn flicker_inside_debounce_is_silent() {
        // Final ingest of a killed run: Failing -> Recording -> Failing within 2 s.
        let got = run(&[(0, failing("a")), (5000, Health::Recording), (5300, failing("b"))]);
        assert_eq!(got, vec![(0, HealthToast::Failing("a".into()))]);
    }

    #[test]
    fn restarting_after_recovery_start_delays_recovery() {
        let got = run(&[
            (0, failing("a")),
            (1000, Health::Recording),
            (1500, restarting("x")),
            (4000, Health::Recording),
        ]);
        assert_eq!(got, vec![(0, HealthToast::Failing("a".into())), (6000, HealthToast::Recovered)]);
    }

    #[test]
    fn idle_clears_without_toast_and_restarting_alone_is_silent() {
        let got = run(&[(0, failing("a")), (1000, Health::Idle), (2000, restarting("starting")), (3000, Health::Recording)]);
        assert_eq!(got, vec![(0, HealthToast::Failing("a".into()))]);
        // A second failure after Idle is announced again.
        let got = run(&[(0, failing("a")), (1000, Health::Idle), (2000, failing("b"))]);
        assert_eq!(got, vec![(0, HealthToast::Failing("a".into())), (2000, HealthToast::Failing("b".into()))]);
    }

    #[test]
    fn tray_states() {
        assert_eq!(tray_state(&Health::Idle), TrayState::Idle);
        assert_eq!(tray_state(&Health::Recording), TrayState::Recording);
        assert_eq!(tray_state(&restarting("x")), TrayState::Error);
        assert_eq!(tray_state(&failing("x")), TrayState::Error);
    }

    #[test]
    fn missing_output_detected_from_reason() {
        assert!(is_missing_output(&restarting("FFmpeg exited (exit code 1); FFmpeg: Failed to enumerate DXGI output 1")));
        assert!(!is_missing_output(&restarting("no new segment for 5 s")));
        assert!(!is_missing_output(&Health::Recording));
    }
}
