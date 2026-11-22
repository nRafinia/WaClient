//! Read-receipt privacy (the 👻 incognito toggle, per-chat receipts, "mark as
//! read now") and contacts' online / last-seen presence.

use std::time::Duration;

use chrono::{DateTime, Local};
use slint::{Timer, TimerMode};
use wa_engine::{Command, ConnState};
use wa_store::ChatKind;

use super::{App, ProfileState, with_app};
use crate::models;
use crate::settings::EngineSettingsDto;

/// "online" is dropped to "last seen" when nothing newer arrived for this long.
const ONLINE_TTL_SECS: i64 = 300;
const PRESENCE_TICK: Duration = Duration::from_secs(30);

thread_local! {
    static PRESENCE_TIMER: Timer = Timer::default();
}

/// What the engine last reported about a contact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PresenceEntry {
    pub online: bool,
    pub last_seen: Option<i64>,
    /// Unix seconds when the report arrived.
    pub received_at: i64,
}

/// Incognito means neither chat nor status read receipts go out.
pub fn incognito_active(e: &EngineSettingsDto) -> bool {
    !e.send_read_receipts && !e.send_status_receipts
}

/// Switches both receipt settings together; off restores them to the default (on).
pub fn set_incognito(e: &mut EngineSettingsDto, on: bool) {
    e.send_read_receipts = !on;
    e.send_status_receipts = !on;
}

/// Header subtitle for a contact: "online", a "last seen ..." text, or empty.
pub(super) fn presence_subtitle(entry: Option<&PresenceEntry>, now: DateTime<Local>) -> String {
    let Some(e) = entry else { return String::new() };
    if e.online {
        if now.timestamp() - e.received_at < ONLINE_TTL_SECS {
            return "online".to_string();
        }
        return models::format_last_seen(e.received_at, now);
    }
    e.last_seen.map(|ts| models::format_last_seen(ts, now)).unwrap_or_default()
}

fn is_dm(jid: &str) -> bool {
    ChatKind::from_jid(jid) == ChatKind::Dm
}

impl App {
    /// Pushes the active profile's receipt settings into the window.
    pub(super) fn refresh_privacy_ui(&self) {
        let Some(win) = self.win() else { return };
        let p = self.p();
        let e = p.settings.borrow().engine.clone();
        win.set_incognito(incognito_active(&e));
        win.set_receipts_global_off(!e.send_read_receipts);
        if win.get_show_settings() {
            let mut m = win.get_settings();
            m.send_read_receipts = e.send_read_receipts;
            m.send_status_receipts = e.send_status_receipts;
            win.set_settings(m);
        }
    }

    pub(super) fn toggle_incognito(&self) {
        let p = self.p();
        let mut next = p.settings.borrow().clone();
        let on = !incognito_active(&next.engine);
        set_incognito(&mut next.engine, on);
        (p.send)(Command::UpdateSettings(next.engine.to_engine()));
        *p.settings.borrow_mut() = next;
        self.settings_dirty.borrow_mut().insert(p.id.clone());
        self.flush_settings();
        self.refresh_privacy_ui();
    }

    pub(super) fn set_chat_receipts(&self, jid: &str, enabled: bool) {
        self.send(Command::SetChatReceipts { chat_jid: jid.to_string(), enabled });
    }

    pub(super) fn read_receipts_now(&self, jid: &str) {
        if !self.active_connected() {
            self.show_toast("You're offline — read receipts will not be sent");
            return;
        }
        self.send(Command::SendReadReceiptsNow { chat_jid: jid.to_string() });
        self.show_toast("Read receipts sent");
    }

    fn active_connected(&self) -> bool {
        self.p().st.borrow().conn == ConnState::Connected
    }

    /// Forgets every contact's presence (stale "online" must not outlive the
    /// connection) and refreshes what shows it.
    pub(super) fn clear_presence(&self, p: &ProfileState) {
        let jids: Vec<String> = {
            let mut st = p.st.borrow_mut();
            st.presence.drain().map(|(jid, _)| jid).collect()
        };
        if jids.is_empty() || !self.is_active(p) {
            return;
        }
        self.update_header();
        for jid in jids {
            self.info_presence_changed(&p.id, &jid);
        }
    }

    /// Asks for the contact's presence; groups have none. The engine dedupes
    /// repeated requests. Skipped while offline (the engine would only answer
    /// with an error banner); reconnecting re-subscribes the open chat.
    pub(super) fn subscribe_presence(&self, jid: &str) {
        if is_dm(jid) && self.active_connected() {
            self.send(Command::SubscribePresence { jid: jid.to_string() });
        }
    }

    /// Re-subscribes for the open DM (the window just became active).
    pub(super) fn subscribe_open_presence(&self) {
        let jid = self.p().st.borrow().selected.clone();
        if let Some(jid) = jid {
            self.subscribe_presence(&jid);
        }
    }

    pub(super) fn on_presence(&self, p: &ProfileState, jid: String, online: bool, last_seen: Option<i64>) {
        let entry = PresenceEntry { online, last_seen, received_at: Local::now().timestamp() };
        let is_open = {
            let mut st = p.st.borrow_mut();
            st.presence.insert(jid.clone(), entry);
            st.selected.as_deref() == Some(jid.as_str())
        };
        if online {
            PRESENCE_TIMER.with(|t| {
                if !t.running() {
                    t.start(TimerMode::Repeated, PRESENCE_TICK, || with_app(App::tick_presence));
                }
            });
        }
        if is_open && self.is_active(p) {
            self.update_header();
        }
        if self.is_active(p) {
            self.info_presence_changed(&p.id, &jid);
        }
    }

    /// Turns expired "online" entries into "last seen" in the open header and
    /// stops itself once nothing is online.
    fn tick_presence(&self) {
        let now = Local::now().timestamp();
        let mut any_online = false;
        let mut open_expired = false;
        let mut expired: Vec<(String, String)> = Vec::new();
        for p in self.profs.borrow().iter() {
            let st = p.st.borrow();
            for (jid, e) in &st.presence {
                if !e.online {
                    continue;
                }
                if now - e.received_at < ONLINE_TTL_SECS {
                    any_online = true;
                } else if self.is_active(p) {
                    expired.push((p.id.clone(), jid.clone()));
                    if st.selected.as_deref() == Some(jid.as_str()) {
                        open_expired = true;
                    }
                }
            }
        }
        if open_expired {
            self.update_header();
        }
        for (profile, jid) in expired {
            self.info_presence_changed(&profile, &jid);
        }
        if !any_online {
            PRESENCE_TIMER.with(Timer::stop);
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn at(d: u32, h: u32, m: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 3, d, h, m, 0).unwrap()
    }

    #[test]
    fn incognito_needs_both_receipts_off() {
        let mut e = EngineSettingsDto::default();
        assert!(!incognito_active(&e));
        e.send_read_receipts = false;
        assert!(!incognito_active(&e));
        e.send_status_receipts = false;
        assert!(incognito_active(&e));
    }

    #[test]
    fn incognito_toggle_sets_and_restores_both() {
        let mut e = EngineSettingsDto::default();
        set_incognito(&mut e, true);
        assert!(!e.send_read_receipts && !e.send_status_receipts && incognito_active(&e));
        set_incognito(&mut e, false);
        assert!(e.send_read_receipts && e.send_status_receipts && !incognito_active(&e));
    }

    #[test]
    fn subtitle_online_then_expires_to_last_seen() {
        let got = at(10, 12, 0);
        let e = PresenceEntry { online: true, last_seen: None, received_at: got.timestamp() };
        assert_eq!(presence_subtitle(Some(&e), at(10, 12, 4)), "online");
        assert_eq!(presence_subtitle(Some(&e), at(10, 12, 6)), "last seen today at 12:00");
    }

    #[test]
    fn subtitle_offline_uses_last_seen_or_nothing() {
        let e = PresenceEntry { online: false, last_seen: Some(at(9, 8, 30).timestamp()), received_at: 0 };
        assert_eq!(presence_subtitle(Some(&e), at(10, 12, 0)), "last seen yesterday at 08:30");
        let hidden = PresenceEntry { online: false, last_seen: None, received_at: 0 };
        assert_eq!(presence_subtitle(Some(&hidden), at(10, 12, 0)), "");
        assert_eq!(presence_subtitle(None, at(10, 12, 0)), "");
    }
}
