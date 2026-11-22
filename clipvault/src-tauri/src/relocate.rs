//! Moves library clips from one folder to another (after the clip folder
//! changed in Settings), keeping titles, tags, markers and trims: only each
//! clip's file and its `path` row move.
//!
//! * Only clips stored directly in `from` move; other files there stay.
//! * Same drive: a rename. Another drive: copy to `<name>.mp4.moving` (left
//!   behind by a crash, removed by `compress::clean_stale_temps`), compare the
//!   size, rename to `.mp4`, update the row, then delete the original. The
//!   final rename and the row update happen under the library lock, after
//!   checking that the clip was neither deleted nor changed meanwhile. An
//!   original that cannot be deleted (open in a player) is kept and reported.
//! * A clip that background compression holds (`busy`) is skipped: its file
//!   may be replaced while it is copied.
//! * Name clashes in `to` get a `_N` suffix, like saves.
//! * One move at a time ([`begin`]); [`cancel`] stops after the current file.
//! * The latest move's state ([`status`]) outlives the page that started it,
//!   so Settings can show its progress whenever it is opened.
use crate::db::clips::{self, Clip, ClipFilter};
use crate::locks;
use anyhow::{bail, Context, Result};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// Suffix of a file being copied to the new folder.
pub const PARTIAL_SUFFIX: &str = ".mp4.moving";
/// Free space left on the target drive after the copy.
const SPACE_MARGIN: u64 = 1 << 30;

static RUNNING: AtomicBool = AtomicBool::new(false);
static CANCEL: AtomicBool = AtomicBool::new(false);

/// Held while a move runs; dropping it allows the next one.
pub struct Running(());

impl Drop for Running {
    fn drop(&mut self) {
        RUNNING.store(false, Ordering::SeqCst);
    }
}

/// `None` while another move runs.
pub fn begin() -> Option<Running> {
    if RUNNING.swap(true, Ordering::SeqCst) {
        return None;
    }
    CANCEL.store(false, Ordering::SeqCst);
    Some(Running(()))
}

/// Asks the running move to stop after the current file.
pub fn cancel() {
    CANCEL.store(true, Ordering::SeqCst);
}

/// The running or last finished move, for the UI.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct MoveState {
    pub running: bool,
    pub from: String,
    pub to: String,
    pub progress: Option<MoveProgress>,
    pub report: Option<MoveReport>,
    pub error: Option<String>,
}

static STATE: Mutex<Option<MoveState>> = Mutex::new(None);

/// The running or last finished move since ClipVault started.
pub fn status() -> Option<MoveState> {
    locks::lock(&STATE).clone()
}

/// Changes the stored state and returns a copy of it.
pub fn update_status(f: impl FnOnce(&mut MoveState)) -> MoveState {
    let mut g = locks::lock(&STATE);
    let s = g.get_or_insert_with(MoveState::default);
    f(s);
    s.clone()
}

/// What a move from a folder would take.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct MovePreview {
    pub count: u32,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct MoveProgress {
    pub done: u32,
    pub total: u32,
    pub bytes_done: u64,
    pub bytes_total: u64,
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct MoveReport {
    pub moved: u32,
    /// Held by background compression; move again later.
    pub skipped_busy: u32,
    /// `"<file>: <reason>"` for clips that stayed where they were.
    pub failed: Vec<String>,
    /// Moved, but the original could not be deleted (still in the old folder).
    pub originals_kept: Vec<String>,
    pub cancelled: bool,
}

fn same_dir(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| p.to_string_lossy().trim_end_matches(['\\', '/']).to_lowercase();
    norm(a) == norm(b)
}

/// Library clips stored directly in `dir`, oldest first.
fn clips_in(conn: &Connection, dir: &Path) -> Result<Vec<Clip>> {
    let mut v: Vec<Clip> =
        clips::list(conn, ClipFilter::default())?.into_iter().filter(|c| c.path.parent().is_some_and(|p| same_dir(p, dir))).collect();
    v.sort_by_key(|c| c.recorded_at);
    Ok(v)
}

fn file_len(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

pub fn preview(conn: &Connection, from: &Path) -> Result<MovePreview> {
    let v = clips_in(conn, from)?;
    Ok(MovePreview { count: v.len() as u32, bytes: v.iter().map(|c| file_len(&c.path)).sum() })
}

/// Free bytes on `to`'s drive, for the space check; `None` skips it.
pub type FreeBytes<'a> = &'a dyn Fn(&Path) -> Option<u64>;

/// Moves every library clip in `from` to `to` (see module docs). Blocking.
pub fn move_clips(
    db: &Mutex<Connection>,
    from: &Path,
    to: &Path,
    busy: &dyn Fn(&str) -> bool,
    free: FreeBytes,
    on_progress: &mut dyn FnMut(&MoveProgress),
) -> Result<MoveReport> {
    if !from.is_absolute() || !to.is_absolute() {
        bail!("both folders must be full paths");
    }
    if same_dir(from, to) {
        bail!("the clips are already in {}", to.display());
    }
    std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    let copy = crate::disk::volume_key(from) != crate::disk::volume_key(to);
    let todo = clips_in(&locks::lock(db), from)?;
    let bytes_total: u64 = todo.iter().map(|c| file_len(&c.path)).sum();
    if copy {
        if let Some(avail) = free(to) {
            if avail < bytes_total + SPACE_MARGIN {
                bail!(
                    "not enough space on {}: the clips need {}, {} is free",
                    to.display(),
                    crate::disk::format_bytes(bytes_total + SPACE_MARGIN),
                    crate::disk::format_bytes(avail)
                );
            }
        }
    }
    let mut report = MoveReport::default();
    let mut progress = MoveProgress { done: 0, total: todo.len() as u32, bytes_done: 0, bytes_total };
    on_progress(&progress);
    for clip in todo {
        if CANCEL.load(Ordering::SeqCst) {
            report.cancelled = true;
            break;
        }
        let size = file_len(&clip.path);
        let name = clip.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if busy(&clip.id) {
            report.skipped_busy += 1;
        } else {
            match move_one(db, &clip, to, copy) {
                Ok(kept) => {
                    report.moved += 1;
                    if let Some(why) = kept {
                        tracing::warn!("[move] {}: original kept ({why})", clip.path.display());
                        report.originals_kept.push(clip.path.to_string_lossy().into_owned());
                    }
                }
                Err(e) => {
                    tracing::warn!("[move] {}: {e:#}", clip.path.display());
                    report.failed.push(format!("{name}: {e:#}"));
                }
            }
        }
        progress.done += 1;
        progress.bytes_done += size;
        on_progress(&progress);
    }
    tracing::info!(
        "[move] {} -> {}: {} moved, {} busy, {} failed, {} originals kept{}",
        from.display(),
        to.display(),
        report.moved,
        report.skipped_busy,
        report.failed.len(),
        report.originals_kept.len(),
        if report.cancelled { ", cancelled" } else { "" }
    );
    Ok(report)
}

/// Moves one clip. `Ok(Some(reason))`: moved, but the original stayed.
fn move_one(db: &Mutex<Connection>, clip: &Clip, to: &Path, copy: bool) -> Result<Option<String>> {
    let src = &clip.path;
    if !src.is_file() {
        bail!("file not found");
    }
    let name = src.file_name().context("no file name")?.to_string_lossy().into_owned();
    // The row is checked again under the lock: deleted, moved or replaced
    // (compression) meanwhile means the file must not move.
    let unchanged = |conn: &Connection, len: u64| -> Result<bool> {
        Ok(clips::get(conn, &clip.id)?.is_some_and(|c| c.path == *src) && file_len(src) == len)
    };
    if !copy {
        let conn = locks::lock(db);
        if clips::get(&conn, &clip.id)?.is_none_or(|c| c.path != *src) {
            bail!("changed while moving");
        }
        let dst = crate::save::unique_clip_path(to, &name);
        std::fs::rename(src, &dst).with_context(|| format!("renaming to {}", dst.display()))?;
        if let Err(e) = clips::set_path(&conn, &clip.id, &dst) {
            let _ = std::fs::rename(&dst, src);
            return Err(e);
        }
        return Ok(None);
    }
    let len = file_len(src);
    let dst = crate::save::unique_clip_path(to, &name);
    let partial = PathBuf::from(format!("{}{}", dst.with_extension("").to_string_lossy(), PARTIAL_SUFFIX));
    let copied = std::fs::copy(src, &partial).with_context(|| format!("copying to {}", to.display()));
    let copied = match copied {
        Ok(n) => n,
        Err(e) => {
            let _ = std::fs::remove_file(&partial);
            return Err(e);
        }
    };
    let conn = locks::lock(db);
    if copied != len || file_len(&partial) != len || !unchanged(&conn, len)? {
        drop(conn);
        let _ = std::fs::remove_file(&partial);
        bail!("changed while moving");
    }
    // `unique_clip_path` was asked before the copy; a save may have taken the name.
    let dst = if dst.exists() { crate::save::unique_clip_path(to, &name) } else { dst };
    if let Err(e) = std::fs::rename(&partial, &dst) {
        drop(conn);
        let _ = std::fs::remove_file(&partial);
        return Err(e).with_context(|| format!("renaming to {}", dst.display()));
    }
    if let Err(e) = clips::set_path(&conn, &clip.id, &dst) {
        drop(conn);
        let _ = std::fs::remove_file(&dst);
        return Err(e);
    }
    drop(conn);
    Ok(std::fs::remove_file(src).err().map(|e| e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_memory;
    use chrono::DateTime;

    fn clip(id: &str, path: &Path, source: &str) -> Clip {
        Clip {
            id: id.into(),
            title: format!("title {id}"),
            description: String::new(),
            path: path.to_path_buf(),
            game: "Rocket League".into(),
            recorded_at: DateTime::parse_from_rfc3339("2026-11-20T20:15:00+01:00").unwrap(),
            duration_s: 10.0,
            fps: 60,
            width: 1920,
            height: 1080,
            size: 3,
            trim_in: Some(1.0),
            trim_out: None,
            thumbnail_path: None,
            source: source.into(),
            source_id: (source == "moments").then(|| format!("m-{id}")),
            tags: vec!["goal".into()],
            markers: Vec::new(),
            starred: true,
            compressed: false,
            original_size: None,
        }
    }

    struct Fx {
        _dir: tempfile::TempDir,
        from: PathBuf,
        to: PathBuf,
        db: Mutex<Connection>,
    }

    fn fixture(files: &[(&str, &str)]) -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let (from, to) = (dir.path().join("old"), dir.path().join("new"));
        std::fs::create_dir_all(&from).unwrap();
        let conn = open_memory().unwrap();
        for (id, name) in files {
            let p = from.join(name);
            std::fs::write(&p, id.as_bytes()).unwrap();
            clips::insert(&conn, &clip(id, &p, if id.starts_with('m') { "moments" } else { "native" })).unwrap();
        }
        Fx { _dir: dir, from, to, db: Mutex::new(conn) }
    }

    /// Serializes the tests that use the global [`begin`] flag.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn run(fx: &Fx, copy: bool, busy: &dyn Fn(&str) -> bool) -> MoveReport {
        // Tests run on one volume; `copy` forces the cross-drive path.
        let mut report = MoveReport::default();
        let todo = clips_in(&locks::lock(&fx.db), &fx.from).unwrap();
        std::fs::create_dir_all(&fx.to).unwrap();
        for c in todo {
            if busy(&c.id) {
                report.skipped_busy += 1;
                continue;
            }
            match move_one(&fx.db, &c, &fx.to, copy) {
                Ok(None) => report.moved += 1,
                Ok(Some(_)) => {
                    report.moved += 1;
                    report.originals_kept.push(c.path.to_string_lossy().into_owned());
                }
                Err(e) => report.failed.push(format!("{e:#}")),
            }
        }
        report
    }

    fn path_of(fx: &Fx, id: &str) -> PathBuf {
        clips::get(&locks::lock(&fx.db), id).unwrap().unwrap().path
    }

    #[test]
    fn rename_and_copy_move_file_and_keep_metadata() {
        for copy in [false, true] {
            let fx = fixture(&[("a", "A.mp4"), ("m1", "M.mp4")]);
            let r = run(&fx, copy, &|_| false);
            assert_eq!((r.moved, r.failed.len()), (2, 0), "copy={copy}");
            for (id, name) in [("a", "A.mp4"), ("m1", "M.mp4")] {
                let p = path_of(&fx, id);
                assert_eq!(p, fx.to.join(name));
                assert_eq!(std::fs::read(&p).unwrap(), id.as_bytes());
                assert!(!fx.from.join(name).exists());
                let c = clips::get(&locks::lock(&fx.db), id).unwrap().unwrap();
                assert_eq!((c.title, c.tags, c.starred, c.trim_in), (format!("title {id}"), vec!["goal".to_string()], true, Some(1.0)));
            }
            let leftovers: Vec<_> = std::fs::read_dir(&fx.to).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().ends_with(PARTIAL_SUFFIX)).collect();
            assert!(leftovers.is_empty());
        }
    }

    #[test]
    fn name_clash_gets_suffix() {
        for copy in [false, true] {
            let fx = fixture(&[("a", "A.mp4")]);
            std::fs::create_dir_all(&fx.to).unwrap();
            std::fs::write(fx.to.join("A.mp4"), b"other").unwrap();
            run(&fx, copy, &|_| false);
            assert_eq!(path_of(&fx, "a"), fx.to.join("A_2.mp4"));
            assert_eq!(std::fs::read(fx.to.join("A.mp4")).unwrap(), b"other");
        }
    }

    #[test]
    fn busy_and_missing_clips_stay() {
        let fx = fixture(&[("a", "A.mp4"), ("b", "B.mp4")]);
        std::fs::remove_file(fx.from.join("B.mp4")).unwrap();
        let r = run(&fx, true, &|id| id == "a");
        assert_eq!((r.moved, r.skipped_busy, r.failed.len()), (0, 1, 1));
        assert_eq!(path_of(&fx, "a"), fx.from.join("A.mp4"));
        assert_eq!(path_of(&fx, "b"), fx.from.join("B.mp4"));
    }

    #[test]
    fn only_clips_directly_in_from_count() {
        let fx = fixture(&[("a", "A.mp4")]);
        let sub = fx.from.join("Exported Clips");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("X.mp4"), b"xx").unwrap();
        clips::insert(&locks::lock(&fx.db), &clip("x", &sub.join("X.mp4"), "native")).unwrap();
        let p = preview(&locks::lock(&fx.db), &fx.from).unwrap();
        assert_eq!(p, MovePreview { count: 1, bytes: 1 });
        // Trailing separator and case do not matter.
        let upper = PathBuf::from(format!("{}\\", fx.from.to_string_lossy().to_uppercase()));
        assert_eq!(preview(&locks::lock(&fx.db), &upper).unwrap().count, 1);
    }

    #[test]
    fn move_clips_checks_folders_and_space() {
        let _s = locks::lock(&SERIAL);
        let fx = fixture(&[("a", "A.mp4")]);
        let none: FreeBytes = &|_| None;
        let err = |to: &Path, free: FreeBytes| {
            let _g = begin().unwrap();
            move_clips(&fx.db, &fx.from, to, &|_| false, free, &mut |_| {}).unwrap_err().to_string()
        };
        assert!(err(&fx.from, none).contains("already"));
        assert!(err(Path::new("relative"), none).contains("full paths"));
        // Space is only checked across drives; same drive here, so this moves.
        let _g = begin().unwrap();
        let mut seen = Vec::new();
        let r = move_clips(&fx.db, &fx.from, &fx.to, &|_| false, &|_| Some(0), &mut |p| seen.push(p.done)).unwrap();
        assert_eq!(r.moved, 1);
        assert_eq!(seen, vec![0, 1]);
    }

    #[test]
    fn status_keeps_the_latest_move() {
        let _s = locks::lock(&SERIAL);
        update_status(|s| *s = MoveState { running: true, from: "a".into(), to: "b".into(), ..Default::default() });
        let p = MoveProgress { done: 1, total: 2, bytes_done: 5, bytes_total: 10 };
        update_status(|s| s.progress = Some(p.clone()));
        let s = status().unwrap();
        assert_eq!((s.running, s.progress, s.from.as_str()), (true, Some(p), "a"));
    }

    #[test]
    fn only_one_move_at_a_time() {
        let _s = locks::lock(&SERIAL);
        let g = begin().unwrap();
        assert!(begin().is_none());
        drop(g);
        assert!(begin().is_some());
    }
}
