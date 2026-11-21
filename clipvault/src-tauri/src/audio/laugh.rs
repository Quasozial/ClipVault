//! Laugh detector: loudness-spike detector over the mic stream.
//!
//! Input is interleaved stereo f32 at 48 kHz. Audio is cut into 100 ms windows
//! (4800 frames = 9600 values); RMS is taken over both channels combined.
//! A marker fires when 3 consecutive windows (300 ms) are at or above the
//! threshold; the returned time is the start of the FIRST window of the run.
//!
//! Cooldown: 5 s measured from the returned (spike start) time. Windows that
//! start before `fire + 5 s` are ignored and break any run, so a run that spans
//! the end of cooldown fires only if it has a full 300 ms above threshold
//! starting at/after the cooldown end.
use std::time::{Duration, SystemTime};

const SAMPLE_RATE: usize = 48_000;
const CHANNELS: usize = 2;
const WINDOW_MS: u64 = 100;
const WINDOW_VALUES: usize = SAMPLE_RATE * CHANNELS * WINDOW_MS as usize / 1000;
const HOLD_WINDOWS: u32 = 3;
const RESYNC_TOLERANCE: Duration = Duration::from_millis(50);
const COOLDOWN: Duration = Duration::from_secs(5);

/// RMS level of `samples` (all channels combined) in dBFS.
/// Empty or all-zero input returns `f32::NEG_INFINITY`.
pub fn rms_dbfs(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return f32::NEG_INFINITY;
    }
    let sum: f64 = samples.iter().map(|&x| (x as f64) * (x as f64)).sum();
    let rms = (sum / samples.len() as f64).sqrt();
    (20.0 * rms.log10()) as f32
}

pub struct LaughDetector {
    threshold_dbfs: f32,
    buf: Vec<f32>,
    /// Wall time of the first value in `buf` (or of the next expected sample
    /// when `buf` is empty); valid once `started`.
    buf_start: SystemTime,
    started: bool,
    run_len: u32,
    run_start: SystemTime,
    cooldown_until: Option<SystemTime>,
    /// Expected start of the next window fed through [`Self::feed_window`].
    next_window: Option<SystemTime>,
}

impl LaughDetector {
    pub fn new(threshold_dbfs: f32) -> Self {
        Self {
            threshold_dbfs,
            buf: Vec::with_capacity(WINDOW_VALUES * 2),
            buf_start: SystemTime::UNIX_EPOCH,
            started: false,
            run_len: 0,
            run_start: SystemTime::UNIX_EPOCH,
            cooldown_until: None,
            next_window: None,
        }
    }

    pub fn set_threshold(&mut self, threshold_dbfs: f32) {
        self.threshold_dbfs = threshold_dbfs;
    }

    /// Feed one pre-computed 100 ms window: its RMS level in dBFS and the wall
    /// time of its first sample. Same hold / cooldown rules as [`Self::feed`];
    /// a window starting more than 50 ms away from where the previous one
    /// ended (stream pause, dropped windows) restarts the hold.
    pub fn feed_window(&mut self, rms_dbfs: f32, window_start: SystemTime) -> Option<SystemTime> {
        if let Some(expected) = self.next_window {
            let skew = window_start.duration_since(expected).unwrap_or_else(|e| e.duration());
            if skew > RESYNC_TOLERANCE {
                self.run_len = 0;
            }
        }
        self.next_window = Some(window_start + Duration::from_millis(WINDOW_MS));
        self.on_window(window_start, rms_dbfs >= self.threshold_dbfs)
    }

    /// Hold / cooldown state machine over consecutive windows.
    fn on_window(&mut self, start: SystemTime, loud: bool) -> Option<SystemTime> {
        if self.cooldown_until.is_some_and(|c| start < c) || !loud {
            self.run_len = 0;
            return None;
        }
        if self.run_len == 0 {
            self.run_start = start;
        }
        self.run_len += 1;
        if self.run_len == HOLD_WINDOWS {
            self.cooldown_until = Some(self.run_start + COOLDOWN);
            return Some(self.run_start);
        }
        None
    }

    /// Feed an arbitrary-length chunk; `at` is the wall time of its first sample.
    /// Returns the spike start time when a marker should fire.
    ///
    /// Clock resync: window times are derived from sample counts, but `at` is
    /// checked against the expected end of the samples already seen. If it
    /// differs by more than 50 ms (`RESYNC_TOLERANCE`; stream pause or clock
    /// drift), the buffered partial window is dropped, the current run is reset
    /// and the clock is re-based to `at`. Returned times therefore stay within
    /// about 50 ms of the wall clock instead of accumulating drift. A capture
    /// pause longer than 50 ms in the middle of a laugh restarts the 300 ms hold.
    pub fn feed(&mut self, samples: &[f32], at: SystemTime) -> Option<SystemTime> {
        if !self.started {
            self.started = true;
            self.buf_start = at;
        } else {
            let frames = (self.buf.len() / CHANNELS) as u64;
            let expected = self.buf_start + Duration::from_nanos(frames * 1_000_000_000 / SAMPLE_RATE as u64);
            let skew = at.duration_since(expected).unwrap_or_else(|e| e.duration());
            if skew > RESYNC_TOLERANCE {
                self.buf.clear();
                self.run_len = 0;
                self.buf_start = at;
            }
        }
        self.buf.extend_from_slice(samples);

        let mut fired = None;
        let mut consumed = 0;
        while self.buf.len() - consumed >= WINDOW_VALUES {
            let win = &self.buf[consumed..consumed + WINDOW_VALUES];
            let start = self.buf_start;
            let loud = rms_dbfs(win) >= self.threshold_dbfs;
            consumed += WINDOW_VALUES;
            self.buf_start += Duration::from_millis(WINDOW_MS);
            if let Some(t) = self.on_window(start, loud) {
                fired.get_or_insert(t);
            }
        }
        self.buf.drain(..consumed);
        fired
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const WIN: usize = 9600; // 100 ms stereo f32 values

    fn loud(ms: usize) -> Vec<f32> {
        vec![0.5; WIN * ms / 100] // -6 dBFS
    }
    fn quiet(ms: usize) -> Vec<f32> {
        vec![0.001; WIN * ms / 100] // -60 dBFS
    }
    fn t0() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1000)
    }

    #[test]
    fn fires_after_300ms_above() {
        let mut d = LaughDetector::new(-20.0);
        let start = t0() + Duration::from_millis(200);
        assert_eq!(d.feed(&quiet(200), t0()), None);
        assert_eq!(d.feed(&loud(200), start), None);
        assert_eq!(d.feed(&loud(100), start + Duration::from_millis(200)), Some(start));
    }

    #[test]
    fn fires_with_unaligned_chunks() {
        let mut d = LaughDetector::new(-20.0);
        let all = loud(500);
        let mut got = None;
        let mut off = 0;
        for n in [1000usize, 4321, 17, 20000, 100000] {
            let end = (off + n).min(all.len());
            let at = t0() + Duration::from_micros((off as u64 / 2) * 1_000_000 / 48_000);
            if let Some(r) = d.feed(&all[off..end], at) {
                got = Some(r);
            }
            off = end;
        }
        assert_eq!(got, Some(t0()));
    }

    #[test]
    fn no_fire_for_200ms_burst() {
        let mut d = LaughDetector::new(-20.0);
        assert_eq!(d.feed(&loud(200), t0()), None);
        assert_eq!(d.feed(&quiet(500), t0() + Duration::from_millis(200)), None);
    }

    #[test]
    fn cooldown_5s() {
        let mut d = LaughDetector::new(-20.0);
        assert_eq!(d.feed(&loud(300), t0()), Some(t0()));
        // continuous loudness; every window starting before t0+5s is ignored
        assert_eq!(d.feed(&loud(4700), t0() + Duration::from_millis(300)), None);
        assert_eq!(d.feed(&loud(200), t0() + Duration::from_millis(5000)), None);
        // third window starting at/after t0+5s completes the run: fire time is exactly t0+5s
        assert_eq!(
            d.feed(&loud(100), t0() + Duration::from_millis(5200)),
            Some(t0() + Duration::from_millis(5000))
        );
    }

    #[test]
    fn resyncs_after_gap() {
        let mut d = LaughDetector::new(-20.0);
        assert_eq!(d.feed(&loud(250), t0()), None); // leaves a 50 ms partial window
        let after = t0() + Duration::from_millis(250 + 2000);
        assert_eq!(d.feed(&loud(300), after), Some(after));
    }

    #[test]
    fn no_drift_with_odd_packet_sizes() {
        // Wall clock runs 200 ppm faster than the sample clock: over 10 min the
        // timestamps drift ~120 ms from the sample-count clock.
        let mut d = LaughDetector::new(-20.0);
        let at = |frames: u64| {
            let ns = (frames as f64 * 1e9 / 48_000.0 * 1.0002) as u64;
            t0() + Duration::from_nanos(ns)
        };
        // One continuous stream in 441-frame packets; the burst starts mid-packet
        // on a sample-clock window boundary, so the buffer is never empty there.
        let burst_frame: u64 = 600 * 48_000;
        let end_frame = burst_frame + 19_200;
        let burst_wall = at(burst_frame);
        let mut frames = 0u64;
        let mut fired = None;
        while frames < end_frame {
            // First packet is 1 frame so packet boundaries (1 + 441k) never hit a
            // 4800-frame window boundary: the buffer is never empty, so only the
            // resync logic (not an incidental reset) can absorb the drift.
            let n = if frames == 0 { 1 } else { 441.min(end_frame - frames) };
            let pkt: Vec<f32> = (frames..frames + n)
                .flat_map(|f| {
                    let v = if f >= burst_frame { 0.5 } else { 0.001 };
                    [v, v]
                })
                .collect();
            if let Some(t) = d.feed(&pkt, at(frames)) {
                assert!(fired.is_none());
                fired = Some(t);
            }
            frames += n;
        }
        let t = fired.expect("should fire");
        let diff = t.duration_since(burst_wall).unwrap_or_else(|e| e.duration());
        assert!(diff <= Duration::from_millis(50), "diff {diff:?}");
    }

    #[test]
    fn set_threshold_applies() {
        let mut d = LaughDetector::new(-3.0);
        assert_eq!(d.feed(&loud(300), t0()), None);
        d.set_threshold(-10.0);
        let at = t0() + Duration::from_millis(300);
        assert_eq!(d.feed(&loud(300), at), Some(at));
    }

    fn feed_windows(d: &mut LaughDetector, start: SystemTime, levels: &[f32]) -> Vec<SystemTime> {
        levels
            .iter()
            .enumerate()
            .filter_map(|(i, &l)| d.feed_window(l, start + Duration::from_millis(100 * i as u64)))
            .collect()
    }

    #[test]
    fn feed_window_fires_after_3_windows() {
        let mut d = LaughDetector::new(-20.0);
        let got = feed_windows(&mut d, t0(), &[-60.0, -10.0, -20.0, -15.0, -10.0]);
        assert_eq!(got, vec![t0() + Duration::from_millis(100)], "threshold is inclusive");
        let mut d = LaughDetector::new(-20.0);
        assert!(feed_windows(&mut d, t0(), &[-10.0, -10.0, -30.0, -10.0, -10.0]).is_empty());
    }

    #[test]
    fn feed_window_cooldown_5s() {
        let mut d = LaughDetector::new(-20.0);
        // 6 s of continuous loudness: fires at t0, then again at exactly t0 + 5 s.
        let got = feed_windows(&mut d, t0(), &[-6.0; 60]);
        assert_eq!(got, vec![t0(), t0() + Duration::from_secs(5)]);
    }

    #[test]
    fn feed_window_gap_restarts_hold() {
        let mut d = LaughDetector::new(-20.0);
        assert_eq!(d.feed_window(-6.0, t0()), None);
        assert_eq!(d.feed_window(-6.0, t0() + Duration::from_millis(100)), None);
        // 1 s pause in the stream: the run starts over.
        let after = t0() + Duration::from_millis(1200);
        assert_eq!(d.feed_window(-6.0, after), None);
        assert_eq!(d.feed_window(-6.0, after + Duration::from_millis(100)), None);
        assert_eq!(d.feed_window(-6.0, after + Duration::from_millis(200)), Some(after));
    }

    #[test]
    fn rms_of_full_scale_sine_is_minus_3db() {
        let s: Vec<f32> = (0..9600)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * ((i / 2) as f32) / 48000.0).sin())
            .collect();
        assert!((rms_dbfs(&s) - -3.01).abs() < 0.1);
        assert_eq!(rms_dbfs(&[0.0; 100]), f32::NEG_INFINITY);
    }
}
