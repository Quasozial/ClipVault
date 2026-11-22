//! Per-track audio for the clip editor: every audio stream of a clip is copied
//! into its own `.m4a` (so the webview can play and mix them separately — a
//! `<video>` only ever plays the first track) and reduced to a peak envelope
//! for the waveform. Results are cached next to the thumbnails.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Peak buckets per second of audio.
pub const PEAKS_PER_SEC: f64 = 50.0;
/// Sample rate the envelope is computed from.
const ENVELOPE_RATE: u32 = 8_000;
/// Cache format version; bump when the files' layout changes.
const CACHE_VERSION: u32 = 1;
const EXTRACT_TIMEOUT: Duration = Duration::from_secs(120);
/// Total size of cached per-track audio; the least recently opened clips'
/// tracks are evicted beyond it (they are re-extracted when opened again).
pub const CACHE_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

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

/// Runs FFmpeg (see [`crate::ffmpeg_proc`]); a probe's non-zero exit is fine.
fn run(ffmpeg: &Path, args: &[OsString], what: &str) -> Result<String> {
    use crate::ffmpeg_proc::{run, Priority, Run};
    match run(ffmpeg, args, EXTRACT_TIMEOUT, Priority::Normal, &|| false, |_, _| {}) {
        Run::Finished { success, stderr } if success || what == "probe" => Ok(stderr),
        Run::Finished { stderr, .. } => {
            let last = stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
            Err(anyhow!("FFmpeg {what} failed: {last}"))
        }
        _ => Err(anyhow!("{what} timed out")),
    }
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
                // Marks the clip as recently used for eviction.
                if let Ok(f) = std::fs::File::options().write(true).open(&meta_path) {
                    let _ = f.set_modified(std::time::SystemTime::now());
                }
                return Ok(m.tracks);
            }
        }
    }
    std::fs::create_dir_all(cache_dir).with_context(|| format!("cannot create {}", cache_dir.display()))?;

    let probe: Vec<OsString> = vec!["-hide_banner".into(), "-i".into(), video.into()];
    let titles = parse_audio_streams(&run(ffmpeg, &probe, "probe")?);
    if titles.is_empty() {
        return Ok(Vec::new());
    }

    // One pass: per track an m4a (stream copy) and a mono 8 kHz raw envelope source.
    let m4a = |n: usize| cache_dir.join(format!("{safe}.a{n}.m4a"));
    let raw = |n: usize| cache_dir.join(format!("{safe}.a{n}.f32"));
    let mut args: Vec<OsString> = ["-y", "-hide_banner", "-loglevel", "error", "-i"].map(OsString::from).into();
    args.push(video.into());
    for n in 0..titles.len() {
        args.extend(["-map", &format!("0:a:{n}"), "-vn", "-c:a", "copy", "-f", "mp4"].map(OsString::from));
        args.push(m4a(n).into());
        args.extend(["-map", &format!("0:a:{n}"), "-vn", "-ac", "1", "-ar", &ENVELOPE_RATE.to_string(), "-f", "f32le"].map(OsString::from));
        args.push(raw(n).into());
    }
    run(ffmpeg, &args, "audio extraction")?;

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
    let evicted = evict(cache_dir, &safe, CACHE_MAX_BYTES);
    if evicted > 0 {
        tracing::info!("audio cache: evicted tracks of {evicted} clip(s)");
    }
    Ok(tracks)
}

/// Clip key of a cache file: `{safe}.audio.json` or `{safe}.a{n}.m4a` / `.f32`.
fn cache_key(name: &str) -> Option<&str> {
    if let Some(k) = name.strip_suffix(".audio.json") {
        return Some(k);
    }
    let stem = name.strip_suffix(".m4a").or_else(|| name.strip_suffix(".f32"))?;
    let (k, n) = stem.rsplit_once(".a")?;
    (!n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())).then_some(k)
}

/// Deletes whole clips' cached audio, least recently used first (newest file
/// time of the clip's cache files), until the total is at most `max_bytes`.
/// Never touches `keep` (the clip just opened) or non-cache files such as
/// thumbnails. Returns how many clips were evicted.
pub fn evict(cache_dir: &Path, keep: &str, max_bytes: u64) -> usize {
    use std::collections::HashMap;
    let Ok(rd) = std::fs::read_dir(cache_dir) else { return 0 };
    // key -> (bytes, last used, files)
    let mut groups: HashMap<String, (u64, std::time::SystemTime, Vec<PathBuf>)> = HashMap::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(key) = cache_key(&name) else { continue };
        let Ok(m) = e.metadata() else { continue };
        if !m.is_file() {
            continue;
        }
        let used = m.modified().unwrap_or(std::time::UNIX_EPOCH);
        let g = groups.entry(key.to_string()).or_insert((0, std::time::UNIX_EPOCH, Vec::new()));
        g.0 += m.len();
        g.1 = g.1.max(used);
        g.2.push(e.path());
    }
    let mut total: u64 = groups.values().map(|g| g.0).sum();
    let mut order: Vec<(String, u64, std::time::SystemTime, Vec<PathBuf>)> =
        groups.into_iter().filter(|(k, _)| k != keep).map(|(k, (b, t, f))| (k, b, t, f)).collect();
    order.sort_by_key(|g| g.2);
    let mut evicted = 0;
    for (_, bytes, _, files) in order {
        if total <= max_bytes {
            break;
        }
        for f in files {
            let _ = std::fs::remove_file(f);
        }
        total = total.saturating_sub(bytes);
        evicted += 1;
    }
    evicted
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
    fn cache_keys_cover_only_audio_cache_files() {
        assert_eq!(cache_key("abc-1.audio.json"), Some("abc-1"));
        assert_eq!(cache_key("abc-1.a0.m4a"), Some("abc-1"));
        assert_eq!(cache_key("abc-1.a12.f32"), Some("abc-1"));
        assert_eq!(cache_key("abc-1.jpg"), None);
        assert_eq!(cache_key("abc-1.a.m4a"), None);
        assert_eq!(cache_key("abc-1.ax.m4a"), None);
    }

    /// Over the cap, whole clips go, least recently used first; the clip just
    /// opened and thumbnails are never removed.
    #[test]
    fn eviction_drops_least_recently_used_clips() {
        let dir = tempfile::tempdir().unwrap();
        let t0 = std::time::SystemTime::now() - Duration::from_secs(1000);
        let put = |name: &str, len: usize, age: u64| {
            let p = dir.path().join(name);
            std::fs::write(&p, vec![0u8; len]).unwrap();
            std::fs::File::options().write(true).open(&p).unwrap().set_modified(t0 + Duration::from_secs(age)).unwrap();
            p
        };
        let old = [put("old.a0.m4a", 400, 1), put("old.audio.json", 10, 1)];
        let mid = [put("mid.a0.m4a", 400, 2), put("mid.a1.m4a", 100, 2), put("mid.audio.json", 10, 2)];
        let new = [put("new.a0.m4a", 400, 3), put("new.audio.json", 10, 3)];
        let cur = [put("cur.a0.m4a", 400, 0), put("cur.audio.json", 10, 0)];
        let thumb = put("old.jpg", 5000, 0);
        // Total audio 1740; cap 1000 => evict old (410) and mid (510).
        assert_eq!(evict(dir.path(), "cur", 1000), 2);
        assert!(old.iter().chain(&mid).all(|p| !p.exists()));
        assert!(new.iter().chain(&cur).all(|p| p.exists()));
        assert!(thumb.exists());
        assert_eq!(evict(dir.path(), "cur", 1000), 0);
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
