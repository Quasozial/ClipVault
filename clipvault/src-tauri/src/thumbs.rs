//! Clip thumbnails via FFmpeg.

use anyhow::{anyhow, Context, Result};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A thumbnail of a local file never legitimately takes this long.
const THUMB_TIMEOUT: Duration = Duration::from_secs(30);
/// `CREATE_NO_WINDOW`: no console window flashes up for FFmpeg.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Writes a 480 px wide JPEG of the frame at `at_s` seconds of `video` to `out`.
/// Blocking; call from `spawn_blocking` in async code.
pub fn generate_thumbnail(ffmpeg: &Path, video: &Path, out: &Path, at_s: f64) -> Result<()> {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-y", "-hide_banner", "-loglevel", "error", "-ss", &format!("{:.3}", at_s.max(0.0)), "-i"])
        .arg(video)
        .args(["-frames:v", "1", "-vf", "scale=480:-2", "-q:v", "3", "-c:v", "mjpeg", "-f", "image2"])
        .arg(out)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // Discarded: piping without a reader could block FFmpeg; callers only log failures.
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn().with_context(|| format!("could not run {}", ffmpeg.display()))?;
    let started = Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if started.elapsed() > THUMB_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(out);
            return Err(anyhow!("thumbnail timed out after {} s", THUMB_TIMEOUT.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    if !status.success() {
        let _ = std::fs::remove_file(out);
        return Err(anyhow!("FFmpeg thumbnail failed ({status})"));
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
