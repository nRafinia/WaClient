//! Own profile editing, the block list and the chat-list row actions
//! (mark as unread, block / unblock, clear chat).
//!
//! The engine answers `FetchOwnProfile` / `FetchBlocklist` / `SetBlocked` with
//! events; the per-profile copies live here, keyed by profile id, so a profile
//! that is not on screen keeps its block list.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Image, ModelRc, Timer, TimerMode, VecModel};
use wa_engine::{Command, OwnProfile};
use wa_store::ChatKind;

use super::{App, ProfileState, with_app};
use crate::{AccountState, ChatActions, ContactRowItem, MainWindow, models};

/// WhatsApp's limits for the profile name and the About text.
pub const NAME_MAX: usize = 25;
pub const ABOUT_MAX: usize = 139;
/// Rows built for the "Block a contact" picker; the search narrows further.
const PICKER_LIMIT: usize = 50;
/// Only the first rows of the picker get an avatar lookup.
const PICKER_AVATARS: usize = 12;
const PICKER_DEBOUNCE: Duration = Duration::from_millis(150);
/// An own-profile change that the engine has not answered by then is forgotten.
const PENDING_TTL: Duration = Duration::from_secs(20);
const DEMO_ABOUT: &str = "Hey there! I am using WhatsApp.";

#[derive(Default)]
struct AccountData {
    blocked: Vec<String>,
    /// `blocked` in comparable form (phone-number addresses where known).
    blocked_keys: HashSet<String>,
    /// The block list was requested for the current connection.
    blocklist_asked: bool,
    own: Option<OwnProfile>,
    /// When an own-profile change was sent and not yet confirmed.
    pending: Option<Instant>,
}

thread_local! {
    static ACCOUNTS: RefCell<HashMap<String, AccountData>> = RefCell::new(HashMap::new());
    /// README.md next to the program, the only file "Open README" can open.
    static PICKER_TIMER: Timer = Timer::default();
    static README: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

fn with_data<R>(id: &str, f: impl FnOnce(&mut AccountData) -> R) -> R {
    ACCOUNTS.with(|a| f(a.borrow_mut().entry(id.to_string()).or_default()))
}

/// Cuts a typed text to the limit of its field (0 name, 1 about).
pub fn clamp_field(kind: i32, text: &str) -> String {
    let max = if kind == 0 { NAME_MAX } else { ABOUT_MAX };
    text.chars().take(max).collect()
}

pub fn validate_name(raw: &str) -> Result<String, &'static str> {
    let name = raw.trim();
    if name.is_empty() {
        Err("The name cannot be empty")
    } else if name.chars().count() > NAME_MAX {
        Err("The name can have at most 25 characters")
    } else {
        Ok(name.to_string())
    }
}

pub fn validate_about(raw: &str) -> Result<String, &'static str> {
    let about = raw.trim();
    if about.is_empty() {
        Err("The About text cannot be empty")
    } else if about.chars().count() > ABOUT_MAX {
        Err("The About text can have at most 139 characters")
    } else {
        Ok(about.to_string())
    }
}

/// The address without its device suffix, so list and chat jids compare equal.
fn bare(jid: &str) -> String {
    match jid.split_once('@') {
        Some((user, server)) => format!("{}@{server}", user.split(':').next().unwrap_or(user)),
        None => jid.to_string(),
    }
}

/// The address in its phone-number form when it is a LID the contact table
/// knows a number for; everything else stays as it is (without device suffix).
fn comparable(jid: &str, phone_of: &impl Fn(&str) -> Option<String>) -> String {
    let base = bare(jid);
    if base.ends_with("@lid") {
        let digits: String = phone_of(&base).unwrap_or_default().chars().filter(char::is_ascii_digit).collect();
        if !digits.is_empty() {
            return format!("{digits}@s.whatsapp.net");
        }
    }
    base
}

/// The engine reports blocked contacts in its canonical form (phone number
/// when the LID mapping is known, LID otherwise) and chats may still carry
/// the other form, so both sides are compared in phone-number form.
fn same_contact(a: &str, b: &str, phone_of: &impl Fn(&str) -> Option<String>) -> bool {
    bare(a) == bare(b) || comparable(a, phone_of) == comparable(b, phone_of)
}

/// Whether an engine error text belongs to an own-profile edit (wording of
/// crates/engine/src/account.rs); anything else is left to the normal toast.
fn is_profile_edit_error(message: &str) -> bool {
    const PREFIXES: [&str; 8] = [
        "The name ",
        "The About text ",
        "The picture ",
        "Could not change the name",
        "Could not change the About text",
        "Could not change the picture",
        "Could not remove the picture",
        "Not logged in",
    ];
    PREFIXES.iter().any(|p| message.starts_with(p))
}

fn pending_active(since: Option<Instant>, now: Instant) -> bool {
    since.is_some_and(|t| now.saturating_duration_since(t) < PENDING_TTL)
}

/// "+4915..." for phone-number addresses, the plain user part otherwise.
fn phone_text(jid: &str) -> String {
    let user = jid.split('@').next().unwrap_or(jid).split(':').next().unwrap_or(jid);
    if jid.ends_with("@s.whatsapp.net") { format!("+{user}") } else { user.to_string() }
}

fn display_phone(phone: &str) -> String {
    if phone.starts_with('+') { phone.to_string() } else { format!("+{phone}") }
}

/// Contact-table lookup of the number behind a LID address.
fn phone_lookup(p: &ProfileState) -> impl Fn(&str) -> Option<String> + '_ {
    move |jid| p.store.contact(jid).ok().flatten().and_then(|c| c.phone)
}

fn contact_row(jid: &str, name: &str, avatar: Option<Image>) -> ContactRowItem {
    let phone = phone_text(jid);
    let shown = if name.is_empty() { phone.clone() } else { name.to_string() };
    ContactRowItem {
        jid: jid.into(),
        subtitle: if shown == phone { "".into() } else { phone.as_str().into() },
        initials: models::initials(&shown).into(),
        tint: models::tint_for(jid),
        has_avatar: avatar.is_some(),
        avatar: avatar.unwrap_or_default(),
        name: shown.into(),
    }
}

/// Wires the callbacks of `AccountState` and `ChatActions`.
pub(super) fn wire(win: &MainWindow) {
    let acc = win.global::<AccountState>();
    acc.set_app_version(env!("CARGO_PKG_VERSION").into());
    let readme = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("README.md")))
        .filter(|p| p.is_file());
    acc.set_has_readme(readme.is_some());
    README.with(|r| *r.borrow_mut() = readme);
    acc.on_clamp(|kind, text| clamp_field(kind, text.as_str()).into());
    acc.on_set_name(|t| with_app(|a| a.set_own_name(t.as_str())));
    acc.on_set_about(|t| with_app(|a| a.set_own_about(t.as_str())));
    acc.on_change_photo(|| with_app(App::change_photo));
    acc.on_remove_photo(|| {
        with_app(|a| {
            let p = a.p();
            a.own_picture(&p, None);
        })
    });
    acc.on_unblock(|jid| with_app(|a| a.set_blocked(jid.as_str(), false)));
    acc.on_find_candidates(|q| {
        if q.trim().is_empty() {
            PICKER_TIMER.with(Timer::stop);
            with_app(|a| a.find_candidates(""));
            return;
        }
        // Typing restarts the wait: the list is built once the user pauses.
        let q = q.to_string();
        PICKER_TIMER.with(|t| {
            t.start(TimerMode::SingleShot, PICKER_DEBOUNCE, move || with_app(|a| a.find_candidates(&q)));
        });
    });
    acc.on_open_readme(|| with_app(App::open_readme));

    let chat = win.global::<ChatActions>();
    chat.on_mark_unread(|jid| {
        with_app(|a| {
            log::info!("chat menu: MarkChatUnread for {}", models::redact_jid(jid.as_str()));
            a.send(Command::MarkChatUnread { chat_jid: jid.to_string() });
        });
    });
    chat.on_set_blocked(|jid, blocked| with_app(|a| a.set_blocked(jid.as_str(), blocked)));
    chat.on_clear_chat(|jid, keep| {
        with_app(|a| {
            log::info!("chat menu: ClearChat(keep starred {keep}) for {}", models::redact_jid(jid.as_str()));
            a.send(Command::ClearChat { chat_jid: jid.to_string(), keep_starred: keep });
        });
    });
    chat.set_starred_enabled(true);
    chat.on_open_starred(|| with_app(|a| a.open_starred(None)));
}

impl App {
    /// Whether the active profile has blocked this chat's contact.
    pub(super) fn is_blocked(&self, jid: &str) -> bool {
        let p = self.p();
        self.blocked_matcher(&p)(jid)
    }

    /// A blocked-contact test that copies the block list once; use it when many
    /// chats are checked in a row.
    pub(super) fn blocked_matcher<'a>(&self, p: &'a ProfileState) -> impl Fn(&str) -> bool + 'a {
        let keys = with_data(&p.id, |d| d.blocked_keys.clone());
        move |jid| {
            if keys.is_empty() {
                return false;
            }
            let phone_of = phone_lookup(p);
            keys.contains(&bare(jid)) || keys.contains(&comparable(jid, &phone_of))
        }
    }

    /// Whether `jid` is a direct chat with a contact the active profile blocked.
    pub(super) fn is_blocked_dm(&self, jid: &str) -> bool {
        ChatKind::from_jid(jid) == ChatKind::Dm && self.is_blocked(jid)
    }

    /// Whether the open chat is a blocked DM.
    pub(super) fn chat_is_blocked_dm(&self) -> bool {
        let sel = self.p().st.borrow().selected.clone();
        sel.is_some_and(|j| self.is_blocked_dm(&j))
    }

    /// Toasts and returns true when messages to `jid` must not go out.
    pub(super) fn refuse_blocked(&self, jid: &str) -> bool {
        let refused = self.is_blocked_dm(jid);
        if refused {
            self.show_toast("Unblock this contact to send messages");
        }
        refused
    }

    /// Asks for the block list once per connection.
    pub(super) fn account_connection(&self, p: &ProfileState, connected: bool) {
        let ask = with_data(&p.id, |d| {
            if !connected {
                d.blocklist_asked = false;
                return false;
            }
            !std::mem::replace(&mut d.blocklist_asked, true)
        });
        if ask {
            (p.send)(Command::FetchBlocklist);
        }
    }

    pub(super) fn on_blocklist(&self, p: &ProfileState, active: bool, jids: Vec<String>) {
        let phone_of = phone_lookup(p);
        let keys: HashSet<String> = jids.iter().flat_map(|j| [bare(j), comparable(j, &phone_of)]).collect();
        with_data(&p.id, |d| {
            d.blocked = jids;
            d.blocked_keys = keys;
        });
        if active {
            self.show_blocklist(p);
            self.refresh_chats();
            self.update_header();
        }
    }

    /// The composer's "Unblock" button for the open direct chat.
    pub(super) fn unblock_open_chat(&self) {
        let Some(jid) = self.p().st.borrow().selected.clone() else { return };
        self.set_blocked(&jid, false);
    }

    pub(super) fn on_own_profile(&self, p: &ProfileState, active: bool, own: OwnProfile) {
        with_data(&p.id, |d| {
            d.own = Some(own);
            d.pending = None;
        });
        if active {
            if let Some(win) = self.win() {
                win.global::<AccountState>().set_error("".into());
            }
            self.show_own(p);
        }
    }

    /// An engine error while an own-profile change is in flight: the optimistic
    /// value is replaced by what the account really has.
    pub(super) fn account_error(&self, p: &ProfileState, active: bool, message: &str) {
        if !is_profile_edit_error(message) {
            return;
        }
        let since = with_data(&p.id, |d| d.pending.take());
        if !pending_active(since, Instant::now()) {
            return;
        }
        (p.send)(Command::FetchOwnProfile);
        if active {
            if let Some(win) = self.win() {
                win.global::<AccountState>().set_error(message.into());
            }
        }
    }

    /// Settings opened: refresh the profile and show what is known.
    pub(super) fn account_settings_opened(&self) {
        let Some(win) = self.win() else { return };
        let p = self.p();
        let acc = win.global::<AccountState>();
        acc.set_error("".into());
        acc.set_candidates(ModelRc::new(VecModel::default()));
        if self.persist {
            (p.send)(Command::FetchOwnProfile);
            self.timer_settings_opened();
        } else {
            self.demo_own(&p);
            self.on_default_timer(604_800);
        }
        self.show_own(&p);
        self.show_blocklist(&p);
    }

    /// The demo has no engine: its profile is made up from the local one.
    fn demo_own(&self, p: &ProfileState) {
        with_data(&p.id, |d| {
            if d.own.is_none() {
                d.own = Some(OwnProfile {
                    jid: p.meta.borrow().own_jid.clone().unwrap_or_default(),
                    phone: Some("491701234567".to_string()),
                    name: Some(p.name()),
                    about: Some(DEMO_ABOUT.to_string()),
                    picture_path: None,
                });
            }
        });
    }

    fn show_own(&self, p: &ProfileState) {
        let Some(win) = self.win().filter(|_| self.is_active(p)) else { return };
        let own = with_data(&p.id, |d| d.own.clone());
        let acc = win.global::<AccountState>();
        let own = own.unwrap_or_default();
        let name = own.name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| p.name());
        let key = if own.jid.is_empty() { p.id.clone() } else { own.jid.clone() };
        acc.set_initials(models::initials(&name).into());
        acc.set_tint(models::tint_for(&key));
        acc.set_name(own.name.as_deref().unwrap_or_default().into());
        acc.set_about(own.about.as_deref().unwrap_or_default().into());
        acc.set_phone(own.phone.as_deref().map(display_phone).unwrap_or_default().into());
        let photo = own.picture_path.as_ref().and_then(|path| match Image::load_from_path(path) {
            Ok(img) => Some(img),
            Err(e) => {
                log::warn!("could not load the profile photo: {e:?}");
                None
            }
        });
        acc.set_has_photo(photo.is_some());
        acc.set_photo(photo.unwrap_or_default());
    }

    fn show_blocklist(&self, p: &ProfileState) {
        let Some(win) = self.win().filter(|_| self.is_active(p)) else { return };
        let jids = with_data(&p.id, |d| d.blocked.clone());
        let phone_of = phone_lookup(p);
        let rows: Vec<ContactRowItem> = {
            let mut st = p.st.borrow_mut();
            jids.iter()
                .map(|jid| {
                    let name = st.chats.iter().find(|c| same_contact(&c.jid, jid, &phone_of)).map(|c| c.display_name.clone());
                    let avatar = self.avatar(p, &mut st, jid);
                    contact_row(jid, name.as_deref().unwrap_or_default(), avatar)
                })
                .collect()
        };
        win.global::<AccountState>().set_blocked(ModelRc::new(VecModel::from(rows)));
    }

    fn find_candidates(&self, query: &str) {
        let Some(win) = self.win() else { return };
        let p = self.p();
        let needle = query.trim().to_lowercase();
        let keys = with_data(&p.id, |d| d.blocked_keys.clone());
        let phone_of = phone_lookup(&p);
        let rows: Vec<ContactRowItem> = {
            let mut st = p.st.borrow_mut();
            let hits: Vec<(String, String)> = st
                .chats
                .iter()
                .filter(|c| c.kind == ChatKind::Dm)
                .filter(|c| {
                    needle.is_empty()
                        || c.display_name.to_lowercase().contains(&needle)
                        || phone_text(&c.jid).contains(&needle)
                })
                .filter(|c| !keys.contains(&bare(&c.jid)) && !keys.contains(&comparable(&c.jid, &phone_of)))
                .take(PICKER_LIMIT)
                .map(|c| (c.jid.clone(), c.display_name.clone()))
                .collect();
            hits.into_iter()
                .enumerate()
                .map(|(i, (jid, name))| {
                    let avatar = if i < PICKER_AVATARS { self.avatar(&p, &mut st, &jid) } else { None };
                    contact_row(&jid, &name, avatar)
                })
                .collect()
        };
        win.global::<AccountState>().set_candidates(ModelRc::new(VecModel::from(rows)));
    }

    pub(super) fn set_blocked(&self, jid: &str, blocked: bool) {
        let p = self.p();
        log::info!("SetBlocked({blocked}) for {}", models::redact_jid(jid));
        if self.persist {
            (p.send)(Command::SetBlocked { jid: jid.to_string(), blocked });
            return;
        }
        // No engine in the demo: the list changes locally.
        let mut jids = with_data(&p.id, |d| d.blocked.clone());
        let phone_of = phone_lookup(&p);
        jids.retain(|b| !same_contact(b, jid, &phone_of));
        if blocked {
            jids.push(jid.to_string());
        }
        self.on_blocklist(&p, true, jids);
    }

    fn set_own_name(&self, raw: &str) {
        match validate_name(raw) {
            Ok(name) => self.own_edit(Command::SetOwnName { name: name.clone() }, move |o| o.name = Some(name)),
            Err(why) => self.account_message(why),
        }
    }

    fn set_own_about(&self, raw: &str) {
        match validate_about(raw) {
            Ok(text) => self.own_edit(Command::SetOwnAbout { text: text.clone() }, move |o| o.about = Some(text)),
            Err(why) => self.account_message(why),
        }
    }

    fn account_message(&self, text: &str) {
        if let Some(win) = self.win() {
            win.global::<AccountState>().set_error(text.into());
        }
    }

    /// Sends an own-profile change and shows it right away; an engine error
    /// brings the real value back (see `account_error`).
    fn own_edit(&self, cmd: Command, apply: impl FnOnce(&mut OwnProfile)) {
        let p = self.p();
        if !self.persist {
            self.demo_own(&p);
        }
        let changed = with_data(&p.id, |d| {
            let own = d.own.get_or_insert_with(OwnProfile::default);
            let before = own.clone();
            apply(own);
            let changed = *own != before;
            d.pending = (changed && self.persist).then(Instant::now);
            changed
        });
        if changed {
            if self.persist {
                (p.send)(cmd);
            }
            self.show_own(&p);
        }
    }

    fn change_photo(&self) {
        let Some(win) = self.win() else { return };
        let acc = win.global::<AccountState>();
        if acc.get_photo_busy() {
            return;
        }
        acc.set_photo_busy(true);
        let id = self.p().id.clone();
        let spawned = std::thread::Builder::new().name("pick-photo".into()).spawn(move || {
            let picked = rfd::FileDialog::new()
                .set_title("Choose a profile photo")
                .add_filter("Images", &["jpg", "jpeg", "png"])
                .pick_file();
            let queued = slint::invoke_from_event_loop(move || with_app(|a| a.photo_picked(&id, picked)));
            if let Err(e) = queued {
                log::error!("could not deliver the picked photo to the UI: {e}");
            }
        });
        if let Err(e) = spawned {
            acc.set_photo_busy(false);
            self.show_banner(1, &format!("Could not open the file picker: {e}"));
        }
    }

    fn photo_picked(&self, id: &str, picked: Option<PathBuf>) {
        if let Some(win) = self.win() {
            win.global::<AccountState>().set_photo_busy(false);
        }
        let Some(path) = picked else { return };
        // The profile may have been removed while the dialog was open.
        let Some(p) = self.find(id) else { return };
        self.own_picture(&p, Some(path));
    }

    /// Sets (Some) or removes (None) the photo of profile `p`.
    fn own_picture(&self, p: &ProfileState, path: Option<PathBuf>) {
        if self.persist {
            with_data(&p.id, |d| d.pending = Some(Instant::now()));
            (p.send)(Command::SetOwnPicture { path });
            return;
        }
        self.demo_own(p);
        with_data(&p.id, |d| {
            if let Some(own) = d.own.as_mut() {
                own.picture_path = path;
            }
        });
        self.show_own(p);
    }

    fn open_readme(&self) {
        let Some(path) = README.with(|r| r.borrow().clone()) else { return };
        if let Err(e) = super::launch(&path, false) {
            self.show_banner(1, &format!("Could not open the README: {e}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_trimmed_and_limited() {
        assert_eq!(validate_name("  Ada  "), Ok("Ada".to_string()));
        assert!(validate_name("   ").is_err());
        assert!(validate_name(&"a".repeat(25)).is_ok());
        assert!(validate_name(&"a".repeat(26)).is_err());
    }

    #[test]
    fn limits_count_characters_not_bytes() {
        assert!(validate_name(&"é".repeat(25)).is_ok());
        assert!(validate_about(&"😀".repeat(139)).is_ok());
        assert!(validate_about(&"😀".repeat(140)).is_err());
    }

    #[test]
    fn about_must_not_be_empty() {
        assert!(validate_about("").is_err());
        assert_eq!(validate_about(" busy "), Ok("busy".to_string()));
    }

    #[test]
    fn typed_text_is_cut_to_the_field_limit() {
        assert_eq!(clamp_field(0, &"x".repeat(40)).chars().count(), 25);
        assert_eq!(clamp_field(1, &"x".repeat(200)).chars().count(), 139);
        assert_eq!(clamp_field(0, "short"), "short");
    }

    #[test]
    fn device_suffix_is_ignored_when_comparing() {
        assert_eq!(bare("49170@s.whatsapp.net"), "49170@s.whatsapp.net");
        assert_eq!(bare("49170:12@s.whatsapp.net"), "49170@s.whatsapp.net");
        assert_eq!(bare("plain"), "plain");
    }

    #[test]
    fn lid_and_phone_forms_of_one_contact_match() {
        let phone_of = |jid: &str| (jid == "100@lid").then(|| "+49170".to_string());
        assert!(same_contact("100@lid", "49170@s.whatsapp.net", &phone_of));
        assert!(same_contact("49170:3@s.whatsapp.net", "100@lid", &phone_of));
        assert!(same_contact("100@lid", "100@lid", &phone_of));
        assert!(!same_contact("101@lid", "49170@s.whatsapp.net", &phone_of));
        assert!(!same_contact("49171@s.whatsapp.net", "100@lid", &phone_of));
    }

    #[test]
    fn unmapped_lid_only_matches_itself() {
        let none = |_: &str| None;
        assert!(!same_contact("100@lid", "49170@s.whatsapp.net", &none));
        assert!(same_contact("100:2@lid", "100@lid", &none));
    }

    #[test]
    fn only_profile_edit_errors_are_claimed() {
        assert!(is_profile_edit_error("The picture is larger than 5 MB"));
        assert!(is_profile_edit_error("Could not change the name: boom"));
        assert!(is_profile_edit_error("The About text cannot be empty"));
        assert!(!is_profile_edit_error("Could not send the message"));
        assert!(!is_profile_edit_error("Can't block this contact yet"));
    }

    #[test]
    fn pending_expires_after_the_ttl() {
        let t0 = Instant::now();
        assert!(pending_active(Some(t0), t0 + Duration::from_secs(19)));
        assert!(!pending_active(Some(t0), t0 + Duration::from_secs(20)));
        assert!(!pending_active(None, t0));
    }

    #[test]
    fn phone_text_per_address_kind() {
        assert_eq!(phone_text("49170@s.whatsapp.net"), "+49170");
        assert_eq!(phone_text("123@lid"), "123");
        assert_eq!(display_phone("491"), "+491");
        assert_eq!(display_phone("+491"), "+491");
    }
}
