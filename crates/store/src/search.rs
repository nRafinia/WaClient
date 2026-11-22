//! In-chat message search and "jump to result" windows.
//!
//! Matching is a case-insensitive substring match over the message text
//! (which also holds media captions) and the media file name.
//!
//! SQLite's `lower()` and `LIKE` only fold ASCII. So an ASCII-only query is
//! answered entirely in SQL (`LIKE ... ESCAPE`), while a query containing any
//! non-ASCII character (Persian, Arabic, Cyrillic, ...) is matched in Rust by
//! scanning the chat's non-revoked messages newest first (text columns only,
//! in batches, the store lock released between them) with a Unicode lowercase
//! fold, stopping at `limit`; the matching rows are then fetched by id.

use std::collections::HashMap;

use rusqlite::{params, OptionalExtension};

use crate::model::MessageRow;
use crate::queries::{message_from_row, MESSAGE_COLS};
use crate::{Result, Store};

/// Longer queries are truncated to this many characters.
const MAX_QUERY_CHARS: usize = 200;
const SCAN_BATCH: u32 = 2_000;
const ID_CHUNK: usize = 500;

fn like_pattern(query: &str) -> String {
    let mut out = String::with_capacity(query.len() + 2);
    out.push('%');
    for c in query.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

fn fold(s: &str) -> String {
    s.chars().flat_map(char::to_lowercase).collect()
}

struct ScanRow {
    id: String,
    text: Option<String>,
    name: Option<String>,
    ts: i64,
    rowid: i64,
}

impl Store {
    /// Up to `limit` non-revoked messages of `chat_jid` matching `query`,
    /// newest first. A blank query or a zero limit returns nothing.
    pub fn search_messages(
        &self,
        chat_jid: &str,
        query: &str,
        limit: u32,
    ) -> Result<Vec<MessageRow>> {
        let query = query.trim();
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let query: String = query.chars().take(MAX_QUERY_CHARS).collect();
        let query = query.as_str();
        if query.is_ascii() {
            let sql = format!(
                "SELECT {MESSAGE_COLS} FROM messages m \
                 LEFT JOIN contacts ct ON ct.jid = m.sender_jid \
                 WHERE m.chat_jid = ?1 AND m.revoked = 0 \
                 AND (m.text LIKE ?2 ESCAPE '\\' OR m.media_name LIKE ?2 ESCAPE '\\') \
                 ORDER BY m.ts DESC, m.rowid DESC LIMIT ?3"
            );
            let conn = self.lock();
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map(
                    params![chat_jid, like_pattern(query), limit],
                    message_from_row,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            return Ok(rows);
        }

        let folded = fold(query);
        let ids = self.scan_matching_ids(chat_jid, &folded, limit as usize)?;
        self.messages_by_ids(chat_jid, &ids)
    }

    /// Phase one of the Unicode search: scans only the searchable columns in
    /// batches (newest first), releasing the store lock between batches so
    /// other readers and writers are not starved by a long chat.
    fn scan_matching_ids(&self, chat_jid: &str, folded: &str, limit: usize) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut cursor = (i64::MAX, i64::MAX);
        while out.len() < limit {
            let batch: Vec<ScanRow> = {
                let conn = self.lock();
                let mut stmt = conn.prepare(
                    "SELECT id, text, media_name, ts, rowid FROM messages \
                     WHERE chat_jid = ?1 AND revoked = 0 \
                     AND (ts < ?2 OR (ts = ?2 AND rowid < ?3)) \
                     ORDER BY ts DESC, rowid DESC LIMIT ?4",
                )?;
                stmt.query_map(params![chat_jid, cursor.0, cursor.1, SCAN_BATCH], |r| {
                    Ok(ScanRow {
                        id: r.get(0)?,
                        text: r.get(1)?,
                        name: r.get(2)?,
                        ts: r.get(3)?,
                        rowid: r.get(4)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
            };
            let Some(last) = batch.last() else { break };
            cursor = (last.ts, last.rowid);
            let exhausted = batch.len() < SCAN_BATCH as usize;
            for row in batch {
                let hit = [row.text.as_deref(), row.name.as_deref()]
                    .into_iter()
                    .flatten()
                    .any(|f| fold(f).contains(folded));
                if hit {
                    out.push(row.id);
                    if out.len() >= limit {
                        break;
                    }
                }
            }
            if exhausted {
                break;
            }
        }
        Ok(out)
    }

    /// Full rows of `ids` in the given order; ids that vanished are skipped.
    fn messages_by_ids(&self, chat_jid: &str, ids: &[String]) -> Result<Vec<MessageRow>> {
        let mut found: HashMap<String, MessageRow> = HashMap::with_capacity(ids.len());
        for chunk in ids.chunks(ID_CHUNK) {
            let marks = vec!["?"; chunk.len()].join(",");
            let sql = format!(
                "SELECT {MESSAGE_COLS} FROM messages m \
                 LEFT JOIN contacts ct ON ct.jid = m.sender_jid \
                 WHERE m.chat_jid = ?1 AND m.id IN ({marks})"
            );
            let conn = self.lock();
            let mut stmt = conn.prepare(&sql)?;
            let args = std::iter::once(chat_jid).chain(chunk.iter().map(String::as_str));
            let rows = stmt
                .query_map(rusqlite::params_from_iter(args), message_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for row in rows {
                found.insert(row.id.clone(), row);
            }
        }
        Ok(ids.iter().filter_map(|id| found.remove(id)).collect())
    }

    /// The message `id` with up to `before` older and `after` newer neighbours
    /// of the same chat, oldest first. Empty when `id` is not in the chat.
    pub fn message_window_around(
        &self,
        chat_jid: &str,
        id: &str,
        before: u32,
        after: u32,
    ) -> Result<Vec<MessageRow>> {
        let conn = self.lock();
        let anchor: Option<(i64, i64)> = conn
            .query_row(
                "SELECT ts, rowid FROM messages WHERE chat_jid = ?1 AND id = ?2",
                params![chat_jid, id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((ts, rowid)) = anchor else {
            return Ok(Vec::new());
        };
        let select = format!(
            "SELECT {MESSAGE_COLS} FROM messages m \
             LEFT JOIN contacts ct ON ct.jid = m.sender_jid \
             WHERE m.chat_jid = ?1 AND "
        );

        let older_sql = format!(
            "{select}(m.ts < ?2 OR (m.ts = ?2 AND m.rowid < ?3)) \
             ORDER BY m.ts DESC, m.rowid DESC LIMIT ?4"
        );
        let mut stmt = conn.prepare(&older_sql)?;
        let mut window = stmt
            .query_map(params![chat_jid, ts, rowid, before], message_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        window.reverse();

        // The anchor itself is the first row of the "newer" half.
        let newer_sql = format!(
            "{select}(m.ts > ?2 OR (m.ts = ?2 AND m.rowid >= ?3)) \
             ORDER BY m.ts ASC, m.rowid ASC LIMIT ?4"
        );
        let mut stmt = conn.prepare(&newer_sql)?;
        let from_anchor = stmt
            .query_map(
                params![chat_jid, ts, rowid, i64::from(after) + 1],
                message_from_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        window.extend(from_anchor);
        Ok(window)
    }
}

#[cfg(test)]
mod tests {
    use crate::{MessageKind, NewMessage, Store};

    const CHAT: &str = "1@s.whatsapp.net";

    fn msg(id: &str, ts: i64, text: Option<&str>) -> NewMessage {
        NewMessage {
            chat_jid: CHAT.into(),
            id: id.into(),
            sender_jid: CHAT.into(),
            from_me: false,
            ts,
            kind: MessageKind::Text,
            text: text.map(str::to_string),
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

    fn store_with(msgs: &[NewMessage]) -> Store {
        let s = Store::open_in_memory().unwrap();
        for m in msgs {
            assert!(s.insert_message(m, false).unwrap());
        }
        s
    }

    fn ids(rows: &[crate::MessageRow]) -> Vec<&str> {
        rows.iter().map(|r| r.id.as_str()).collect()
    }

    #[test]
    fn matches_case_insensitive_newest_first() {
        let s = store_with(&[
            msg("a", 1, Some("Hello World")),
            msg("b", 2, Some("nothing")),
            msg("c", 3, Some("say HELLO")),
        ]);
        let rows = s.search_messages(CHAT, "hello", 10).unwrap();
        assert_eq!(ids(&rows), ["c", "a"]);
        assert_eq!(ids(&s.search_messages(CHAT, "hello", 1).unwrap()), ["c"]);
        assert!(s.search_messages(CHAT, "  ", 10).unwrap().is_empty());
        assert!(s.search_messages("other", "hello", 10).unwrap().is_empty());
    }

    #[test]
    fn matches_media_name_and_caption() {
        let mut doc = msg("d", 5, None);
        doc.kind = MessageKind::Document;
        doc.media_name = Some("Report_Final.pdf".into());
        let s = store_with(&[doc, msg("e", 6, Some("caption text"))]);
        assert_eq!(ids(&s.search_messages(CHAT, "report_final", 10).unwrap()), ["d"]);
        assert_eq!(ids(&s.search_messages(CHAT, "caption", 10).unwrap()), ["e"]);
    }

    #[test]
    fn like_wildcards_are_literal() {
        let s = store_with(&[
            msg("a", 1, Some("100% sure")),
            msg("b", 2, Some("a_b")),
            msg("c", 3, Some("axb")),
            msg("d", 4, Some("back\\slash")),
        ]);
        assert_eq!(ids(&s.search_messages(CHAT, "%", 10).unwrap()), ["a"]);
        assert_eq!(ids(&s.search_messages(CHAT, "a_b", 10).unwrap()), ["b"]);
        assert_eq!(ids(&s.search_messages(CHAT, "\\", 10).unwrap()), ["d"]);
    }

    #[test]
    fn persian_and_unicode_case_fold() {
        let s = store_with(&[
            msg("a", 1, Some("سلام دوست من")),
            msg("b", 2, Some("ПРИВЕТ мир")),
            msg("c", 3, Some("hello")),
        ]);
        assert_eq!(ids(&s.search_messages(CHAT, "دوست", 10).unwrap()), ["a"]);
        assert_eq!(ids(&s.search_messages(CHAT, "привет", 10).unwrap()), ["b"]);
        assert_eq!(ids(&s.search_messages(CHAT, "ПРИВЕТ", 10).unwrap()), ["b"]);
        assert!(s.search_messages(CHAT, "غایب", 10).unwrap().is_empty());
        assert_eq!(s.search_messages(CHAT, "سلام", 0).unwrap().len(), 0);
    }

    #[test]
    fn persian_scan_spans_batches_in_order() {
        let msgs: Vec<NewMessage> = (0..5_000)
            .map(|i| {
                let text = if i % 1_000 == 7 { "پیام مهم" } else { "سلام" };
                msg(&format!("p{i}"), i, Some(text))
            })
            .collect();
        let s = store_with(&msgs);
        let rows = s.search_messages(CHAT, "مهم", 10).unwrap();
        assert_eq!(ids(&rows), ["p4007", "p3007", "p2007", "p1007", "p7"]);
        assert_eq!(ids(&s.search_messages(CHAT, "مهم", 2).unwrap()), ["p4007", "p3007"]);
    }

    #[test]
    fn long_query_is_truncated() {
        let long = "a".repeat(300);
        let s = store_with(&[msg("a", 1, Some(&"a".repeat(250))), msg("b", 2, Some(&"a".repeat(150)))]);
        assert_eq!(ids(&s.search_messages(CHAT, &long, 10).unwrap()), ["a"]);
        let persian = "س".repeat(300);
        let s = store_with(&[msg("a", 1, Some(&"س".repeat(250))), msg("b", 2, Some(&"س".repeat(150)))]);
        assert_eq!(ids(&s.search_messages(CHAT, &persian, 10).unwrap()), ["a"]);
    }

    #[test]
    fn revoked_excluded() {
        let s = store_with(&[msg("a", 1, Some("secret")), msg("b", 2, Some("secret too"))]);
        s.revoke_message(CHAT, "a").unwrap();
        assert_eq!(ids(&s.search_messages(CHAT, "secret", 10).unwrap()), ["b"]);
    }

    fn five() -> Store {
        store_with(&[
            msg("m1", 1, Some("one")),
            msg("m2", 2, Some("two")),
            msg("m3", 3, Some("three")),
            msg("m4", 4, Some("four")),
            msg("m5", 5, Some("five")),
        ])
    }

    #[test]
    fn window_middle_ascending() {
        let rows = five().message_window_around(CHAT, "m3", 1, 1).unwrap();
        assert_eq!(ids(&rows), ["m2", "m3", "m4"]);
    }

    #[test]
    fn window_boundaries() {
        let s = five();
        assert_eq!(ids(&s.message_window_around(CHAT, "m1", 5, 1).unwrap()), ["m1", "m2"]);
        assert_eq!(ids(&s.message_window_around(CHAT, "m5", 2, 5).unwrap()), ["m3", "m4", "m5"]);
        assert_eq!(ids(&s.message_window_around(CHAT, "m3", 0, 0).unwrap()), ["m3"]);
        assert_eq!(s.message_window_around(CHAT, "m3", 10, 10).unwrap().len(), 5);
        assert!(s.message_window_around(CHAT, "nope", 2, 2).unwrap().is_empty());
        assert!(s.message_window_around("other", "m3", 2, 2).unwrap().is_empty());
    }

    #[test]
    fn window_same_timestamp_keeps_insertion_order() {
        let s = store_with(&[
            msg("a", 7, Some("x")),
            msg("b", 7, Some("x")),
            msg("c", 7, Some("x")),
        ]);
        assert_eq!(ids(&s.message_window_around(CHAT, "b", 5, 5).unwrap()), ["a", "b", "c"]);
        assert_eq!(ids(&s.message_window_around(CHAT, "a", 0, 1).unwrap()), ["a", "b"]);
    }
}
