//! "Saved Messages": the chat with our own number, pinned at the top of the
//! chat list, plus "Save to Saved Messages" in the message menu.

use slint::ComponentHandle;
use wa_engine::Command;
use wa_store::{ChatKind, ChatRow};

use super::{App, ProfileState, with_app};
use crate::models;
use crate::{ChatItem, MainWindow, SavedMessages};

pub const SAVED_TITLE: &str = "Saved Messages";
const SAVED_EMPTY_PREVIEW: &str = "Message yourself";

/// The chat with our own account: `user[:device][_agent]@server` becomes
/// `user@server`. `None` when the JID is not a plain account JID.
pub fn saved_messages_jid(own_jid: &str) -> Option<String> {
    let (local, server) = own_jid.split_once('@')?;
    let user = local.split([':', '_']).next().unwrap_or("");
    let plain = !user.is_empty() && !server.is_empty() && !server.contains('@') && server != "g.us";
    plain.then(|| format!("{user}@{server}"))
}

pub(super) fn wire(win: &MainWindow) {
    win.global::<SavedMessages>().on_save(|id| with_app(|a| a.saved_save(id.as_str())));
}

impl App {
    /// The Saved Messages chat of a profile, once its own number is known.
    fn saved_jid(&self, p: &ProfileState) -> Option<String> {
        p.meta.borrow().own_jid.as_deref().and_then(saved_messages_jid)
    }

    /// Tells the window which chat is Saved Messages and which chat is open.
    pub(super) fn saved_sync(&self) {
        let Some(win) = self.win() else { return };
        let p = self.p();
        let g = win.global::<SavedMessages>();
        g.set_jid(self.saved_jid(&p).unwrap_or_default().into());
        g.set_open_chat(p.st.borrow().selected.clone().unwrap_or_default().into());
    }

    /// The header title: our own chat is never labelled with our own name.
    pub(super) fn header_title(&self, p: &ProfileState, row: &ChatRow) -> String {
        if self.saved_jid(p).as_deref() == Some(row.jid.as_str()) {
            SAVED_TITLE.to_string()
        } else {
            row.display_name.clone()
        }
    }

    /// Puts the pinned row first in the plain chat list (no search, not the archive)
    /// and renames our own chat everywhere else. Works with or without a stored
    /// chat row: the row is created when the chat is opened.
    pub(super) fn saved_apply(&self, p: &ProfileState, full: &[ChatRow], items: &mut Vec<ChatItem>, plain: bool) {
        self.saved_sync();
        let Some(jid) = self.saved_jid(p) else { return };
        if !p.settings.borrow().show_saved_messages {
            return;
        }
        for item in items.iter_mut().filter(|i| i.jid.as_str() == jid) {
            item.name = SAVED_TITLE.into();
        }
        if !plain {
            return;
        }
        items.retain(|i| i.jid.as_str() != jid);
        let row = full.iter().find(|r| r.jid == jid).cloned().unwrap_or_else(|| ChatRow {
            jid: jid.clone(),
            kind: ChatKind::Dm,
            display_name: SAVED_TITLE.to_string(),
            last_ts: 0,
            unread: 0,
            pinned: false,
            archived: false,
            muted_until: 0,
            last_preview: None,
            receipts_off: false,
            ephemeral_secs: 0,
            ephemeral_ts: 0,
        });
        let avatar = None;
        let mut item = models::chat_item(&row, avatar, chrono::Local::now());
        item.name = SAVED_TITLE.into();
        item.saved = true;
        item.archived = false;
        if row.last_preview.as_deref().is_none_or(str::is_empty) {
            item.preview = SAVED_EMPTY_PREVIEW.into();
        }
        if let Some(draft) = self.draft_preview(&jid) {
            item.preview = draft.into();
            item.draft = true;
        }
        items.insert(0, item);
    }

    /// Creates the local chat row of Saved Messages when it is opened for the first time.
    pub(super) fn saved_ensure_chat(&self, jid: &str) {
        let p = self.p();
        if self.saved_jid(&p).as_deref() != Some(jid) {
            return;
        }
        match p.store.chat(jid) {
            Ok(Some(_)) => {}
            Ok(None) => {
                let created = p.store.upsert_chat(jid, ChatKind::Dm, Some(SAVED_TITLE));
                self.ok("create Saved Messages", created);
            }
            Err(e) => log::error!("chat lookup failed: {e}"),
        }
    }

    /// "Save to Saved Messages": forwards the message to our own chat.
    fn saved_save(&self, id: &str) {
        let p = self.p();
        let Some(to) = self.saved_jid(&p) else {
            self.show_toast("Your own number is not known yet");
            return;
        };
        let Some(from) = p.st.borrow().selected.clone() else { return };
        if from == to {
            return;
        }
        self.saved_ensure_chat(&to);
        self.send(Command::ForwardMessages { from_chat: from, ids: vec![id.to_string()], to_chats: vec![to] });
        self.show_toast("Saved to Saved Messages");
    }

    /// Debug aid for the demo: gives the profile a number, saves the first
    /// message of the first chat into Saved Messages and opens it.
    #[cfg(debug_assertions)]
    pub(super) fn demo_saved(&self) {
        let p = self.p();
        if p.meta.borrow().own_jid.is_none() {
            p.meta.borrow_mut().own_jid = Some("999:3@s.whatsapp.net".to_string());
        }
        self.refresh_chats();
        self.select_chat("111@s.whatsapp.net");
        let first = p.st.borrow().msgs.first().map(|m| m.id.clone());
        if let Some(id) = first {
            self.saved_save(&id);
        }
        if let Some(jid) = self.saved_jid(&p) {
            self.select_chat(&jid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_device_part_is_dropped() {
        assert_eq!(saved_messages_jid("123456:7@s.whatsapp.net").as_deref(), Some("123456@s.whatsapp.net"));
        assert_eq!(saved_messages_jid("123456@s.whatsapp.net").as_deref(), Some("123456@s.whatsapp.net"));
        assert_eq!(saved_messages_jid("98765_1:2@lid").as_deref(), Some("98765@lid"));
    }

    #[test]
    fn jids_that_are_not_an_account_are_rejected() {
        assert_eq!(saved_messages_jid(""), None);
        assert_eq!(saved_messages_jid("nonsense"), None);
        assert_eq!(saved_messages_jid("@s.whatsapp.net"), None);
        assert_eq!(saved_messages_jid("123@"), None);
        assert_eq!(saved_messages_jid("123-456@g.us"), None);
    }
}
