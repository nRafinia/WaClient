//! Settings page glue: per-profile `settings.json`, engine updates, the
//! app-level effects of each setting, cache cleanup and autostart.
//!
//! App-level settings (close to tray, text size, Enter key) follow the active
//! profile; "start with Windows" is a single registry value shared by all.

use std::cell::RefCell;
use std::path::PathBuf;
use std::time::Duration;

use slint::{ComponentHandle, ModelRc, TimerMode, VecModel};
use wa_engine::Command;

use super::{App, ProfileState, Removal, with_app};
use crate::profiles::Profile;
use crate::settings::{self, AppSettings};
use crate::{MainWindow, SettingsModel, StorageFolder, StorageState, Theme, location};

/// Slider drags change settings continuously; the file is written once they settle.
const SAVE_DELAY: Duration = Duration::from_millis(400);

/// Where a profile's `settings.json` lives. The demo never writes into the
/// real data directory: it uses a temporary folder instead.
pub(super) fn settings_dir(root: &std::path::Path, persist: bool, profile: &Profile) -> PathBuf {
    if persist {
        crate::profiles::data_dir(root, profile)
    } else {
        std::env::temp_dir().join("waclient-demo-settings").join(&profile.id)
    }
}

fn to_model(s: &AppSettings, autostart: bool) -> SettingsModel {
    SettingsModel {
        notifications: s.engine.notifications,
        notification_preview: s.notification_preview,
        notification_sound: s.notification_sound,
        send_read_receipts: s.engine.send_read_receipts,
        send_status_receipts: s.engine.send_status_receipts,
        send_typing: s.engine.send_typing,
        send_presence: s.engine.send_presence,
        send_link_previews: s.engine.send_link_previews,
        auto_download_images: s.engine.auto_download_images,
        auto_download_voice: s.engine.auto_download_voice,
        auto_download_documents: s.engine.auto_download_documents,
        auto_download_max_mb: i32::try_from(s.engine.auto_download_max_mb).unwrap_or(i32::MAX),
        enter_sends: s.enter_sends,
        font_scale_index: i32::try_from(s.font_scale_index()).unwrap_or(1),
        close_to_tray: s.close_to_tray,
        start_with_windows: autostart,
    }
}

fn apply_model(base: &AppSettings, m: &SettingsModel) -> AppSettings {
    let mut s = base.clone();
    s.engine.notifications = m.notifications;
    s.notification_preview = m.notification_preview;
    s.notification_sound = m.notification_sound;
    s.engine.send_read_receipts = m.send_read_receipts;
    s.engine.send_status_receipts = m.send_status_receipts;
    s.engine.send_typing = m.send_typing;
    s.engine.send_presence = m.send_presence;
    s.engine.send_link_previews = m.send_link_previews;
    s.engine.auto_download_images = m.auto_download_images;
    s.engine.auto_download_voice = m.auto_download_voice;
    s.engine.auto_download_documents = m.auto_download_documents;
    s.engine.auto_download_max_mb = u32::try_from(m.auto_download_max_mb).unwrap_or(1);
    s.enter_sends = m.enter_sends;
    // Keeps a stored scale that is between the choices until the user picks one.
    let index = usize::try_from(m.font_scale_index).unwrap_or(1);
    if index != s.font_scale_index() {
        s.set_font_scale_index(index);
    }
    s.close_to_tray = m.close_to_tray;
    s.start_minimized_with_windows = m.start_with_windows;
    s
}

impl App {
    /// Whether the app currently starts with Windows. The demo has no registry
    /// access, so it just remembers the toggle.
    fn autostart_state(&self, s: &AppSettings) -> bool {
        if self.persist { settings::autostart_enabled() } else { s.start_minimized_with_windows }
    }

    pub fn close_to_tray(&self) -> bool {
        self.close_to_tray.get()
    }

    pub(super) fn open_settings(&self) {
        let Some(win) = self.win() else { return };
        let p = self.p();
        let model = to_model(&p.settings.borrow(), self.autostart_state(&p.settings.borrow()));
        win.set_settings(model);
        win.set_settings_profile(p.name().into());
        win.set_cache_status("".into());
        self.refresh_storage();
        self.account_settings_opened();
    }

    pub(super) fn settings_changed(&self, m: SettingsModel) {
        let Some(win) = self.win() else { return };
        let p = self.p();
        let prev = p.settings.borrow().clone();
        let mut next = apply_model(&prev, &m).clamped();
        let wanted = m.start_with_windows;
        if self.persist && wanted != settings::autostart_enabled() {
            let done = std::env::current_exe()
                .map_err(anyhow::Error::from)
                .and_then(|exe| settings::set_autostart(wanted, &exe));
            if let Err(e) = done {
                log::error!("could not change start with Windows: {e:#}");
                self.show_banner(1, &format!("Could not change start with Windows: {e:#}"));
            }
        }
        // The registry is the truth (a failed write reverts the toggle).
        next.start_minimized_with_windows = if self.persist { settings::autostart_enabled() } else { wanted };
        if next != prev {
            if next.engine != prev.engine {
                (p.send)(Command::UpdateSettings(next.engine.to_engine()));
            }
            *p.settings.borrow_mut() = next.clone();
            self.settings_dirty.borrow_mut().insert(p.id.clone());
            self.settings_timer.start(TimerMode::SingleShot, SAVE_DELAY, || with_app(App::flush_settings));
            self.apply_profile_prefs(&p);
        }
        win.set_settings(to_model(&next, next.start_minimized_with_windows));
    }

    /// Remembers the audience of the last status post for this profile.
    pub(super) fn remember_status_audience(&self, pref: settings::StatusAudiencePref) {
        let p = self.p();
        if p.settings.borrow().status_audience == pref {
            return;
        }
        p.settings.borrow_mut().status_audience = pref;
        self.settings_dirty.borrow_mut().insert(p.id.clone());
        self.settings_timer.start(TimerMode::SingleShot, SAVE_DELAY, || with_app(App::flush_settings));
    }

    /// Writes the settings of every profile changed since the last write.
    pub(super) fn flush_settings(&self) {
        self.settings_timer.stop();
        let ids: Vec<String> = self.settings_dirty.borrow_mut().drain().collect();
        for id in ids {
            // A profile being removed must not get its folder recreated.
            let Some(p) = self.find(&id).filter(|p| p.removal.get() == Removal::No) else { continue };
            let dir = self.profile_dir(&p);
            let saved = settings::save(&dir, &p.settings.borrow());
            if let Err(e) = saved {
                log::error!("could not save the settings of {id}: {e:#}");
                self.show_banner(1, &format!("Could not save settings: {e:#}"));
            }
        }
    }

    /// Applies the settings that act on the window itself.
    pub(super) fn apply_profile_prefs(&self, p: &ProfileState) {
        let Some(win) = self.win() else { return };
        let s = p.settings.borrow();
        win.global::<Theme>().set_font_scale(s.font_scale);
        win.set_enter_sends(s.enter_sends);
        self.close_to_tray.set(s.close_to_tray);
        self.translate_sync(&s.engine);
    }

    pub(super) fn profile_dir(&self, p: &ProfileState) -> PathBuf {
        settings_dir(&self.root, self.persist, &p.meta.borrow())
    }

    pub(super) fn settings_logout(&self) {
        let id = self.p().id.clone();
        self.request_remove(&id);
    }
}

thread_local! {
    /// The validated move waiting for the user's confirmation.
    static MOVE_PLAN: RefCell<Option<location::Plan>> = const { RefCell::new(None) };
}

impl App {
    /// Wires the Storage rows of the settings page. Not used by the demo, which
    /// has no data folder of its own (the rows then stay hidden).
    pub fn install_storage(&self, win: &MainWindow, bootstrap: PathBuf) {
        let st = win.global::<StorageState>();
        st.on_open_folder(|path| with_app(|a| a.storage_open(path.as_str())));
        let choose_bootstrap = bootstrap.clone();
        st.on_choose_location(move || with_app(|a| a.storage_choose(&choose_bootstrap)));
        st.on_confirm_move(move || with_app(|a| a.storage_confirm(&bootstrap)));
        st.on_cancel_move(|| with_app(App::storage_cancel));
        self.refresh_storage();
    }

    /// The data folder plus every profile's folder, as shown in Settings.
    fn storage_folders(&self) -> Vec<(String, PathBuf)> {
        self.registry
            .borrow()
            .profiles
            .iter()
            .map(|p| (p.name.clone(), crate::profiles::data_dir(&self.root, p)))
            .collect()
    }

    fn refresh_storage(&self) {
        let Some(win) = self.win() else { return };
        if !self.persist {
            return;
        }
        let st = win.global::<StorageState>();
        let folders: Vec<StorageFolder> = self
            .storage_folders()
            .into_iter()
            .map(|(name, path)| StorageFolder { name: name.into(), path: path.display().to_string().into() })
            .collect();
        st.set_folders(ModelRc::new(VecModel::from(folders)));
        st.set_data_path(self.root.display().to_string().into());
        st.set_size_text("Calculating size…".into());
        st.set_status("".into());
        st.set_confirm_text("".into());
        MOVE_PLAN.with(|p| *p.borrow_mut() = None);
        let root = self.root.clone();
        let spawned = std::thread::Builder::new().name("storage-size".into()).spawn(move || {
            let size = location::data_size(&root);
            let queued = slint::invoke_from_event_loop(move || with_app(|a| a.storage_size(&root, size)));
            if let Err(e) = queued {
                log::debug!("storage size dropped: {e}");
            }
        });
        if let Err(e) = spawned {
            log::warn!("could not start the size calculation: {e}");
            st.set_size_text("".into());
        }
    }

    fn storage_size(&self, root: &std::path::Path, size: u64) {
        let Some(win) = self.win() else { return };
        if root == self.root {
            win.global::<StorageState>().set_size_text(format!("{} used", location::fmt_bytes(size)).into());
        }
    }

    /// Only the data folder and the profile folders listed in Settings are
    /// ever handed to Explorer, whatever string the UI passes.
    fn storage_open(&self, path: &str) {
        let wanted = std::path::Path::new(path);
        let known = wanted == self.root || self.storage_folders().iter().any(|(_, p)| p == wanted);
        if !known {
            log::warn!("refusing to open an unlisted folder");
            return;
        }
        if let Err(e) = super::launch(wanted, false) {
            self.show_banner(1, &format!("Could not open the folder: {e}"));
        }
    }

    fn storage_choose(&self, bootstrap: &std::path::Path) {
        let Some(win) = self.win() else { return };
        let st = win.global::<StorageState>();
        if st.get_busy() {
            return;
        }
        st.set_confirm_text("".into());
        MOVE_PLAN.with(|p| *p.borrow_mut() = None);
        // A new move would race the deletion of the previous one's old data.
        if let Some(old) = location::pending_cleanup(bootstrap) {
            st.set_status(format!("Previous move cleanup pending: {}. Restart WaClient to finish it.", old.display()).into());
            return;
        }
        st.set_busy(true);
        st.set_status("".into());
        let root = self.root.clone();
        let bootstrap = bootstrap.to_path_buf();
        let spawned = std::thread::Builder::new().name("pick-folder".into()).spawn(move || {
            let picked = rfd::FileDialog::new().set_title("Choose where to store the WaClient data").pick_folder();
            // Walking the data and probing the disk must not block the UI.
            let result = picked.map(|dir| location::check_target(&bootstrap, &root, &dir));
            let queued = slint::invoke_from_event_loop(move || with_app(|a| a.storage_checked(result)));
            if let Err(e) = queued {
                log::error!("could not deliver the folder check to the UI: {e}");
            }
        });
        if let Err(e) = spawned {
            st.set_busy(false);
            self.show_banner(1, &format!("Could not open the folder picker: {e}"));
        }
    }

    fn storage_checked(&self, result: Option<Result<location::Plan, String>>) {
        let Some(win) = self.win() else { return };
        let st = win.global::<StorageState>();
        st.set_busy(false);
        match result {
            None => {}
            Some(Err(why)) => st.set_status(why.into()),
            Some(Ok(plan)) => {
                st.set_confirm_text(
                    format!(
                        "WaClient will restart and move {} to {}. This can take a while; keep the computer on until it finishes.",
                        location::fmt_bytes(plan.size),
                        plan.target.display()
                    )
                    .into(),
                );
                MOVE_PLAN.with(|p| *p.borrow_mut() = Some(plan));
            }
        }
    }

    fn storage_cancel(&self) {
        MOVE_PLAN.with(|p| *p.borrow_mut() = None);
        if let Some(win) = self.win() {
            win.global::<StorageState>().set_confirm_text("".into());
        }
    }

    /// Records the move and quits; `main` relaunches once the engines are down,
    /// and the new process waits for this one to exit before it starts moving.
    fn storage_confirm(&self, bootstrap: &std::path::Path) {
        let Some(plan) = MOVE_PLAN.with(|p| p.borrow_mut().take()) else { return };
        let Some(win) = self.win() else { return };
        let st = win.global::<StorageState>();
        st.set_confirm_text("".into());
        let now = chrono::Utc::now().timestamp();
        match location::begin_move(bootstrap, &self.root, &plan.target, now) {
            Ok(()) => {
                log::info!("moving the data to {}", location::short(&plan.target));
                location::request_relaunch();
                self.quit();
            }
            Err(e) => {
                log::error!("could not record the move: {e}");
                st.set_status(format!("Could not start the move: {e}").into());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_roundtrip_keeps_every_field() {
        let mut s = AppSettings::default();
        s.engine.auto_download_max_mb = 42;
        s.engine.send_presence = false;
        s.engine.send_link_previews = true;
        s.enter_sends = false;
        s.close_to_tray = false;
        s.notification_sound = false;
        s.set_font_scale_index(2);
        let m = to_model(&s, true);
        let mut back = apply_model(&s, &m);
        assert!(back.start_minimized_with_windows);
        back.start_minimized_with_windows = s.start_minimized_with_windows;
        assert_eq!(back, s);
    }

    #[test]
    fn model_edits_reach_the_settings() {
        let s = AppSettings::default();
        let mut m = to_model(&s, false);
        m.notification_preview = false;
        m.auto_download_max_mb = 500;
        m.font_scale_index = 0;
        let next = apply_model(&s, &m).clamped();
        assert!(!next.notification_preview);
        assert_eq!(next.engine.auto_download_max_mb, 100);
        assert_eq!(next.font_scale, settings::FONT_SCALES[0]);
    }

    #[test]
    fn demo_settings_stay_out_of_the_data_dir() {
        let p = Profile {
            id: "abc12345".into(),
            name: "x".into(),
            color: "#000000".into(),
            created_at: 0,
            order: 0,
            own_jid: None,
            in_root: false,
            migrating: false,
            broken: None,
        };
        let root = std::path::Path::new("root");
        assert_eq!(settings_dir(root, true, &p), root.join("profiles").join("abc12345"));
        assert!(settings_dir(root, false, &p).starts_with(std::env::temp_dir()));
    }
}
