use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Subfolder of `buffer_dir` that holds the segments (see [`Settings::segment_dir`]).
pub const BUFFER_SUBDIR: &str = "ClipVault-buffer";
/// Subfolder of `buffer_dir` for per-save temp dirs (same volume as the
/// segments, so a save hard-links them instead of copying).
pub const SAVE_TEMP_SUBDIR: &str = "ClipVault-save";

/// Video encoder for recording. `Auto` picks the best one this PC can run (see
/// `capture::encoders`); the others are fixed choices, replaced by `Auto`'s pick
/// when this PC cannot run them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Encoder {
    Auto,
    H264Nvenc,
    HevcNvenc,
    Av1Nvenc,
    H264Amf,
    HevcAmf,
    Av1Amf,
    H264Qsv,
    HevcQsv,
    Av1Qsv,
    /// libx264 on the CPU: works everywhere, costs CPU while gaming.
    X264,
}

impl Encoder {
    /// Every concrete encoder, best first: AV1, then HEVC, then H.264 on the
    /// GPU (NVIDIA, AMD, Intel), CPU last. `Auto` takes the first one that works.
    pub const PREFERENCE: [Encoder; 10] = [
        Encoder::Av1Nvenc,
        Encoder::Av1Amf,
        Encoder::Av1Qsv,
        Encoder::HevcNvenc,
        Encoder::HevcAmf,
        Encoder::HevcQsv,
        Encoder::H264Nvenc,
        Encoder::H264Amf,
        Encoder::H264Qsv,
        Encoder::X264,
    ];

    /// Whether recording can use it. The replay buffer is MPEG-TS segments and
    /// FFmpeg's MPEG-TS muxer writes AV1 as an unknown data stream: the clip
    /// would have no video (black, no thumbnail, failing concat). AV1 stays
    /// in [`Self::PREFERENCE`] for background compression, which writes MP4.
    pub fn can_record(self) -> bool {
        !matches!(self, Encoder::Av1Nvenc | Encoder::Av1Amf | Encoder::Av1Qsv)
    }

    /// FFmpeg encoder name (`Auto` has none).
    pub fn ffmpeg_name(self) -> Option<&'static str> {
        Some(match self {
            Encoder::Auto => return None,
            Encoder::H264Nvenc => "h264_nvenc",
            Encoder::HevcNvenc => "hevc_nvenc",
            Encoder::Av1Nvenc => "av1_nvenc",
            Encoder::H264Amf => "h264_amf",
            Encoder::HevcAmf => "hevc_amf",
            Encoder::Av1Amf => "av1_amf",
            Encoder::H264Qsv => "h264_qsv",
            Encoder::HevcQsv => "hevc_qsv",
            Encoder::Av1Qsv => "av1_qsv",
            Encoder::X264 => "libx264",
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Encoder::Auto => "Automatic",
            Encoder::H264Nvenc => "NVIDIA H.264",
            Encoder::HevcNvenc => "NVIDIA HEVC",
            Encoder::Av1Nvenc => "NVIDIA AV1",
            Encoder::H264Amf => "AMD H.264",
            Encoder::HevcAmf => "AMD HEVC",
            Encoder::Av1Amf => "AMD AV1",
            Encoder::H264Qsv => "Intel H.264",
            Encoder::HevcQsv => "Intel HEVC",
            Encoder::Av1Qsv => "Intel AV1",
            Encoder::X264 => "CPU H.264 (x264)",
        }
    }

    pub fn is_hevc(self) -> bool {
        matches!(self, Encoder::HevcNvenc | Encoder::HevcAmf | Encoder::HevcQsv)
    }

    pub fn is_hardware(self) -> bool {
        !matches!(self, Encoder::Auto | Encoder::X264)
    }
}

/// How a game is recorded (see `capture::window_capture`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum GameCapture {
    /// The game window itself, whatever monitor it is on.
    #[default]
    Window,
    /// The whole monitor the game is on.
    Monitor,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub fps: u32,
    pub save_length_s: u32,
    pub max_save_length_s: u32,
    pub encoder: Encoder,
    /// Upper limit: recording is quality-based and uses less on quiet scenes.
    pub bitrate_kbps: u32,
    pub output_height: u32,
    pub monitor_id: Option<String>,
    pub desktop_capture: bool,
    /// How games are recorded: their window, or the monitor they are on.
    pub game_capture: GameCapture,
    pub buffer_dir: PathBuf,
    pub capture_dir: PathBuf,
    pub mic_device: Option<String>,
    pub output_device: Option<String>,
    pub mic_enabled: bool,
    pub discord_split: bool,
    pub laugh_enabled: bool,
    pub laugh_threshold_dbfs: f32,
    pub hotkey_save: String,
    pub hotkey_mark: String,
    pub hotkey_toggle_desktop: String,
    pub start_with_windows: bool,
    /// Re-encode saved clips to AV1 in the background (see `compress.rs`).
    pub compress_clips: bool,
}

fn default_buffer_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("ClipVault").join("buffer")
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            fps: 60,
            save_length_s: 60,
            max_save_length_s: 120,
            encoder: Encoder::Auto,
            bitrate_kbps: 40000,
            output_height: 1080,
            monitor_id: None,
            desktop_capture: false,
            game_capture: GameCapture::default(),
            buffer_dir: default_buffer_dir(),
            capture_dir: PathBuf::from(r"E:\GameClips"),
            mic_device: None,
            output_device: None,
            mic_enabled: true,
            discord_split: true,
            laugh_enabled: true,
            laugh_threshold_dbfs: -20.0,
            hotkey_save: "Ctrl+Shift+F10".into(),
            hotkey_mark: "Ctrl+Shift+F9".into(),
            hotkey_toggle_desktop: "Ctrl+Shift+F8".into(),
            start_with_windows: false,
            compress_clips: false,
        }
    }
}

impl Settings {
    /// Load settings; a missing file yields defaults. Missing fields fall back to defaults.
    pub fn load(path: &Path) -> Result<Settings> {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s)
                .with_context(|| format!("parsing settings {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
            Err(e) => Err(e).with_context(|| format!("reading settings {}", path.display())),
        }
    }

    /// Like [`Settings::load`], but never fails: a file that cannot be parsed is
    /// renamed to `settings.json.corrupt-<unix ts>` and defaults are used. The
    /// second value is a user-facing notice when that happened (or the file
    /// could not be read at all).
    pub fn load_or_reset(path: &Path) -> (Settings, Option<String>) {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Settings::default(), None),
            Err(e) => {
                tracing::error!("reading settings {}: {e}; using defaults", path.display());
                return (Settings::default(), Some(format!("Settings could not be read ({e}); using defaults")));
            }
        };
        match serde_json::from_str(&text) {
            Ok(s) => (s, None),
            Err(e) => {
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                let mut name = path.file_name().unwrap_or_default().to_os_string();
                name.push(format!(".corrupt-{ts}"));
                let moved = path.with_file_name(name);
                match std::fs::rename(path, &moved) {
                    Ok(()) => tracing::error!("settings {} corrupt ({e}); moved to {}", path.display(), moved.display()),
                    Err(re) => tracing::error!("settings {} corrupt ({e}); could not move it: {re}", path.display()),
                }
                (Settings::default(), Some("Settings were reset (file was corrupt)".into()))
            }
        }
    }

    /// Settings whose change requires restarting the capture pipeline.
    pub fn capture_differs(&self, other: &Settings) -> bool {
        self.fps != other.fps
            || self.encoder != other.encoder
            || self.bitrate_kbps != other.bitrate_kbps
            || self.output_height != other.output_height
            || self.monitor_id != other.monitor_id
            || self.buffer_dir != other.buffer_dir
            || self.mic_enabled != other.mic_enabled
            || self.mic_device != other.mic_device
            || self.output_device != other.output_device
            || self.discord_split != other.discord_split
            || self.game_capture != other.game_capture
    }

    /// Save length actually used: `save_length_s` clamped to `max_save_length_s`.
    pub fn effective_save_length_s(&self) -> u32 {
        self.save_length_s.min(self.max_save_length_s)
    }

    /// Where FFmpeg writes the rolling buffer: a dedicated subfolder of
    /// `buffer_dir`, so the startup wipe can never meet a user's own files
    /// even if `buffer_dir` points at a shared folder.
    pub fn segment_dir(&self) -> PathBuf {
        self.buffer_dir.join(BUFFER_SUBDIR)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        let mut s = Settings::default();
        s.fps = 30;
        s.encoder = Encoder::HevcNvenc;
        s.mic_device = Some("Mic".into());
        s.laugh_threshold_dbfs = -17.5;
        s.save(&p).unwrap();
        assert_eq!(Settings::load(&p).unwrap(), s);
    }

    #[test]
    fn settings_missing_file_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let s = Settings::load(&dir.path().join("nope.json")).unwrap();
        assert_eq!(s.fps, 60);
        assert_eq!(s.bitrate_kbps, 40000);
        assert_eq!(s.capture_dir, std::path::PathBuf::from(r"E:\GameClips"));
        assert_eq!(s.hotkey_save, "Ctrl+Shift+F10");
        assert_eq!(s.hotkey_mark, "Ctrl+Shift+F9");
        assert_eq!(s.hotkey_toggle_desktop, "Ctrl+Shift+F8");
        assert_eq!(s.max_save_length_s, 120);
        assert_eq!(s, Settings::default());
    }

    #[test]
    fn corrupt_settings_renamed_and_defaults_used() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        std::fs::write(&p, "{ not json").unwrap();
        let (s, notice) = Settings::load_or_reset(&p);
        assert_eq!(s, Settings::default());
        assert_eq!(notice.as_deref(), Some("Settings were reset (file was corrupt)"));
        assert!(!p.exists(), "corrupt file moved away");
        let moved: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(moved.len(), 1);
        assert!(moved[0].starts_with("settings.json.corrupt-"), "{moved:?}");
        assert_eq!(std::fs::read_to_string(dir.path().join(&moved[0])).unwrap(), "{ not json");
    }

    #[test]
    fn load_or_reset_ok_and_missing_have_no_notice() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        assert_eq!(Settings::load_or_reset(&p), (Settings::default(), None));
        let mut s = Settings::default();
        s.fps = 30;
        s.save(&p).unwrap();
        assert_eq!(Settings::load_or_reset(&p), (s, None));
    }

    #[test]
    fn capture_relevant_changes() {
        let base = Settings::default();
        let changed = |f: &dyn Fn(&mut Settings)| {
            let mut s = base.clone();
            f(&mut s);
            base.capture_differs(&s)
        };
        assert!(changed(&|s| s.fps = 30));
        assert!(changed(&|s| s.encoder = Encoder::HevcNvenc));
        assert!(changed(&|s| s.bitrate_kbps = 1));
        assert!(changed(&|s| s.output_height = 720));
        assert!(changed(&|s| s.monitor_id = Some("x".into())));
        assert!(changed(&|s| s.mic_enabled = false));
        assert!(changed(&|s| s.mic_device = Some("m".into())));
        assert!(changed(&|s| s.output_device = Some("o".into())));
        assert!(changed(&|s| s.discord_split = false));
        assert!(changed(&|s| s.buffer_dir = "C:/b".into()));
        // Live-updatable: no restart (the buffer is not wiped for these).
        assert!(!changed(&|s| s.hotkey_save = "F1".into()));
        assert!(!changed(&|s| s.laugh_threshold_dbfs = -3.0));
        assert!(!changed(&|s| s.laugh_enabled = false));
        assert!(!changed(&|s| s.capture_dir = "D:/c".into()));
        assert!(!changed(&|s| s.save_length_s = 30));
        assert!(!changed(&|s| s.desktop_capture = true));
        assert!(!changed(&|s| s.start_with_windows = true));
    }

    #[test]
    fn game_capture_defaults_to_window_and_restarts_capture() {
        assert_eq!(Settings::default().game_capture, GameCapture::Window);
        let old: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(old.game_capture, GameCapture::Window, "settings files without the field");
        let base = Settings::default();
        let mut s = base.clone();
        s.game_capture = GameCapture::Monitor;
        assert!(base.capture_differs(&s));
    }

    #[test]
    fn segments_live_in_a_dedicated_subfolder() {
        let mut s = Settings::default();
        s.buffer_dir = PathBuf::from(r"E:\Stuff");
        assert_eq!(s.segment_dir(), PathBuf::from(r"E:\Stuff\ClipVault-buffer"));
    }

    #[test]
    fn save_length_clamped_to_max() {
        let mut s = Settings::default();
        s.save_length_s = 300;
        s.max_save_length_s = 120;
        assert_eq!(s.effective_save_length_s(), 120);
        s.save_length_s = 45;
        assert_eq!(s.effective_save_length_s(), 45);
    }

    #[test]
    fn settings_partial_file_fills_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.json");
        std::fs::write(&p, r#"{"fps": 30}"#).unwrap();
        let s = Settings::load(&p).unwrap();
        assert_eq!(s.fps, 30);
        assert_eq!(s.bitrate_kbps, 40000);
    }
}
