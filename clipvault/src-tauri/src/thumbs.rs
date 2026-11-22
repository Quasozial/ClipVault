//! Clip thumbnails via FFmpeg.

use anyhow::{anyhow, Result};
use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

/// A thumbnail of a local file never legitimately takes this long.
const THUMB_TIMEOUT: Duration = Duration::from_secs(30);

/// Writes a 480 px wide JPEG of the frame at `at_s` seconds of `video` to `out`.
/// Blocking; call from `spawn_blocking` in async code.
pub fn generate_thumbnail(ffmpeg: &Path, video: &Path, out: &Path, at_s: f64) -> Result<()> {
    let mut args: Vec<OsString> =
        ["-y", "-hide_banner", "-loglevel", "error", "-ss", &format!("{:.3}", at_s.max(0.0)), "-i"].map(OsString::from).into();
    args.push(video.into());
    args.extend(["-frames:v", "1", "-vf", "scale=480:-2", "-q:v", "3", "-c:v", "mjpeg", "-f", "image2"].map(OsString::from));
    args.push(out.into());
    if let Err(e) = crate::ffmpeg_proc::run_ok(ffmpeg, &args, THUMB_TIMEOUT, "thumbnail") {
        let _ = std::fs::remove_file(out);
        return Err(e);
    }
    match std::fs::metadata(out) {
        Ok(m) if m.len() > 0 => Ok(()),
        _ => Err(anyhow!("FFmpeg produced no thumbnail at {}", out.display())),
    }
}

/// Frames in a hover-scrub strip.
pub const STRIP_FRAMES: u32 = 8;
const MIN_SPAN_S: f64 = 0.1;

/// `(start, span)` in seconds the scrub strip covers: the trim range if set,
/// else the whole clip. The span is never below 0.1 s, so the frame rate below
/// stays finite for empty or inverted ranges.
pub fn strip_window(duration_s: f64, trim_in: Option<f64>, trim_out: Option<f64>) -> (f64, f64) {
    let start = trim_in.unwrap_or(0.0).max(0.0);
    let end = trim_out.unwrap_or(duration_s);
    (start, (end - start).max(MIN_SPAN_S))
}

/// FFmpeg arguments for a strip of [`STRIP_FRAMES`] frames evenly spread over
/// `[start_s, start_s + span_s]`, 320 px wide each, tiled side by side.
pub fn strip_args(video: &Path, out: &Path, start_s: f64, span_s: f64) -> Vec<OsString> {
    let span = span_s.max(MIN_SPAN_S);
    let vf = format!("fps={:.6},scale=320:-2,tile={STRIP_FRAMES}x1", STRIP_FRAMES as f64 / span);
    let mut args: Vec<OsString> =
        ["-y", "-hide_banner", "-loglevel", "error", "-ss", &format!("{:.3}", start_s.max(0.0)), "-t", &format!("{span:.3}"), "-i"]
            .map(OsString::from)
            .into();
    args.push(video.into());
    args.extend(["-vf", &vf, "-frames:v", "1", "-q:v", "4", "-c:v", "mjpeg", "-f", "image2"].map(OsString::from));
    args.push(out.into());
    args
}

/// Writes the scrub strip JPEG for `video` to `out`. Blocking.
pub fn generate_strip(ffmpeg: &Path, video: &Path, out: &Path, start_s: f64, span_s: f64) -> Result<()> {
    if let Err(e) = crate::ffmpeg_proc::run_ok(ffmpeg, &strip_args(video, out, start_s, span_s), THUMB_TIMEOUT, "scrub strip") {
        let _ = std::fs::remove_file(out);
        return Err(e);
    }
    match std::fs::metadata(out) {
        Ok(m) if m.len() > 0 => Ok(()),
        _ => Err(anyhow!("FFmpeg produced no scrub strip at {}", out.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_window_covers_clip_or_trim() {
        assert_eq!(strip_window(60.0, None, None), (0.0, 60.0));
        assert_eq!(strip_window(60.0, Some(10.0), Some(20.0)), (10.0, 10.0));
        assert_eq!(strip_window(60.0, Some(50.0), None), (50.0, 10.0));
    }

    #[test]
    fn strip_window_never_empty() {
        assert!(strip_window(0.0, None, None).1 >= 0.1);
        assert!(strip_window(60.0, Some(5.0), Some(5.0)).1 >= 0.1);
        assert!(strip_window(60.0, Some(30.0), Some(10.0)).1 >= 0.1);
    }

    #[test]
    fn strip_args_tile_eight_frames_once() {
        let a: Vec<String> = strip_args(Path::new("v.mp4"), Path::new("o.jpg"), 0.0, 0.1)
            .into_iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        let vf = &a[a.iter().position(|x| x == "-vf").unwrap() + 1];
        assert!(vf.contains("tile=8x1"), "{vf}");
        assert!(vf.contains("fps=80"), "{vf}");
        assert!(!vf.contains("inf") && !vf.contains("NaN"), "{vf}");
        assert!(a.windows(2).any(|w| w[0] == "-frames:v" && w[1] == "1"), "{a:?}");
        assert_eq!(a.last().unwrap(), "o.jpg");
    }

    #[test]
    fn strip_errors_when_ffmpeg_missing() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("s.jpg");
        assert!(generate_strip(&dir.path().join("no-ffmpeg.exe"), &dir.path().join("v.mp4"), &out, 0.0, 10.0).is_err());
        assert!(!out.exists());
    }

    #[test]
    fn thumbnail_errors_when_ffmpeg_missing() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("t.jpg");
        let r = generate_thumbnail(&dir.path().join("no-ffmpeg.exe"), &dir.path().join("v.mp4"), &out, 1.0);
        assert!(r.is_err());
        assert!(!out.exists());
    }
}
