//! SteelSeries metadata embedded in Moments clip files.
//!
//! Moments writes each clip's details into the MP4 itself, as JSON split over
//! global tags `STEELSERIES_META0000`, `STEELSERIES_META0001`, ... (name,
//! description, trim points, tags, GameSense events, game). This survives
//! when GG's own database is gone, so clips found by the capture-folder
//! rescan (see `rescan`) get their Moments name, markers, tags and trim back.
//!
//! The tags are read with `ffmpeg -f ffmetadata`, which writes values
//! losslessly; the `ffmpeg -i` description mangles some chunk boundaries.
//! Files are only read, never written.
use crate::db::clips::{self, Clip, Marker};
use crate::ffmpeg_proc::{self, Priority, Run};
use crate::import_moments::{parse_markers, parse_tags, TRIM_EPS};
use crate::locks;
use anyhow::{anyhow, Result};
use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

const KEY_PREFIX: &str = "STEELSERIES_META";
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// `settings` row set once every library clip was checked (see [`backfill`]).
const BACKFILL_DONE_KEY: &str = "moments_meta_backfill_v1";

/// What Moments stored about a clip, ready for the library.
#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddedMeta {
    /// Name the user gave the clip; `None` for Moments' generated default
    /// ("Moments clip from May 05, 2026"), which ClipVault's own title beats.
    pub name: Option<String>,
    pub description: String,
    /// Game name as Moments knew it (`None` when empty).
    pub game: Option<String>,
    pub tags: Vec<String>,
    pub markers: Vec<Marker>,
    /// Trim in / out on the file's timeline, when Moments had the clip trimmed.
    pub trim: Option<(f64, f64)>,
}

/// Global `key=value` pairs of an `ffmetadata` file, unescaped. Stream and
/// chapter sections are left out.
pub fn parse_ffmetadata(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut entry = String::new();
    let mut key: Option<String> = None;
    let mut chars = text.chars();
    let mut at_line_start = true;
    let mut skip_line = false;
    loop {
        let c = chars.next();
        if at_line_start {
            match c {
                // A section header ends the global part.
                Some('[') => break,
                Some(';') | Some('#') => skip_line = true,
                _ => {}
            }
            at_line_start = false;
        }
        match c {
            None | Some('\n') => {
                if let Some(k) = key.take().filter(|_| !skip_line) {
                    out.push((k, std::mem::take(&mut entry).trim_end_matches('\r').to_string()));
                }
                entry.clear();
                key = None;
                skip_line = false;
                at_line_start = true;
                if c.is_none() {
                    break;
                }
            }
            Some('\\') => {
                if let Some(n) = chars.next() {
                    entry.push(n);
                }
            }
            Some('=') if key.is_none() => key = Some(std::mem::take(&mut entry)),
            Some(c) => entry.push(c),
        }
    }
    out
}

/// The joined `STEELSERIES_META####` JSON from `ffmetadata` text.
fn steelseries_json(text: &str) -> Option<Value> {
    let mut parts: Vec<(u32, String)> = parse_ffmetadata(text)
        .into_iter()
        .filter_map(|(k, v)| Some((k.strip_prefix(KEY_PREFIX)?.parse().ok()?, v)))
        .collect();
    if parts.is_empty() {
        return None;
    }
    parts.sort_by_key(|(i, _)| *i);
    let json: String = parts.into_iter().map(|(_, v)| v).collect();
    serde_json::from_str(&json).ok()
}

/// Moments' generated names end in an English date ("... from May 05, 2026",
/// whatever the UI language); user-given names practically never do.
pub fn is_default_name(name: &str) -> bool {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let mut words = name.split_whitespace().rev();
    let (Some(year), Some(day), Some(month)) = (words.next(), words.next(), words.next()) else { return false };
    let day = day.strip_suffix(',').unwrap_or("");
    MONTHS.contains(&month)
        && year.len() == 4
        && year.chars().all(|c| c.is_ascii_digit())
        && (1..=2).contains(&day.len())
        && day.chars().all(|c| c.is_ascii_digit())
}

/// Parses `ffmetadata` text; `duration_s` is the file's length (markers past
/// it are clamped). `None` when the file has no Moments metadata.
pub fn from_ffmetadata(text: &str, duration_s: f64) -> Option<EmbeddedMeta> {
    let j = steelseries_json(text)?;
    let s = |k: &str| j.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
    let f = |k: &str| j.get(k).and_then(Value::as_f64).filter(|v| v.is_finite()).unwrap_or(0.0);
    let full = match f("full_length") {
        l if l > 0.0 => l,
        _ => duration_s,
    };
    let (start, end) = (f("clip_start_point"), f("clip_end_point"));
    let trim = (start > TRIM_EPS || (end > 0.0 && end < full - TRIM_EPS))
        .then(|| (start.max(0.0), if end > 0.0 { end } else { full }))
        .filter(|(a, b)| b > a);
    let json_of = |k: &str| j.get(k).map(Value::to_string).unwrap_or_else(|| "[]".into());
    let name = s("name");
    Some(EmbeddedMeta {
        name: (!name.is_empty() && !is_default_name(&name)).then_some(name),
        description: s("description"),
        game: Some(s("last_game_name")).filter(|g| !g.is_empty()),
        tags: parse_tags(&json_of("tags")),
        markers: parse_markers(&json_of("gamesense_events"), full, 0.0, if duration_s > 0.0 { duration_s } else { full }),
        trim,
    })
}

/// Reads the Moments metadata of `path`. `Ok(None)`: the file has none;
/// `Err`: FFmpeg did not run or could not read the file.
pub fn read(ffmpeg: &Path, path: &Path, duration_s: f64) -> Result<Option<EmbeddedMeta>> {
    // `pipe:2` puts the metadata on stderr, the only stream `run` captures;
    // `-v error` keeps FFmpeg's own lines out of it.
    use std::ffi::OsStr;
    let mut args: Vec<&OsStr> = ["-v", "error", "-i"].map(OsStr::new).to_vec();
    args.push(path.as_os_str());
    args.extend(["-vn", "-an", "-sn", "-dn", "-f", "ffmetadata", "pipe:2"].map(OsStr::new));
    match ffmpeg_proc::run(ffmpeg, &args, READ_TIMEOUT, Priority::Normal, &|| false, |_, _| {}) {
        Run::Finished { success: true, stderr } => Ok(from_ffmetadata(&stderr, duration_s)),
        Run::Finished { success: false, stderr } => {
            Err(anyhow!("reading metadata of {} failed: {}", path.display(), stderr.lines().last().unwrap_or("")))
        }
        Run::TimedOut | Run::Busy => Err(anyhow!("reading metadata of {} timed out", path.display())),
    }
}

/// Title the rescan gives a clip it finds (see `rescan`); a clip still titled
/// like this was never renamed.
pub fn auto_title(game: &str, recorded: &chrono::DateTime<chrono::FixedOffset>) -> String {
    format!("{game} {}", recorded.format("%d.%m.%Y %H:%M"))
}

/// Fills in `clip` from `meta` where the library has nothing of its own:
/// the name while the title is still the rescan's, description, tags,
/// markers and trim while empty. Returns whether anything changed.
pub fn apply(clip: &mut Clip, meta: &EmbeddedMeta) -> bool {
    let mut changed = false;
    let auto = auto_title(&clip.game, &clip.recorded_at);
    if let Some(name) = meta.name.as_ref().filter(|n| clip.title == auto && clip.title != **n) {
        clip.title = name.clone();
        changed = true;
    }
    if clip.description.is_empty() && !meta.description.is_empty() {
        clip.description = meta.description.clone();
        changed = true;
    }
    if clip.tags.is_empty() && !meta.tags.is_empty() {
        clip.tags = meta.tags.clone();
        changed = true;
    }
    if clip.markers.is_empty() && !meta.markers.is_empty() {
        clip.markers = meta.markers.clone();
        changed = true;
    }
    let untrimmed = clip.trim_in.is_none() && clip.trim_out.is_none();
    if let Some((a, b)) = meta.trim.filter(|(_, b)| untrimmed && *b <= clip.duration_s + 1.0) {
        (clip.trim_in, clip.trim_out) = (Some(a), Some(b.min(clip.duration_s)));
        changed = true;
    }
    changed
}

fn backfill_done(conn: &Connection) -> bool {
    conn.query_row("SELECT value FROM settings WHERE key = ?1", [BACKFILL_DONE_KEY], |r| r.get::<_, String>(0))
        .optional()
        .ok()
        .flatten()
        .is_some()
}

/// Whether [`backfill`] still has to run on this library.
pub fn backfill_pending(db: &Mutex<Connection>) -> bool {
    !backfill_done(&locks::lock(db))
}

/// Gives clips the library found on disk (`native` rows) the Moments
/// metadata embedded in their files (see [`apply`]). Runs until it has
/// checked every clip once, then never again. `on_updated` gets each changed
/// clip. Returns how many changed. Blocking.
pub fn backfill(db: &Mutex<Connection>, ffmpeg: &Path, on_updated: &dyn Fn(&Clip)) -> Result<usize> {
    if backfill_done(&locks::lock(db)) {
        return Ok(0);
    }
    let candidates: Vec<Clip> = clips::list(&locks::lock(db), clips::ClipFilter::default())?
        .into_iter()
        .filter(|c| c.source == "native" && c.path.is_file())
        .collect();
    let (mut updated, mut failed) = (0, 0);
    for c in candidates {
        let meta = match read(ffmpeg, &c.path, c.duration_s) {
            Ok(Some(m)) => m,
            Ok(None) => continue,
            Err(e) => {
                tracing::warn!("[moments-meta] {e:#}");
                failed += 1;
                continue;
            }
        };
        let conn = locks::lock(db);
        // Fresh row: the user may have edited the clip while FFmpeg ran.
        let Some(mut clip) = clips::get(&conn, &c.id)? else { continue };
        let before = clip.clone();
        if !apply(&mut clip, &meta) {
            continue;
        }
        let tx = conn.unchecked_transaction()?;
        clips::update_meta(&tx, &clip.id, &clip.title, &clip.description, &clip.tags)?;
        if before.markers.is_empty() && !clip.markers.is_empty() {
            clips::add_markers(&tx, &clip.id, &clip.markers)?;
        }
        if (before.trim_in, before.trim_out) != (clip.trim_in, clip.trim_out) {
            clips::set_trim(&tx, &clip.id, clip.trim_in, clip.trim_out)?;
        }
        tx.commit()?;
        let saved = clips::get(&conn, &clip.id)?;
        drop(conn);
        if let Some(saved) = saved {
            on_updated(&saved);
        }
        updated += 1;
    }
    // A failed read is tried again on the next start; files without Moments
    // metadata are not.
    if failed == 0 {
        locks::lock(db).execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
            [BACKFILL_DONE_KEY, &chrono::Utc::now().to_rfc3339()],
        )?;
    }
    tracing::info!("[moments-meta] backfill: {updated} clip(s) updated, {failed} unreadable");
    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::clips::MarkerKind;

    /// Moments' JSON split into 255-char chunks like the real files, with a
    /// `\\` escape cut in half, written the way `-f ffmetadata` escapes them.
    fn ffmetadata(json: &str) -> String {
        let chars: Vec<char> = json.chars().collect();
        let mut out = String::from(";FFMETADATA1\nmajor_brand=isom\n");
        // Reverse order: FFmpeg does not sort the keys either.
        let chunks: Vec<String> = chars.chunks(255).map(|c| c.iter().collect()).collect();
        for (i, chunk) in chunks.iter().enumerate().rev() {
            let esc = chunk.replace('\\', "\\\\").replace('=', "\\=").replace(';', "\\;").replace('#', "\\#");
            out.push_str(&format!("{KEY_PREFIX}{i:04}={esc}\n"));
        }
        out.push_str("encoder=Lavf62.3.100\n[STREAM]\nSTEELSERIES_META0000=junk\n");
        out
    }

    fn sample_json(name: &str, start: f64, end: f64) -> String {
        let pad = "x".repeat(300);
        format!(
            r#"{{"name":"{name}","description":"a=b; #c","clip_start_point":{start},"clip_end_point":{end},"full_length":58.166,"last_game_name":"Counter-Strike 2","thumbnail_path":"I:\\GameClips\\thumbs\\{pad}.jpg","gamesense_events":[{{"localized_name_key":"moments.timelineEvents.savedClip","unlocalized_display_name":"","clip_timestamp":30.5}},{{"localized_name_key":"x.kill","unlocalized_display_name":"Kill","clip_timestamp":12.0}}],"tags":["clutch"]}}"#
        )
    }

    #[test]
    fn parses_escaped_ffmetadata() {
        let text = ";FFMETADATA1\na=1\\=2\\;\\#\\\\\nmulti=line\\\nnext\n#comment=x\nb=\n[CHAPTER]\nc=3\n";
        assert_eq!(
            parse_ffmetadata(text),
            vec![("a".into(), "1=2;#\\".into()), ("multi".into(), "line\nnext".into()), ("b".into(), "".into())]
        );
    }

    #[test]
    fn reads_chunked_moments_json() {
        let m = from_ffmetadata(&ffmetadata(&sample_json("Nice redirect", 0.0, 58.166)), 58.2).unwrap();
        assert_eq!(m.name.as_deref(), Some("Nice redirect"));
        assert_eq!(m.description, "a=b; #c");
        assert_eq!(m.game.as_deref(), Some("Counter-Strike 2"));
        assert_eq!(m.tags, vec!["clutch".to_string()]);
        assert_eq!(m.trim, None);
        assert_eq!(m.markers.len(), 2);
        assert_eq!((m.markers[0].t_s, m.markers[0].kind, m.markers[0].label.as_str()), (12.0, MarkerKind::Imported, "Kill"));
        assert_eq!((m.markers[1].t_s, m.markers[1].kind), (30.5, MarkerKind::Saved));
    }

    #[test]
    fn default_names_and_trim() {
        let m = from_ffmetadata(&ffmetadata(&sample_json("Momentaufnahmen vom May 05, 2026", 2.0, 40.0)), 58.2).unwrap();
        assert_eq!(m.name, None);
        assert_eq!(m.trim, Some((2.0, 40.0)));
        assert!(is_default_name("Moments Desktop clip from Oct 23, 2026"));
        assert!(!is_default_name("ARSCH SAFE"));
        assert!(!is_default_name("Ace on May 5th"));
        assert_eq!(from_ffmetadata(";FFMETADATA1\nencoder=x\n", 10.0), None);
    }

    fn clip(title: &str) -> Clip {
        let recorded = chrono::DateTime::parse_from_rfc3339("2026-05-05T19:31:00+02:00").unwrap();
        Clip {
            id: "a".into(),
            title: title.into(),
            description: String::new(),
            path: r"D:\GameClips\Assetto-Corsa__2026-05-05__19-31-00.mp4".into(),
            game: "Assetto Corsa".into(),
            recorded_at: recorded,
            duration_s: 58.2,
            fps: 30,
            width: 1920,
            height: 1080,
            size: 1,
            trim_in: None,
            trim_out: None,
            thumbnail_path: None,
            source: "native".into(),
            source_id: None,
            tags: Vec::new(),
            markers: Vec::new(),
            starred: false,
            compressed: true,
            original_size: None,
        }
    }

    /// Only what the library has nothing of its own for is taken over.
    #[test]
    fn apply_keeps_user_edits() {
        let meta = from_ffmetadata(&ffmetadata(&sample_json("Wo Molotov", 1.0, 30.0)), 58.2).unwrap();
        let mut c = clip("Assetto Corsa 05.05.2026 19:31");
        assert!(apply(&mut c, &meta));
        assert_eq!(c.title, "Wo Molotov");
        assert_eq!((c.trim_in, c.trim_out), (Some(1.0), Some(30.0)));
        assert_eq!(c.markers.len(), 2);
        assert!(!apply(&mut c, &meta), "second run changes nothing");

        let mut renamed = clip("My title");
        renamed.tags = vec!["mine".into()];
        renamed.trim_in = Some(5.0);
        apply(&mut renamed, &meta);
        assert_eq!(renamed.title, "My title");
        assert_eq!(renamed.tags, vec!["mine".to_string()]);
        assert_eq!((renamed.trim_in, renamed.trim_out), (Some(5.0), None));
    }

    #[test]
    fn backfill_runs_once() {
        let db = Mutex::new(crate::db::open_memory().unwrap());
        assert!(backfill_pending(&db));
        // No clips: nothing to read, the run counts as complete.
        assert_eq!(backfill(&db, Path::new("no-ffmpeg.exe"), &|_| panic!("no clips")).unwrap(), 0);
        assert!(!backfill_pending(&db));
    }

    /// A file FFmpeg cannot read leaves the backfill pending for the next start.
    #[test]
    fn failed_reads_keep_backfill_pending() {
        let dir = tempfile::tempdir().unwrap();
        let db = Mutex::new(crate::db::open_memory().unwrap());
        let mut c = clip("Assetto Corsa 05.05.2026 19:31");
        c.path = dir.path().join("a.mp4");
        std::fs::write(&c.path, b"x").unwrap();
        clips::insert(&locks::lock(&db), &c).unwrap();
        assert_eq!(backfill(&db, Path::new("no-ffmpeg.exe"), &|_| panic!("nothing read")).unwrap(), 0);
        assert!(backfill_pending(&db));
    }
}
