//! Stickers for the picker: the ones seen in chats ("recent") and the ones
//! the user starred ("favourites"). A sticker's identity is the hash of its
//! file, which only the caller can compute, so it is passed in.

use std::collections::HashSet;

use rusqlite::params;

use crate::model::{MessageKind, MessageRow};
use crate::queries::{message_from_row, MESSAGE_COLS};
use crate::{Result, Store};

/// Candidate rows read per wanted recent sticker: the same sticker is often
/// sent again and again, so many rows collapse into one.
const CANDIDATE_FACTOR: u32 = 4;

/// Keeps the first row of every distinct key, in order, up to `limit`. Rows
/// without a key (`None`) are dropped.
pub(crate) fn distinct_by_key<K: std::hash::Hash + Eq>(
    rows: Vec<MessageRow>,
    limit: usize,
    mut key: impl FnMut(&MessageRow) -> Option<K>,
) -> Vec<MessageRow> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for row in rows {
        if out.len() >= limit {
            break;
        }
        if let Some(k) = key(&row) {
            if seen.insert(k) {
                out.push(row);
            }
        }
    }
    out
}

impl Store {
    /// Sticker messages that are not deleted and whose file is stored locally,
    /// newest first and distinct by `key` (the file hash; `None` skips a row,
    /// e.g. when its file cannot be read), at most `limit`. `key` runs after
    /// the database lock is released, so it may read files.
    pub fn recent_stickers<K: std::hash::Hash + Eq>(
        &self,
        limit: u32,
        key: impl FnMut(&MessageRow) -> Option<K>,
    ) -> Result<Vec<MessageRow>> {
        let candidates = {
            let sql = format!(
                "SELECT {MESSAGE_COLS} FROM messages m \
                 LEFT JOIN contacts ct ON ct.jid = m.sender_jid \
                 WHERE m.kind = 'sticker' AND m.revoked = 0 \
                 AND m.local_path IS NOT NULL AND m.local_path != '' \
                 ORDER BY m.ts DESC, m.rowid DESC LIMIT ?1"
            );
            let conn = self.lock();
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map(params![limit.saturating_mul(CANDIDATE_FACTOR)], message_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        Ok(distinct_by_key(candidates, limit as usize, key))
    }

    /// Marks the sticker with file hash `sha` as a favourite, remembering the
    /// message it can be sent from. Adding it again refreshes the message
    /// and moves it to the front.
    pub fn add_sticker_fav(&self, sha: &str, chat_jid: &str, msg_id: &str, now: i64) -> Result<()> {
        self.lock().execute(
            "INSERT INTO sticker_favs (sha, chat_jid, msg_id, added_at) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(sha) DO UPDATE SET chat_jid = ?2, msg_id = ?3, added_at = ?4",
            params![sha, chat_jid, msg_id, now],
        )?;
        Ok(())
    }

    /// `true` when `sha` was a favourite.
    pub fn remove_sticker_fav(&self, sha: &str) -> Result<bool> {
        let n = self.lock().execute("DELETE FROM sticker_favs WHERE sha = ?1", params![sha])?;
        Ok(n > 0)
    }

    /// Whether the sticker with file hash `sha` is a favourite.
    pub fn is_sticker_fav(&self, sha: &str) -> Result<bool> {
        let n: i64 = self.lock().query_row(
            "SELECT COUNT(*) FROM sticker_favs WHERE sha = ?1",
            params![sha],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Favourites as `(sha, message)`, most recently added first. A favourite
    /// whose message is gone, deleted or has no local file is left out (it
    /// comes back if the file does).
    pub fn sticker_favs(&self) -> Result<Vec<(String, MessageRow)>> {
        let refs = {
            let conn = self.lock();
            let mut stmt = conn
                .prepare("SELECT sha, chat_jid, msg_id FROM sticker_favs ORDER BY added_at DESC, rowid DESC")?;
            let refs = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            refs
        };
        let mut out = Vec::with_capacity(refs.len());
        for (sha, chat_jid, msg_id) in refs {
            let Some(row) = self.message(&chat_jid, &msg_id)? else { continue };
            let cached = row.local_path.as_deref().is_some_and(|p| !p.is_empty());
            if row.kind == MessageKind::Sticker && !row.revoked && cached {
                out.push((sha, row));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use crate::{MessageKind, NewMessage, Store};

    const A: &str = "111@s.whatsapp.net";
    const B: &str = "222@s.whatsapp.net";

    fn sticker(chat: &str, id: &str, ts: i64, path: Option<&str>) -> NewMessage {
        NewMessage {
            chat_jid: chat.into(),
            id: id.into(),
            sender_jid: chat.into(),
            ts,
            kind: MessageKind::Sticker,
            local_path: path.map(String::from),
            ..Default::default()
        }
    }

    fn store(rows: &[NewMessage]) -> Store {
        let s = Store::open_in_memory().expect("store");
        for m in rows {
            assert!(s.insert_message(m, false).expect("insert"));
        }
        s
    }

    fn ids(rows: &[crate::MessageRow]) -> Vec<&str> {
        rows.iter().map(|r| r.id.as_str()).collect()
    }

    /// The "hash" of a test sticker is its file name.
    fn by_path(r: &crate::MessageRow) -> Option<String> {
        r.local_path.clone()
    }

    #[test]
    fn recent_is_newest_first_distinct_and_cached_only() {
        let s = store(&[
            sticker(A, "s1", 10, Some("one.webp")),
            sticker(A, "s2", 20, Some("two.webp")),
            sticker(B, "s3", 30, Some("one.webp")),
            sticker(B, "s4", 40, None),
            sticker(B, "s5", 50, Some("")),
            NewMessage { kind: MessageKind::Image, ..sticker(B, "i1", 60, Some("img.jpg")) },
        ]);
        let rows = s.recent_stickers(60, by_path).expect("recent");
        assert_eq!(ids(&rows), ["s3", "s2"]);
    }

    #[test]
    fn recent_respects_the_limit_and_skips_unkeyed_rows() {
        let s = store(&[
            sticker(A, "s1", 10, Some("a.webp")),
            sticker(A, "s2", 20, Some("b.webp")),
            sticker(A, "s3", 30, Some("c.webp")),
        ]);
        assert_eq!(ids(&s.recent_stickers(2, by_path).expect("limited")), ["s3", "s2"]);
        let skip_c = |r: &crate::MessageRow| by_path(r).filter(|p| p != "c.webp");
        assert_eq!(ids(&s.recent_stickers(2, skip_c).expect("skipping")), ["s2", "s1"]);
    }

    #[test]
    fn recent_leaves_out_deleted_stickers() {
        let s = store(&[sticker(A, "s1", 10, Some("a.webp")), sticker(A, "s2", 20, Some("b.webp"))]);
        s.revoke_message(A, "s2").expect("revoke");
        assert_eq!(ids(&s.recent_stickers(60, by_path).expect("recent")), ["s1"]);
    }

    #[test]
    fn favourites_add_remove_and_order() {
        let s = store(&[sticker(A, "s1", 10, Some("a.webp")), sticker(B, "s2", 20, Some("b.webp"))]);
        assert!(!s.is_sticker_fav("sha-a").expect("fav?"));
        s.add_sticker_fav("sha-a", A, "s1", 100).expect("add");
        s.add_sticker_fav("sha-b", B, "s2", 200).expect("add");
        assert!(s.is_sticker_fav("sha-a").expect("fav?"));
        let favs = s.sticker_favs().expect("favs");
        assert_eq!(favs.iter().map(|(h, _)| h.as_str()).collect::<Vec<_>>(), ["sha-b", "sha-a"]);
        assert_eq!(favs[0].1.id, "s2");

        // Adding again moves it to the front and updates its source message.
        s.add_sticker_fav("sha-a", A, "s1", 300).expect("re-add");
        assert_eq!(s.sticker_favs().expect("favs")[0].0, "sha-a");

        assert!(s.remove_sticker_fav("sha-a").expect("remove"));
        assert!(!s.remove_sticker_fav("sha-a").expect("remove again"));
        assert_eq!(s.sticker_favs().expect("favs").len(), 1);
    }

    #[test]
    fn favourites_without_a_usable_message_are_hidden() {
        let s = store(&[sticker(A, "s1", 10, Some("a.webp")), sticker(A, "s2", 20, None)]);
        s.add_sticker_fav("sha-1", A, "s1", 1).expect("add");
        s.add_sticker_fav("sha-2", A, "s2", 2).expect("add");
        s.add_sticker_fav("sha-3", A, "gone", 3).expect("add");
        let favs = s.sticker_favs().expect("favs");
        assert_eq!(favs.iter().map(|(h, _)| h.as_str()).collect::<Vec<_>>(), ["sha-1"]);
        s.revoke_message(A, "s1").expect("revoke");
        assert!(s.sticker_favs().expect("favs").is_empty());
    }
}
