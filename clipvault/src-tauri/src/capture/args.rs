//! Pure FFmpeg argument builders. No process spawning here.
use crate::settings::Encoder;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoSource {
    Ddagrab,
    /// `testsrc2` lavfi source, used by integration tests.
    TestSrc,
}

/// Raw PCM (f32le, 48 kHz, 2 ch) delivered over a Windows named pipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioPipe {
    pub pipe_name: String,
    pub title: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioInput {
    Pipe(AudioPipe),
    /// `anullsrc` stand-in, treated exactly like a pipe input.
    Silence { title: &'static str },
}

#[derive(Debug, Clone)]
pub struct CaptureSpec {
    pub output_idx: u32,
    pub fps: u32,
    pub out_w: u32,
    pub out_h: u32,
    /// `Some(scale)` on an HDR desktop when the GPU path is usable: capture in
    /// FP16 scRGB and divide by `scale` (SDR white in scRGB units, e.g. 3.0 for
    /// 240 nits) so SDR content keeps its on-screen brightness. `None`: 8-bit path.
    pub hdr_sdr_white: Option<f32>,
    pub encoder: Encoder,
    pub bitrate_kbps: u32,
    pub audio_inputs: Vec<AudioInput>,
    pub segment_dir: PathBuf,
    pub segment_prefix: String,
    pub video_source: VideoSource,
    /// Wall-clock origin T0 (µs since the Unix epoch) shared by every input: all
    /// output timestamps are `capture wall time - T0`. Pick it just before
    /// spawning FFmpeg; the supervisor sets it per run.
    pub wallclock_origin_us: i64,
}

/// Video source graph for ddagrab. Verified on hardware in Task 4 (RTX 5090,
/// FFmpeg N-127043, three HDR-enabled monitors).
///
/// * SDR desktop: 8-bit BGRA (`output_fmt=8bit`), scaled on the CPU: `scale_d3d11`
///   fails to allocate its output pool on NVIDIA (CreateTexture2D E_INVALIDARG).
///   ~0.9 CPU core for 3840x2160@60 -> 1920x1080.
/// * HDR desktop: the 8-bit (and 10-bit) Desktop Duplication formats are
///   scRGB clamped at 1.0 = 80 nits, i.e. SDR content at the "SDR content
///   brightness" level (e.g. 240 nits = 3.0) arrives 3x too bright with
///   everything above a third clipped (measured 2026-11-22 against FP16). So
///   the HDR graph captures FP16 (`rgbaf16`) and only downloads it here; the
///   output filter ([`hdr_output_filter`]) uploads it to Vulkan where
///   libplacebo scales, divides by SDR white and encodes sRGB.
///   No GPU route from ddagrab's D3D11 frames exists in this FFmpeg (`hwmap` to
///   Vulkan/CUDA/OpenCL fails with ENOSYS) and swscale's FP16 path runs ~35 fps
///   at 4K on 5 cores, hence the download/upload round trip (~1.5 cores, ~50 fps
///   at 4K). Highlights above SDR white are clipped, not tone-mapped.
fn video_filter_graph(spec: &CaptureSpec) -> String {
    let (i, fps, w, h) = (spec.output_idx, spec.fps, spec.out_w, spec.out_h);
    if spec.hdr_sdr_white.is_some() {
        return format!(
            "ddagrab=output_idx={i}:framerate={fps}:draw_mouse=1:output_fmt=rgbaf16,{},hwdownload,format=rgbaf16le",
            origin_stamp(spec.wallclock_origin_us, "")
        );
    }
    format!(
        // The origin stamp comes straight after ddagrab (it only touches timestamps,
        // so D3D11 frames pass): time(0) for frame 0 must be read before the
        // GPU->CPU copy and CPU scale, not after.
        "ddagrab=output_idx={i}:framerate={fps}:draw_mouse=1:output_fmt=8bit,{},\
         hwdownload,format=bgra,\
         scale={w}:{h}:flags=fast_bilinear:out_color_matrix=bt709:out_range=tv,format=nv12,\
         {TAG_709}",
        origin_stamp(spec.wallclock_origin_us, "")
    )
}

/// Timing model: every output timestamp is `capture wall time - T0`, with T0 =
/// `spec.wallclock_origin_us` shared by all inputs, so tracks line up whatever
/// order or speed FFmpeg opens the inputs in. `-copyts` stops FFmpeg from
/// rebasing each input to its own start (which would give each track its own
/// zero). Note the mpegts muxer still adds its constant 1.4 s to every stream.
///
/// lavfi sources (ddagrab, testsrc2, anullsrc) keep their own regular
/// timestamps, shifted so that frame 0 sits at the wall time it was produced
/// (`time(0)` when setpts first runs, i.e. as the frame leaves the source);
/// stamping every frame with its read time would add per-frame jitter.
fn origin_stamp(t0_us: i64, prefix: &str) -> String {
    format!(
        "{prefix}settb=AVTB,{prefix}setpts='if(eq(N,0),st(0,(time(0)-{})/TB));PTS-STARTPTS+ld(0)'",
        origin_secs(t0_us)
    )
}

/// T0 in seconds with exact microseconds, e.g. `1795000000.012345`.
fn origin_secs(t0_us: i64) -> String {
    format!("{}.{:06}", t0_us.div_euclid(1_000_000), t0_us.rem_euclid(1_000_000))
}

/// The encoder copies primaries/transfer from the frames (ddagrab: sRGB transfer,
/// testsrc2: unspecified), ignoring `-color_primaries`/`-color_trc`, so they are
/// set on the frames here. Matrix and range come from the scale and output options.
const TAG_709: &str = "setparams=color_primaries=bt709:color_trc=bt709";

/// Global options for the HDR path: a Vulkan device for `hwupload`/libplacebo.
pub const HDR_HW_DEVICE: [&str; 4] = ["-init_hw_device", "vulkan=vk", "-filter_hw_device", "vk"];

/// Output video filter for the HDR path: linear scRGB in, BT.709 NV12 out.
/// libplacebo's `contrast` multiplies before linearization, which for linear
/// input is a plain scale: SDR white (`sdr_white` in scRGB) becomes 1.0.
/// Output is sRGB-encoded and tagged BT.709 like the SDR path.
pub fn hdr_output_filter(w: u32, h: u32, sdr_white: f32) -> String {
    format!(
        "setparams=color_primaries=bt709:color_trc=linear:colorspace=gbr:range=pc,hwupload,\
         libplacebo=w={w}:h={h}:format=nv12:colorspace=bt709:color_primaries=bt709:\
         color_trc=iec61966-2-1:range=tv:contrast={:.5},hwdownload,format=nv12,{TAG_709}",
        1.0 / sdr_white.max(1.0)
    )
}

/// Video encoder options for recording. Quality-based rate control capped at
/// `bitrate_kbps`: busy scenes get up to the cap, quiet ones (menus, desktop)
/// far less, unlike the constant bitrate used before. GOP 2 s (the segments).
///
/// `Auto` must be resolved first (`capture::encoders::resolve`); passed here it
/// falls back to x264, which runs everywhere.
pub fn video_codec_args(encoder: Encoder, fps: u32, bitrate_kbps: u32) -> Vec<String> {
    let max = format!("{bitrate_kbps}k");
    let buf = format!("{}k", bitrate_kbps.saturating_mul(2));
    let half = format!("{}k", bitrate_kbps / 2);
    let gop = (fps * 2).to_string();
    let name = encoder.ffmpeg_name().unwrap_or("libx264");
    let mut v: Vec<&str> = vec!["-c:v", name];
    match encoder {
        // NVENC (verified on RTX 5090, FFmpeg N-127043): high-quality tune with
        // B-frames and adaptive quantisation; the old low-latency tune turned
        // B-frames off, which only matters for streaming.
        Encoder::H264Nvenc | Encoder::HevcNvenc | Encoder::Av1Nvenc => {
            let cq = match encoder {
                Encoder::H264Nvenc => "23",
                Encoder::HevcNvenc => "25",
                _ => "32",
            };
            v.extend([
                "-preset", "p4", "-tune", "hq", "-rc", "vbr", "-cq", cq, "-b:v", "0",
                "-maxrate", &max, "-bufsize", &buf, "-bf", "2", "-spatial-aq", "1", "-temporal-aq", "1",
                "-forced-idr", "1",
            ]);
        }
        Encoder::H264Amf | Encoder::HevcAmf | Encoder::Av1Amf => {
            // AMF's own default quality level; untested on AMD hardware here.
            v.extend([
                "-usage", "transcoding", "-quality", "balanced", "-rc", "qvbr", "-qvbr_quality_level", "23",
                "-b:v", &half, "-maxrate", &max, "-bufsize", &buf,
            ]);
        }
        // ICQ for H.264/HEVC (quality 1-51); AV1's ICQ scale differs between
        // driver versions, so it gets capped VBR instead.
        Encoder::H264Qsv | Encoder::HevcQsv => {
            v.extend(["-preset", "veryfast", "-global_quality", if encoder == Encoder::H264Qsv { "23" } else { "25" }]);
        }
        Encoder::Av1Qsv => v.extend(["-preset", "veryfast", "-b:v", &half, "-maxrate", &max, "-bufsize", &buf]),
        Encoder::X264 | Encoder::Auto => {
            v.extend(["-preset", "veryfast", "-crf", "23", "-maxrate", &max, "-bufsize", &buf]);
        }
    }
    v.extend(["-g", &gop]);
    v.into_iter().map(String::from).collect()
}

/// Build the FFmpeg capture command line (excluding the executable).
/// No `-fps_mode` is set: ddagrab's `framerate` and testsrc2's `rate` already yield CFR input.
pub fn build_capture_args(spec: &CaptureSpec) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    let mut push = |items: &[&str]| a.extend(items.iter().map(|s| s.to_string()));

    push(&["-y", "-hide_banner", "-loglevel", "warning", "-copyts"]);
    let stamp_v = origin_stamp(spec.wallclock_origin_us, "");
    let stamp_a = origin_stamp(spec.wallclock_origin_us, "a");
    let t0 = origin_secs(spec.wallclock_origin_us);

    match spec.video_source {
        VideoSource::Ddagrab => {
            if spec.hdr_sdr_white.is_some() {
                push(&HDR_HW_DEVICE);
            }
            let g = video_filter_graph(spec);
            push(&["-f", "lavfi", "-i", &g]);
        }
        VideoSource::TestSrc => {
            let g = format!("testsrc2=size={}x{}:rate={},{stamp_v},{TAG_709}", spec.out_w, spec.out_h, spec.fps);
            // lavfi generators run faster than realtime; `-re` paces them to wall clock.
            push(&["-re", "-f", "lavfi", "-i", &g]);
        }
    }

    for input in &spec.audio_inputs {
        match input {
            AudioInput::Pipe(p) => {
                let path = format!(r"\\.\pipe\{}", p.pipe_name);
                // No input -thread_queue_size: this FFmpeg rejects it before an input.
                // Packets are stamped with absolute wall-clock read time (rebased by
                // the per-stream filter below).
                push(&[
                    "-use_wallclock_as_timestamps", "1",
                    "-f", "f32le", "-ar", "48000", "-ac", "2", "-i", &path,
                ]);
            }
            AudioInput::Silence { .. } => {
                let g = format!("anullsrc=r=48000:cl=stereo,{stamp_a}");
                push(&["-re", "-f", "lavfi", "-i", &g]);
            }
        }
    }

    push(&["-map", "0:v"]);
    for n in 0..spec.audio_inputs.len() {
        push(&["-map", &format!("{}:a", n + 1)]);
    }

    if spec.video_source == VideoSource::TestSrc {
        push(&["-pix_fmt", "yuv420p"]);
    } else if let Some(white) = spec.hdr_sdr_white {
        push(&["-filter:v", &hdr_output_filter(spec.out_w, spec.out_h, white)]);
    }
    a.extend(video_codec_args(spec.encoder, spec.fps, spec.bitrate_kbps));
    let mut push = |items: &[&str]| a.extend(items.iter().map(|s| s.to_string()));
    // Every 2 s counted from the first frame (the segment muxer's reference), not
    // from t = 0: with -copyts the first frame sits at its wall time - T0.
    push(&["-force_key_frames", "expr:if(isnan(prev_forced_t),1,gte(t+0.001,prev_forced_t+2))"]);
    // Explicit VUI tags: BT.709 limited range, matching the scale filter's conversion.
    push(&[
        "-colorspace", "bt709", "-color_primaries", "bt709",
        "-color_trc", "bt709", "-color_range", "tv",
    ]);

    for (n, input) in spec.audio_inputs.iter().enumerate() {
        // Pipe packets are stamped with the absolute wall-clock time at which the
        // demuxer finished reading them, i.e. their *last* sample (measured +80 ms
        // late without correction): subtract the packet duration, then T0, to put
        // them on the shared clock. Unrebased they would sit decades ahead of the
        // video and the muxer would stop reading the pipes.
        let (title, filter) = match input {
            AudioInput::Pipe(p) => (p.title, format!("asetpts=PTS-NB_SAMPLES/SR/TB-{t0}/TB,aresample=async=1000")),
            AudioInput::Silence { title } => (*title, "aresample=async=1000".to_string()),
        };
        push(&[
            &format!("-filter:a:{n}"), &filter,
            &format!("-metadata:s:a:{n}"), &format!("title={title}"),
        ]);
    }
    if !spec.audio_inputs.is_empty() {
        push(&["-c:a", "aac", "-b:a", "160k"]);
    }

    let dir = spec.segment_dir.display();
    let list = format!(r"{dir}\{}list.csv", spec.segment_prefix);
    let pattern = format!(r"{dir}\{}seg_%08d.ts", spec.segment_prefix);
    push(&[
        // The segment muxer cuts at the first keyframe >= its first video pts
        // (rounded to µs) + 2 n s, which can sit < 1 µs after a keyframe that is
        // exactly 2 n s on; without slack those cuts slip by 2 s (4 s segments).
        "-f", "segment", "-segment_time", "2", "-segment_time_delta", "0.001", "-segment_format", "mpegts",
        "-segment_list", &list, "-segment_list_type", "csv", "-segment_list_flags", "live",
        &pattern,
    ]);
    a
}

/// Build the concat-to-MP4 command line (excluding the executable). The caller
/// supplies `out` (typically `*.mp4.tmp`), hence the explicit `-f mp4`.
pub fn build_concat_args(list_file: &Path, out: &Path, track_titles: &[&str]) -> Vec<String> {
    let mut a: Vec<String> = ["-y", "-hide_banner", "-loglevel", "warning", "-f", "concat", "-safe", "0", "-i"]
        .iter().map(|s| s.to_string()).collect();
    a.push(list_file.display().to_string());
    a.extend(["-map", "0", "-c", "copy"].iter().map(|s| s.to_string()));
    // The mp4 muxer writes `title` as the track's udta `name` (ffprobe tag `name`);
    // most players show the `hdlr` `handler_name` instead, so set both.
    for (n, t) in track_titles.iter().enumerate() {
        a.push(format!("-metadata:s:a:{n}"));
        a.push(format!("title={t}"));
        a.push(format!("-metadata:s:a:{n}"));
        a.push(format!("handler_name={t}"));
    }
    a.extend(["-movflags", "+faststart", "-f", "mp4"].iter().map(|s| s.to_string()));
    a.push(out.display().to_string());
    a
}

/// Key of the stream layout a capture produces: segments with the same key can
/// be stream-copied into one clip, others cannot. Covers everything that
/// changes the encoded streams (size, rate, codec, audio track count, source);
/// not the monitor, bitrate, folder or timing. Only compared within one app
/// run, never stored.
pub fn layout_key(spec: &CaptureSpec) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (spec.fps, spec.out_w, spec.out_h, spec.audio_inputs.len()).hash(&mut h);
    format!("{:?}/{:?}", spec.encoder, spec.video_source).hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Starts whose streams can be stream-copied together share a key; any
    /// change to the encoded format gives a new one.
    #[test]
    fn layout_key_tracks_only_the_stream_format() {
        let base = layout_key(&spec());
        let mut same = spec();
        same.output_idx = 2;
        same.bitrate_kbps = 8000;
        same.segment_dir = PathBuf::from(r"D:\other");
        same.segment_prefix = "r9_".into();
        same.wallclock_origin_us = 1;
        assert_eq!(layout_key(&same), base, "monitor, bitrate, folder and timing do not change the format");
        let changes: [fn(&mut CaptureSpec); 5] = [
            |s| s.out_w = 1280,
            |s| s.fps = 30,
            |s| s.encoder = Encoder::HevcNvenc,
            |s| s.audio_inputs.push(AudioInput::Silence { title: "Mic" }),
            |s| s.video_source = VideoSource::TestSrc,
        ];
        for (i, change) in changes.iter().enumerate() {
            let mut s = spec();
            change(&mut s);
            assert_ne!(layout_key(&s), base, "change {i} must give a new layout");
        }
    }

    fn spec() -> CaptureSpec {
        CaptureSpec {
            output_idx: 1,
            fps: 60,
            out_w: 1920,
            out_h: 1080,
            hdr_sdr_white: None,
            encoder: Encoder::H264Nvenc,
            bitrate_kbps: 40000,
            audio_inputs: vec![],
            segment_dir: PathBuf::from(r"C:\buf"),
            segment_prefix: "r3_".into(),
            video_source: VideoSource::Ddagrab,
            wallclock_origin_us: T0_US,
        }
    }

    /// Shared origin used by `spec()`, and its rendering in seconds.
    const T0_US: i64 = 1_795_000_000_012_345;
    const T0_S: &str = "1795000000.012345";
    const STAMP_V: &str = "settb=AVTB,setpts='if(eq(N,0),st(0,(time(0)-1795000000.012345)/TB));PTS-STARTPTS+ld(0)'";
    const STAMP_A: &str = "asettb=AVTB,asetpts='if(eq(N,0),st(0,(time(0)-1795000000.012345)/TB));PTS-STARTPTS+ld(0)'";

    #[test]
    fn all_inputs_share_one_wallclock_origin() {
        // -copyts: FFmpeg must not rebase inputs individually (that would give
        // each track its own zero); every input is put on `wall time - T0` instead.
        for src in [VideoSource::Ddagrab, VideoSource::TestSrc] {
            let mut s = spec();
            s.video_source = src;
            s.audio_inputs = vec![
                AudioInput::Pipe(AudioPipe { pipe_name: "p_mic".into(), title: "Mic" }),
                AudioInput::Silence { title: "Discord" },
            ];
            let v = build_capture_args(&s);
            let a = joined(&v);
            assert_eq!(v.iter().filter(|x| *x == "-copyts").count(), 1, "{a}");
            // Video: stamped with the wall time of its first frame, then its own
            // regular timestamps (no per-frame wall-clock jitter). The stamp must
            // run as the frame leaves the source: for ddagrab before the GPU->CPU
            // copy and CPU scale, which would otherwise delay frame 0's time(0).
            let video_graph = &v[v.iter().position(|x| x == "-i").unwrap() + 1];
            let (source, rest) = video_graph.split_once(',').unwrap();
            assert!(rest.starts_with(STAMP_V), "{video_graph}");
            assert_eq!(video_graph.matches("setpts=").count(), 1, "{video_graph}");
            assert!(source.starts_with(if src == VideoSource::Ddagrab { "ddagrab=" } else { "testsrc2=" }));
            // Pipe: absolute wall-clock timestamps minus T0; no per-stream STARTPTS.
            assert!(a.contains(&format!("-filter:a:0 asetpts=PTS-NB_SAMPLES/SR/TB-{T0_S}/TB,aresample=async=1000")), "{a}");
            assert!(!a.contains("asetpts=PTS-STARTPTS,aresample"), "{a}");
            // Silence: stamped like the video.
            assert!(a.contains(&format!("-i anullsrc=r=48000:cl=stereo,{STAMP_A} ")), "{a}");
            assert!(a.contains("-filter:a:1 aresample=async=1000"), "{a}");
        }
    }

    #[test]
    fn keyframes_every_2s_from_first_frame() {
        // With -copyts the first frame sits at its wall time - T0, not 0. The
        // segment muxer counts its 2 s from the first packet, so keyframes must
        // too: absolute `gte(t,n_forced*2)` keyframes can land a hair before the
        // segment boundary, which then slips to the next keyframe (4 s segments;
        // seen in supervisor_it).
        let a = joined(&build_capture_args(&spec()));
        assert!(a.contains("-force_key_frames expr:if(isnan(prev_forced_t),1,gte(t+0.001,prev_forced_t+2))"), "{a}");
        assert!(!a.contains("n_forced*2"), "{a}");
        // The muxer's threshold is first_pts (rounded to µs) + 2 s, which can
        // exceed a keyframe exactly 2 s later by < 1 µs; 1 ms of slack.
        assert!(a.contains("-f segment -segment_time 2 -segment_time_delta 0.001 -segment_format mpegts"), "{a}");
    }

    #[test]
    fn origin_renders_microseconds_exactly() {
        let mut s = spec();
        s.wallclock_origin_us = 1_000_000_000_000_007;
        s.audio_inputs = pipes();
        assert!(joined(&build_capture_args(&s)).contains("-1000000000.000007/TB,aresample"));
    }

    fn joined(a: &[String]) -> String {
        a.join(" ")
    }

    fn pipes() -> Vec<AudioInput> {
        [("p_mic", "Mic"), ("p_dc", "Discord"), ("p_sys", "Game/System")]
            .iter()
            .map(|(n, t)| AudioInput::Pipe(AudioPipe { pipe_name: n.to_string(), title: t }))
            .collect()
    }

    #[test]
    fn sdr_args_contain_ddagrab_scale_nvenc() {
        let a = joined(&build_capture_args(&spec()));
        assert!(a.contains(&format!("-f lavfi -i ddagrab=output_idx=1:framerate=60:draw_mouse=1:output_fmt=8bit,{STAMP_V},{SCALE_1080} ")), "{a}");
        assert!(a.contains("-c:v h264_nvenc -preset p4 -tune hq -rc vbr -cq 23 -b:v 0 -maxrate 40000k -bufsize 80000k -bf 2 -spatial-aq 1 -temporal-aq 1 -forced-idr 1 -g 120 -force_key_frames expr:if(isnan(prev_forced_t),1,gte(t+0.001,prev_forced_t+2))"), "{a}");
        assert!(a.contains(COLOR_TAGS), "{a}");
        assert!(a.contains("-f segment -segment_time 2 -segment_time_delta 0.001 -segment_format mpegts"), "{a}");
        assert!(a.contains(r"-segment_list C:\buf\r3_list.csv -segment_list_type csv -segment_list_flags live C:\buf\r3_seg_%08d.ts"), "{a}");
    }

    const COLOR_TAGS: &str = "-colorspace bt709 -color_primaries bt709 -color_trc bt709 -color_range tv";

    const SCALE_1080: &str = "hwdownload,format=bgra,scale=1920:1080:flags=fast_bilinear:out_color_matrix=bt709:out_range=tv,format=nv12,setparams=color_primaries=bt709:color_trc=bt709";

    /// On an HDR desktop the 8-bit DDA format is scRGB clamped at 80 nits
    /// (overbright SDR content), so HDR captures FP16 and converts on Vulkan.
    #[test]
    fn hdr_args_capture_fp16_and_scale_by_sdr_white() {
        let mut s = spec();
        s.hdr_sdr_white = Some(3.0);
        let v = build_capture_args(&s);
        let a = joined(&v);
        assert!(a.starts_with("-y -hide_banner -loglevel warning -copyts -init_hw_device vulkan=vk -filter_hw_device vk -f lavfi -i "), "{a}");
        assert!(a.contains(&format!("ddagrab=output_idx=1:framerate=60:draw_mouse=1:output_fmt=rgbaf16,{STAMP_V},hwdownload,format=rgbaf16le ")), "{a}");
        let vf = v.iter().position(|x| x == "-filter:v").map(|i| v[i + 1].clone()).expect("-filter:v");
        assert_eq!(vf, "setparams=color_primaries=bt709:color_trc=linear:colorspace=gbr:range=pc,hwupload,libplacebo=w=1920:h=1080:format=nv12:colorspace=bt709:color_primaries=bt709:color_trc=iec61966-2-1:range=tv:contrast=0.33333,hwdownload,format=nv12,setparams=color_primaries=bt709:color_trc=bt709");
        assert!(!a.contains("8bit") && !a.contains("hwmap"), "{a}");
        assert!(a.contains(COLOR_TAGS), "{a}");
    }

    #[test]
    fn sdr_args_have_no_vulkan() {
        let a = joined(&build_capture_args(&spec()));
        assert!(!a.contains("vulkan") && !a.contains("libplacebo") && !a.contains("-filter:v"), "{a}");
    }

    #[test]
    fn no_scale_d3d11() {
        // scale_d3d11 cannot allocate its output pool on NVIDIA (E_INVALIDARG).
        for hdr in [None, Some(2.5)] {
            let mut s = spec();
            s.hdr_sdr_white = hdr;
            assert!(!joined(&build_capture_args(&s)).contains("scale_d3d11"));
        }
    }

    #[test]
    fn three_audio_pipes_mapped_in_order() {
        let mut s = spec();
        s.audio_inputs = pipes();
        let v = build_capture_args(&s);
        let a = joined(&v);
        for n in ["p_mic", "p_dc", "p_sys"] {
            let want = ["-i".to_string(), format!(r"\\.\pipe\{n}")];
            assert!(v.windows(2).any(|w| w == want), "{a}");
        }
        assert!(a.contains("-use_wallclock_as_timestamps 1 -f f32le -ar 48000 -ac 2 -i"), "{a}");
        // FFmpeg N-127043 only knows -thread_queue_size as a muxer option and
        // rejects it before an input (verified in Task 7's audio smoke test).
        assert!(!a.contains("thread_queue_size"), "{a}");
        for (i, t) in ["Mic", "Discord", "Game/System"].iter().enumerate() {
            assert!(a.contains(&format!("-metadata:s:a:{i} title={t}")), "{a}");
            // Pipe timestamps are absolute wall-clock time; subtracting the shared
            // origin T0 puts them on the video's clock. Unrebased they sit ~55
            // years ahead of the video and the muxer stops reading the pipes.
            assert!(a.contains(&format!("-filter:a:{i} asetpts=PTS-NB_SAMPLES/SR/TB-{T0_S}/TB,aresample=async=1000")), "{a}");
        }
        let maps: Vec<&str> = v.windows(2).filter(|w| w[0] == "-map").map(|w| w[1].as_str()).collect();
        assert_eq!(maps, ["0:v", "1:a", "2:a", "3:a"]);
        assert!(a.contains("-c:a aac -b:a 160k"), "{a}");
    }

    #[test]
    fn silence_inputs_use_anullsrc() {
        let mut s = spec();
        s.audio_inputs = vec![
            AudioInput::Silence { title: "Mic" },
            AudioInput::Silence { title: "Discord" },
            AudioInput::Silence { title: "Game/System" },
        ];
        let a = joined(&build_capture_args(&s));
        // `-re` throttles lavfi to wall-clock speed (otherwise anullsrc runs flat out).
        assert_eq!(a.matches("-re -f lavfi -i anullsrc=r=48000:cl=stereo").count(), 3, "{a}");
        assert!(!a.contains("pipe"), "{a}");
        assert!(a.contains("-metadata:s:a:2 title=Game/System"), "{a}");
        assert!(a.contains("-filter:a:1 aresample=async=1000"), "{a}");
    }

    #[test]
    fn realtime_sources_are_not_throttled() {
        // ddagrab and pipes are already paced by the real world; `-re` must not touch them.
        let mut s = spec();
        s.audio_inputs = pipes();
        let v = build_capture_args(&s);
        assert!(!v.iter().any(|x| x == "-re"), "{}", joined(&v));
    }

    #[test]
    fn gop_is_two_seconds() {
        let a = joined(&build_capture_args(&spec()));
        assert!(a.contains("-g 120"), "{a}");
        let mut s = spec();
        s.fps = 30;
        let a = joined(&build_capture_args(&s));
        assert!(a.contains("-g 60"), "{a}");
    }

    #[test]
    fn testsrc_replaces_ddagrab() {
        let mut s = spec();
        s.video_source = VideoSource::TestSrc;
        let a = joined(&build_capture_args(&s));
        assert!(a.contains(&format!("-re -f lavfi -i testsrc2=size=1920x1080:rate=60,{STAMP_V},setparams=color_primaries=bt709:color_trc=bt709 ")), "{a}");
        assert!(a.contains("-pix_fmt yuv420p"), "{a}");
        assert!(a.contains(COLOR_TAGS), "{a}");
        assert!(!a.contains("ddagrab") && !a.contains("scale_d3d11"), "{a}");
        assert!(a.contains("h264_nvenc"), "{a}");
    }

    #[test]
    fn encoder_mapping() {
        let mut s = spec();
        let args = |s: &CaptureSpec| joined(&build_capture_args(s));
        s.encoder = Encoder::HevcNvenc;
        assert!(args(&s).contains("-c:v hevc_nvenc -preset p4 -tune hq -rc vbr -cq 25 -b:v 0 -maxrate 40000k"));
        s.encoder = Encoder::Av1Nvenc;
        assert!(args(&s).contains("-c:v av1_nvenc -preset p4 -tune hq -rc vbr -cq 32 -b:v 0 -maxrate 40000k"));
        s.encoder = Encoder::H264Amf;
        assert!(args(&s).contains("-c:v h264_amf -usage transcoding -quality balanced -rc qvbr -qvbr_quality_level 23 -b:v 20000k -maxrate 40000k -bufsize 80000k -g 120"));
        s.encoder = Encoder::Av1Amf;
        assert!(args(&s).contains("-c:v av1_amf -usage transcoding -quality balanced -rc qvbr -qvbr_quality_level 23"));
        s.encoder = Encoder::H264Qsv;
        assert!(args(&s).contains("-c:v h264_qsv -preset veryfast -global_quality 23 -g 120"));
        s.encoder = Encoder::Av1Qsv;
        assert!(args(&s).contains("-c:v av1_qsv -preset veryfast -b:v 20000k -maxrate 40000k -bufsize 80000k -g 120"));
        s.encoder = Encoder::X264;
        assert!(args(&s).contains("-c:v libx264 -preset veryfast -crf 23 -maxrate 40000k -bufsize 80000k -g 120"));
        // Unresolved Auto never reaches FFmpeg as such: it falls back to x264.
        s.encoder = Encoder::Auto;
        assert!(args(&s).contains("-c:v libx264 "));
    }

    /// Every concrete encoder emits its FFmpeg name and the 2 s GOP.
    #[test]
    fn every_encoder_has_args() {
        for e in Encoder::PREFERENCE {
            let a = video_codec_args(e, 60, 20000).join(" ");
            assert!(a.starts_with(&format!("-c:v {} ", e.ffmpeg_name().unwrap())), "{a}");
            assert!(a.ends_with("-g 120"), "{a}");
        }
    }

    #[test]
    fn concat_args_faststart_mp4() {
        let v = build_concat_args(Path::new(r"C:\t\list.txt"), Path::new(r"C:\o\clip.mp4.tmp"), &["Mic", "Discord"]);
        let a = joined(&v);
        assert!(a.contains(r"-f concat -safe 0 -i C:\t\list.txt -map 0 -c copy"), "{a}");
        assert!(a.contains("-metadata:s:a:0 title=Mic") && a.contains("-metadata:s:a:1 title=Discord"), "{a}");
        // The mp4 muxer stores `title` as the udta `name`; players show `handler_name`.
        assert!(a.contains("-metadata:s:a:0 handler_name=Mic -metadata:s:a:1"), "{a}");
        assert!(a.contains("-metadata:s:a:1 handler_name=Discord"), "{a}");
        assert!(a.ends_with(r"-movflags +faststart -f mp4 C:\o\clip.mp4.tmp"), "{a}");
    }
}
