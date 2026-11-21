//! Game watcher: decides what the replay buffer should capture (a game, the
//! desktop, or nothing) from the list of running processes.
//!
//! Named `game_watcher` (not `games`) to avoid confusion with `db::games`.
//!
//! Rules (see `GameWatcher::tick`):
//! - Match only on `exe_names` (case-insensitive file names; entries holding a
//!   path are reduced to their file name). If `exe_names` is empty, the file
//!   name of `path` is used. No launcher logic.
//! - First enabled running game in slice order wins; an already-active game
//!   that is still running is kept (no flip-flopping).
//! - When the active game disappears the target is held for `GRACE` (10 s).
//!   If it reappears (any pid) within the grace, it stays active.
//! - After the grace: `Desktop` if `desktop`, else `None`. Toggling `desktop`
//!   applies on the next tick with no grace. A game starting while Desktop is
//!   active switches immediately.
//! - The first tick always returns `Some(target)`.
//! - A pid-only change of the same game is NOT reported (the supervisor does
//!   not need a restart); the stored pid is still updated internally.

use crate::db::games::Game;
use std::path::Path;
use std::time::{Duration, Instant};

pub const GRACE: Duration = Duration::from_secs(10);

pub trait ProcessSource {
    /// (pid, lowercase exe file name)
    fn running_exes(&self) -> Vec<(u32, String)>;
}

#[derive(Debug, Clone, PartialEq)]
pub enum CaptureTarget {
    None,
    Game { name: String, pid: u32 },
    Desktop,
}

/// Real process source backed by `sysinfo` (process list only).
pub struct SysinfoProcesses {
    /// `None` if sysinfo failed to initialise (then no processes are reported).
    sys: std::cell::RefCell<Option<sysinfo::System>>,
}

impl SysinfoProcesses {
    pub fn new() -> Self {
        Self { sys: std::cell::RefCell::new(sysinfo::System::new().ok()) }
    }
}

impl Default for SysinfoProcesses {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessSource for SysinfoProcesses {
    fn running_exes(&self) -> Vec<(u32, String)> {
        let mut guard = self.sys.borrow_mut();
        let Some(sys) = guard.as_mut() else { return Vec::new() };
        sys.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::All,
            true,
            sysinfo::ProcessRefreshKind::nothing(),
        );
        sys.processes()
            .iter()
            .filter_map(|(pid, p)| {
                let pid = pid.as_u32();
                if pid == 0 || pid == 4 {
                    return None;
                }
                Some((pid, p.name().to_string_lossy().to_lowercase()))
            })
            .collect()
    }
}

fn file_name_lower(s: &str) -> String {
    // Handle both separators regardless of host platform.
    s.rsplit(|c: char| c == char::from(92u8) || c == '/').next().unwrap_or(s).trim().to_lowercase()
}

fn game_exes(g: &Game) -> Vec<String> {
    let mut v: Vec<String> = g
        .exe_names
        .iter()
        .map(|e| file_name_lower(e))
        .filter(|e| !e.is_empty())
        .collect();
    if v.is_empty() {
        if let Some(n) = g.path.as_deref().and_then(Path::to_str).map(file_name_lower).filter(|n| !n.is_empty()) {
            v.push(n);
        }
    }
    // Easy Anti-Cheat: `Game_EAC.exe` is only a bootstrap that starts
    // `Game.exe` (e.g. Rocket League); Moments stores the bootstrap's path.
    let real: Vec<String> = v
        .iter()
        .filter_map(|e| e.strip_suffix("_eac.exe").filter(|b| !b.is_empty()).map(|b| format!("{b}.exe")))
        .collect();
    v.extend(real);
    v
}

/// pid of the first running process matching `g`, if any.
fn find_running(g: &Game, running: &[(u32, String)]) -> Option<u32> {
    if !g.enabled {
        return None;
    }
    let exes = game_exes(g);
    running.iter().find(|(_, n)| exes.iter().any(|e| e == n)).map(|(p, _)| *p)
}

fn same_target(a: &CaptureTarget, b: &CaptureTarget) -> bool {
    match (a, b) {
        (CaptureTarget::Game { name: x, .. }, CaptureTarget::Game { name: y, .. }) => x == y,
        _ => a == b,
    }
}

#[derive(Debug)]
pub struct GameWatcher {
    /// (game id, name, pid) of the active game, if any.
    active: Option<(i64, String, u32)>,
    gone_since: Option<Instant>,
    current: Option<CaptureTarget>,
}

impl Default for GameWatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl GameWatcher {
    pub fn new() -> Self {
        Self { active: None, gone_since: None, current: None }
    }

    /// Returns `Some` only when the target changed (always on the first tick).
    pub fn tick(
        &mut self,
        now: Instant,
        games: &[Game],
        desktop: bool,
        procs: &dyn ProcessSource,
    ) -> Option<CaptureTarget> {
        let running = procs.running_exes();

        // Keep the active game while it runs, or during the grace period.
        if let Some((id, _, _)) = self.active.clone() {
            let pid = games.iter().find(|g| g.id == id).and_then(|g| find_running(g, &running));
            match pid {
                Some(pid) => {
                    self.gone_since = None;
                    if let Some(a) = self.active.as_mut() {
                        a.2 = pid;
                    }
                }
                None => {
                    let since = *self.gone_since.get_or_insert(now);
                    if now.saturating_duration_since(since) >= GRACE {
                        self.active = None;
                        self.gone_since = None;
                    }
                }
            }
        }

        // No active game: first enabled running game in slice order.
        if self.active.is_none() {
            self.active = games
                .iter()
                .find_map(|g| find_running(g, &running).map(|p| (g.id, g.name.clone(), p)));
            self.gone_since = None;
        }

        let target = match &self.active {
            Some((_, name, pid)) => CaptureTarget::Game { name: name.clone(), pid: *pid },
            None if desktop => CaptureTarget::Desktop,
            None => CaptureTarget::None,
        };

        let changed = match &self.current {
            None => true,
            Some(cur) => !same_target(cur, &target),
        };
        self.current = Some(target.clone());
        changed.then_some(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::games::Game;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    struct Fake(Vec<(u32, String)>);
    impl ProcessSource for Fake {
        fn running_exes(&self) -> Vec<(u32, String)> {
            self.0.clone()
        }
    }
    fn procs(v: &[(u32, &str)]) -> Fake {
        Fake(v.iter().map(|(p, n)| (*p, n.to_string())).collect())
    }
    fn game(id: i64, name: &str, exes: &[&str], enabled: bool) -> Game {
        Game {
            id,
            name: name.into(),
            exe_names: exes.iter().map(|s| s.to_string()).collect(),
            path: None,
            enabled,
        }
    }
    fn g(name: &str, pid: u32) -> CaptureTarget {
        CaptureTarget::Game { name: name.into(), pid }
    }
    fn s(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// Moments stores the Easy Anti-Cheat bootstrap (`RocketLeague_EAC.exe`);
    /// the process that actually runs is `RocketLeague.exe`.
    #[test]
    fn eac_bootstrap_exe_matches_the_real_game_exe() {
        let mut w = GameWatcher::new();
        let games = [game(1, "Rocket League", &["rocketleague_eac.exe"], true)];
        let r = w.tick(Instant::now(), &games, false, &procs(&[(42, "rocketleague.exe")]));
        assert_eq!(r, Some(g("Rocket League", 42)));
        let mut w = GameWatcher::new();
        let r = w.tick(Instant::now(), &games, false, &procs(&[(7, "rocketleague_eac.exe")]));
        assert_eq!(r, Some(g("Rocket League", 7)));
    }

    #[test]
    fn detects_enabled_game() {
        let mut w = GameWatcher::new();
        let t0 = Instant::now();
        let games = [game(1, "RL", &["rocketleague.exe"], true)];
        let r = w.tick(t0, &games, false, &procs(&[(10, "explorer.exe"), (42, "rocketleague.exe")]));
        assert_eq!(r, Some(g("RL", 42)));
        // unchanged -> None
        assert_eq!(w.tick(t0 + s(2), &games, false, &procs(&[(42, "rocketleague.exe")])), None);
    }

    #[test]
    fn ignores_disabled_game() {
        let mut w = GameWatcher::new();
        let games = [game(1, "RL", &["rocketleague.exe"], false)];
        let r = w.tick(Instant::now(), &games, false, &procs(&[(42, "rocketleague.exe")]));
        assert_eq!(r, Some(CaptureTarget::None));
    }

    #[test]
    fn first_tick_always_reports() {
        let mut w = GameWatcher::new();
        assert_eq!(w.tick(Instant::now(), &[], false, &procs(&[])), Some(CaptureTarget::None));
    }

    #[test]
    fn grace_10s_before_stop() {
        let mut w = GameWatcher::new();
        let t0 = Instant::now();
        let games = [game(1, "RL", &["rocketleague.exe"], true)];
        w.tick(t0, &games, false, &procs(&[(42, "rocketleague.exe")]));
        let none = procs(&[]);
        assert_eq!(w.tick(t0 + s(2), &games, false, &none), None);
        assert_eq!(w.tick(t0 + s(11), &games, false, &none), None);
        assert_eq!(w.tick(t0 + s(12), &games, false, &none), Some(CaptureTarget::None));
        assert_eq!(w.tick(t0 + s(14), &games, false, &none), None);
    }

    #[test]
    fn reappear_within_grace_resets_and_pid_change_is_silent() {
        let mut w = GameWatcher::new();
        let t0 = Instant::now();
        let games = [game(1, "RL", &["rocketleague.exe"], true)];
        w.tick(t0, &games, false, &procs(&[(42, "rocketleague.exe")]));
        assert_eq!(w.tick(t0 + s(2), &games, false, &procs(&[])), None);
        assert_eq!(w.tick(t0 + s(6), &games, false, &procs(&[(77, "rocketleague.exe")])), None);
        // grace was reset: gone again at +8, still held at +17, released at +18
        assert_eq!(w.tick(t0 + s(8), &games, false, &procs(&[])), None);
        assert_eq!(w.tick(t0 + s(17), &games, false, &procs(&[])), None);
        assert_eq!(w.tick(t0 + s(18), &games, false, &procs(&[])), Some(CaptureTarget::None));
    }

    #[test]
    fn desktop_when_no_game_and_enabled() {
        let mut w = GameWatcher::new();
        let t0 = Instant::now();
        let games = [game(1, "RL", &["rocketleague.exe"], true)];
        assert_eq!(w.tick(t0, &games, true, &procs(&[])), Some(CaptureTarget::Desktop));
        // toggling off takes effect next tick, no grace
        assert_eq!(w.tick(t0 + s(2), &games, false, &procs(&[])), Some(CaptureTarget::None));
        assert_eq!(w.tick(t0 + s(4), &games, true, &procs(&[])), Some(CaptureTarget::Desktop));
    }

    #[test]
    fn desktop_after_grace_when_game_exits() {
        let mut w = GameWatcher::new();
        let t0 = Instant::now();
        let games = [game(1, "RL", &["rocketleague.exe"], true)];
        w.tick(t0, &games, true, &procs(&[(42, "rocketleague.exe")]));
        assert_eq!(w.tick(t0 + s(2), &games, true, &procs(&[])), None);
        assert_eq!(w.tick(t0 + s(12), &games, true, &procs(&[])), Some(CaptureTarget::Desktop));
    }

    #[test]
    fn game_overrides_desktop() {
        let mut w = GameWatcher::new();
        let t0 = Instant::now();
        let games = [game(1, "RL", &["rocketleague.exe"], true)];
        assert_eq!(w.tick(t0, &games, true, &procs(&[])), Some(CaptureTarget::Desktop));
        assert_eq!(
            w.tick(t0 + s(2), &games, true, &procs(&[(5, "rocketleague.exe")])),
            Some(g("RL", 5))
        );
    }

    #[test]
    fn exe_match_case_insensitive() {
        let mut w = GameWatcher::new();
        let games = [game(1, "RL", &["RocketLeague.EXE"], true)];
        let r = w.tick(Instant::now(), &games, false, &procs(&[(9, "rocketleague.exe")]));
        assert_eq!(r, Some(g("RL", 9)));
    }

    #[test]
    fn path_entries_normalised_and_path_fallback() {
        let mut w = GameWatcher::new();
        let games = [game(1, "A", &[r"C:\Games\A\a.exe"], true)];
        assert_eq!(w.tick(Instant::now(), &games, false, &procs(&[(3, "a.exe")])), Some(g("A", 3)));

        let mut w = GameWatcher::new();
        let mut b = game(2, "B", &[], true);
        b.path = Some(PathBuf::from(r"D:\x\B.exe"));
        assert_eq!(w.tick(Instant::now(), &[b], false, &procs(&[(4, "b.exe")])), Some(g("B", 4)));
    }

    #[test]
    fn first_in_slice_wins_and_active_is_sticky() {
        let mut w = GameWatcher::new();
        let t0 = Instant::now();
        let games = [game(1, "A", &["a.exe"], true), game(2, "B", &["b.exe"], true)];
        assert_eq!(
            w.tick(t0, &games, false, &procs(&[(1, "a.exe"), (2, "b.exe")])),
            Some(g("A", 1))
        );
        // only B running first, then A appears: B stays
        let mut w = GameWatcher::new();
        assert_eq!(w.tick(t0, &games, false, &procs(&[(2, "b.exe")])), Some(g("B", 2)));
        assert_eq!(w.tick(t0 + s(2), &games, false, &procs(&[(1, "a.exe"), (2, "b.exe")])), None);
    }

    #[test]
    fn switches_to_other_game_after_grace() {
        let mut w = GameWatcher::new();
        let t0 = Instant::now();
        let games = [game(1, "A", &["a.exe"], true), game(2, "B", &["b.exe"], true)];
        w.tick(t0, &games, false, &procs(&[(1, "a.exe"), (2, "b.exe")]));
        let only_b = procs(&[(2, "b.exe")]);
        assert_eq!(w.tick(t0 + s(2), &games, false, &only_b), None);
        assert_eq!(w.tick(t0 + s(12), &games, false, &only_b), Some(g("B", 2)));
    }
}
