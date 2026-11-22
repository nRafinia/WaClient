//! The Esc key: one press closes one layer, topmost first. The decision is
//! pure; `App::escape` only carries it out.

use slint::ComponentHandle;

use super::App;
use crate::{FolderState, GroupAdmin, GroupNew, LocationState, MsgOps, PollState, ShareState, StatusPost, TimerState};

/// What is open right now, as far as the controller can tell. Popup menus and
/// the emoji picker are not listed: they own the keyboard while open and
/// close themselves, so the window never sees their Esc.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EscState {
    /// The lock screen is up: no layer below it may receive Esc.
    pub locked: bool,
    /// The folder form or its delete confirmation: modal.
    pub folder_dialog: bool,
    /// A group-admin dialog or the inline name editor: topmost.
    pub group_dialog: bool,
    /// A dialog of "new group" / "join group by link".
    pub group_new: bool,
    /// The share-contact picker or the disappearing-timer dialog: modal.
    pub share_open: bool,
    pub timer_dialog: bool,
    /// The forward picker or the pin-duration dialog: modal, so topmost.
    pub ops_dialog: bool,
    /// The audience chooser of the status editor (on top of the editor).
    pub audience_picker: bool,
    /// The status editor.
    pub status_post: bool,
    /// The send-location dialog.
    pub location: bool,
    /// The create-poll dialog.
    pub poll_create: bool,
    /// The @mention suggestions of the composer: the topmost layer.
    pub mention_open: bool,
    pub attach_open: bool,
    /// Files are being sent: the preview cannot be dismissed.
    pub attach_busy: bool,
    pub viewer_open: bool,
    /// An incoming-call banner is showing (the topmost one is dismissed).
    pub call_banner: bool,
    pub search_open: bool,
    /// Invite-link or group-settings sub-view of the info panel.
    pub group_view: bool,
    pub info_open: bool,
    /// The message-info or starred-messages panel.
    pub ops_panel: bool,
    /// The composer is editing a sent message.
    pub editing: bool,
    /// Selection mode (checkboxes and the bottom bar).
    pub selecting: bool,
    pub has_reply: bool,
    pub has_chat: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscAction {
    /// Nothing to close.
    None,
    /// Swallow the key without closing anything.
    Ignore,
    CloseFolderDialog,
    CloseGroupDialog,
    CloseGroupNew,
    CloseShare,
    CloseTimerDialog,
    CloseOpsDialog,
    CloseAudiencePicker,
    CloseStatusPost,
    CloseLocation,
    ClosePollCreate,
    CloseMention,
    CloseAttach,
    CloseViewer,
    DismissCall,
    CloseSearch,
    CloseGroupView,
    CloseInfo,
    CloseOpsPanel,
    CancelEdit,
    LeaveSelection,
    CancelReply,
    CloseChat,
}

pub fn esc_action(s: &EscState) -> EscAction {
    if s.locked {
        EscAction::Ignore
    } else if s.folder_dialog {
        EscAction::CloseFolderDialog
    } else if s.group_dialog {
        EscAction::CloseGroupDialog
    } else if s.group_new {
        EscAction::CloseGroupNew
    } else if s.share_open {
        EscAction::CloseShare
    } else if s.timer_dialog {
        EscAction::CloseTimerDialog
    } else if s.ops_dialog {
        EscAction::CloseOpsDialog
    } else if s.audience_picker {
        EscAction::CloseAudiencePicker
    } else if s.status_post {
        EscAction::CloseStatusPost
    } else if s.location {
        EscAction::CloseLocation
    } else if s.poll_create {
        EscAction::ClosePollCreate
    } else if s.mention_open {
        EscAction::CloseMention
    } else if s.attach_open {
        if s.attach_busy { EscAction::Ignore } else { EscAction::CloseAttach }
    } else if s.viewer_open {
        EscAction::CloseViewer
    } else if s.call_banner {
        EscAction::DismissCall
    } else if s.search_open {
        EscAction::CloseSearch
    } else if s.group_view {
        EscAction::CloseGroupView
    } else if s.info_open {
        EscAction::CloseInfo
    } else if s.ops_panel {
        EscAction::CloseOpsPanel
    } else if s.editing {
        EscAction::CancelEdit
    } else if s.selecting {
        EscAction::LeaveSelection
    } else if s.has_reply {
        EscAction::CancelReply
    } else if s.has_chat {
        EscAction::CloseChat
    } else {
        EscAction::None
    }
}

impl App {
    /// Window-level Esc. True when the key was used.
    pub(super) fn escape(&self) -> bool {
        let Some(win) = self.win() else { return false };
        let ops = win.global::<MsgOps>();
        let post = win.global::<StatusPost>();
        let group = win.global::<GroupAdmin>();
        let state = EscState {
            folder_dialog: win.global::<FolderState>().get_dialog() != 0,
            locked: self.is_locked(),
            group_dialog: group.get_dialog() != 0 || group.get_editing_name(),
            group_new: win.global::<GroupNew>().get_dialog() != 0,
            share_open: win.global::<ShareState>().get_open(),
            timer_dialog: win.global::<TimerState>().get_dialog_open(),
            ops_dialog: ops.get_forward_open() || !ops.get_pin_ask_id().is_empty(),
            audience_picker: post.get_open() && post.get_picker_open(),
            status_post: post.get_open(),
            location: win.global::<LocationState>().get_open(),
            poll_create: win.global::<PollState>().get_open(),
            mention_open: super::composer::mention_open(),
            attach_open: win.get_attach_open(),
            attach_busy: win.get_attach_busy(),
            viewer_open: win.get_viewer_open(),
            call_banner: super::call_ui::has_banners(),
            search_open: win.get_search_open(),
            group_view: group.get_view() != 0 && win.get_contact_info_open(),
            info_open: win.get_contact_info_open(),
            ops_panel: ops.get_panel() != 0 && !win.get_contact_info_open(),
            editing: ops.get_editing(),
            selecting: ops.get_selecting(),
            has_reply: win.get_has_reply(),
            has_chat: win.get_has_chat(),
        };
        let action = esc_action(&state);
        match action {
            EscAction::None => return false,
            EscAction::Ignore => {}
            EscAction::CloseFolderDialog => self.folder_cancel(),
            EscAction::CloseGroupDialog => self.group_close_dialog(),
            EscAction::CloseGroupNew => self.group_new_cancel(),
            EscAction::CloseShare => self.share_close(),
            EscAction::CloseTimerDialog => self.timer_close_dialog(),
            EscAction::CloseOpsDialog => self.ops_dialog_close(),
            EscAction::CloseAudiencePicker => self.audience_cancel(),
            EscAction::CloseStatusPost => self.status_post_cancel(),
            EscAction::CloseLocation => self.location_close(),
            EscAction::ClosePollCreate => self.poll_close(),
            EscAction::CloseMention => self.close_mentions(),
            EscAction::CloseAttach => self.attach_cancel(),
            EscAction::CloseViewer => self.status_close(),
            EscAction::DismissCall => self.call_dismiss_top(),
            EscAction::CloseSearch => {
                win.set_search_open(false);
                win.set_search_query(Default::default());
                self.search_close();
                win.invoke_focus_composer();
            }
            EscAction::CloseGroupView => self.group_close_view(),
            EscAction::CloseInfo => {
                self.info_close();
                win.invoke_focus_composer();
            }
            EscAction::CloseOpsPanel => {
                self.ops_panel_close();
                win.invoke_focus_composer();
            }
            EscAction::CancelEdit => self.edit_cancel(),
            EscAction::LeaveSelection => self.select_cancel(),
            EscAction::CancelReply => self.cancel_reply(),
            EscAction::CloseChat => self.close_chat(),
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> EscState {
        EscState {
            locked: false,
            folder_dialog: true,
            group_dialog: true,
            group_new: true,
            group_view: true,
            share_open: true,
            timer_dialog: true,
            ops_dialog: true,
            audience_picker: true,
            status_post: true,
            location: true,
            poll_create: true,
            mention_open: true,
            attach_open: true,
            attach_busy: false,
            viewer_open: true,
            call_banner: true,
            search_open: true,
            info_open: true,
            ops_panel: true,
            editing: true,
            selecting: true,
            has_reply: true,
            has_chat: true,
        }
    }

    #[test]
    fn layers_close_in_priority_order() {
        let mut s = all();
        let order = [
            EscAction::CloseFolderDialog,
            EscAction::CloseGroupDialog,
            EscAction::CloseGroupNew,
            EscAction::CloseShare,
            EscAction::CloseTimerDialog,
            EscAction::CloseOpsDialog,
            EscAction::CloseAudiencePicker,
            EscAction::CloseStatusPost,
            EscAction::CloseLocation,
            EscAction::ClosePollCreate,
            EscAction::CloseMention,
            EscAction::CloseAttach,
            EscAction::CloseViewer,
            EscAction::DismissCall,
            EscAction::CloseSearch,
            EscAction::CloseGroupView,
            EscAction::CloseInfo,
            EscAction::CloseOpsPanel,
            EscAction::CancelEdit,
            EscAction::LeaveSelection,
            EscAction::CancelReply,
            EscAction::CloseChat,
        ];
        for want in order {
            assert_eq!(esc_action(&s), want);
            match want {
                EscAction::CloseFolderDialog => s.folder_dialog = false,
                EscAction::CloseGroupDialog => s.group_dialog = false,
                EscAction::CloseGroupNew => s.group_new = false,
                EscAction::CloseShare => s.share_open = false,
                EscAction::CloseTimerDialog => s.timer_dialog = false,
                EscAction::CloseOpsDialog => s.ops_dialog = false,
                EscAction::CloseAudiencePicker => s.audience_picker = false,
                EscAction::CloseStatusPost => s.status_post = false,
                EscAction::CloseLocation => s.location = false,
                EscAction::ClosePollCreate => s.poll_create = false,
                EscAction::CloseMention => s.mention_open = false,
                EscAction::CloseAttach => s.attach_open = false,
                EscAction::CloseViewer => s.viewer_open = false,
                EscAction::DismissCall => s.call_banner = false,
                EscAction::CloseSearch => s.search_open = false,
                EscAction::CloseGroupView => s.group_view = false,
                EscAction::CloseInfo => s.info_open = false,
                EscAction::CloseOpsPanel => s.ops_panel = false,
                EscAction::CancelEdit => s.editing = false,
                EscAction::LeaveSelection => s.selecting = false,
                EscAction::CancelReply => s.has_reply = false,
                _ => s.has_chat = false,
            }
        }
        assert_eq!(esc_action(&s), EscAction::None);
    }

    #[test]
    fn busy_attach_swallows_esc() {
        let s = EscState { attach_open: true, attach_busy: true, has_chat: true, ..Default::default() };
        assert_eq!(esc_action(&s), EscAction::Ignore);
    }

    #[test]
    fn lock_screen_swallows_esc_before_every_layer() {
        let s = EscState { locked: true, ..all() };
        assert_eq!(esc_action(&s), EscAction::Ignore);
        let s = EscState { locked: true, ..Default::default() };
        assert_eq!(esc_action(&s), EscAction::Ignore);
    }

    #[test]
    fn nothing_open_does_nothing() {
        assert_eq!(esc_action(&EscState::default()), EscAction::None);
    }

    #[test]
    fn group_layers_close_before_the_panel_that_hosts_them() {
        let s = EscState { group_view: true, info_open: true, has_chat: true, ..Default::default() };
        assert_eq!(esc_action(&s), EscAction::CloseGroupView);
        let s = EscState { group_dialog: true, group_view: true, info_open: true, ..Default::default() };
        assert_eq!(esc_action(&s), EscAction::CloseGroupDialog);
    }

    #[test]
    fn call_banner_closes_before_panels_but_after_dialogs() {
        let s = EscState { call_banner: true, info_open: true, has_chat: true, ..Default::default() };
        assert_eq!(esc_action(&s), EscAction::DismissCall);
        let s = EscState { call_banner: true, poll_create: true, ..Default::default() };
        assert_eq!(esc_action(&s), EscAction::ClosePollCreate);
    }

    #[test]
    fn info_panel_closes_before_the_chat() {
        let s = EscState { info_open: true, has_chat: true, ..Default::default() };
        assert_eq!(esc_action(&s), EscAction::CloseInfo);
    }
}
