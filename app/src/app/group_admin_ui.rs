//! Group administration of the info panel: renaming, description, photo,
//! members, invite link, settings and leaving. The panel itself is drawn from
//! `contact_info` / `info_ui`; this file turns its callbacks into engine
//! commands and the engine's answers into dialogs.

use std::cell::RefCell;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, ModelRc, VecModel};
use wa_engine::{Command, GroupSetting, MemberAction, MemberChangeResult};

use super::contact_info::format_phone;
use super::group_new_ui::validate_subject;
use super::share_ui::MAX_ADD_MEMBERS;
use super::{App, ProfileState, with_app};
use crate::{GroupAdmin, MainWindow, MemberFailure, models};

/// Longest group description WhatsApp accepts.
const MAX_DESCRIPTION_CHARS: usize = 2048;
/// How long a send-invites request waits for the link before it is forgotten.
const INVITE_WAIT: Duration = Duration::from_secs(30);

/// Dialog numbers shared with `group_admin.slint`.
const DIALOG_NONE: i32 = 0;
const DIALOG_FAILURES: i32 = 5;

/// Members whose change failed and who could be sent the invite link instead.
struct Failures {
    profile: String,
    group: String,
    invite: Vec<String>,
}

/// Members waiting for the invite link of a group to arrive.
struct InviteWait {
    profile: String,
    group: String,
    members: Vec<String>,
    since: Instant,
}

/// The members to message when the link of `group` (in `profile`) arrives at
/// `now`. A request older than `INVITE_WAIT` is dropped; one for another
/// group or profile is kept for its own reply.
fn take_invite_wait(slot: &mut Option<InviteWait>, profile: &str, group: &str, now: Instant) -> Option<Vec<String>> {
    if slot.as_ref().is_some_and(|w| now.saturating_duration_since(w.since) >= INVITE_WAIT) {
        *slot = None;
    }
    if slot.as_ref().is_some_and(|w| w.profile == profile && w.group == group) {
        return slot.take().map(|w| w.members);
    }
    None
}

fn invite_sent_text(count: usize) -> String {
    match count {
        1 => "Invite link sent to 1 person".to_string(),
        n => format!("Invite link sent to {n} people"),
    }
}

#[derive(Default)]
struct AdminState {
    failures: Option<Failures>,
    /// Members waiting for the invite link to arrive.
    invite_for: Option<InviteWait>,
    /// The link shown in the sub-view, for "Copy link".
    link: String,
    /// The photo file dialog is open.
    photo_busy: bool,
}

thread_local! {
    static ADMIN: RefCell<AdminState> = RefCell::default();
}

/// The trimmed description (`None` clears it), or why it cannot be used.
fn validate_description(raw: &str) -> Result<Option<String>, &'static str> {
    let text = raw.trim();
    if text.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err("The description can have at most 2048 characters");
    }
    Ok((!text.is_empty()).then(|| text.to_string()))
}

fn invite_text(link: &str) -> String {
    format!("Follow this link to join my WhatsApp group: {link}")
}

fn setting_of(kind: i32, on: bool) -> Option<GroupSetting> {
    match kind {
        0 => Some(GroupSetting::AnnounceOnly(on)),
        1 => Some(GroupSetting::Locked(on)),
        2 => Some(GroupSetting::ApprovalRequired(on)),
        _ => None,
    }
}

fn failures_title(action: MemberAction) -> &'static str {
    match action {
        MemberAction::Add => "Couldn't add some members",
        MemberAction::Remove => "Couldn't remove some members",
        MemberAction::Promote => "Couldn't make some members admins",
        MemberAction::Demote => "Couldn't dismiss some admins",
    }
}

fn default_failure(action: MemberAction) -> &'static str {
    match action {
        MemberAction::Add => "Couldn't be added",
        MemberAction::Remove => "Couldn't be removed",
        MemberAction::Promote => "Couldn't be made an admin",
        MemberAction::Demote => "Couldn't be dismissed as admin",
    }
}

/// The text after the dash of a failure line.
fn failure_text(action: MemberAction, r: &MemberChangeResult) -> String {
    r.error.as_deref().map(str::trim).filter(|e| !e.is_empty()).unwrap_or_else(|| default_failure(action)).to_string()
}

/// Members the invite link can be sent to instead, without repeats.
fn invite_targets(results: &[MemberChangeResult]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in results.iter().filter(|r| !r.ok && r.invite_instead) {
        if !out.contains(&r.jid) && out.len() < MAX_ADD_MEMBERS {
            out.push(r.jid.clone());
        }
    }
    out
}

pub(super) fn wire(win: &MainWindow) {
    let g = win.global::<GroupAdmin>();
    g.on_save_name(|name| with_app(|a| a.group_save_name(name.as_str())));
    g.on_save_description(|text| with_app(|a| a.group_save_description(text.as_str())));
    g.on_change_photo(|| with_app(App::group_change_photo));
    g.on_remove_photo(|| with_app(App::group_remove_photo));
    g.on_add_members(|| with_app(App::group_add_members));
    g.on_member_role(|jid, kind| with_app(|a| a.group_member_role(jid.as_str(), kind)));
    g.on_remove_member(|jid| with_app(|a| a.group_remove_member(jid.as_str())));
    g.on_open_invite(|| with_app(App::group_open_invite));
    g.on_copy_invite(|| with_app(App::group_copy_invite));
    g.on_reset_invite(|| with_app(App::group_reset_invite));
    g.on_set_setting(|kind, on| with_app(|a| a.group_set_setting(kind, on)));
    g.on_leave(|| with_app(App::group_leave));
    g.on_send_invites(|| with_app(App::group_send_invites));
    g.on_close_view(|| with_app(App::group_close_view));
}

impl App {
    /// The open group when the viewer is an admin, for actions only admins have.
    fn admin_group(&self) -> Option<String> {
        let jid = self.info_group_jid()?;
        self.info_controls().is_admin.then_some(jid)
    }

    /// The open group when the viewer may edit its name, description and photo.
    fn editable_group(&self) -> Option<String> {
        let jid = self.info_group_jid()?;
        self.info_controls().can_edit_info.then_some(jid)
    }

    /// Forgets everything of the previous panel: dialogs, sub-views, edits.
    pub(super) fn group_admin_reset(&self) {
        ADMIN.with(|a| {
            let mut a = a.borrow_mut();
            a.failures = None;
            a.invite_for = None;
            a.link.clear();
        });
        let Some(win) = self.win() else { return };
        let g = win.global::<GroupAdmin>();
        g.set_dialog(DIALOG_NONE);
        g.set_view(0);
        g.set_editing_name(false);
        g.set_invite_link(Default::default());
        g.set_invite_loading(false);
    }

    /// Esc on a dialog; with none open, on the inline name editor.
    pub(super) fn group_close_dialog(&self) {
        let Some(win) = self.win() else { return };
        let g = win.global::<GroupAdmin>();
        if g.get_dialog() != DIALOG_NONE {
            g.set_dialog(DIALOG_NONE);
        } else {
            g.set_editing_name(false);
        }
    }

    /// Back from the invite-link or settings sub-view.
    pub(super) fn group_close_view(&self) {
        if let Some(win) = self.win() {
            win.global::<GroupAdmin>().set_view(0);
        }
    }

    fn close_dialog(&self) {
        if let Some(win) = self.win() {
            win.global::<GroupAdmin>().set_dialog(DIALOG_NONE);
        }
    }

    fn group_save_name(&self, raw: &str) {
        let Some(group) = self.editable_group() else { return };
        let name = match validate_subject(raw) {
            Ok(n) => n,
            Err(why) => {
                self.show_toast(why);
                return;
            }
        };
        if let Some(win) = self.win() {
            win.global::<GroupAdmin>().set_editing_name(false);
        }
        if self.info_group_texts().0 == name {
            return;
        }
        log::info!("group: SetGroupSubject for {}", models::redact_jid(&group));
        self.send(Command::SetGroupSubject { group_jid: group, subject: name });
    }

    fn group_save_description(&self, raw: &str) {
        let Some(group) = self.editable_group() else {
            self.close_dialog();
            return;
        };
        let description = match validate_description(raw) {
            Ok(d) => d,
            Err(why) => {
                self.show_toast(why);
                return;
            }
        };
        self.close_dialog();
        if description.as_deref().unwrap_or_default() == self.info_group_texts().1 {
            return;
        }
        log::info!("group: SetGroupDescription for {}", models::redact_jid(&group));
        self.send(Command::SetGroupDescription { group_jid: group, description });
    }

    fn group_change_photo(&self) {
        let Some(group) = self.editable_group() else { return };
        if ADMIN.with(|a| std::mem::replace(&mut a.borrow_mut().photo_busy, true)) {
            return;
        }
        let profile = self.p().id.clone();
        let spawned = std::thread::Builder::new().name("pick-group-photo".into()).spawn(move || {
            let picked = rfd::FileDialog::new()
                .set_title("Choose a group photo")
                .add_filter("Images", &["jpg", "jpeg", "png"])
                .pick_file();
            let queued = slint::invoke_from_event_loop(move || with_app(|a| a.group_photo_picked(&profile, &group, picked)));
            if let Err(e) = queued {
                log::error!("could not deliver the picked group photo to the UI: {e}");
            }
        });
        if let Err(e) = spawned {
            ADMIN.with(|a| a.borrow_mut().photo_busy = false);
            self.show_banner(1, &format!("Could not open the file picker: {e}"));
        }
    }

    fn group_photo_picked(&self, profile: &str, group: &str, picked: Option<PathBuf>) {
        ADMIN.with(|a| a.borrow_mut().photo_busy = false);
        let Some(path) = picked else { return };
        // The profile may have been removed while the dialog was open.
        let Some(p) = self.find(profile) else { return };
        log::info!("group: SetGroupPicture for {}", models::redact_jid(group));
        (p.send)(Command::SetGroupPicture { group_jid: group.to_string(), path: Some(path) });
    }

    fn group_remove_photo(&self) {
        let Some(group) = self.editable_group() else { return };
        log::info!("group: remove picture of {}", models::redact_jid(&group));
        self.send(Command::SetGroupPicture { group_jid: group, path: None });
    }

    fn group_add_members(&self) {
        let Some(group) = self.admin_group() else { return };
        let members = self.info_member_keys();
        self.open_add_members(&group, &members);
    }

    /// The picker's answer: the contacts to add.
    pub(super) fn add_members_picked(&self, group: &str, jids: Vec<String>) {
        if jids.is_empty() {
            return;
        }
        log::info!("group: adding {} member(s) to {}", jids.len(), models::redact_jid(group));
        self.send(Command::UpdateGroupMembers { group_jid: group.to_string(), jids, action: MemberAction::Add });
    }

    fn group_member_role(&self, jid: &str, kind: i32) {
        let Some(group) = self.admin_group() else { return };
        let action = if kind == 0 { MemberAction::Promote } else { MemberAction::Demote };
        self.send(Command::UpdateGroupMembers { group_jid: group, jids: vec![jid.to_string()], action });
    }

    fn group_remove_member(&self, jid: &str) {
        self.close_dialog();
        let Some(group) = self.admin_group() else { return };
        self.send(Command::UpdateGroupMembers { group_jid: group, jids: vec![jid.to_string()], action: MemberAction::Remove });
    }

    fn group_open_invite(&self) {
        let Some(group) = self.admin_group() else { return };
        ADMIN.with(|a| a.borrow_mut().link.clear());
        if let Some(win) = self.win() {
            let g = win.global::<GroupAdmin>();
            g.set_invite_link(Default::default());
            g.set_invite_loading(true);
            g.set_view(1);
        }
        self.send(Command::FetchInviteLink { group_jid: group, reset: false });
    }

    fn group_copy_invite(&self) {
        let link = ADMIN.with(|a| a.borrow().link.clone());
        if !link.is_empty() {
            self.copy_text(&link);
        }
    }

    fn group_reset_invite(&self) {
        self.close_dialog();
        let Some(group) = self.admin_group() else { return };
        ADMIN.with(|a| a.borrow_mut().link.clear());
        if let Some(win) = self.win() {
            let g = win.global::<GroupAdmin>();
            g.set_invite_link(Default::default());
            g.set_invite_loading(true);
        }
        self.send(Command::FetchInviteLink { group_jid: group, reset: true });
    }

    fn group_set_setting(&self, kind: i32, on: bool) {
        let Some(group) = self.admin_group() else { return };
        let Some(setting) = setting_of(kind, on) else { return };
        log::info!("group: SetGroupSetting for {}", models::redact_jid(&group));
        self.send(Command::SetGroupSetting { group_jid: group, setting });
    }

    fn group_leave(&self) {
        self.close_dialog();
        let Some(group) = self.info_group_jid() else { return };
        if !self.info_controls().can_leave {
            return;
        }
        log::info!("group: LeaveGroup {}", models::redact_jid(&group));
        self.send(Command::LeaveGroup { group_jid: group });
    }

    /// "Send invite link" of the failure dialog: fetches the link; the members
    /// get it in a private chat once it arrives (see `on_invite_link`).
    fn group_send_invites(&self) {
        self.close_dialog();
        let p = self.p();
        let Some(f) = ADMIN.with(|a| a.borrow_mut().failures.take()) else { return };
        if f.profile != p.id || f.invite.is_empty() {
            return;
        }
        let wait = InviteWait { profile: f.profile, group: f.group.clone(), members: f.invite, since: Instant::now() };
        ADMIN.with(|a| a.borrow_mut().invite_for = Some(wait));
        self.show_toast("Sending the invite link...");
        self.send(Command::FetchInviteLink { group_jid: f.group, reset: false });
    }

    /// Name to show for a member in the failure dialog.
    fn member_label(&self, p: &ProfileState, jid: &str) -> String {
        if let Some(name) = self.info_member_name(jid).filter(|n| !n.is_empty()) {
            return name;
        }
        let chat = p.store.chat(jid).ok().flatten().map(|c| c.display_name).filter(|n| !n.trim().is_empty());
        chat.unwrap_or_else(|| {
            let user = jid.split(['@', ':']).next().unwrap_or(jid);
            if user.bytes().all(|b| b.is_ascii_digit()) { format_phone(user) } else { user.to_string() }
        })
    }

    /// `UiEvent::GroupMembersResult`: nothing to show when every change worked
    /// (the panel is refreshed by the engine); otherwise a list of who failed.
    pub(super) fn on_group_members_result(&self, p: &ProfileState, active: bool, group: &str, action: MemberAction, results: &[MemberChangeResult]) {
        if !active || results.iter().all(|r| r.ok) {
            return;
        }
        let rows: Vec<MemberFailure> = results
            .iter()
            .filter(|r| !r.ok)
            .map(|r| MemberFailure { name: self.member_label(p, &r.jid).into(), error: failure_text(action, r).into() })
            .collect();
        let invite = invite_targets(results);
        let offer = !invite.is_empty();
        ADMIN.with(|a| {
            a.borrow_mut().failures = offer.then(|| Failures { profile: p.id.clone(), group: group.to_string(), invite });
        });
        let Some(win) = self.win() else { return };
        let g = win.global::<GroupAdmin>();
        g.set_failures_title(failures_title(action).into());
        g.set_failures(ModelRc::new(VecModel::from(rows)));
        g.set_failures_invite(offer);
        g.set_dialog(DIALOG_FAILURES);
    }

    /// `UiEvent::Error`: a send-invites request of this profile that was
    /// waiting for the link will not get it.
    pub(super) fn invite_request_failed(&self, p: &ProfileState) {
        ADMIN.with(|a| {
            let mut a = a.borrow_mut();
            if a.invite_for.as_ref().is_some_and(|w| w.profile == p.id) {
                a.invite_for = None;
            }
        });
    }

    /// `UiEvent::InviteLink`: shown in the sub-view of the open panel, and sent
    /// to the members that were waiting for it.
    pub(super) fn on_invite_link(&self, p: &ProfileState, active: bool, group: &str, link: &str) {
        let waiting = ADMIN.with(|a| take_invite_wait(&mut a.borrow_mut().invite_for, &p.id, group, Instant::now()));
        if let Some(jids) = waiting {
            let count = jids.len();
            for chat_jid in jids {
                (p.send)(Command::SendText { chat_jid, text: invite_text(link), quoted_id: None, mentions: Vec::new() });
            }
            if active {
                self.show_toast(&invite_sent_text(count));
            }
        }
        if !active || !self.info_is_open_for(&p.id, group) {
            return;
        }
        ADMIN.with(|a| a.borrow_mut().link = link.to_string());
        if let Some(win) = self.win() {
            let g = win.global::<GroupAdmin>();
            g.set_invite_link(link.into());
            g.set_invite_loading(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(jid: &str, ok: bool, error: Option<&str>, invite: bool) -> MemberChangeResult {
        MemberChangeResult { jid: jid.into(), ok, error: error.map(String::from), invite_instead: invite }
    }

    fn waiting(at: Instant) -> Option<InviteWait> {
        Some(InviteWait { profile: "p1".into(), group: "g@g.us".into(), members: vec!["a@s.whatsapp.net".into()], since: at })
    }

    #[test]
    fn the_link_reaches_only_the_request_waiting_for_it() {
        let t0 = Instant::now();
        let mut slot = waiting(t0);
        assert_eq!(take_invite_wait(&mut slot, "p2", "g@g.us", t0), None);
        assert_eq!(take_invite_wait(&mut slot, "p1", "other@g.us", t0), None);
        assert!(slot.is_some());
        assert_eq!(take_invite_wait(&mut slot, "p1", "g@g.us", t0 + Duration::from_secs(5)), Some(vec!["a@s.whatsapp.net".to_string()]));
        assert!(slot.is_none());
        assert_eq!(take_invite_wait(&mut slot, "p1", "g@g.us", t0), None);
    }

    #[test]
    fn a_request_expires_after_thirty_seconds() {
        let t0 = Instant::now();
        let mut slot = waiting(t0);
        assert!(take_invite_wait(&mut slot, "p1", "g@g.us", t0 + Duration::from_secs(29)).is_some());
        let mut slot = waiting(t0);
        assert_eq!(take_invite_wait(&mut slot, "p1", "g@g.us", t0 + INVITE_WAIT), None);
        assert!(slot.is_none());
    }

    #[test]
    fn at_most_twenty_members_get_the_link() {
        let results: Vec<MemberChangeResult> = (0..30).map(|i| result(&format!("{i}@s.whatsapp.net"), false, None, true)).collect();
        assert_eq!(invite_targets(&results).len(), 20);
        assert_eq!(MAX_ADD_MEMBERS, 20);
    }

    #[test]
    fn the_toast_counts_the_people() {
        assert_eq!(invite_sent_text(1), "Invite link sent to 1 person");
        assert_eq!(invite_sent_text(7), "Invite link sent to 7 people");
    }

    #[test]
    fn empty_description_clears_it() {
        assert_eq!(validate_description("  \n "), Ok(None));
        assert_eq!(validate_description(" Lunch at noon \n").unwrap_or_default().as_deref(), Some("Lunch at noon"));
        assert!(validate_description(&"x".repeat(2048)).is_ok());
        assert!(validate_description(&"x".repeat(2049)).is_err());
    }

    #[test]
    fn invite_message_carries_the_link() {
        assert_eq!(invite_text("https://chat.whatsapp.com/abc"), "Follow this link to join my WhatsApp group: https://chat.whatsapp.com/abc");
    }

    #[test]
    fn settings_map_to_their_engine_values() {
        assert_eq!(setting_of(0, true), Some(GroupSetting::AnnounceOnly(true)));
        assert_eq!(setting_of(1, false), Some(GroupSetting::Locked(false)));
        assert_eq!(setting_of(2, true), Some(GroupSetting::ApprovalRequired(true)));
        assert_eq!(setting_of(3, true), None);
    }

    #[test]
    fn failure_lines_prefer_the_engines_reason() {
        let with = result("a@s.whatsapp.net", false, Some(" Already in the group "), false);
        let without = result("b@s.whatsapp.net", false, None, false);
        assert_eq!(failure_text(MemberAction::Add, &with), "Already in the group");
        assert_eq!(failure_text(MemberAction::Add, &without), "Couldn't be added");
        assert_eq!(failure_text(MemberAction::Remove, &without), "Couldn't be removed");
    }

    #[test]
    fn only_refused_adds_that_allow_it_get_the_link() {
        let results = [
            result("a@s.whatsapp.net", false, None, true),
            result("b@s.whatsapp.net", true, None, true),
            result("c@s.whatsapp.net", false, None, false),
            result("a@s.whatsapp.net", false, None, true),
            result("d@s.whatsapp.net", false, None, true),
        ];
        assert_eq!(invite_targets(&results), ["a@s.whatsapp.net", "d@s.whatsapp.net"]);
        assert!(invite_targets(&[]).is_empty());
    }
}
