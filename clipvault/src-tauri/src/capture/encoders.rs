//! Which video encoders this PC can run, and which one recording uses.
//!
//! FFmpeg lists every encoder it was built with, whether or not the GPU or
//! driver exists, so each one is tried: a tiny test encode with the real
//! recording options. Done once at startup (the probes run in parallel and
//! take about a second; a missing GPU fails almost instantly).
use super::args::video_codec_args;
use crate::settings::Encoder;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// `CREATE_NO_WINDOW`: no console window flashes up for FFmpeg.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A probe that has not finished by then counts as failed (a hung driver).
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// FFmpeg arguments for one probe: 10 frames of 640x360 (above every
/// encoder's minimum size) in the recording's pixel format, thrown away.
pub fn probe_args(encoder: Encoder) -> Vec<String> {
    let mut a: Vec<String> = ["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", "color=black:s=640x360:r=30", "-frames:v", "10", "-vf", "format=nv12"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    a.extend(video_codec_args(encoder, 30, 4000));
    a.extend(["-f", "null", "-"].iter().map(|s| s.to_string()));
    a
}

fn probe_one(ffmpeg: &Path, encoder: Encoder) -> bool {
    run_probe(ffmpeg, &probe_args(encoder), &format!("encoder {encoder:?}"))
}

/// FFmpeg arguments for the HDR probe: FP16 frames through the HDR output
/// filter (Vulkan upload + libplacebo), thrown away.
pub fn hdr_probe_args() -> Vec<String> {
    let mut a: Vec<String> = super::args::HDR_HW_DEVICE.iter().map(|s| s.to_string()).collect();
    a.extend(
        ["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", "color=gray:s=640x360:r=30,format=rgba64le", "-frames:v", "3", "-filter:v"]
            .iter()
            .map(|s| s.to_string()),
    );
    a.push(super::args::hdr_output_filter(320, 180, 3.0));
    a.extend(["-f", "null", "-"].iter().map(|s| s.to_string()));
    a
}

/// Whether the HDR capture path (Vulkan + libplacebo) runs on this PC.
pub fn probe_hdr_gpu(ffmpeg: &Path) -> bool {
    let ok = run_probe(ffmpeg, &hdr_probe_args(), "HDR conversion (Vulkan/libplacebo)");
    tracing::info!("HDR conversion on GPU available: {ok}");
    ok
}

fn run_probe(ffmpeg: &Path, args: &[String], label: &str) -> bool {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("{label} probe: cannot start FFmpeg: {e}");
            return false;
        }
    };
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    let mut err = String::new();
                    if let Some(mut s) = child.stderr.take() {
                        let _ = std::io::Read::read_to_string(&mut s, &mut err);
                    }
                    let first = err.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
                    tracing::info!("{label} not available: {first}");
                }
                return status.success();
            }
            Ok(None) if start.elapsed() > PROBE_TIMEOUT => {
                tracing::warn!("{label} probe timed out");
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(e) => {
                tracing::warn!("{label} probe: {e}");
                return false;
            }
        }
    }
}

/// The encoders that work on this PC, best first ([`Encoder::PREFERENCE`]).
/// Empty when FFmpeg cannot run at all.
pub fn probe_all(ffmpeg: &Path) -> Vec<Encoder> {
    let started = Instant::now();
    let results: Vec<(Encoder, bool)> = std::thread::scope(|s| {
        let handles: Vec<_> = Encoder::PREFERENCE.iter().map(|&e| (e, s.spawn(move || probe_one(ffmpeg, e)))).collect();
        handles.into_iter().map(|(e, h)| (e, h.join().unwrap_or(false))).collect()
    });
    let ok: Vec<Encoder> = results.into_iter().filter(|(_, ok)| *ok).map(|(e, _)| e).collect();
    tracing::info!("video encoders available ({} ms): {ok:?}", started.elapsed().as_millis());
    ok
}

/// What recording uses for the `wanted` setting, and a notice for the user when
/// it is not what they picked. `available` is [`probe_all`]'s result; when it
/// is empty (probing impossible) a fixed choice is trusted as it is.
pub fn resolve(wanted: Encoder, available: &[Encoder]) -> (Encoder, Option<String>) {
    if available.is_empty() {
        return (if wanted == Encoder::Auto { Encoder::X264 } else { wanted }, None);
    }
    if wanted != Encoder::Auto && available.contains(&wanted) {
        return (wanted, None);
    }
    let best = best_of(available);
    let notice = if wanted != Encoder::Auto {
        Some(format!("{} is not available on this PC – recording with {}", wanted.label(), best.label()))
    } else {
        None
    };
    (best, notice)
}

/// The first of [`Encoder::PREFERENCE`] in `available` (x264 if none).
pub fn best_of(available: &[Encoder]) -> Encoder {
    Encoder::PREFERENCE.into_iter().find(|e| available.contains(e)).unwrap_or(Encoder::X264)
}

/// Shown once when recording falls back to the CPU.
pub const CPU_NOTICE: &str =
    "No graphics card video encoder found – recording uses the CPU. If games stutter, lower the resolution or FPS.";

#[cfg(test)]
mod tests {
    use super::*;
    use Encoder::*;

    #[test]
    fn auto_picks_best_available() {
        assert_eq!(resolve(Auto, &[H264Nvenc, HevcNvenc, Av1Nvenc, X264]), (Av1Nvenc, None));
        // Older NVIDIA card (no AV1): HEVC.
        assert_eq!(resolve(Auto, &[HevcNvenc, H264Nvenc, X264]), (HevcNvenc, None));
        // AMD before RX 7000.
        assert_eq!(resolve(Auto, &[H264Amf, HevcAmf, X264]), (HevcAmf, None));
        // No usable GPU encoder.
        assert_eq!(resolve(Auto, &[X264]), (X264, None));
    }

    #[test]
    fn fixed_choice_kept_when_available() {
        assert_eq!(resolve(H264Nvenc, &[Av1Nvenc, H264Nvenc, X264]), (H264Nvenc, None));
        assert_eq!(resolve(X264, &[Av1Nvenc, X264]), (X264, None));
    }

    #[test]
    fn unavailable_choice_replaced_with_notice() {
        // Settings copied from an NVIDIA PC to an AMD one.
        let (e, n) = resolve(H264Nvenc, &[HevcAmf, H264Amf, X264]);
        assert_eq!(e, HevcAmf);
        assert_eq!(n.as_deref(), Some("NVIDIA H.264 is not available on this PC – recording with AMD HEVC"));
    }

    #[test]
    fn unknown_availability_trusts_the_setting() {
        assert_eq!(resolve(HevcNvenc, &[]), (HevcNvenc, None));
        assert_eq!(resolve(Auto, &[]), (X264, None));
    }

    #[test]
    fn probe_uses_recording_options() {
        let a = probe_args(Av1Nvenc).join(" ");
        assert!(a.contains("-i color=black:s=640x360:r=30 -frames:v 10 -vf format=nv12 -c:v av1_nvenc -preset p4 -tune hq -rc vbr"), "{a}");
        assert!(a.ends_with("-f null -"), "{a}");
    }

    #[test]
    fn missing_ffmpeg_finds_nothing() {
        assert!(probe_all(Path::new(r"C:\does\not\exist\ffmpeg.exe")).is_empty());
    }
}
