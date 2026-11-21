//! Per-track audio for the clip editor: every audio stream of a clip is copied
//! into its own `.m4a` (so the webview can play and mix them separately — a
//! `<video>` only ever plays the first track) and reduced to a peak envelope
//! for the waveform. Results are cached next to the thumbnails.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Peak buckets per second of audio.
pub const PEAKS_PER_SEC: f64 = 50.0;
/// Sample rate the envelope is computed from.
const ENVELOPE_RATE: u32 = 8_000;
/// Cache format version; bump when the files' layout changes.
const CACHE_VERSION: u32 = 1;
const EXTRACT_TIMEOUT: Duration = Duration::from_secs(120);
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioTrack {
    /// Audio stream index (0 = first audio stream).
    pub index: usize,
    /// Track title as stored in the file (`Game`, `Mic`, `Discord`, ...).
    pub title: String,
    /// What the editor shows (`Desktop audio`, `Microphone`, ...).
    pub label: String,
    /// `game` | `mic` | `voice` | `other`: picks the row colour.
    pub kind: String,
    /// The track alone as an `.m4a`, playable through the asset protocol.
    pub path: String,
    /// Peak level (0..1) per 1/[`PEAKS_PER_SEC`] s.
    pub peaks: Vec<f32>,
    pub peaks_per_sec: f64,
}

#[derive(Serialize, Deserialize)]
struct CacheMeta {
    version: u32,
    source_len: u64,
    source_mtime: u64,
    tracks: Vec<AudioTrack>,
}

/// Audio stream titles of `ffmpeg -i` stderr, in stream order. A stream with no
/// usable title gets an empty string.
pub fn parse_audio_streams(stderr: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut in_audio = false;
    for line in stderr.lines() {
        let t = line.trim();
        if t.starts_with("Stream #") {
            in_audio = t.contains(": Audio:");
            if in_audio {
                out.push(String::new());
            }
            continue;
        }
        if !in_audio {
            continue;
        }
        if let Some((k, v)) = t.split_once(':') {
            let (k, v) = (k.trim(), v.trim());
            let generic = v.is_empty() || v == "SoundHandler" || v.starts_with("Core Media") || v.starts_with("ISO Media");
            let slot = out.last_mut().unwrap();
            // `title`/`name` win over `handler_name`.
            match k {
                "title" | "name" if !generic => *slot = v.to_string(),
                "handler_name" if !generic && slot.is_empty() => *slot = v.to_string(),
                _ => {}
            }
        }
    }
    out
}

/// (label, kind) for a stored track title; `count` = number of audio tracks.
pub fn classify(title: &str, index: usize, count: usize) -> (String, &'static str) {
    let l = title.to_ascii_lowercase();
    if l.contains("mic") {
        ("Microphone".into(), "mic")
    } else if l.contains("discord") || l.contains("voice") || l.contains("chat") {
        ("Voice chat".into(), "voice")
    } else if l.contains("game") || l.contains("system") || l.contains("desktop") {
        ("Desktop audio".into(), "game")
    } else if title.is_empty() && count == 2 {
        // Moments-style pair without titles: output device first, mic second.
        if index == 0 {
            ("Desktop audio".into(), "game")
        } else {
            ("Microphone".into(), "mic")
        }
    } else if title.is_empty() {
        (format!("Audio {}", index + 1), "other")
    } else {
        (title.to_string(), "other")
    }
}

/// Max-abs envelope of mono f32 `samples` at `rate` Hz, `per_sec` buckets per second.
pub fn envelope(samples: &[f32], rate: u32, per_sec: f64) -> Vec<f32> {
    let bucket = ((rate as f64 / per_sec).round() as usize).max(1);
    samples
        .chunks(bucket)
        .map(|c| c.iter().fold(0f32, |m, s| m.max(s.abs())).min(1.0))
        .map(|v| (v * 1000.0).round() / 1000.0)
        .collect()
}

fn display_order(kind: &str) -> u8 {
    match kind {
        "game" => 0,
        "voice" => 1,
        "other" => 2,
        _ => 3,
    }
}

fn hide(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

fn run(mut cmd: Command, what: &str) -> Result<String> {
    hide(&mut cmd);
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("could not run FFmpeg for {what}"))?;
    let mut err = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut err, &mut s);
        s
    });
    let started = Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if started.elapsed() > EXTRACT_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow!("{what} timed out"));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stderr = reader.join().unwrap_or_default();
    if !status.success() && what != "probe" {
        let last = stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
        return Err(anyhow!("FFmpeg {what} failed ({status}): {last}"));
    }
    Ok(stderr)
}

fn stamp(p: &Path) -> (u64, u64) {
    let m = std::fs::metadata(p).ok();
    let len = m.as_ref().map(|m| m.len()).unwrap_or(0);
    let mtime = m
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    (len, mtime)
}

/// Audio tracks of `video` (clip `id`), extracting them into `cache_dir` on first use.
/// Blocking; call from `spawn_blocking`.
pub fn audio_tracks(ffmpeg: &Path, video: &Path, cache_dir: &Path, id: &str) -> Result<Vec<AudioTrack>> {
    if !video.is_file() {
        return Err(anyhow!("clip file not found: {}", video.display()));
    }
    let safe: String = id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    let meta_path = cache_dir.join(format!("{safe}.audio.json"));
    let (source_len, source_mtime) = stamp(video);
    if let Ok(bytes) = std::fs::read(&meta_path) {
        if let Ok(m) = serde_json::from_slice::<CacheMeta>(&bytes) {
            if m.version == CACHE_VERSION
                && m.source_len == source_len
                && m.source_mtime == source_mtime
                && m.tracks.iter().all(|t| Path::new(&t.path).is_file())
            {
                return Ok(m.tracks);
            }
        }
    }
    std::fs::create_dir_all(cache_dir).with_context(|| format!("cannot create {}", cache_dir.display()))?;

    let mut probe = Command::new(ffmpeg);
    probe.args(["-hide_banner", "-i"]).arg(video);
    let titles = parse_audio_streams(&run(probe, "probe")?);
    if titles.is_empty() {
        return Ok(Vec::new());
    }

    // One pass: per track an m4a (stream copy) and a mono 8 kHz raw envelope source.
    let m4a = |n: usize| cache_dir.join(format!("{safe}.a{n}.m4a"));
    let raw = |n: usize| cache_dir.join(format!("{safe}.a{n}.f32"));
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-y", "-hide_banner", "-loglevel", "error", "-i"]).arg(video);
    for n in 0..titles.len() {
        cmd.args(["-map", &format!("0:a:{n}"), "-vn", "-c:a", "copy", "-f", "mp4"]).arg(m4a(n));
        cmd.args(["-map", &format!("0:a:{n}"), "-vn", "-ac", "1", "-ar", &ENVELOPE_RATE.to_string(), "-f", "f32le"])
            .arg(raw(n));
    }
    run(cmd, "audio extraction")?;

    let mut tracks = Vec::with_capacity(titles.len());
    for (n, title) in titles.iter().enumerate() {
        let bytes = std::fs::read(raw(n)).with_context(|| format!("reading {}", raw(n).display()))?;
        let _ = std::fs::remove_file(raw(n));
        let samples: Vec<f32> =
            bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
        let (label, kind) = classify(title, n, titles.len());
        tracks.push(AudioTrack {
            index: n,
            title: title.clone(),
            label,
            kind: kind.to_string(),
            path: m4a(n).to_string_lossy().into_owned(),
            peaks: envelope(&samples, ENVELOPE_RATE, PEAKS_PER_SEC),
            peaks_per_sec: PEAKS_PER_SEC,
        });
    }
    tracks.sort_by_key(|t| (display_order(&t.kind), t.index));

    let meta = CacheMeta { version: CACHE_VERSION, source_len, source_mtime, tracks: tracks.clone() };
    if let Err(e) = std::fs::write(&meta_path, serde_json::to_vec(&meta)?) {
        tracing::warn!("audio cache: writing {}: {e}", meta_path.display());
    }
    Ok(tracks)
}

/// Removes the cached audio of clip `id` (after the clip is deleted).
pub fn forget(cache_dir: &Path, id: &str) {
    let safe: String = id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    let Ok(rd) = std::fs::read_dir(cache_dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with(&format!("{safe}.a")) || name == format!("{safe}.audio.json") {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Folder for the per-track audio cache.
pub fn cache_dir(thumbs_dir: &Path) -> PathBuf {
    thumbs_dir.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOMENTS: &str = "  Stream #0:0[0x1](und): Video: h264 (Main), yuv420p, 1920x1080, 60 fps (default)
    Metadata:
      handler_name    : VideoHandler
  Stream #0:1[0x2](und): Audio: aac (LC), 48000 Hz, stereo, fltp, 192 kb/s (default)
    Metadata:
      handler_name    : Game
  Stream #0:2[0x3](und): Audio: aac (LC), 48000 Hz, stereo, fltp, 192 kb/s
    Metadata:
      handler_name    : Mic
";

    #[test]
    fn parses_moments_handler_names() {
        assert_eq!(parse_audio_streams(MOMENTS), ["Game", "Mic"]);
    }

    #[test]
    fn title_beats_handler_and_generic_handler_is_ignored() {
        let s = "  Stream #0:1: Audio: aac
    Metadata:
      handler_name    : SoundHandler
      name            : Discord
  Stream #0:2: Audio: aac
    Metadata:
      handler_name    : SoundHandler
";
        assert_eq!(parse_audio_streams(s), ["Discord", ""]);
    }

    #[test]
    fn classifies_titles() {
        assert_eq!(classify("Game", 0, 2), ("Desktop audio".to_string(), "game"));
        assert_eq!(classify("Game/System", 2, 3), ("Desktop audio".to_string(), "game"));
        assert_eq!(classify("Mic", 1, 2), ("Microphone".to_string(), "mic"));
        assert_eq!(classify("Discord", 1, 3), ("Voice chat".to_string(), "voice"));
        assert_eq!(classify("", 1, 2), ("Microphone".to_string(), "mic"));
        assert_eq!(classify("", 0, 1), ("Audio 1".to_string(), "other"));
    }

    #[test]
    fn envelope_takes_bucket_max_abs() {
        let mut s = vec![0.0f32; 320];
        s[10] = -0.8;
        s[200] = 0.3;
        let e = envelope(&s, 8000, 50.0); // 160 samples per bucket
        assert_eq!(e, vec![0.8, 0.3]);
    }
}
