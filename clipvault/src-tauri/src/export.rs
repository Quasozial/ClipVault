//! "Export for sharing": re-encodes a clip (its trim range, if set) into one
//! H.264 MP4 with a single mixed audio track built from the chosen tracks at
//! the chosen volumes, optionally under a size cap (Discord). The result goes
//! to `<capture dir>/Exports` and is not added to the library. The source file
//! is only read.

use crate::db::clips::Clip;
use crate::settings::Encoder;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// One audio track to keep. Muted tracks are simply left out.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct ExportTrack {
    /// Audio stream index (0 = first audio stream), as in `AudioTrack::index`.
    pub index: usize,
    /// Linear gain, 0..=2 (1 = unchanged).
    pub volume: f32,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ExportOptions {
    /// File name suffix, e.g. `discord`. Letters, digits, `-` and `_` only.
    pub label: String,
    /// Size cap in MB (1 MB = 1 000 000 bytes); `None` = quality-based.
    pub max_mb: Option<f64>,
    /// Output height (keeps aspect); `None` or taller than the source = source height.
    pub height: Option<u32>,
    pub tracks: Vec<ExportTrack>,
}

/// Bytes per MB for the size cap. Decimal on purpose: smaller than MiB, so a
/// file under N "MB" passes either reading of the limit.
const MB: f64 = 1_000_000.0;
/// Share of the cap the streams may use; the rest is MP4 overhead and encoder overshoot.
const SIZE_MARGIN: f64 = 0.92;
const MIN_VIDEO_KBPS: f64 = 300.0;

/// Audio bitrate for an export: lower when the size budget is tight.
pub fn audio_kbps(has_audio: bool, max_mb: Option<f64>) -> u32 {
    match (has_audio, max_mb) {
        (false, _) => 0,
        (true, Some(mb)) if mb <= 10.0 => 96,
        (true, _) => 160,
    }
}

/// Video bitrate (kbit/s) that keeps `duration_s` of video + audio under
/// `max_mb` times `scale` (scale < 1 is used for a retry after overshooting).
/// Errors when the clip is too long for the cap, saying how long it may be.
pub fn video_kbps_for_cap(max_mb: f64, duration_s: f64, audio_kbps: u32, scale: f64) -> Result<u32> {
    let total_kbit = max_mb * MB * 8.0 / 1000.0 * SIZE_MARGIN * scale;
    let kbps = total_kbit / duration_s - audio_kbps as f64;
    if kbps < MIN_VIDEO_KBPS {
        let max_s = total_kbit / (MIN_VIDEO_KBPS + audio_kbps as f64);
        bail!(
            "{:.0} s is too long for {max_mb} MB. Trim it to about {:.0} s or less, or pick a bigger preset.",
            duration_s,
            max_s.floor()
        );
    }
    Ok(kbps.floor() as u32)
}

/// `-filter_complex` graph mixing the chosen tracks into `[aout]`; `None` without tracks.
pub fn audio_filter(tracks: &[ExportTrack]) -> Option<String> {
    if tracks.is_empty() {
        return None;
    }
    let mut g = String::new();
    for (i, t) in tracks.iter().enumerate() {
        g.push_str(&format!("[0:a:{}]volume={:.3}[a{i}];", t.index, t.volume.clamp(0.0, 2.0)));
    }
    for i in 0..tracks.len() {
        g.push_str(&format!("[a{i}]"));
    }
    // normalize=0 keeps each track at the volume picked (amix would otherwise
    // divide by the input count); the limiter catches the summed peaks.
    g.push_str(&format!(
        "amix=inputs={}:duration=longest:normalize=0,alimiter=limit=0.97:level=false[aout]",
        tracks.len()
    ));
    Some(g)
}

/// H.264 encoder on the same vendor as the capture encoder (Discord and
/// phones play H.264 everywhere; HEVC/AV1 often not). `Auto` is not resolved
/// here: see [`export_encoder`].
pub fn h264_codec(enc: Encoder) -> &'static str {
    match enc {
        Encoder::H264Nvenc | Encoder::HevcNvenc | Encoder::Av1Nvenc => "h264_nvenc",
        Encoder::H264Amf | Encoder::HevcAmf | Encoder::Av1Amf => "h264_amf",
        Encoder::H264Qsv | Encoder::HevcQsv | Encoder::Av1Qsv => "h264_qsv",
        Encoder::X264 | Encoder::Auto => "libx264",
    }
}

/// The settings' encoder, with `Auto` replaced by the best H.264 encoder this
/// PC has (probed once per run).
pub fn export_encoder(ffmpeg: &Path, wanted: Encoder) -> Encoder {
    if wanted != Encoder::Auto {
        return wanted;
    }
    static PICK: std::sync::OnceLock<Encoder> = std::sync::OnceLock::new();
    *PICK.get_or_init(|| {
        let avail = crate::capture::encoders::probe_all(ffmpeg);
        [Encoder::H264Nvenc, Encoder::H264Amf, Encoder::H264Qsv]
            .into_iter()
            .find(|e| avail.contains(e))
            .unwrap_or(Encoder::X264)
    })
}

/// Video rate control arguments: a bitrate (capped exports) or constant quality.
fn video_rc(enc: Encoder, video_kbps: Option<u32>) -> Vec<String> {
    let codec = h264_codec(enc);
    let mut a: Vec<String> = vec!["-c:v".into(), codec.into()];
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    match (codec, video_kbps) {
        ("h264_nvenc", Some(k)) => {
            let (b, buf) = (format!("{k}k"), format!("{}k", k * 2));
            a.extend(s(&["-preset", "p6", "-rc", "vbr", "-multipass", "fullres", "-b:v", &b, "-maxrate", &b, "-bufsize", &buf]));
        }
        ("h264_nvenc", None) => a.extend(s(&["-preset", "p6", "-rc", "vbr", "-cq", "19", "-b:v", "0"])),
        ("libx264", None) => a.extend(s(&["-preset", "medium", "-crf", "19"])),
        (_, Some(k)) => {
            let (b, buf) = (format!("{k}k"), format!("{}k", k * 2));
            a.extend(s(&["-b:v", &b, "-maxrate", &b, "-bufsize", &buf]));
        }
        (_, None) => a.extend(s(&["-b:v", "20000k"])),
    }
    a.extend(s(&["-pix_fmt", "yuv420p"]));
    a
}

/// FFmpeg arguments (without the binary) for one export attempt into `out`.
#[allow(clippy::too_many_arguments)]
pub fn export_args(
    src: &Path,
    out: &Path,
    t_in: f64,
    t_out: f64,
    src_height: u32,
    opts: &ExportOptions,
    enc: Encoder,
    video_kbps: Option<u32>,
) -> Vec<OsString> {
    let mut a: Vec<OsString> = Vec::new();
    let mut push = |v: &[&str]| a.extend(v.iter().map(OsString::from));
    push(&["-y", "-hide_banner", "-loglevel", "error"]);
    push(&["-ss", &format!("{t_in:.3}"), "-to", &format!("{t_out:.3}"), "-i"]);
    a.push(src.into());
    let mut push = |v: &[&str]| a.extend(v.iter().map(OsString::from));
    push(&["-map", "0:v:0"]);
    if let Some(h) = opts.height.filter(|h| *h < src_height) {
        push(&["-vf", &format!("scale=-2:{h}:flags=lanczos")]);
    }
    match audio_filter(&opts.tracks) {
        Some(g) => {
            let ak = audio_kbps(true, opts.max_mb);
            push(&["-filter_complex", &g, "-map", "[aout]", "-c:a", "aac", "-b:a", &format!("{ak}k"), "-ac", "2"]);
        }
        None => push(&["-an"]),
    }
    let rc = video_rc(enc, video_kbps);
    a.extend(rc.iter().map(OsString::from));
    let mut push = |v: &[&str]| a.extend(v.iter().map(OsString::from));
    push(&["-movflags", "+faststart", "-f", "mp4"]);
    a.push(out.into());
    a
}

fn safe_label(label: &str) -> String {
    let s: String = label.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    if s.is_empty() { "export".into() } else { s }
}

/// Runs the export and returns the new file's path. Blocking.
pub fn export_clip(ffmpeg: &Path, clip: &Clip, capture_dir: &Path, enc: Encoder, opts: &ExportOptions) -> Result<PathBuf> {
    let enc = export_encoder(ffmpeg, enc);
    if !clip.path.is_file() {
        bail!("The clip file no longer exists: {}", clip.path.display());
    }
    let t_in = clip.trim_in.unwrap_or(0.0).max(0.0);
    let t_out = clip.trim_out.unwrap_or(clip.duration_s).min(clip.duration_s);
    if !(t_out > t_in) {
        bail!("The trim start must be before the trim end");
    }
    let duration = t_out - t_in;
    let has_audio = !opts.tracks.is_empty();

    let dir = capture_dir.join("Exports");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let stem = clip.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "clip".into());
    let final_path = crate::save::unique_clip_path(&dir, &format!("{stem}__{}.mp4", safe_label(&opts.label)));
    let mut tmp = final_path.clone().into_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);

    let result = (|| -> Result<()> {
        // A capped export that still overshoots is retried once at a lower bitrate.
        for scale in [1.0, 0.85] {
            let vk = match opts.max_mb {
                Some(mb) => Some(video_kbps_for_cap(mb, duration, audio_kbps(has_audio, Some(mb)), scale)?),
                None => None,
            };
            let args = export_args(&clip.path, &tmp, t_in, t_out, clip.height, opts, enc, vk);
            crate::library::run_ffmpeg(ffmpeg, &args)?;
            let size = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
            match opts.max_mb {
                Some(mb) if size as f64 > mb * MB => {
                    tracing::warn!("[export] {size} bytes is over {mb} MB at scale {scale}");
                    continue;
                }
                _ => return Ok(()),
            }
        }
        bail!("The export came out bigger than {} MB. Trim the clip a bit shorter and try again.", opts.max_mb.unwrap_or(0.0))
    })()
    .and_then(|()| std::fs::rename(&tmp, &final_path).context("moving the exported file into place"));
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(final_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn joined(a: &[OsString]) -> String {
        a.iter().map(|s| s.to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ")
    }

    fn opts(max_mb: Option<f64>, height: Option<u32>, tracks: Vec<ExportTrack>) -> ExportOptions {
        ExportOptions { label: "discord".into(), max_mb, height, tracks }
    }

    #[test]
    fn bitrate_fits_cap() {
        // 30 s under 10 MB with 96 k audio: 10e6*8/1000*0.92/30 - 96 = 2357
        assert_eq!(video_kbps_for_cap(10.0, 30.0, 96, 1.0).unwrap(), 2357);
        let lower = video_kbps_for_cap(10.0, 30.0, 96, 0.85).unwrap();
        assert!(lower < 2357);
    }

    #[test]
    fn too_long_for_cap_says_how_long() {
        let e = video_kbps_for_cap(10.0, 600.0, 96, 1.0).unwrap_err().to_string();
        assert!(e.contains("too long for 10 MB"), "{e}");
        assert!(e.contains("Trim it to about 185 s"), "{e}");
    }

    #[test]
    fn audio_graph_mixes_chosen_tracks_at_their_volume() {
        let g = audio_filter(&[ExportTrack { index: 0, volume: 1.0 }, ExportTrack { index: 2, volume: 0.5 }]).unwrap();
        assert_eq!(
            g,
            "[0:a:0]volume=1.000[a0];[0:a:2]volume=0.500[a1];[a0][a1]amix=inputs=2:duration=longest:normalize=0,alimiter=limit=0.97:level=false[aout]"
        );
        assert!(audio_filter(&[]).is_none());
        let g = audio_filter(&[ExportTrack { index: 1, volume: 9.0 }]).unwrap();
        assert!(g.starts_with("[0:a:1]volume=2.000[a0];"), "{g}");
    }

    #[test]
    fn capped_args_scale_and_rate() {
        let a = joined(&export_args(
            Path::new("in.mp4"),
            Path::new("out.tmp"),
            2.0,
            32.0,
            1080,
            &opts(Some(10.0), Some(720), vec![ExportTrack { index: 0, volume: 1.0 }]),
            Encoder::H264Nvenc,
            Some(2357),
        ));
        assert!(a.contains("-ss 2.000 -to 32.000 -i in.mp4"), "{a}");
        assert!(a.contains("-map 0:v:0 -vf scale=-2:720:flags=lanczos"), "{a}");
        assert!(a.contains("-map [aout] -c:a aac -b:a 96k -ac 2"), "{a}");
        assert!(a.contains("-c:v h264_nvenc -preset p6 -rc vbr -multipass fullres -b:v 2357k -maxrate 2357k -bufsize 4714k"), "{a}");
        assert!(a.ends_with("-movflags +faststart -f mp4 out.tmp"), "{a}");
    }

    #[test]
    fn uncapped_no_audio_keeps_height() {
        let a = joined(&export_args(
            Path::new("in.mp4"),
            Path::new("out.tmp"),
            0.0,
            10.0,
            1080,
            &opts(None, Some(1440), vec![]),
            Encoder::HevcNvenc,
            None,
        ));
        assert!(!a.contains("scale="), "{a}");
        assert!(a.contains("-an"), "{a}");
        assert!(a.contains("-c:v h264_nvenc -preset p6 -rc vbr -cq 19 -b:v 0"), "{a}");
    }

    #[test]
    fn export_stays_h264_on_same_vendor() {
        assert_eq!(h264_codec(Encoder::Av1Nvenc), "h264_nvenc");
        assert_eq!(h264_codec(Encoder::HevcAmf), "h264_amf");
        assert_eq!(h264_codec(Encoder::Av1Qsv), "h264_qsv");
        assert_eq!(h264_codec(Encoder::X264), "libx264");
    }

    #[test]
    fn labels_are_file_safe() {
        assert_eq!(safe_label("discord-10"), "discord-10");
        assert_eq!(safe_label("../x y"), "xy");
        assert_eq!(safe_label("??"), "export");
    }
}
