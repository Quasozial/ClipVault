//! Rocket League Stats API integration.
//!
//! RL (when `PacketSendRate > 0` in its stats ini) acts as a local TCP server
//! (default port 49123) streaming JSON objects `{"Event": "...", "Data": ...}`.
//! `GoalScored` events become "goal" markers on the [`EventBus`].

use crate::db::clips::MarkerKind;
use crate::db::games::Game;
use crate::events::{BusEvent, EventBus};
use anyhow::{Context, Result};
use serde_json::Value;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

pub const DEFAULT_PORT: u16 = 49123;
const RECONNECT_DELAY: Duration = Duration::from_secs(3);
const MAX_BUFFER: usize = 1024 * 1024;
const INI_SECTION: &str = "[TAGame.MatchStatsExporter_TA]";

#[derive(Debug, Clone, PartialEq)]
pub struct RlEvent {
    pub kind: MarkerKind,
    pub label: String,
}

/// Parses one stats-API message. `Data` may be an object or a JSON-encoded string.
/// Only `GoalScored` yields an event.
pub fn parse_message(raw: &str) -> Option<RlEvent> {
    let v: Value = serde_json::from_str(raw).ok()?;
    parse_value(&v)
}

fn parse_value(v: &Value) -> Option<RlEvent> {
    if v.get("Event")?.as_str()? != "GoalScored" {
        return None;
    }
    let data = match v.get("Data") {
        Some(Value::String(s)) => serde_json::from_str::<Value>(s).ok(),
        Some(d) => Some(d.clone()),
        None => None,
    };
    let name = data
        .as_ref()
        .and_then(|d| d.get("Scorer"))
        .and_then(|s| s.get("Name"))
        .and_then(|n| n.as_str())
        .map(str::trim)
        .filter(|n| !n.is_empty());
    let label = match name {
        Some(n) => format!("Goal \u{2013} {n}"),
        None => "Goal".to_string(),
    };
    Some(RlEvent { kind: MarkerKind::Goal, label })
}

/// Splits an undelimited byte stream into JSON values, keeping the unparsed tail.
#[derive(Default)]
pub struct Framer {
    buf: Vec<u8>,
}

impl Framer {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Value> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        loop {
            let mut consumed = 0usize;
            let mut err = None;
            {
                let mut it = serde_json::Deserializer::from_slice(&self.buf).into_iter::<Value>();
                while let Some(r) = it.next() {
                    match r {
                        Ok(v) => {
                            out.push(v);
                            consumed = it.byte_offset();
                        }
                        Err(e) => {
                            err = Some(e);
                            break;
                        }
                    }
                }
            }
            self.buf.drain(..consumed);
            match err {
                None => {
                    self.buf.clear();
                    break;
                }
                Some(e) if e.is_eof() => break, // partial message: wait for more bytes
                Some(e) => {
                    tracing::warn!("rl_stats: skipping malformed data: {e}");
                    // Drop up to the next '{' after the bad position.
                    let skip = self.buf.iter().skip(1).position(|&b| b == b'{').map(|p| p + 1);
                    match skip {
                        Some(n) => {
                            self.buf.drain(..n);
                        }
                        None => {
                            self.buf.clear();
                            break;
                        }
                    }
                }
            }
        }
        if self.buf.len() > MAX_BUFFER {
            tracing::warn!("rl_stats: buffer exceeded {MAX_BUFFER} bytes, discarding");
            self.buf.clear();
        }
        out
    }
}

/// Connects to the stats socket and pushes goal events; reconnects every 3 s.
pub async fn run_listener(addr: SocketAddr, bus: Arc<EventBus>, cancel: CancellationToken) {
    run_listener_with(addr, bus, cancel, Arc::new(AtomicBool::new(false)), RECONNECT_DELAY).await
}

/// [`run_listener`] that publishes the connection state in `connected`.
pub async fn run_listener_flagged(addr: SocketAddr, bus: Arc<EventBus>, cancel: CancellationToken, connected: Arc<AtomicBool>) {
    run_listener_with(addr, bus, cancel, connected, RECONNECT_DELAY).await
}

/// Like [`run_listener`], exposing connection state (`connected`) for the UI.
pub async fn run_listener_with(
    addr: SocketAddr,
    bus: Arc<EventBus>,
    cancel: CancellationToken,
    connected: Arc<AtomicBool>,
    retry: Duration,
) {
    loop {
        let stream = tokio::select! {
            _ = cancel.cancelled() => break,
            r = TcpStream::connect(addr) => r,
        };
        if let Ok(mut stream) = stream {
            if !connected.swap(true, Ordering::SeqCst) {
                tracing::info!("rl_stats: connected to {addr}");
            }
            let mut framer = Framer::default();
            let mut chunk = [0u8; 8192];
            loop {
                let n = tokio::select! {
                    _ = cancel.cancelled() => {
                        connected.store(false, Ordering::SeqCst);
                        return;
                    }
                    r = stream.read(&mut chunk) => r,
                };
                match n {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        for v in framer.feed(&chunk[..n]) {
                            if let Some(ev) = parse_value(&v) {
                                bus.push(BusEvent { at: SystemTime::now(), kind: ev.kind, label: ev.label });
                            }
                        }
                    }
                }
            }
            if connected.swap(false, Ordering::SeqCst) {
                tracing::info!("rl_stats: disconnected from {addr}");
            }
        }
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(retry) => {}
        }
    }
    connected.store(false, Ordering::SeqCst);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum StatsApiStatus {
    Enabled,
    Disabled,
    NotFound,
}

/// Finds the RL install root (the folder containing `TAGame`).
pub fn find_rl_install(games: &[Game]) -> Option<PathBuf> {
    find_rl_install_in(
        games,
        &[
            PathBuf::from(r"C:\Program Files\Epic Games\rocketleague"),
            PathBuf::from(r"C:\Program Files (x86)\Steam\steamapps\common\rocketleague"),
        ],
    )
}

fn find_rl_install_in(games: &[Game], defaults: &[PathBuf]) -> Option<PathBuf> {
    let from_games = games
        .iter()
        .filter(|g| g.name.eq_ignore_ascii_case("Rocket League"))
        .filter_map(|g| g.path.as_deref())
        .find_map(|p| p.ancestors().find(|a| a.join("TAGame").is_dir()).map(Path::to_path_buf));
    from_games.or_else(|| defaults.iter().find(|d| d.join("TAGame").is_dir()).cloned())
}

/// Config dir where RL keeps the user's stats ini (known-folder Documents).
fn docs_config_dir(docs: &Path) -> PathBuf {
    docs.join("My Games").join("Rocket League").join("TAGame").join("Config")
}

/// Resolves the stats ini. Order: `<Documents>\My Games\Rocket League\TAGame\Config\TAStatsAPI.ini`,
/// `<install>\TAGame\Config\TAStatsAPI.ini`, `<install>\TAGame\Config\DefaultStatsAPI.ini`.
/// If none exists but the Documents config dir does, returns the (not yet existing)
/// Documents path so it can be created. `None` => not found at all.
pub fn resolve_stats_ini(docs_dir: Option<&Path>, rl_install: Option<&Path>) -> Option<PathBuf> {
    let docs_ini = docs_dir.map(|d| docs_config_dir(d).join("TAStatsAPI.ini"));
    let install_dir = rl_install.map(|i| i.join("TAGame").join("Config"));
    let candidates = docs_ini
        .iter()
        .cloned()
        .chain(install_dir.iter().flat_map(|d| [d.join("TAStatsAPI.ini"), d.join("DefaultStatsAPI.ini")]));
    candidates
        .into_iter()
        .find(|p| p.is_file())
        .or_else(|| docs_ini.filter(|p| p.parent().is_some_and(Path::is_dir)))
}

fn section_key(line: &str, key: &str) -> Option<String> {
    let (k, v) = line.split_once('=')?;
    k.trim().eq_ignore_ascii_case(key).then(|| v.trim().to_string())
}

fn is_section(line: &str, name: &str) -> bool {
    line.trim().eq_ignore_ascii_case(name)
}

fn packet_send_rate(text: &str) -> Option<f64> {
    let mut inside = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            inside = is_section(t, INI_SECTION);
        } else if inside {
            if let Some(v) = section_key(t, "PacketSendRate") {
                return v.parse().ok();
            }
        }
    }
    None
}

/// `Port=` from the exporter section of a stats ini, if set and valid.
pub fn stats_port(text: &str) -> Option<u16> {
    let mut inside = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            inside = is_section(t, INI_SECTION);
        } else if inside {
            if let Some(v) = section_key(t, "Port") {
                return v.parse().ok();
            }
        }
    }
    None
}

/// Stats API port from the resolved ini; [`DEFAULT_PORT`] when unknown.
pub fn configured_port(rl_install: Option<&Path>) -> u16 {
    resolve_stats_ini(dirs::document_dir().as_deref(), rl_install)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| stats_port(&t))
        .unwrap_or(DEFAULT_PORT)
}

pub fn stats_api_status(rl_install: Option<&Path>) -> StatsApiStatus {
    stats_api_status_in(dirs::document_dir().as_deref(), rl_install)
}

pub fn stats_api_status_in(docs_dir: Option<&Path>, rl_install: Option<&Path>) -> StatsApiStatus {
    let Some(p) = resolve_stats_ini(docs_dir, rl_install) else { return StatsApiStatus::NotFound };
    if !p.exists() {
        return StatsApiStatus::Disabled; // config dir exists, ini not created yet
    }
    match std::fs::read_to_string(p) {
        Ok(t) if packet_send_rate(&t).is_some_and(|r| r > 0.0) => StatsApiStatus::Enabled,
        Ok(_) => StatsApiStatus::Disabled,
        Err(_) => StatsApiStatus::NotFound,
    }
}

/// Returns `text` with `PacketSendRate=30` set in the exporter section.
fn apply_enable(text: &str) -> String {
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let Some(hdr) = lines.iter().position(|l| is_section(l, INI_SECTION)) else {
        let mut out = text.to_string();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push_str(eol);
        }
        out.push_str(&format!("{INI_SECTION}{eol}PacketSendRate=30{eol}Port={DEFAULT_PORT}{eol}"));
        return out;
    };
    let end = lines[hdr + 1..]
        .iter()
        .position(|l| l.trim().starts_with('['))
        .map_or(lines.len(), |p| hdr + 1 + p);
    let mut out: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
    if let Some(i) = (hdr + 1..end).find(|&i| section_key(lines[i].trim(), "PacketSendRate").is_some()) {
        let ending = &lines[i][lines[i].trim_end_matches(['\r', '\n']).len()..];
        out[i] = format!("PacketSendRate=30{}", if ending.is_empty() { eol } else { ending });
    } else {
        // Insert after the last non-blank line of the section.
        let at = (hdr + 1..end).rev().find(|&i| !lines[i].trim().is_empty()).unwrap_or(hdr) + 1;
        if !out[at - 1].ends_with('\n') {
            out[at - 1].push_str(eol);
        }
        out.insert(at, format!("PacketSendRate=30{eol}"));
    }
    out.concat()
}

/// Sets `PacketSendRate=30` (keeping any `Port`/`WebPort`). Backs an existing ini up first
/// to `<name>.bak`, or `.bak2`, `.bak3`, ... (never overwrites a backup). A missing ini in an
/// existing Documents config dir is created with the section, `PacketSendRate=30`, `Port=49123`.
/// Writes atomically via temp file + rename.
pub fn enable_stats_api(rl_install: Option<&Path>) -> Result<()> {
    enable_stats_api_in(dirs::document_dir().as_deref(), rl_install)
}

pub fn enable_stats_api_in(docs_dir: Option<&Path>, rl_install: Option<&Path>) -> Result<()> {
    let path = resolve_stats_ini(docs_dir, rl_install).context("Stats API ini not found")?;
    let name = path.file_name().unwrap().to_string_lossy().to_string();
    let text = if path.exists() {
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let bak = (1..)
            .map(|n| path.with_file_name(if n == 1 { format!("{name}.bak") } else { format!("{name}.bak{n}") }))
            .find(|p| !p.exists())
            .unwrap();
        std::fs::copy(&path, &bak).context("writing backup")?;
        text
    } else {
        String::new()
    };
    let tmp = path.with_file_name(format!("{name}.tmp"));
    std::fs::write(&tmp, apply_enable(&text)).context("writing temp ini")?;
    std::fs::rename(&tmp, &path).context("replacing ini")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    const GOAL_OBJ: &str = r#"{"Event":"GoalScored","Data":{"MatchGuid":"x","GoalSpeed":80.5,"Scorer":{"Name":"Bob","Shortcut":1,"TeamNum":0}}}"#;
    const GOAL_STR: &str = r#"{"Event":"GoalScored","Data":"{\"Scorer\":{\"Name\":\"Ann\",\"TeamNum\":1}}"}"#;
    const DASH: &str = "\u{2013}";

    #[test]
    fn stats_port_read_from_exporter_section() {
        let ini = "[Other]\nPort=1\n[TAGame.MatchStatsExporter_TA]\nPort = 49200\nWebPort=49124\nPacketSendRate=30\n";
        assert_eq!(stats_port(ini), Some(49200));
        assert_eq!(stats_port("[TAGame.MatchStatsExporter_TA]\nPacketSendRate=30\n"), None);
        assert_eq!(stats_port("[TAGame.MatchStatsExporter_TA]\nPort=abc\n"), None);
        assert_eq!(stats_port("[Other]\nPort=5\n"), None);
    }

    #[test]
    fn parse_goal_object_data() {
        let e = parse_message(GOAL_OBJ).unwrap();
        assert_eq!(e, RlEvent { kind: MarkerKind::Goal, label: format!("Goal {DASH} Bob") });
    }

    #[test]
    fn parse_goal_string_data() {
        assert_eq!(parse_message(GOAL_STR).unwrap().label, format!("Goal {DASH} Ann"));
    }

    #[test]
    fn goal_without_scorer_is_plain_goal() {
        assert_eq!(parse_message(r#"{"Event":"GoalScored","Data":{}}"#).unwrap().label, "Goal");
        assert_eq!(parse_message(r#"{"Event":"GoalScored"}"#).unwrap().label, "Goal");
    }

    #[test]
    fn ignores_other_events() {
        assert!(parse_message(r#"{"Event":"UpdateState","Data":{}}"#).is_none());
        assert!(parse_message("not json").is_none());
    }

    #[test]
    fn framer_handles_split_garbage_and_cap() {
        let mut f = Framer::default();
        let all = format!("{GOAL_OBJ}{GOAL_OBJ}");
        let (a, b) = all.as_bytes().split_at(all.len() - 10);
        assert_eq!(f.feed(a).len(), 1);
        assert_eq!(f.feed(b).len(), 1);
        assert_eq!(f.feed(format!("garbage!!{GOAL_OBJ}").as_bytes()).len(), 1);
        assert!(f.feed(&vec![b'['; 2 * MAX_BUFFER]).is_empty());
        assert_eq!(f.feed(GOAL_OBJ.as_bytes()).len(), 1);
    }

    fn events(bus: &EventBus) -> Vec<BusEvent> {
        bus.between(SystemTime::UNIX_EPOCH, SystemTime::now() + Duration::from_secs(3600))
    }

    async fn wait_for(bus: &EventBus, n: usize) {
        for _ in 0..100 {
            if events(bus).len() >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("expected {n} events, got {}", events(bus).len());
    }

    #[tokio::test]
    async fn listener_reads_concatenated_messages() {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let bus = Arc::new(EventBus::new(Duration::from_secs(60)));
        let cancel = CancellationToken::new();
        let connected = Arc::new(AtomicBool::new(false));
        let h = tokio::spawn(run_listener_with(addr, bus.clone(), cancel.clone(), connected.clone(), Duration::from_millis(50)));
        let (mut s, _) = l.accept().await.unwrap();
        s.write_all(format!("{GOAL_OBJ}{GOAL_STR}").as_bytes()).await.unwrap();
        wait_for(&bus, 2).await;
        let labels: Vec<String> = events(&bus).into_iter().map(|e| e.label).collect();
        assert!(labels.contains(&format!("Goal {DASH} Bob")) && labels.contains(&format!("Goal {DASH} Ann")));
        assert!(connected.load(Ordering::SeqCst));
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(2), h).await.unwrap().unwrap();
        assert!(!connected.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn listener_reconnects() {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let bus = Arc::new(EventBus::new(Duration::from_secs(60)));
        let cancel = CancellationToken::new();
        let h = tokio::spawn(run_listener_with(addr, bus.clone(), cancel.clone(), Arc::new(AtomicBool::new(false)), Duration::from_millis(50)));
        let (mut s, _) = l.accept().await.unwrap();
        s.write_all(GOAL_OBJ.as_bytes()).await.unwrap();
        drop(s);
        let (mut s2, _) = l.accept().await.unwrap();
        s2.write_all(GOAL_STR.as_bytes()).await.unwrap();
        wait_for(&bus, 2).await;
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(2), h).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn listener_cancels_while_sleeping() {
        // Nothing listens: connect fails, listener sleeps; cancel must stop it promptly.
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        drop(l);
        let cancel = CancellationToken::new();
        let bus = Arc::new(EventBus::new(Duration::from_secs(60)));
        let h = tokio::spawn(run_listener(addr, bus, cancel.clone()));
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(1), h).await.unwrap().unwrap();
    }

    fn rl_dir(ini: Option<&str>) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let cfg = d.path().join("TAGame").join("Config");
        std::fs::create_dir_all(&cfg).unwrap();
        if let Some(t) = ini {
            std::fs::write(cfg.join("DefaultStatsAPI.ini"), t).unwrap();
        }
        d
    }

    fn ini_file(d: &tempfile::TempDir) -> PathBuf {
        d.path().join("TAGame/Config/DefaultStatsAPI.ini")
    }

    fn bak(d: &tempfile::TempDir, suffix: &str) -> PathBuf {
        d.path().join(format!("TAGame/Config/DefaultStatsAPI.ini.{suffix}"))
    }

    #[test]
    fn enable_writes_ini_and_backup() {
        let orig = "[Other]\r\nA=1\r\n[TAGame.MatchStatsExporter_TA]\r\nPort=49200\r\nPacketSendRate=0\r\n[Tail]\r\nB=2\r\n";
        let d = rl_dir(Some(orig));
        assert_eq!(stats_api_status_in(None, Some(d.path())), StatsApiStatus::Disabled);
        enable_stats_api_in(None, Some(d.path())).unwrap();
        assert_eq!(stats_api_status_in(None, Some(d.path())), StatsApiStatus::Enabled);
        let new = std::fs::read_to_string(ini_file(&d)).unwrap();
        assert_eq!(new, orig.replace("PacketSendRate=0", "PacketSendRate=30"));
        assert_eq!(std::fs::read_to_string(bak(&d, "bak")).unwrap(), orig);
        // second run keeps the first backup
        enable_stats_api_in(None, Some(d.path())).unwrap();
        assert_eq!(std::fs::read_to_string(bak(&d, "bak")).unwrap(), orig);
        assert!(bak(&d, "bak2").exists());
    }

    #[test]
    fn enable_adds_missing_key_and_section() {
        let d = rl_dir(Some("[TAGame.MatchStatsExporter_TA]\nPort=49123\n\n[Z]\nq=1\n"));
        enable_stats_api_in(None, Some(d.path())).unwrap();
        assert_eq!(
            std::fs::read_to_string(ini_file(&d)).unwrap(),
            "[TAGame.MatchStatsExporter_TA]\nPort=49123\nPacketSendRate=30\n\n[Z]\nq=1\n"
        );
        let d = rl_dir(Some("[Z]\nq=1"));
        enable_stats_api_in(None, Some(d.path())).unwrap();
        assert_eq!(
            std::fs::read_to_string(ini_file(&d)).unwrap(),
            "[Z]\nq=1\n[TAGame.MatchStatsExporter_TA]\nPacketSendRate=30\nPort=49123\n"
        );
        assert_eq!(stats_api_status_in(None, Some(d.path())), StatsApiStatus::Enabled);
    }

    #[test]
    fn status_not_found_when_ini_missing() {
        let d = rl_dir(None);
        assert_eq!(stats_api_status_in(None, Some(d.path())), StatsApiStatus::NotFound);
        assert!(enable_stats_api_in(None, Some(d.path())).is_err());
    }

    fn game(name: &str, path: Option<PathBuf>) -> Game {
        Game { id: 1, name: name.into(), exe_names: vec![], path, enabled: true }
    }

    #[test]
    fn find_install_from_games_exe_path_and_defaults() {
        let d = rl_dir(None);
        let exe = d.path().join("Binaries/Win64/RocketLeague.exe");
        let g = [game("Other", None), game("Rocket League", Some(exe))];
        assert_eq!(find_rl_install_in(&g, &[]), Some(d.path().to_path_buf()));
        let def = [PathBuf::from("C:/nonexistent-rl"), d.path().to_path_buf()];
        assert_eq!(find_rl_install_in(&[], &def), Some(d.path().to_path_buf()));
        assert_eq!(find_rl_install_in(&[game("Rocket League", None)], &[]), None);
    }

    const DOCS_INI: &str = "[TAGame.MatchStatsExporter_TA]
Port=49123
WebPort=49124
PacketSendRate=0
";

    fn docs_dir(ini: Option<&str>) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let cfg = d.path().join("My Games/Rocket League/TAGame/Config");
        std::fs::create_dir_all(&cfg).unwrap();
        if let Some(t) = ini {
            std::fs::write(cfg.join("TAStatsAPI.ini"), t).unwrap();
        }
        d
    }

    fn docs_ini_file(d: &tempfile::TempDir) -> PathBuf {
        d.path().join("My Games/Rocket League/TAGame/Config/TAStatsAPI.ini")
    }

    #[test]
    fn docs_ini_preferred_over_install() {
        let docs = docs_dir(Some(DOCS_INI)); // disabled
        let inst = rl_dir(Some("[TAGame.MatchStatsExporter_TA]
PacketSendRate=30
")); // enabled
        assert_eq!(resolve_stats_ini(Some(docs.path()), Some(inst.path())), Some(docs_ini_file(&docs)));
        assert_eq!(stats_api_status_in(Some(docs.path()), Some(inst.path())), StatsApiStatus::Disabled);
    }

    #[test]
    fn docs_ini_disabled_then_enable() {
        let docs = docs_dir(Some(DOCS_INI));
        assert_eq!(stats_api_status_in(Some(docs.path()), None), StatsApiStatus::Disabled);
        enable_stats_api_in(Some(docs.path()), None).unwrap();
        assert_eq!(stats_api_status_in(Some(docs.path()), None), StatsApiStatus::Enabled);
        assert_eq!(
            std::fs::read_to_string(docs_ini_file(&docs)).unwrap(),
            DOCS_INI.replace("PacketSendRate=0", "PacketSendRate=30")
        );
        let b = docs_ini_file(&docs).with_file_name("TAStatsAPI.ini.bak");
        assert_eq!(std::fs::read_to_string(b).unwrap(), DOCS_INI);
    }

    #[test]
    fn docs_config_dir_without_ini_is_disabled_and_enable_creates() {
        let docs = docs_dir(None);
        assert_eq!(stats_api_status_in(Some(docs.path()), None), StatsApiStatus::Disabled);
        enable_stats_api_in(Some(docs.path()), None).unwrap();
        assert_eq!(
            std::fs::read_to_string(docs_ini_file(&docs)).unwrap(),
            "[TAGame.MatchStatsExporter_TA]
PacketSendRate=30
Port=49123
"
        );
        assert_eq!(stats_api_status_in(Some(docs.path()), None), StatsApiStatus::Enabled);
    }

    #[test]
    fn install_fallback_when_no_docs() {
        let empty_docs = tempfile::tempdir().unwrap(); // no My Games folder
        let inst = rl_dir(Some("[TAGame.MatchStatsExporter_TA]
PacketSendRate=0
"));
        assert_eq!(stats_api_status_in(Some(empty_docs.path()), Some(inst.path())), StatsApiStatus::Disabled);
        assert_eq!(stats_api_status_in(Some(empty_docs.path()), None), StatsApiStatus::NotFound);
        enable_stats_api_in(None, Some(inst.path())).unwrap();
        assert_eq!(stats_api_status_in(None, Some(inst.path())), StatsApiStatus::Enabled);
    }
}
