//! Moves engine events from a profile's engine channel onto the Slint event
//! loop, tagged with the profile they belong to.

use std::sync::mpsc;

use slint::Weak;
use wa_engine::UiEventReceiver;

use crate::MainWindow;

/// The returned receiver reports `Disconnected` once the engine has closed its
/// event channel, i.e. when it has finished shutting down.
///
/// If the thread cannot be started the error is returned: the caller must not
/// treat the engine as running with a working event path.
pub fn try_spawn(
    profile: String,
    mut rx: UiEventReceiver,
    window: Weak<MainWindow>,
) -> std::io::Result<mpsc::Receiver<()>> {
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let thread_name = format!("ui-events-{profile}");
    std::thread::Builder::new().name(thread_name).spawn(move || {
        let _done = done_tx;
        while let Some(event) = rx.blocking_recv() {
            let window = window.clone();
            let id = profile.clone();
            let queued = slint::invoke_from_event_loop(move || {
                if window.upgrade().is_some() {
                    crate::app::with_app(|app| app.on_event(&id, event));
                }
            });
            if queued.is_err() {
                return;
            }
        }
        // Lets the UI finish removing a profile whose engine has stopped.
        let queued = slint::invoke_from_event_loop(move || {
            crate::app::with_app(|app| app.on_engine_closed(&profile));
        });
        if let Err(e) = queued {
            log::debug!("engine closure not delivered: {e}");
        }
    })?;
    Ok(done_rx)
}
