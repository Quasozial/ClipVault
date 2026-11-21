//! Rolling-buffer segment index: tracks FFmpeg MPEG-TS segments, prunes old
//! ones and selects the segments covering a save window.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Gaps between consecutive segments larger than this count as a hole.
const GAP_THRESHOLD: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// Numeric part of the trailing `_NNNNNNNN.ts` in the filename. Indices
    /// restart on every FFmpeg run, so they are NOT unique or ordered across
    /// runs; ordering is always by `start`.
    pub index: u64,
    pub path: PathBuf,
    pub start: SystemTime,
    pub end: SystemTime,
    /// Stream layout key (see `capture::args::layout_key`): only segments with
    /// the same key can be stream-copied into one file.
    pub layout: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub segments: Vec<Segment>,
    pub covered: Duration,
    pub has_gap: bool,
    /// Layout of the selected segments (0 when nothing was selected).
    pub layout: u64,
}

#[derive(Debug)]
pub struct SegmentIndex {
    retention: Duration,
    segments: Vec<Segment>,
}

impl SegmentIndex {
    pub fn new(retention: Duration) -> Self {
        Self { retention, segments: Vec::new() }
    }

    pub fn retention(&self) -> Duration {
        self.retention
    }

    /// Takes effect at the next [`SegmentIndex::prune`].
    pub fn set_retention(&mut self, retention: Duration) {
        self.retention = retention;
    }

    pub fn push(&mut self, seg: Segment) {
        self.segments.push(seg);
    }

    /// Removes segments with `end < now - retention`; returns their paths.
    pub fn prune(&mut self, now: SystemTime) -> Vec<PathBuf> {
        let Some(cutoff) = now.checked_sub(self.retention) else {
            return Vec::new();
        };
        let mut removed = Vec::new();
        self.segments.retain(|s| {
            if s.end < cutoff {
                removed.push(s.path.clone());
                false
            } else {
                true
            }
        });
        removed
    }

    /// Whether a segment with this file is in the index.
    pub fn contains(&self, path: &Path) -> bool {
        self.segments.iter().any(|s| s.path == path)
    }

    pub fn latest_end(&self) -> Option<SystemTime> {
        self.segments.iter().map(|s| s.end).max()
    }

    /// Segments overlapping `[from, to]` (`end > from && start < to`), sorted
    /// by start. `covered` is the total time of those segments clipped to the
    /// window. Segments of different layouts cannot go into one clip: only the
    /// layout covering most of the window is taken (a tie goes to the newer).
    pub fn select(&self, from: SystemTime, to: SystemTime) -> Selection {
        let clipped = |s: &Segment| s.end.min(to).duration_since(s.start.max(from)).unwrap_or(Duration::ZERO);
        let in_window: Vec<&Segment> = self.segments.iter().filter(|s| s.end > from && s.start < to).collect();

        // (layout, covered, latest end) per layout.
        let mut per_layout: Vec<(u64, Duration, SystemTime)> = Vec::new();
        for s in &in_window {
            match per_layout.iter_mut().find(|(l, _, _)| *l == s.layout) {
                Some(e) => {
                    e.1 += clipped(s);
                    e.2 = e.2.max(s.end);
                }
                None => per_layout.push((s.layout, clipped(s), s.end)),
            }
        }
        let layout = per_layout.iter().max_by_key(|(_, covered, end)| (*covered, *end)).map_or(0, |e| e.0);

        let mut segments: Vec<Segment> = in_window.into_iter().filter(|s| s.layout == layout).cloned().collect();
        segments.sort_by_key(|s| s.start);
        let covered = segments.iter().map(clipped).sum();

        let has_gap = segments.windows(2).any(|w| {
            w[1].start
                .duration_since(w[0].end)
                .map(|d| d > GAP_THRESHOLD)
                .unwrap_or(false)
        });

        Selection { segments, covered, has_gap, layout }
    }
}

/// Parses an FFmpeg `segment_list_type csv` line, e.g.
/// `r3_seg_00000042.ts,84.000000,86.000000`. Times are seconds relative to
/// `run_start`. Any filename is accepted and joined to `dir`; the index is
/// the number after the last `_` in the file stem.
pub fn parse_segment_list_line(line: &str, run_start: SystemTime, dir: &Path) -> Option<Segment> {
    let mut parts = line.trim().split(',');
    let name = parts.next()?.trim();
    let start_s: f64 = parts.next()?.trim().parse().ok()?;
    let end_s: f64 = parts.next()?.trim().parse().ok()?;
    if name.is_empty() || !start_s.is_finite() || !end_s.is_finite() || start_s < 0.0 || end_s < start_s {
        return None;
    }
    let stem = Path::new(name).file_stem()?.to_str()?;
    let index: u64 = stem.rsplit('_').next()?.parse().ok()?;
    Some(Segment {
        index,
        path: dir.join(name),
        start: run_start + Duration::from_secs_f64(start_s),
        end: run_start + Duration::from_secs_f64(end_s),
        layout: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000)
    }
    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }
    fn seg(i: u64, start_s: u64, len_s: u64) -> Segment {
        Segment {
            index: i,
            path: PathBuf::from(format!("seg_{i:08}.ts")),
            start: t0() + secs(start_s),
            end: t0() + secs(start_s + len_s),
            layout: 0,
        }
    }
    fn seg_l(i: u64, start_s: u64, len_s: u64, layout: u64) -> Segment {
        Segment { layout, ..seg(i, start_s, len_s) }
    }

    /// Segments of different stream layouts (e.g. a game recorded before a
    /// settings change) cannot be concatenated: the layout covering most of
    /// the window wins, so a save right after the switch keeps the game.
    #[test]
    fn select_takes_the_layout_covering_most_of_the_window() {
        let mut idx = SegmentIndex::new(secs(600));
        for i in 0..20 {
            idx.push(seg_l(i, i * 2, 2, 7)); // 0..40, layout 7
        }
        for i in 0..5 {
            idx.push(seg_l(i, 50 + i * 2, 2, 9)); // 50..60, layout 9
        }
        let sel = idx.select(t0(), t0() + secs(60));
        assert_eq!(sel.layout, 7);
        assert_eq!(sel.segments.len(), 20);
        assert!(sel.segments.iter().all(|s| s.layout == 7));
        assert_eq!(sel.covered, secs(40));
        // 30..60: 10 s of each; a tie goes to the newer layout.
        let sel = idx.select(t0() + secs(30), t0() + secs(60));
        assert_eq!(sel.layout, 9);
        assert_eq!(sel.segments.len(), 5);
        assert_eq!(sel.covered, secs(10));
    }

    #[test]
    fn select_full_window() {
        let mut idx = SegmentIndex::new(secs(600));
        for i in 0..40 {
            idx.push(seg(i, i * 2, 2));
        }
        let end = idx.latest_end().unwrap();
        let sel = idx.select(end - secs(60), end);
        assert_eq!(sel.segments.len(), 30);
        assert_eq!(sel.covered, secs(60));
        assert!(!sel.has_gap);
        assert_eq!(sel.segments[0].index, 10);
    }

    #[test]
    fn select_short_buffer() {
        let mut idx = SegmentIndex::new(secs(600));
        for i in 0..5 {
            idx.push(seg(i, i * 2, 2));
        }
        let end = idx.latest_end().unwrap();
        let sel = idx.select(end - secs(60), end);
        assert_eq!(sel.segments.len(), 5);
        assert_eq!(sel.covered, secs(10));
        assert!(!sel.has_gap);
    }

    #[test]
    fn select_detects_gap_and_sorts_by_start_across_runs() {
        let mut idx = SegmentIndex::new(secs(600));
        // Run 2 pushed first, with restarted indices, to prove sort is by start.
        for i in 0..3 {
            idx.push(seg(i, 12 + i * 2, 2)); // 12..18
        }
        for i in 0..3 {
            idx.push(seg(i, i * 2, 2)); // 0..6, 6 s hole before 12
        }
        let sel = idx.select(t0(), t0() + secs(18));
        assert!(sel.has_gap);
        assert_eq!(sel.segments.len(), 6);
        assert!(sel.segments.windows(2).all(|w| w[0].start <= w[1].start));
        assert_eq!(sel.covered, secs(12));
    }

    #[test]
    fn small_gap_is_tolerated() {
        let mut idx = SegmentIndex::new(secs(600));
        idx.push(seg(0, 0, 2));
        let mut b = seg(1, 2, 2);
        b.start += Duration::from_millis(500);
        idx.push(b);
        assert!(!idx.select(t0(), t0() + secs(10)).has_gap);
    }

    #[test]
    fn prune_respects_retention() {
        let mut idx = SegmentIndex::new(secs(130));
        for i in 0..100 {
            idx.push(seg(i, i * 2, 2));
        }
        let now = t0() + secs(200); // cutoff = t0+70; ends are 2,4,..; end<70 => i<=33
        let removed = idx.prune(now);
        assert_eq!(removed.len(), 34);
        assert!(removed.contains(&PathBuf::from("seg_00000000.ts")));
        assert!(!removed.contains(&PathBuf::from("seg_00000034.ts"))); // end == 70, kept
        assert_eq!(idx.select(t0(), t0() + secs(1000)).segments.len(), 66);
        assert!(idx.prune(now).is_empty());
    }

    #[test]
    fn parse_csv_line() {
        let dir = Path::new("buf");
        let s = parse_segment_list_line("seg_00000042.ts,84.000000,86.000000", t0(), dir).unwrap();
        assert_eq!(s.index, 42);
        assert_eq!(s.start, t0() + secs(84));
        assert_eq!(s.end, t0() + secs(86));
        assert_eq!(s.path, dir.join("seg_00000042.ts"));
    }

    #[test]
    fn parse_csv_line_with_run_prefix() {
        let s = parse_segment_list_line("r3_seg_00000042.ts,84.0,86.0\r\n", t0(), Path::new("buf")).unwrap();
        assert_eq!(s.index, 42);
        assert_eq!(s.path, Path::new("buf").join("r3_seg_00000042.ts"));
    }

    #[test]
    fn parse_rejects_garbage() {
        let d = Path::new("buf");
        assert!(parse_segment_list_line("", t0(), d).is_none());
        assert!(parse_segment_list_line("seg_1.ts,abc,2", t0(), d).is_none());
        assert!(parse_segment_list_line("seg_1.ts,5,2", t0(), d).is_none());
        assert!(parse_segment_list_line("seg_1.ts,1", t0(), d).is_none());
        assert!(parse_segment_list_line("noindex.ts,1,2", t0(), d).is_none());
    }
}
