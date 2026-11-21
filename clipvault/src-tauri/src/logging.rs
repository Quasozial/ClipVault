//! `tracing` to `%LOCALAPPDATA%\ClipVault\logs\clipvault.<date>.log` (daily
//! rotation, 7 files kept) and to stderr.
use std::path::Path;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{Builder, Rotation};
use tracing_subscriber::prelude::*;
use tracing_subscriber::{fmt, EnvFilter};

/// Writes every panic (message, location, thread) to the log before the
/// default hook runs: a release build has no console, and the "internal error
/// – see log" toasts point here.
pub fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "(non-string payload)".into());
        let at = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
        tracing::error!("PANIC in thread {:?} at {at}: {msg}", thread.name().unwrap_or("?"));
        default(info);
    }));
}

/// Installs the global subscriber. Keep the guard alive for the whole run
/// (dropping it flushes and stops the file writer).
pub fn init(logs_dir: &Path) -> Option<WorkerGuard> {
    let filter = || EnvFilter::try_from_env("CLIPVAULT_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    // The appender does not create its directory.
    let _ = std::fs::create_dir_all(logs_dir);
    let file = Builder::new()
        .rotation(Rotation::DAILY)
        .filename_prefix("clipvault")
        .filename_suffix("log")
        .max_log_files(7)
        .build(logs_dir);
    match file {
        Ok(appender) => {
            let (writer, guard) = tracing_appender::non_blocking(appender);
            let file_layer = fmt::layer().with_ansi(false).with_writer(writer).with_filter(filter());
            let stderr = fmt::layer().with_writer(std::io::stderr).with_filter(filter());
            let _ = tracing_subscriber::registry().with(file_layer).with(stderr).try_init();
            Some(guard)
        }
        Err(e) => {
            let stderr = fmt::layer().with_writer(std::io::stderr).with_filter(filter());
            let _ = tracing_subscriber::registry().with(stderr).try_init();
            tracing::error!("log file in {} unavailable: {e}", logs_dir.display());
            None
        }
    }
}
