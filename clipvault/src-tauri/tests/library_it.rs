//! Library export against a real FFmpeg.
//!
//! Set `CLIPVAULT_FFMPEG` to an ffmpeg.exe (libx264 + h264_nvenc, ffprobe.exe
//! beside it); without it the tests print a skip message and pass.

use clipvault_lib::db::clips::{self, Clip, Marker, MarkerKind};
use clipvault_lib::library::export_trimmed;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

fn ffmpeg() -> Option<PathBuf> {
    match std::env::var_os("CLIPVAULT_FFMPEG") {
        Some(p) => Some(PathBuf::from(p)),
        None => {
            eprintln!("SKIP: CLIPVAULT_FFMPEG not set; library export test skipped");
            None
        }
    }
}

fn probe_duration(ff: &Path, file: &Path) -> f64 {
    let o = Command::new(ff.with_file_name("ffprobe.exe"))
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"])
        .arg(file)
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).trim().parse().unwrap()
}

fn probe_frames(ff: &Path, file: &Path) -> (u64, f64) {
    let o = Command::new(ff.with_file_name("ffprobe.exe"))
        .args(["-v", "error", "-select_streams", "v:0", "-count_frames", "-show_entries", "stream=nb_read_frames,r_frame_rate", "-of", "csv=p=0"])
        .arg(file)
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
    let (rate, frames) = s.split_once(',').unwrap();
    let (n, d) = rate.split_once('/').unwrap();
    (frames.parse().unwrap(), n.parse::<f64>().unwrap() / d.parse::<f64>().unwrap())
}

/// 20 s TestSrc clip with a keyframe every second (so stream copy can cut near 5 s).
fn make_source(ff: &Path, dir: &Path) -> PathBuf {
    let out = dir.join("Rocket-League__2026-11-20__20-00-00.mp4");
    let st = Command::new(ff)
        .args(["-y", "-v", "error", "-f", "lavfi", "-i", "testsrc=duration=20:size=320x240:rate=30"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=20"])
        .args(["-c:v", "libx264", "-g", "30", "-pix_fmt", "yuv420p", "-c:a", "aac", "-shortest"])
        .arg(&out)
        .status()
        .unwrap();
    assert!(st.success());
    out
}

fn setup(ff: &Path) -> (tempfile::TempDir, Mutex<Connection>, PathBuf, Clip) {
    let root = tempfile::Builder::new().prefix("Clip Vault's ").tempdir().unwrap();
    let src = make_source(ff, root.path());
    let conn = clipvault_lib::db::open(&root.path().join("lib.db")).unwrap();
    let clip = Clip {
        id: "orig".into(),
        title: "Big play".into(),
        description: "d".into(),
        path: src.clone(),
        game: "Rocket League".into(),
        recorded_at: chrono::DateTime::parse_from_rfc3339("2026-11-20T20:00:00+01:00").unwrap(),
        duration_s: 20.0,
        fps: 30,
        width: 320,
        height: 240,
        size: std::fs::metadata(&src).unwrap().len(),
        trim_in: Some(5.0),
        trim_out: Some(10.0),
        thumbnail_path: None,
        source: "native".into(),
        source_id: None,
        tags: vec!["t".into()],
        starred: false,
        compressed: false,
        original_size: None,
        markers: vec![
            Marker { t_s: 2.0, kind: MarkerKind::Goal, label: "before".into() },
            Marker { t_s: 7.0, kind: MarkerKind::Goal, label: "inside".into() },
            Marker { t_s: 15.0, kind: MarkerKind::Laugh, label: "after".into() },
        ],
    };
    clips::insert(&conn, &clip).unwrap();
    let thumbs = root.path().join("thumbs");
    (root, Mutex::new(conn), thumbs, clip)
}

#[test]
fn export_trimmed_creates_new_clip() {
    let Some(ff) = ffmpeg() else { return };
    let (root, db, thumbs, orig) = setup(&ff);

    let out = export_trimmed(&db, &ff, &thumbs, &thumbs.with_file_name("captures"), "orig", None).unwrap();
    let out = PathBuf::from(out);
    assert_eq!(out.file_name().unwrap(), "Rocket-League__2026-11-20__20-00-00__trim.mp4");
    assert_eq!(out.parent(), orig.path.parent());
    let d = probe_duration(&ff, &out);
    assert!((4.5..=6.0).contains(&d), "stream copy duration {d}");
    assert!(!out.with_file_name("Rocket-League__2026-11-20__20-00-00__trim.mp4.tmp").exists());

    let conn = db.lock().unwrap();
    let all = clips::list(&conn, Default::default()).unwrap();
    assert_eq!(all.len(), 2);
    let new = all.iter().find(|c| c.id != "orig").unwrap();
    assert_eq!(new.title, "Big play (trim)");
    assert_eq!(new.game, "Rocket League");
    assert_eq!(new.recorded_at, orig.recorded_at);
    assert_eq!(new.source, "native");
    assert!(new.source_id.is_none());
    assert_eq!((new.trim_in, new.trim_out), (None, None));
    assert_eq!(new.path, out);
    assert_eq!(new.markers.len(), 1);
    assert_eq!(new.markers[0].label, "inside");
    assert!((new.markers[0].t_s - 2.0).abs() < 1e-6, "{}", new.markers[0].t_s);
    let thumb = new.thumbnail_path.clone().expect("fresh thumbnail");
    assert!(thumb.exists());
    // original untouched
    assert_eq!(clips::get(&conn, "orig").unwrap().unwrap().path, orig.path);
    drop(conn);

    // second export does not overwrite: _2
    let out2 = PathBuf::from(export_trimmed(&db, &ff, &thumbs, &thumbs.with_file_name("captures"), "orig", None).unwrap());
    assert_eq!(out2.file_name().unwrap(), "Rocket-League__2026-11-20__20-00-00__trim_2.mp4");
    drop(root);
}

#[test]
fn export_trimmed_accurate_is_frame_exact() {
    let Some(ff) = ffmpeg() else { return };
    let (_root, db, thumbs, _orig) = setup(&ff);
    // 5.0 s is on a keyframe in the source; use 5.4..10.4 so only a re-encode can be exact
    clips::set_trim(&db.lock().unwrap(), "orig", Some(5.4), Some(10.4)).unwrap();
    let out = PathBuf::from(export_trimmed(&db, &ff, &thumbs, &thumbs.with_file_name("captures"), "orig", Some(clipvault_lib::settings::Encoder::Auto)).unwrap());
    let (frames, fps) = probe_frames(&ff, &out);
    let secs = frames as f64 / fps;
    assert!((secs - 5.0).abs() <= 1.0 / fps + 1e-6, "accurate export {frames} frames @ {fps} = {secs}");
}

#[test]
fn export_requires_trim() {
    let Some(ff) = ffmpeg() else { return };
    let (_root, db, thumbs, _orig) = setup(&ff);
    clips::set_trim(&db.lock().unwrap(), "orig", None, None).unwrap();
    let e = export_trimmed(&db, &ff, &thumbs, &thumbs.with_file_name("captures"), "orig", None).unwrap_err();
    assert!(e.to_string().to_lowercase().contains("trim"), "{e}");
    assert_eq!(clips::list(&db.lock().unwrap(), Default::default()).unwrap().len(), 1);
}
