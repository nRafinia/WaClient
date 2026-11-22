//! Starting a group and joining one through an invite link. Step one of a new
//! group is the contact picker (`share_ui`, `Target::NewGroup`); this file
//! owns the name/photo step, the invite-link dialogs and the engine's answers
//! (`GroupCreated`, `InvitePreview`, `GroupJoined`).

use std::cell::RefCell;
use std::path::PathBuf;

use slint::{ComponentHandle, ModelRc, VecModel};
use wa_engine::{Command, InvitePreview, MemberAction, MemberChangeResult};

use super::share_ui::{Candidate, member_add_jid, profile_matches};
use super::{App, ProfileState, with_app};
use crate::{GroupNew, MainWindow, ShareRow, models};

/// Longest group name WhatsApp accepts.
const MAX_SUBJECT_CHARS: usize = 100;
/// The only host whose links are invite links.
const INVITE_HOST: &str = "chat.whatsapp.com";
/// Shortest and longest bare invite code accepted from the paste field.
const CODE_LEN: std::ops::RangeInclusive<usize> = 10..=40;

/// Dialog numbers shared with `group_new.slint`.
const DIALOG_NONE: i32 = 0;
const DIALOG_NAME: i32 = 1;
const DIALOG_LINK: i32 = 2;
const DIALOG_PREVIEW: i32 = 3;

/// The invite link a preview was asked for, and in which profile.
struct Request {
    profile: String,
    link: String,
}

#[derive(Default)]
struct Flow {
    /// Profile the name step was opened for; creating from another one is refused.
    profile: String,
    /// Members picked in step one, in pick order.
    members: Vec<Candidate>,
    picture: Option<PathBuf>,
    photo_busy: bool,
    /// Profile whose `CreateGroup` has not been answered yet.
    creating_for: Option<String>,
    /// Profile whose `JoinGroup` has not been answered yet.
    joining_for: Option<String>,
    request: Option<Request>,
    /// The answer of the open preview, when the link is usable.
    preview: Option<InvitePreview>,
}

thread_local! {
    static FLOW: RefCell<Flow> = RefCell::default();
}

/// The trimmed group name, or why it cannot be used.
pub(super) fn validate_subject(raw: &str) -> Result<String, &'static str> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("The group name can't be empty");
    }
    if name.chars().count() > MAX_SUBJECT_CHARS {
        return Err("The group name can have at most 100 characters");
    }
    Ok(name.to_string())
}

/// The text after `http://` or `https://` (any case), if there is such a scheme.
fn after_scheme(url: &str) -> Option<&str> {
    ["https://", "http://"].into_iter().find_map(|scheme| {
        let head = url.get(..scheme.len())?;
        head.eq_ignore_ascii_case(scheme).then(|| &url[scheme.len()..])
    })
}

/// True for a web link whose host is exactly `chat.whatsapp.com`.
pub fn is_invite_link(url: &str) -> bool {
    let Some(rest) = after_scheme(url.trim()) else { return false };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    authority.eq_ignore_ascii_case(INVITE_HOST)
}

/// What the paste field holds, as something `PreviewInvite` accepts: an invite
/// link (a missing scheme is added) or a bare invite code.
pub fn invite_input(raw: &str) -> Option<String> {
    let text = raw.trim();
    let has_code = |link: &str| {
        let rest = after_scheme(link).unwrap_or_default();
        rest.split(['?', '#']).next().and_then(|path| path.split('/').nth(1)).is_some_and(|code| !code.is_empty())
    };
    if is_invite_link(text) {
        return has_code(text).then(|| text.to_string());
    }
    let schemeless = format!("{INVITE_HOST}/");
    if text.get(..schemeless.len()).is_some_and(|h| h.eq_ignore_ascii_case(&schemeless)) {
        let link = format!("https://{text}");
        return has_code(&link).then_some(link);
    }
    let bare = CODE_LEN.contains(&text.len()) && text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    bare.then(|| text.to_string())
}

/// An answer belongs to the request when it names the same link.
pub fn reply_matches(requested: &str, reply: &str) -> bool {
    !requested.trim().is_empty() && requested.trim() == reply.trim()
}

fn member_count_text(count: u32) -> String {
    match count {
        1 => "Group · 1 member".to_string(),
        n => format!("Group · {n} members"),
    }
}

/// The failures of `GroupCreated` as add results the failure dialog understands.
fn failed_adds(failed: &[MemberChangeResult]) -> Vec<MemberChangeResult> {
    failed.iter().cloned().map(|r| MemberChangeResult { ok: false, ..r }).collect()
}

pub(super) fn wire(win: &MainWindow) {
    let g = win.global::<GroupNew>();
    g.on_open_new(|| with_app(|a| a.open_new_group_picker(&[])));
    g.on_open_join(|| with_app(App::join_open_dialog));
    g.on_choose_photo(|| with_app(App::new_group_choose_photo));
    g.on_back(|| with_app(App::new_group_back));
    g.on_create(|subject| with_app(|a| a.new_group_create(subject.as_str())));
    g.on_preview(|text| with_app(|a| a.join_preview_input(text.as_str())));
    g.on_join(|| with_app(App::join_confirm));
    g.on_open_chat(|| with_app(App::join_open_chat));
    g.on_cancel(|| with_app(App::group_new_cancel));
}

impl App {
    /// Step two of a new group: its name and photo, with the picked members listed.
    pub(super) fn new_group_details(&self, members: Vec<Candidate>) {
        let Some(win) = self.win() else { return };
        let p = self.p();
        let rows: Vec<ShareRow> = {
            let mut st = p.st.borrow_mut();
            members
                .iter()
                .map(|c| {
                    let avatar = self.avatar(&p, &mut st, &c.jid);
                    ShareRow {
                        key: c.jid.as_str().into(),
                        name: c.name.as_str().into(),
                        phone: Default::default(),
                        initials: models::initials(&c.name).into(),
                        tint: models::tint_for(&c.jid),
                        has_avatar: avatar.is_some(),
                        avatar: avatar.unwrap_or_default(),
                        selected: true,
                    }
                })
                .collect()
        };
        FLOW.with(|f| {
            let mut f = f.borrow_mut();
            f.profile = p.id.clone();
            f.members = members;
            f.picture = None;
            f.creating_for = None;
        });
        let g = win.global::<GroupNew>();
        g.set_subject(Default::default());
        g.set_photo(Default::default());
        g.set_has_photo(false);
        g.set_members(ModelRc::new(VecModel::from(rows)));
        g.set_busy(false);
        g.set_dialog(DIALOG_NAME);
    }

    /// "Back" of the name step: the picker again, with the same members ticked.
    fn new_group_back(&self) {
        let keys: Vec<String> = FLOW.with(|f| f.borrow().members.iter().map(|c| c.jid.clone()).collect());
        self.group_new_cancel();
        self.open_new_group_picker(&keys);
    }

    /// Closes whichever dialog of this flow is open and forgets its requests.
    pub(super) fn group_new_cancel(&self) {
        FLOW.with(|f| {
            let mut f = f.borrow_mut();
            f.members.clear();
            f.picture = None;
            f.creating_for = None;
            f.joining_for = None;
            f.request = None;
            f.preview = None;
        });
        let Some(win) = self.win() else { return };
        let g = win.global::<GroupNew>();
        g.set_dialog(DIALOG_NONE);
        g.set_busy(false);
        g.set_members(ModelRc::new(VecModel::default()));
        g.set_photo(Default::default());
        g.set_has_photo(false);
        win.invoke_focus_composer();
    }

    fn new_group_choose_photo(&self) {
        if FLOW.with(|f| std::mem::replace(&mut f.borrow_mut().photo_busy, true)) {
            return;
        }
        let profile = self.p().id.clone();
        let spawned = std::thread::Builder::new().name("pick-new-group-photo".into()).spawn(move || {
            let picked = rfd::FileDialog::new()
                .set_title("Choose a group photo")
                .add_filter("Images", &["jpg", "jpeg", "png"])
                .pick_file();
            let queued = slint::invoke_from_event_loop(move || with_app(|a| a.new_group_photo_picked(&profile, picked)));
            if let Err(e) = queued {
                log::error!("could not deliver the picked group photo to the UI: {e}");
            }
        });
        if let Err(e) = spawned {
            FLOW.with(|f| f.borrow_mut().photo_busy = false);
            self.show_banner(1, &format!("Could not open the file picker: {e}"));
        }
    }

    fn new_group_photo_picked(&self, profile: &str, picked: Option<PathBuf>) {
        FLOW.with(|f| f.borrow_mut().photo_busy = false);
        let Some(path) = picked else { return };
        let Some(win) = self.win() else { return };
        let g = win.global::<GroupNew>();
        // The dialog may have been closed, or the profile switched, meanwhile.
        if g.get_dialog() != DIALOG_NAME || self.p().id != profile {
            return;
        }
        match slint::Image::load_from_path(&path) {
            Ok(image) => {
                g.set_photo(image);
                g.set_has_photo(true);
                FLOW.with(|f| f.borrow_mut().picture = Some(path));
            }
            Err(_) => self.show_banner(1, "That image could not be opened"),
        }
    }

    fn new_group_create(&self, raw: &str) {
        let subject = match validate_subject(raw) {
            Ok(s) => s,
            Err(why) => {
                self.show_toast(why);
                return;
            }
        };
        let p = self.p();
        if !FLOW.with(|f| profile_matches(&f.borrow().profile, &p.id)) {
            self.group_new_cancel();
            self.show_toast("The profile changed; no group was created");
            return;
        }
        let request = FLOW.with(|f| {
            let mut f = f.borrow_mut();
            if f.creating_for.is_some() || f.members.is_empty() {
                return None;
            }
            f.creating_for = Some(p.id.clone());
            let participants: Vec<String> = f.members.iter().map(member_add_jid).collect();
            Some((participants, f.picture.clone()))
        });
        let Some((participants, picture)) = request else { return };
        if let Some(win) = self.win() {
            win.global::<GroupNew>().set_busy(true);
        }
        log::info!("group: CreateGroup with {} participant(s)", participants.len());
        self.send(Command::CreateGroup { subject, participants, picture });
    }

    /// Opens a group's chat in the chat list (switching away from statuses).
    fn open_group_chat(&self, jid: &str) {
        if let Some(win) = self.win() {
            win.set_status_mode(false);
        }
        self.refresh_chats();
        self.select_chat(jid);
    }

    /// `UiEvent::GroupCreated`: closes the flow, opens the chat and lists the
    /// members that could not be added.
    pub(super) fn on_group_created(&self, p: &ProfileState, active: bool, group_jid: &str, failed: &[MemberChangeResult]) {
        let mine = FLOW.with(|f| {
            let mut f = f.borrow_mut();
            let mine = f.creating_for.as_deref() == Some(p.id.as_str());
            if mine {
                f.creating_for = None;
            }
            mine
        });
        if mine {
            self.group_new_cancel();
        }
        if !active {
            return;
        }
        if mine {
            self.open_group_chat(group_jid);
        } else {
            self.show_toast("Group created");
        }
        if !failed.is_empty() {
            // After the chat opened: opening a chat resets the group dialogs.
            self.on_group_members_result(p, active, group_jid, MemberAction::Add, &failed_adds(failed));
        }
    }

    /// Tells the user a create or join failed; another profile's failure is named.
    fn group_failure_notice(&self, p: &ProfileState, active: bool, message: &str) {
        if active {
            self.show_banner(1, message);
        } else {
            self.show_banner(1, &format!("[{}] {message}", p.name()));
        }
    }

    /// `UiEvent::GroupCreateFailed`: the Create button works again when the
    /// request that was waiting was this profile's.
    pub(super) fn on_group_create_failed(&self, p: &ProfileState, active: bool, message: &str) {
        let mine = FLOW.with(|f| {
            let mut f = f.borrow_mut();
            let mine = f.creating_for.as_deref() == Some(p.id.as_str());
            if mine {
                f.creating_for = None;
            }
            mine
        });
        if mine && active {
            if let Some(win) = self.win() {
                win.global::<GroupNew>().set_busy(false);
            }
        }
        self.group_failure_notice(p, active, message);
    }

    /// `UiEvent::GroupJoinFailed`: Join works again when the failed link is the
    /// one of the request that was waiting.
    pub(super) fn on_group_join_failed(&self, p: &ProfileState, active: bool, link: &str, message: &str) {
        let mine = FLOW.with(|f| {
            let mut f = f.borrow_mut();
            let asked = f.request.as_ref().is_some_and(|r| r.profile == p.id && reply_matches(&r.link, link));
            let mine = asked && f.joining_for.as_deref() == Some(p.id.as_str());
            if mine {
                f.joining_for = None;
            }
            mine
        });
        if mine && active {
            if let Some(win) = self.win() {
                win.global::<GroupNew>().set_busy(false);
            }
        }
        self.group_failure_notice(p, active, message);
    }

    /// The active profile changed: a dialog of this flow belongs to the old one.
    pub(super) fn group_new_profile_changed(&self) {
        let open = self.win().is_some_and(|w| w.global::<GroupNew>().get_dialog() != DIALOG_NONE);
        if open {
            self.group_new_cancel();
        }
    }

    // ---- joining through an invite link ------------------------------------------------

    /// "Join group by link…" of the chat-list menu.
    fn join_open_dialog(&self) {
        let Some(win) = self.win() else { return };
        let g = win.global::<GroupNew>();
        g.set_link_text(Default::default());
        g.set_busy(false);
        g.set_dialog(DIALOG_LINK);
    }

    /// "Next" of the paste dialog.
    fn join_preview_input(&self, text: &str) {
        match invite_input(text) {
            Some(link) => self.preview_invite(&link),
            None => self.show_toast("That doesn't look like a group invite link"),
        }
    }

    /// Asks what `link` leads to and shows the preview dialog (loading until
    /// the answer arrives). Also the entry point of links clicked in messages.
    pub(super) fn preview_invite(&self, link: &str) {
        let Some(win) = self.win() else { return };
        let profile = self.p().id.clone();
        FLOW.with(|f| {
            let mut f = f.borrow_mut();
            f.request = Some(Request { profile, link: link.to_string() });
            f.preview = None;
            f.joining_for = None;
        });
        let g = win.global::<GroupNew>();
        g.set_pv_loading(true);
        g.set_pv_error(Default::default());
        g.set_busy(false);
        g.set_dialog(DIALOG_PREVIEW);
        self.send(Command::PreviewInvite { link: link.to_string() });
    }

    /// `UiEvent::InvitePreview`: fills the dialog when it is the answer to the
    /// request in flight.
    pub(super) fn on_invite_preview(&self, p: &ProfileState, active: bool, preview: InvitePreview) {
        let wanted = FLOW.with(|f| {
            f.borrow().request.as_ref().is_some_and(|r| r.profile == p.id && reply_matches(&r.link, &preview.link))
        });
        if !wanted || !active {
            return;
        }
        let Some(win) = self.win() else { return };
        let g = win.global::<GroupNew>();
        let error = preview.error.as_deref().map(str::trim).filter(|e| !e.is_empty());
        let subject = preview.subject.as_deref().map(str::trim).filter(|s| !s.is_empty());
        g.set_pv_loading(false);
        match (error, subject) {
            (None, Some(subject)) => {
                g.set_pv_error(Default::default());
                g.set_pv_subject(subject.into());
                g.set_pv_initials(models::initials(subject).into());
                g.set_pv_tint(models::tint_for(subject));
                g.set_pv_count(member_count_text(preview.member_count).into());
                g.set_pv_description(preview.description.as_deref().map(str::trim).unwrap_or_default().into());
                g.set_pv_approval(preview.approval_required);
                g.set_pv_member(preview.already_member && preview.group_jid.is_some());
                FLOW.with(|f| f.borrow_mut().preview = Some(preview));
            }
            _ => {
                g.set_pv_error(error.unwrap_or("This invite link is invalid or has expired.").into());
                FLOW.with(|f| f.borrow_mut().preview = None);
            }
        }
    }

    /// "Join group" of the preview.
    fn join_confirm(&self) {
        let p = self.p();
        let link = FLOW.with(|f| {
            let mut f = f.borrow_mut();
            if f.joining_for.is_some() || f.preview.is_none() {
                return None;
            }
            let link = f.request.as_ref().filter(|r| r.profile == p.id)?.link.clone();
            f.joining_for = Some(p.id.clone());
            Some(link)
        });
        let Some(link) = link else { return };
        if let Some(win) = self.win() {
            win.global::<GroupNew>().set_busy(true);
        }
        self.send(Command::JoinGroup { link });
    }

    /// "Open chat" of the preview of a group we are already in.
    fn join_open_chat(&self) {
        let jid = FLOW.with(|f| f.borrow().preview.as_ref().and_then(|v| v.group_jid.clone()));
        self.group_new_cancel();
        if let Some(jid) = jid {
            self.open_group_chat(&jid);
        }
    }

    /// `UiEvent::GroupJoined`: a pending request is only announced; otherwise
    /// the group's chat opens.
    pub(super) fn on_group_joined(&self, p: &ProfileState, active: bool, group_jid: &str, pending: bool) {
        let mine = FLOW.with(|f| {
            let mut f = f.borrow_mut();
            let mine = f.joining_for.as_deref() == Some(p.id.as_str());
            if mine {
                f.joining_for = None;
            }
            mine
        });
        if mine {
            self.group_new_cancel();
        }
        if !active {
            return;
        }
        if pending {
            self.show_toast("Request sent. An admin must approve it.");
        } else if mine {
            self.open_group_chat(group_jid);
        } else {
            self.show_toast("You joined the group");
        }
    }
}

#[cfg(debug_assertions)]
impl App {
    /// Demo (`WACLIENT_DEMO_UI=new-group`): the name step with a few members.
    /// The demo chats have no real phone numbers, so the picker is skipped.
    pub fn demo_new_group(&self) {
        let Some(win) = self.win() else { return };
        let p = self.p();
        let members: Vec<Candidate> = p
            .st
            .borrow()
            .chats
            .iter()
            .filter(|c| c.kind == wa_store::ChatKind::Dm)
            .take(4)
            .map(|c| Candidate { jid: c.jid.clone(), name: c.display_name.clone(), phone: "+15550100".to_string(), last_ts: c.last_ts })
            .collect();
        self.new_group_details(members);
        win.global::<GroupNew>().set_subject("Weekend plans".into());
    }

    /// Demo (`WACLIENT_DEMO_UI=join-preview`): the preview of a made-up invite.
    pub fn demo_join_preview(&self) {
        let link = "https://chat.whatsapp.com/DemoInviteCode0123456789";
        self.preview_invite(link);
        let preview = InvitePreview {
            link: link.to_string(),
            subject: Some("Hiking club".to_string()),
            description: Some(
                "Weekend hikes around the lakes. Share routes, photos and lifts. Be kind, keep it on topic, no spam. \
                 New members are welcome, please introduce yourself and tell us which trails you like best."
                    .to_string(),
            ),
            member_count: 87,
            group_jid: Some("123456@g.us".to_string()),
            already_member: false,
            approval_required: true,
            error: None,
        };
        self.on_invite_preview(&self.p(), true, preview);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subject_is_trimmed_and_limited() {
        assert_eq!(validate_subject("  Team  ").as_deref(), Ok("Team"));
        assert!(validate_subject("  \t ").is_err());
        assert!(validate_subject(&"x".repeat(100)).is_ok());
        assert!(validate_subject(&"x".repeat(101)).is_err());
        assert!(validate_subject(&"é".repeat(100)).is_ok());
    }

    #[test]
    fn only_the_invite_host_over_http_is_an_invite_link() {
        assert!(is_invite_link("https://chat.whatsapp.com/AbCdEf123456"));
        assert!(is_invite_link("http://chat.whatsapp.com/AbCdEf123456"));
        assert!(is_invite_link("HTTPS://Chat.WhatsApp.com/AbCdEf123456?x=1"));
        assert!(is_invite_link("https://chat.whatsapp.com"));
        assert!(!is_invite_link("https://whatsapp.com/AbCdEf123456"));
        assert!(!is_invite_link("https://chat.whatsapp.com.evil.com/AbCdEf"));
        assert!(!is_invite_link("https://chat.whatsapp.com@evil.com/AbCdEf"));
        assert!(!is_invite_link("https://evil.com/chat.whatsapp.com/AbCdEf"));
        assert!(!is_invite_link("ftp://chat.whatsapp.com/AbCdEf"));
        assert!(!is_invite_link("mailto:chat.whatsapp.com"));
        assert!(!is_invite_link("chat.whatsapp.com/AbCdEf"));
        assert!(!is_invite_link("é"));
        assert!(!is_invite_link(""));
    }

    #[test]
    fn pasted_text_becomes_a_link_or_code() {
        let code = "AbCdEf1234567890xyZ";
        let link = format!("https://chat.whatsapp.com/{code}");
        assert_eq!(invite_input(&format!("  {link}\n")), Some(link.clone()));
        assert_eq!(invite_input(&format!("chat.whatsapp.com/{code}")), Some(link.clone()));
        assert_eq!(invite_input(code), Some(code.to_string()));
        assert_eq!(invite_input("https://chat.whatsapp.com/"), None);
        assert_eq!(invite_input("https://example.com/AbCdEf1234567890xyZ"), None);
        assert_eq!(invite_input("short"), None);
        assert_eq!(invite_input("two words here that are long"), None);
        assert_eq!(invite_input("   "), None);
    }

    #[test]
    fn replies_are_matched_by_link() {
        assert!(reply_matches("https://chat.whatsapp.com/a", " https://chat.whatsapp.com/a "));
        assert!(!reply_matches("https://chat.whatsapp.com/a", "https://chat.whatsapp.com/b"));
        assert!(!reply_matches("", ""));
    }

    #[test]
    fn member_counts_are_pluralised() {
        assert_eq!(member_count_text(1), "Group · 1 member");
        assert_eq!(member_count_text(0), "Group · 0 members");
        assert_eq!(member_count_text(87), "Group · 87 members");
    }

    #[test]
    fn failed_members_are_shown_as_failed_adds() {
        let failed = [MemberChangeResult { jid: "a@s.whatsapp.net".into(), ok: true, error: None, invite_instead: true }];
        let rows = failed_adds(&failed);
        assert!(!rows[0].ok);
        assert!(rows[0].invite_instead);
    }
}
