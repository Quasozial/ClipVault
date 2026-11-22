//! One way to run a short-lived FFmpeg (thumbnails, waveforms, exports,
//! compression, probes): no console window, in the kill-on-close job (dies
//! with ClipVault even on a crash), stderr drained on its own thread (a chatty
//! FFmpeg never blocks on a full pipe), and a timeout after which it is killed
//! and reaped. The capture process (supervisor) and the save concat (async)
//! have their own launchers.
use std::ffi::OsStr;
use std::path::Path;
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// `CREATE_NO_WINDOW`: no console window flashes up.
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// `IDLE_PRIORITY_CLASS`: only CPU time nothing else wants.
const IDLE_PRIORITY_CLASS: u32 = 0x0000_0040;
/// How often a running process is checked for exit / busy / timeout.
const POLL: Duration = Duration::from_millis(20);
/// Slower poll for long background jobs.
const POLL_IDLE: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    /// The user waits for it (thumbnail, export).
    Normal,
    /// Background work (compression).
    Idle,
}

/// How a run ended.
#[derive(Debug)]
pub enum Run {
    Finished { success: bool, stderr: String },
    /// `busy` turned true: the process was killed.
    Busy,
    /// Still running after the timeout: the process was killed.
    TimedOut,
}

/// Runs `program` with `args` and waits for it (see module docs), killing
/// (and reaping) it when `busy()` turns true or after `timeout`. `on_spawn`
/// gets the pid and whether it is in the kill-on-close job (for tests). A
/// spawn error is `Finished { success: false }` with the reason as `stderr`.
pub fn run<S: AsRef<OsStr>>(
    program: &Path,
    args: &[S],
    timeout: Duration,
    priority: Priority,
    busy: &dyn Fn() -> bool,
    on_spawn: impl FnOnce(u32, bool),
) -> Run {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    cmd.creation_flags(match priority {
        Priority::Normal => CREATE_NO_WINDOW,
        Priority::Idle => CREATE_NO_WINDOW | IDLE_PRIORITY_CLASS,
    });
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Run::Finished { success: false, stderr: format!("starting {}: {e}", program.display()) },
    };
    #[cfg(windows)]
    let in_job = {
        use std::os::windows::io::AsRawHandle;
        let h = child.as_raw_handle();
        if let Err(e) = crate::capture::job::assign(h) {
            tracing::warn!("FFmpeg not in kill-on-close job: {e}");
        }
        crate::capture::job::in_kill_job(h)
    };
    #[cfg(not(windows))]
    let in_job = false;
    on_spawn(child.id(), in_job);
    let stderr = child.stderr.take().map(|mut s| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = std::io::Read::read_to_string(&mut s, &mut buf);
            buf
        })
    });
    let poll = if priority == Priority::Idle { POLL_IDLE } else { POLL };
    let deadline = Instant::now() + timeout;
    let stopped = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stderr = stderr.and_then(|t| t.join().ok()).unwrap_or_default();
                return Run::Finished { success: status.success(), stderr };
            }
            Ok(None) => {}
            Err(e) => break Run::Finished { success: false, stderr: format!("waiting for FFmpeg: {e}") },
        }
        if busy() {
            break Run::Busy;
        }
        if Instant::now() >= deadline {
            break Run::TimedOut;
        }
        std::thread::sleep(poll);
    };
    let _ = child.kill();
    let _ = child.wait();
    stopped
}

/// [`run`] at normal priority, never busy; `Ok(stderr)` on success, else an
/// error naming `what` with FFmpeg's last stderr line.
pub fn run_ok<S: AsRef<OsStr>>(program: &Path, args: &[S], timeout: Duration, what: &str) -> anyhow::Result<String> {
    match run(program, args, timeout, Priority::Normal, &|| false, |_, _| {}) {
        Run::Finished { success: true, stderr } => Ok(stderr),
        Run::Finished { success: false, stderr } => {
            let last = crate::save::last_stderr_line(&stderr).unwrap_or_default();
            Err(anyhow::anyhow!("FFmpeg {what} failed: {last}"))
        }
        Run::TimedOut => Err(anyhow::anyhow!("FFmpeg {what} still running after {} s; stopped", timeout.as_secs())),
        Run::Busy => Err(anyhow::anyhow!("FFmpeg {what} interrupted")),
    }
}

/// `ffmpeg -i <path>` stderr (the input description). FFmpeg exits non-zero
/// without an output, so the exit status is ignored; `None` when it did not run.
pub fn describe(ffmpeg: &Path, path: &Path, timeout: Duration) -> Option<String> {
    let args = [OsStr::new("-hide_banner"), OsStr::new("-i"), path.as_os_str()];
    match run(ffmpeg, &args, timeout, Priority::Normal, &|| false, |_, _| {}) {
        Run::Finished { stderr, .. } => Some(stderr),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    fn ping(n: &str) -> (std::path::PathBuf, Vec<String>) {
        let ping = std::path::PathBuf::from(std::env::var("SystemRoot").unwrap()).join(r"System32\PING.EXE");
        (ping, ["-n", n, "127.0.0.1"].iter().map(|s| s.to_string()).collect())
    }

    #[cfg(windows)]
    #[test]
    fn normal_priority_run_is_in_the_job_and_times_out() {
        let (prog, args) = ping("30");
        let mut seen = None;
        let r = run(&prog, &args, Duration::from_millis(400), Priority::Normal, &|| false, |p, j| seen = Some((p, j)));
        assert!(matches!(r, Run::TimedOut), "{r:?}");
        assert!(seen.unwrap().1, "FFmpeg must die with ClipVault");
    }

    #[cfg(windows)]
    #[test]
    fn run_ok_reports_success_and_failure() {
        let (prog, args) = ping("1");
        assert!(run_ok(&prog, &args, Duration::from_secs(20), "ping").is_ok());
        let e = run_ok(Path::new("no-such-ffmpeg.exe"), &args, Duration::from_secs(5), "thumbnail").unwrap_err();
        assert!(e.to_string().contains("FFmpeg thumbnail failed"), "{e}");
    }
}
