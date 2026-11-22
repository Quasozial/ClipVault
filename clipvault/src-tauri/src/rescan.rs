//! Capture-folder rescan after a library restore.
//!
//! When `library.db` is damaged it is replaced by the newest daily backup (see
//! `db::open_library`). Clips saved after that backup are still on disk but
//! have no row. This finds them, directly in the capture folder, by
//! ClipVault's file naming (`<Game>__YYYY-MM-DD__HH-MM-SS[_N].mp4`, see
//! `save::clip_filename`), probes them with FFmpeg and adds them back.
//!
//! * Only files modified after `since` (the restored backup's time) are taken:
//!   older ones were known when the backup was written, either in it or
//!   removed from the library on purpose. `None` (no usable backup, an empty
//!   library) takes every matching file.
//! * Added clips have no markers and no thumbnail (made on demand by the grid).
//! * `may_compress == false` marks them compressed so background compression
//!   never rewrites them: without a backup to tell, a file could be a Moments
//!   clip, which ClipVault must not write.
use crate::compress::parse_media_info;
use crate::db::clips::{self, Clip, ClipFilter};
use crate::locks;
use anyhow::Result;
use chrono::{Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone};
use rusqlite::Connection;
use std::collections::HashSet;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

const PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// Game name and local recording time from a ClipVault clip file name.
/// Spaces in the game name were written as `-`; they come back as spaces.
pub fn parse_clip_filename(name: &str) -> Option<(String, NaiveDateTime)> {
    let stem = name.strip_suffix(".mp4").or_else(|| name.strip_suffix(".MP4"))?;
    let mut parts = stem.rsplitn(3, "__");
    let (time, date, game) = (parts.next()?, parts.next()?, parts.next()?);
    let time = NaiveTime::parse_from_str(time.get(..8)?, "%H-%M-%S").ok()?;
    let date = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    if game.is_empty() {
        return None;
    }
    Some((game.replace('-', " "), date.and_time(time)))
}

/// Video size, frame rate and whether the video is AV1, from `ffmpeg -i` stderr.
pub fn parse_video_stream(stderr: &str) -> (u32, u32, u32, bool) {
    let Some(line) = stderr.lines().find(|l| l.trim_start().starts_with("Stream #0:") && l.contains(": Video:")) else {
        return (0, 0, 0, false);
    };
    let av1 = line.contains("Video: av1");
    let (mut w, mut h, mut fps) = (0, 0, 0);
    for part in line.split(',') {
        let p = part.trim();
        if let Some(f) = p.strip_suffix(" fps") {
            fps = f.trim().parse::<f64>().map(|f| f.round() as u32).unwrap_or(0);
        }
        let dims = p.split_whitespace().next().unwrap_or("");
        if let Some((a, b)) = dims.split_once('x') {
            if let (Ok(a), Ok(b)) = (a.parse(), b.parse()) {
                if w == 0 {
                    (w, h) = (a, b);
                }
            }
        }
    }
    (w, h, fps, av1)
}

/// Adds clip files in `dir` that the library does not know (see module docs).
/// Returns how many were added. Blocking.
pub fn rescan(db: &Mutex<Connection>, ffmpeg: &Path, dir: &Path, since: Option<SystemTime>, may_compress: bool) -> Result<usize> {
    let known: HashSet<String> = clips::list(&locks::lock(db), ClipFilter::default())?
        .into_iter()
        .map(|c| c.path.to_string_lossy().to_lowercase())
        .collect();
    let Ok(entries) = std::fs::read_dir(dir) else { return Ok(0) };
    let mut added = 0;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some((game, local)) = parse_clip_filename(&name) else { continue };
        let path = e.path();
        if known.contains(&path.to_string_lossy().to_lowercase()) {
            continue;
        }
        let Ok(meta) = e.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        if let (Some(since), Ok(modified)) = (since, meta.modified()) {
            if modified <= since {
                continue;
            }
        }
        let Some(stderr) = crate::ffmpeg_proc::describe(ffmpeg, &path, PROBE_TIMEOUT) else { continue };
        let Some(info) = parse_media_info(&stderr) else {
            tracing::warn!("[rescan] {}: not a readable video, skipped", path.display());
            continue;
        };
        let (width, height, fps, av1) = parse_video_stream(&stderr);
        let recorded = Local.from_local_datetime(&local).earliest().unwrap_or_else(Local::now);
        let clip = Clip {
            id: uuid::Uuid::new_v4().to_string(),
            title: format!("{game} {}", recorded.format("%d.%m.%Y %H:%M")),
            description: String::new(),
            path: path.clone(),
            game,
            recorded_at: recorded.fixed_offset(),
            duration_s: info.duration_s,
            fps,
            width,
            height,
            size: meta.len(),
            trim_in: None,
            trim_out: None,
            thumbnail_path: None,
            source: "native".into(),
            source_id: None,
            tags: Vec::new(),
            markers: Vec::new(),
            starred: false,
            compressed: false,
        };
        let conn = locks::lock(db);
        clips::insert(&conn, &clip)?;
        if av1 || !may_compress {
            clips::set_compressed(&conn, &clip.id, clip.size)?;
        }
        drop(conn);
        tracing::info!("[rescan] re-added {}", path.display());
        added += 1;
    }
    Ok(added)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_clipvault_file_names() {
        let t = |s| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap();
        assert_eq!(
            parse_clip_filename("Rocket-League__2026-11-18__21-37-14.mp4"),
            Some(("Rocket League".into(), t("2026-11-18 21:37:14")))
        );
        assert_eq!(
            parse_clip_filename("Rocket-League__2026-11-18__21-37-14_2.mp4"),
            Some(("Rocket League".into(), t("2026-11-18 21:37:14")))
        );
        assert_eq!(
            parse_clip_filename("Tom-Clancy's_-Siege___2026-11-18__21-37-14.mp4"),
            Some(("Tom Clancy's_ Siege_".into(), t("2026-11-18 21:37:14")))
        );
        assert_eq!(parse_clip_filename("holiday.mp4"), None);
        assert_eq!(parse_clip_filename("G__2026-11-18__21-37-14.mp4.tmp"), None);
        assert_eq!(parse_clip_filename("G__2026-13-18__21-37-14.mp4"), None);
        assert_eq!(parse_clip_filename("__2026-11-18__21-37-14.mp4"), None);
    }

    #[test]
    fn parses_video_stream_line() {
        let s = "  Duration: 00:01:00.00, start: 0.0\n  Stream #0:0[0x1](und): Video: h264 (High) (avc1 / 0x31637661), yuv420p(tv, bt709, progressive), 1920x1080 [SAR 1:1 DAR 16:9], 20000 kb/s, 60 fps, 60 tbr, 15360 tbn (default)\n  Stream #0:1: Audio: aac\n";
        assert_eq!(parse_video_stream(s), (1920, 1080, 60, false));
        let a = "  Stream #0:0: Video: av1 (libdav1d) (Main), yuv420p, 2560x1440, 59.94 fps\n";
        assert_eq!(parse_video_stream(a), (2560, 1440, 60, true));
        assert_eq!(parse_video_stream("nothing"), (0, 0, 0, false));
    }

    /// Only unknown, newer, readable files matching the naming are added.
    #[test]
    fn rescan_skips_known_old_and_foreign_files() {
        let dir = tempfile::tempdir().unwrap();
        let db = Mutex::new(crate::db::open_memory().unwrap());
        std::fs::write(dir.path().join("holiday.mp4"), b"x").unwrap();
        let since = SystemTime::now() + Duration::from_secs(3600);
        std::fs::write(dir.path().join("G__2026-11-18__21-37-14.mp4"), b"x").unwrap();
        // Everything is older than `since`, so nothing is probed or added.
        assert_eq!(rescan(&db, Path::new("no-ffmpeg.exe"), dir.path(), Some(since), true).unwrap(), 0);
        // Without `since` the match is probed; a missing FFmpeg skips it.
        assert_eq!(rescan(&db, Path::new("no-ffmpeg.exe"), dir.path(), None, false).unwrap(), 0);
        assert_eq!(rescan(&db, Path::new("x"), &dir.path().join("missing"), None, false).unwrap(), 0);
    }
}
