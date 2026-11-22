//! Sharing contacts: the picker that sends contact cards into the open chat,
//! the chat picker that re-sends a card (from a bubble or the info panel), and
//! the "Message" button of cards (opens the chat, or asks WhatsApp whether the
//! number is registered first).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Model, ModelRc, VecModel};
use wa_engine::{Command, PhoneLookupResult, STATUS_JID, SharedContact};
use wa_store::{ChatKind, ChatRow};

use super::msgops_ui::{Toggle, toggle_limited};
use super::{App, ProfileState, State, with_app};
use crate::{AttachActions, MainWindow, ShareRow, ShareState, models};

/// Most contacts one share carries (the engine's cap per message).
pub const MAX_SHARED_CONTACTS: usize = 20;
/// Most chats one re-share goes to (same as forwarding).
const MAX_SHARE_CHATS: usize = 5;
/// Most members one "Add members" pick may hold. Also the most people one
/// invite-link round messages (those are the members that could not be added),
/// so the two limits cannot drift apart.
pub const MAX_ADD_MEMBERS: usize = 20;
/// Most members one new group may start with.
const MAX_NEW_GROUP_MEMBERS: usize = 256;
/// A lookup that got no answer for this long is forgotten.
const LOOKUP_TTL: Duration = Duration::from_secs(60);
/// Longest name shown on a card.
const MAX_NAME_CHARS: usize = 100;

/// A row of the picker: a chat, and the number of its contact in contact mode.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub jid: String,
    pub name: String,
    /// "+digits"; empty for chats (and groups) in chat mode.
    pub phone: String,
    pub last_ts: i64,
}

/// "+digits" of a direct chat: from the address when it is a phone-number
/// address, else from the stored contact number (LID chats).
pub fn candidate_phone(jid: &str, stored: Option<&str>) -> Option<String> {
    let usable = |d: &str| (5..=15).contains(&d.len()).then(|| format!("+{d}"));
    if jid.ends_with("@s.whatsapp.net") {
        let user = jid.split('@').next()?.split(':').next()?;
        return user.bytes().all(|b| b.is_ascii_digit()).then(|| usable(user)).flatten();
    }
    let digits: String = stored?.chars().filter(char::is_ascii_digit).collect();
    usable(&digits)
}

/// Direct chats that have a phone number, by name.
pub fn contact_candidates(chats: &[ChatRow], phone_of: impl Fn(&str) -> Option<String>) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = chats
        .iter()
        .filter(|c| c.kind == ChatKind::Dm)
        .filter_map(|c| {
            let phone = candidate_phone(&c.jid, phone_of(&c.jid).as_deref())?;
            let name = if c.display_name.trim().is_empty() { phone.clone() } else { c.display_name.clone() };
            Some(Candidate { jid: c.jid.clone(), name, phone, last_ts: c.last_ts })
        })
        .collect();
    out.sort_by_key(|c| c.name.to_lowercase());
    out
}

/// Every chat that can be written to, most recent first.
pub fn chat_candidates(chats: &[ChatRow], blocked: impl Fn(&str) -> bool) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = chats
        .iter()
        .filter(|c| c.kind != ChatKind::Other && c.jid != STATUS_JID)
        .filter(|c| !(c.kind == ChatKind::Dm && blocked(&c.jid)))
        .map(|c| Candidate { jid: c.jid.clone(), name: c.display_name.clone(), phone: String::new(), last_ts: c.last_ts })
        .collect();
    out.sort_by(|a, b| b.last_ts.cmp(&a.last_ts));
    out
}

/// Candidates whose name contains `query` (any case), or whose number
/// contains the digits of it.
pub fn filter_candidates<'a>(all: &'a [Candidate], query: &str) -> Vec<&'a Candidate> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return all.iter().collect();
    }
    let digits: String = q.chars().filter(char::is_ascii_digit).collect();
    all.iter()
        .filter(|c| {
            c.name.to_lowercase().contains(&q) || (!digits.is_empty() && c.phone.chars().filter(char::is_ascii_digit).collect::<String>().contains(&digits))
        })
        .collect()
}

/// The cards of the picked contacts, in pick order; `names` holds edited names.
pub fn build_cards(picked: &[String], all: &[Candidate], names: &HashMap<String, String>) -> Vec<SharedContact> {
    picked
        .iter()
        .filter_map(|jid| {
            let c = all.iter().find(|c| &c.jid == jid)?;
            let edited = names.get(jid).map(|n| n.trim()).filter(|n| !n.is_empty());
            let name: String = edited.unwrap_or(&c.name).chars().take(MAX_NAME_CHARS).collect();
            Some(SharedContact { name, phones: vec![c.phone.clone()] })
        })
        .collect()
}

/// "989123" of "989123:7@s.whatsapp.net" and "989123@lid".
fn user_of(jid: &str) -> &str {
    let user = jid.split('@').next().unwrap_or(jid);
    user.split(':').next().unwrap_or(user)
}

/// Candidates that are not already in the group. `members` holds the jid and
/// the phone digits of each member: the same person may be listed under a LID
/// address while the chat uses the phone-number address, or the other way round.
pub fn exclude_members(all: Vec<Candidate>, members: &[(String, String)]) -> Vec<Candidate> {
    all.into_iter()
        .filter(|c| {
            let digits = digits_of(&c.phone);
            !members.iter().any(|(jid, phone)| {
                user_of(jid) == user_of(&c.jid) || (!digits.is_empty() && *phone == digits) || (!phone.is_empty() && user_of(&c.jid) == phone)
            })
        })
        .collect()
}

/// The address a candidate is added under: the chat's own phone-number
/// address, or one built from the number for a LID chat.
pub fn member_add_jid(c: &Candidate) -> String {
    if c.jid.ends_with("@s.whatsapp.net") {
        return c.jid.clone();
    }
    format!("{}@s.whatsapp.net", digits_of(&c.phone))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LookupOutcome {
    /// The number has a WhatsApp account at this address.
    Registered(String),
    NotOnWhatsApp,
    /// The answer does not mention the number.
    Unknown,
}

fn digits_of(s: &str) -> String {
    s.chars().filter(char::is_ascii_digit).collect()
}

/// What the engine's answer says about `digits`.
pub fn lookup_outcome(results: &[PhoneLookupResult], digits: &str) -> LookupOutcome {
    match results.iter().find(|r| digits_of(&r.number) == digits) {
        None => LookupOutcome::Unknown,
        Some(r) => match r.jid.as_deref().filter(|j| !j.is_empty()) {
            Some(jid) => LookupOutcome::Registered(jid.to_string()),
            None => LookupOutcome::NotOnWhatsApp,
        },
    }
}

enum Target {
    /// Contacts are picked to be sent into this chat.
    Contacts { chat: String },
    /// Chats are picked to receive these cards.
    Chats { cards: Vec<SharedContact> },
    /// Contacts are picked to be added to this group.
    AddMembers { group: String },
    /// Contacts are picked to start a new group (its name comes next).
    NewGroup,
}

/// True when a dialog opened for `opened` may still act in the `active` profile.
pub fn profile_matches(opened: &str, active: &str) -> bool {
    opened == active
}

struct Draft {
    /// The profile the picker was opened for; sending from another one is refused.
    profile: String,
    target: Target,
    all: Vec<Candidate>,
    query: String,
    picked: Vec<String>,
    names: HashMap<String, String>,
    model: Rc<VecModel<ShareRow>>,
}

impl Draft {
    fn max(&self) -> usize {
        match self.target {
            Target::Contacts { .. } => MAX_SHARED_CONTACTS,
            Target::Chats { .. } => MAX_SHARE_CHATS,
            Target::AddMembers { .. } => MAX_ADD_MEMBERS,
            Target::NewGroup => MAX_NEW_GROUP_MEMBERS,
        }
    }

    fn chat_mode(&self) -> bool {
        matches!(self.target, Target::Chats { .. })
    }

    fn contact_mode(&self) -> bool {
        matches!(self.target, Target::Contacts { .. })
    }

    /// The only picked contact in contact mode: its card name is editable.
    fn editable(&self) -> Option<&Candidate> {
        match (self.contact_mode(), self.picked.as_slice()) {
            (true, [one]) => self.all.iter().find(|c| &c.jid == one),
            _ => None,
        }
    }
}

struct PendingLookup {
    profile: String,
    digits: String,
    at: Instant,
}

thread_local! {
    static SHARE: RefCell<Option<Draft>> = const { RefCell::new(None) };
    static LOOKUPS: RefCell<Vec<PendingLookup>> = const { RefCell::new(Vec::new()) };
}

pub(super) fn wire(win: &MainWindow) {
    let g = win.global::<ShareState>();
    g.on_search(|q| with_app(|a| a.share_search(q.as_str())));
    g.on_toggle(|jid| with_app(|a| a.share_toggle(jid.as_str())));
    g.on_rename(|name| with_app(|a| a.share_rename(name.as_str())));
    g.on_send(|| with_app(App::share_send));
    g.on_cancel(|| with_app(App::share_close));
    g.on_share_card(|id| with_app(|a| a.share_card(id.as_str())));
    g.on_share_info(|| with_app(App::share_info));
    win.global::<AttachActions>().on_contact(|| with_app(App::open_share_contact));
}

impl App {
    /// "Share contact" of the attach menu: pick contacts to send into the open chat.
    pub(super) fn open_share_contact(&self) {
        let p = self.p();
        let Some(chat) = p.st.borrow().selected.clone() else { return };
        if self.refuse_blocked(&chat) {
            return;
        }
        let all = {
            let st = p.st.borrow();
            contact_candidates(&st.chats, |jid| p.store.contact(jid).ok().flatten().and_then(|c| c.phone))
        };
        self.share_open(Target::Contacts { chat }, all, "Share contact", "Search name or number", "Pick up to 20 contacts");
    }

    /// "Share contact" of a card bubble: pick chats to send that card to.
    fn share_card(&self, msg_id: &str) {
        let cards = {
            let p = self.p();
            let st = p.st.borrow();
            let text = st.msgs.iter().find(|m| m.id == msg_id && models::is_contact_card(m)).and_then(|m| m.text.clone());
            text.map(|t| {
                models::parse_shared_contacts(&t)
                    .into_iter()
                    .filter(|c| !c.phone.is_empty())
                    .map(|c| SharedContact { name: c.name, phones: vec![c.phone] })
                    .collect::<Vec<_>>()
            })
        };
        if let Some(cards) = cards.filter(|c| !c.is_empty()) {
            self.share_to_chats(cards);
        }
    }

    /// "Share contact" of the info panel: pick chats to send this person to.
    fn share_info(&self) {
        if let Some(card) = self.info_shared_contact() {
            self.share_to_chats(vec![card]);
        }
    }

    /// "Add members" of a group the viewer administers: pick contacts that are
    /// not in it yet. `members` is `(jid, phone digits)` of the current members.
    pub(super) fn open_add_members(&self, group: &str, members: &[(String, String)]) {
        let p = self.p();
        let all = {
            let st = p.st.borrow();
            let all = contact_candidates(&st.chats, |jid| p.store.contact(jid).ok().flatten().and_then(|c| c.phone));
            exclude_members(all, members)
        };
        let target = Target::AddMembers { group: group.to_string() };
        self.share_open(target, all, "Add members", "Search name or number", &format!("Pick up to {MAX_ADD_MEMBERS} contacts"));
    }

    /// "New group" of the chat-list menu: pick the first members. `preselected`
    /// are kept ticked when coming back from the name step.
    pub(super) fn open_new_group_picker(&self, preselected: &[String]) {
        let p = self.p();
        let all = {
            let st = p.st.borrow();
            contact_candidates(&st.chats, |jid| p.store.contact(jid).ok().flatten().and_then(|c| c.phone))
        };
        self.share_open(Target::NewGroup, all, "Add group members", "Search name or number", "Pick up to 256 contacts");
        SHARE.with(|s| {
            if let Some(d) = s.borrow_mut().as_mut() {
                let keep: Vec<String> = preselected.iter().filter(|j| d.all.iter().any(|c| &c.jid == *j)).cloned().collect();
                d.picked = keep;
            }
        });
        self.share_refresh();
    }

    fn share_to_chats(&self, cards: Vec<SharedContact>) {
        let p = self.p();
        let all = {
            let st = p.st.borrow();
            let blocked = self.blocked_matcher(&p);
            chat_candidates(&st.chats, blocked)
        };
        let title = match cards.as_slice() {
            [one] => format!("Send {} to", one.name),
            many => format!("Send {} contacts to", many.len()),
        };
        self.share_open(Target::Chats { cards }, all, &title, "Search chats", "Pick up to 5 chats");
    }

    fn share_open(&self, target: Target, all: Vec<Candidate>, title: &str, hint: &str, note: &str) {
        let Some(win) = self.win() else { return };
        let model = Rc::new(VecModel::default());
        let chat_mode = matches!(target, Target::Chats { .. });
        let adding = matches!(target, Target::AddMembers { .. });
        let new_group = matches!(target, Target::NewGroup);
        SHARE.with(|s| {
            *s.borrow_mut() =
                Some(Draft {
                    profile: self.p().id.clone(),
                    target,
                    all,
                    query: String::new(),
                    picked: Vec::new(),
                    names: HashMap::new(),
                    model: model.clone(),
                });
        });
        let g = win.global::<ShareState>();
        g.set_title(title.into());
        g.set_hint(hint.into());
        g.set_send_label(if chat_mode { "Send" } else if adding { "Add" } else if new_group { "Next" } else { "Share" }.into());
        g.set_empty_text(
            if chat_mode {
                "No chats"
            } else if adding {
                "No contacts to add"
            } else {
                "No contacts with a phone number"
            }
            .into(),
        );
        g.set_note(note.into());
        g.set_rows(ModelRc::from(model));
        g.set_can_edit_name(false);
        g.set_edit_name(Default::default());
        g.set_picked(0);
        g.set_open(true);
        self.share_refresh();
    }

    /// The active profile changed: an open picker belongs to the old one.
    pub(super) fn share_profile_changed(&self) {
        if SHARE.with(|s| s.borrow().is_some()) {
            self.share_close();
        }
    }

    pub(super) fn share_close(&self) {
        SHARE.with(|s| *s.borrow_mut() = None);
        if let Some(win) = self.win() {
            let g = win.global::<ShareState>();
            g.set_open(false);
            g.set_rows(ModelRc::new(VecModel::default()));
            win.invoke_focus_composer();
        }
    }

    fn share_search(&self, q: &str) {
        SHARE.with(|s| {
            if let Some(d) = s.borrow_mut().as_mut() {
                d.query = q.trim().to_string();
            }
        });
        self.share_refresh();
    }

    /// Rebuilds the rows for the current search.
    fn share_refresh(&self) {
        let snapshot = SHARE.with(|s| {
            let s = s.borrow();
            let d = s.as_ref()?;
            let rows: Vec<Candidate> = filter_candidates(&d.all, &d.query).into_iter().cloned().collect();
            Some((rows, d.picked.clone(), d.model.clone()))
        });
        let Some((rows, picked, model)) = snapshot else { return };
        let p = self.p();
        let items: Vec<ShareRow> = {
            let mut st = p.st.borrow_mut();
            rows.iter().map(|c| self.share_row(&p, &mut st, c, picked.contains(&c.jid))).collect()
        };
        model.set_vec(items);
        self.share_publish();
    }

    fn share_row(&self, p: &ProfileState, st: &mut State, c: &Candidate, selected: bool) -> ShareRow {
        let avatar = self.avatar(p, st, &c.jid);
        ShareRow {
            key: c.jid.as_str().into(),
            name: c.name.as_str().into(),
            phone: if c.phone.is_empty() { Default::default() } else { super::contact_info::format_phone(&c.phone).into() },
            initials: models::initials(&c.name).into(),
            tint: models::tint_for(&c.jid),
            has_avatar: avatar.is_some(),
            avatar: avatar.unwrap_or_default(),
            selected,
        }
    }

    /// Count, note and the name field follow the pick.
    fn share_publish(&self) {
        let Some(win) = self.win() else { return };
        let state = SHARE.with(|s| {
            let s = s.borrow();
            let d = s.as_ref()?;
            let names: Vec<&str> = d.picked.iter().filter_map(|j| d.all.iter().find(|c| &c.jid == j)).map(|c| c.name.as_str()).collect();
            let edit = d.editable().map(|c| d.names.get(&c.jid).cloned().unwrap_or_else(|| c.name.clone()));
            Some((d.picked.len(), names.join(", "), d.max(), !d.chat_mode(), edit))
        });
        let Some((count, names, max, contacts, edit)) = state else { return };
        let g = win.global::<ShareState>();
        g.set_picked(i32::try_from(count).unwrap_or(i32::MAX));
        if names.is_empty() {
            let what = if contacts { "contacts" } else { "chats" };
            g.set_note(format!("Pick up to {max} {what}").into());
        } else {
            g.set_note(names.into());
        }
        g.set_can_edit_name(edit.is_some());
        if let Some(name) = edit {
            g.set_edit_name(name.into());
        }
    }

    pub(super) fn share_toggle(&self, jid: &str) {
        let Some(win) = self.win() else { return };
        let outcome = SHARE.with(|s| {
            s.borrow_mut().as_mut().map(|d| {
                let max = d.max();
                (toggle_limited(&mut d.picked, jid, max), d.model.clone(), max)
            })
        });
        let Some((outcome, model, max)) = outcome else { return };
        if outcome == Toggle::Full {
            let text = SHARE.with(|s| match s.borrow().as_ref().map(|d| &d.target) {
                Some(Target::Contacts { .. }) => format!("You can share up to {max} contacts"),
                Some(Target::AddMembers { .. }) => format!("You can add up to {max} members at once"),
                Some(Target::NewGroup) => format!("A group starts with at most {max} members"),
                _ => format!("You can pick up to {max} chats"),
            });
            win.global::<ShareState>().set_note(text.into());
            return;
        }
        for i in 0..model.row_count() {
            let Some(mut row) = model.row_data(i) else { continue };
            if row.key.as_str() == jid {
                row.selected = outcome == Toggle::Added;
                model.set_row_data(i, row);
                break;
            }
        }
        self.share_publish();
    }

    /// The name field was edited: remembered for the one picked contact.
    fn share_rename(&self, name: &str) {
        SHARE.with(|s| {
            let mut s = s.borrow_mut();
            let Some(d) = s.as_mut() else { return };
            let Some(jid) = d.editable().map(|c| c.jid.clone()) else { return };
            d.names.insert(jid, name.to_string());
        });
    }

    pub(super) fn share_send(&self) {
        let opened_for = SHARE.with(|s| s.borrow().as_ref().map(|d| d.profile.clone()));
        if opened_for.is_some_and(|opened| !profile_matches(&opened, &self.p().id)) {
            self.share_close();
            self.show_toast("The profile changed; nothing was sent");
            return;
        }
        let new_group = SHARE.with(|s| {
            let s = s.borrow();
            let d = s.as_ref()?;
            matches!(d.target, Target::NewGroup)
                .then(|| d.picked.iter().filter_map(|j| d.all.iter().find(|c| &c.jid == j)).cloned().collect::<Vec<_>>())
        });
        if let Some(members) = new_group {
            if members.is_empty() {
                return;
            }
            self.share_close();
            self.new_group_details(members);
            return;
        }
        let add = SHARE.with(|s| {
            let s = s.borrow();
            let d = s.as_ref()?;
            let Target::AddMembers { group } = &d.target else { return None };
            let jids: Vec<String> = d.picked.iter().filter_map(|j| d.all.iter().find(|c| &c.jid == j)).map(member_add_jid).collect();
            Some((group.clone(), jids))
        });
        if let Some((group, jids)) = add {
            self.share_close();
            self.add_members_picked(&group, jids);
            return;
        }
        let draft = SHARE.with(|s| {
            s.borrow().as_ref().map(|d| match &d.target {
                Target::Contacts { chat } => (Some(chat.clone()), build_cards(&d.picked, &d.all, &d.names), d.picked.clone()),
                Target::Chats { cards } => (None, cards.clone(), d.picked.clone()),
                Target::AddMembers { .. } | Target::NewGroup => (None, Vec::new(), Vec::new()),
            })
        });
        let Some((chat, cards, picked)) = draft else { return };
        if cards.is_empty() || picked.is_empty() {
            return;
        }
        self.share_close();
        match chat {
            Some(chat_jid) => {
                let p = self.p();
                if p.st.borrow().selected.as_deref() != Some(chat_jid.as_str()) {
                    self.show_toast("The chat was closed; nothing was sent");
                    return;
                }
                if self.refuse_blocked(&chat_jid) || self.refuse_gated() {
                    return;
                }
                let quoted = {
                    let mut st = p.st.borrow_mut();
                    st.force_bottom = true;
                    st.reply.take().map(|r| r.0)
                };
                self.send(Command::SendContact { chat_jid, contacts: cards, quoted_id: quoted });
                self.sync_reply();
            }
            None => {
                let targets: Vec<String> = picked.into_iter().filter(|jid| !self.is_blocked_dm(jid)).collect();
                for chat_jid in &targets {
                    self.send(Command::SendContact { chat_jid: chat_jid.clone(), contacts: cards.clone(), quoted_id: None });
                }
                match targets.len() {
                    0 => {}
                    1 => self.show_toast("Contact sent"),
                    _ => self.show_toast("Contact sent to the chats"),
                }
            }
        }
    }

    // ---- "Message" of a contact card ---------------------------------------------------

    /// A card's "Message": opens the chat when the number is known; otherwise
    /// asks WhatsApp whether it is registered first.
    pub(super) fn contact_message(&self, digits: &str) {
        let Some(jid) = models::contact_chat_jid(digits) else {
            self.message_contact(digits);
            return;
        };
        let known = {
            let p = self.p();
            matches!(p.store.chat(&jid), Ok(Some(_))) || matches!(p.store.contact(&jid), Ok(Some(_)))
        };
        if known {
            self.message_contact(digits);
        } else {
            self.lookup_and_open(digits);
        }
    }

    /// Asks the engine whether `digits` is on WhatsApp; the answer opens the
    /// chat or tells the user it is not registered (see `on_phone_lookup`).
    pub(super) fn lookup_and_open(&self, digits: &str) {
        let profile = self.p().id.clone();
        let now = Instant::now();
        LOOKUPS.with(|l| {
            let mut l = l.borrow_mut();
            l.retain(|x| now.saturating_duration_since(x.at) < LOOKUP_TTL && !(x.profile == profile && x.digits == digits));
            l.push(PendingLookup { profile, digits: digits.to_string(), at: now });
        });
        self.show_toast("Checking WhatsApp...");
        self.send(Command::LookupPhones { numbers: vec![format!("+{digits}")] });
    }

    /// `UiEvent::PhoneLookup`: finishes the lookups started by `lookup_and_open`
    /// that the answer covers. Other users of `LookupPhones` can reuse this
    /// entry point by recording their own pending request here.
    pub(super) fn on_phone_lookup(&self, profile: &str, active: bool, results: &[PhoneLookupResult]) {
        let now = Instant::now();
        let done: Vec<(String, LookupOutcome)> = LOOKUPS.with(|l| {
            let mut l = l.borrow_mut();
            l.retain(|x| now.saturating_duration_since(x.at) < LOOKUP_TTL);
            let mut done = Vec::new();
            l.retain(|x| {
                let outcome = if x.profile == profile { lookup_outcome(results, &x.digits) } else { LookupOutcome::Unknown };
                let finished = outcome != LookupOutcome::Unknown;
                if finished {
                    done.push((x.digits.clone(), outcome));
                }
                !finished
            });
            done
        });
        if !active {
            return;
        }
        for (digits, outcome) in done {
            match outcome {
                LookupOutcome::Registered(jid) => self.open_registered(&jid, &digits),
                LookupOutcome::NotOnWhatsApp => self.show_toast("This number isn't on WhatsApp"),
                LookupOutcome::Unknown => {}
            }
        }
    }

    /// Opens the direct chat with a registered number, creating it when needed.
    fn open_registered(&self, jid: &str, digits: &str) {
        let p = self.p();
        if !matches!(p.store.chat(jid), Ok(Some(_))) {
            let name = format!("+{digits}");
            if self.ok("create chat", p.store.upsert_chat(jid, ChatKind::Dm, Some(&name))).is_none() {
                return;
            }
        }
        if let Some(win) = self.win() {
            win.set_status_mode(false);
        }
        self.select_chat(jid);
        self.refresh_chats();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(jid: &str, name: &str, ts: i64) -> ChatRow {
        ChatRow {
            jid: jid.into(),
            kind: ChatKind::from_jid(jid),
            display_name: name.into(),
            last_ts: ts,
            unread: 0,
            pinned: false,
            archived: false,
            muted_until: 0,
            last_preview: None,
            receipts_off: false,
            ephemeral_secs: 0,
            ephemeral_ts: 0,
        }
    }

    fn sample() -> Vec<Candidate> {
        let chats = [
            chat("4915111@s.whatsapp.net", "bob", 5),
            chat("989123456789@s.whatsapp.net", "Alice", 9),
            chat("999@g.us", "Family", 7),
            chat("abc@lid", "Lidy", 3),
            chat("12@s.whatsapp.net", "Short", 1),
        ];
        contact_candidates(&chats, |jid| (jid == "abc@lid").then(|| "+44 7911 123456".to_string()))
    }

    #[test]
    fn candidates_need_a_phone_number_and_are_sorted_by_name() {
        let all = sample();
        assert_eq!(all.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["Alice", "bob", "Lidy"]);
        assert_eq!(all[0].phone, "+989123456789");
        assert_eq!(all[2].phone, "+447911123456");
    }

    #[test]
    fn filter_matches_name_or_number() {
        let all = sample();
        let names = |q: &str| filter_candidates(&all, q).into_iter().map(|c| c.name.clone()).collect::<Vec<_>>();
        assert_eq!(names("").len(), 3);
        assert_eq!(names("ALI"), ["Alice"]);
        assert_eq!(names("+49 151"), ["bob"]);
        assert_eq!(names("7911"), ["Lidy"]);
        assert!(names("zzz").is_empty());
    }

    #[test]
    fn current_members_are_not_offered_for_adding() {
        let all = sample();
        let members = vec![
            ("989123456789:3@s.whatsapp.net".to_string(), String::new()),
            ("999@lid".to_string(), "4915111".to_string()),
            ("1@lid".to_string(), String::new()),
        ];
        let left = exclude_members(all.clone(), &members);
        assert_eq!(left.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["Lidy"]);
        assert_eq!(exclude_members(all.clone(), &[]).len(), all.len());
    }

    #[test]
    fn members_are_added_under_phone_number_addresses() {
        let all = sample();
        assert_eq!(member_add_jid(&all[0]), "989123456789@s.whatsapp.net");
        assert_eq!(member_add_jid(&all[2]), "447911123456@s.whatsapp.net");
    }

    #[test]
    fn a_picker_only_acts_in_the_profile_it_was_opened_for() {
        assert!(profile_matches("p1", "p1"));
        assert!(!profile_matches("p1", "p2"));
        assert!(!profile_matches("", "p2"));
    }

    #[test]
    fn adding_members_is_capped_at_twenty() {
        assert_eq!(MAX_ADD_MEMBERS, 20);
        let mut picked = Vec::new();
        for i in 0..MAX_ADD_MEMBERS {
            assert_eq!(toggle_limited(&mut picked, &format!("m{i}"), MAX_ADD_MEMBERS), Toggle::Added);
        }
        assert_eq!(toggle_limited(&mut picked, "extra", MAX_ADD_MEMBERS), Toggle::Full);
    }

    #[test]
    fn at_most_twenty_contacts_can_be_picked() {
        let mut picked = Vec::new();
        for i in 0..MAX_SHARED_CONTACTS {
            assert_eq!(toggle_limited(&mut picked, &format!("c{i}"), MAX_SHARED_CONTACTS), Toggle::Added);
        }
        assert_eq!(toggle_limited(&mut picked, "extra", MAX_SHARED_CONTACTS), Toggle::Full);
        assert_eq!(picked.len(), MAX_SHARED_CONTACTS);
        assert_eq!(toggle_limited(&mut picked, "c3", MAX_SHARED_CONTACTS), Toggle::Removed);
        assert_eq!(toggle_limited(&mut picked, "extra", MAX_SHARED_CONTACTS), Toggle::Added);
    }

    #[test]
    fn cards_use_the_edited_name_when_it_is_not_blank() {
        let all = sample();
        let picked = vec!["989123456789@s.whatsapp.net".to_string(), "4915111@s.whatsapp.net".to_string()];
        let mut names = HashMap::new();
        names.insert(picked[0].clone(), "  Ali J  ".to_string());
        names.insert(picked[1].clone(), "   ".to_string());
        let cards = build_cards(&picked, &all, &names);
        assert_eq!(cards[0], SharedContact { name: "Ali J".into(), phones: vec!["+989123456789".into()] });
        assert_eq!(cards[1].name, "bob");
        assert!(build_cards(&["unknown".to_string()], &all, &names).is_empty());
    }

    #[test]
    fn chat_candidates_skip_blocked_and_status_and_sort_by_recency() {
        let chats = [
            chat("1@s.whatsapp.net", "One", 1),
            chat("2@s.whatsapp.net", "Two", 3),
            chat("999@g.us", "Group", 2),
            chat(STATUS_JID, "Status", 9),
        ];
        let rows = chat_candidates(&chats, |j| j == "1@s.whatsapp.net");
        assert_eq!(rows.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["Two", "Group"]);
    }

    fn found(number: &str, jid: Option<&str>) -> PhoneLookupResult {
        PhoneLookupResult { number: number.into(), jid: jid.map(str::to_string), ..Default::default() }
    }

    #[test]
    fn lookup_outcome_tells_registered_from_unregistered() {
        let results = [found("+34600123456", Some("34600123456@s.whatsapp.net")), found("+15551234567", None), found("+1", Some(""))];
        assert_eq!(lookup_outcome(&results, "34600123456"), LookupOutcome::Registered("34600123456@s.whatsapp.net".into()));
        assert_eq!(lookup_outcome(&results, "15551234567"), LookupOutcome::NotOnWhatsApp);
        assert_eq!(lookup_outcome(&results, "1"), LookupOutcome::NotOnWhatsApp);
        assert_eq!(lookup_outcome(&results, "999"), LookupOutcome::Unknown);
        assert_eq!(lookup_outcome(&[], "34600123456"), LookupOutcome::Unknown);
    }
}
