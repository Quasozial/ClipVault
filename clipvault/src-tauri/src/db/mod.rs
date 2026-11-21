use anyhow::{bail, Context, Result};
use rusqlite::{Connection, ErrorCode};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub mod clips;
pub mod games;

const SCHEMA_V1: &str = include_str!("schema.sql");
/// v2: `clips.starred`, and `removed_sources` so a Moments clip removed from
/// the library is not brought back by a later import.
const SCHEMA_V2: &str = "
ALTER TABLE clips ADD COLUMN starred INTEGER NOT NULL DEFAULT 0;
CREATE INDEX idx_clips_starred ON clips(starred);
CREATE TABLE removed_sources (
    source    TEXT NOT NULL,
    source_id TEXT NOT NULL,
    PRIMARY KEY (source, source_id)
);
";
/// v3: `clips.compressed`: the video was re-encoded to AV1 (or tried and
/// would not get smaller), so background compression skips it.
const SCHEMA_V3: &str = "ALTER TABLE clips ADD COLUMN compressed INTEGER NOT NULL DEFAULT 0;";
/// v4: index for the library's newest-first order (`ORDER BY
/// julianday(recorded_at) DESC, id`), so a refresh does not sort every row.
const SCHEMA_V4: &str = "CREATE INDEX idx_clips_recorded_jd ON clips(julianday(recorded_at) DESC, id);";
const LATEST_VERSION: i64 = 4;

/// How long a statement waits for another connection's write lock (the
/// Moments import uses its own connection) before failing with SQLITE_BUSY.
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(10);
/// Daily library backups kept in the backups folder.
const MAX_BACKUPS: usize = 5;

/// Open (creating if needed) the library database and run migrations.
pub fn open(path: &Path) -> Result<Connection> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut conn = Connection::open(path)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    // NORMAL is the documented safe setting with WAL (no corruption on power
    // loss; at worst the last commits are lost).
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    init(&mut conn)?;
    Ok(conn)
}

/// The integrity check found damage.
#[derive(Debug)]
struct Damaged(String);

impl std::fmt::Display for Damaged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "integrity check failed: {}", self.0)
    }
}

impl std::error::Error for Damaged {}

/// [`open`] plus `PRAGMA quick_check`.
fn open_checked(path: &Path) -> Result<Connection> {
    let conn = open(path)?;
    let verdict: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    if verdict != "ok" {
        return Err(Damaged(verdict).into());
    }
    Ok(conn)
}

/// Whether `e` means the database file is damaged (not: locked, missing
/// permissions, schema too new, ...). Only then is it set aside.
fn is_damage(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.is::<Damaged>()
            || matches!(c.downcast_ref::<rusqlite::Error>(),
                Some(rusqlite::Error::SqliteFailure(f, _))
                    if matches!(f.code, ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase))
    })
}

/// Opens the library at startup. A damaged library never stops the app: it is
/// renamed to `library.db.corrupt-<time>` (with its `-wal`/`-shm`, kept for
/// rescue) and replaced by the newest backup from `backups`, or by a new empty
/// library. The returned notice tells the user what happened.
pub fn open_library(path: &Path, backups: &Path) -> Result<(Connection, Option<String>)> {
    let err = match open_checked(path) {
        Ok(conn) => {
            tracing::info!("library {}", canonical(path).display());
            return Ok((conn, None));
        }
        Err(e) if is_damage(&e) => e,
        Err(e) => return Err(e),
    };
    tracing::error!("library {} is damaged: {err:#}", path.display());
    let aside = set_aside(path)?;
    if let Some((backup, day)) = newest_backup(backups) {
        match std::fs::copy(&backup, path).map_err(anyhow::Error::from).and_then(|_| open_checked(path)) {
            Ok(conn) => {
                tracing::warn!("library restored from {}", backup.display());
                let notice = format!(
                    "The clip library was damaged and has been restored from the backup of {day}. \
                     Clips saved after that are still on disk. The damaged file was kept as {}.",
                    aside.display()
                );
                return Ok((conn, Some(notice)));
            }
            Err(e) => {
                tracing::error!("backup {} unusable: {e:#}", backup.display());
                remove_db_files(path);
            }
        }
    }
    let conn = open_checked(path).context("creating a new library")?;
    let notice = format!(
        "The clip library was damaged and no backup could be used; a new, empty library was created. \
         Your clip files are untouched. The damaged file was kept as {}.",
        aside.display()
    );
    Ok((conn, Some(notice)))
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

/// Renames `path` (and its `-wal`/`-shm`) to `<name>.corrupt-<time>[-wal|-shm]`.
fn set_aside(path: &Path) -> Result<PathBuf> {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let aside = sidecar(path, &format!(".corrupt-{stamp}"));
    std::fs::rename(path, &aside).with_context(|| format!("moving damaged library {} aside", path.display()))?;
    for suffix in ["-wal", "-shm"] {
        let from = sidecar(path, suffix);
        if from.exists() {
            if let Err(e) = std::fs::rename(&from, sidecar(&aside, suffix)) {
                tracing::warn!("moving {} aside: {e}", from.display());
                let _ = std::fs::remove_file(&from);
            }
        }
    }
    Ok(aside)
}

fn remove_db_files(path: &Path) {
    for p in [path.to_path_buf(), sidecar(path, "-wal"), sidecar(path, "-shm")] {
        let _ = std::fs::remove_file(p);
    }
}

/// `library-YYYY-MM-DD.db` files in `dir`, oldest first.
fn backups_in(dir: &Path) -> Vec<(PathBuf, String)> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<(PathBuf, String)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let day = name.strip_prefix("library-")?.strip_suffix(".db")?.to_string();
            Some((e.path(), day))
        })
        .collect();
    out.sort_by(|a, b| a.1.cmp(&b.1));
    out
}

fn newest_backup(dir: &Path) -> Option<(PathBuf, String)> {
    backups_in(dir).pop()
}

/// When the newest backup in `dir` was written (its file time), if any.
pub fn newest_backup_time(dir: &Path) -> Option<std::time::SystemTime> {
    let (path, _) = newest_backup(dir)?;
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Writes a consistent copy of the library to `dir/library-<day>.db`
/// (replacing that day's copy) and keeps only the newest [`MAX_BACKUPS`].
pub fn backup(conn: &Connection, dir: &Path, day: &str) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let target = dir.join(format!("library-{day}.db"));
    let tmp = dir.join(format!("library-{day}.db.tmp"));
    let _ = std::fs::remove_file(&tmp);
    conn.execute("VACUUM INTO ?1", [tmp.to_string_lossy()]).context("writing library backup")?;
    std::fs::rename(&tmp, &target).with_context(|| format!("storing backup {}", target.display()))?;
    let all = backups_in(dir);
    for (old, _) in all.iter().take(all.len().saturating_sub(MAX_BACKUPS)) {
        let _ = std::fs::remove_file(old);
    }
    Ok(())
}

/// Today's backup, unless it exists already. Failures are only logged.
pub fn backup_daily(conn: &Connection, dir: &Path) {
    let day = chrono::Local::now().format("%Y-%m-%d").to_string();
    if dir.join(format!("library-{day}.db")).exists() {
        return;
    }
    match backup(conn, dir, &day) {
        Ok(()) => tracing::info!("library backup {day} written"),
        Err(e) => tracing::warn!("library backup failed: {e:#}"),
    }
}

/// The real path behind `path` (so the log shows whether Windows redirected
/// it, e.g. MSIX AppData virtualization).
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
pub(crate) fn open_memory() -> Result<Connection> {
    let mut conn = Connection::open_in_memory()?;
    init(&mut conn)?;
    Ok(conn)
}

fn init(conn: &mut Connection) -> Result<()> {
    conn.pragma_update(None, "foreign_keys", "ON")?;
    migrate(conn)
}

fn migrate(conn: &mut Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version > LATEST_VERSION {
        bail!("database schema v{version} is newer than supported v{LATEST_VERSION}");
    }
    if version < 1 {
        let tx = conn.transaction()?;
        tx.execute_batch(SCHEMA_V1)?;
        // Brand-new library only: detect Rocket League out of the box. A
        // Moments import later fills in its install path.
        games::upsert_by_name(&tx, "Rocket League", &["RocketLeague.exe".into()], None)?;
        tx.pragma_update(None, "user_version", 1)?;
        tx.commit()?;
    }
    if version < 2 {
        let tx = conn.transaction()?;
        tx.execute_batch(SCHEMA_V2)?;
        tx.pragma_update(None, "user_version", 2)?;
        tx.commit()?;
    }
    if version < 3 {
        let tx = conn.transaction()?;
        tx.execute_batch(SCHEMA_V3)?;
        tx.pragma_update(None, "user_version", 3)?;
        tx.commit()?;
    }
    if version < 4 {
        let tx = conn.transaction()?;
        tx.execute_batch(SCHEMA_V4)?;
        tx.pragma_update(None, "user_version", 4)?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_migrates_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(&dir.path().join("lib.db")).unwrap();
        for t in ["clips", "markers", "games", "settings"] {
            let n: i64 = conn
                .query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1", [t], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 1, "table {t}");
        }
        let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, LATEST_VERSION);
        // A fresh library detects Rocket League without any setup.
        let g = games::list(&conn).unwrap();
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].name, "Rocket League");
        assert_eq!(g[0].exe_names, vec!["RocketLeague.exe"]);
        assert!(g[0].enabled);
        // reopen is idempotent
        drop(conn);
        assert_eq!(games::list(&open(&dir.path().join("lib.db")).unwrap()).unwrap().len(), 1);
        let conn = open(&dir.path().join("lib.db")).unwrap();
        let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, LATEST_VERSION);
    }

    fn game_names(conn: &Connection) -> Vec<String> {
        let mut st = conn.prepare("SELECT name FROM games ORDER BY name").unwrap();
        st.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
    }

    fn corrupt_leftovers(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("library.db.corrupt-"))
            .collect()
    }

    #[test]
    fn open_sets_busy_timeout_and_synchronous() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(&dir.path().join("lib.db")).unwrap();
        let busy: i64 = conn.query_row("PRAGMA busy_timeout", [], |r| r.get(0)).unwrap();
        let sync: i64 = conn.query_row("PRAGMA synchronous", [], |r| r.get(0)).unwrap();
        assert_eq!(busy, BUSY_TIMEOUT.as_millis() as i64);
        assert_eq!(sync, 1, "NORMAL: the documented safe setting with WAL");
    }

    /// A corrupt library must not kill startup: it is set aside (kept for
    /// rescue), a fresh one is created and the caller gets a notice.
    #[test]
    fn corrupt_library_is_set_aside_and_recreated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        std::fs::write(&path, b"this is not a database ".repeat(400)).unwrap();
        let (conn, notice) = open_library(&path, &dir.path().join("backups")).unwrap();
        assert!(notice.is_some_and(|n| n.contains("damaged")));
        assert_eq!(game_names(&conn), ["Rocket League"], "fresh library");
        let left = corrupt_leftovers(dir.path());
        assert_eq!(left.len(), 1, "damaged file kept: {left:?}");
    }

    /// With a backup the library comes back as of the backup.
    #[test]
    fn corrupt_library_is_restored_from_the_newest_backup() {
        let dir = tempfile::tempdir().unwrap();
        let (path, backups) = (dir.path().join("library.db"), dir.path().join("backups"));
        {
            let (conn, notice) = open_library(&path, &backups).unwrap();
            assert!(notice.is_none());
            conn.execute("INSERT INTO games (name) VALUES ('Old')", []).unwrap();
            backup(&conn, &backups, "2026-11-01").unwrap();
            conn.execute("INSERT INTO games (name) VALUES ('RL')", []).unwrap();
            backup(&conn, &backups, "2026-11-02").unwrap();
        }
        // Clobber the header: SQLite now refuses the file.
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[..16].copy_from_slice(b"garbage garbage!");
        std::fs::write(&path, bytes).unwrap();
        let (conn, notice) = open_library(&path, &backups).unwrap();
        assert!(notice.is_some_and(|n| n.contains("2026-11-02")));
        assert_eq!(game_names(&conn), ["Old", "RL", "Rocket League"]);
        assert_eq!(corrupt_leftovers(dir.path()).len(), 1);
    }

    #[test]
    fn backups_are_daily_and_only_the_newest_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(&dir.path().join("lib.db")).unwrap();
        let backups = dir.path().join("backups");
        for day in 1..=7 {
            backup(&conn, &backups, &format!("2026-11-0{day}")).unwrap();
        }
        backup(&conn, &backups, "2026-11-07").unwrap(); // same day again: replaced, not added
        let mut names: Vec<String> =
            std::fs::read_dir(&backups).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        let want: Vec<String> = (3..=7).map(|d| format!("library-2026-11-0{d}.db")).collect();
        assert_eq!(names, want);
        let copy = Connection::open(backups.join("library-2026-11-07.db")).unwrap();
        let v: i64 = copy.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, LATEST_VERSION, "a backup is a complete, openable library");
    }

    #[test]
    fn newer_schema_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib.db");
        Connection::open(&path).unwrap().pragma_update(None, "user_version", LATEST_VERSION + 1).unwrap();
        let err = open(&path).unwrap_err().to_string();
        assert!(err.contains("newer than supported"), "{err}");
    }

    #[test]
    fn db_migrates_v1_to_v2_keeping_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
            conn.execute_batch(
                "INSERT INTO games (name) VALUES ('RL');
                 INSERT INTO clips (id, title, path, game_id, recorded_at, duration, fps, width, height, size,
                    source, created_at, updated_at)
                 VALUES ('c1', 't', 'p', 1, '2026-01-01T00:00:00+00:00', 1, 60, 1, 1, 1, 'native', 'x', 'x');",
            )
            .unwrap();
        }
        let conn = open(&path).unwrap();
        let starred: i64 = conn.query_row("SELECT starred FROM clips WHERE id='c1'", [], |r| r.get(0)).unwrap();
        assert_eq!(starred, 0);
        let n: i64 = conn.query_row("SELECT count(*) FROM removed_sources", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
    }
}
