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
//!
//! [`Store::search_everywhere`] is the cross-chat variant with filters (kind,
//! sender, date range) and a `(ts, chat_jid, id)` keyset cursor.

use std::collections::HashMap;

use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, OptionalExtension};

use crate::gallery::HAS_LINK;
use crate::model::{MessageRow, STATUS_JID};
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

/// Which kind of message a cross-chat search looks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchKind {
    /// Anything.
    #[default]
    All,
    /// Photos and videos.
    Media,
    /// Messages holding a link.
    Links,
    /// Documents.
    Documents,
    /// Voice messages and audio files.
    Voice,
}

impl SearchKind {
    fn clause(self) -> Option<String> {
        match self {
            SearchKind::All => None,
            SearchKind::Media => Some("m.kind IN ('image','video')".to_string()),
            SearchKind::Links => Some(format!("({HAS_LINK} OR m.link_url IS NOT NULL)")),
            SearchKind::Documents => Some("m.kind = 'document'".to_string()),
            SearchKind::Voice => Some("m.kind IN ('voice','audio')".to_string()),
        }
    }
}

/// Who wrote the messages a cross-chat search looks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SenderFilter {
    /// Messages sent from this account.
    Me,
    /// Messages received from this sender.
    Jid(String),
}

/// A cross-chat search: every part narrows the result, none is required.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GlobalFilter {
    /// Case-insensitive substring of the text, caption or file name.
    pub text: String,
    pub kind: SearchKind,
    pub sender: Option<SenderFilter>,
    /// First second (inclusive) a hit may be sent at.
    pub from_ts: Option<i64>,
    /// First second (exclusive) a hit may no longer be sent at.
    pub to_ts: Option<i64>,
}

impl GlobalFilter {
    /// Nothing narrows the search, so there is nothing to list.
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
            && self.kind == SearchKind::All
            && self.sender.is_none()
            && self.from_ts.is_none()
            && self.to_ts.is_none()
    }
}

/// A sender that has written at least one message, for the "From:" picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SenderRow {
    pub jid: String,
    /// Saved name, push name or number; `None` when nothing is known.
    pub name: Option<String>,
}

/// The `(ts, chat_jid, id)` of the last row of the previous page.
pub type SearchCursor<'a> = (i64, &'a str, &'a str);

const ORDER_KEYSET: &str = "ORDER BY m.ts DESC, m.chat_jid DESC, m.id DESC";
const PAIR_CHUNK: usize = 200;

/// WHERE clause (without the text match) and its arguments.
fn filter_where(f: &GlobalFilter, before: Option<SearchCursor<'_>>) -> (String, Vec<Value>) {
    let mut parts = vec!["m.revoked = 0".to_string(), "m.chat_jid <> ?".to_string()];
    let mut args = vec![Value::from(STATUS_JID.to_string())];
    if let Some(c) = f.kind.clause() {
        parts.push(c);
    }
    match &f.sender {
        Some(SenderFilter::Me) => parts.push("m.from_me = 1".to_string()),
        Some(SenderFilter::Jid(jid)) => {
            parts.push("m.sender_jid = ? AND m.from_me = 0".to_string());
            args.push(Value::from(jid.clone()));
        }
        None => {}
    }
    if let Some(ts) = f.from_ts {
        parts.push("m.ts >= ?".to_string());
        args.push(Value::from(ts));
    }
    if let Some(ts) = f.to_ts {
        parts.push("m.ts < ?".to_string());
        args.push(Value::from(ts));
    }
    if let Some((ts, chat, id)) = before {
        parts.push("(m.ts, m.chat_jid, m.id) < (?, ?, ?)".to_string());
        args.push(Value::from(ts));
        args.push(Value::from(chat.to_string()));
        args.push(Value::from(id.to_string()));
    }
    (parts.join(" AND "), args)
}

fn clean_query(text: &str) -> String {
    text.trim().chars().take(MAX_QUERY_CHARS).collect()
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

    /// One page of messages of every chat matching `f`, newest first, strictly
    /// older than the `before` cursor. A filter that narrows nothing, or a zero
    /// limit, returns nothing. Status updates and revoked messages are skipped.
    pub fn search_everywhere(
        &self,
        f: &GlobalFilter,
        before: Option<SearchCursor<'_>>,
        limit: u32,
    ) -> Result<Vec<MessageRow>> {
        if f.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let query = clean_query(&f.text);
        if query.is_empty() || query.is_ascii() {
            return self.search_everywhere_sql(f, &query, before, limit);
        }
        let keys = self.scan_everywhere(f, &fold(&query), before, limit as usize)?;
        self.rows_by_keys(&keys)
    }

    fn search_everywhere_sql(
        &self,
        f: &GlobalFilter,
        query: &str,
        before: Option<SearchCursor<'_>>,
        limit: u32,
    ) -> Result<Vec<MessageRow>> {
        let (mut cond, mut args) = filter_where(f, before);
        if !query.is_empty() {
            cond.push_str(" AND (m.text LIKE ? ESCAPE '\\' OR m.media_name LIKE ? ESCAPE '\\')");
            let pattern = like_pattern(query);
            args.push(Value::from(pattern.clone()));
            args.push(Value::from(pattern));
        }
        args.push(Value::from(i64::from(limit)));
        let sql = format!(
            "SELECT {MESSAGE_COLS} FROM messages m \
             LEFT JOIN contacts ct ON ct.jid = m.sender_jid \
             WHERE {cond} {ORDER_KEYSET} LIMIT ?"
        );
        let conn = self.lock();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params_from_iter(args), message_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Unicode path of [`Store::search_everywhere`]: scans the searchable
    /// columns of the rows passing the other filters in batches (the lock is
    /// released between them) and keeps the `(chat_jid, id)` of the matches.
    fn scan_everywhere(
        &self,
        f: &GlobalFilter,
        folded: &str,
        before: Option<SearchCursor<'_>>,
        limit: usize,
    ) -> Result<Vec<(String, String)>> {
        type Scanned = (String, String, Option<String>, Option<String>, i64);
        let mut out = Vec::new();
        let mut cursor: Option<(i64, String, String)> =
            before.map(|(t, c, i)| (t, c.to_string(), i.to_string()));
        while out.len() < limit {
            let key = cursor.as_ref().map(|(t, c, i)| (*t, c.as_str(), i.as_str()));
            let (cond, mut args) = filter_where(f, key);
            args.push(Value::from(i64::from(SCAN_BATCH)));
            let sql = format!(
                "SELECT m.chat_jid, m.id, m.text, m.media_name, m.ts FROM messages m \
                 WHERE {cond} {ORDER_KEYSET} LIMIT ?"
            );
            let batch: Vec<Scanned> = {
                let conn = self.lock();
                let mut stmt = conn.prepare(&sql)?;
                stmt.query_map(params_from_iter(args), |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
            };
            let Some((chat, id, _, _, ts)) = batch.last() else { break };
            cursor = Some((*ts, chat.clone(), id.clone()));
            let exhausted = batch.len() < SCAN_BATCH as usize;
            for (chat, id, text, name, _) in batch {
                let hit = [text.as_deref(), name.as_deref()]
                    .into_iter()
                    .flatten()
                    .any(|t| fold(t).contains(folded));
                if hit {
                    out.push((chat, id));
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

    /// Full rows of `(chat_jid, id)` keys in the given order; vanished ones are skipped.
    fn rows_by_keys(&self, keys: &[(String, String)]) -> Result<Vec<MessageRow>> {
        let mut found: HashMap<(String, String), MessageRow> = HashMap::with_capacity(keys.len());
        for chunk in keys.chunks(PAIR_CHUNK) {
            let marks = vec!["(?,?)"; chunk.len()].join(",");
            let sql = format!(
                "SELECT {MESSAGE_COLS} FROM messages m \
                 LEFT JOIN contacts ct ON ct.jid = m.sender_jid \
                 WHERE (m.chat_jid, m.id) IN (VALUES {marks})"
            );
            let conn = self.lock();
            let mut stmt = conn.prepare(&sql)?;
            let args = chunk.iter().flat_map(|(c, i)| [c.as_str(), i.as_str()]);
            let rows = stmt
                .query_map(params_from_iter(args), message_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for row in rows {
                found.insert((row.chat_jid.clone(), row.id.clone()), row);
            }
        }
        Ok(keys.iter().filter_map(|k| found.remove(k)).collect())
    }

    /// Senders that wrote at least one message, whose name or number contains
    /// `query` (blank: all), by name. Our own messages are not listed.
    pub fn known_senders(&self, query: &str, limit: usize) -> Result<Vec<SenderRow>> {
        let mut rows: Vec<SenderRow> = {
            let conn = self.lock();
            let mut stmt = conn.prepare(
                "SELECT s.jid, COALESCE(NULLIF(ct.full_name,''), NULLIF(ct.push_name,''), NULLIF(ct.phone,'')) \
                 FROM (SELECT DISTINCT sender_jid AS jid FROM messages WHERE from_me = 0) s \
                 LEFT JOIN contacts ct ON ct.jid = s.jid",
            )?;
            stmt.query_map([], |r| Ok(SenderRow { jid: r.get(0)?, name: r.get(1)? }))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let needle = fold(query.trim());
        if !needle.is_empty() {
            rows.retain(|r| {
                let user = r.jid.split('@').next().unwrap_or(&r.jid);
                fold(r.name.as_deref().unwrap_or("")).contains(&needle) || user.contains(&needle)
            });
        }
        rows.sort_by_cached_key(|r| (r.name.is_none(), fold(r.name.as_deref().unwrap_or(&r.jid)), r.jid.clone()));
        rows.truncate(limit);
        Ok(rows)
    }

    /// Timestamps of the messages of `chat_jid` sent in `[from_ts, to_ts)`, oldest first.
    pub fn chat_message_times(&self, chat_jid: &str, from_ts: i64, to_ts: i64) -> Result<Vec<i64>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT ts FROM messages WHERE chat_jid = ?1 AND ts >= ?2 AND ts < ?3 ORDER BY ts",
        )?;
        let rows = stmt
            .query_map(params![chat_jid, from_ts, to_ts], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<i64>>>()?;
        Ok(rows)
    }

    /// Timestamps of the oldest and the newest message of `chat_jid`.
    pub fn chat_time_span(&self, chat_jid: &str) -> Result<Option<(i64, i64)>> {
        let conn = self.lock();
        let span: (Option<i64>, Option<i64>) = conn.query_row(
            "SELECT MIN(ts), MAX(ts) FROM messages WHERE chat_jid = ?1",
            params![chat_jid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(span.0.zip(span.1))
    }

    /// Timestamps of the oldest and the newest message of any chat.
    pub fn message_time_span(&self) -> Result<Option<(i64, i64)>> {
        let conn = self.lock();
        let span: (Option<i64>, Option<i64>) = conn.query_row(
            "SELECT MIN(ts), MAX(ts) FROM messages WHERE chat_jid <> ?1",
            params![STATUS_JID],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(span.0.zip(span.1))
    }

    /// Id of the first message of `chat_jid` sent in `[from_ts, to_ts)`.
    pub fn first_message_between(&self, chat_jid: &str, from_ts: i64, to_ts: i64) -> Result<Option<String>> {
        Ok(self
            .lock()
            .query_row(
                "SELECT id FROM messages WHERE chat_jid = ?1 AND ts >= ?2 AND ts < ?3 \
                 ORDER BY ts ASC, rowid ASC LIMIT 1",
                params![chat_jid, from_ts, to_ts],
                |r| r.get(0),
            )
            .optional()?)
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

    use super::{GlobalFilter, SearchKind, SenderFilter};
    use crate::STATUS_JID;

    const OTHER: &str = "2@s.whatsapp.net";

    fn mk(chat: &str, id: &str, ts: i64, kind: MessageKind, text: Option<&str>) -> NewMessage {
        NewMessage { chat_jid: chat.into(), sender_jid: chat.into(), kind, ..msg(id, ts, text) }
    }

    fn gkey(rows: &[crate::MessageRow]) -> Vec<String> {
        rows.iter().map(|r| format!("{}:{}", r.chat_jid, r.id)).collect()
    }

    fn mixed() -> Store {
        let mut me = mk(CHAT, "me1", 15, MessageKind::Text, Some("note to self"));
        me.from_me = true;
        let mut doc = mk(OTHER, "doc", 30, MessageKind::Document, None);
        doc.media_name = Some("Invoice.pdf".into());
        let mut link = mk(CHAT, "lnk", 40, MessageKind::Text, Some("see https://example.com/x"));
        link.link_url = Some("https://example.com/x".into());
        store_with(&[
            mk(CHAT, "img", 10, MessageKind::Image, Some("holiday")),
            mk(OTHER, "vid", 20, MessageKind::Video, None),
            me,
            doc,
            mk(CHAT, "vo", 35, MessageKind::Voice, None),
            link,
            mk(OTHER, "txt", 50, MessageKind::Text, Some("hello www.site.org")),
            mk(STATUS_JID, "st", 60, MessageKind::Image, Some("status hello")),
        ])
    }

    fn run(s: &Store, f: &GlobalFilter) -> Vec<String> {
        gkey(&s.search_everywhere(f, None, 50).unwrap())
    }

    #[test]
    fn global_filters_by_kind_newest_first_across_chats() {
        let s = mixed();
        let by = |kind| run(&s, &GlobalFilter { kind, ..Default::default() });
        assert_eq!(by(SearchKind::Media), [format!("{OTHER}:vid"), format!("{CHAT}:img")]);
        assert_eq!(by(SearchKind::Documents), [format!("{OTHER}:doc")]);
        assert_eq!(by(SearchKind::Voice), [format!("{CHAT}:vo")]);
        assert_eq!(by(SearchKind::Links), [format!("{OTHER}:txt"), format!("{CHAT}:lnk")]);
        assert!(by(SearchKind::All).is_empty(), "no filter lists nothing");
    }

    #[test]
    fn global_text_combines_with_filters_and_skips_status_and_revoked() {
        let s = mixed();
        let f = GlobalFilter { text: "HELLO".into(), ..Default::default() };
        assert_eq!(run(&s, &f), [format!("{OTHER}:txt")]);
        let f = GlobalFilter { text: "invoice".into(), kind: SearchKind::Documents, ..Default::default() };
        assert_eq!(run(&s, &f), [format!("{OTHER}:doc")]);
        let f = GlobalFilter { text: "invoice".into(), kind: SearchKind::Media, ..Default::default() };
        assert!(run(&s, &f).is_empty());
        s.revoke_message(OTHER, "txt").unwrap();
        let f = GlobalFilter { text: "hello".into(), ..Default::default() };
        assert!(run(&s, &f).is_empty());
    }

    #[test]
    fn global_sender_and_date_range() {
        let s = mixed();
        let f = GlobalFilter { sender: Some(SenderFilter::Me), ..Default::default() };
        assert_eq!(run(&s, &f), [format!("{CHAT}:me1")]);
        let f = GlobalFilter { sender: Some(SenderFilter::Jid(OTHER.into())), ..Default::default() };
        assert_eq!(run(&s, &f), [format!("{OTHER}:txt"), format!("{OTHER}:doc"), format!("{OTHER}:vid")]);
        // [20, 40): the end is exclusive, the start inclusive.
        let f = GlobalFilter { from_ts: Some(20), to_ts: Some(40), ..Default::default() };
        assert_eq!(run(&s, &f), [format!("{CHAT}:vo"), format!("{OTHER}:doc"), format!("{OTHER}:vid")]);
        let f = GlobalFilter { from_ts: Some(50), ..Default::default() };
        assert_eq!(run(&s, &f), [format!("{OTHER}:txt")]);
    }

    #[test]
    fn global_unicode_text_uses_the_scan_with_filters() {
        let s = store_with(&[
            mk(CHAT, "a", 1, MessageKind::Text, Some("سلام دوست")),
            mk(OTHER, "b", 2, MessageKind::Image, Some("عکس دوست")),
            mk(OTHER, "c", 3, MessageKind::Text, Some("دوست")),
        ]);
        let f = GlobalFilter { text: "دوست".into(), ..Default::default() };
        assert_eq!(run(&s, &f), [format!("{OTHER}:c"), format!("{OTHER}:b"), format!("{CHAT}:a")]);
        let f = GlobalFilter { text: "دوست".into(), kind: SearchKind::Media, ..Default::default() };
        assert_eq!(run(&s, &f), [format!("{OTHER}:b")]);
    }

    #[test]
    fn global_keyset_paging_visits_ties_exactly_once() {
        let msgs: Vec<NewMessage> = (0..1_300)
            .map(|i| {
                let chat = if i % 2 == 0 { CHAT } else { OTHER };
                mk(chat, &format!("m{i:04}"), 100 + i64::from(i / 100), MessageKind::Text, Some("needle"))
            })
            .collect();
        let s = store_with(&msgs);
        let f = GlobalFilter { text: "needle".into(), ..Default::default() };
        let mut seen = Vec::new();
        let mut cursor: Option<(i64, String, String)> = None;
        loop {
            let key = cursor.as_ref().map(|(t, c, i)| (*t, c.as_str(), i.as_str()));
            let page = s.search_everywhere(&f, key, 128).unwrap();
            let Some(last) = page.last() else { break };
            cursor = Some((last.ts, last.chat_jid.clone(), last.id.clone()));
            seen.extend(gkey(&page));
        }
        assert_eq!(seen.len(), 1_300);
        let mut sorted = seen.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 1_300, "no row repeated");
    }

    #[test]
    fn global_unicode_paging_with_cursor() {
        let msgs: Vec<NewMessage> =
            (0..30).map(|i| mk(CHAT, &format!("u{i:02}"), i, MessageKind::Text, Some("پیام"))).collect();
        let s = store_with(&msgs);
        let f = GlobalFilter { text: "پیام".into(), ..Default::default() };
        let first = s.search_everywhere(&f, None, 10).unwrap();
        assert_eq!(first.len(), 10);
        let last = first.last().unwrap();
        let second = s.search_everywhere(&f, Some((last.ts, &last.chat_jid, &last.id)), 10).unwrap();
        assert_eq!(second.first().map(|m| m.id.as_str()), Some("u19"));
        assert_eq!(second.len(), 10);
    }

    #[test]
    fn known_senders_are_named_filtered_and_sorted() {
        let s = mixed();
        s.upsert_contact(&crate::ContactPatch {
            jid: OTHER.into(),
            full_name: Some("Zed".into()),
            push_name: None,
            phone: None,
        })
        .unwrap();
        let all = s.known_senders("", 10).unwrap();
        let jids: Vec<&str> = all.iter().map(|r| r.jid.as_str()).collect();
        assert_eq!(jids, [OTHER, CHAT, STATUS_JID]);
        assert_eq!(all[0].name.as_deref(), Some("Zed"));
        assert_eq!(s.known_senders("ze", 10).unwrap().len(), 1);
        assert_eq!(s.known_senders("", 1).unwrap().len(), 1);
        assert!(s.known_senders("nobody", 10).unwrap().is_empty());
    }

    /// `cargo test -p wa_store --release search::tests::global_search_perf -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn global_search_perf_50k() {
        use std::time::{Duration, Instant};

        const TOTAL: i64 = 50_000;
        let s = Store::open_in_memory().unwrap();
        let kinds = [
            MessageKind::Text,
            MessageKind::Text,
            MessageKind::Text,
            MessageKind::Text,
            MessageKind::Text,
            MessageKind::Text,
            MessageKind::Image,
            MessageKind::Video,
            MessageKind::Document,
            MessageKind::Voice,
        ];
        for start in (0..TOTAL).step_by(1_000) {
            let batch: Vec<NewMessage> = (start..start + 1_000)
                .map(|i| {
                    let chat = format!("{:03}@s.whatsapp.net", i % 200);
                    let text = match i {
                        _ if i % 5_000 == 17 => "a needle in the stack",
                        _ if i % 7_000 == 3 => "یک سوزن در انبار",
                        _ => "hello world, this is a typical message body",
                    };
                    mk(&chat, &format!("M{i}"), 1_600_000_000 + i, kinds[(i % 10) as usize], Some(text))
                })
                .collect();
            assert_eq!(s.insert_messages_bulk(&batch).unwrap(), 1_000);
        }

        let day = 86_400;
        let timed = |name: &str, limit: Duration, f: &GlobalFilter, want: Option<usize>| {
            let t = Instant::now();
            let rows = s.search_everywhere(f, None, 40).unwrap();
            let d = t.elapsed();
            println!("PERF {name}: {} rows in {d:?}", rows.len());
            if let Some(w) = want {
                assert_eq!(rows.len(), w, "{name}");
            }
            assert!(d < limit, "{name} too slow: {d:?}");
        };
        let q = |text: &str, kind| GlobalFilter { text: text.into(), kind, ..Default::default() };
        let fast = Duration::from_millis(250);
        timed("media newest", fast, &q("", SearchKind::Media), Some(40));
        timed("documents newest", fast, &q("", SearchKind::Documents), Some(40));
        timed("voice newest", fast, &q("", SearchKind::Voice), Some(40));
        timed("links newest", fast, &q("", SearchKind::Links), None);
        timed("common word", fast, &q("hello", SearchKind::All), Some(40));
        timed("rare word, whole table", fast, &q("needle", SearchKind::All), Some(10));
        timed("rare word in media", fast, &q("needle", SearchKind::Media), None);
        let sender = GlobalFilter { sender: Some(SenderFilter::Jid("007@s.whatsapp.net".into())), ..Default::default() };
        timed("one sender", fast, &sender, Some(40));
        let dates = GlobalFilter {
            from_ts: Some(1_600_000_000 + day / 4),
            to_ts: Some(1_600_000_000 + day / 2),
            ..Default::default()
        };
        timed("date range", fast, &dates, Some(40));
        timed("rare unicode word", Duration::from_secs(2), &q("سوزن", SearchKind::All), Some(8));
        let t = Instant::now();
        let senders = s.known_senders("", 60).unwrap();
        println!("PERF known_senders: {} in {:?}", senders.len(), t.elapsed());
        assert_eq!(senders.len(), 60);
    }

    #[test]
    fn chat_day_helpers() {
        let s = five();
        assert_eq!(s.chat_message_times(CHAT, 2, 5).unwrap(), [2, 3, 4]);
        assert_eq!(s.chat_time_span(CHAT).unwrap(), Some((1, 5)));
        assert_eq!(s.chat_time_span("other").unwrap(), None);
        assert_eq!(s.message_time_span().unwrap(), Some((1, 5)));
        assert_eq!(Store::open_in_memory().unwrap().message_time_span().unwrap(), None);
        assert_eq!(s.first_message_between(CHAT, 3, 10).unwrap().as_deref(), Some("m3"));
        assert_eq!(s.first_message_between(CHAT, 6, 10).unwrap(), None);
        assert_eq!(s.first_message_between("other", 0, 10).unwrap(), None);
    }
}
