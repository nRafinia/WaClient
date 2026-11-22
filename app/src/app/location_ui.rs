//! Locations: the "Send location" dialog and the clicks of the location card.
//!
//! Coordinates are personal data: nothing here logs them.

use std::cell::RefCell;
use std::time::Duration;

use slint::ComponentHandle;
use wa_engine::Command;

use super::{App, with_app};
use crate::geo::{self, GeoError, GeoFix};
use crate::{AttachActions, LocationActions, LocationState, MainWindow};

/// How long Windows may take to produce a fix.
const LOCATE_TIMEOUT: Duration = Duration::from_secs(15);
/// Longest place name that is sent.
const NAME_MAX_CHARS: usize = 100;

#[derive(Default)]
struct Draft {
    /// Chat the location goes to (the chat that was open when the dialog opened).
    chat: Option<String>,
    /// The profile the dialog was opened for; a send from another one is refused.
    profile: String,
    coords: Option<(f64, f64)>,
    accuracy: Option<f64>,
    /// The position came from Windows (not from pasted text).
    from_fix: bool,
    /// Bumped whenever the dialog closes or a new request starts, so a late
    /// answer from Windows is dropped.
    generation: u64,
}

thread_local! {
    static DRAFT: RefCell<Draft> = RefCell::new(Draft::default());
}

/// What the pasted text means.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Pasted {
    Empty,
    Valid(f64, f64),
    Invalid,
}

fn read_pasted(text: &str) -> Pasted {
    if text.trim().is_empty() {
        return Pasted::Empty;
    }
    geo::parse_location(text).map_or(Pasted::Invalid, |(lat, lon)| Pasted::Valid(lat, lon))
}

/// The place name as sent: trimmed, limited, none when blank.
fn clean_name(name: &str) -> Option<String> {
    let name: String = name.trim().chars().take(NAME_MAX_CHARS).collect();
    (!name.trim().is_empty()).then_some(name)
}

fn build_send(chat_jid: String, (lat, lon): (f64, f64), name: &str, quoted_id: Option<String>) -> Command {
    Command::SendLocation { chat_jid, lat, lon, name: clean_name(name), address: None, quoted_id }
}

/// What Copy puts on the clipboard for a message with a position.
fn copy_payload(lat: Option<f64>, lon: Option<f64>) -> Option<String> {
    lat.zip(lon).map(|(la, lo)| geo::coords_text(la, lo))
}

pub(super) fn wire(win: &MainWindow) {
    let s = win.global::<LocationState>();
    s.on_use_current(|| with_app(App::location_use_current));
    s.on_paste_edited(|t| with_app(|a| a.location_pasted(t.as_str())));
    s.on_send(|| with_app(App::location_send));
    s.on_cancel(|| with_app(App::location_close));
    win.global::<AttachActions>().on_location(|| with_app(App::open_send_location));
    win.global::<LocationActions>().on_open(|geo| with_app(|a| a.open_geo(geo.as_str())));
}

impl App {
    /// Opens the dialog for the open chat. The attach menu calls this.
    pub(super) fn open_send_location(&self) {
        let Some(win) = self.win() else { return };
        let Some(chat) = self.p().st.borrow().selected.clone() else { return };
        if self.refuse_blocked(&chat) {
            return;
        }
        let profile = self.p().id.clone();
        DRAFT.with(|d| {
            let mut d = d.borrow_mut();
            d.generation += 1;
            d.chat = Some(chat);
            d.profile = profile;
            d.coords = None;
            d.accuracy = None;
            d.from_fix = false;
        });
        let s = win.global::<LocationState>();
        s.set_paste_text(Default::default());
        s.set_name_text(Default::default());
        s.set_paste_error(Default::default());
        s.set_error(Default::default());
        s.set_locating(false);
        self.location_publish();
        s.set_open(true);
    }

    pub(super) fn location_close(&self) {
        DRAFT.with(|d| {
            let mut d = d.borrow_mut();
            d.generation += 1;
            d.chat = None;
            d.profile.clear();
            d.coords = None;
            d.accuracy = None;
            d.from_fix = false;
        });
        if let Some(win) = self.win() {
            let s = win.global::<LocationState>();
            s.set_open(false);
            s.set_locating(false);
        }
    }

    /// Shows the draft position in the dialog.
    fn location_publish(&self) {
        let Some(win) = self.win() else { return };
        let (coords, accuracy, from_fix) = DRAFT.with(|d| {
            let d = d.borrow();
            (d.coords, d.accuracy, d.from_fix)
        });
        let s = win.global::<LocationState>();
        s.set_coords_text(coords.map(|(la, lo)| geo::coords_text(la, lo)).unwrap_or_default().into());
        let info = match (coords, from_fix) {
            (None, _) => String::new(),
            (Some(_), true) => geo::accuracy_text(accuracy),
            (Some(_), false) => "From the pasted text".to_string(),
        };
        s.set_info_text(info.into());
    }

    fn location_use_current(&self) {
        let Some(win) = self.win() else { return };
        let generation = DRAFT.with(|d| {
            let mut d = d.borrow_mut();
            d.generation += 1;
            d.generation
        });
        let s = win.global::<LocationState>();
        s.set_locating(true);
        s.set_error(Default::default());
        let spawned = std::thread::Builder::new().name("geolocate".into()).spawn(move || {
            let fix = geo::current_location(LOCATE_TIMEOUT);
            let queued = slint::invoke_from_event_loop(move || with_app(|a| a.location_fix(generation, fix)));
            if let Err(e) = queued {
                log::error!("could not deliver the location result to the UI: {e}");
            }
        });
        if let Err(e) = spawned {
            log::error!("could not start the location thread: {e}");
            s.set_locating(false);
            s.set_error(format!("Could not get the current location: {e}").into());
        }
    }

    fn location_fix(&self, generation: u64, fix: Result<GeoFix, GeoError>) {
        let current = DRAFT.with(|d| d.borrow().generation == generation && d.borrow().chat.is_some());
        if !current {
            return;
        }
        let Some(win) = self.win() else { return };
        let s = win.global::<LocationState>();
        s.set_locating(false);
        match fix {
            Ok(fix) => self.location_got_fix(fix),
            Err(e) => {
                log::warn!("current location unavailable: {e}");
                s.set_error(e.to_string().into());
            }
        }
    }

    /// A position from Windows replaces whatever was pasted.
    fn location_got_fix(&self, fix: GeoFix) {
        DRAFT.with(|d| {
            let mut d = d.borrow_mut();
            d.coords = Some((fix.lat, fix.lon));
            d.accuracy = fix.accuracy_m;
            d.from_fix = true;
        });
        if let Some(win) = self.win() {
            let s = win.global::<LocationState>();
            s.set_paste_text(Default::default());
            s.set_paste_error(Default::default());
            s.set_error(Default::default());
        }
        self.location_publish();
    }

    fn location_pasted(&self, text: &str) {
        let Some(win) = self.win() else { return };
        let pasted = read_pasted(text);
        DRAFT.with(|d| {
            let mut d = d.borrow_mut();
            // Typing replaces a fix that came from Windows.
            d.generation += 1;
            d.coords = match pasted {
                Pasted::Valid(la, lo) => Some((la, lo)),
                _ => None,
            };
            d.accuracy = None;
            d.from_fix = false;
        });
        let s = win.global::<LocationState>();
        s.set_locating(false);
        s.set_error(Default::default());
        let error = if pasted == Pasted::Invalid { "That is not a coordinate pair or a map link" } else { "" };
        s.set_paste_error(error.into());
        self.location_publish();
    }

    fn location_send(&self) {
        let Some(win) = self.win() else { return };
        let (chat, coords, profile) = DRAFT.with(|d| {
            let d = d.borrow();
            (d.chat.clone(), d.coords, d.profile.clone())
        });
        let (Some(chat), Some(coords)) = (chat, coords) else { return };
        // The chat id belongs to the profile the dialog was opened for.
        if profile != self.p().id {
            self.location_close();
            self.show_toast("The profile changed: open the location dialog again");
            return;
        }
        if self.refuse_blocked(&chat) || self.refuse_gated() {
            return;
        }
        let name = win.global::<LocationState>().get_name_text();
        let quoted = {
            let p = self.p();
            let mut st = p.st.borrow_mut();
            if st.selected.as_deref() == Some(chat.as_str()) {
                st.force_bottom = true;
                st.reply.take().map(|r| r.0)
            } else {
                None
            }
        };
        self.send(build_send(chat, coords, name.as_str(), quoted));
        self.sync_reply();
        self.location_close();
    }

    /// A click on a location card or on the preview link: the map in the browser.
    fn open_geo(&self, geo_text: &str) {
        match geo::parse_location(geo_text) {
            Some((lat, lon)) => self.open_link(&geo::maps_url(lat, lon)),
            None => self.show_toast("That location could not be opened"),
        }
    }

    /// Copy of the bubble menu: a location copies its coordinates, anything
    /// else its text.
    pub(super) fn copy_message_or_coords(&self, id: &str) {
        let coords = {
            let p = self.p();
            let st = p.st.borrow();
            st.msgs
                .iter()
                .find(|m| m.id == id && !m.revoked)
                .and_then(|m| copy_payload(m.geo_lat, m.geo_lon))
        };
        match coords {
            Some(text) => self.copy_text(&text),
            None => self.copy_message(id),
        }
    }

    /// Demo only: the dialog with a pasted position (`fix`: as if Windows answered).
    #[cfg(debug_assertions)]
    pub(super) fn demo_send_location(&self, fix: bool) {
        self.open_send_location();
        let Some(win) = self.win() else { return };
        if fix {
            self.location_got_fix(GeoFix { lat: 48.85837, lon: 2.29448, accuracy_m: Some(24.6) });
        } else {
            let s = win.global::<LocationState>();
            s.set_paste_text("https://www.google.com/maps/@48.85837,2.29448,17z".into());
            s.set_name_text("Eiffel Tower".into());
            self.location_pasted("https://www.google.com/maps/@48.85837,2.29448,17z");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pasted_text_is_empty_valid_or_invalid() {
        assert_eq!(read_pasted("   "), Pasted::Empty);
        assert_eq!(read_pasted("48.8584, 2.2945"), Pasted::Valid(48.8584, 2.2945));
        assert_eq!(read_pasted("https://www.google.com/maps/@48.8584,2.2945,15z"), Pasted::Valid(48.8584, 2.2945));
        assert_eq!(read_pasted("hello"), Pasted::Invalid);
        assert_eq!(read_pasted("95, 10"), Pasted::Invalid);
    }

    #[test]
    fn place_name_is_trimmed_limited_and_optional() {
        assert_eq!(clean_name("  "), None);
        assert_eq!(clean_name("  Home "), Some("Home".to_string()));
        assert_eq!(clean_name(&"x".repeat(300)).map(|n| n.chars().count()), Some(NAME_MAX_CHARS));
    }

    #[test]
    fn send_command_carries_position_name_and_reply() {
        let cmd = build_send("1@s.whatsapp.net".into(), (1.5, -2.5), " Cafe ", Some("m1".into()));
        match cmd {
            Command::SendLocation { chat_jid, lat, lon, name, address, quoted_id } => {
                assert_eq!(chat_jid, "1@s.whatsapp.net");
                assert_eq!((lat, lon), (1.5, -2.5));
                assert_eq!(name.as_deref(), Some("Cafe"));
                assert_eq!(address, None);
                assert_eq!(quoted_id.as_deref(), Some("m1"));
            }
            other => panic!("unexpected command {other:?}"),
        }
    }

    #[test]
    fn copy_gives_the_coordinates_only_for_a_position() {
        assert_eq!(copy_payload(Some(35.6892), Some(51.389)), Some("35.68920, 51.38900".to_string()));
        assert_eq!(copy_payload(Some(1.0), None), None);
        assert_eq!(copy_payload(None, None), None);
    }
}
