//! Process-level helpers: window focus, logging and housekeeping.

use std::path::Path;
use std::time::{Duration, SystemTime};

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::ComponentHandle;

use crate::MainWindow;

const LOG_FILE: &str = "waclient.log";
const LOG_ROTATE_BYTES: u64 = 5 * 1024 * 1024;
const DEFAULT_FILTER: &str = "warn,waclient=info,wa_engine=info,wa_store=info";
const OUTGOING_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

#[cfg(windows)]
#[link(name = "user32")]
unsafe extern "system" {
    fn GetForegroundWindow() -> isize;
    fn SetForegroundWindow(hwnd: isize) -> i32;
}

pub(crate) fn hwnd(win: &MainWindow) -> Option<isize> {
    let handle = win.window().window_handle();
    match handle.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}

/// True when the window is shown, not minimized and is the foreground window.
#[cfg(windows)]
pub fn is_active(win: &MainWindow) -> bool {
    let w = win.window();
    if !w.is_visible() || w.is_minimized() {
        return false;
    }
    // SAFETY: GetForegroundWindow has no preconditions.
    let fg = unsafe { GetForegroundWindow() };
    hwnd(win).is_some_and(|h| h == fg)
}

#[cfg(not(windows))]
pub fn is_active(win: &MainWindow) -> bool {
    let w = win.window();
    w.is_visible() && !w.is_minimized()
}

/// Shows, restores and focuses the window (used by the tray and toasts).
pub fn bring_to_front(win: &MainWindow) {
    if let Err(e) = win.show() {
        log::error!("could not show the window: {e}");
        return;
    }
    win.window().set_minimized(false);
    #[cfg(windows)]
    if let Some(h) = hwnd(win) {
        // SAFETY: `h` is the HWND of our own window. The call may be refused by
        // the OS foreground lock; that only leaves the window unfocused.
        if unsafe { SetForegroundWindow(h) } == 0 {
            log::debug!("SetForegroundWindow was refused");
        }
    }
}

/// Logs to stderr in debug builds and to `<data_dir>/waclient.log` in release
/// builds (there is no console there).
pub fn init_logging(data_dir: &Path) {
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(DEFAULT_FILTER));
    if !cfg!(debug_assertions) {
        match open_log_file(data_dir) {
            Ok(file) => {
                builder.target(env_logger::Target::Pipe(Box::new(file)));
            }
            Err(e) => eprintln!("could not open the log file: {e}"),
        }
    }
    if let Err(e) = builder.try_init() {
        eprintln!("logger already initialised: {e}");
    }
}

fn open_log_file(dir: &Path) -> std::io::Result<std::fs::File> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(LOG_FILE);
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > LOG_ROTATE_BYTES) {
        let old = dir.join(format!("{LOG_FILE}.1"));
        // Rename fails on Windows if the target exists.
        let _ = std::fs::remove_file(&old);
        std::fs::rename(&path, &old)?;
    }
    std::fs::OpenOptions::new().create(true).append(true).open(path)
}

/// Deletes leftover voice recordings older than 24 hours from
/// `<data_dir>/media/outgoing`. Runs on its own thread.
pub fn spawn_outgoing_cleanup(data_dir: &Path) {
    let dir = data_dir.join("media").join("outgoing");
    let spawned = std::thread::Builder::new()
        .name("outgoing-cleanup".into())
        .spawn(move || cleanup_outgoing(&dir, OUTGOING_MAX_AGE));
    if let Err(e) = spawned {
        log::warn!("could not start the recording cleanup thread: {e}");
    }
}

fn cleanup_outgoing(dir: &Path, max_age: Duration) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            log::warn!("cannot scan {}: {e}", dir.display());
            return;
        }
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let age = meta
            .modified()
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .unwrap_or_default();
        if age > max_age {
            match std::fs::remove_file(&path) {
                Ok(()) => log::info!("removed old recording {}", path.display()),
                Err(e) => log::warn!("could not remove {}: {e}", path.display()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_removes_only_old_files() {
        let dir = std::env::temp_dir().join(format!("waclient-cleanup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let file = dir.join("a.ogg");
        std::fs::write(&file, b"x").expect("write");
        cleanup_outgoing(&dir, Duration::from_secs(3600));
        assert!(file.exists());
        cleanup_outgoing(&dir, Duration::ZERO);
        std::thread::sleep(Duration::from_millis(20));
        cleanup_outgoing(&dir, Duration::ZERO);
        assert!(!file.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
