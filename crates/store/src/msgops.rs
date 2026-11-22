//! Message operations: pinned message of a chat, starred messages and
//! per-recipient receipts.

use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, OptionalExtension};

use crate::model::{MessageRow, ReceiptKind, ReceiptRow};
use crate::queries::{message_from_row, MESSAGE_COLS};
use crate::{Result, Store};

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Orders receipts for display: recipients who read it first (earliest read
/// first), then those it was only delivered to, then the rest; ties by jid.
fn sort_receipts(rows: &mut [ReceiptRow]) {
    let rank = |r: &ReceiptRow| match (r.read_at.or(r.played_at), r.delivered_at) {
        (Some(t), _) => (0, t),
        (None, Some(t)) => (1, t),
        (None, None) => (2, 0),
    };
    rows.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.jid.cmp(&b.jid)));
}

impl Store {
    /// Sets (`Some(id)`) or clears (`None`) the message pinned in a chat.
    /// `until` is the unix second the pin expires at. Returns whether the row
    /// changed.
    pub fn set_pinned_message(&self, chat_jid: &str, id: Option<&str>, until: i64) -> Result<bool> {
        let until = if id.is_some() { until } else { 0 };
        let changed = self.lock().execute(
            "UPDATE chats SET pinned_msg_id = ?2, pinned_msg_until = ?3 \
             WHERE jid = ?1 AND (pinned_msg_id IS NOT ?2 OR pinned_msg_until <> ?3)",
            params![chat_jid, id, until],
        )?;
        Ok(changed > 0)
    }

    /// Clears the pin of a chat, but only while it still is message `id`.
    /// Returns whether it was cleared.
    pub fn clear_pinned_message_if(&self, chat_jid: &str, id: &str) -> Result<bool> {
        let changed = self.lock().execute(
            "UPDATE chats SET pinned_msg_id = NULL, pinned_msg_until = 0 \
             WHERE jid = ?1 AND pinned_msg_id = ?2",
            params![chat_jid, id],
        )?;
        Ok(changed > 0)
    }

    /// The chat's pinned message `(id, expires_at)`; `None` when nothing is
    /// pinned or the pin has expired.
    pub fn pinned_message(&self, chat_jid: &str) -> Result<Option<(String, i64)>> {
        self.pinned_message_at(chat_jid, now_secs())
    }

    /// [`Store::pinned_message`] as of `now` (unix seconds).
    pub fn pinned_message_at(&self, chat_jid: &str, now: i64) -> Result<Option<(String, i64)>> {
        let row: Option<(Option<String>, i64)> = self
            .lock()
            .query_row(
                "SELECT pinned_msg_id, pinned_msg_until FROM chats WHERE jid = ?1",
                params![chat_jid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(match row {
            Some((Some(id), until)) if until > now => Some((id, until)),
            _ => None,
        })
    }

    /// Marks a message starred or not. Returns whether the flag changed (false
    /// for an unknown message or when it already had that value).
    pub fn set_starred(&self, chat_jid: &str, id: &str, starred: bool) -> Result<bool> {
        let changed = self.lock().execute(
            "UPDATE messages SET starred = ?3 WHERE chat_jid = ?1 AND id = ?2 AND starred <> ?3",
            params![chat_jid, id, starred],
        )?;
        Ok(changed > 0)
    }

    /// Starred, not deleted messages of `chat` (every chat when `None`),
    /// newest first. `before` is the `(ts, chat_jid, id)` of the last row of
    /// the previous page: a keyset cursor over the full sort key, so any number
    /// of rows sharing a timestamp can be paged through exactly once.
    pub fn starred_messages(
        &self,
        limit: u32,
        chat: Option<&str>,
        before: Option<(i64, &str, &str)>,
    ) -> Result<Vec<MessageRow>> {
        let sql = format!(
            "SELECT {MESSAGE_COLS} FROM messages m \
             LEFT JOIN contacts ct ON ct.jid = m.sender_jid \
             WHERE m.starred = 1 AND m.revoked = 0 AND (?1 IS NULL OR m.chat_jid = ?1) \
             AND (?2 IS NULL OR (m.ts, m.chat_jid, m.id) < (?2, ?3, ?4)) \
             ORDER BY m.ts DESC, m.chat_jid DESC, m.id DESC LIMIT ?5"
        );
        let (b_ts, b_chat, b_id) = match before {
            Some((ts, c, i)) => (Some(ts), Some(c), Some(i)),
            None => (None, None, None),
        };
        let conn = self.lock();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![chat, b_ts, b_chat, b_id, limit], message_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Records that `jid` delivered / read / played our message. Each kind
    /// keeps its earliest timestamp, so a replayed receipt cannot move it.
    pub fn record_receipt(
        &self,
        chat_jid: &str,
        msg_id: &str,
        jid: &str,
        kind: ReceiptKind,
        ts: i64,
    ) -> Result<()> {
        let column = match kind {
            ReceiptKind::Delivered => "delivered_at",
            ReceiptKind::Read => "read_at",
            ReceiptKind::Played => "played_at",
        };
        let sql = format!(
            "INSERT INTO receipts (chat_jid, msg_id, jid, {column}) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(chat_jid, msg_id, jid) DO UPDATE SET \
             {column} = MIN(COALESCE(receipts.{column}, excluded.{column}), excluded.{column})"
        );
        self.lock().execute(&sql, params![chat_jid, msg_id, jid, ts])?;
        Ok(())
    }

    /// Receipts of one message per recipient, read first, then delivered.
    pub fn message_receipts(&self, chat_jid: &str, msg_id: &str) -> Result<Vec<ReceiptRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT jid, delivered_at, read_at, played_at FROM receipts \
             WHERE chat_jid = ?1 AND msg_id = ?2",
        )?;
        let mut rows = stmt
            .query_map(params![chat_jid, msg_id], |r| {
                Ok(ReceiptRow {
                    jid: r.get(0)?,
                    delivered_at: r.get(1)?,
                    read_at: r.get(2)?,
                    played_at: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        sort_receipts(&mut rows);
        Ok(rows)
    }
}
