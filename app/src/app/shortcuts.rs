//! Telegram-style keyboard shortcuts: Up edits the last own message, Ctrl+Tab
//! and Alt+Up/Down move between chats. The key handling lives in the Slint
//! files; this module holds the decisions and the controller entry points.

use slint::{ComponentHandle, Model};
use wa_store::MessageRow;

use super::App;
use crate::models;
use crate::{FolderState, GroupAdmin, GroupNew, LocationState, MsgOps, PollState, ShareState, StatusPost, TimerState};

/// The position after (`forward`) or before `cur` in a list of `len`, wrapping.
/// Without a current position forward starts at the first, backward at the last.
pub fn step_index(len: usize, cur: Option<usize>, forward: bool) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(match (cur.filter(|&c| c < len), forward) {
        (Some(c), true) => (c + 1) % len,
        (Some(c), false) => (c + len - 1) % len,
        (None, true) => 0,
        (None, false) => len - 1,
    })
}

/// The next chat with unread messages after (`forward`) or before `cur`,
/// wrapping. The current chat itself is only returned when it is the only one.
pub fn step_unread(unread: &[i64], cur: Option<usize>, forward: bool) -> Option<usize> {
    let len = unread.len();
    let mut i = cur.filter(|&c| c < len);
    for _ in 0..len {
        i = step_index(len, i, forward);
        let idx = i?;
        if unread[idx] > 0 && Some(idx) != cur {
            return Some(idx);
        }
    }
    cur.filter(|&c| unread.get(c).is_some_and(|&u| u > 0))
}

/// The newest message of the open chat that can still be edited.
pub fn last_editable(msgs: &[MessageRow], now: i64) -> Option<&MessageRow> {
    msgs.iter().filter(|m| models::can_edit(m, now)).max_by_key(|m| m.ts)
}

impl App {
    /// A dialog, picker or full-screen view owns the keyboard.
    fn shortcuts_blocked(&self) -> bool {
        let Some(win) = self.win() else { return true };
        let ops = win.global::<MsgOps>();
        let post = win.global::<StatusPost>();
        let group = win.global::<GroupAdmin>();
        self.is_locked()
            || win.get_show_settings()
            || win.get_attach_open()
            || win.get_viewer_open()
            || win.global::<FolderState>().get_dialog() != 0
            || group.get_dialog() != 0
            || group.get_editing_name()
            || win.global::<GroupNew>().get_dialog() != 0
            || win.global::<ShareState>().get_open()
            || win.global::<TimerState>().get_dialog_open()
            || ops.get_forward_open()
            || !ops.get_pin_ask_id().is_empty()
            || post.get_open()
            || win.global::<LocationState>().get_open()
            || win.global::<PollState>().get_open()
    }

    /// Ctrl+Tab / Ctrl+Shift+Tab: the next or previous chat of the visible list.
    pub(super) fn step_chat(&self, forward: bool) {
        self.step_to(forward, false);
    }

    /// Alt+Down / Alt+Up: the next or previous chat with unread messages.
    pub(super) fn step_unread_chat(&self, forward: bool) {
        self.step_to(forward, true);
    }

    fn step_to(&self, forward: bool, unread_only: bool) {
        if self.shortcuts_blocked() {
            return;
        }
        let selected = self.p().st.borrow().selected.clone();
        let rows: Vec<_> = self.chats_model.iter().collect();
        let cur = selected.as_deref().and_then(|s| rows.iter().position(|r| r.jid.as_str() == s));
        let target = if unread_only {
            let unread: Vec<i64> = rows.iter().map(|r| i64::from(r.unread)).collect();
            step_unread(&unread, cur, forward)
        } else {
            step_index(rows.len(), cur, forward)
        };
        if let Some(row) = target.and_then(|i| rows.get(i)) {
            self.select_chat(row.jid.as_str());
        }
    }

    /// Up in an empty composer: edit the newest editable own message.
    pub(super) fn edit_last_message(&self) {
        if self.shortcuts_blocked() {
            return;
        }
        let Some(win) = self.win() else { return };
        if win.global::<MsgOps>().get_editing() || win.global::<MsgOps>().get_selecting() {
            return;
        }
        let now = chrono::Utc::now().timestamp();
        let id = {
            let p = self.p();
            let st = p.st.borrow();
            last_editable(&st.msgs, now).map(|m| m.id.clone())
        };
        if let Some(id) = id {
            self.begin_edit_message(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wa_store::{MessageKind, MessageStatus};

    #[test]
    fn step_wraps_both_ways() {
        assert_eq!(step_index(3, Some(2), true), Some(0));
        assert_eq!(step_index(3, Some(0), false), Some(2));
        assert_eq!(step_index(3, Some(1), true), Some(2));
        assert_eq!(step_index(3, Some(1), false), Some(0));
    }

    #[test]
    fn step_without_current_opens_first_or_last() {
        assert_eq!(step_index(3, None, true), Some(0));
        assert_eq!(step_index(3, None, false), Some(2));
        assert_eq!(step_index(0, None, true), None);
        assert_eq!(step_index(2, Some(9), true), Some(0));
    }

    #[test]
    fn unread_skips_read_chats_and_wraps() {
        let u = [0, 2, 0, 0, 5];
        assert_eq!(step_unread(&u, Some(1), true), Some(4));
        assert_eq!(step_unread(&u, Some(4), true), Some(1));
        assert_eq!(step_unread(&u, Some(1), false), Some(4));
        assert_eq!(step_unread(&u, Some(4), false), Some(1));
        assert_eq!(step_unread(&u, Some(2), true), Some(4));
        assert_eq!(step_unread(&u, None, true), Some(1));
        assert_eq!(step_unread(&u, None, false), Some(4));
    }

    #[test]
    fn unread_none_or_only_current() {
        assert_eq!(step_unread(&[0, 0, 0], Some(1), true), None);
        assert_eq!(step_unread(&[], None, true), None);
        assert_eq!(step_unread(&[0, 3, 0], Some(1), true), Some(1));
        assert_eq!(step_unread(&[0, 3, 0], Some(0), true), Some(1));
    }

    fn text(id: &str, from_me: bool, ts: i64) -> MessageRow {
        MessageRow {
            id: id.into(),
            from_me,
            ts,
            kind: MessageKind::Text,
            text: Some("hi".into()),
            status: MessageStatus::Sent,
            ..Default::default()
        }
    }

    #[test]
    fn last_editable_picks_newest_own_text() {
        let now = 10_000;
        let msgs = vec![text("a", true, now - 60), text("b", true, now - 30), text("c", false, now - 10)];
        assert_eq!(last_editable(&msgs, now).map(|m| m.id.as_str()), Some("b"));
    }

    #[test]
    fn last_editable_skips_old_revoked_and_media() {
        let now = 100_000;
        let mut revoked = text("r", true, now - 5);
        revoked.revoked = true;
        let mut media = text("m", true, now - 4);
        media.kind = MessageKind::Image;
        let msgs = vec![text("old", true, now - 3600), revoked, media];
        assert!(last_editable(&msgs, now).is_none());
        assert!(last_editable(&[], now).is_none());
    }
}
