use anyhow::{anyhow, Result};
use chrono::{DateTime, FixedOffset, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MarkerKind {
    Saved,
    Goal,
    Laugh,
    Manual,
    Imported,
}

impl MarkerKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            MarkerKind::Saved => "saved",
            MarkerKind::Goal => "goal",
            MarkerKind::Laugh => "laugh",
            MarkerKind::Manual => "manual",
            MarkerKind::Imported => "imported",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "saved" => MarkerKind::Saved,
            "goal" => MarkerKind::Goal,
            "laugh" => MarkerKind::Laugh,
            "manual" => MarkerKind::Manual,
            "imported" => MarkerKind::Imported,
            other => return Err(anyhow!("unknown marker kind {other:?}")),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Marker {
    pub t_s: f64,
    pub kind: MarkerKind,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Clip {
    pub id: String,
    pub title: String,
    pub description: String,
    pub path: PathBuf,
    pub game: String,
    pub recorded_at: DateTime<FixedOffset>,
    pub duration_s: f64,
    pub fps: u32,
    pub width: u32,
    pub height: u32,
    pub size: u64,
    pub trim_in: Option<f64>,
    pub trim_out: Option<f64>,
    pub thumbnail_path: Option<PathBuf>,
    pub source: String,
    pub source_id: Option<String>,
    pub tags: Vec<String>,
    pub markers: Vec<Marker>,
    #[serde(default)]
    pub starred: bool,
    /// Re-encoded to AV1 by background compression (or tried; see `compress.rs`).
    #[serde(default)]
    pub compressed: bool,
    /// Size before compression replaced the file; `None` if it never did (or
    /// did before schema v6).
    #[serde(default)]
    pub original_size: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct ClipFilter {
    /// Exact game name.
    pub game: Option<String>,
    /// Whitespace-separated words; every word must appear (case-insensitive)
    /// in the title, description, a tag, the game name, the recording date
    /// (`2026-11-20`) or a marker's label or kind (`goal`, `laugh`, ...).
    pub query: Option<String>,
    pub starred_only: bool,
    pub limit: Option<u32>,
}

const SELECT: &str = "SELECT c.id, c.title, c.description, c.path, g.name, c.recorded_at, c.duration, c.fps,
    c.width, c.height, c.size, c.trim_in, c.trim_out, c.thumbnail_path, c.source, c.source_id, c.tags_json,
    c.starred, c.compressed, c.original_size
    FROM clips c JOIN games g ON g.id = c.game_id";

pub fn insert(conn: &Connection, clip: &Clip) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute("INSERT OR IGNORE INTO games (name) VALUES (?1)", [&clip.game])?;
    let game_id: i64 =
        tx.query_row("SELECT id FROM games WHERE name = ?1", [&clip.game], |r| r.get(0))?;
    let now = Utc::now().to_rfc3339();
    tx.execute(
        "INSERT INTO clips (id, title, description, path, game_id, recorded_at, duration, fps, width, height,
            size, trim_in, trim_out, thumbnail_path, source, source_id, tags_json, created_at, updated_at, starred)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?18,?19)",
        params![
            clip.id,
            clip.title,
            clip.description,
            clip.path.to_string_lossy(),
            game_id,
            clip.recorded_at.to_rfc3339(),
            clip.duration_s,
            clip.fps,
            clip.width,
            clip.height,
            clip.size as i64,
            clip.trim_in,
            clip.trim_out,
            clip.thumbnail_path.as_ref().map(|p| p.to_string_lossy().into_owned()),
            clip.source,
            clip.source_id,
            serde_json::to_string(&clip.tags)?,
            now,
            clip.starred,
        ],
    )?;
    for m in &clip.markers {
        tx.execute(
            "INSERT INTO markers (clip_id, t_seconds, type, label) VALUES (?1, ?2, ?3, ?4)",
            params![clip.id, m.t_s, m.kind.as_str(), m.label],
        )?;
    }
    tx.commit()?;
    Ok(())
}

fn row_to_clip(r: &rusqlite::Row) -> rusqlite::Result<Clip> {
    let conv = |i: usize, e: Box<dyn std::error::Error + Send + Sync>| {
        rusqlite::Error::FromSqlConversionFailure(i, rusqlite::types::Type::Text, e)
    };
    let recorded: String = r.get(5)?;
    let tags: String = r.get(16)?;
    let path: String = r.get(3)?;
    let thumb: Option<String> = r.get(13)?;
    Ok(Clip {
        id: r.get(0)?,
        title: r.get(1)?,
        description: r.get(2)?,
        path: PathBuf::from(path),
        game: r.get(4)?,
        recorded_at: DateTime::parse_from_rfc3339(&recorded).map_err(|e| conv(5, Box::new(e)))?,
        duration_s: r.get(6)?,
        fps: r.get(7)?,
        width: r.get(8)?,
        height: r.get(9)?,
        size: r.get::<_, i64>(10)? as u64,
        trim_in: r.get(11)?,
        trim_out: r.get(12)?,
        thumbnail_path: thumb.map(PathBuf::from),
        source: r.get(14)?,
        source_id: r.get(15)?,
        tags: serde_json::from_str(&tags).map_err(|e| conv(16, Box::new(e)))?,
        markers: Vec::new(),
        starred: r.get(17)?,
        compressed: r.get(18)?,
        original_size: r.get::<_, Option<i64>>(19)?.map(|v| v as u64),
    })
}

fn load_markers(conn: &Connection, clip_id: &str) -> Result<Vec<Marker>> {
    let mut stmt = conn.prepare(
        "SELECT t_seconds, type, label FROM markers WHERE clip_id = ?1 ORDER BY t_seconds, id",
    )?;
    let rows = stmt.query_map([clip_id], |r| {
        Ok((r.get::<_, f64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (t_s, kind, label) = row?;
        out.push(Marker { t_s, kind: MarkerKind::parse(&kind)?, label });
    }
    Ok(out)
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<Clip>> {
    let clip = conn
        .query_row(&format!("{SELECT} WHERE c.id = ?1"), [id], row_to_clip)
        .optional()?;
    match clip {
        Some(mut c) => {
            c.markers = load_markers(conn, &c.id)?;
            Ok(Some(c))
        }
        None => Ok(None),
    }
}

/// List clips, newest first.
pub fn list(conn: &Connection, filter: ClipFilter) -> Result<Vec<Clip>> {
    let (sql, args) = list_query(&filter);
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        rusqlite::params_from_iter(args.iter().map(|a| a.as_ref())),
        row_to_clip,
    )?;
    let mut clips = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut markers = load_markers_for(conn, clips.iter().map(|c| c.id.as_str()))?;
    for c in &mut clips {
        c.markers = markers.remove(&c.id).unwrap_or_default();
    }
    Ok(clips)
}

/// Markers of many clips with one query per 500 clips (not one per clip).
fn load_markers_for<'a>(
    conn: &Connection,
    ids: impl Iterator<Item = &'a str>,
) -> Result<std::collections::HashMap<String, Vec<Marker>>> {
    let ids: Vec<&str> = ids.collect();
    let mut out: std::collections::HashMap<String, Vec<Marker>> = std::collections::HashMap::new();
    for chunk in ids.chunks(500) {
        let marks = vec!["?"; chunk.len()].join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT clip_id, t_seconds, type, label FROM markers WHERE clip_id IN ({marks}) ORDER BY clip_id, t_seconds, id"
        ))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?))
        })?;
        for row in rows {
            let (clip_id, t_s, kind, label) = row?;
            out.entry(clip_id).or_default().push(Marker { t_s, kind: MarkerKind::parse(&kind)?, label });
        }
    }
    Ok(out)
}

/// SQL and arguments of [`list`].
fn list_query(filter: &ClipFilter) -> (String, Vec<Box<dyn rusqlite::ToSql>>) {
    let mut sql = format!("{SELECT} WHERE 1=1");
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(g) = &filter.game {
        args.push(Box::new(g.clone()));
        sql.push_str(&format!(" AND g.name = ?{}", args.len()));
    }
    if filter.starred_only {
        sql.push_str(" AND c.starred = 1");
    }
    for word in filter.query.as_deref().unwrap_or("").split_whitespace() {
        let esc = word.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        args.push(Box::new(format!("%{esc}%")));
        let n = args.len();
        sql.push_str(&format!(
            " AND (c.title LIKE ?{n} ESCAPE '\\' OR c.description LIKE ?{n} ESCAPE '\\'
               OR c.tags_json LIKE ?{n} ESCAPE '\\' OR g.name LIKE ?{n} ESCAPE '\\'
               OR c.recorded_at LIKE ?{n} ESCAPE '\\'
               OR EXISTS (SELECT 1 FROM markers m WHERE m.clip_id = c.id
                          AND (m.label LIKE ?{n} ESCAPE '\\' OR m.type LIKE ?{n} ESCAPE '\\')))"
        ));
    }
    sql.push_str(" ORDER BY julianday(c.recorded_at) DESC, c.id");
    if let Some(l) = filter.limit {
        sql.push_str(&format!(" LIMIT {l}"));
    }
    (sql, args)
}

pub fn update_meta(
    conn: &Connection,
    id: &str,
    title: &str,
    description: &str,
    tags: &[String],
) -> Result<()> {
    let n = conn.execute(
        "UPDATE clips SET title = ?2, description = ?3, tags_json = ?4, updated_at = ?5 WHERE id = ?1",
        params![id, title, description, serde_json::to_string(tags)?, Utc::now().to_rfc3339()],
    )?;
    if n == 0 {
        return Err(anyhow!("clip {id} not found"));
    }
    Ok(())
}

/// Adds `markers` to the clip (existing ones stay).
pub fn add_markers(conn: &Connection, id: &str, markers: &[Marker]) -> Result<()> {
    for m in markers {
        conn.execute(
            "INSERT INTO markers (clip_id, t_seconds, type, label) VALUES (?1, ?2, ?3, ?4)",
            params![id, m.t_s, m.kind.as_str(), m.label],
        )?;
    }
    Ok(())
}

pub fn set_trim(
    conn: &Connection,
    id: &str,
    trim_in: Option<f64>,
    trim_out: Option<f64>,
) -> Result<()> {
    let n = conn.execute(
        "UPDATE clips SET trim_in = ?2, trim_out = ?3, updated_at = ?4 WHERE id = ?1",
        params![id, trim_in, trim_out, Utc::now().to_rfc3339()],
    )?;
    if n == 0 {
        return Err(anyhow!("clip {id} not found"));
    }
    Ok(())
}

/// Stores (or clears) the thumbnail path.
pub fn set_thumbnail(conn: &Connection, id: &str, path: Option<&std::path::Path>) -> Result<()> {
    conn.execute(
        "UPDATE clips SET thumbnail_path = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, path.map(|p| p.to_string_lossy().into_owned()), Utc::now().to_rfc3339()],
    )?;
    Ok(())
}

/// Points the clip at its file's new location (after a folder move).
pub fn set_path(conn: &Connection, id: &str, path: &std::path::Path) -> Result<()> {
    let n = conn.execute(
        "UPDATE clips SET path = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, path.to_string_lossy(), Utc::now().to_rfc3339()],
    )?;
    if n == 0 {
        return Err(anyhow!("clip {id} not found"));
    }
    Ok(())
}

pub fn set_starred(conn: &Connection, id: &str, starred: bool) -> Result<()> {
    let n = conn.execute(
        "UPDATE clips SET starred = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, starred, Utc::now().to_rfc3339()],
    )?;
    if n == 0 {
        return Err(anyhow!("clip {id} not found"));
    }
    Ok(())
}

/// After compression: the file's new size, and the clip is marked done.
/// Marks the clip as done by background compression with its current `size`.
/// `original_size`: the size before the file was replaced; `None` when it was
/// not replaced (failed, timed out, not smaller).
pub fn set_compressed(conn: &Connection, id: &str, size: u64, original_size: Option<u64>) -> Result<()> {
    conn.execute(
        "UPDATE clips SET compressed = 1, size = ?2, original_size = ?4, updated_at = ?3 WHERE id = ?1",
        params![id, size as i64, Utc::now().to_rfc3339(), original_size.map(|v| v as i64)],
    )?;
    Ok(())
}

/// Native and imported Moments clips not compressed yet, oldest first.
pub fn uncompressed_ids(conn: &Connection) -> Result<Vec<String>> {
    let mut st = conn.prepare(
        "SELECT id FROM clips WHERE compressed = 0 AND source IN ('native', 'moments') ORDER BY julianday(recorded_at)",
    )?;
    let ids = st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
    Ok(ids)
}

/// Delete the clip row (markers cascade). Does not touch files on disk. An
/// imported clip's source id is remembered so a later import skips it.
pub fn delete(conn: &Connection, id: &str) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    delete_rows(&tx, id)?;
    tx.commit()?;
    Ok(())
}

/// The statements of [`delete`], without a transaction of their own (the
/// caller runs them inside one).
pub fn delete_rows(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO removed_sources (source, source_id)
         SELECT source, source_id FROM clips WHERE id = ?1 AND source_id IS NOT NULL",
        [id],
    )?;
    conn.execute("DELETE FROM clips WHERE id = ?1", [id])?;
    Ok(())
}

/// Number of clips in the library and their total size on disk.
pub fn totals(conn: &Connection) -> Result<(u64, u64)> {
    let (n, bytes): (i64, i64) = conn.query_row("SELECT COUNT(*), COALESCE(SUM(size), 0) FROM clips", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok((n as u64, bytes as u64))
}

/// Paths of every clip in the library.
pub fn all_paths(conn: &Connection) -> Result<Vec<PathBuf>> {
    let mut st = conn.prepare("SELECT path FROM clips")?;
    let paths = st.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(paths.into_iter().map(PathBuf::from).collect())
}

/// Id of the native row for the file at `path` (compared
/// case-insensitively, as the unique index does), if any.
pub fn id_by_path(conn: &Connection, path: &std::path::Path) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT id FROM clips WHERE source = 'native' AND path = ?1 COLLATE NOCASE", [path.to_string_lossy()], |r| r.get(0))
        .optional()?)
}

/// True when the source clip is in the library or was removed from it.
pub fn exists_source(conn: &Connection, source: &str, source_id: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM clips WHERE source = ?1 AND source_id = ?2)
             OR EXISTS(SELECT 1 FROM removed_sources WHERE source = ?1 AND source_id = ?2)",
        params![source, source_id],
        |r| r.get(0),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_memory;
    use chrono::DateTime;

    fn sample(id: &str, source: &str, source_id: Option<&str>) -> Clip {
        Clip {
            id: id.into(),
            title: "Nice goal".into(),
            description: "desc".into(),
            path: r"E:\GameClips\a.mp4".into(),
            game: "Rocket League".into(),
            recorded_at: DateTime::parse_from_rfc3339("2026-11-20T20:15:00+01:00").unwrap(),
            duration_s: 61.5,
            fps: 60,
            width: 1920,
            height: 1080,
            size: 123456,
            trim_in: None,
            trim_out: None,
            thumbnail_path: Some(r"E:\t.jpg".into()),
            source: source.into(),
            source_id: source_id.map(|s| s.into()),
            tags: vec!["goal".into(), "fun".into()],
            starred: false,
            compressed: false,
            original_size: None,
            markers: vec![
                Marker { t_s: 10.0, kind: MarkerKind::Goal, label: "G".into() },
                Marker { t_s: 3.0, kind: MarkerKind::Laugh, label: "".into() },
            ],
        }
    }

    #[test]
    fn set_compressed_records_original_size_only_when_given() {
        let conn = open_memory().unwrap();
        insert(&conn, &sample("a", "native", None)).unwrap();
        let mut other = sample("b", "native", None);
        other.path = r"E:\GameClips\other.mp4".into();
        insert(&conn, &other).unwrap();
        set_compressed(&conn, "a", 400, Some(1000)).unwrap();
        set_compressed(&conn, "b", 1000, None).unwrap();
        let a = get(&conn, "a").unwrap().unwrap();
        assert_eq!((a.size, a.original_size, a.compressed), (400, Some(1000), true));
        let b = get(&conn, "b").unwrap().unwrap();
        assert_eq!((b.size, b.original_size, b.compressed), (1000, None, true));
    }

    #[test]
    fn new_clips_have_no_original_size() {
        let conn = open_memory().unwrap();
        insert(&conn, &sample("a", "native", None)).unwrap();
        assert_eq!(get(&conn, "a").unwrap().unwrap().original_size, None);
    }

    /// The library list (newest first) is served from an index, not by
    /// sorting every row on each refresh.
    #[test]
    fn list_order_uses_an_index() {
        let conn = open_memory().unwrap();
        let (sql, _) = list_query(&ClipFilter::default());
        let mut st = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let plan: Vec<String> = st.query_map([], |r| r.get::<_, String>(3)).unwrap().map(|r| r.unwrap()).collect();
        let plan = plan.join(" | ");
        assert!(plan.contains("idx_clips_recorded_jd"), "{plan}");
        assert!(!plan.contains("TEMP B-TREE"), "{plan}");
    }

    /// Markers are loaded for all listed clips at once and land on the right clip.
    #[test]
    fn list_attaches_each_clips_markers() {
        let conn = open_memory().unwrap();
        for (i, id) in ["a", "b", "c"].iter().enumerate() {
            let mut c = sample(id, "native", None);
            c.path = format!(r"E:\GameClips\{id}.mp4").into();
            c.recorded_at = DateTime::parse_from_rfc3339(&format!("2026-11-0{}T10:00:00+01:00", i + 1)).unwrap();
            c.markers = (0..i).map(|k| Marker { t_s: k as f64, kind: MarkerKind::Manual, label: format!("{id}{k}") }).collect();
            insert(&conn, &c).unwrap();
        }
        let got: Vec<(String, Vec<String>)> = list(&conn, ClipFilter::default())
            .unwrap()
            .into_iter()
            .map(|c| (c.id, c.markers.into_iter().map(|m| m.label).collect()))
            .collect();
        let want = |id: &str, m: &[&str]| (id.to_string(), m.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(got, vec![want("c", &["c0", "c1"]), want("b", &["b0"]), want("a", &[])]);
    }

    #[test]
    fn clip_insert_get_with_markers() {
        let conn = open_memory().unwrap();
        let c = sample("c1", "native", None);
        insert(&conn, &c).unwrap();
        let got = get(&conn, "c1").unwrap().unwrap();
        let mut expect = c.clone();
        expect.markers.sort_by(|a, b| a.t_s.partial_cmp(&b.t_s).unwrap());
        assert_eq!(got, expect);
        assert!(get(&conn, "zz").unwrap().is_none());
        assert_eq!(list(&conn, ClipFilter::default()).unwrap().len(), 1);
    }

    #[test]
    fn clip_update_meta_trim_delete() {
        let conn = open_memory().unwrap();
        insert(&conn, &sample("c1", "native", None)).unwrap();
        update_meta(&conn, "c1", "New", "d2", &["x".to_string()]).unwrap();
        set_trim(&conn, "c1", Some(1.0), Some(20.0)).unwrap();
        let g = get(&conn, "c1").unwrap().unwrap();
        assert_eq!(g.title, "New");
        assert_eq!(g.description, "d2");
        assert_eq!(g.tags, vec!["x".to_string()]);
        assert_eq!((g.trim_in, g.trim_out), (Some(1.0), Some(20.0)));
        delete(&conn, "c1").unwrap();
        assert!(get(&conn, "c1").unwrap().is_none());
        let n: i64 = conn.query_row("SELECT count(*) FROM markers", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn clip_unique_source_id() {
        let conn = open_memory().unwrap();
        insert(&conn, &sample("a", "moments", Some("m1"))).unwrap();
        assert!(exists_source(&conn, "moments", "m1").unwrap());
        assert!(!exists_source(&conn, "moments", "m2").unwrap());
        assert!(insert(&conn, &sample("b", "moments", Some("m1"))).is_err());
        // failed insert left nothing behind
        assert!(get(&conn, "b").unwrap().is_none());
    }

    /// Imported SteelSeries Moments clips are compressed like native ones;
    /// done clips are not queued again.
    #[test]
    fn uncompressed_ids_include_imported_moments_clips() {
        let conn = open_memory().unwrap();
        let mut native = sample("n", "native", None);
        native.path = r"E:\GameClips\n.mp4".into();
        let mut moments = sample("m", "moments", Some("m1"));
        moments.path = r"E:\Moments\m.mp4".into();
        let mut done = sample("d", "moments", Some("m2"));
        done.path = r"E:\Moments\d.mp4".into();
        for c in [&native, &moments, &done] {
            insert(&conn, c).unwrap();
        }
        set_compressed(&conn, "d", 1, None).unwrap();
        let mut ids = uncompressed_ids(&conn).unwrap();
        ids.sort();
        assert_eq!(ids, ["m", "n"]);
    }

    #[test]
    fn clip_source_check() {
        let conn = open_memory().unwrap();
        assert!(insert(&conn, &sample("a", "bogus", None)).is_err());
    }
}
