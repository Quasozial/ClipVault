//! Kill-on-close Job Object for FFmpeg (and any other helper process).
//!
//! `kill_on_drop` only runs when ClipVault shuts down normally. After a crash
//! or a force-kill, Windows does not kill child processes, so an orphaned
//! FFmpeg would keep encoding into the buffer dir and hold an NVENC session.
//! Every child is therefore put into one process-wide job created with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. The job handle is never closed by us;
//! the OS closes it when ClipVault's process ends, which kills every process
//! still in the job.

use std::os::windows::io::RawHandle;
use std::sync::OnceLock;
use windows::core::{BOOL, PCWSTR};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

/// The process-wide job handle (as usize: `HANDLE` is not `Sync`), or why it
/// could not be created.
fn job() -> Result<HANDLE, String> {
    static JOB: OnceLock<Result<usize, String>> = OnceLock::new();
    let r = JOB.get_or_init(|| {
        // SAFETY: creating and configuring a fresh, unnamed job object we own.
        unsafe {
            let job = CreateJobObjectW(None, PCWSTR::null()).map_err(|e| format!("CreateJobObject: {e}"))?;
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
            .map_err(|e| format!("SetInformationJobObject: {e}"))?;
            // Deliberately never closed: the OS closes it when this process ends.
            Ok(job.0 as usize)
        }
    });
    r.clone().map(|h| HANDLE(h as *mut core::ffi::c_void))
}

/// Puts the process behind `handle` (a raw process handle) into the kill-on-close
/// job. Errors are returned so the caller can log them; capture still works.
pub fn assign(handle: RawHandle) -> Result<(), String> {
    let job = job()?;
    // SAFETY: `handle` is a live process handle owned by the caller.
    unsafe { AssignProcessToJobObject(job, HANDLE(handle)) }.map_err(|e| format!("AssignProcessToJobObject: {e}"))
}

/// Whether the process behind `handle` is in ClipVault's kill-on-close job.
pub fn in_kill_job(handle: RawHandle) -> bool {
    let Ok(job) = job() else { return false };
    let mut r = BOOL(0);
    // SAFETY: `handle` is a live process handle owned by the caller.
    unsafe { IsProcessInJob(HANDLE(handle), Some(job), &mut r) }.is_ok() && r.as_bool()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::io::AsRawHandle;
    use std::time::{Duration, Instant};

    fn ping(secs: u32) -> std::process::Child {
        std::process::Command::new("ping")
            .args(["-n", &secs.to_string(), "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap()
    }

    #[test]
    fn child_is_assigned_to_kill_job() {
        let mut c = ping(30);
        assert!(!in_kill_job(c.as_raw_handle()));
        assign(c.as_raw_handle()).unwrap();
        assert!(in_kill_job(c.as_raw_handle()));
        c.kill().unwrap();
    }

    /// Helper half of `job_kills_children_when_owner_dies`: runs only when
    /// started by that test (env var = PID file), spawns a child into its own
    /// job, writes the child's PID to the file and waits to be killed.
    #[test]
    fn job_owner_helper() {
        let Some(pid_file) = std::env::var_os("CLIPVAULT_JOB_HELPER") else { return };
        let c = ping(60);
        assign(c.as_raw_handle()).unwrap();
        // Write + rename so the reader never sees a partial number.
        let tmp = std::path::PathBuf::from(&pid_file).with_extension("tmp");
        std::fs::write(&tmp, c.id().to_string()).unwrap();
        std::fs::rename(&tmp, &pid_file).unwrap();
        std::thread::sleep(Duration::from_secs(60));
    }

    fn process_alive(pid: u32) -> bool {
        use windows::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
        use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
        // SAFETY: plain handle open / wait / close.
        unsafe {
            let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) else { return false };
            let alive = WaitForSingleObject(h, 0) == WAIT_TIMEOUT;
            let _ = CloseHandle(h);
            alive
        }
    }

    /// Force-kills a process that owns the job (like a crashed ClipVault) and
    /// checks that the OS kills the job's child too.
    #[test]
    fn job_kills_children_when_owner_dies() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_file = tmp.path().join("pid");
        let exe = std::env::current_exe().unwrap();
        let mut helper = std::process::Command::new(exe)
            .args(["--exact", "capture::job::tests::job_owner_helper", "--test-threads=1"])
            .env("CLIPVAULT_JOB_HELPER", &pid_file)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let end = Instant::now() + Duration::from_secs(20);
        let pid = loop {
            if let Some(p) = std::fs::read_to_string(&pid_file).ok().and_then(|t| t.trim().parse::<u32>().ok()) {
                break p;
            }
            assert!(Instant::now() < end, "helper wrote no PID");
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(process_alive(pid), "child should run while the job owner lives");
        helper.kill().unwrap(); // TerminateProcess: no destructors, like a crash
        let _ = helper.wait();
        let end = Instant::now() + Duration::from_secs(5);
        while process_alive(pid) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!process_alive(pid), "orphaned child {pid} survived its job owner");
    }
}
