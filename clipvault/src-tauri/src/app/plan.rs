//! Pure decisions for the capture controller: where FFmpeg is, output size,
//! what a target is called.
use crate::db::games::Game;
use crate::game_watcher::CaptureTarget;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// FFmpeg lookup order: `CLIPVAULT_FFMPEG`, the Tauri sidecar next to the exe
/// (`ffmpeg.exe` once bundled, `ffmpeg-x86_64-pc-windows-msvc.exe` in a raw
/// build), then `PATH`. Only existing files count.
pub fn resolve_ffmpeg(env: Option<OsString>, exe_dir: Option<&Path>, path_var: Option<OsString>) -> Option<PathBuf> {
    let env = env.filter(|v| !v.is_empty()).map(PathBuf::from);
    let sidecar = exe_dir
        .into_iter()
        .flat_map(|d| [d.join("ffmpeg.exe"), d.join("ffmpeg-x86_64-pc-windows-msvc.exe")]);
    let on_path: Vec<PathBuf> =
        path_var.map(|p| std::env::split_paths(&p).map(|d| d.join("ffmpeg.exe")).collect()).unwrap_or_default();
    env.into_iter().chain(sidecar).chain(on_path).find(|p| p.is_file())
}

/// 16:9 output for `output_height` (e.g. 1080 ⇒ 1920x1080), both even.
pub fn output_size(output_height: u32) -> (u32, u32) {
    let h = output_height.max(2).div_ceil(2) * 2;
    let w = (h * 16).div_ceil(9).div_ceil(2) * 2;
    (w, h)
}

/// Game name used for clips: the game, or `Desktop`; `None` when not capturing.
pub fn target_name(t: &CaptureTarget) -> Option<String> {
    match t {
        CaptureTarget::Game { name, .. } => Some(name.clone()),
        CaptureTarget::Desktop => Some("Desktop".into()),
        CaptureTarget::None => None,
    }
}

/// The RL stats listener runs only while the target is Rocket League: the
/// game is called "Rocket League" / "RocketLeague" (any case), or the game of
/// that name in the library runs `RocketLeague.exe` (whatever it was named).
pub fn is_rocket_league(t: &CaptureTarget, games: &[Game]) -> bool {
    let CaptureTarget::Game { name, .. } = t else { return false };
    let lower = name.trim().to_lowercase();
    lower == "rocket league"
        || lower == "rocketleague"
        || games
            .iter()
            .filter(|g| g.name == *name)
            .any(|g| g.exe_names.iter().any(|e| e.trim().eq_ignore_ascii_case("rocketleague.exe")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffmpeg_lookup_order() {
        let dir = tempfile::tempdir().unwrap();
        let envf = dir.path().join("env").join("ffmpeg.exe");
        let exe_dir = dir.path().join("app");
        let pathdir = dir.path().join("bin dir");
        for d in [envf.parent().unwrap(), &exe_dir, &pathdir] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(&envf, b"").unwrap();
        std::fs::write(pathdir.join("ffmpeg.exe"), b"").unwrap();
        let path_var = std::env::join_paths([dir.path().join("nope"), pathdir.clone()]).unwrap();

        assert_eq!(resolve_ffmpeg(Some(envf.clone().into()), Some(&exe_dir), Some(path_var.clone())), Some(envf.clone()));
        // Env pointing nowhere is skipped.
        assert_eq!(
            resolve_ffmpeg(Some(dir.path().join("x.exe").into()), Some(&exe_dir), Some(path_var.clone())),
            Some(pathdir.join("ffmpeg.exe"))
        );
        std::fs::write(exe_dir.join("ffmpeg-x86_64-pc-windows-msvc.exe"), b"").unwrap();
        assert_eq!(
            resolve_ffmpeg(None, Some(&exe_dir), Some(path_var.clone())),
            Some(exe_dir.join("ffmpeg-x86_64-pc-windows-msvc.exe"))
        );
        std::fs::write(exe_dir.join("ffmpeg.exe"), b"").unwrap();
        assert_eq!(resolve_ffmpeg(None, Some(&exe_dir), Some(path_var)), Some(exe_dir.join("ffmpeg.exe")));
        assert_eq!(resolve_ffmpeg(None, None, None), None);
    }

    #[test]
    fn rocket_league_by_name_or_exe() {
        let target = |n: &str| CaptureTarget::Game { name: n.into(), pid: 7 };
        // Name variants (e.g. added from the process list as "RocketLeague").
        assert!(is_rocket_league(&target("RocketLeague"), &[]));
        assert!(is_rocket_league(&target("ROCKET LEAGUE"), &[]));
        // Any name, as long as the game's exe is RocketLeague.exe.
        let games = [game("RL", &["RocketLeague.exe"]), game("Notepad", &["notepad.exe"])];
        assert!(is_rocket_league(&target("RL"), &games));
        assert!(!is_rocket_league(&target("Notepad"), &games));
        assert!(!is_rocket_league(&target("Rocket"), &games));
        assert!(!is_rocket_league(&CaptureTarget::Desktop, &games));
    }

    #[test]
    fn output_is_16_9_even() {
        assert_eq!(output_size(1080), (1920, 1080));
        assert_eq!(output_size(1440), (2560, 1440));
        assert_eq!(output_size(720), (1280, 720));
        assert_eq!(output_size(1081), (1924, 1082));
    }

    fn game(name: &str, exes: &[&str]) -> Game {
        Game { id: 1, name: name.into(), exe_names: exes.iter().map(|e| e.to_string()).collect(), path: None, enabled: true }
    }

    #[test]
    fn names_and_rl() {
        let rl = CaptureTarget::Game { name: "rocket league".into(), pid: 1 };
        assert!(is_rocket_league(&rl, &[]));
        assert!(!is_rocket_league(&CaptureTarget::Game { name: "Notepad".into(), pid: 1 }, &[]));
        assert!(!is_rocket_league(&CaptureTarget::Desktop, &[]));
        assert_eq!(target_name(&rl).as_deref(), Some("rocket league"));
        assert_eq!(target_name(&CaptureTarget::Desktop).as_deref(), Some("Desktop"));
        assert_eq!(target_name(&CaptureTarget::None), None);
    }
}
