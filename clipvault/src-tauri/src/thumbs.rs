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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumbnail_errors_when_ffmpeg_missing() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("t.jpg");
        let r = generate_thumbnail(&dir.path().join("no-ffmpeg.exe"), &dir.path().join("v.mp4"), &out, 1.0);
        assert!(r.is_err());
        assert!(!out.exists());
    }
}
