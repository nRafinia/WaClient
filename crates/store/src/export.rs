//! Read-only queries behind "Export chat": a chat's messages in ascending
//! order, paged by `(ts, rowid)` so a long history is never held in memory.

use rusqlite::params;

use crate::model::MessageRow;
use crate::queries::{message_from_row, MESSAGE_COLS};
use crate::{Result, Store};

/// Position after which the next export page starts: `(ts, rowid)` of the last
/// row already read.
pub type ExportCursor = (i64, i64);

impl Store {
    /// Number of messages of a chat with `from <= ts < to` (a missing bound is
    /// open).
    pub fn export_count(&self, chat_jid: &str, from: Option<i64>, to: Option<i64>) -> Result<u64> {
        let n: i64 = self.lock().query_row(
            "SELECT COUNT(*) FROM messages m WHERE m.chat_jid = ?1 \
             AND (?2 IS NULL OR m.ts >= ?2) AND (?3 IS NULL OR m.ts < ?3)",
            params![chat_jid, from, to],
            |r| r.get(0),
        )?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    /// Up to `limit` messages in the range, oldest first, strictly after
    /// `after`. Each row comes with its cursor.
    pub fn export_page(
        &self,
        chat_jid: &str,
        from: Option<i64>,
        to: Option<i64>,
        after: Option<ExportCursor>,
        limit: u32,
    ) -> Result<Vec<(ExportCursor, MessageRow)>> {
        let sql = format!(
            "SELECT {MESSAGE_COLS}, m.rowid FROM messages m \
             LEFT JOIN contacts ct ON ct.jid = m.sender_jid \
             WHERE m.chat_jid = ?1 AND (?2 IS NULL OR m.ts >= ?2) AND (?3 IS NULL OR m.ts < ?3) \
             AND (?4 IS NULL OR m.ts > ?4 OR (m.ts = ?4 AND m.rowid > ?5)) \
             ORDER BY m.ts, m.rowid LIMIT ?6"
        );
        let (after_ts, after_rowid) = after.unzip();
        let conn = self.lock();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![chat_jid, from, to, after_ts, after_rowid, limit], |r| {
                let row = message_from_row(r)?;
                let rowid: i64 = r.get(28)?;
                Ok(((row.ts, rowid), row))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}
