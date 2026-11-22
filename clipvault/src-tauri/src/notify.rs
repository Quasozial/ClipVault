//! User feedback: toasts, sounds and UI events.
//!
//! Everything the core wants to tell the user goes through the [`Ui`] trait so
//! the core runs (and is tested end to end) without Tauri. The Tauri
//! implementation lives in `app::tauri_ui`.
//!
//! Every save request ends in exactly one of [`saved`], [`error`] or
//! [`not_recording`]: a toast plus a sound ("not recording" uses the error
//! sound), so a press is never silent. The hotkey path first shows
//! [`saving`] at once, so a slow save is not mistaken for a missed press.
use crate::capture::supervisor::Health;
use crate::db::clips::Clip;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sound {
    /// Two short rising tones.
    Saved,
    /// One low tone.
    Error,
    /// A short tick (manual marker).
    Mark,
}

/// Look of the on-screen banner (`overlay`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BannerKind {
    /// "Saving clip…": the press was heard; replaced by the result.
    Saving,
    Saved,
    Error,
}

pub trait Ui: Send + Sync {
    fn toast(&self, title: &str, body: &str);
    /// On-screen banner over the game ("Clip saved"); a no-op outside the app.
    fn banner(&self, _kind: BannerKind, _title: &str, _detail: &str) {}
    fn play(&self, sound: Sound);
    /// Health of the capture pipeline plus the current target label
    /// (`Rocket League`, `Desktop`, or `None` when idle).
    fn health_changed(&self, health: &Health, target: Option<&str>);
    fn clip_saved(&self, clip: &Clip);
    /// Mic level in dBFS, at most 10 Hz, only while the Settings page is open.
    fn mic_level(&self, dbfs: f32);
    /// Desktop capture was toggled (tray checkbox).
    fn desktop_capture_changed(&self, on: bool);
    /// The stored settings changed (any source: Settings page, tray / hotkey
    /// desktop toggle, Moments import), so an open Settings page can refresh.
    fn settings_changed(&self) {}
}

pub const APP_TITLE: &str = "ClipVault";

/// Banner detail line: "58 s · Rocket League" + " · started late" / " · contains gap".
pub fn saved_detail(clip: &Clip, gap: bool, short: bool) -> String {
    let mut t = format!("{} s · {}", clip.duration_s.round() as u64, clip.game);
    if short {
        t.push_str(" · started late");
    }
    if gap {
        t.push_str(" · contains gap");
    }
    t
}

/// "Clip saved (58 s, Rocket League)" + " – started late" / " – contains gap".
pub fn saved_text(clip: &Clip, gap: bool, short: bool) -> String {
    let mut t = format!("Clip saved ({} s, {})", clip.duration_s.round() as u64, clip.game);
    if short {
        t.push_str(" – started late");
    }
    if gap {
        t.push_str(" – contains gap");
    }
    t
}

/// Instant acknowledgement of a save press (before the save itself runs):
/// banner plus tick, no toast (the result brings one). `queued`: another save
/// is still running and this one starts after it.
pub fn saving(ui: &dyn Ui, game: &str, queued: bool) {
    let detail = if queued { format!("{game} · after the previous save") } else { game.to_string() };
    ui.banner(BannerKind::Saving, "Saving clip…", &detail);
    ui.play(Sound::Mark);
}

pub fn saved(ui: &dyn Ui, clip: &Clip, gap: bool, short: bool) {
    ui.toast(APP_TITLE, &saved_text(clip, gap, short));
    ui.banner(BannerKind::Saved, "Clip saved", &saved_detail(clip, gap, short));
    ui.play(Sound::Saved);
    ui.clip_saved(clip);
}

pub fn error(ui: &dyn Ui, reason: &str) {
    ui.toast(APP_TITLE, &format!("Clip NOT saved: {reason}"));
    ui.banner(BannerKind::Error, "Clip NOT saved", reason);
    ui.play(Sound::Error);
}

pub fn not_recording(ui: &dyn Ui) {
    ui.toast(APP_TITLE, "Not recording – nothing to save");
    ui.banner(BannerKind::Error, "Not recording", "Nothing to save");
    ui.play(Sound::Error);
}

/// Mono 16-bit PCM WAV (22.05 kHz) of a sequence of sine tones
/// `(frequency Hz, duration ms)`, each with a 5 ms fade in/out to avoid clicks.
pub fn tone_wav(tones: &[(f32, u32)], volume: f32) -> Vec<u8> {
    const RATE: u32 = 22_050;
    let mut pcm: Vec<i16> = Vec::new();
    for &(freq, ms) in tones {
        let n = (RATE * ms / 1000) as usize;
        let fade = (RATE as usize * 5 / 1000).min(n / 2).max(1);
        for i in 0..n {
            let env = (i.min(n - 1 - i) as f32 / fade as f32).min(1.0);
            let v = (2.0 * std::f32::consts::PI * freq * i as f32 / RATE as f32).sin() * env * volume;
            pcm.push((v * i16::MAX as f32) as i16);
        }
    }
    let data_len = (pcm.len() * 2) as u32;
    let mut w = Vec::with_capacity(44 + data_len as usize);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data_len).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes()); // PCM
    w.extend_from_slice(&1u16.to_le_bytes()); // mono
    w.extend_from_slice(&RATE.to_le_bytes());
    w.extend_from_slice(&(RATE * 2).to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        w.extend_from_slice(&s.to_le_bytes());
    }
    w
}

/// One struck note of a [`chime_wav`]: starts at `start_ms`, rings out with an
/// exponential decay of time constant `decay_ms`.
#[derive(Debug, Clone, Copy)]
pub struct Note {
    pub freq: f32,
    pub start_ms: u32,
    pub decay_ms: f32,
    pub gain: f32,
}

/// Mono 16-bit PCM WAV (44.1 kHz) of soft bell-like notes: a 6 ms attack, an
/// exponential decay, a quiet octave partial for warmth and a 20 ms fade at the
/// end, so nothing clicks or sounds like a bare test tone.
pub fn chime_wav(notes: &[Note], total_ms: u32, volume: f32) -> Vec<u8> {
    const RATE: u32 = 44_100;
    let n = (RATE * total_ms / 1000) as usize;
    let mut mix = vec![0f32; n];
    let attack = RATE as f32 * 0.006;
    for note in notes {
        let start = (RATE * note.start_ms / 1000) as usize;
        let decay = RATE as f32 * note.decay_ms / 1000.0;
        for (i, slot) in mix.iter_mut().enumerate().skip(start) {
            let k = (i - start) as f32;
            let env = (k / attack).min(1.0) * (-k / decay).exp();
            if env < 1e-4 && k > attack {
                break;
            }
            let ph = 2.0 * std::f32::consts::PI * note.freq * k / RATE as f32;
            // The octave partial decays twice as fast, like a struck bar.
            let v = ph.sin() + 0.18 * (2.0 * ph).sin() * (-k / (decay * 0.5)).exp();
            *slot += v * env * note.gain;
        }
    }
    let fade = (RATE as usize * 20 / 1000).min(n);
    let peak = mix.iter().fold(0f32, |m, v| m.max(v.abs())).max(1e-6);
    let pcm: Vec<i16> = mix
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let tail = ((n - i) as f32 / fade as f32).min(1.0);
            (v / peak * volume * tail * i16::MAX as f32) as i16
        })
        .collect();
    wav_pcm16(RATE, &pcm)
}

fn wav_pcm16(rate: u32, pcm: &[i16]) -> Vec<u8> {
    let data_len = (pcm.len() * 2) as u32;
    let mut w = Vec::with_capacity(44 + data_len as usize);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data_len).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes()); // PCM
    w.extend_from_slice(&1u16.to_le_bytes()); // mono
    w.extend_from_slice(&rate.to_le_bytes());
    w.extend_from_slice(&(rate * 2).to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        w.extend_from_slice(&s.to_le_bytes());
    }
    w
}

fn sound_wav(sound: Sound) -> &'static [u8] {
    use std::sync::OnceLock;
    static SAVED: OnceLock<Vec<u8>> = OnceLock::new();
    static ERROR: OnceLock<Vec<u8>> = OnceLock::new();
    static MARK: OnceLock<Vec<u8>> = OnceLock::new();
    match sound {
        // Soft rising major third (G5 -> B5): "got it".
        Sound::Saved => SAVED.get_or_init(|| {
            let n = |freq, start_ms, gain| Note { freq, start_ms, decay_ms: 140.0, gain };
            chime_wav(&[n(783.99, 0, 0.8), n(987.77, 75, 1.0)], 520, 0.22)
        }),
        // Low falling minor third (E4 -> C#4): clearly different, not alarming.
        Sound::Error => ERROR.get_or_init(|| {
            let n = |freq, start_ms| Note { freq, start_ms, decay_ms: 160.0, gain: 1.0 };
            chime_wav(&[n(329.63, 0), n(277.18, 140)], 650, 0.3)
        }),
        // Single short tick.
        Sound::Mark => MARK.get_or_init(|| {
            chime_wav(&[Note { freq: 1174.66, start_ms: 0, decay_ms: 45.0, gain: 1.0 }], 220, 0.18)
        }),
    }
}

/// Plays `sound` asynchronously (returns at once). The WAV buffers are static,
/// so they outlive the asynchronous playback `SND_MEMORY` requires.
pub fn play_sound(sound: Sound) {
    #[cfg(windows)]
    {
        use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};
        let wav = sound_wav(sound);
        // SAFETY: with SND_MEMORY the first argument points at the WAV image,
        // which is 'static.
        let ok = unsafe {
            PlaySoundW(windows::core::PCWSTR(wav.as_ptr() as *const u16), None, SND_MEMORY | SND_ASYNC | SND_NODEFAULT)
        };
        if !ok.as_bool() {
            tracing::warn!("PlaySound failed for {sound:?}");
        }
    }
    #[cfg(not(windows))]
    let _ = sound_wav(sound);
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Local;

    fn clip(duration_s: f64, game: &str) -> Clip {
        Clip {
            id: "x".into(),
            title: "t".into(),
            description: String::new(),
            path: "C:/c.mp4".into(),
            game: game.into(),
            recorded_at: Local::now().fixed_offset(),
            duration_s,
            fps: 60,
            width: 1920,
            height: 1080,
            size: 1,
            trim_in: None,
            trim_out: None,
            thumbnail_path: None,
            source: "native".into(),
            source_id: None,
            tags: vec![],
            starred: false,
            compressed: false,
            original_size: None,
            markers: vec![],
        }
    }

    #[test]
    fn saved_text_variants() {
        let c = clip(58.4, "Rocket League");
        assert_eq!(saved_text(&c, false, false), "Clip saved (58 s, Rocket League)");
        assert_eq!(saved_text(&c, false, true), "Clip saved (58 s, Rocket League) – started late");
        assert_eq!(saved_text(&c, true, false), "Clip saved (58 s, Rocket League) – contains gap");
        assert_eq!(
            saved_text(&clip(12.6, "Desktop"), true, true),
            "Clip saved (13 s, Desktop) – started late – contains gap"
        );
        assert_eq!(saved_detail(&c, false, false), "58 s · Rocket League");
        assert_eq!(saved_detail(&clip(12.6, "Desktop"), true, true), "13 s · Desktop · started late · contains gap");
    }

    #[test]
    fn chime_wav_is_valid_soft_pcm() {
        let w = chime_wav(&[Note { freq: 784.0, start_ms: 0, decay_ms: 140.0, gain: 1.0 }], 500, 0.25);
        assert_eq!(&w[0..4], b"RIFF");
        let riff_len = u32::from_le_bytes(w[4..8].try_into().unwrap()) as usize;
        assert_eq!(riff_len + 8, w.len());
        let s: Vec<i16> = w[44..].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        assert_eq!(s.len(), 44_100 / 2);
        // Starts and ends silent (no clicks), peak at the requested volume.
        assert!(s[0].abs() < 50 && s[s.len() - 1].abs() < 50);
        let peak = s.iter().map(|v| v.unsigned_abs()).max().unwrap() as f32 / i16::MAX as f32;
        assert!((peak - 0.25).abs() < 0.01, "{peak}");
    }

    #[test]
    fn tone_wav_is_valid_pcm() {
        let w = tone_wav(&[(880.0, 60), (1320.0, 70)], 0.5);
        assert_eq!(&w[0..4], b"RIFF");
        assert_eq!(&w[8..16], b"WAVEfmt ");
        let riff_len = u32::from_le_bytes(w[4..8].try_into().unwrap()) as usize;
        assert_eq!(riff_len + 8, w.len());
        let data_len = u32::from_le_bytes(w[40..44].try_into().unwrap()) as usize;
        // 130 ms at 22.05 kHz, 2 bytes per sample.
        assert_eq!(data_len, (22_050 * 60 / 1000 + 22_050 * 70 / 1000) * 2);
        // Faded: starts at (near) zero, reaches a real amplitude.
        let samples: Vec<i16> = w[44..].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        assert!(samples[0].abs() < 100);
        assert!(samples.iter().map(|s| s.unsigned_abs()).max().unwrap() > 10_000);
    }
}
