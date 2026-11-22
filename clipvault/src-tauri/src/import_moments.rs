//! SteelSeries Moments import.
//!
//! Reads GG's `database.db` (never the original: a copy in a temp dir, opened
//! read-only) and adds clip metadata, markers and the games list to the
//! ClipVault library. Video files stay where they are. Re-runs are idempotent
//! on `(moments, uuid)` and never overwrite existing rows.

use crate::db::clips::{self, Clip, Marker, MarkerKind};
use crate::db::games;
use crate::settings::Settings;
use crate::thumbs::generate_thumbnail;
use anyhow::{Context, Result};
use chrono::{DateTime, FixedOffset, Local};
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Default location of GG's Moments database.
pub const DEFAULT_GG_DB: &str = r"C:\ProgramData\SteelSeries\GG\apps\moments\db\database.db";

const SAVED_KEY: &str = "moments.timelineEvents.savedClip";
const TRIM_EPS: f64 = 0.05;

#[derive(Debug, Clone, Default)]
pub struct ImportOptions {
    /// Generate a JPEG per imported clip (needs FFmpeg; failures are logged only).
    pub generate_thumbnails: bool,
    /// Where thumbnails go (`<id>.jpg`).
    pub thumbs_dir: PathBuf,
    /// Only the N newest imported clips get a thumbnail (`None` = all).
    pub thumbnail_limit: Option<usize>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ImportReport {
    pub imported: u32,
    pub skipped_existing: u32,
    pub missing_files: Vec<PathBuf>,
    pub games_imported: u32,
    /// `captureDir` / `videoFps` were taken over: the caller must persist `settings`.
    pub settings_changed: bool,
    /// What Moments had for captureDir / videoFps; the caller re-merges it onto
    /// the then-current settings (see [`merge_imported_settings`]).
    pub imported_settings: ImportedSettings,
}

pub fn default_gg_db() -> PathBuf {
    PathBuf::from(DEFAULT_GG_DB)
}

pub fn moments_db_available_at(path: &Path) -> bool {
    path.is_file()
}

pub fn moments_db_available() -> bool {
    moments_db_available_at(&default_gg_db())
}

static IMPORT_RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Held while an import runs; a second import is refused meanwhile.
pub struct ImportRunning(());

impl Drop for ImportRunning {
    fn drop(&mut self) {
        IMPORT_RUNNING.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// `None` while another import is running.
pub fn begin_import() -> Option<ImportRunning> {
    IMPORT_RUNNING
        .compare_exchange(false, true, std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst)
        .ok()
        .map(|_| ImportRunning(()))
}

/// Removes the temp copy on drop.
struct TempCopy(PathBuf);

impl Drop for TempCopy {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Copies GG's `database.db` to a fresh temp dir as one consistent snapshot
/// (`VACUUM INTO` through a read-only connection, so changes GG makes meanwhile
/// never give a torn copy); returns the guard and the copy's path. Falls back
/// to copying the files if SQLite cannot read the source.
fn copy_source(gg_db: &Path) -> Result<(TempCopy, PathBuf)> {
    let dir = std::env::temp_dir().join(format!("clipvault-moments-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir)?;
    let guard = TempCopy(dir.clone());
    let dst = dir.join("database.db");
    let snapshot = Connection::open_with_flags(gg_db, OpenFlags::SQLITE_OPEN_READ_ONLY).and_then(|src| {
        src.busy_timeout(crate::db::BUSY_TIMEOUT)?;
        src.execute("VACUUM INTO ?1", [dst.to_string_lossy()])
    });
    match snapshot {
        Ok(_) => return Ok((guard, dst)),
        Err(e) => {
            tracing::warn!("[moments] snapshot of {} failed ({e}); copying the files", gg_db.display());
            let _ = std::fs::remove_file(&dst);
        }
    }
    std::fs::copy(gg_db, &dst).with_context(|| format!("copying {}", gg_db.display()))?;
    for suffix in ["-wal", "-shm"] {
        let mut name = gg_db.as_os_str().to_os_string();
        name.push(suffix);
        let src = PathBuf::from(name);
        if src.is_file() {
            let mut d = dst.as_os_str().to_os_string();
            d.push(suffix);
            std::fs::copy(&src, PathBuf::from(d)).with_context(|| format!("copying {}", src.display()))?;
        }
    }
    Ok((guard, dst))
}

struct SrcClip {
    id: String,
    name: String,
    description: String,
    path: String,
    start: f64,
    end: f64,
    fps: i64,
    height: i64,
    width: i64,
    recorded: String,
    trimmed_path: String,
    game: String,
    full_length: f64,
    size: i64,
    events: String,
    tags: String,
}

fn parse_tags(json: &str) -> Vec<String> {
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(json) else { return Vec::new() };
    let mut out: Vec<String> = Vec::new();
    for it in items {
        let s = match &it {
            Value::String(s) => Some(s.as_str()),
            Value::Object(o) => ["name", "label", "value"].iter().find_map(|k| o.get(*k).and_then(Value::as_str)),
            _ => None,
        };
        if let Some(s) = s.map(str::trim).filter(|s| !s.is_empty()) {
            if !out.iter().any(|t| t == s) {
                out.push(s.to_string());
            }
        }
    }
    out
}

/// `offset` is subtracted from every timestamp (clip played from a pre-trimmed file).
fn parse_markers(json: &str, full_length: f64, offset: f64, limit: f64) -> Vec<Marker> {
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(json) else { return Vec::new() };
    let mut out = Vec::new();
    for it in items {
        let Some(t) = it.get("clip_timestamp").and_then(Value::as_f64) else { continue };
        if !t.is_finite() || t < 0.0 || t > full_length + 1.0 {
            continue;
        }
        let t = (t - offset).max(0.0).min(limit.max(0.0));
        let s = |k: &str| it.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        if s("localized_name_key") == SAVED_KEY {
            out.push(Marker { t_s: t, kind: MarkerKind::Saved, label: String::new() });
        } else {
            let label = match s("unlocalized_display_name") {
                l if l.is_empty() => s("localized_name_key"),
                l => l,
            };
            out.push(Marker { t_s: t, kind: MarkerKind::Imported, label });
        }
    }
    out.sort_by(|a, b| a.t_s.total_cmp(&b.t_s));
    out
}

fn file_mtime(p: &Path) -> Option<DateTime<FixedOffset>> {
    let t = std::fs::metadata(p).ok()?.modified().ok()?;
    Some(DateTime::<Local>::from(t).fixed_offset())
}

fn import_games(src: &Connection, conn: &Connection) -> Result<u32> {
    let mut stmt = src.prepare("SELECT name, path, capture_enabled FROM moments_games ORDER BY id")?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut n = 0;
    for (name, path, enabled) in rows {
        let name = name.trim();
        // R10: the game's own exe only (never launcher_executables); no path => no game.
        let Some(exe) = Path::new(path.trim()).file_name().and_then(|f| f.to_str()).filter(|_| !path.trim().is_empty())
        else {
            continue;
        };
        // Skip games the library already knows, unless it has no install path
        // yet (the built-in Rocket League entry): then take Moments' path.
        let existing = games::list(conn)?.into_iter().find(|g| g.name == name);
        if name.is_empty() || existing.as_ref().is_some_and(|g| g.path.is_some()) {
            continue;
        }
        // Keep the exes the entry already has (the built-in RocketLeague.exe).
        let mut exes = existing.map(|g| g.exe_names).unwrap_or_default();
        if !exes.iter().any(|e| e.eq_ignore_ascii_case(exe)) {
            exes.push(exe.to_string());
        }
        let id = games::upsert_by_name(conn, name, &exes, Some(Path::new(path.trim())))?;
        if enabled == 0 {
            games::set_enabled(conn, id, false)?;
        }
        n += 1;
    }
    Ok(n)
}

/// Settings found in Moments that ClipVault may take over.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ImportedSettings {
    pub capture_dir: Option<PathBuf>,
    pub fps: Option<u32>,
}

/// `current` with the imported values patched in, each only while the current
/// value still equals the default. `None` when nothing would change (so the
/// caller need not apply or restart anything). Unrelated fields are untouched.
pub fn merge_imported_settings(current: &Settings, imported: &ImportedSettings) -> Option<Settings> {
    let defaults = Settings::default();
    let mut out = current.clone();
    // Only values ClipVault itself accepts (see `library::validate_settings`):
    // an absolute folder, 30 or 60 fps.
    if let Some(dir) = imported.capture_dir.as_ref().filter(|d| d.is_absolute()) {
        if current.capture_dir == defaults.capture_dir {
            out.capture_dir = dir.clone();
        }
    }
    if let Some(fps) = imported.fps.filter(|f| matches!(f, 30 | 60)) {
        if current.fps == defaults.fps {
            out.fps = fps;
        }
    }
    (out != *current).then_some(out)
}

fn read_imported_settings(src: &Connection) -> ImportedSettings {
    let get = |key: &str| -> Option<String> {
        src.query_row("SELECT value FROM moments_settings WHERE key = ?1 ORDER BY id DESC LIMIT 1", [key], |r| r.get(0)).ok()
    };
    ImportedSettings {
        capture_dir: get("captureDir").map(|d| d.trim().to_string()).filter(|d| !d.is_empty()).map(PathBuf::from),
        fps: get("videoFps").and_then(|v| v.trim().parse::<u32>().ok()).filter(|f| *f > 0),
    }
}

/// Imports the Moments library found in `gg_db` into `conn`. Blocking; call from
/// `spawn_blocking`. `progress(done, total)` is called after every clip.
pub fn import_moments(
    gg_db: &Path,
    conn: &Connection,
    settings: &mut Settings,
    ffmpeg: &Path,
    options: &ImportOptions,
    mut progress: impl FnMut(u32, u32),
) -> Result<ImportReport> {
    let (_guard, copy) = copy_source(gg_db)?;
    let src = Connection::open_with_flags(&copy, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening copy of {}", gg_db.display()))?;
    let mut report = ImportReport::default();

    match import_games(&src, conn) {
        Ok(n) => report.games_imported = n,
        Err(e) => tracing::warn!("[moments] games import failed: {e:#}"),
    }
    report.imported_settings = read_imported_settings(&src);
    if let Some(merged) = merge_imported_settings(settings, &report.imported_settings) {
        *settings = merged;
        report.settings_changed = true;
    }

    let mut stmt = src.prepare(
        "SELECT id, name, description, path, clip_start_point, clip_end_point, framerate, resolution_height,
                resolution_width, recording_timestamp, trimmed_clip_path, last_game_name, full_length, filesize,
                COALESCE(gamesense_events,'[]'), COALESCE(tags,'[]')
         FROM moments_clips WHERE is_deleted = 0 ORDER BY julianday(recording_timestamp) DESC, id",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(SrcClip {
                id: r.get::<_, String>(0)?,
                name: r.get(1)?,
                description: r.get(2)?,
                path: r.get(3)?,
                start: r.get(4)?,
                end: r.get(5)?,
                fps: r.get(6)?,
                height: r.get(7)?,
                width: r.get(8)?,
                recorded: r.get(9)?,
                trimmed_path: r.get(10)?,
                game: r.get(11)?,
                full_length: r.get(12)?,
                size: r.get(13)?,
                events: r.get(14)?,
                tags: r.get(15)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let total = rows.len() as u32;
    let mut thumbs_made = 0usize;
    for (i, c) in rows.into_iter().enumerate() {
        let done = i as u32 + 1;
        if clips::exists_source(conn, "moments", &c.id)? {
            report.skipped_existing += 1;
            progress(done, total);
            continue;
        }
        let main = PathBuf::from(&c.path);
        let trimmed = PathBuf::from(&c.trimmed_path);
        let (path, from_trimmed) = if !c.path.is_empty() && main.is_file() {
            (main, false)
        } else if !c.trimmed_path.is_empty() && trimmed.is_file() {
            (trimmed, true)
        } else {
            report.missing_files.push(PathBuf::from(&c.path));
            progress(done, total);
            continue;
        };

        let recorded_at = DateTime::parse_from_rfc3339(c.recorded.trim()).ok().or_else(|| {
            let m = file_mtime(&path);
            tracing::warn!("[moments] clip {}: bad recording_timestamp {:?}; using file time", c.id, c.recorded);
            m
        });
        let recorded_at = recorded_at.unwrap_or_else(|| Local::now().fixed_offset());

        // A pre-trimmed file already is the trimmed clip: no trim, shifted markers.
        let (duration_s, trim_in, trim_out, offset) = if from_trimmed {
            let d = if c.end > c.start { c.end - c.start } else { c.full_length };
            (d, None, None, c.start.max(0.0))
        } else if c.start > TRIM_EPS || c.end < c.full_length - TRIM_EPS {
            let end = if c.end > 0.0 { c.end } else { c.full_length };
            (c.full_length, Some(c.start.max(0.0)), Some(end), 0.0)
        } else {
            (c.full_length, None, None, 0.0)
        };
        let size = if c.size > 0 { c.size as u64 } else { std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) };
        let game = match c.game.trim() {
            "" => "Unknown".to_string(),
            g => g.to_string(),
        };
        let id = uuid::Uuid::new_v4().to_string();

        let mut thumbnail_path = None;
        if options.generate_thumbnails && options.thumbnail_limit.is_none_or(|l| thumbs_made < l) {
            thumbs_made += 1;
            let out = options.thumbs_dir.join(format!("{id}.jpg"));
            let at = (duration_s / 2.0).min(1.0);
            let r = std::fs::create_dir_all(&options.thumbs_dir)
                .map_err(anyhow::Error::from)
                .and_then(|_| generate_thumbnail(ffmpeg, &path, &out, at));
            match r {
                Ok(()) => thumbnail_path = Some(out),
                Err(e) => tracing::warn!("[moments] thumbnail for {} failed: {e:#}", path.display()),
            }
        }

        let clip = Clip {
            id,
            title: c.name,
            description: c.description,
            path,
            game,
            recorded_at,
            duration_s,
            fps: c.fps.max(0) as u32,
            width: c.width.max(0) as u32,
            height: c.height.max(0) as u32,
            size,
            trim_in,
            trim_out,
            thumbnail_path,
            source: "moments".into(),
            source_id: Some(c.id.clone()),
            tags: parse_tags(&c.tags),
            starred: false,
            compressed: false,
            original_size: None,
            markers: parse_markers(&c.events, c.full_length, offset, duration_s),
        };
        match clips::insert(conn, &clip) {
            Ok(()) => report.imported += 1,
            Err(e) => {
                tracing::error!("[moments] clip {} not imported: {e:#}", c.id);
                if let Some(t) = &clip.thumbnail_path {
                    let _ = std::fs::remove_file(t);
                }
            }
        }
        progress(done, total);
    }
    tracing::info!(
        "[moments] import: {} imported, {} existing, {} missing, {} games",
        report.imported,
        report.skipped_existing,
        report.missing_files.len(),
        report.games_imported
    );
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{self, clips};
    use crate::settings::Settings;
    use rusqlite::{params, Connection};
    use std::path::{Path, PathBuf};

    const SCHEMA: &str = include_str!("../tests/fixtures/moments_schema.sql");
    const SAVED_EVENT: &str = r#"[{"type":"","display_name_prefix":"","display_name_suffix":"","localized_name_key":"moments.timelineEvents.savedClip","unlocalized_display_name":"","clip_timestamp":34.2415958,"game":"","previewable":false}]"#;

    struct Fx {
        dir: tempfile::TempDir,
        gg: PathBuf,
    }

    fn fx() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let gg = dir.path().join("database.db");
        let c = Connection::open(&gg).unwrap();
        c.execute_batch(SCHEMA).unwrap();
        Fx { dir, gg }
    }

    impl Fx {
        fn file(&self, name: &str) -> PathBuf {
            let p = self.dir.path().join(name);
            std::fs::write(&p, b"").unwrap();
            p
        }
        fn clip(&self, id: &str, name: &str, path: &Path, deleted: bool, events: &str) {
            let c = Connection::open(&self.gg).unwrap();
            c.execute(
                "INSERT INTO moments_clips (id,name,description,path,clip_start_point,clip_end_point,framerate,
                 resolution_height,resolution_width,recording_timestamp,last_game_name,full_length,filesize,
                 gamesense_events,tags,is_deleted) VALUES (?1,?2,'d',?3,0.0,36.85,60,1080,1920,
                 '2026-11-18T21:37:15+01:00','Rocket League',36.85,1234,?4,'[\"a\",{\"name\":\"b\"},{\"label\":\"c\"},3]',?5)",
                params![id, name, path.to_string_lossy(), events, deleted as i64],
            )
            .unwrap();
        }
        fn run(&self, conn: &Connection) -> ImportReport {
            let mut s = Settings::default();
            import_moments(&self.gg, conn, &mut s, Path::new("none"), &ImportOptions::default(), |_, _| {}).unwrap()
        }
    }

    /// GG keeps writing while we import: the copy is one consistent snapshot
    /// (a single file, WAL folded in), not database.db + -wal copied at
    /// different moments.
    #[test]
    fn source_copy_is_one_consistent_snapshot() {
        let f = fx();
        f.clip("1", "old name", &f.file("a.mp4"), false, "[]");
        // GG running: its open connection keeps the latest change in the WAL.
        let live = Connection::open(&f.gg).unwrap();
        let _: String = live.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0)).unwrap();
        live.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        live.execute("UPDATE moments_clips SET name = 'in wal only'", []).unwrap();
        let mut gg_wal = f.gg.as_os_str().to_os_string();
        gg_wal.push("-wal");
        let wal_len = std::fs::metadata(PathBuf::from(gg_wal)).map(|m| m.len()).unwrap_or(0);
        assert!(wal_len > 0, "fixture: the change must still be in GG's WAL");
        let (_guard, copy) = copy_source(&f.gg).unwrap();
        let mut wal = copy.as_os_str().to_os_string();
        wal.push("-wal");
        assert!(!PathBuf::from(wal).exists(), "the copy must not depend on a separately copied WAL");
        let c = Connection::open_with_flags(&copy, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let name: String = c.query_row("SELECT name FROM moments_clips", [], |r| r.get(0)).unwrap();
        assert_eq!(name, "in wal only", "changes still in GG's WAL are in the snapshot");
        drop(live);
    }

    /// A second click while an import runs is refused instead of racing it.
    #[test]
    fn only_one_import_at_a_time() {
        let first = begin_import().expect("first import starts");
        assert!(begin_import().is_none(), "second import must be refused");
        drop(first);
        assert!(begin_import().is_some(), "free again once the first ended");
    }

    #[test]
    fn imports_live_clips_only() {
        let f = fx();
        let p = f.file("a.mp4");
        f.clip("u1", "Live", &p, false, "[]");
        f.clip("u2", "Gone", &p, true, "[]");
        let conn = db::open_memory().unwrap();
        let r = f.run(&conn);
        assert_eq!(r.imported, 1);
        assert!(clips::exists_source(&conn, "moments", "u1").unwrap());
        assert!(!clips::exists_source(&conn, "moments", "u2").unwrap());
        let c = clips::list(&conn, Default::default()).unwrap().remove(0);
        assert_eq!(c.title, "Live");
        assert_eq!(c.game, "Rocket League");
        assert_eq!(c.tags, vec!["a", "b", "c"]);
        assert_eq!((c.fps, c.width, c.height, c.size), (60, 1920, 1080, 1234));
        assert_eq!((c.trim_in, c.trim_out), (None, None));
        assert_eq!(c.recorded_at.to_rfc3339(), "2026-11-18T21:37:15+01:00");
    }

    #[test]
    fn missing_file_reported() {
        let f = fx();
        let gone = f.dir.path().join("nope.mp4");
        f.clip("u1", "X", &gone, false, "[]");
        let conn = db::open_memory().unwrap();
        let r = f.run(&conn);
        assert_eq!(r.imported, 0);
        assert_eq!(r.missing_files, vec![gone]);
    }

    #[test]
    fn falls_back_to_trimmed_path() {
        let f = fx();
        let t = f.file("t.mp4");
        f.clip("u1", "X", &f.dir.path().join("nope.mp4"), false, "[]");
        Connection::open(&f.gg)
            .unwrap()
            .execute("UPDATE moments_clips SET trimmed_clip_path=?1", [t.to_string_lossy()])
            .unwrap();
        let conn = db::open_memory().unwrap();
        assert_eq!(f.run(&conn).imported, 1);
        assert_eq!(clips::list(&conn, Default::default()).unwrap()[0].path, t);
    }

    #[test]
    fn null_gamesense_events_ok() {
        let f = fx();
        let p = f.file("a.mp4");
        f.clip("u1", "X", &p, false, "null");
        let conn = db::open_memory().unwrap();
        assert_eq!(f.run(&conn).imported, 1);
        assert!(clips::list(&conn, Default::default()).unwrap()[0].markers.is_empty());
    }

    #[test]
    fn saved_marker_mapped() {
        let f = fx();
        let p = f.file("a.mp4");
        let other = r#"[{"localized_name_key":"k.goal","unlocalized_display_name":"Goal!","clip_timestamp":5.0},
            {"localized_name_key":"k.x","unlocalized_display_name":"","clip_timestamp":6.0},
            {"localized_name_key":"k.y","clip_timestamp":-1.0},{"localized_name_key":"k.z","clip_timestamp":99.0},
            {"localized_name_key":"k.w"}]"#;
        f.clip("u1", "X", &p, false, SAVED_EVENT);
        f.clip("u2", "Y", &p, false, other);
        let conn = db::open_memory().unwrap();
        f.run(&conn);
        let all = clips::list(&conn, Default::default()).unwrap();
        let m1 = &all.iter().find(|c| c.source_id.as_deref() == Some("u1")).unwrap().markers;
        assert_eq!(m1.len(), 1);
        assert_eq!(m1[0].kind, clips::MarkerKind::Saved);
        assert!((m1[0].t_s - 34.24).abs() < 0.01);
        let m2 = &all.iter().find(|c| c.source_id.as_deref() == Some("u2")).unwrap().markers;
        assert_eq!(m2.len(), 2);
        assert_eq!((m2[0].kind, m2[0].label.as_str()), (clips::MarkerKind::Imported, "Goal!"));
        assert_eq!(m2[1].label, "k.x");
    }

    #[test]
    fn trim_only_when_meaningful() {
        let f = fx();
        let p = f.file("a.mp4");
        f.clip("u1", "A", &p, false, "[]");
        f.clip("u2", "B", &p, false, "[]");
        f.clip("u3", "C", &p, false, "[]");
        let c = Connection::open(&f.gg).unwrap();
        c.execute("UPDATE moments_clips SET clip_start_point=2.5 WHERE id='u1'", []).unwrap();
        c.execute("UPDATE moments_clips SET clip_end_point=30.0 WHERE id='u2'", []).unwrap();
        c.execute("UPDATE moments_clips SET clip_start_point=0.01, clip_end_point=36.84 WHERE id='u3'", []).unwrap();
        let conn = db::open_memory().unwrap();
        f.run(&conn);
        let all = clips::list(&conn, Default::default()).unwrap();
        let t = |id: &str| {
            let c = all.iter().find(|c| c.source_id.as_deref() == Some(id)).unwrap();
            (c.trim_in, c.trim_out)
        };
        assert_eq!(t("u1"), (Some(2.5), Some(36.85)));
        assert_eq!(t("u2"), (Some(0.0), Some(30.0)));
        assert_eq!(t("u3"), (None, None));
    }

    #[test]
    fn bad_timestamp_falls_back_to_mtime() {
        let f = fx();
        let p = f.file("a.mp4");
        f.clip("u1", "A", &p, false, "[]");
        Connection::open(&f.gg).unwrap().execute("UPDATE moments_clips SET recording_timestamp='garbage'", []).unwrap();
        let conn = db::open_memory().unwrap();
        assert_eq!(f.run(&conn).imported, 1);
        let c = &clips::list(&conn, Default::default()).unwrap()[0];
        assert!((chrono::Utc::now() - c.recorded_at.with_timezone(&chrono::Utc)).num_seconds().abs() < 120);
    }

    #[test]
    fn idempotent_second_run() {
        let f = fx();
        let p = f.file("a.mp4");
        f.clip("u1", "A", &p, false, "[]");
        f.clip("u2", "B", &p, false, "[]");
        let conn = db::open_memory().unwrap();
        assert_eq!(f.run(&conn).imported, 2);
        let id = clips::list(&conn, Default::default()).unwrap()[0].id.clone();
        clips::update_meta(&conn, &id, "Edited", "", &[]).unwrap();
        let r = f.run(&conn);
        assert_eq!((r.imported, r.skipped_existing), (0, 2));
        assert_eq!(clips::get(&conn, &id).unwrap().unwrap().title, "Edited");
        assert_eq!(clips::list(&conn, Default::default()).unwrap().len(), 2);
    }

    #[test]
    fn source_db_unchanged() {
        let f = fx();
        let p = f.file("a.mp4");
        f.clip("u1", "A", &p, false, SAVED_EVENT);
        // WAL-mode source with a live -wal file, as the real GG DB has.
        let keep = Connection::open(&f.gg).unwrap();
        keep.pragma_update(None, "journal_mode", "WAL").unwrap();
        keep.execute("UPDATE moments_clips SET description='wal'", []).unwrap();
        let wal = f.dir.path().join("database.db-wal");
        let snap = || (std::fs::read(&f.gg).unwrap(), std::fs::read(&wal).ok());
        let before = snap();
        let conn = db::open_memory().unwrap();
        let r = f.run(&conn);
        assert_eq!(r.imported, 1);
        assert_eq!(clips::list(&conn, Default::default()).unwrap()[0].description, "wal");
        assert_eq!(snap(), before);
    }

    /// Importing into the built-in Rocket League entry keeps its exe and adds
    /// the Moments one (often the EAC bootstrap) plus the install path.
    #[test]
    fn game_import_merges_exes_into_the_builtin_entry() {
        let f = fx();
        let c = Connection::open(&f.gg).unwrap();
        c.execute_batch(
            r#"INSERT INTO moments_games (name,path,launcher_executables,capture_enabled) VALUES
               ('Rocket League','I:\rl\RocketLeague_EAC.exe','[]',1);"#,
        )
        .unwrap();
        let conn = db::open_memory().unwrap();
        f.run(&conn);
        let g = crate::db::games::list(&conn).unwrap();
        let rl = g.iter().find(|g| g.name == "Rocket League").unwrap();
        assert_eq!(rl.exe_names, vec!["RocketLeague.exe", "RocketLeague_EAC.exe"]);
        assert_eq!(rl.path.as_deref(), Some(Path::new(r"I:\rl\RocketLeague_EAC.exe")));
    }

    #[test]
    fn games_imported_with_exes() {
        let f = fx();
        let c = Connection::open(&f.gg).unwrap();
        c.execute_batch(
            r#"INSERT INTO moments_games (name,path,launcher_executables,capture_enabled) VALUES
               ('Rocket League','C:\Games\RL\RocketLeague.exe','["EpicGamesLauncher.exe"]',1),
               ('Nopath','','["steam.exe"]',1),
               ('Off','D:\x\off.exe','[]',0);"#,
        )
        .unwrap();
        let conn = db::open_memory().unwrap();
        let r = f.run(&conn);
        assert_eq!(r.games_imported, 2);
        let g = crate::db::games::list(&conn).unwrap();
        let rl = g.iter().find(|g| g.name == "Rocket League").unwrap();
        assert_eq!(rl.exe_names, vec!["RocketLeague.exe"]);
        assert!(rl.enabled);
        assert!(!g.iter().find(|g| g.name == "Off").unwrap().enabled);
        assert!(g.iter().all(|g| g.name != "Nopath"));
    }

    #[test]
    fn settings_only_if_default() {
        let f = fx();
        let c = Connection::open(&f.gg).unwrap();
        c.execute_batch(
            r#"INSERT INTO moments_settings (key,value) VALUES ('captureDir','F:\Moments'),('videoFps','30'),('selectedMonitor','\\.\DISPLAY1');"#,
        )
        .unwrap();
        let conn = db::open_memory().unwrap();
        let mut s = Settings::default();
        let r = import_moments(&f.gg, &conn, &mut s, Path::new("none"), &ImportOptions::default(), |_, _| {}).unwrap();
        assert_eq!((s.fps, s.capture_dir.clone()), (30, PathBuf::from(r"F:\Moments")));
        assert_eq!(s.monitor_id, None);
        assert!(r.settings_changed);
        let mut s2 = Settings::default();
        s2.fps = 120;
        s2.capture_dir = r"G:\Mine".into();
        let r2 = import_moments(&f.gg, &conn, &mut s2, Path::new("none"), &ImportOptions::default(), |_, _| {}).unwrap();
        assert_eq!((s2.fps, s2.capture_dir), (120, PathBuf::from(r"G:\Mine")));
        assert!(!r2.settings_changed);
    }

    #[test]
    fn progress_and_thumbnail_failure_nonfatal() {
        let f = fx();
        let p = f.file("a.mp4");
        f.clip("u1", "A", &p, false, "[]");
        f.clip("u2", "B", &p, false, "[]");
        let conn = db::open_memory().unwrap();
        let mut seen = vec![];
        let mut s = Settings::default();
        let opts = ImportOptions { generate_thumbnails: true, thumbs_dir: f.dir.path().join("th"), thumbnail_limit: None };
        let r = import_moments(&f.gg, &conn, &mut s, Path::new("no-ffmpeg"), &opts, |d, t| seen.push((d, t))).unwrap();
        assert_eq!(r.imported, 2);
        assert_eq!(seen.last(), Some(&(2, 2)));
        assert!(clips::list(&conn, Default::default()).unwrap().iter().all(|c| c.thumbnail_path.is_none()));
    }

    #[test]
    fn merge_preserves_unrelated_changes() {
        let imp = ImportedSettings { capture_dir: Some(r"F:\Moments".into()), fps: Some(30) };
        let mut cur = Settings::default();
        cur.bitrate_kbps = 12345;
        let m = merge_imported_settings(&cur, &imp).unwrap();
        assert_eq!(m.bitrate_kbps, 12345);
        assert_eq!((m.fps, m.capture_dir), (30, PathBuf::from(r"F:\Moments")));
    }

    #[test]
    fn merge_does_not_overwrite_user_choice() {
        let imp = ImportedSettings { capture_dir: Some(r"F:\Moments".into()), fps: Some(30) };
        let mut cur = Settings::default();
        cur.capture_dir = r"G:\Mine".into();
        let m = merge_imported_settings(&cur, &imp).unwrap();
        assert_eq!((m.capture_dir, m.fps), (PathBuf::from(r"G:\Mine"), 30));
        cur.fps = 120;
        assert!(merge_imported_settings(&cur, &imp).is_none());
    }

    #[test]
    fn merge_none_when_nothing_to_change() {
        let cur = Settings::default();
        assert!(merge_imported_settings(&cur, &ImportedSettings::default()).is_none());
        let same = ImportedSettings { capture_dir: Some(cur.capture_dir.clone()), fps: Some(cur.fps) };
        assert!(merge_imported_settings(&cur, &same).is_none());
    }

    #[test]
    fn merge_skips_values_clipvault_rejects() {
        let cur = Settings::default();
        let imp = ImportedSettings { capture_dir: Some(r"Moments\clips".into()), fps: Some(144) };
        assert!(merge_imported_settings(&cur, &imp).is_none());
        let imp = ImportedSettings { capture_dir: Some(r"Moments\clips".into()), fps: Some(30) };
        let m = merge_imported_settings(&cur, &imp).unwrap();
        assert_eq!((m.fps, m.capture_dir), (30, cur.capture_dir.clone()));
    }

    #[test]
    fn available_false_for_missing() {
        assert!(!moments_db_available_at(Path::new(r"Z:\definitely\not\here.db")));
    }
}
