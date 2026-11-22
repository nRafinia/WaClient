//! Read-only queries behind the "Media, links and docs" view of the info
//! panel. All results are newest first.

use rusqlite::{params, OptionalExtension};

use crate::model::MessageRow;
use crate::queries::{message_from_row, MESSAGE_COLS};
use crate::{Result, Store};

const MEDIA_KINDS: &str = "('image','video')";
const DOC_KINDS: &str = "('document')";
/// Prefilter shared by the list and the count, so the badge matches what the
/// list returns. The app still extracts exact links from the text and may drop
/// a row that has none; `www.` only counts at the start of a word.
const HAS_LINK: &str = "(m.text LIKE '%http://%' OR m.text LIKE '%https://%' \
    OR m.text LIKE '% www.%' OR m.text LIKE 'www.%')";

impl Store {
    /// Images and videos of a chat, newest first, strictly older than the
    /// `before` anchor (`(ts, id)` of a message of this chat).
    pub fn chat_media(
        &self,
        chat_jid: &str,
        before: Option<(i64, String)>,
        limit: u32,
    ) -> Result<Vec<MessageRow>> {
        self.kind_page(chat_jid, MEDIA_KINDS, before, limit)
    }

    /// Documents of a chat, newest first, paged like [`Store::chat_media`].
    pub fn chat_docs(
        &self,
        chat_jid: &str,
        before: Option<(i64, String)>,
        limit: u32,
    ) -> Result<Vec<MessageRow>> {
        self.kind_page(chat_jid, DOC_KINDS, before, limit)
    }

    /// Newest `limit` messages whose text looks like it holds a link.
    pub fn chat_links(&self, chat_jid: &str, limit: u32) -> Result<Vec<MessageRow>> {
        let sql = format!(
            "SELECT {MESSAGE_COLS} FROM messages m \
             LEFT JOIN contacts ct ON ct.jid = m.sender_jid \
             WHERE m.chat_jid = ?1 AND m.revoked = 0 AND {HAS_LINK} \
             ORDER BY m.ts DESC, m.rowid DESC LIMIT ?2"
        );
        let conn = self.lock();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![chat_jid, limit], message_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// `(media, docs, links)`; links counts messages that look like they hold one.
    pub fn chat_media_counts(&self, chat_jid: &str) -> Result<(u32, u32, u32)> {
        let conn = self.lock();
        let count = |filter: &str| -> rusqlite::Result<u32> {
            conn.query_row(
                &format!(
                    "SELECT COUNT(*) FROM messages m WHERE m.chat_jid = ?1 AND m.revoked = 0 AND {filter}"
                ),
                params![chat_jid],
                |r| r.get(0),
            )
        };
        Ok((
            count(&format!("m.kind IN {MEDIA_KINDS}"))?,
            count(&format!("m.kind IN {DOC_KINDS}"))?,
            count(HAS_LINK)?,
        ))
    }

    fn kind_page(
        &self,
        chat_jid: &str,
        kinds: &str,
        before: Option<(i64, String)>,
        limit: u32,
    ) -> Result<Vec<MessageRow>> {
        let conn = self.lock();
        let (anchor_ts, anchor_rowid) = match &before {
            None => (None, None),
            Some((ts, id)) => {
                let rowid: Option<i64> = conn
                    .query_row(
                        "SELECT rowid FROM messages WHERE chat_jid = ?1 AND id = ?2",
                        params![chat_jid, id],
                        |r| r.get(0),
                    )
                    .optional()?;
                // Without a row, `i64::MAX` keeps every message sharing the
                // anchor's timestamp: one already shown may repeat (the app
                // dedups by id) but none is skipped.
                (Some(*ts), Some(rowid.unwrap_or(i64::MAX)))
            }
        };
        let sql = format!(
            "SELECT {MESSAGE_COLS} FROM messages m \
             LEFT JOIN contacts ct ON ct.jid = m.sender_jid \
             WHERE m.chat_jid = ?1 AND m.revoked = 0 AND m.kind IN {kinds} \
             AND (?2 IS NULL OR m.ts < ?2 OR (m.ts = ?2 AND m.rowid < ?3)) \
             ORDER BY m.ts DESC, m.rowid DESC LIMIT ?4"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![chat_jid, anchor_ts, anchor_rowid, limit], message_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use crate::{MessageKind, NewMessage, Store};

    const CHAT: &str = "111@s.whatsapp.net";

    fn msg(chat: &str, id: &str, ts: i64, kind: MessageKind, text: Option<&str>) -> NewMessage {
        NewMessage {
            chat_jid: chat.into(),
            id: id.into(),
            sender_jid: chat.into(),
            from_me: false,
            ts,
            kind,
            text: text.map(String::from),
            quoted_id: None,
            quoted_preview: None,
            media_mime: None,
            media_name: None,
            media_size: None,
            media_duration: None,
            thumbnail: None,
            local_path: None,
            raw: None,
            ..Default::default()
        }
    }

    fn store() -> Store {
        let s = Store::open_in_memory().expect("store");
        let rows = [
            msg(CHAT, "t1", 10, MessageKind::Text, Some("hello")),
            msg(CHAT, "i1", 20, MessageKind::Image, None),
            msg(CHAT, "v1", 30, MessageKind::Video, None),
            msg(CHAT, "i2", 30, MessageKind::Image, Some("caption")),
            msg(CHAT, "s1", 35, MessageKind::Sticker, None),
            msg(CHAT, "d1", 40, MessageKind::Document, None),
            msg(CHAT, "a1", 45, MessageKind::Voice, None),
            msg(CHAT, "l1", 50, MessageKind::Text, Some("see https://example.com/x")),
            msg(CHAT, "l2", 60, MessageKind::Text, Some("WWW.example.org here")),
            msg(CHAT, "l3", 70, MessageKind::Text, Some("http://a.io")),
            msg("222@s.whatsapp.net", "o1", 80, MessageKind::Image, None),
        ];
        for m in &rows {
            assert!(s.insert_message(m, false).expect("insert"));
        }
        s
    }

    fn ids(rows: &[crate::MessageRow]) -> Vec<&str> {
        rows.iter().map(|r| r.id.as_str()).collect()
    }

    #[test]
    fn media_is_images_and_videos_newest_first() {
        let s = store();
        let rows = s.chat_media(CHAT, None, 10).expect("media");
        assert_eq!(ids(&rows), ["i2", "v1", "i1"]);
    }

    #[test]
    fn media_pages_without_skipping_equal_timestamps() {
        let s = store();
        let first = s.chat_media(CHAT, None, 1).expect("page 1");
        assert_eq!(ids(&first), ["i2"]);
        let anchor = Some((first[0].ts, first[0].id.clone()));
        let second = s.chat_media(CHAT, anchor, 1).expect("page 2");
        assert_eq!(ids(&second), ["v1"]);
        let anchor = Some((second[0].ts, second[0].id.clone()));
        let third = s.chat_media(CHAT, anchor, 5).expect("page 3");
        assert_eq!(ids(&third), ["i1"]);
    }

    #[test]
    fn revoked_messages_are_left_out() {
        let s = store();
        s.revoke_message(CHAT, "v1").expect("revoke");
        let rows = s.chat_media(CHAT, None, 10).expect("media");
        assert_eq!(ids(&rows), ["i2", "i1"]);
        assert_eq!(s.chat_media_counts(CHAT).expect("counts").0, 2);
    }

    #[test]
    fn docs_are_documents_only() {
        let s = store();
        let rows = s.chat_docs(CHAT, None, 10).expect("docs");
        assert_eq!(ids(&rows), ["d1"]);
    }

    #[test]
    fn links_match_scheme_or_www_case_insensitively() {
        let s = store();
        let rows = s.chat_links(CHAT, 10).expect("links");
        assert_eq!(ids(&rows), ["l3", "l2", "l1"]);
        assert_eq!(ids(&s.chat_links(CHAT, 2).expect("limited")), ["l3", "l2"]);
    }

    #[test]
    fn www_must_start_a_word() {
        let s = Store::open_in_memory().expect("store");
        for (id, ts, t) in [("a", 1, "xwww.foo"), ("b", 2, "go www.foo"), ("c", 3, "www.foo")] {
            let m = msg(CHAT, id, ts, MessageKind::Text, Some(t));
            assert!(s.insert_message(&m, false).expect("insert"));
        }
        assert_eq!(ids(&s.chat_links(CHAT, 10).expect("links")), ["c", "b"]);
        assert_eq!(s.chat_media_counts(CHAT).expect("counts").2, 2);
    }

    #[test]
    fn missing_anchor_keeps_equal_timestamps() {
        let s = store();
        let page = s.chat_media(CHAT, Some((30, "gone".into())), 10).expect("page");
        assert_eq!(ids(&page), ["i2", "v1", "i1"]);
    }

    #[test]
    fn counts_are_per_chat() {
        let s = store();
        assert_eq!(s.chat_media_counts(CHAT).expect("counts"), (3, 1, 3));
        assert_eq!(s.chat_media_counts("222@s.whatsapp.net").expect("counts"), (1, 0, 0));
        assert_eq!(s.chat_media_counts("nobody@s.whatsapp.net").expect("counts"), (0, 0, 0));
    }
}
