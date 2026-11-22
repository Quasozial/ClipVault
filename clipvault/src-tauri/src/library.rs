//! Library commands' logic as plain functions (the Tauri handlers in
//! `commands.rs` are thin wrappers), so tests call them with temp DBs/files.

use crate::db::clips::{self, Clip, ClipFilter, Marker};
use crate::db::games::{self, Game};
use crate::settings::{Encoder, Settings};
use anyhow::{anyhow, bail, Context, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

// ----- listing / editing -----

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortOrder {
    #[default]
    Newest,
    Oldest,
}

/// `{ game?: string, search?: string, starred?: bool, sort: "newest" | "oldest" }`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ListFilter {
    /// Exact game name.
    pub game: Option<String>,
    /// Words that must all match (see [`ClipFilter::query`]); blank = no filter.
    pub search: Option<String>,
    /// Only starred clips.
    pub starred: bool,
    pub sort: SortOrder,
}

pub fn list_clips(conn: &Connection, filter: &ListFilter) -> Result<Vec<Clip>> {
    let query = filter.search.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let game = filter.game.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let mut v = clips::list(conn, ClipFilter { game, query, starred_only: filter.starred, limit: None })?;
    if filter.sort == SortOrder::Oldest {
        v.reverse();
    }
    Ok(v)
}

pub fn get_clip(conn: &Connection, id: &str) -> Result<Clip> {
    clips::get(conn, id)?.ok_or_else(|| anyhow!("Clip not found"))
}

pub fn update_clip_meta(conn: &Connection, id: &str, title: &str, description: &str, tags: &[String]) -> Result<Clip> {
    let title = title.trim();
    if title.is_empty() {
        bail!("The title must not be empty");
    }
    let mut clean: Vec<String> = Vec::new();
    for t in tags.iter().map(|t| t.trim()).filter(|t| !t.is_empty()) {
        if !clean.iter().any(|c| c.eq_ignore_ascii_case(t)) {
            clean.push(t.to_string());
        }
    }
    clips::update_meta(conn, id, title, description, &clean)?;
    get_clip(conn, id)
}

/// Stores the trim range (`None` = start / end of the clip; both `None` clears it).
pub fn set_trim(conn: &Connection, id: &str, trim_in: Option<f64>, trim_out: Option<f64>) -> Result<Clip> {
    let clip = get_clip(conn, id)?;
    for v in [trim_in, trim_out].into_iter().flatten() {
        if !v.is_finite() || v < 0.0 {
            bail!("Trim points must be zero or more seconds");
        }
    }
    if let Some(i) = trim_in {
        if i >= clip.duration_s {
            bail!("The trim start is past the end of the clip");
        }
    }
    if let Some(o) = trim_out {
        if o > clip.duration_s + 0.05 {
            bail!("The trim end is past the end of the clip");
        }
    }
    if let (Some(i), Some(o)) = (trim_in, trim_out) {
        if i >= o {
            bail!("The trim start must be before the trim end");
        }
    }
    clips::set_trim(conn, id, trim_in, trim_out)?;
    get_clip(conn, id)
}

pub fn set_starred(conn: &Connection, id: &str, starred: bool) -> Result<Clip> {
    clips::set_starred(conn, id, starred)?;
    get_clip(conn, id)
}

// ----- delete -----

/// True for files ClipVault does not own (imported from SteelSeries Moments):
/// they are never written, moved or deleted.
pub fn is_foreign(clip: &Clip) -> bool {
    clip.source == "moments"
}

/// Moves the clip file to the Recycle Bin and removes the row and thumbnail.
/// An imported Moments clip only leaves the library; its file stays put.
pub fn delete_clip(conn: &Connection, id: &str) -> Result<()> {
    delete_clip_with(conn, id, &|p| trash::delete(p).map_err(|e| e.to_string()))
}

/// [`delete_clip`] with the trash operation injected. A file that is already
/// gone still removes the row; any other trash failure keeps the row. The row
/// is deleted first inside a transaction that only commits once the file is in
/// the Recycle Bin, so file and row never disagree.
pub fn delete_clip_with(conn: &Connection, id: &str, trasher: &dyn Fn(&Path) -> Result<(), String>) -> Result<()> {
    let clip = get_clip(conn, id)?;
    let tx = conn.unchecked_transaction()?;
    clips::delete_rows(&tx, id).context("Could not remove the clip from the library")?;
    if !is_foreign(&clip) && clip.path.exists() {
        if let Err(e) = trasher(&clip.path) {
            if clip.path.exists() {
                bail!("Could not move {} to the Recycle Bin: {e}", clip.path.display()); // rolls back
            }
        }
    }
    tx.commit()?;
    if let Some(t) = &clip.thumbnail_path {
        let _ = std::fs::remove_file(t);
    }
    Ok(())
}

// ----- thumbnails -----

/// One FFmpeg thumbnail at a time, however fast the grid asks.
static THUMB_LOCK: Mutex<()> = Mutex::new(());

/// Async front of thumbnail generation: requests wait on a semaphore (async, no
/// thread parked) and only the one holding the permit occupies a blocking
/// thread, so a grid asking for hundreds cannot exhaust the blocking pool.
/// Failed clips are remembered (until restart) so they do not rerun FFmpeg on
/// every grid render.
pub struct ThumbQueue {
    permit: tokio::sync::Semaphore,
    failed: Mutex<Vec<String>>,
}

impl ThumbQueue {
    pub const fn new() -> Self {
        ThumbQueue { permit: tokio::sync::Semaphore::const_new(1), failed: Mutex::new(Vec::new()) }
    }

    /// Runs `job` (which should itself return an existing thumbnail without
    /// work, as [`ensure_thumbnail`] does) one at a time. `None` on failure
    /// (remembered) or when the job has nothing to show.
    pub async fn run<F>(&self, id: &str, job: F) -> Option<String>
    where
        F: FnOnce() -> Result<Option<String>> + Send + 'static,
    {
        if self.failed.lock().unwrap().iter().any(|f| f == id) {
            return None;
        }
        let _permit = self.permit.acquire().await.ok()?;
        // Re-check: an earlier queued request for this id may have failed meanwhile.
        if self.failed.lock().unwrap().iter().any(|f| f == id) {
            return None;
        }
        match tokio::task::spawn_blocking(job).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                tracing::warn!("thumbnail for {id}: {e:#}");
                self.failed.lock().unwrap().push(id.to_string());
                None
            }
            Err(e) => {
                tracing::warn!("thumbnail task for {id}: {e}");
                None
            }
        }
    }
}

/// The app-wide thumbnail queue used by the `ensure_thumbnail` command.
pub static THUMBS: ThumbQueue = ThumbQueue::new();

fn thumb_time(duration_s: f64) -> f64 {
    (duration_s * 0.25).clamp(0.0, 5.0)
}

fn thumb_path(thumbs_dir: &Path, id: &str) -> PathBuf {
    thumbs_dir.join(format!("{id}.jpg"))
}

fn generate_thumb_locked(ffmpeg: &Path, video: &Path, out: &Path, at_s: f64) -> Result<()> {
    if let Some(d) = out.parent() {
        std::fs::create_dir_all(d)?;
    }
    crate::thumbs::generate_thumbnail(ffmpeg, video, out, at_s)
}

/// The clip's thumbnail path, generating and storing one if it has none (or
/// the file vanished). `Ok(None)` when the video itself is missing. Calls queue
/// up behind [`THUMB_LOCK`]; the DB lock is never held while waiting or encoding.
pub fn ensure_thumbnail(db: &Mutex<Connection>, ffmpeg: &Path, thumbs_dir: &Path, id: &str) -> Result<Option<String>> {
    let have = |c: &Clip| c.thumbnail_path.clone().filter(|p| p.is_file());
    let clip = get_clip(&crate::locks::lock(&db), id)?;
    if let Some(p) = have(&clip) {
        return Ok(Some(p.to_string_lossy().into_owned()));
    }
    if !clip.path.is_file() {
        return Ok(None);
    }
    let _guard = THUMB_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Someone queued ahead of us may have made it already.
    let clip = get_clip(&crate::locks::lock(&db), id)?;
    if let Some(p) = have(&clip) {
        return Ok(Some(p.to_string_lossy().into_owned()));
    }
    let out = thumb_path(thumbs_dir, id);
    generate_thumb_locked(ffmpeg, &clip.path, &out, thumb_time(clip.duration_s))?;
    clips::set_thumbnail(&crate::locks::lock(&db), id, Some(&out))?;
    Ok(Some(out.to_string_lossy().into_owned()))
}

/// Where a clip's hover-scrub strip is cached.
pub fn strip_path(thumbs_dir: &Path, id: &str) -> PathBuf {
    thumbs_dir.join(format!("{id}.strip.jpg"))
}

/// The clip's hover-scrub strip (see [`crate::thumbs::strip_args`]), made on
/// first use and cached next to the thumbnails. `Ok(None)` when the video is
/// missing. Shares [`THUMB_LOCK`] with thumbnails: one FFmpeg at a time.
pub fn ensure_scrub_strip(db: &Mutex<Connection>, ffmpeg: &Path, thumbs_dir: &Path, id: &str) -> Result<Option<String>> {
    let out = strip_path(thumbs_dir, id);
    let clip = get_clip(&crate::locks::lock(db), id)?;
    if !clip.path.is_file() {
        return Ok(None);
    }
    if out.is_file() {
        return Ok(Some(out.to_string_lossy().into_owned()));
    }
    let _guard = THUMB_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if out.is_file() {
        return Ok(Some(out.to_string_lossy().into_owned()));
    }
    if let Some(d) = out.parent() {
        std::fs::create_dir_all(d)?;
    }
    let (start, span) = crate::thumbs::strip_window(clip.duration_s, clip.trim_in, clip.trim_out);
    crate::thumbs::generate_strip(ffmpeg, &clip.path, &out, start, span)?;
    Ok(Some(out.to_string_lossy().into_owned()))
}

/// Drops a cached strip (trim changed, clip deleted). Errors are ignored.
pub fn forget_strip(thumbs_dir: &Path, id: &str) {
    let _ = std::fs::remove_file(strip_path(thumbs_dir, id));
}

// ----- export -----

/// FFmpeg arguments (without the binary) for cutting `src` to `[trim_in, trim_out]`
/// into `tmp` (written as MP4 despite the `.tmp` extension). `reencode`: the
/// encoder for a frame-exact cut (`None`: streams copied, cuts on keyframes).
pub fn export_args(src: &Path, tmp: &Path, trim_in: f64, trim_out: f64, reencode: Option<Encoder>) -> Vec<std::ffi::OsString> {
    let mut a: Vec<std::ffi::OsString> = ["-y", "-hide_banner", "-loglevel", "error", "-ss"].map(Into::into).into();
    a.push(format!("{trim_in:.3}").into());
    a.push("-to".into());
    a.push(format!("{trim_out:.3}").into());
    a.push("-i".into());
    a.push(src.into());
    a.extend(["-map", "0", "-c", "copy"].map(std::ffi::OsString::from));
    if let Some(enc) = reencode {
        a.extend(crate::export::video_rc(enc, None).into_iter().map(std::ffi::OsString::from));
    }
    a.extend(["-movflags", "+faststart", "-f", "mp4"].map(std::ffi::OsString::from));
    a.push(tmp.into());
    a
}

/// Timeout for an export of `duration_s` seconds of video: generous (a CPU
/// re-encode of a long clip is slow), but a hung FFmpeg never spins forever.
pub fn export_timeout(duration_s: f64) -> std::time::Duration {
    std::time::Duration::from_secs(300) + std::time::Duration::from_secs_f64(duration_s.max(0.0) * 20.0)
}

/// Runs FFmpeg to completion inside the kill-on-close job, killed after
/// `timeout`. Blocking.
pub(crate) fn run_ffmpeg(ffmpeg: &Path, args: &[std::ffi::OsString], timeout: std::time::Duration) -> Result<()> {
    crate::ffmpeg_proc::run_ok(ffmpeg, args, timeout, "export").map(|_| ())
}

/// Folder for files made from `clip`: next to it, unless it is a Moments file
/// (that folder belongs to SteelSeries), then the capture folder.
pub fn output_dir_for(clip: &Clip, capture_dir: &Path) -> Result<PathBuf> {
    if is_foreign(clip) {
        std::fs::create_dir_all(capture_dir)
            .with_context(|| format!("creating {}", capture_dir.display()))?;
        return Ok(capture_dir.to_path_buf());
    }
    Ok(clip.path.parent().ok_or_else(|| anyhow!("The clip path has no folder"))?.to_path_buf())
}

/// Cuts the clip's trim range into `<stem>__trim.mp4` next to it and adds it to
/// the library as a new clip. `accurate`: re-encode the video frame-exact
/// with this encoder setting (`Auto`: the best H.264 encoder this PC has);
/// `None`: streams are copied (cuts land on keyframes). Blocking:
/// call from `spawn_blocking`. Returns the new file's path.
pub fn export_trimmed(
    db: &Mutex<Connection>,
    ffmpeg: &Path,
    thumbs_dir: &Path,
    capture_dir: &Path,
    id: &str,
    accurate: Option<Encoder>,
) -> Result<String> {
    let orig = get_clip(&crate::locks::lock(&db), id)?;
    let (t_in, t_out) = match (orig.trim_in, orig.trim_out) {
        (None, None) => bail!("Set a trim range first"),
        (i, o) => (i.unwrap_or(0.0), o.unwrap_or(orig.duration_s)),
    };
    if !(t_out > t_in) {
        bail!("The trim start must be before the trim end");
    }
    if !orig.path.is_file() {
        bail!("The clip file no longer exists: {}", orig.path.display());
    }
    let dir = output_dir_for(&orig, capture_dir)?;
    let dir = dir.as_path();
    let stem = orig.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "clip".into());
    let final_path = crate::save::unique_clip_path(dir, &format!("{stem}__trim.mp4"));
    let mut tmp = final_path.clone().into_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);

    let reencode = accurate.map(|wanted| crate::export::export_encoder(ffmpeg, wanted));
    let result = run_ffmpeg(ffmpeg, &export_args(&orig.path, &tmp, t_in, t_out, reencode), export_timeout(t_out - t_in))
        .and_then(|()| std::fs::rename(&tmp, &final_path).context("moving the exported file into place"));
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    let new_id = uuid::Uuid::new_v4().to_string();
    let duration_s = t_out - t_in;
    let thumb = thumb_path(thumbs_dir, &new_id);
    let thumbnail_path = {
        let _g = THUMB_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        match generate_thumb_locked(ffmpeg, &final_path, &thumb, thumb_time(duration_s)) {
            Ok(()) => Some(thumb),
            Err(e) => {
                tracing::warn!("[export] thumbnail failed: {e:#}");
                None
            }
        }
    };
    let new_clip = Clip {
        id: new_id,
        title: format!("{} (trim)", orig.title),
        description: orig.description.clone(),
        path: final_path.clone(),
        game: orig.game.clone(),
        recorded_at: orig.recorded_at,
        duration_s,
        fps: orig.fps,
        width: orig.width,
        height: orig.height,
        size: std::fs::metadata(&final_path).map(|m| m.len()).unwrap_or(0),
        trim_in: None,
        trim_out: None,
        thumbnail_path,
        source: "native".into(),
        source_id: None,
        tags: orig.tags.clone(),
        starred: false,
        compressed: false,
        original_size: None,
        markers: orig
            .markers
            .iter()
            .filter(|m| m.t_s >= t_in && m.t_s <= t_out)
            .map(|m| Marker { t_s: m.t_s - t_in, kind: m.kind, label: m.label.clone() })
            .collect(),
    };
    if let Err(e) = clips::insert(&crate::locks::lock(&db), &new_clip) {
        let _ = std::fs::remove_file(&final_path);
        if let Some(t) = &new_clip.thumbnail_path {
            let _ = std::fs::remove_file(t);
        }
        return Err(e.context("adding the exported clip to the library"));
    }
    Ok(final_path.to_string_lossy().into_owned())
}

// ----- settings -----

/// Checks a settings object coming from the UI before it is applied.
pub fn validate_settings(s: &Settings) -> Result<(), String> {
    if s.fps != 30 && s.fps != 60 {
        return Err("Frame rate must be 30 or 60 fps".into());
    }
    if s.max_save_length_s > 600 {
        return Err("Maximum save length must be 600 seconds or less".into());
    }
    if !(5..=s.max_save_length_s).contains(&s.save_length_s) {
        return Err(format!("Save length must be between 5 and {} seconds", s.max_save_length_s));
    }
    if !(2000..=150_000).contains(&s.bitrate_kbps) {
        return Err("Bitrate must be between 2000 and 150000 kbps".into());
    }
    if s.capture_dir.as_os_str().to_string_lossy().trim().is_empty() {
        return Err("The clip folder must not be empty".into());
    }
    // Relative would resolve against the working directory, which differs
    // between an autostart and a normal start.
    if !s.capture_dir.is_absolute() {
        return Err(r"The clip folder must be a full path, e.g. E:\GameClips".into());
    }
    if s.buffer_dir.as_os_str().to_string_lossy().trim().is_empty() {
        return Err("Buffer folder must not be empty".into());
    }
    if !s.buffer_dir.is_absolute() {
        return Err(r"Buffer folder must be a full path, e.g. C:\ClipVault\buffer".into());
    }
    Ok(())
}

/// Folders the asset protocol must serve: capture dir, thumbnails and every
/// distinct parent folder of a library clip (imported clips live elsewhere).
pub fn asset_dirs(settings: &Settings, thumbs_dir: &Path, all_clips: &[Clip]) -> Vec<PathBuf> {
    asset_dirs_for_paths(settings, thumbs_dir, all_clips.iter().map(|c| c.path.clone()))
}

/// Entries for the asset protocol scope: `(dir, recursive)`. Clips and
/// thumbnails are stored flat, so nothing is recursive (a clip at `E:\x.mp4`
/// must not expose all of `E:\`).
pub fn asset_scope_entries(settings: &Settings, thumbs_dir: &Path, clip_paths: impl IntoIterator<Item = PathBuf>) -> Vec<(PathBuf, bool)> {
    asset_dirs_for_paths(settings, thumbs_dir, clip_paths).into_iter().map(|d| (d, false)).collect()
}

/// [`asset_dirs`] from bare clip file paths.
pub fn asset_dirs_for_paths(settings: &Settings, thumbs_dir: &Path, clip_paths: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut add = |p: &Path| {
        if !p.as_os_str().is_empty() && !out.iter().any(|o| o == p) {
            out.push(p.to_path_buf());
        }
    };
    add(&settings.capture_dir);
    add(thumbs_dir);
    for p in clip_paths {
        if let Some(d) = p.parent() {
            add(d);
        }
    }
    out
}

/// The file Explorer may show for `reveal_in_folder`: an existing file in the
/// capture folder (exports included) or a library clip. Anything else is
/// refused, so the webview cannot point Explorer at arbitrary paths.
pub fn revealable(path: &Path, capture_dir: &Path, clip_paths: &[PathBuf]) -> Result<PathBuf> {
    // Canonical paths, so `..`, links and case differences cannot slip past;
    // dunce drops the `\\?\` prefix Explorer may not accept.
    let file = dunce::canonicalize(path).with_context(|| format!("{} does not exist", path.display()))?;
    if !file.is_file() {
        bail!("{} is not a file", path.display());
    }
    let in_capture = dunce::canonicalize(capture_dir).is_ok_and(|d| file.starts_with(d));
    let is_clip = || clip_paths.iter().any(|c| dunce::canonicalize(c).is_ok_and(|c| c == file));
    if in_capture || is_clip() {
        Ok(file)
    } else {
        bail!("{} is not a ClipVault file", path.display())
    }
}

// ----- games / processes -----

/// A running program the user can add as a game.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunningProcess {
    pub pid: u32,
    /// Suggested game name (exe name without `.exe`).
    pub name: String,
    /// Exe file name, e.g. `RocketLeague.exe`.
    pub exe: String,
    /// Full exe path when readable.
    pub path: Option<String>,
}

/// `RocketLeague.exe` -> `RocketLeague`; first letter upper-cased.
pub fn game_name_from_exe(exe: &str) -> String {
    let base = exe.trim();
    let base = if base.len() >= 4 && base[base.len() - 4..].eq_ignore_ascii_case(".exe") { &base[..base.len() - 4] } else { base };
    let mut c = base.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

/// Adds the process as a game, merging its exe name into an existing game of
/// the same name. Returns the stored game.
pub fn add_game(conn: &Connection, p: &RunningProcess) -> Result<Game> {
    let name = if p.name.trim().is_empty() { game_name_from_exe(&p.exe) } else { p.name.trim().to_string() };
    if name.is_empty() || p.exe.trim().is_empty() {
        bail!("That process has no usable name");
    }
    let existing = games::list(conn)?.into_iter().find(|g| g.name == name);
    let mut exes = existing.as_ref().map(|g| g.exe_names.clone()).unwrap_or_default();
    if !exes.iter().any(|e| e.eq_ignore_ascii_case(&p.exe)) {
        exes.push(p.exe.clone());
    }
    let path = p.path.as_ref().map(PathBuf::from).or_else(|| existing.and_then(|g| g.path));
    let id = games::upsert_by_name(conn, &name, &exes, path.as_deref())?;
    games::list(conn)?.into_iter().find(|g| g.id == id).ok_or_else(|| anyhow!("game vanished after insert"))
}

fn file_name_of(p: &Path) -> Option<String> {
    p.file_name().map(|n| n.to_string_lossy().into_owned())
}

fn process_of(p: &sysinfo::Process, pid: u32) -> Option<RunningProcess> {
    let path = p.exe().map(|e| e.to_string_lossy().into_owned());
    let exe = p.exe().and_then(file_name_of).unwrap_or_else(|| p.name().to_string_lossy().into_owned());
    if exe.is_empty() {
        return None;
    }
    Some(RunningProcess { pid, name: game_name_from_exe(&exe), exe, path })
}

fn snapshot() -> Vec<(u32, RunningProcess)> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, UpdateKind};
    let Ok(mut sys) = sysinfo::System::new() else { return Vec::new() };
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_exe(UpdateKind::OnlyIfNotSet),
    );
    sys.processes()
        .iter()
        .filter_map(|(pid, p)| {
            let pid = pid.as_u32();
            (pid != 0 && pid != 4).then(|| process_of(p, pid)).flatten().map(|r| (pid, r))
        })
        .collect()
}

/// Processes of the current user that own a visible, uncloaked top-level
/// window with a title (what the user sees in the taskbar), one entry per exe,
/// sorted by name. Own process and `explorer.exe` are left out.
pub fn list_running_processes() -> Vec<RunningProcess> {
    let visible = windows_with_visible_ui();
    let own = std::process::id();
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<RunningProcess> = snapshot()
        .into_iter()
        .filter(|(pid, p)| *pid != own && visible.contains(pid) && !p.exe.eq_ignore_ascii_case("explorer.exe"))
        .map(|(_, p)| p)
        .filter(|p| seen.insert(p.exe.to_ascii_lowercase()))
        .collect();
    out.sort_by_key(|p| p.name.to_lowercase());
    out
}

/// Any running process by pid (not only those with windows).
pub fn find_process(pid: u32) -> Option<RunningProcess> {
    snapshot().into_iter().find(|(p, _)| *p == pid).map(|(_, r)| r)
}

pub fn add_game_from_process(conn: &Connection, pid: u32) -> Result<Game> {
    let p = find_process(pid).ok_or_else(|| anyhow!("That program is no longer running"))?;
    add_game(conn, &p)
}

fn windows_with_visible_ui() -> std::collections::HashSet<u32> {
    use windows::core::BOOL;
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextLengthW, GetWindowThreadProcessId, IsWindowVisible,
    };
    unsafe extern "system" fn each(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: `lparam` is the `&mut HashSet` passed to EnumWindows below, alive for the call.
        let set = unsafe { &mut *(lparam.0 as *mut std::collections::HashSet<u32>) };
        unsafe {
            if IsWindowVisible(hwnd).as_bool() && GetWindowTextLengthW(hwnd) > 0 {
                let mut cloaked = 0u32;
                let _ = DwmGetWindowAttribute(
                    hwnd,
                    DWMWA_CLOAKED,
                    &mut cloaked as *mut u32 as *mut core::ffi::c_void,
                    std::mem::size_of::<u32>() as u32,
                );
                if cloaked == 0 {
                    let mut pid = 0u32;
                    GetWindowThreadProcessId(hwnd, Some(&mut pid));
                    if pid != 0 {
                        set.insert(pid);
                    }
                }
            }
        }
        BOOL(1)
    }
    let mut set = std::collections::HashSet::new();
    // SAFETY: the callback only touches `set` through the pointer for the duration of the call.
    let _ = unsafe { EnumWindows(Some(each), LPARAM(&mut set as *mut _ as isize)) };
    set
}

// ----- Rocket League Stats API -----

/// `{ status: "enabled"|"disabled"|"not_found", ini_path, port, listener_connected, rl_install, note }`
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RlStatsInfo {
    pub status: &'static str,
    pub ini_path: Option<String>,
    pub port: u16,
    /// ClipVault's stats listener currently has a socket to Rocket League.
    pub listener_connected: bool,
    pub rl_install: Option<String>,
    /// Set after enabling: the game must be restarted.
    pub note: Option<String>,
}

pub fn rl_info(docs_dir: Option<&Path>, rl_install: Option<&Path>, listener_connected: bool) -> RlStatsInfo {
    use crate::rl_stats::{resolve_stats_ini, stats_api_status_in, stats_port, StatsApiStatus, DEFAULT_PORT};
    let ini = resolve_stats_ini(docs_dir, rl_install);
    let port = ini
        .as_deref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| stats_port(&t))
        .unwrap_or(DEFAULT_PORT);
    let status = match stats_api_status_in(docs_dir, rl_install) {
        StatsApiStatus::Enabled => "enabled",
        StatsApiStatus::Disabled => "disabled",
        StatsApiStatus::NotFound => "not_found",
    };
    RlStatsInfo {
        status,
        ini_path: ini.map(|p| p.to_string_lossy().into_owned()),
        port,
        listener_connected,
        rl_install: rl_install.map(|p| p.to_string_lossy().into_owned()),
        note: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::clips::{Clip, Marker, MarkerKind};
    use crate::db::open_memory;
    use crate::settings::Settings;
    use chrono::DateTime;
    use std::path::{Path, PathBuf};

    fn clip(id: &str, title: &str, game: &str, tags: &[&str], at: &str) -> Clip {
        Clip {
            id: id.into(),
            title: title.into(),
            description: String::new(),
            path: PathBuf::from(format!(r"E:\GameClips\{id}.mp4")),
            game: game.into(),
            recorded_at: DateTime::parse_from_rfc3339(at).unwrap(),
            duration_s: 20.0,
            fps: 60,
            width: 1920,
            height: 1080,
            size: 1000,
            trim_in: None,
            trim_out: None,
            thumbnail_path: None,
            source: "native".into(),
            source_id: None,
            tags: tags.iter().map(|t| t.to_string()).collect(),
            starred: false,
            compressed: false,
            original_size: None,
            markers: vec![Marker { t_s: 7.0, kind: MarkerKind::Goal, label: "G".into() }],
        }
    }

    fn seeded() -> rusqlite::Connection {
        let conn = open_memory().unwrap();
        for c in [
            clip("a", "Insane goal", "Rocket League", &["clutch"], "2026-11-01T10:00:00+01:00"),
            clip("b", "Funny fail", "Valorant", &["lol", "goal"], "2026-11-02T10:00:00+01:00"),
            clip("c", "Ace", "Valorant", &[], "2026-11-03T10:00:00+01:00"),
        ] {
            crate::db::clips::insert(&conn, &c).unwrap();
        }
        conn
    }

    #[test]
    fn scrub_strip_reuses_existing_file_without_ffmpeg() {
        let dir = tempfile::tempdir().unwrap();
        let video = dir.path().join("a.mp4");
        std::fs::write(&video, b"x").unwrap();
        let mut c = clip("a", "A", "RL", &[], "2026-11-01T10:00:00+01:00");
        c.path = video;
        let conn = open_memory().unwrap();
        crate::db::clips::insert(&conn, &c).unwrap();
        let db = Mutex::new(conn);
        let strip = strip_path(dir.path(), "a");
        std::fs::write(&strip, b"jpg").unwrap();
        let got = ensure_scrub_strip(&db, &dir.path().join("no-ffmpeg.exe"), dir.path(), "a").unwrap();
        assert_eq!(got.as_deref(), Some(strip.to_string_lossy().as_ref()));
    }

    #[test]
    fn scrub_strip_none_when_video_missing() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_memory().unwrap();
        crate::db::clips::insert(&conn, &clip("a", "A", "RL", &[], "2026-11-01T10:00:00+01:00")).unwrap();
        let db = Mutex::new(conn);
        assert_eq!(ensure_scrub_strip(&db, &dir.path().join("no-ffmpeg.exe"), dir.path(), "a").unwrap(), None);
    }

    #[test]
    fn forget_strip_removes_file_and_ignores_missing() {
        let dir = tempfile::tempdir().unwrap();
        let strip = strip_path(dir.path(), "a");
        std::fs::write(&strip, b"jpg").unwrap();
        forget_strip(dir.path(), "a");
        assert!(!strip.exists());
        forget_strip(dir.path(), "a");
    }

    fn ids(v: &[Clip]) -> Vec<&str> {
        v.iter().map(|c| c.id.as_str()).collect()
    }

    #[test]
    fn search_matches_title_and_tags() {
        let conn = seeded();
        let f = |s: &str| ListFilter { search: Some(s.into()), ..Default::default() };
        // "fail": title of b only; "goal" also hits every clip's goal marker.
        assert_eq!(ids(&list_clips(&conn, &f("fail")).unwrap()), ["b"]);
        assert_eq!(ids(&list_clips(&conn, &f("goal")).unwrap()), ["c", "b", "a"]);
        assert_eq!(ids(&list_clips(&conn, &f("CLUTCH")).unwrap()), ["a"]);
        assert_eq!(ids(&list_clips(&conn, &f("lol")).unwrap()), ["b"]);
        assert!(list_clips(&conn, &f("zzz")).unwrap().is_empty());
        // blank search = no filter
        assert_eq!(list_clips(&conn, &f("  ")).unwrap().len(), 3);
    }

    #[test]
    fn search_covers_game_date_markers_and_needs_every_word() {
        let conn = seeded();
        let f = |s: &str| ListFilter { search: Some(s.into()), ..Default::default() };
        assert_eq!(ids(&list_clips(&conn, &f("valorant")).unwrap()), ["c", "b"]);
        assert_eq!(ids(&list_clips(&conn, &f("2026-11-02")).unwrap()), ["b"]);
        // every word must match somewhere (game AND tag)
        assert_eq!(ids(&list_clips(&conn, &f("valorant lol")).unwrap()), ["b"]);
        assert!(list_clips(&conn, &f("rocket lol")).unwrap().is_empty());
        // marker kind and label
        conn.execute("INSERT INTO markers (clip_id, t_seconds, type, label) VALUES ('a', 1, 'laugh', 'big one')", []).unwrap();
        assert_eq!(ids(&list_clips(&conn, &f("laugh")).unwrap()), ["a"]);
        assert_eq!(ids(&list_clips(&conn, &f("big")).unwrap()), ["a"]);
        // LIKE wildcards are literal
        assert!(list_clips(&conn, &f("%")).unwrap().is_empty());
    }

    #[test]
    fn star_and_starred_filter() {
        let conn = seeded();
        assert!(!get_clip(&conn, "b").unwrap().starred);
        assert!(set_starred(&conn, "b", true).unwrap().starred);
        let f = ListFilter { starred: true, ..Default::default() };
        assert_eq!(ids(&list_clips(&conn, &f).unwrap()), ["b"]);
        set_starred(&conn, "b", false).unwrap();
        assert!(list_clips(&conn, &f).unwrap().is_empty());
        assert!(set_starred(&conn, "nope", true).is_err());
        let f: ListFilter = serde_json::from_str(r#"{"starred":true}"#).unwrap();
        assert!(f.starred);
    }

    #[test]
    fn deleting_moments_clip_keeps_its_file_and_blocks_reimport() {
        let conn = seeded();
        let file = temp_clip_file();
        with_path(&conn, "a", &file, None);
        conn.execute("UPDATE clips SET source = 'moments', source_id = 'm-a' WHERE id = 'a'", []).unwrap();
        let trashed = std::cell::Cell::new(false);
        delete_clip_with(&conn, "a", &|_| {
            trashed.set(true);
            Ok(())
        })
        .unwrap();
        assert!(!trashed.get(), "a SteelSeries file must never go to the Recycle Bin");
        assert!(file.exists());
        assert!(crate::db::clips::get(&conn, "a").unwrap().is_none());
        assert!(crate::db::clips::exists_source(&conn, "moments", "m-a").unwrap());
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn outputs_of_moments_clips_go_to_capture_dir() {
        let conn = seeded();
        let cap = std::env::temp_dir().join(format!("clipvault-cap-{}", uuid::Uuid::new_v4()));
        let mut c = get_clip(&conn, "a").unwrap();
        c.path = r"E:\GameClips\x.mp4".into();
        assert_eq!(output_dir_for(&c, &cap).unwrap(), PathBuf::from(r"E:\GameClips"));
        c.source = "moments".into();
        assert_eq!(output_dir_for(&c, &cap).unwrap(), cap);
        assert!(cap.is_dir());
        let _ = std::fs::remove_dir(&cap);
    }

    #[test]
    fn filter_by_game_and_sort() {
        let conn = seeded();
        let f = ListFilter { game: Some("Valorant".into()), ..Default::default() };
        assert_eq!(ids(&list_clips(&conn, &f).unwrap()), ["c", "b"]);
        let f = ListFilter { game: Some("Valorant".into()), sort: SortOrder::Oldest, ..Default::default() };
        assert_eq!(ids(&list_clips(&conn, &f).unwrap()), ["b", "c"]);
    }

    #[test]
    fn filter_json_shape() {
        let f: ListFilter = serde_json::from_str(r#"{"game":"X","search":"y","sort":"oldest"}"#).unwrap();
        assert_eq!(f.sort, SortOrder::Oldest);
        let f: ListFilter = serde_json::from_str(r#"{"sort":"newest"}"#).unwrap();
        assert!(f.game.is_none() && f.search.is_none());
    }

    #[test]
    fn meta_and_trim_validation() {
        let conn = seeded();
        let c = update_clip_meta(&conn, "a", "  New  ", "d", &[" x ".into(), "".into(), "x".into(), "y".into()]).unwrap();
        assert_eq!((c.title.as_str(), c.tags.clone()), ("New", vec!["x".to_string(), "y".to_string()]));
        assert!(update_clip_meta(&conn, "a", "   ", "", &[]).is_err());
        assert!(update_clip_meta(&conn, "nope", "t", "", &[]).is_err());

        let c = set_trim(&conn, "a", Some(2.0), Some(10.0)).unwrap();
        assert_eq!((c.trim_in, c.trim_out), (Some(2.0), Some(10.0)));
        assert!(set_trim(&conn, "a", Some(10.0), Some(10.0)).is_err());
        assert!(set_trim(&conn, "a", Some(-1.0), None).is_err());
        assert!(set_trim(&conn, "a", None, Some(f64::NAN)).is_err());
        assert!(set_trim(&conn, "a", Some(1.0), Some(21.0)).is_err()); // past duration 20
        // failed calls changed nothing; clearing works
        assert_eq!(get_clip(&conn, "a").unwrap().trim_out, Some(10.0));
        let c = set_trim(&conn, "a", None, None).unwrap();
        assert_eq!((c.trim_in, c.trim_out), (None, None));
    }

    #[test]
    fn only_capture_folder_files_and_clips_are_revealable() {
        let root = tempfile::tempdir().unwrap();
        let capture = root.path().join("Clips");
        let other = root.path().join("Other");
        std::fs::create_dir_all(capture.join("Exports")).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let export = capture.join("Exports").join("a.mp4");
        let clip = other.join("imported.mp4");
        let secret = other.join("secret.txt");
        for f in [&export, &clip, &secret] {
            std::fs::write(f, b"x").unwrap();
        }
        let clips = vec![clip.clone()];
        let shown = revealable(&export, &capture, &clips).expect("export in the capture folder");
        assert!(!shown.to_string_lossy().starts_with(r"\\?\"), "Explorer gets a plain path: {}", shown.display());
        assert!(revealable(&clip, &capture, &clips).is_ok(), "library clip outside it");
        assert!(revealable(&secret, &capture, &clips).is_err(), "unrelated file");
        let sneaky = capture.join("..").join("Other").join("secret.txt");
        assert!(revealable(&sneaky, &capture, &clips).is_err(), "`..` must not escape the capture folder");
        assert!(revealable(&capture.join("gone.mp4"), &capture, &clips).is_err(), "missing file");
        assert!(revealable(&capture.join("Exports"), &capture, &clips).is_err(), "folders are not files");
    }

    fn temp_clip_file() -> PathBuf {
        let p = std::env::temp_dir().join(format!("clipvault-test-{}.mp4", uuid::Uuid::new_v4()));
        std::fs::write(&p, b"not really a video").unwrap();
        p
    }

    fn with_path(conn: &rusqlite::Connection, id: &str, path: &Path, thumb: Option<&Path>) {
        let mut c = crate::db::clips::get(conn, id).unwrap().unwrap();
        crate::db::clips::delete(conn, id).unwrap();
        c.path = path.to_path_buf();
        c.thumbnail_path = thumb.map(|p| p.to_path_buf());
        crate::db::clips::insert(conn, &c).unwrap();
    }

    #[test]
    fn delete_moves_to_trash_and_removes_row() {
        let conn = seeded();
        let file = temp_clip_file();
        let thumb = std::env::temp_dir().join(format!("clipvault-test-{}.jpg", uuid::Uuid::new_v4()));
        std::fs::write(&thumb, b"jpg").unwrap();
        with_path(&conn, "a", &file, Some(&thumb));
        delete_clip(&conn, "a").unwrap();
        assert!(!file.exists(), "file must be gone from its path (Recycle Bin)");
        assert!(!thumb.exists());
        assert!(crate::db::clips::get(&conn, "a").unwrap().is_none());
        assert!(delete_clip(&conn, "a").is_err()); // unknown id
    }

    #[test]
    fn delete_missing_file_still_removes_row() {
        let conn = seeded();
        let gone = std::env::temp_dir().join(format!("clipvault-test-{}.mp4", uuid::Uuid::new_v4()));
        with_path(&conn, "b", &gone, None);
        delete_clip_with(&conn, "b", &|_| panic!("must not trash a missing file")).unwrap();
        assert!(crate::db::clips::get(&conn, "b").unwrap().is_none());
    }

    /// If the row cannot be removed, the file must stay where it is: never a
    /// library entry pointing at a file in the Recycle Bin.
    #[test]
    fn delete_row_failure_keeps_file() {
        let conn = seeded();
        let file = temp_clip_file();
        with_path(&conn, "a", &file, None);
        conn.execute_batch("CREATE TRIGGER no_delete BEFORE DELETE ON clips BEGIN SELECT RAISE(ABORT, 'database is locked'); END;")
            .unwrap();
        let trashed = std::cell::Cell::new(false);
        let r = delete_clip_with(&conn, "a", &|_| {
            trashed.set(true);
            Ok(())
        });
        assert!(r.is_err());
        assert!(!trashed.get(), "file must not be trashed when the row stays");
        assert!(file.exists());
        conn.execute_batch("DROP TRIGGER no_delete;").unwrap();
        assert!(crate::db::clips::get(&conn, "a").unwrap().is_some());
    }

    #[test]
    fn delete_trash_failure_keeps_row() {
        let conn = seeded();
        let file = temp_clip_file();
        with_path(&conn, "c", &file, None);
        let err = delete_clip_with(&conn, "c", &|_| Err("locked by another process".into())).unwrap_err();
        assert!(err.to_string().contains("locked"), "{err}");
        assert!(file.exists());
        assert!(crate::db::clips::get(&conn, "c").unwrap().is_some());
        std::fs::remove_file(file).unwrap();
    }

    #[test]
    fn settings_validation() {
        let ok = Settings::default();
        assert!(validate_settings(&ok).is_ok());
        let bad = |f: &dyn Fn(&mut Settings)| {
            let mut s = Settings::default();
            f(&mut s);
            validate_settings(&s).unwrap_err()
        };
        assert!(bad(&|s| s.fps = 45).contains("fps"));
        assert!(bad(&|s| s.save_length_s = 4).contains("Save length"));
        assert!(bad(&|s| {
            s.max_save_length_s = 100;
            s.save_length_s = 101
        })
        .contains("Save length"));
        assert!(bad(&|s| s.max_save_length_s = 601).contains("600"));
        assert!(bad(&|s| s.bitrate_kbps = 1999).contains("itrate"));
        assert!(bad(&|s| s.bitrate_kbps = 150_001).contains("itrate"));
        assert!(bad(&|s| s.capture_dir = PathBuf::from("  ")).contains("folder"));
        assert!(bad(&|s| s.capture_dir = PathBuf::from("GameClips")).contains("full path"));
        assert!(bad(&|s| s.capture_dir = PathBuf::from(r"E:GameClips")).contains("full path"));
        // C2: the buffer folder is wiped of our segments on every start, so it
        // must be a real absolute folder.
        assert!(bad(&|s| s.buffer_dir = PathBuf::from("")).contains("Buffer folder"));
        assert!(bad(&|s| s.buffer_dir = PathBuf::from("  ")).contains("Buffer folder"));
        assert!(bad(&|s| s.buffer_dir = PathBuf::from("buffer")).contains("full path"));
        assert!(bad(&|s| s.buffer_dir = PathBuf::from(r"\buffer")).contains("full path"));
        assert!(bad(&|s| s.buffer_dir = PathBuf::from(r"C:buffer")).contains("full path"));
        let mut s = Settings::default();
        s.fps = 30;
        s.save_length_s = 5;
        s.max_save_length_s = 600;
        s.bitrate_kbps = 2000;
        assert!(validate_settings(&s).is_ok());
    }

    #[test]
    fn game_names_from_exe() {
        assert_eq!(game_name_from_exe("RocketLeague.exe"), "RocketLeague");
        assert_eq!(game_name_from_exe("valorant-win64-shipping.EXE"), "Valorant-win64-shipping");
        assert_eq!(game_name_from_exe("x"), "X");
    }

    #[test]
    fn add_game_merges_exes() {
        let conn = open_memory().unwrap();
        let p = RunningProcess {
            pid: 7,
            name: "RocketLeague".into(),
            exe: "RocketLeague.exe".into(),
            path: Some(r"C:\g\RocketLeague.exe".into()),
        };
        let g = add_game(&conn, &p).unwrap();
        assert_eq!((g.name.as_str(), g.exe_names.clone()), ("RocketLeague", vec!["RocketLeague.exe".to_string()]));
        // same derived name, another exe: merged, not replaced
        let p2 = RunningProcess { exe: "RocketLeague.exe".into(), ..p.clone() };
        assert_eq!(add_game(&conn, &p2).unwrap().exe_names.len(), 1);
        let p3 = RunningProcess { exe: "RocketLeague.exe".into(), ..p.clone() };
        let g2 = add_game(&conn, &RunningProcess { exe: "RLLauncher.exe".into(), ..p3 }).unwrap();
        assert_eq!(g.id, g2.id);
        assert_eq!(g2.exe_names.len(), 2);
    }

    #[test]
    fn rl_info_reports_ini_and_port() {
        let docs = tempfile::tempdir().unwrap();
        let cfg = docs.path().join("My Games").join("Rocket League").join("TAGame").join("Config");
        std::fs::create_dir_all(&cfg).unwrap();
        // no ini yet but config dir exists => disabled, path resolved (to be created)
        let i = rl_info(Some(docs.path()), None, false);
        assert_eq!(i.status, "disabled");
        assert_eq!(i.port, crate::rl_stats::DEFAULT_PORT);
        assert!(i.ini_path.unwrap().ends_with("TAStatsAPI.ini"));
        std::fs::write(cfg.join("TAStatsAPI.ini"), "[TAGame.MatchStatsExporter_TA]\nPacketSendRate=30\nPort=50000\n").unwrap();
        let i = rl_info(Some(docs.path()), None, true);
        assert_eq!((i.status, i.port, i.listener_connected), ("enabled", 50000, true));
        let none = tempfile::tempdir().unwrap();
        assert_eq!(rl_info(Some(none.path()), None, false).status, "not_found");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn thumb_queue_runs_one_worker_and_leaves_pool_free() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        static Q: ThumbQueue = ThumbQueue::new();
        let (cur, max, ran) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let mut tasks = Vec::new();
        for i in 0..200 {
            let (cur, max, ran) = (cur.clone(), max.clone(), ran.clone());
            tasks.push(tokio::spawn(async move {
                Q.run(&format!("clip{i}"), move || {
                    let n = cur.fetch_add(1, Ordering::SeqCst) + 1;
                    max.fetch_max(n, Ordering::SeqCst);
                    ran.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(3));
                    cur.fetch_sub(1, Ordering::SeqCst);
                    Ok(Some("t.jpg".into()))
                })
                .await
            }));
        }
        // While the queue is busy, other blocking work must still be served promptly.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let t = std::time::Instant::now();
        tokio::task::spawn_blocking(|| ()).await.unwrap();
        assert!(t.elapsed() < std::time::Duration::from_millis(250), "pool starved: {:?}", t.elapsed());
        for t in tasks {
            assert_eq!(t.await.unwrap().as_deref(), Some("t.jpg"));
        }
        assert_eq!(max.load(Ordering::SeqCst), 1);
        assert_eq!(ran.load(Ordering::SeqCst), 200);
    }

    #[tokio::test]
    async fn thumb_queue_remembers_failures() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        static Q: ThumbQueue = ThumbQueue::new();
        let runs = Arc::new(AtomicUsize::new(0));
        for _ in 0..3 {
            let r = runs.clone();
            let out = Q
                .run("broken", move || {
                    r.fetch_add(1, Ordering::SeqCst);
                    Err(anyhow!("ffmpeg failed"))
                })
                .await;
            assert_eq!(out, None);
        }
        assert_eq!(runs.load(Ordering::SeqCst), 1, "failed clip must not rerun FFmpeg");
        // a different clip still runs
        assert_eq!(Q.run("ok", || Ok(Some("x".into()))).await.as_deref(), Some("x"));
    }

    #[test]
    fn asset_scope_entries_are_non_recursive() {
        let mut s = Settings::default();
        s.capture_dir = PathBuf::from(r"E:\GameClips");
        let e = asset_scope_entries(
            &s,
            Path::new(r"C:\data\thumbs"),
            vec![PathBuf::from(r"E:\x.mp4"), PathBuf::from(r"D:\M\y.mp4")],
        );
        assert_eq!(e.len(), 4);
        assert!(e.iter().all(|(_, recursive)| !recursive));
        assert!(e.iter().any(|(d, _)| d == Path::new(r"E:\")));
    }

    #[test]
    fn asset_dirs_are_distinct_parents() {
        let mut s = Settings::default();
        s.capture_dir = PathBuf::from(r"E:\GameClips");
        let mut a = clip("a", "t", "g", &[], "2026-11-01T10:00:00+01:00");
        a.path = PathBuf::from(r"E:\GameClips\a.mp4");
        let mut b = clip("b", "t", "g", &[], "2026-11-01T10:00:00+01:00");
        b.path = PathBuf::from(r"D:\Moments\b.mp4");
        let mut c = clip("c", "t", "g", &[], "2026-11-01T10:00:00+01:00");
        c.path = PathBuf::from(r"D:\Moments\c.mp4");
        let dirs = asset_dirs(&s, Path::new(r"C:\data\thumbs"), &[a, b, c]);
        assert_eq!(
            dirs,
            vec![PathBuf::from(r"E:\GameClips"), PathBuf::from(r"C:\data\thumbs"), PathBuf::from(r"D:\Moments")]
        );
    }
}
