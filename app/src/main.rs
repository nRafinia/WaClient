#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod audio;
mod bridge;
mod decode;
#[cfg(debug_assertions)]
mod demo;
mod desktop;
mod drop_paste;
mod emoji;
mod format;
mod geo;
mod handoff;
mod links;
mod location;
mod models;
mod native_dialog;
mod profiles;
mod qr;
mod settings;
mod shell;
mod single_instance;
mod status_view;
mod thumbs;
#[cfg(test)]
mod ui_check;

slint::include_modules!();

use std::path::PathBuf;
use std::rc::Rc;

use slint::{CloseRequestResponse, ComponentHandle};
use wa_engine::EngineConfig;

/// The fixed folder for the few files needed before the data location is
/// known: `location.json`, the log and the notification icon. All other data
/// lives under the data root that `location.json` names (see `location`).
fn bootstrap_dir() -> PathBuf {
    match std::env::var_os("APPDATA") {
        Some(appdata) => PathBuf::from(appdata).join("WaClient"),
        None => PathBuf::from("data"),
    }
}

/// Claims the instance. With `wait`, a briefly lingering predecessor is given
/// time to release it before giving up.
fn acquire_primary(name: &str, wait: std::time::Duration) -> Option<single_instance::Guard> {
    let deadline = std::time::Instant::now() + wait;
    loop {
        match single_instance::acquire(name) {
            single_instance::Instance::Primary(guard) => return Some(guard),
            single_instance::Instance::Secondary => {
                if std::time::Instant::now() >= deadline {
                    single_instance::signal_primary(name);
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
    }
}

/// Runs the recorded data move (and old-copy cleanup) before anything opens a
/// database, showing a small progress window while it works.
fn run_location_job(bootstrap: &std::path::Path) -> location::MoveReport {
    let window = match MovingWindow::new() {
        Ok(w) => w,
        Err(e) => {
            log::warn!("no progress window ({e}); moving the data without one");
            return location::run_startup(bootstrap, &location::Ops::real(), &mut |_| {});
        }
    };
    // The window must not be closed away while files are being copied.
    window.window().on_close_requested(|| CloseRequestResponse::KeepWindowShown);
    let result = std::sync::Arc::new(std::sync::Mutex::new(None));
    let weak = window.as_weak();
    let worker_result = result.clone();
    let dir = bootstrap.to_path_buf();
    let spawned = std::thread::Builder::new().name("data-move".into()).spawn(move || {
        let report = location::run_startup(&dir, &location::Ops::real(), &mut |p| {
            let (label, fraction) = (p.label(), p.fraction());
            let _ = weak.upgrade_in_event_loop(move |w| {
                w.set_status(label.into());
                w.set_progress(fraction);
            });
        });
        *worker_result.lock().unwrap_or_else(|e| e.into_inner()) = Some(report);
        if let Err(e) = slint::invoke_from_event_loop(|| {
            if let Err(e) = slint::quit_event_loop() {
                log::error!("could not stop the progress loop: {e}");
            }
        }) {
            log::error!("could not finish the progress window: {e}");
        }
    });
    if let Err(e) = spawned {
        log::error!("could not start the move thread: {e}");
        return location::run_startup(bootstrap, &location::Ops::real(), &mut |_| {});
    }
    if let Err(e) = window.show().and_then(|()| slint::run_event_loop_until_quit()) {
        log::error!("progress window failed: {e}");
    }
    if let Err(e) = window.hide() {
        log::debug!("could not hide the progress window: {e}");
    }
    let report = result.lock().unwrap_or_else(|e| e.into_inner()).take();
    report.unwrap_or_else(|| location::MoveReport {
        error: Some("moving the data was interrupted unexpectedly".into()),
        ..location::MoveReport::default()
    })
}

/// Banner text for what the start-up job did: (kind, text); kind 1 notice, 2 error.
fn report_banner(report: &location::MoveReport) -> Option<(i32, String)> {
    let mut parts = Vec::new();
    if let Some(to) = &report.moved {
        parts.push(format!("Your data now lives in {}.", to.display()));
    }
    for old in &report.leftover {
        parts.push(format!("Old copy at {} could not be fully removed.", old.display()));
    }
    parts.extend(report.notices.iter().cloned());
    if let Some(why) = &report.error {
        let text = format!("Could not move the data: {why}. Your data stays where it was.");
        parts.insert(0, text);
        return Some((2, parts.join("  ")));
    }
    (!parts.is_empty()).then(|| (1, parts.join("  ")))
}

/// Starts a fresh copy of this program (without the autostart flag, so its
/// window is shown). It is told to wait for this process to exit before it
/// takes the instance guard or touches any data.
fn relaunch() {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            log::error!("cannot relaunch: {e}");
            return;
        }
    };
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let args = handoff::successor_args(&args, settings::MINIMIZED_FLAG, std::process::id());
    if let Err(e) = std::process::Command::new(exe).args(args).spawn() {
        log::error!("could not relaunch: {e}");
    }
}

/// Where this run keeps its data.
enum RootChoice {
    /// The configured (or default) data root is usable.
    Ready,
    /// The configured root (or `location.json`) is unusable and the user chose
    /// to continue this session without it.
    SessionDefault { unavailable: std::path::PathBuf, location_unreadable: bool },
    Quit,
}

/// Where a "use default for this session" run keeps its data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionFolder {
    /// The bootstrap folder: the pre-move default, nothing stale in it.
    Bootstrap,
    /// A fresh empty temp folder: the bootstrap folder may hold a stale copy of
    /// data that was moved away (or its state cannot be known), and must not
    /// be opened or written to.
    Temp,
}

fn session_folder(pending_cleanup: Option<&std::path::Path>, location_unreadable: bool) -> SessionFolder {
    if pending_cleanup.is_some() || location_unreadable { SessionFolder::Temp } else { SessionFolder::Bootstrap }
}

fn session_banner(unavailable: &std::path::Path, folder: SessionFolder) -> String {
    match folder {
        SessionFolder::Bootstrap => format!("Using the default folder because {} is unavailable.", unavailable.display()),
        SessionFolder::Temp => {
            format!("Using a temporary empty session because {} is unavailable.", unavailable.display())
        }
    }
}

/// An empty per-process session folder that is removed again on drop.
struct SessionTemp(PathBuf);

impl SessionTemp {
    fn create() -> std::io::Result<SessionTemp> {
        let dir = std::env::temp_dir().join(format!("WaClient-session-{}", std::process::id()));
        // Only ever our own pid-named folder: a leftover of an earlier process
        // with the same pid must not leak into this session.
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        std::fs::create_dir_all(&dir)?;
        Ok(SessionTemp(dir))
    }
}

impl Drop for SessionTemp {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.0) {
            log::warn!("could not remove the temporary session folder: {e}");
        }
    }
}

/// The folder, banner and (for a temp session) cleanup guard of a run that
/// continues without the configured data location. Writes nothing to the
/// bootstrap folder or to the unavailable root.
fn begin_session_default(
    bootstrap: &std::path::Path,
    unavailable: &std::path::Path,
    location_unreadable: bool,
) -> Result<(PathBuf, (i32, String), Option<SessionTemp>), Box<dyn std::error::Error>> {
    let pending = location::pending_cleanup(bootstrap);
    let folder = session_folder(pending.as_deref(), location_unreadable);
    let banner = (2, session_banner(unavailable, folder));
    Ok(match folder {
        SessionFolder::Bootstrap => (bootstrap.to_path_buf(), banner, None),
        SessionFolder::Temp => {
            let temp = SessionTemp::create()?;
            (temp.0.clone(), banner, Some(temp))
        }
    })
}

/// Repeats `check` against a freshly read `location.json` until it passes or
/// the user decides what to do about an unavailable configured root or an
/// unreadable `location.json`. With no configured root there is nothing to
/// offer a choice for: the error is returned.
fn guard_root(
    bootstrap: &std::path::Path,
    check: impl Fn(&location::Location) -> Result<(), location::RootError>,
) -> Result<RootChoice, Box<dyn std::error::Error>> {
    loop {
        let loc = match location::load(bootstrap) {
            Ok(loc) => loc,
            Err(e) => {
                let path = bootstrap.join(location::LOCATION_FILE);
                match native_dialog::location_unreadable(&path, &e.to_string()) {
                    native_dialog::RootChoice::Retry => continue,
                    native_dialog::RootChoice::UseDefault => {
                        return Ok(RootChoice::SessionDefault { unavailable: path, location_unreadable: true });
                    }
                    native_dialog::RootChoice::Quit => return Ok(RootChoice::Quit),
                }
            }
        };
        let Err(problem) = check(&loc) else { return Ok(RootChoice::Ready) };
        log::error!("data folder unavailable: {} ({})", problem.path.display(), problem.reason);
        if loc.configured_root().is_none() {
            return Err(format!("cannot use {}: {}", problem.path.display(), problem.reason).into());
        }
        match native_dialog::root_unavailable(&problem.path, &problem.reason) {
            native_dialog::RootChoice::Retry => continue,
            native_dialog::RootChoice::UseDefault => {
                return Ok(RootChoice::SessionDefault { unavailable: problem.path, location_unreadable: false });
            }
            native_dialog::RootChoice::Quit => return Ok(RootChoice::Quit),
        }
    }
}

#[cfg(debug_assertions)]
fn demo_requested() -> bool {
    std::env::var("WACLIENT_DEMO").is_ok_and(|v| v == "1")
}

#[cfg(not(debug_assertions))]
fn demo_requested() -> bool {
    false
}

/// One independent engine (own runtime thread, session, store and media
/// folder) per profile.
///
/// Fails closed: while the proxy settings are unusable and the user has not
/// chosen to connect without one, no engine is started (a direct connection
/// would expose the user's IP address).
fn engine_starter(startup: app::proxy_ui::StartupProxy) -> app::Starter {
    Box::new(move |profile, dir, saved| {
        anyhow::ensure!(profile.is_ready(), "the profile is not ready to be opened");
        let proxy = app::proxy_ui::engine_proxy(&startup).map_err(|why| anyhow::anyhow!("not connecting: {why}"))?;
        let (handle, events) =
            wa_engine::start_with_settings(EngineConfig { data_dir: dir.to_path_buf(), proxy: proxy.clone() }, saved.engine.to_engine())?;
        let store = handle.store().clone();
        Ok(app::ProfileRuntime {
            store,
            send: Rc::new(move |cmd| handle.send(cmd)),
            events: Some(events),
        })
    })
}

/// Debug-only demo: two profiles with different data, nothing persisted.
/// `WACLIENT_DEMO_ACTIVE=<n>` starts on the n-th profile.
#[cfg(debug_assertions)]
fn demo_setup(root: &std::path::Path) -> Result<(profiles::Registry, app::Starter), Box<dyn std::error::Error>> {
    let now = chrono::Utc::now().timestamp();
    let mut registry = profiles::Registry::default();
    let personal = registry.add_in(root, Some("Personal"), now)?;
    let work = registry.add_in(root, Some("Work"), now)?;
    let start_on = std::env::var("WACLIENT_DEMO_ACTIVE")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .and_then(|i| registry.profiles.get(i))
        .map(|p| p.id.clone());
    registry.last_active = start_on.or(Some(personal.id.clone()));
    let starter: app::Starter = Box::new(move |profile, _dir, _saved| {
        let store = if profile.id == personal.id {
            demo::store()
        } else if profile.id == work.id {
            demo::work_store()
        } else {
            wa_engine::Store::open_in_memory().map(std::sync::Arc::new).map_err(Into::into)
        }
        .map_err(|e| anyhow::anyhow!("{e}"))?;
        let demo_store = store.clone();
        let profile_id = profile.id.clone();
        Ok(app::ProfileRuntime {
            store,
            // Stands in for the engine: logs the command and applies the chat
            // flags to the store, then reports ChatsChanged as the engine does.
            send: Rc::new(move |cmd| {
                log::info!("demo: command {}", demo::command_name(&cmd));
                if demo::apply_chat_flags(&demo_store, &cmd) {
                    let id = profile_id.clone();
                    slint::Timer::single_shot(std::time::Duration::from_millis(0), move || {
                        app::with_app(|a| a.on_event(&id, wa_engine::UiEvent::ChatsChanged));
                    });
                }
                for ev in demo::apply_msg_ops(&demo_store, &cmd) {
                    let id = profile_id.clone();
                    slint::Timer::single_shot(std::time::Duration::from_millis(0), move || {
                        app::with_app(|a| a.on_event(&id, ev));
                    });
                }
            }),
            events: None,
        })
    });
    Ok((registry, starter))
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            // The release build has no console: without this the failure is silent.
            log::error!("WaClient failed to start: {e}");
            native_dialog::fatal("WaClient could not start", &e.to_string());
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    // First, before logging, profiles or migration: a second process must not
    // touch the databases of the running one. The guard lives for the whole run.
    let demo = demo_requested();
    let instance_name = if demo { "WaClient-demo" } else { "WaClient" };
    let bootstrap = bootstrap_dir();
    // A relaunch after "move my data" must not touch anything until the
    // process that asked for it has fully exited and closed its databases.
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let wait_pid = if demo { None } else { handoff::parse_wait_pid(&args) };
    let handoff = handoff::handoff_outcome(wait_pid, |pid| handoff::wait_for_exit(pid, handoff::PARENT_EXIT_TIMEOUT));
    let Some(guard) = acquire_primary(instance_name, handoff.guard_wait()) else {
        return Ok(());
    };
    shell::init_logging(&bootstrap);
    // Before the registry and any engine: finish a recorded data move. The
    // demo never touches the real location.
    let mut session_default = false;
    // Removes the temporary session folder when `run` ends.
    let mut _session_temp: Option<SessionTemp> = None;
    let (dir, banner) = if demo {
        (bootstrap.clone(), None)
    } else {
        match guard_root(&bootstrap, location::check_root_present)? {
            RootChoice::Quit => return Ok(()),
            RootChoice::SessionDefault { unavailable, location_unreadable } => {
                // Nothing is moved, swept or written: location.json stays as it is.
                session_default = true;
                let (dir, banner, temp) = begin_session_default(&bootstrap, &unavailable, location_unreadable)?;
                _session_temp = temp;
                (dir, Some(banner))
            }
            RootChoice::Ready => {
                let report = if !handoff.move_allowed() {
                    location::MoveReport {
                        error: Some(
                            "the previous WaClient process did not exit in time, so the move was not started; it will be retried the next time WaClient starts"
                                .into(),
                        ),
                        ..location::MoveReport::default()
                    }
                } else if location::load(&bootstrap).map_or(true, |l| l.needs_startup_work()) {
                    run_location_job(&bootstrap)
                } else {
                    location::MoveReport::default()
                };
                if let Some(why) = &report.error {
                    log::error!("data move failed: {why}");
                }
                // An unreadable file is never read as "default root".
                (location::load(&bootstrap)?.root(&bootstrap), report_banner(&report))
            }
        }
    };
    // The move may have changed the root, so the profiles folder is ensured
    // afterwards (and never before the old copy was checked against the root).
    let (dir, banner) = if demo || session_default {
        (dir, banner)
    } else {
        match guard_root(&bootstrap, |loc| location::create_profiles_root(loc, &bootstrap))? {
            RootChoice::Quit => return Ok(()),
            RootChoice::SessionDefault { unavailable, location_unreadable } => {
                session_default = true;
                let (dir, banner, temp) = begin_session_default(&bootstrap, &unavailable, location_unreadable)?;
                _session_temp = temp;
                (dir, Some(banner))
            }
            RootChoice::Ready => (dir, banner),
        }
    };
    log::info!("data folder: {}", location::short(&dir));
    log::debug!("data folder (full path): {}", dir.display());
    let window = MainWindow::new()?;

    let desktop = match desktop::Desktop::init(&bootstrap) {
        Ok(d) => Some(d),
        Err(e) => {
            log::warn!("tray and notifications unavailable: {e:#}");
            None
        }
    };
    let has_tray = desktop.is_some();
    if has_tray {
        // Closing the window keeps the app (and the engines) running in the
        // tray, unless the active profile's "close to tray" setting is off.
        window.window().on_close_requested(|| {
            let mut keep_running = true;
            app::with_app(|a| keep_running = a.close_to_tray());
            if !keep_running {
                let queued = slint::invoke_from_event_loop(|| app::with_app(app::App::quit));
                if let Err(e) = queued {
                    log::error!("could not queue quit: {e}");
                }
            }
            CloseRequestResponse::HideWindow
        });
    }

    let app = if demo {
        #[cfg(debug_assertions)]
        {
            let (registry, starter) = demo_setup(&dir)?;
            let app = app::App::install(&window, dir.clone(), registry, false, starter)?;
            for id in app.profile_ids() {
                app.on_event(&id, wa_engine::UiEvent::SessionRestored);
                app.on_event(&id, wa_engine::UiEvent::Connection(wa_engine::ConnState::Connected));
            }
            app
        }
        #[cfg(not(debug_assertions))]
        {
            return Err("demo mode is not available in release builds".into());
        }
    } else {
        std::fs::create_dir_all(&dir)?;
        let registry = profiles::open(&dir);
        app::App::install(&window, dir.clone(), registry, true, engine_starter(app::proxy_ui::startup_proxy(&dir)))?
    };
    if let Some(d) = desktop {
        app.attach_desktop(d);
    }
    if !demo {
        // A session on the fallback folder must not offer to move data that
        // is not the configured one.
        // Without a regular bootstrap folder (APPDATA unset) a move could not
        // be recorded safely, so the rows stay hidden.
        if !session_default && location::bootstrap_supports_move(&bootstrap) {
            app.install_storage(&window, bootstrap.clone());
        }
        if let Some((kind, text)) = banner {
            window.set_banner_text(text.into());
            window.set_banner_kind(kind);
        }
    }
    // A second launch asks this instance to show its window.
    guard.on_activate(|| {
        let queued = slint::invoke_from_event_loop(|| app::with_app(app::App::show_window));
        if let Err(e) = queued {
            log::debug!("activation request dropped: {e}");
        }
    });

    // Debug aid: `WACLIENT_DEMO_SWITCH_MS=<ms>` cycles through the demo
    // profiles through the normal switch path, so it can be checked unattended.
    #[cfg(debug_assertions)]
    let _switcher = demo
        .then(|| std::env::var("WACLIENT_DEMO_SWITCH_MS").ok().and_then(|v| v.parse::<u64>().ok()))
        .flatten()
        .map(|ms| {
            let timer = slint::Timer::default();
            let next = std::cell::Cell::new(0usize);
            timer.start(slint::TimerMode::Repeated, std::time::Duration::from_millis(ms), move || {
                with_switch(&next);
            });
            timer
        });

    #[cfg(debug_assertions)]
    let _demo_ui = demo.then(|| std::env::var("WACLIENT_DEMO_UI").ok()).flatten().map(|what| {
        let timer = slint::Timer::default();
        let ms = std::env::var("WACLIENT_DEMO_UI_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(800);
        timer.start(slint::TimerMode::SingleShot, std::time::Duration::from_millis(ms), move || {
            app::with_app(|a| a.demo_open(&what));
        });
        timer
    });

    // Autostart launches with the flag: start in the tray, window hidden. Without
    // a tray there would be no way to bring it back, so it is shown then.
    let start_hidden = has_tray && std::env::args().any(|a| a == settings::MINIMIZED_FLAG);
    if has_tray {
        if !start_hidden {
            window.show()?;
        }
        slint::run_event_loop_until_quit()?;
    } else {
        window.run()?;
    }
    // Without a tray nothing has asked the engines to stop yet; give them a
    // moment to disconnect cleanly before the process exits.
    app::with_app(app::App::shutdown_and_wait);
    if location::relaunch_requested() {
        // The successor waits (by pid) until this process has fully exited, so
        // the guard is not released early; exiting closes our database handles.
        relaunch();
        std::process::exit(0);
    }
    Ok(())
}

#[cfg(debug_assertions)]
fn with_switch(next: &std::cell::Cell<usize>) {
    app::with_app(|a| {
        let ids = a.profile_ids();
        if ids.is_empty() {
            return;
        }
        let i = next.get() % ids.len();
        next.set(i + 1);
        a.activate(&ids[i]);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn a_pending_cleanup_or_unreadable_location_forces_a_temp_session() {
        assert_eq!(session_folder(None, false), SessionFolder::Bootstrap);
        assert_eq!(session_folder(Some(Path::new(r"E:\old\WaClient")), false), SessionFolder::Temp);
        assert_eq!(session_folder(None, true), SessionFolder::Temp);
    }

    #[test]
    fn the_banners_name_the_unavailable_path() {
        let p = Path::new(r"E:\gone\WaClient");
        let temp = session_banner(p, SessionFolder::Temp);
        assert!(temp.starts_with("Using a temporary empty session because") && temp.contains(r"E:\gone\WaClient"));
        assert!(session_banner(p, SessionFolder::Bootstrap).starts_with("Using the default folder"));
    }

    #[test]
    fn the_temp_session_folder_is_empty_and_removed_on_drop() {
        let dir = {
            let temp = SessionTemp::create().expect("create");
            assert!(temp.0.is_dir());
            assert_eq!(std::fs::read_dir(&temp.0).expect("list").count(), 0);
            temp.0.clone()
        };
        assert!(!dir.exists());
    }
}
