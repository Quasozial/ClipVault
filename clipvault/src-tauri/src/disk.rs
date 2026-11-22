//! Free disk space: a probe, the pre-save check, and the low-space warning
//! state. A full disk must never turn into a silent failure: a save that
//! would not fit fails up front with a clear reason, and a volume running
//! low is announced while there is still room to act.
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

pub const GIB: u64 = 1024 * 1024 * 1024;
pub const MIB: u64 = 1024 * 1024;

/// Warn when a volume holding the buffer or the clips drops below this.
pub const LOW_SPACE: u64 = 5 * GIB;
/// Warn again only after free space has climbed back above
/// `LOW_SPACE + REARM_HYSTERESIS` (no toast storm around the threshold).
pub const REARM_HYSTERESIS: u64 = GIB;
/// Headroom a save keeps on top of the clip's own size (MP4 index, thumbnail,
/// other writers on the same volume).
pub const SAVE_MARGIN: u64 = 64 * MIB;

/// Bytes available to the caller on the volume holding `path`. `path` may not
/// exist yet (a capture folder created on first save): its nearest existing
/// ancestor is asked instead.
pub fn free_bytes(path: &Path) -> std::io::Result<u64> {
    let mut p = path;
    while !p.exists() {
        p = p.parent().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, format!("no existing folder above {}", path.display()))
        })?;
    }
    free_bytes_of_existing(p)
}

#[cfg(windows)]
fn free_bytes_of_existing(path: &Path) -> std::io::Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let mut available = 0u64;
    // SAFETY: `wide` is NUL-terminated and outlives the call; the out pointer
    // is a valid u64.
    unsafe { GetDiskFreeSpaceExW(PCWSTR(wide.as_ptr()), Some(&mut available), None, None) }
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(available)
}

#[cfg(not(windows))]
fn free_bytes_of_existing(_path: &Path) -> std::io::Result<u64> {
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "free space probe is Windows-only"))
}

/// `12.3 GB` / `850 MB`.
pub fn format_bytes(b: u64) -> String {
    if b >= GIB {
        format!("{:.1} GB", b as f64 / GIB as f64)
    } else {
        format!("{} MB", b.div_ceil(MIB))
    }
}

/// Why a clip of `clip_bytes` cannot be written to `dir` with `free` bytes
/// left, or `None` if it fits (with [`SAVE_MARGIN`] to spare).
pub fn save_space_error(dir: &Path, free: u64, clip_bytes: u64) -> Option<String> {
    let need = clip_bytes.saturating_add(SAVE_MARGIN);
    (free < need).then(|| {
        format!(
            "not enough disk space in {}: {} free, the clip needs about {}. Free up space or pick another clip folder.",
            dir.display(),
            format_bytes(free),
            format_bytes(need)
        )
    })
}

/// The volume a path lives on (`E:`, `\\server\share`), used to warn once per
/// volume when the buffer and the clips share it.
pub fn volume_key(path: &Path) -> PathBuf {
    match path.components().next() {
        Some(Component::Prefix(p)) => PathBuf::from(p.as_os_str().to_ascii_uppercase()),
        _ => PathBuf::from("/"),
    }
}

/// Per-volume low-space warning state with hysteresis.
#[derive(Debug, Default)]
pub struct LowSpaceWatcher {
    warned: HashMap<PathBuf, bool>,
}

impl LowSpaceWatcher {
    /// Feeds one reading; returns true when this reading should be announced
    /// (the volume just dropped below [`LOW_SPACE`]).
    pub fn on_reading(&mut self, volume: &Path, free: u64) -> bool {
        let warned = self.warned.entry(volume.to_path_buf()).or_insert(false);
        if free < LOW_SPACE {
            let announce = !*warned;
            *warned = true;
            announce
        } else {
            if free > LOW_SPACE + REARM_HYSTERESIS {
                *warned = false;
            }
            false
        }
    }
}

/// Toast text for a volume low on space; `uses` says what lives there.
pub fn low_space_text(volume: &Path, uses: &str, free: u64) -> String {
    format!(
        "Low disk space on {}: {} left ({uses}). Saving clips will fail once it is full.",
        volume.display(),
        format_bytes(free)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_fits_only_with_margin() {
        let d = Path::new(r"E:\GameClips");
        assert!(save_space_error(d, GIB, 500 * MIB).is_none());
        assert!(save_space_error(d, 500 * MIB + SAVE_MARGIN, 500 * MIB).is_none());
        let e = save_space_error(d, 500 * MIB + SAVE_MARGIN - 1, 500 * MIB).unwrap();
        assert!(e.contains(r"E:\GameClips"), "{e}");
        assert!(e.contains("564 MB"), "{e}");
    }

    #[test]
    fn bytes_format() {
        assert_eq!(format_bytes(0), "0 MB");
        assert_eq!(format_bytes(1), "1 MB");
        assert_eq!(format_bytes(850 * MIB), "850 MB");
        assert_eq!(format_bytes(12 * GIB + 300 * MIB), "12.3 GB");
    }

    #[cfg(windows)]
    #[test]
    fn volume_keys() {
        assert_eq!(volume_key(Path::new(r"e:\GameClips")), PathBuf::from("E:"));
        assert_eq!(volume_key(Path::new(r"E:\buffer\ClipVault-buffer")), PathBuf::from("E:"));
        assert_ne!(volume_key(Path::new(r"C:\x")), volume_key(Path::new(r"E:\x")));
    }

    #[test]
    fn warns_once_then_rearms_after_hysteresis() {
        let v = Path::new("E:");
        let mut w = LowSpaceWatcher::default();
        assert!(!w.on_reading(v, 10 * GIB));
        assert!(w.on_reading(v, 4 * GIB));
        assert!(!w.on_reading(v, 3 * GIB));
        // Back just above the threshold: still armed off (hysteresis).
        assert!(!w.on_reading(v, LOW_SPACE + MIB));
        assert!(!w.on_reading(v, 4 * GIB));
        // Clearly recovered, then low again: announced again.
        assert!(!w.on_reading(v, LOW_SPACE + REARM_HYSTERESIS + 1));
        assert!(w.on_reading(v, 4 * GIB));
        // Volumes are independent.
        assert!(w.on_reading(Path::new("C:"), GIB));
    }

    #[cfg(windows)]
    #[test]
    fn probe_reads_real_volume_and_missing_folders() {
        let dir = tempfile::tempdir().unwrap();
        let free = free_bytes(dir.path()).unwrap();
        assert!(free > 0);
        let missing = dir.path().join("not").join("yet");
        let free2 = free_bytes(&missing).unwrap();
        assert!(free2 > 0);
    }
}
