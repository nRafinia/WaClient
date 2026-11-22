//! Notification sounds and per-chat exceptions: which sound a message plays,
//! whether its text is shown, and the "always notify" exception for muted chats.
//!
//! Sounds are synthesized (see `audio::render_notes`), so nothing is bundled
//! or downloaded; "Windows default" is the toast's own sound.
//!
//! The engine emits `UiEvent::Notify` for muted chats too, flagged `muted`; the
//! app shows those only for chats with the "always notify" exception.

use slint::{ComponentHandle, ModelRc, VecModel};

use super::{App, ProfileState, with_app};
use crate::audio::{self, Note};
use crate::settings::ChatNotifyPref;
use crate::{MainWindow, NotifyInfo, SoundChoice};

/// No sound.
pub const SOUND_NONE: &str = "none";
/// The toast's own sound (also what notifications used before sounds were selectable).
pub const SOUND_DEFAULT: &str = "default";
/// What the lock screen and masked toasts say instead of the message.
const MASKED_BODY: &str = "New message";

struct Tone {
    id: &'static str,
    label: &'static str,
    notes: &'static [Note],
}

/// Built-in tones: (frequency Hz, start s, length s).
const TONES: [Tone; 5] = [
    Tone { id: "chime", label: "Chime", notes: &[(659.25, 0.0, 0.5), (987.77, 0.12, 0.7)] },
    Tone { id: "bell", label: "Bell", notes: &[(1046.5, 0.0, 1.2), (1568.0, 0.0, 0.8)] },
    Tone { id: "drop", label: "Drop", notes: &[(880.0, 0.0, 0.18), (587.33, 0.12, 0.45)] },
    Tone { id: "pulse", label: "Pulse", notes: &[(784.0, 0.0, 0.12), (784.0, 0.16, 0.12), (1046.5, 0.32, 0.25)] },
    Tone { id: "marimba", label: "Marimba", notes: &[(523.25, 0.0, 0.25), (659.25, 0.1, 0.25), (783.99, 0.2, 0.4)] },
];

fn tone(id: &str) -> Option<&'static Tone> {
    TONES.iter().find(|t| t.id == id)
}

/// The id as a known static one, or `None` for anything unrecognised.
fn known_sound(id: &str) -> Option<&'static str> {
    match id {
        SOUND_NONE => Some(SOUND_NONE),
        SOUND_DEFAULT => Some(SOUND_DEFAULT),
        other => tone(other).map(|t| t.id),
    }
}

/// A stored global tone id, falling back to the default for unknown values.
pub fn valid_tone(id: &str) -> &'static str {
    match known_sound(id) {
        Some(SOUND_NONE) | None => SOUND_DEFAULT,
        Some(known) => known,
    }
}

fn sound_label(id: &str) -> &'static str {
    match id {
        SOUND_NONE => "None",
        SOUND_DEFAULT => "Windows default",
        other => tone(other).map_or("Windows default", |t| t.label),
    }
}

/// The sound a message plays: the chat's own choice, else the global one, else the default.
pub fn resolve_sound(chat: Option<&str>, global_on: bool, global_tone: &str) -> &'static str {
    if let Some(id) = chat.and_then(known_sound) {
        return id;
    }
    if global_on { valid_tone(global_tone) } else { SOUND_NONE }
}

/// Everything that decides whether a message is announced.
pub struct Facts {
    /// The global notifications switch.
    pub enabled: bool,
    pub locked: bool,
    pub muted: bool,
    /// The chat's "always notify" exception.
    pub always: bool,
    pub global_preview: bool,
    pub chat_preview: Option<bool>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Decision {
    pub show: bool,
    /// The message text may be shown (otherwise "New message").
    pub text: bool,
}

pub fn decide(f: &Facts) -> Decision {
    if !f.enabled || (f.muted && !f.always) {
        return Decision { show: false, text: false };
    }
    Decision { show: true, text: !f.locked && f.chat_preview.unwrap_or(f.global_preview) }
}

fn sound_choices(chat: bool) -> Vec<SoundChoice> {
    let item = |id: &str, label: &str| SoundChoice { id: id.into(), label: label.into() };
    let mut v = Vec::new();
    if chat {
        v.push(item("", "Default"));
        v.push(item(SOUND_NONE, sound_label(SOUND_NONE)));
    }
    v.push(item(SOUND_DEFAULT, sound_label(SOUND_DEFAULT)));
    v.extend(TONES.iter().map(|t| item(t.id, t.label)));
    v
}

fn preview_label(pref: Option<bool>, global: bool) -> String {
    match pref {
        Some(true) => "On".to_string(),
        Some(false) => "Off".to_string(),
        None => format!("Default ({})", if global { "on" } else { "off" }),
    }
}

/// Default -> On -> Off -> Default.
fn next_preview(pref: Option<bool>) -> Option<bool> {
    match pref {
        None => Some(true),
        Some(true) => Some(false),
        Some(false) => None,
    }
}

pub(super) fn wire(win: &MainWindow) {
    let g = win.global::<NotifyInfo>();
    g.set_sounds(ModelRc::new(VecModel::from(sound_choices(true))));
    g.set_global_sounds(ModelRc::new(VecModel::from(sound_choices(false))));
    g.on_set_sound(|id| with_app(|a| a.notify_set_sound(id.as_str())));
    g.on_preview_sound(|id| with_app(|a| a.notify_preview(id.as_str())));
    g.on_cycle_preview(|| with_app(App::notify_cycle_preview));
    g.on_set_always(|on| with_app(|a| a.notify_set_always(on)));
}

impl App {
    /// Shows the open chat's notification settings in the info panel.
    pub(super) fn notify_sync(&self) {
        let Some(win) = self.win() else { return };
        let p = self.p();
        let Some(jid) = p.st.borrow().selected.clone() else { return };
        let s = p.settings.borrow();
        let pref = s.chat_notify.get(&jid).cloned().unwrap_or_default();
        let g = win.global::<NotifyInfo>();
        g.set_chat_sound(pref.sound.clone().unwrap_or_default().into());
        let label = match pref.sound.as_deref().and_then(known_sound) {
            Some(id) => sound_label(id).to_string(),
            None => format!("Default ({})", sound_label(resolve_sound(None, s.notification_sound, &s.notification_tone))),
        };
        g.set_sound_label(label.into());
        g.set_preview_label(preview_label(pref.preview, s.notification_preview).into());
        g.set_always(pref.always);
    }

    /// Applies `change` to the open chat's exceptions and stores them.
    fn notify_update(&self, change: impl FnOnce(&mut ChatNotifyPref)) {
        let p = self.p();
        let Some(jid) = p.st.borrow().selected.clone() else { return };
        {
            let mut s = p.settings.borrow_mut();
            let mut pref = s.chat_notify.get(&jid).cloned().unwrap_or_default();
            change(&mut pref);
            if pref.is_default() {
                s.chat_notify.remove(&jid);
            } else {
                s.chat_notify.insert(jid, pref);
            }
        }
        self.mark_settings_dirty(&p);
        self.notify_sync();
    }

    fn notify_set_sound(&self, id: &str) {
        let sound = known_sound(id).map(str::to_string);
        self.notify_update(|pref| pref.sound = sound);
    }

    fn notify_cycle_preview(&self) {
        self.notify_update(|pref| pref.preview = next_preview(pref.preview));
    }

    fn notify_set_always(&self, on: bool) {
        self.notify_update(|pref| pref.always = on);
    }

    /// The ▶ buttons: plays a choice of the info panel ("" = the global sound)
    /// or of Settings. The Windows default has no raw sample here, so it is
    /// previewed with a real toast.
    fn notify_preview(&self, id: &str) {
        let p = self.p();
        let id = if id.is_empty() {
            let s = p.settings.borrow();
            resolve_sound(None, s.notification_sound, &s.notification_tone)
        } else {
            known_sound(id).unwrap_or(SOUND_DEFAULT)
        };
        match id {
            SOUND_NONE => {}
            SOUND_DEFAULT => {
                let jid = p.st.borrow().selected.clone().unwrap_or_default();
                if let Some(d) = self.desktop.borrow().as_ref() {
                    d.notify("WaClient", "Sound preview", &p.id, &jid, true);
                }
            }
            other => {
                if let Some(t) = tone(other) {
                    audio::play_chime(t.notes);
                }
            }
        }
    }

    /// Announces an incoming message. `muted` is the engine's flag for a muted chat.
    pub(super) fn notify_incoming(&self, p: &ProfileState, chat_jid: &str, title: &str, body: &str, muted: bool) {
        let locked = self.is_locked();
        let looking = !locked
            && self.is_active(p)
            && p.st.borrow().selected.as_deref() == Some(chat_jid)
            && self.window_active();
        if looking {
            return;
        }
        let (decision, sound) = {
            let s = p.settings.borrow();
            let pref = s.chat_notify.get(chat_jid);
            let facts = Facts {
                enabled: s.engine.notifications,
                locked,
                muted,
                always: pref.is_some_and(|c| c.always),
                global_preview: s.notification_preview,
                chat_preview: pref.and_then(|c| c.preview),
            };
            let sound = resolve_sound(
                pref.and_then(|c| c.sound.as_deref()),
                s.notification_sound,
                &s.notification_tone,
            );
            (decide(&facts), sound)
        };
        if !decision.show {
            return;
        }
        // Nothing about the chat, the sender or the text while locked.
        let (title, body) = if locked {
            ("WaClient".to_string(), MASKED_BODY.to_string())
        } else {
            let title = if self.multi() { format!("[{}] {title}", p.name()) } else { title.to_string() };
            (title, if decision.text { body.to_string() } else { MASKED_BODY.to_string() })
        };
        if let Some(d) = self.desktop.borrow().as_ref() {
            d.notify(&title, &body, &p.id, chat_jid, sound == SOUND_DEFAULT);
        }
        if let Some(t) = tone(sound) {
            audio::play_chime(t.notes);
        }
    }

    /// Settings page changes that need the notification state refreshed.
    pub(super) fn notify_settings_changed(&self) {
        self.notify_sync();
    }

    /// Debug aid for the demo: `notif-settings` / `notif-sounds` show the info panel
    /// with a chat exception (sound list collapsed / open), `notif-global` the Notifications page of Settings.
    #[cfg(debug_assertions)]
    pub(super) fn demo_notif(&self, what: &str) {
        let Some(win) = self.win() else { return };
        if what == "notif-global" {
            self.open_settings();
            win.set_settings_start_category(3);
            win.set_show_settings(true);
            return;
        }
        self.demo_info("info");
        self.notify_set_sound("bell");
        self.notify_set_always(true);
        win.global::<NotifyInfo>().set_expanded(what == "notif-sounds");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> Facts {
        Facts { enabled: true, locked: false, muted: false, always: false, global_preview: true, chat_preview: None }
    }

    #[test]
    fn a_normal_message_is_shown_with_text() {
        assert_eq!(decide(&facts()), Decision { show: true, text: true });
    }

    #[test]
    fn the_global_switch_silences_everything_even_the_exception() {
        let f = Facts { enabled: false, muted: true, always: true, ..facts() };
        assert!(!decide(&f).show);
    }

    #[test]
    fn muted_chats_are_silent_unless_they_are_an_exception() {
        assert!(!decide(&Facts { muted: true, ..facts() }).show);
        assert!(decide(&Facts { muted: true, always: true, ..facts() }).show);
    }

    #[test]
    fn the_lock_hides_the_text_but_still_notifies() {
        let d = decide(&Facts { locked: true, chat_preview: Some(true), ..facts() });
        assert_eq!(d, Decision { show: true, text: false });
    }

    #[test]
    fn the_chat_preview_choice_beats_the_global_one() {
        assert!(!decide(&Facts { chat_preview: Some(false), ..facts() }).text);
        assert!(decide(&Facts { global_preview: false, chat_preview: Some(true), ..facts() }).text);
        assert!(!decide(&Facts { global_preview: false, ..facts() }).text);
    }

    #[test]
    fn sound_resolution_goes_chat_then_global_then_default() {
        assert_eq!(resolve_sound(Some("bell"), true, "chime"), "bell");
        assert_eq!(resolve_sound(Some(SOUND_NONE), true, "chime"), SOUND_NONE);
        assert_eq!(resolve_sound(None, true, "chime"), "chime");
        assert_eq!(resolve_sound(None, false, "chime"), SOUND_NONE);
        assert_eq!(resolve_sound(None, true, "gone"), SOUND_DEFAULT);
        assert_eq!(resolve_sound(Some("gone"), true, "drop"), "drop");
        assert_eq!(resolve_sound(Some(SOUND_DEFAULT), true, "drop"), SOUND_DEFAULT);
    }

    #[test]
    fn a_stored_global_tone_is_always_playable() {
        assert_eq!(valid_tone("marimba"), "marimba");
        assert_eq!(valid_tone(SOUND_NONE), SOUND_DEFAULT);
        assert_eq!(valid_tone("../etc"), SOUND_DEFAULT);
    }

    #[test]
    fn every_tone_renders_to_audible_samples() {
        for t in &TONES {
            assert!(!audio::render_notes(t.notes, 48_000).is_empty(), "{}", t.id);
        }
        let ids: Vec<&str> = TONES.iter().map(|t| t.id).collect();
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), ids.len());
    }

    #[test]
    fn preview_choice_cycles_through_all_three_states() {
        assert_eq!(next_preview(None), Some(true));
        assert_eq!(next_preview(Some(true)), Some(false));
        assert_eq!(next_preview(Some(false)), None);
        assert_eq!(preview_label(None, false), "Default (off)");
    }

    #[test]
    fn the_lists_offer_default_and_none_only_for_a_chat() {
        let chat = sound_choices(true);
        let global = sound_choices(false);
        assert_eq!(chat[0].id.as_str(), "");
        assert_eq!(chat.len(), global.len() + 2);
        assert!(global.iter().all(|c| !c.id.is_empty() && c.id != SOUND_NONE));
    }
}
