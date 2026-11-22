//! SaveService: turns a save-hotkey press into an MP4 clip, a library row and
//! markers (spec §7.5).
//!
//! Flow of one save (all saves run strictly one at a time, in call order, via a
//! fair `tokio::sync::Mutex`):
//! 1. Health `Idle` at call time and no footage in `[t_press - save_length,
//!    t_press]` ⇒ `NotRecording`. Footage still in the buffer (the game just
//!    ended) is saved.
//! 2. Poll (≤ 3 s) until the segment containing `t_press` has closed
//!    (`latest_end >= t_press`) or health leaves `Recording` (a restarting or
//!    failing capture produces no new segments, so waiting is pointless; the
//!    segments already on disk are saved).
//! 3. Under the index lock snapshot the segments overlapping
//!    `[t_press - save_length, t_press]`, release it and immediately hardlink
//!    (fallback: copy) each into `<temp_root>\clipvault-save-{uuid}`. A segment
//!    the supervisor pruned in between is skipped; none left ⇒ `Failed`.
//! 4. FFmpeg concat demuxer (`-c copy`, `+faststart`, `-f mp4`) into
//!    `<capture_dir>\<Game>__date__time[_N].mp4.tmp`, then rename to `.mp4`.
//!    The concat FFmpeg runs in the kill-on-close job; on a timeout it is
//!    killed and reaped before the `.tmp` is removed.
//! 5. Thumbnail (failure only logged), markers, library row.
//!
//! Every failure is a `Failed { reason }` naming what broke; the temp dir and
//! any `.tmp` are removed by drop guards on every path. If the library insert
//! fails after the MP4 was written, the MP4 is kept (never destroy footage) and
//! the reason says where it is.
//!
//! Clip duration is the sum of the selected segments' durations: concat copies
//! whole segments and removes holes between them, so markers are mapped onto
//! that concatenated timeline (see [`markers_for`]).

use crate::capture::args::build_concat_args;
use crate::capture::segments::{Segment, SegmentIndex};
use crate::capture::supervisor::{Health, Supervisor};
use crate::db::clips::{self, Clip, Marker, MarkerKind};
use crate::events::{BusEvent, EventBus};
use crate::locks;
use crate::thumbs::generate_thumbnail;
use chrono::{DateTime, Local};
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// Longest wait for the segment containing the press to close.
const WAIT_FOR_SEGMENT: Duration = Duration::from_secs(3);
const WAIT_POLL: Duration = Duration::from_millis(100);
/// Upper bound for a stream-copy concat; it normally takes well under a second.
const CONCAT_TIMEOUT: Duration = Duration::from_secs(120);
/// Segment granularity: a clip shorter than `save_length - this` is `short`.
const SHORT_TOLERANCE_S: f64 = 2.5;
/// Capture infos kept for footage of earlier layouts still in the buffer.
const MAX_LAYOUT_INFOS: usize = 16;
/// `CREATE_NO_WINDOW`: no console window flashes up for FFmpeg.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug, Clone, PartialEq)]
pub enum SaveOutcome {
    Saved { clip: Clip, gap: bool, short: bool },
    NotRecording,
    Failed { reason: String },
}

/// What a capture produces; set by the app whenever capture starts, keyed by
/// its stream layout (`capture::args::layout_key`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureInfo {
    pub fps: u32,
    pub width: u32,
    pub height: u32,
    /// Audio track titles in stream order, e.g. `Mic`, `Discord`, `Game/System`.
    pub track_titles: Vec<String>,
    /// What was recorded (game name or `Desktop`).
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveConfig {
    pub ffmpeg: PathBuf,
    /// Where finished clips go (e.g. `E:\GameClips`); created if missing.
    pub capture_dir: PathBuf,
    /// `%LOCALAPPDATA%\ClipVault\thumbs`.
    pub thumbs_dir: PathBuf,
    /// Parent of the per-save `clipvault-save-{uuid}` dir; in the app a folder
    /// next to the buffer (same volume: hard links, not copies).
    pub temp_root: PathBuf,
    pub save_length: Duration,
}

/// Share it behind an `Arc`; `save` takes `&self`.
pub struct SaveService {
    config: Mutex<SaveConfig>,
    /// The latest capture's info, and the info per layout still in the buffer.
    capture: Mutex<CaptureInfo>,
    infos: Mutex<HashMap<u64, CaptureInfo>>,
    supervisor: Arc<Supervisor>,
    db: Arc<Mutex<Connection>>,
    bus: Arc<EventBus>,
    /// Fair (FIFO) lock: saves run one at a time in call order.
    queue: tokio::sync::Mutex<()>,
    /// Test hook: the next save panics while holding the config lock.
    #[cfg(test)]
    pub(crate) panic_next_save: std::sync::atomic::AtomicBool,
}

impl SaveService {
    pub fn new(
        config: SaveConfig,
        capture: CaptureInfo,
        supervisor: Arc<Supervisor>,
        db: Arc<Mutex<Connection>>,
        bus: Arc<EventBus>,
    ) -> Self {
        Self {
            config: Mutex::new(config),
            capture: Mutex::new(capture),
            infos: Mutex::new(HashMap::new()),
            supervisor,
            db,
            bus,
            queue: tokio::sync::Mutex::new(()),
            #[cfg(test)]
            panic_next_save: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Info of the capture about to start with stream layout `layout`.
    pub fn set_capture_info(&self, layout: u64, info: CaptureInfo) {
        let mut infos = locks::lock(&self.infos);
        if infos.len() >= MAX_LAYOUT_INFOS {
            infos.clear(); // layouts this old have long left the buffer
        }
        infos.insert(layout, info.clone());
        *locks::lock(&self.capture) = info;
    }

    /// Info of the capture that recorded `layout`; the latest capture's if unknown.
    pub fn capture_info_for(&self, layout: u64) -> CaptureInfo {
        locks::lock(&self.infos).get(&layout).cloned().unwrap_or_else(|| locks::lock(&self.capture).clone())
    }

    /// Waits until every save queued so far has finished (true), or `timeout`.
    pub async fn drain(&self, timeout: Duration) -> bool {
        tokio::time::timeout(timeout, self.queue.lock()).await.is_ok()
    }

    /// Takes effect for saves that have not started yet.
    pub fn set_config(&self, config: SaveConfig) {
        *locks::lock(&self.config) = config;
    }

    pub async fn save(&self, t_press: SystemTime, game: &str) -> SaveOutcome {
        if self.supervisor.health() == Health::Idle {
            // Capture stopped (the game ended): its footage may still be in the buffer.
            let save_length = locks::lock(&self.config).save_length;
            let from = t_press.checked_sub(save_length).unwrap_or(SystemTime::UNIX_EPOCH);
            if latest_end(&self.supervisor.segments()).is_none_or(|e| e <= from) {
                tracing::info!("[save] {game}: not recording");
                return SaveOutcome::NotRecording;
            }
            tracing::info!("[save] capture idle; saving the footage still in the buffer");
        }
        let _turn = self.queue.lock().await;
        let cfg = {
            let g = locks::lock(&self.config);
            #[cfg(test)]
            if self.panic_next_save.swap(false, std::sync::atomic::Ordering::SeqCst) {
                panic!("injected save panic (test)");
            }
            g.clone()
        };
        let outcome = match self.run(t_press, game, &cfg).await {
            Ok(o) => o,
            Err(reason) => SaveOutcome::Failed { reason },
        };
        match &outcome {
            SaveOutcome::Saved { clip, gap, short } => tracing::info!(
                "[save] saved {} ({:.1} s, gap={gap}, short={short})",
                clip.path.display(),
                clip.duration_s
            ),
            SaveOutcome::Failed { reason } => tracing::warn!("[save] {game}: FAILED: {reason}"),
            SaveOutcome::NotRecording => {}
        }
        outcome
    }

    async fn run(
        &self,
        t_press: SystemTime,
        game: &str,
        cfg: &SaveConfig,
    ) -> Result<SaveOutcome, String> {
        let index = self.supervisor.segments();

        // 2. Let the segment containing the press close.
        let deadline = Instant::now() + WAIT_FOR_SEGMENT;
        while latest_end(&index).is_none_or(|e| e < t_press)
            && self.supervisor.health() == Health::Recording
            && Instant::now() < deadline
        {
            tokio::time::sleep(WAIT_POLL).await;
        }

        let capture_dir = cfg.capture_dir.clone();
        tokio::task::spawn_blocking(move || std::fs::create_dir_all(&capture_dir))
            .await
            .map_err(|e| format!("internal error: {e}"))?
            .map_err(|e| format!("cannot create capture folder {}: {e}", cfg.capture_dir.display()))?;

        // 3. Snapshot + link in one blocking step, so pruning gets no chance in between.
        let from = t_press.checked_sub(cfg.save_length).unwrap_or(SystemTime::UNIX_EPOCH);
        let temp = TempDir(cfg.temp_root.join(format!("clipvault-save-{}", uuid::Uuid::new_v4())));
        let temp_path = temp.0.clone();
        let (has_gap, layout, linked) =
            tokio::task::spawn_blocking(move || snapshot_and_link(&index, from, t_press, &temp_path))
                .await
                .map_err(|e| format!("internal error: {e}"))??;
        if linked.is_empty() {
            return Err(format!("no recorded video in the last {} s", cfg.save_length.as_secs()));
        }
        // The stream-copied clip is about as big as its segments: fail now,
        // with a clear reason, rather than with a truncated file mid-concat.
        check_space(&cfg.capture_dir, &linked).await?;
        // Metadata of the capture that recorded this footage (it may have
        // ended, or been restarted with another layout, since). A known
        // layout also names the clip after what it recorded.
        let known = locks::lock(&self.infos).contains_key(&layout);
        let info = &self.capture_info_for(layout);
        let game = if known { info.label.as_str() } else { game };

        // 4. Concat into <name>.mp4.tmp, then rename.
        let list = temp.0.join("concat.txt");
        let files: Vec<PathBuf> = linked.iter().map(|s| s.path.clone()).collect();
        std::fs::write(&list, concat_list(&files))
            .map_err(|e| format!("cannot write concat list {}: {e}", list.display()))?;
        let name = clip_filename(game, DateTime::<Local>::from(t_press));
        let final_path = unique_clip_path(&cfg.capture_dir, &name);
        let tmp_out = TmpFile(tmp_path_for(&final_path));
        let titles: Vec<&str> = info.track_titles.iter().map(String::as_str).collect();
        run_concat(&cfg.ffmpeg, &build_concat_args(&list, &tmp_out.0, &titles)).await?;
        match std::fs::metadata(&tmp_out.0) {
            Ok(m) if m.len() > 0 => {}
            _ => return Err(format!("FFmpeg reported success but wrote no clip to {}", tmp_out.0.display())),
        }
        std::fs::rename(&tmp_out.0, &final_path)
            .map_err(|e| format!("cannot rename {} to {}: {e}", tmp_out.0.display(), final_path.display()))?;
        tmp_out.disarm();
        drop(temp);

        // 5. Thumbnail, markers, library row.
        let duration_s: f64 = linked.iter().map(seg_secs).sum();
        let id = uuid::Uuid::new_v4().to_string();
        let thumbnail_path = make_thumbnail(cfg, &final_path, &id, duration_s).await;
        let size = std::fs::metadata(&final_path).map(|m| m.len()).unwrap_or(0);
        let recorded = DateTime::<Local>::from(t_press);
        let clip = Clip {
            id,
            title: format!("{game} {}", recorded.format("%d.%m.%Y %H:%M")),
            description: String::new(),
            path: final_path.clone(),
            game: game.to_string(),
            recorded_at: recorded.fixed_offset(),
            duration_s,
            fps: info.fps,
            width: info.width,
            height: info.height,
            size,
            trim_in: None,
            trim_out: None,
            thumbnail_path,
            source: "native".into(),
            source_id: None,
            tags: Vec::new(),
            starred: false,
            compressed: false,
            markers: markers_for(&linked, &self.bus.between(from, t_press), t_press),
        };
        let db = self.db.clone();
        let row = clip.clone();
        tokio::task::spawn_blocking(move || {
            let conn = locks::lock(&db);
            // A rescan may have picked the new file up first; this save's
            // row (with markers and the real title) replaces that one.
            if let Some(old) = clips::id_by_path(&conn, &row.path)? {
                tracing::info!("replacing rescanned row {old} for {}", row.path.display());
                clips::delete_rows(&conn, &old)?;
            }
            clips::insert(&conn, &row)
        })
            .await
            .map_err(|e| format!("internal error: {e}"))?
            .map_err(|e| {
                format!("clip saved as {} but could not be added to the library: {e:#}", final_path.display())
            })?;

        let short = duration_s < cfg.save_length.as_secs_f64() - SHORT_TOLERANCE_S;
        Ok(SaveOutcome::Saved { clip, gap: has_gap, short })
    }
}

fn latest_end(index: &Mutex<SegmentIndex>) -> Option<SystemTime> {
    locks::lock(&index).latest_end()
}

fn seg_secs(s: &Segment) -> f64 {
    s.end.duration_since(s.start).unwrap_or_default().as_secs_f64()
}

/// Selects `[from, to]` under the index lock, then links the snapshot into
/// `temp` (created here). Returns `has_gap`, the selected layout and the
/// linked segments, whose paths point into `temp`.
fn snapshot_and_link(
    index: &Mutex<SegmentIndex>,
    from: SystemTime,
    to: SystemTime,
    temp: &Path,
) -> Result<(bool, u64, Vec<Segment>), String> {
    let selection = locks::lock(&index).select(from, to);
    let layout = selection.layout;
    if selection.segments.is_empty() {
        return Ok((selection.has_gap, layout, Vec::new()));
    }
    std::fs::create_dir_all(temp).map_err(|e| format!("cannot create temp folder {}: {e}", temp.display()))?;
    let mut linked = Vec::with_capacity(selection.segments.len());
    for (i, seg) in selection.segments.into_iter().enumerate() {
        let dest = temp.join(format!("{i:04}.ts"));
        match link_or_copy(&seg.path, &dest) {
            Ok(()) => linked.push(Segment { path: dest, ..seg }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!("[save] segment {} vanished before it could be linked; skipped", seg.path.display());
            }
            Err(e) => {
                return Err(format!("cannot copy segment {} to {}: {e}", seg.path.display(), dest.display()));
            }
        }
    }
    if linked.is_empty() {
        return Err("all recorded segments were deleted before they could be saved".into());
    }
    Ok((selection.has_gap, layout, linked))
}

/// Errors if the clip made of `linked` would not fit in `dir`. A failed probe
/// is logged and lets the save go ahead (the probe must never cost a clip).
async fn check_space(dir: &Path, linked: &[Segment]) -> Result<(), String> {
    let dir = dir.to_path_buf();
    let paths: Vec<PathBuf> = linked.iter().map(|s| s.path.clone()).collect();
    tokio::task::spawn_blocking(move || {
        let clip_bytes: u64 = paths.iter().filter_map(|p| std::fs::metadata(p).ok()).map(|m| m.len()).sum();
        match crate::disk::free_bytes(&dir) {
            Ok(free) => crate::disk::save_space_error(&dir, free, clip_bytes).map_or(Ok(()), Err),
            Err(e) => {
                tracing::warn!("[save] could not read free space of {}: {e}; saving anyway", dir.display());
                Ok(())
            }
        }
    })
    .await
    .map_err(|e| format!("internal error: {e}"))?
}

fn link_or_copy(src: &Path, dest: &Path) -> std::io::Result<()> {
    match std::fs::hard_link(src, dest) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(e),
        // Different volume, FAT32, ...: fall back to a copy.
        Err(_) => std::fs::copy(src, dest).map(|_| ()),
    }
}

async fn run_concat(ffmpeg: &Path, args: &[String]) -> Result<(), String> {
    run_concat_with(ffmpeg, args, CONCAT_TIMEOUT, |_| {}).await
}

/// [`run_concat`] with an explicit timeout; `on_spawn` sees the child (tests).
async fn run_concat_with(
    ffmpeg: &Path,
    args: &[String],
    timeout: Duration,
    on_spawn: impl FnOnce(&tokio::process::Child),
) -> Result<(), String> {
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let mut child = cmd.spawn().map_err(|e| format!("could not run FFmpeg {}: {e}", ffmpeg.display()))?;
    // Dies with ClipVault even on a crash or force-kill (see capture::job).
    #[cfg(windows)]
    if let Some(h) = child.raw_handle() {
        if let Err(e) = crate::capture::job::assign(h) {
            tracing::warn!("[save] concat FFmpeg not in kill-on-close job: {e}");
        }
    }
    on_spawn(&child);
    let reader = child.stderr.take().map(|mut err| {
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut buf = Vec::new();
            let _ = err.read_to_end(&mut buf).await;
            buf
        })
    });
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => return Err(format!("waiting for FFmpeg {} failed: {e}", ffmpeg.display())),
        Err(_) => {
            // Kill AND reap before returning: the caller then removes the
            // .tmp, which a still-running FFmpeg would keep writing / locked.
            if let Err(e) = child.kill().await {
                tracing::warn!("[save] could not kill timed-out concat FFmpeg: {e}");
            }
            return Err(format!("FFmpeg concat timed out after {} s", timeout.as_secs()));
        }
    };
    let stderr = match reader {
        Some(r) => r.await.unwrap_or_default(),
        None => Vec::new(),
    };
    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr);
        let last = last_stderr_line(&stderr).unwrap_or_else(|| "no error output".into());
        return Err(format!("FFmpeg concat failed ({status}): {last}"));
    }
    Ok(())
}

/// Thumbnail at `min(1 s, duration / 2)`; failure is logged, never fatal.
async fn make_thumbnail(cfg: &SaveConfig, video: &Path, id: &str, duration_s: f64) -> Option<PathBuf> {
    let (ffmpeg, src, dir) = (cfg.ffmpeg.clone(), video.to_path_buf(), cfg.thumbs_dir.clone());
    let out = dir.join(format!("{id}.jpg"));
    let target = out.clone();
    let at = (duration_s / 2.0).min(1.0);
    let res = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        std::fs::create_dir_all(&dir)?;
        generate_thumbnail(&ffmpeg, &src, &target, at)
    })
    .await;
    match res {
        Ok(Ok(())) => Some(out),
        Ok(Err(e)) => {
            tracing::warn!("[save] warning: thumbnail for {} failed: {e:#}", video.display());
            None
        }
        Err(e) => {
            tracing::warn!("[save] warning: thumbnail task failed: {e}");
            None
        }
    }
}

/// Removes the per-save temp dir on drop.
struct TempDir(PathBuf);

/// Removes `clipvault-save-*` folders directly in `root` (left by a save cut
/// short by a crash or power loss); returns how many. Call before any save runs.
pub fn clean_stale_save_dirs(root: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(root) else { return 0 };
    let mut removed = 0;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with("clipvault-save-") && e.file_type().is_ok_and(|t| t.is_dir()) {
            match std::fs::remove_dir_all(e.path()) {
                Ok(()) => removed += 1,
                Err(err) => tracing::warn!("could not remove stale save folder {:?}: {err}", e.path()),
            }
        }
    }
    removed
}

impl Drop for TempDir {
    fn drop(&mut self) {
        match std::fs::remove_dir_all(&self.0) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!("[save] could not remove temp dir {}: {e}", self.0.display()),
        }
    }
}

/// Removes the `.mp4.tmp` on drop unless disarmed after the rename.
struct TmpFile(PathBuf);

impl TmpFile {
    fn disarm(self) {
        std::mem::forget(self);
    }
}

impl Drop for TmpFile {
    fn drop(&mut self) {
        match std::fs::remove_file(&self.0) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!("[save] could not remove {}: {e}", self.0.display()),
        }
    }
}

fn tmp_path_for(final_path: &Path) -> PathBuf {
    let mut s = final_path.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

/// Replaces `<>:"/\|?*` and control characters with `_`, trims leading
/// whitespace and trailing dots/spaces (Windows strips them); empty ⇒ `Unknown`.
pub fn sanitize_filename(name: &str) -> String {
    let replaced: String = name
        .chars()
        .map(|c| {
            if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || c.is_control() { '_' } else { c }
        })
        .collect();
    let trimmed = replaced.trim_start().trim_end_matches(['.', ' ']);
    if trimmed.is_empty() { "Unknown".into() } else { trimmed.into() }
}

/// `Rocket-League__2026-11-18__21-37-14.mp4` (Moments naming; spaces ⇒ `-`).
pub fn clip_filename(game: &str, at: DateTime<Local>) -> String {
    let g = sanitize_filename(game).replace(' ', "-");
    format!("{g}__{}.mp4", at.format("%Y-%m-%d__%H-%M-%S"))
}

/// Maps bus events onto the concatenated clip timeline: time inside segment k
/// is `sum(durations of segments before k) + (at - k.start)`, i.e. relative to
/// the FIRST SELECTED SEGMENT's start with holes removed. Events before the
/// first segment are dropped (no footage), events inside a hole snap to the
/// next segment's start, events after the last segment clamp to the end. A
/// `Saved` marker for `t_press` is added. Result is sorted by time.
pub fn markers_for(segments: &[Segment], events: &[BusEvent], t_press: SystemTime) -> Vec<Marker> {
    let total: f64 = segments.iter().map(seg_secs).sum();
    let map = |at: SystemTime| -> Option<f64> {
        let first = segments.first()?;
        if at < first.start {
            return None;
        }
        let mut offset = 0.0;
        for s in segments {
            if at < s.start {
                return Some(offset);
            }
            if at <= s.end {
                return Some(offset + at.duration_since(s.start).unwrap_or_default().as_secs_f64());
            }
            offset += seg_secs(s);
        }
        Some(total)
    };
    let mut out: Vec<Marker> = events
        .iter()
        .filter_map(|e| map(e.at).map(|t_s| Marker { t_s, kind: e.kind, label: e.label.clone() }))
        .collect();
    out.push(Marker { t_s: map(t_press).unwrap_or(0.0), kind: MarkerKind::Saved, label: String::new() });
    out.sort_by(|a, b| a.t_s.total_cmp(&b.t_s));
    out
}

/// FFmpeg concat-demuxer script. Paths use forward slashes inside single
/// quotes; a `'` is written as `'\''`.
pub fn concat_list(files: &[PathBuf]) -> String {
    let mut s = String::from("ffconcat version 1.0\n");
    for f in files {
        let p = f.to_string_lossy().replace('\\', "/").replace('\'', r"'\''");
        s.push_str(&format!("file '{p}'\n"));
    }
    s
}

/// `dir\file_name`, or `stem_2.ext`, `stem_3.ext`… if taken (also by a `.tmp`).
pub fn unique_clip_path(dir: &Path, file_name: &str) -> PathBuf {
    let taken = |p: &Path| p.exists() || tmp_path_for(p).exists();
    let first = dir.join(file_name);
    if !taken(&first) {
        return first;
    }
    let p = Path::new(file_name);
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    (2u32..)
        .map(|n| dir.join(format!("{stem}_{n}{ext}")))
        .find(|c| !taken(c))
        .expect("unbounded suffix search")
}

/// Last non-blank line of FFmpeg's stderr, trimmed.
pub fn last_stderr_line(stderr: &str) -> Option<String> {
    stderr.lines().map(str::trim).rfind(|l| !l.is_empty()).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn stale_save_dirs_are_removed_only_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join("clipvault-save-1234");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("seg.ts"), b"x").unwrap();
        let other = dir.path().join("my-folder");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(dir.path().join("clipvault-save-file"), b"x").unwrap();
        assert_eq!(clean_stale_save_dirs(dir.path()), 1);
        assert!(!stale.exists() && other.exists() && dir.path().join("clipvault-save-file").exists());
        assert_eq!(clean_stale_save_dirs(&dir.path().join("missing")), 0);
    }

    fn t0() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000)
    }
    fn at(s: f64) -> SystemTime {
        t0() + Duration::from_secs_f64(s)
    }
    fn seg(i: u64, start: f64, end: f64) -> Segment {
        Segment { index: i, path: PathBuf::from(format!("{i}.ts")), start: at(start), end: at(end), layout: 0 }
    }
    fn ev(s: f64, kind: MarkerKind, label: &str) -> BusEvent {
        BusEvent { at: at(s), kind, label: label.into() }
    }

    #[test]
    fn sanitize_illegal_chars() {
        assert_eq!(sanitize_filename("Tom Clancy's: Siege?"), "Tom Clancy's_ Siege_");
        assert_eq!(sanitize_filename("..."), "Unknown");
        assert_eq!(sanitize_filename(""), "Unknown");
        assert_eq!(sanitize_filename("a<b>c\"d/e\\f|g*h"), "a_b_c_d_e_f_g_h");
        assert_eq!(sanitize_filename("tab\there\u{1}"), "tab_here_");
        assert_eq!(sanitize_filename("Game. . "), "Game");
        assert_eq!(sanitize_filename("Rocket League"), "Rocket League");
    }

    #[test]
    fn clip_filename_format() {
        let t = Local.with_ymd_and_hms(2026, 11, 18, 21, 37, 14).unwrap();
        assert_eq!(clip_filename("Rocket League", t), "Rocket-League__2026-11-18__21-37-14.mp4");
        assert_eq!(clip_filename("Tom Clancy's: Siege?", t), "Tom-Clancy's_-Siege___2026-11-18__21-37-14.mp4");
        assert_eq!(clip_filename("", t), "Unknown__2026-11-18__21-37-14.mp4");
    }

    #[test]
    fn markers_relative_to_clip_start() {
        // Window [0, 10] but the buffer only starts at 3 s: markers count from 3 s,
        // not from the window start. Events before the first segment are dropped.
        let segs = [seg(0, 3.0, 5.0), seg(1, 5.0, 7.0), seg(2, 7.0, 9.0), seg(3, 9.0, 11.0)];
        let events =
            [ev(1.0, MarkerKind::Goal, "too early"), ev(5.5, MarkerKind::Goal, "G"), ev(8.0, MarkerKind::Laugh, "")];
        let m = markers_for(&segs, &events, at(10.0));
        assert_eq!(
            m,
            vec![
                Marker { t_s: 2.5, kind: MarkerKind::Goal, label: "G".into() },
                Marker { t_s: 5.0, kind: MarkerKind::Laugh, label: "".into() },
                Marker { t_s: 7.0, kind: MarkerKind::Saved, label: "".into() },
            ]
        );
    }

    #[test]
    fn markers_skip_gaps_like_concat() {
        // Concat removes the 2 s hole between 4 and 6, so later times shift left.
        let segs = [seg(0, 0.0, 2.0), seg(1, 2.0, 4.0), seg(0, 6.0, 8.0)];
        let events = [ev(5.0, MarkerKind::Manual, "in gap"), ev(7.0, MarkerKind::Goal, "after gap")];
        let m = markers_for(&segs, &events, at(9.0));
        let t: Vec<(f64, MarkerKind)> = m.iter().map(|m| (m.t_s, m.kind)).collect();
        // In-gap snaps to the next segment's start; t_press after the last segment clamps to the end.
        assert_eq!(t, vec![(4.0, MarkerKind::Manual), (5.0, MarkerKind::Goal), (6.0, MarkerKind::Saved)]);
    }

    #[test]
    fn concat_list_escapes_paths() {
        let files = [PathBuf::from(r"C:\Users\O'Brien\save dir\0000.ts"), PathBuf::from(r"C:\x\0001.ts")];
        assert_eq!(
            concat_list(&files),
            "ffconcat version 1.0\nfile 'C:/Users/O'\\''Brien/save dir/0000.ts'\nfile 'C:/x/0001.ts'\n"
        );
    }

    #[test]
    fn unique_clip_path_appends_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let name = "G__2026-11-18__21-37-14.mp4";
        assert_eq!(unique_clip_path(dir.path(), name), dir.path().join(name));
        std::fs::write(dir.path().join(name), b"x").unwrap();
        assert_eq!(unique_clip_path(dir.path(), name), dir.path().join("G__2026-11-18__21-37-14_2.mp4"));
        std::fs::write(dir.path().join("G__2026-11-18__21-37-14_2.mp4.tmp"), b"x").unwrap();
        assert_eq!(unique_clip_path(dir.path(), name), dir.path().join("G__2026-11-18__21-37-14_3.mp4"));
    }

    fn service(dir: &Path) -> Arc<SaveService> {
        let (tx, _) = tokio::sync::broadcast::channel(16);
        let sup = Arc::new(Supervisor::new(dir.join("no-ffmpeg.exe"), Duration::from_secs(30), tx));
        sup.set_health_for_test(Health::Restarting { reason: "test".into() });
        let db = Arc::new(Mutex::new(crate::db::open(&dir.join("library.db")).unwrap()));
        let bus = Arc::new(EventBus::new(Duration::from_secs(20)));
        Arc::new(SaveService::new(test_config(dir), test_info(), sup, db, bus))
    }
    fn test_config(dir: &Path) -> SaveConfig {
        SaveConfig {
            ffmpeg: dir.join("no-ffmpeg.exe"),
            capture_dir: dir.join("clips"),
            thumbs_dir: dir.join("thumbs"),
            temp_root: dir.join("tmp"),
            save_length: Duration::from_secs(10),
        }
    }
    fn test_info() -> CaptureInfo {
        CaptureInfo { fps: 60, width: 1280, height: 720, track_titles: vec![], label: "G".into() }
    }

    /// C1: a panic inside one save (poisoning the config lock) must not take
    /// every later save down with it.
    #[tokio::test(flavor = "multi_thread")]
    async fn save_after_a_panicked_save_still_works() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path());
        svc.panic_next_save.store(true, std::sync::atomic::Ordering::SeqCst);
        let s2 = svc.clone();
        let r = tokio::spawn(async move { s2.save(SystemTime::now(), "G").await }).await;
        assert!(r.unwrap_err().is_panic());
        svc.set_config(test_config(dir.path()));
        svc.set_capture_info(0, test_info());
        match svc.save(SystemTime::now(), "G").await {
            SaveOutcome::Failed { reason } => assert!(reason.contains("no recorded video"), "{reason}"),
            o => panic!("{o:?}"),
        }
    }

    fn push_seg(svc: &SaveService, start: SystemTime, end: SystemTime, layout: u64) {
        let path = PathBuf::from(format!("missing-{layout}.ts"));
        locks::lock(&svc.supervisor.segments()).push(Segment { index: 0, path, start, end, layout });
    }

    /// After the game exits capture goes Idle, but its footage is still in the
    /// buffer: a save must try to use it instead of "not recording".
    #[tokio::test(flavor = "multi_thread")]
    async fn idle_save_uses_recent_leftover_segments() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path());
        svc.supervisor.set_health_for_test(Health::Idle);
        let now = SystemTime::now();
        push_seg(&svc, now - Duration::from_secs(4), now - Duration::from_secs(2), 0);
        match svc.save(now, "G").await {
            // The fake segment file does not exist; what matters is that it was tried.
            SaveOutcome::Failed { reason } => assert!(reason.contains("deleted before"), "{reason}"),
            o => panic!("{o:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn idle_save_without_recent_segments_is_not_recording() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path());
        svc.supervisor.set_health_for_test(Health::Idle);
        assert_eq!(svc.save(SystemTime::now(), "G").await, SaveOutcome::NotRecording, "empty buffer");
        let now = SystemTime::now();
        push_seg(&svc, now - Duration::from_secs(60), now - Duration::from_secs(58), 0);
        assert_eq!(svc.save(now, "G").await, SaveOutcome::NotRecording, "only footage older than the save length");
    }

    /// Clip metadata (size, fps, track titles, label) comes from the capture
    /// that recorded the selected segments, not from whatever runs now.
    #[test]
    fn capture_info_is_looked_up_by_layout() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path());
        let game = CaptureInfo { fps: 120, width: 2560, height: 1440, track_titles: vec!["Mic".into()], label: "RL".into() };
        let desk = CaptureInfo { fps: 30, width: 1280, height: 720, track_titles: vec![], label: "Desktop".into() };
        svc.set_capture_info(7, game.clone());
        svc.set_capture_info(9, desk.clone());
        assert_eq!(svc.capture_info_for(7), game);
        assert_eq!(svc.capture_info_for(9), desk);
        assert_eq!(svc.capture_info_for(1), desk, "unknown layout falls back to the current capture");
    }

    /// Quit waits for a running save instead of killing its concat.
    #[tokio::test(flavor = "multi_thread")]
    async fn drain_waits_for_running_saves() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path());
        let turn = svc.queue.lock().await;
        assert!(!svc.drain(Duration::from_millis(200)).await, "a save is still running");
        drop(turn);
        assert!(svc.drain(Duration::from_millis(200)).await);
    }

    /// m2: the concat FFmpeg is in the kill-on-close job, and on a timeout it
    /// is killed and reaped before the caller removes the .tmp it writes.
    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread")]
    async fn concat_timeout_kills_and_reaps_the_process() {
        use std::os::windows::io::RawHandle;
        let ping = std::path::PathBuf::from(std::env::var("SystemRoot").unwrap()).join(r"System32\PING.EXE");
        let args: Vec<String> = ["-n", "30", "127.0.0.1"].iter().map(|s| s.to_string()).collect();
        let mut seen: Option<(u32, bool)> = None;
        let t = Instant::now();
        let r = run_concat_with(&ping, &args, Duration::from_millis(500), |c| {
            let h = c.raw_handle().unwrap() as RawHandle;
            seen = Some((c.id().unwrap(), crate::capture::job::in_kill_job(h)));
        })
        .await;
        assert!(r.unwrap_err().contains("timed out"));
        assert!(t.elapsed() < Duration::from_secs(10));
        let (pid, in_job) = seen.unwrap();
        assert!(in_job, "concat FFmpeg must be in the kill-on-close job");
        let out = std::process::Command::new("tasklist").args(["/FI", &format!("PID eq {pid}"), "/NH"]).output().unwrap();
        let listing = String::from_utf8_lossy(&out.stdout);
        assert!(!listing.contains(&pid.to_string()), "process {pid} still running after the timeout: {listing}");
    }

    #[test]
    fn last_stderr_line_picks_last_nonempty() {
        assert_eq!(last_stderr_line("a\r\nNo space left on device\r\n\r\n").as_deref(), Some("No space left on device"));
        assert_eq!(last_stderr_line("  \n"), None);
    }
}
