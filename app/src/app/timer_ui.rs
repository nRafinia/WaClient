//! Disappearing-message timers: the open chat's timer (info row, header and
//! bubble markers, change dialog) and the default timer of new chats.

use std::cell::RefCell;

use slint::{ComponentHandle, ModelRc, VecModel};
use wa_engine::Command;

use super::{App, with_app};
use crate::{MainWindow, TimerOption, TimerState};

/// The timers WhatsApp offers, in seconds; 0 is off.
pub const TIMER_CHOICES: [u32; 4] = [0, 86_400, 604_800, 7_776_000];

thread_local! {
    /// Chat the open dialog changes: the one that was open when it appeared.
    static DIALOG_CHAT: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// "Off", "24 hours", "7 days", "90 days"; other values (set by another
/// device) are spelled out.
pub fn timer_label(secs: i64) -> String {
    fn count(n: i64, unit: &str) -> String {
        if n == 1 { format!("1 {unit}") } else { format!("{n} {unit}s") }
    }
    match secs {
        s if s <= 0 => "Off".to_string(),
        86_400 => "24 hours".to_string(),
        s if s % 86_400 == 0 => count(s / 86_400, "day"),
        s if s % 3_600 == 0 => count(s / 3_600, "hour"),
        s if s % 60 == 0 => count(s / 60, "minute"),
        s => count(s, "second"),
    }
}

/// The value a command carries: only the offered timers are accepted.
pub fn valid_timer(secs: i32) -> Option<u32> {
    u32::try_from(secs).ok().filter(|s| TIMER_CHOICES.contains(s))
}

pub(super) fn wire(win: &MainWindow) {
    let g = win.global::<TimerState>();
    let options: Vec<TimerOption> = TIMER_CHOICES
        .iter()
        .map(|s| TimerOption { secs: i32::try_from(*s).unwrap_or(0), label: timer_label(i64::from(*s)).into() })
        .collect();
    g.set_options(ModelRc::new(VecModel::from(options)));
    g.set_chat_label(timer_label(0).into());
    g.on_open_dialog(|| with_app(App::timer_open_dialog));
    g.on_apply(|secs| with_app(|a| a.timer_apply(secs)));
    g.on_cancel_dialog(|| with_app(App::timer_close_dialog));
    g.on_set_default(|secs| with_app(|a| a.timer_set_default(secs)));
}

impl App {
    /// Shows the open chat's timer (called whenever its row is reloaded).
    pub(super) fn timer_sync(&self, secs: i64) {
        let Some(win) = self.win() else { return };
        let g = win.global::<TimerState>();
        g.set_chat_on(secs > 0);
        g.set_chat_secs(i32::try_from(secs).unwrap_or(i32::MAX));
        g.set_chat_label(timer_label(secs).into());
    }

    fn timer_open_dialog(&self) {
        let Some(win) = self.win() else { return };
        let Some(jid) = self.p().st.borrow().selected.clone() else { return };
        DIALOG_CHAT.with(|d| *d.borrow_mut() = Some(jid));
        let g = win.global::<TimerState>();
        g.set_dialog_pick(g.get_chat_secs());
        g.set_dialog_open(true);
    }

    pub(super) fn timer_close_dialog(&self) {
        DIALOG_CHAT.with(|d| *d.borrow_mut() = None);
        if let Some(win) = self.win() {
            win.global::<TimerState>().set_dialog_open(false);
            win.invoke_focus_composer();
        }
    }

    fn timer_apply(&self, secs: i32) {
        let chat = DIALOG_CHAT.with(|d| d.borrow().clone());
        let unchanged = self.win().is_some_and(|w| w.global::<TimerState>().get_chat_secs() == secs);
        self.timer_close_dialog();
        let (Some(chat_jid), Some(secs)) = (chat, valid_timer(secs)) else { return };
        if unchanged {
            return;
        }
        self.send(Command::SetChatTimer { chat_jid, secs });
    }

    /// Reply to `FetchDefaultTimer`.
    pub(super) fn on_default_timer(&self, secs: u32) {
        if let Some(win) = self.win() {
            win.global::<TimerState>().set_default_secs(i32::try_from(secs).unwrap_or(i32::MAX));
        }
    }

    fn timer_set_default(&self, secs: i32) {
        let Some(secs) = valid_timer(secs) else { return };
        self.on_default_timer(secs);
        self.send(Command::SetDefaultTimer { secs });
        // The engine's answer corrects the chip when the change was refused.
        self.send(Command::FetchDefaultTimer);
    }

    /// Settings were opened: shows the stored default timer.
    pub(super) fn timer_settings_opened(&self) {
        self.send(Command::FetchDefaultTimer);
    }
}

#[cfg(debug_assertions)]
impl App {
    /// Debug aid for the demo (`WACLIENT_DEMO_UI=v04-share|v04-card|v04-timer|
    /// v04-viewonce`, also usable as `seq:` steps): opens the screens of this
    /// feature without input.
    pub(super) fn demo_v04(&self, what: &str) {
        let Some(win) = self.win() else { return };
        let p = self.p();
        let alice = "111@s.whatsapp.net";
        let now = chrono::Utc::now().timestamp();
        match what {
            "v04-share" => {
                for (jid, name) in [
                    ("491511234567@s.whatsapp.net", "Hans Mueller"),
                    ("34600123456@s.whatsapp.net", "Maria Garcia"),
                    ("971501234567@s.whatsapp.net", "Omar Khalil"),
                    ("447911123456@s.whatsapp.net", "Carol Smith"),
                ] {
                    self.ok("demo chat", p.store.upsert_chat(jid, wa_store::ChatKind::Dm, Some(name)));
                    // Chats without messages are not listed, and so not offered.
                    let hello = wa_store::NewMessage {
                        id: format!("v04-{jid}"),
                        chat_jid: jid.to_string(),
                        sender_jid: jid.to_string(),
                        ts: now - 3600,
                        kind: wa_store::MessageKind::Text,
                        text: Some("Hi!".to_string()),
                        ..Default::default()
                    };
                    self.ok("demo message", p.store.insert_message(&hello, false));
                }
                self.refresh_chats();
                self.select_chat(alice);
                self.open_share_contact();
                self.share_toggle("34600123456@s.whatsapp.net");
            }
            "v04-card" => {
                self.ok("demo timer", p.store.set_chat_ephemeral(alice, 604_800, now));
                self.refresh_chats();
                self.select_chat(alice);
                // The search scrolls the shared contact into view.
                slint::Timer::single_shot(std::time::Duration::from_millis(1500), || {
                    with_app(|a| {
                        if let Some(w) = a.win() {
                            w.set_search_open(true);
                            w.set_search_query("Maria".into());
                            a.search_input("Maria");
                        }
                    });
                });
            }
            "v04-timer" => {
                self.ok("demo timer", p.store.set_chat_ephemeral(alice, 604_800, now));
                self.refresh_chats();
                self.demo_info("info");
                self.timer_open_dialog();
            }
            "v04-viewonce" => {
                self.select_chat(alice);
                let dir = std::env::temp_dir().join("waclient-demo-drop");
                let png = dir.join("sunset.png");
                let written = std::fs::create_dir_all(&dir).map_err(|e| e.to_string()).and_then(|()| {
                    image::RgbImage::from_fn(160, 100, |x, y| image::Rgb([(200 - y) as u8, (x + 60) as u8, 120])).save(&png).map_err(|e| e.to_string())
                });
                match written {
                    Ok(()) => {
                        self.drop_files(vec![png]);
                        win.global::<crate::AttachOptions>().set_view_once(true);
                    }
                    Err(e) => log::warn!("demo: {e}"),
                }
            }
            "v04-default" => {
                self.open_settings();
                win.set_show_settings(true);
                // The Account category of the settings list.
                slint::Timer::single_shot(std::time::Duration::from_millis(800), || with_app(|a| a.demo_click(514, 146, false)));
            }
            other => log::warn!("unknown v04 demo: {other}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_of_the_offered_timers() {
        let labels: Vec<String> = TIMER_CHOICES.iter().map(|s| timer_label(i64::from(*s))).collect();
        assert_eq!(labels, ["Off", "24 hours", "7 days", "90 days"]);
    }

    #[test]
    fn labels_of_other_values_are_spelled_out() {
        assert_eq!(timer_label(-5), "Off");
        assert_eq!(timer_label(172_800), "2 days");
        assert_eq!(timer_label(7_200), "2 hours");
        assert_eq!(timer_label(3_600), "1 hour");
        assert_eq!(timer_label(120), "2 minutes");
        assert_eq!(timer_label(45), "45 seconds");
    }

    #[test]
    fn only_offered_timers_are_valid() {
        assert_eq!(valid_timer(0), Some(0));
        assert_eq!(valid_timer(604_800), Some(604_800));
        assert_eq!(valid_timer(1234), None);
        assert_eq!(valid_timer(-1), None);
    }
}
