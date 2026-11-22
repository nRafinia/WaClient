use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::model::*;
use crate::{Result, Store};

pub(crate) const MESSAGE_COLS: &str = "m.chat_jid, m.id, m.sender_jid, \
    COALESCE(NULLIF(ct.full_name,''), NULLIF(ct.push_name,''), NULLIF(ct.phone,'')), \
    m.from_me, m.ts, m.kind, m.text, m.quoted_id, m.quoted_preview, m.media_mime, \
    m.media_name, m.media_size, m.media_duration, m.thumbnail, m.local_path, m.status, \
    m.revoked, m.edited, m.link_url, m.link_title, m.link_desc, m.link_thumb, m.starred, \n    m.mentions_me, m.geo_lat, m.geo_lon, m.expires_at";

pub(crate) fn message_from_row(r: &Row<'_>) -> rusqlite::Result<MessageRow> {
    Ok(MessageRow {
        chat_jid: r.get(0)?,
        id: r.get(1)?,
        sender_jid: r.get(2)?,
        sender_name: r.get(3)?,
        from_me: r.get::<_, i64>(4)? != 0,
        ts: r.get(5)?,
        kind: MessageKind::parse(&r.get::<_, String>(6)?),
        text: r.get(7)?,
        quoted_id: r.get(8)?,
        quoted_preview: r.get(9)?,
        media_mime: r.get(10)?,
        media_name: r.get(11)?,
        media_size: r.get(12)?,
        media_duration: r.get(13)?,
        thumbnail: r.get(14)?,
        local_path: r.get(15)?,
        status: MessageStatus::from_i64(r.get(16)?),
        revoked: r.get::<_, i64>(17)? != 0,
        edited: r.get::<_, i64>(18)? != 0,
        link_url: r.get(19)?,
        link_title: r.get(20)?,
        link_desc: r.get(21)?,
        link_thumb: r.get(22)?,
        starred: r.get::<_, i64>(23)? != 0,
        mentions_me: r.get::<_, i64>(24)? != 0,
        geo_lat: r.get(25)?,
        geo_lon: r.get(26)?,
        expires_at: r.get(27)?,
    })
}

const CONTACT_COLS: &str =
    "jid, full_name, push_name, phone, avatar_path, avatar_id, avatar_checked_at, about, \
     about_set_at";

fn contact_from_row(r: &Row<'_>) -> rusqlite::Result<ContactRow> {
    Ok(ContactRow {
        jid: r.get(0)?,
        full_name: r.get(1)?,
        push_name: r.get(2)?,
        phone: r.get(3)?,
        avatar_path: r.get(4)?,
        avatar_id: r.get(5)?,
        avatar_checked_at: r.get(6)?,
        about: r.get(7)?,
        about_set_at: r.get(8)?,
    })
}

const CHAT_SELECT: &str = "SELECT c.jid, c.kind, \
    COALESCE(NULLIF(c.name,''), NULLIF(ct.full_name,''), NULLIF(ct.push_name,''), NULLIF(ct.phone,'')), \
    c.last_ts, c.unread, c.pinned, c.archived, c.muted_until, c.last_preview, \
    c.receipts_off, c.ephemeral_secs, c.ephemeral_ts \
    FROM chats c LEFT JOIN contacts ct ON ct.jid = c.jid";

fn chat_from_row(r: &Row<'_>) -> rusqlite::Result<ChatRow> {
    let jid: String = r.get(0)?;
    let kind = ChatKind::parse(&r.get::<_, String>(1)?);
    let resolved: Option<String> = r.get(2)?;
    let display_name = resolved.unwrap_or_else(|| fallback_name(&jid, kind));
    Ok(ChatRow {
        jid,
        kind,
        display_name,
        last_ts: r.get(3)?,
        unread: r.get(4)?,
        pinned: r.get::<_, i64>(5)? != 0,
        archived: r.get::<_, i64>(6)? != 0,
        muted_until: r.get(7)?,
        last_preview: r.get(8)?,
        receipts_off: r.get::<_, i64>(9)? != 0,
        ephemeral_secs: r.get(10)?,
        ephemeral_ts: r.get(11)?,
    })
}

fn fallback_name(jid: &str, kind: ChatKind) -> String {
    if kind == ChatKind::Dm {
        let user = jid.split('@').next().unwrap_or(jid);
        if !user.is_empty() {
            return format!("+{user}");
        }
    }
    jid.to_string()
}

fn preview_for(msg: &NewMessage) -> String {
    preview_of(msg.text.as_deref(), msg.kind, msg.media_name.as_deref(), msg.media_mime.as_deref())
}

/// MIME type the engine gives shared contacts; their text holds the numbers.
const VCARD_MIME: &str = "text/vcard";

fn preview_of(
    text: Option<&str>,
    kind: MessageKind,
    media_name: Option<&str>,
    media_mime: Option<&str>,
) -> String {
    if media_mime == Some(VCARD_MIME) {
        return format!("👤 {}", media_name.filter(|n| !n.is_empty()).unwrap_or("Contact"));
    }
    if let Some(t) = text.filter(|t| !t.is_empty()) {
        if t == crate::VIEW_ONCE_TEXT {
            return crate::VIEW_ONCE_PREVIEW.to_string();
        }
        return t.to_string();
    }
    match kind {
        MessageKind::Image => "📷 Photo".into(),
        MessageKind::Voice => "🎤 Voice message".into(),
        MessageKind::Document => format!("📄 {}", media_name.unwrap_or("Document")),
        MessageKind::Video => "🎥 Video".into(),
        MessageKind::Audio => "🎵 Audio".into(),
        MessageKind::Sticker => "Sticker".into(),
        MessageKind::Text | MessageKind::Other => String::new(),
    }
}

/// Largest forward correction [`Store::insert_outgoing`] applies to a timestamp.
const OUTGOING_CLAMP_MAX_SECS: i64 = 24 * 3600;

/// Chat preview shown after the latest message was deleted for everyone.
pub const REVOKED_PREVIEW: &str = "🚫 This message was deleted";

/// `(ts, preview)` of the newest message of a chat, `None` for an empty chat.
fn latest_summary(conn: &Connection, chat_jid: &str) -> Result<Option<(i64, String)>> {
    Ok(conn
        .query_row(
            "SELECT ts, revoked, kind, text, media_name, media_mime FROM messages WHERE chat_jid = ?1 \
             ORDER BY ts DESC, rowid DESC LIMIT 1",
            params![chat_jid],
            |r| {
                let revoked = r.get::<_, i64>(1)? != 0;
                let kind = MessageKind::parse(&r.get::<_, String>(2)?);
                let text: Option<String> = r.get(3)?;
                let name: Option<String> = r.get(4)?;
                let mime: Option<String> = r.get(5)?;
                let preview = if revoked {
                    REVOKED_PREVIEW.to_string()
                } else {
                    preview_of(text.as_deref(), kind, name.as_deref(), mime.as_deref())
                };
                Ok((r.get(0)?, preview))
            },
        )
        .optional()?)
}

/// Deletes all messages, reactions and polls of a chat; returns the cached
/// media paths the messages referenced.
fn remove_chat_content(conn: &Connection, chat_jid: &str) -> Result<Vec<String>> {
    let paths = conn
        .prepare("SELECT local_path FROM messages WHERE chat_jid = ?1 AND local_path IS NOT NULL")?
        .query_map(params![chat_jid], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for table in ["messages", "reactions", "polls", "poll_votes", "receipts"] {
        conn.execute(&format!("DELETE FROM {table} WHERE chat_jid = ?1"), params![chat_jid])?;
    }
    Ok(paths)
}

/// Clears the chat's pinned message when it is `id`.
fn unpin_if(conn: &Connection, chat_jid: &str, id: &str) -> Result<()> {
    conn.execute(
        "UPDATE chats SET pinned_msg_id = NULL, pinned_msg_until = 0          WHERE jid = ?1 AND pinned_msg_id = ?2",
        params![chat_jid, id],
    )?;
    Ok(())
}

/// Whether `(chat_jid, id)` is the newest message of its chat.
fn is_latest(conn: &Connection, chat_jid: &str, id: &str) -> Result<bool> {
    let latest: Option<String> = conn
        .query_row(
            "SELECT id FROM messages WHERE chat_jid = ?1 ORDER BY ts DESC, rowid DESC LIMIT 1",
            params![chat_jid],
            |r| r.get(0),
        )
        .optional()?;
    Ok(latest.as_deref() == Some(id))
}

struct ChatState {
    kind: String,
    name: Option<String>,
    last_ts: i64,
    unread: i64,
    pinned: bool,
    archived: bool,
    muted_until: i64,
    last_preview: Option<String>,
    receipts_off: bool,
}

fn read_chat_state(conn: &Connection, jid: &str) -> Result<Option<ChatState>> {
    Ok(conn
        .query_row(
            "SELECT kind, name, last_ts, unread, pinned, archived, muted_until, last_preview, \
             receipts_off FROM chats WHERE jid = ?1",
            params![jid],
            |r| {
                Ok(ChatState {
                    kind: r.get(0)?,
                    name: r.get(1)?,
                    last_ts: r.get(2)?,
                    unread: r.get(3)?,
                    pinned: r.get::<_, i64>(4)? != 0,
                    archived: r.get::<_, i64>(5)? != 0,
                    muted_until: r.get(6)?,
                    last_preview: r.get(7)?,
                    receipts_off: r.get::<_, i64>(8)? != 0,
                })
            },
        )
        .optional()?)
}

/// Inserts one message plus chat bookkeeping; returns whether it was new.
fn insert_one(conn: &Connection, msg: &NewMessage, count_unread: bool) -> Result<bool> {
    // Cached: history sync inserts hundreds of thousands of rows with these
    // exact statements.
    let inserted = conn
        .prepare_cached(
            "INSERT OR IGNORE INTO messages (chat_jid, id, sender_jid, from_me, ts, kind, text,              quoted_id, quoted_preview, media_mime, media_name, media_size, media_duration,              thumbnail, local_path, raw, link_url, link_title, link_desc, link_thumb, mentions_me,              geo_lat, geo_lon, expires_at)              VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24)",
        )?
        .execute(params![
            msg.chat_jid,
            msg.id,
            msg.sender_jid,
            msg.from_me,
            msg.ts,
            msg.kind.as_str(),
            msg.text,
            msg.quoted_id,
            msg.quoted_preview,
            msg.media_mime,
            msg.media_name,
            msg.media_size,
            msg.media_duration,
            msg.thumbnail,
            msg.local_path,
            msg.raw,
            msg.link_url,
            msg.link_title,
            msg.link_desc,
            msg.link_thumb,
            msg.mentions_me,
            msg.geo_lat,
            msg.geo_lon,
            msg.expires_at,
        ])?
        > 0;
    if !inserted {
        return Ok(false);
    }
    conn.prepare_cached("INSERT OR IGNORE INTO chats (jid, kind) VALUES (?1, ?2)")?
        .execute(params![msg.chat_jid, ChatKind::from_jid(&msg.chat_jid).as_str()])?;
    conn.prepare_cached(
        "UPDATE chats SET last_ts = ?2, last_preview = ?3 WHERE jid = ?1 AND ?2 >= last_ts",
    )?
    .execute(params![msg.chat_jid, msg.ts, preview_for(msg)])?;
    // Statuses live in a pseudo chat that has no unread badge.
    if count_unread && !msg.from_me && msg.chat_jid != STATUS_JID {
        conn.prepare_cached("UPDATE chats SET unread = unread + 1 WHERE jid = ?1")?
            .execute(params![msg.chat_jid])?;
    }
    Ok(true)
}

impl Store {
    pub fn upsert_chat(&self, jid: &str, kind: ChatKind, name: Option<&str>) -> Result<()> {
        self.lock().execute(
            "INSERT INTO chats (jid, kind, name) VALUES (?1, ?2, ?3) \
             ON CONFLICT(jid) DO UPDATE SET kind = excluded.kind, \
             name = COALESCE(excluded.name, chats.name)",
            params![jid, kind.as_str(), name],
        )?;
        Ok(())
    }

    pub fn set_chat_flags(
        &self,
        jid: &str,
        pinned: Option<bool>,
        archived: Option<bool>,
        muted_until: Option<i64>,
    ) -> Result<()> {
        self.lock().execute(
            "UPDATE chats SET pinned = COALESCE(?2, pinned), archived = COALESCE(?3, archived), \
             muted_until = COALESCE(?4, muted_until) WHERE jid = ?1",
            params![jid, pinned, archived, muted_until],
        )?;
        Ok(())
    }

    pub fn upsert_contact(&self, patch: &ContactPatch) -> Result<()> {
        self.lock().execute(
            "INSERT INTO contacts (jid, full_name, push_name, phone) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(jid) DO UPDATE SET \
             full_name = COALESCE(excluded.full_name, contacts.full_name), \
             push_name = COALESCE(excluded.push_name, contacts.push_name), \
             phone = COALESCE(excluded.phone, contacts.phone)",
            params![patch.jid, patch.full_name, patch.push_name, patch.phone],
        )?;
        Ok(())
    }

    pub fn set_contact_avatar(&self, jid: &str, avatar_id: &str, path: &str) -> Result<()> {
        self.lock().execute(
            "INSERT INTO contacts (jid, avatar_id, avatar_path) VALUES (?1, ?2, ?3) \
             ON CONFLICT(jid) DO UPDATE SET avatar_id = excluded.avatar_id, \
             avatar_path = excluded.avatar_path",
            params![jid, avatar_id, path],
        )?;
        Ok(())
    }

    /// Forgets the stored profile picture of `jid` (it removed it).
    pub fn clear_contact_avatar(&self, jid: &str) -> Result<()> {
        self.lock().execute(
            "UPDATE contacts SET avatar_id = NULL, avatar_path = NULL WHERE jid = ?1",
            params![jid],
        )?;
        Ok(())
    }

    pub fn contact(&self, jid: &str) -> Result<Option<ContactRow>> {
        let sql = format!("SELECT {CONTACT_COLS} FROM contacts WHERE jid = ?1");
        Ok(self
            .lock()
            .query_row(&sql, params![jid], contact_from_row)
            .optional()?)
    }

    /// Stores the "about" text of a contact; `None` clears it.
    pub fn set_contact_about(
        &self,
        jid: &str,
        about: Option<&str>,
        set_at: Option<i64>,
    ) -> Result<()> {
        self.lock().execute(
            "INSERT INTO contacts (jid, about, about_set_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT(jid) DO UPDATE SET about = excluded.about, \
             about_set_at = excluded.about_set_at",
            params![jid, about, set_at],
        )?;
        Ok(())
    }

    /// Records that the profile picture of `jid` was looked up at `ts`.
    pub fn mark_avatar_checked(&self, jid: &str, ts: i64) -> Result<()> {
        self.lock().execute(
            "INSERT INTO contacts (jid, avatar_checked_at) VALUES (?1, ?2) \
             ON CONFLICT(jid) DO UPDATE SET avatar_checked_at = excluded.avatar_checked_at",
            params![jid, ts],
        )?;
        Ok(())
    }

    /// Returns `true` if the message was new (duplicates are ignored).
    pub fn insert_message(&self, msg: &NewMessage, count_unread: bool) -> Result<bool> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let inserted = insert_one(&tx, msg, count_unread)?;
        tx.commit()?;
        Ok(inserted)
    }

    /// Insert for a message this app sends. Its `ts` comes from the local
    /// clock while the chat's other rows carry the senders' clocks, so a PC
    /// clock that is behind would sort the new message above ones it
    /// answers. The stored `ts` is raised to the chat's newest one. Rows more
    /// than [`OUTGOING_CLAMP_MAX_SECS`] ahead of our clock are ignored, so a
    /// bogus far-future row cannot drag our messages into the future.
    /// Equal timestamps sort by insertion order, i.e. after the existing rows.
    pub fn insert_outgoing(&self, msg: &NewMessage) -> Result<bool> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let newest: Option<i64> = tx.query_row(
            "SELECT MAX(ts) FROM messages WHERE chat_jid = ?1 AND ts <= ?2",
            params![msg.chat_jid, msg.ts.saturating_add(OUTGOING_CLAMP_MAX_SECS)],
            |r| r.get(0),
        )?;
        let ts = newest.map_or(msg.ts, |n| msg.ts.max(n));
        let inserted = if ts == msg.ts {
            insert_one(&tx, msg, false)?
        } else {
            insert_one(&tx, &NewMessage { ts, ..msg.clone() }, false)?
        };
        tx.commit()?;
        Ok(inserted)
    }

    /// History-sync insert: one transaction, never counts unread.
    pub fn insert_messages_bulk(&self, msgs: &[NewMessage]) -> Result<usize> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut n = 0;
        for m in msgs {
            if insert_one(&tx, m, false)? {
                n += 1;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// Offline-drain insert: one transaction, counts unread for every newly
    /// inserted incoming message. Returns, per input message, whether it was new.
    pub fn insert_messages_bulk_counting(&self, msgs: &[NewMessage]) -> Result<Vec<bool>> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut inserted = Vec::with_capacity(msgs.len());
        for m in msgs {
            inserted.push(insert_one(&tx, m, true)?);
        }
        tx.commit()?;
        Ok(inserted)
    }

    /// Marks every outgoing message still `Pending` as `Failed` (the app died
    /// or the connection generation ended before it was sent). Returns the
    /// distinct chats that had such messages.
    pub fn fail_stale_pending(&self) -> Result<Vec<String>> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let chats = {
            let mut stmt = tx.prepare(
                "SELECT DISTINCT chat_jid FROM messages WHERE from_me = 1 AND status = 0",
            )?;
            stmt.query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<String>>>()?
        };
        tx.execute(
            "UPDATE messages SET status = -1 WHERE from_me = 1 AND status = 0",
            [],
        )?;
        tx.commit()?;
        Ok(chats)
    }

    /// Moves the status forward only; `Failed` may only replace `Pending`.
    pub fn update_status(&self, chat_jid: &str, id: &str, status: MessageStatus) -> Result<()> {
        self.lock().execute(
            "UPDATE messages SET status = ?3 WHERE chat_jid = ?1 AND id = ?2 \
             AND (status < ?3 OR (?3 = -1 AND status = 0))",
            params![chat_jid, id, status.to_i64()],
        )?;
        Ok(())
    }

    pub fn set_local_path(&self, chat_jid: &str, id: &str, path: &str) -> Result<()> {
        self.lock().execute(
            "UPDATE messages SET local_path = ?3 WHERE chat_jid = ?1 AND id = ?2",
            params![chat_jid, id, path],
        )?;
        Ok(())
    }

    pub fn set_raw(&self, chat_jid: &str, id: &str, raw: &[u8]) -> Result<()> {
        self.lock().execute(
            "UPDATE messages SET raw = ?3 WHERE chat_jid = ?1 AND id = ?2",
            params![chat_jid, id, raw],
        )?;
        Ok(())
    }

    pub fn message_raw(&self, chat_jid: &str, id: &str) -> Result<Option<Vec<u8>>> {
        let raw: Option<Option<Vec<u8>>> = self
            .lock()
            .query_row(
                "SELECT raw FROM messages WHERE chat_jid = ?1 AND id = ?2",
                params![chat_jid, id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(raw.flatten())
    }

    /// Insertion order of a message within the store (breaks ties of equal `ts`).
    pub fn message_rowid(&self, chat_jid: &str, id: &str) -> Result<Option<i64>> {
        Ok(self
            .lock()
            .query_row(
                "SELECT rowid FROM messages WHERE chat_jid = ?1 AND id = ?2",
                params![chat_jid, id],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn message(&self, chat_jid: &str, id: &str) -> Result<Option<MessageRow>> {
        let sql = format!(
            "SELECT {MESSAGE_COLS} FROM messages m \
             LEFT JOIN contacts ct ON ct.jid = m.sender_jid \
             WHERE m.chat_jid = ?1 AND m.id = ?2"
        );
        Ok(self
            .lock()
            .query_row(&sql, params![chat_jid, id], message_from_row)
            .optional()?)
    }

    /// Chats for the list view. Chats we never exchanged a message in (e.g. a
    /// group we are in but never talked in) are hidden unless pinned;
    /// [`Store::search_chats`] still finds them.
    pub fn list_chats(&self, include_archived: bool) -> Result<Vec<ChatRow>> {
        self.query_chats(include_archived, true)
    }

    fn query_chats(&self, include_archived: bool, hide_empty: bool) -> Result<Vec<ChatRow>> {
        let sql = format!(
            "{CHAT_SELECT} WHERE c.jid <> '{STATUS_JID}' AND (?1 OR c.archived = 0) \
             AND (?2 = 0 OR c.pinned = 1 OR c.last_ts > 0 \
                  OR EXISTS (SELECT 1 FROM messages m WHERE m.chat_jid = c.jid)) \
             ORDER BY c.pinned DESC, c.last_ts DESC, c.rowid"
        );
        let conn = self.lock();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![include_archived, hide_empty], chat_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// One chat with the same display-name resolution as [`Store::list_chats`].
    pub fn chat(&self, jid: &str) -> Result<Option<ChatRow>> {
        let sql = format!("{CHAT_SELECT} WHERE c.jid = ?1");
        Ok(self
            .lock()
            .query_row(&sql, params![jid], chat_from_row)
            .optional()?)
    }

    /// Marks a message as deleted for everyone and drops its content. If it is
    /// the chat's newest message the chat preview becomes [`REVOKED_PREVIEW`].
    pub fn revoke_message(&self, chat_jid: &str, id: &str) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let changed = tx.execute(
            "UPDATE messages SET revoked = 1, text = NULL, thumbnail = NULL, media_mime = NULL, \
             media_name = NULL, media_size = NULL, media_duration = NULL, local_path = NULL, link_url = NULL, \
             link_title = NULL, link_desc = NULL, link_thumb = NULL \
             WHERE chat_jid = ?1 AND id = ?2",
            params![chat_jid, id],
        )?;
        if changed > 0 && is_latest(&tx, chat_jid, id)? {
            tx.execute(
                "UPDATE chats SET last_preview = ?2 WHERE jid = ?1",
                params![chat_jid, REVOKED_PREVIEW],
            )?;
        }
        if changed > 0 {
            unpin_if(&tx, chat_jid, id)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Removes a message locally ("delete for me") together with its
    /// reactions and poll rows. When it was the chat's newest message the chat's
    /// `last_ts` / `last_preview` fall back to the new newest one. Returns the
    /// cached media path of the removed row, if it had one. Deleting an unread
    /// incoming message also decrements the chat's unread count.
    pub fn delete_message(&self, chat_jid: &str, id: &str) -> Result<Option<String>> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let row: Option<(Option<String>, bool, i64, i64)> = tx
            .query_row(
                "SELECT local_path, from_me, ts, rowid FROM messages WHERE chat_jid = ?1 AND id = ?2",
                params![chat_jid, id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((local_path, from_me, ts, rowid)) = row else {
            return Ok(None);
        };
        // Unread messages are the newest incoming ones: removing one of them
        // must not leave the badge counting a message that no longer exists.
        let was_unread = !from_me && {
            let unread: i64 = tx
                .query_row("SELECT unread FROM chats WHERE jid = ?1", params![chat_jid], |r| r.get(0))
                .optional()?
                .unwrap_or(0);
            let newer_or_same: i64 = tx.query_row(
                "SELECT COUNT(*) FROM messages WHERE chat_jid = ?1 AND from_me = 0 AND (ts > ?2 OR (ts = ?2 AND rowid >= ?3))",
                params![chat_jid, ts, rowid],
                |r| r.get(0),
            )?;
            unread > 0 && newer_or_same <= unread
        };
        let was_latest = is_latest(&tx, chat_jid, id)?;
        tx.execute(
            "DELETE FROM messages WHERE chat_jid = ?1 AND id = ?2",
            params![chat_jid, id],
        )?;
        for table in ["reactions", "polls", "poll_votes", "receipts"] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE chat_jid = ?1 AND msg_id = ?2"),
                params![chat_jid, id],
            )?;
        }
        unpin_if(&tx, chat_jid, id)?;
        if was_unread {
            tx.execute(
                "UPDATE chats SET unread = MAX(unread - 1, 0) WHERE jid = ?1",
                params![chat_jid],
            )?;
        }
        if was_latest {
            let (ts, preview) = match latest_summary(&tx, chat_jid)? {
                Some((ts, preview)) => (ts, Some(preview)),
                None => (0, None),
            };
            tx.execute(
                "UPDATE chats SET last_ts = ?2, last_preview = ?3 WHERE jid = ?1",
                params![chat_jid, ts, preview],
            )?;
        }
        tx.commit()?;
        Ok(local_path)
    }

    /// Removes the messages of a chat with their reactions and polls, keeping
    /// the chat row. With `upto = Some((ts, id))` only rows ordered at or before
    /// that message (by `ts`, then insertion order) go: a clear synced from this
    /// device covers what was sent, not what arrived meanwhile. Without a bound
    /// the chat is emptied and its preview, last_ts and unread are reset; with
    /// one they fall back to what remains. Returns the cached media paths of the
    /// removed rows.
    pub fn clear_chat(&self, chat_jid: &str, upto: Option<(i64, &str)>) -> Result<Vec<String>> {
        self.clear_messages(chat_jid, upto, false)
    }

    /// Like [`Store::clear_chat`], but starred messages (with their reactions,
    /// polls and receipts) stay. `last_ts` / `last_preview` fall back to the
    /// newest remaining message, the pinned message is released only when it
    /// was removed, and unread is reset (or capped to the incoming messages
    /// after the bound).
    pub fn clear_chat_keep_starred(
        &self,
        chat_jid: &str,
        upto: Option<(i64, &str)>,
    ) -> Result<Vec<String>> {
        self.clear_messages(chat_jid, upto, true)
    }

    fn clear_messages(
        &self,
        chat_jid: &str,
        upto: Option<(i64, &str)>,
        keep_starred: bool,
    ) -> Result<Vec<String>> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        if upto.is_none() && !keep_starred {
            let paths = remove_chat_content(&tx, chat_jid)?;
            tx.execute(
                "UPDATE chats SET last_ts = 0, unread = 0, last_preview = NULL, pinned_msg_id = NULL, 
             pinned_msg_until = 0 WHERE jid = ?1",
                params![chat_jid],
            )?;
            tx.commit()?;
            return Ok(paths);
        }
        // The bound message may be gone already: then only strictly older rows
        // are in range (rowid -1 matches nothing at the tie).
        let (max_ts, max_rowid): (Option<i64>, i64) = match upto {
            None => (None, 0),
            Some((ts, id)) => {
                let rowid = tx
                    .query_row(
                        "SELECT rowid FROM messages WHERE chat_jid = ?1 AND id = ?2",
                        params![chat_jid, id],
                        |r| r.get::<_, i64>(0),
                    )
                    .optional()?
                    .unwrap_or(-1);
                (Some(ts), rowid)
            }
        };
        let doomed = format!(
            "chat_jid = ?1 AND (?2 IS NULL OR ts < ?2 OR (ts = ?2 AND rowid <= ?3)){}",
            if keep_starred { " AND starred = 0" } else { "" }
        );
        let paths = tx
            .prepare(&format!("SELECT local_path FROM messages WHERE {doomed} AND local_path IS NOT NULL"))?
            .query_map(params![chat_jid, max_ts, max_rowid], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for table in ["reactions", "polls", "poll_votes", "receipts"] {
            tx.execute(
                &format!(
                    "DELETE FROM {table} WHERE chat_jid = ?1 AND msg_id NOT IN                      (SELECT id FROM messages WHERE chat_jid = ?1 AND NOT ({doomed}))"
                ),
                params![chat_jid, max_ts, max_rowid],
            )?;
        }
        tx.execute(&format!("DELETE FROM messages WHERE {doomed}"), params![chat_jid, max_ts, max_rowid])?;
        let (ts, preview) = match latest_summary(&tx, chat_jid)? {
            Some((ts, preview)) => (ts, Some(preview)),
            None => (0, None),
        };
        let unread_cap = if max_ts.is_some() {
            "(SELECT COUNT(*) FROM messages WHERE chat_jid = ?1 AND from_me = 0)"
        } else {
            "0"
        };
        tx.execute(
            &format!(
                "UPDATE chats SET last_ts = ?2, last_preview = ?3, unread = MIN(unread, {unread_cap})                  WHERE jid = ?1"
            ),
            params![chat_jid, ts, preview],
        )?;
        tx.execute(
            "UPDATE chats SET pinned_msg_id = NULL, pinned_msg_until = 0 WHERE jid = ?1              AND pinned_msg_id IS NOT NULL AND pinned_msg_id NOT IN              (SELECT id FROM messages WHERE chat_jid = ?1)",
            params![chat_jid],
        )?;
        tx.commit()?;
        Ok(paths)
    }

    /// Removes a chat entirely: its row, messages, reactions and polls. Returns
    /// the cached media paths of the removed messages.
    pub fn delete_chat(&self, chat_jid: &str) -> Result<Vec<String>> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let paths = remove_chat_content(&tx, chat_jid)?;
        tx.execute("DELETE FROM chats WHERE jid = ?1", params![chat_jid])?;
        tx.commit()?;
        Ok(paths)
    }

    /// Sets the link preview card of a message (e.g. once a fetch that ran after
    /// the row was stored has finished). Ignored for deleted messages.
    pub fn set_link_preview(
        &self,
        chat_jid: &str,
        id: &str,
        url: Option<&str>,
        title: Option<&str>,
        desc: Option<&str>,
        thumb: Option<&[u8]>,
    ) -> Result<()> {
        self.lock().execute(
            "UPDATE messages SET link_url = ?3, link_title = ?4, link_desc = ?5, link_thumb = ?6 \
             WHERE chat_jid = ?1 AND id = ?2 AND revoked = 0",
            params![chat_jid, id, url, title, desc, thumb],
        )?;
        Ok(())
    }

    /// Replaces the text of a message; ignored for deleted messages. A link
    /// preview card is kept only while the new text still contains its link
    /// verbatim, otherwise the card would point somewhere the text does not.
    pub fn edit_message(&self, chat_jid: &str, id: &str, new_text: &str) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let changed = tx.execute(
            "UPDATE messages SET text = ?3, edited = 1, \
             link_title = CASE WHEN link_url IS NOT NULL AND instr(?3, link_url) > 0 THEN link_title END, \
             link_desc = CASE WHEN link_url IS NOT NULL AND instr(?3, link_url) > 0 THEN link_desc END, \
             link_thumb = CASE WHEN link_url IS NOT NULL AND instr(?3, link_url) > 0 THEN link_thumb END, \
             link_url = CASE WHEN link_url IS NOT NULL AND instr(?3, link_url) > 0 THEN link_url END \
             WHERE chat_jid = ?1 AND id = ?2 AND revoked = 0",
            params![chat_jid, id, new_text],
        )?;
        if changed > 0 && is_latest(&tx, chat_jid, id)? {
            tx.execute(
                "UPDATE chats SET last_preview = COALESCE(NULLIF(?2, ''), last_preview) \
                 WHERE jid = ?1",
                params![chat_jid, new_text],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Chats of `kind` that still have no name (group subjects not yet fetched).
    pub fn chats_missing_name(&self, kind: ChatKind) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT jid FROM chats WHERE kind = ?1 AND (name IS NULL OR name = '') ORDER BY jid",
        )?;
        let rows = stmt
            .query_map(params![kind.as_str()], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(rows)
    }

    /// Folds chat and contact `from_jid` into `into_jid` (a LID that turned out
    /// to be a known phone number). Rows already present under `into_jid` win.
    /// Returns whether anything existed under `from_jid`.
    pub fn merge_chat(&self, from_jid: &str, into_jid: &str) -> Result<bool> {
        if from_jid == into_jid {
            return Ok(false);
        }
        let mut moved = false;
        let mut conn = self.lock();
        let tx = conn.transaction()?;

        if let Some(from) = read_chat_state(&tx, from_jid)? {
            moved = true;
            tx.execute(
                "INSERT OR IGNORE INTO chats (jid, kind) VALUES (?1, ?2)",
                params![into_jid, from.kind],
            )?;
            if let Some(into) = read_chat_state(&tx, into_jid)? {
                tx.execute(
                    "UPDATE OR IGNORE messages SET chat_jid = ?2, \
                     sender_jid = CASE WHEN sender_jid = ?1 THEN ?2 ELSE sender_jid END \
                     WHERE chat_jid = ?1",
                    params![from_jid, into_jid],
                )?;
                tx.execute("DELETE FROM messages WHERE chat_jid = ?1", params![from_jid])?;
                tx.execute(
                    "UPDATE OR IGNORE reactions SET chat_jid = ?2, \
                     sender_jid = CASE WHEN sender_jid = ?1 THEN ?2 ELSE sender_jid END \
                     WHERE chat_jid = ?1",
                    params![from_jid, into_jid],
                )?;
                tx.execute(
                    "UPDATE OR IGNORE poll_votes SET chat_jid = ?2, \
                     voter_jid = CASE WHEN voter_jid = ?1 THEN ?2 ELSE voter_jid END \
                     WHERE chat_jid = ?1",
                    params![from_jid, into_jid],
                )?;
                tx.execute(
                    "UPDATE OR IGNORE polls SET chat_jid = ?2 WHERE chat_jid = ?1",
                    params![from_jid, into_jid],
                )?;
                tx.execute(
                    "UPDATE OR IGNORE receipts SET chat_jid = ?2 WHERE chat_jid = ?1",
                    params![from_jid, into_jid],
                )?;
                tx.execute(
                    "UPDATE chats SET                      pinned_msg_id = (SELECT pinned_msg_id FROM chats WHERE jid = ?1),                      pinned_msg_until = (SELECT pinned_msg_until FROM chats WHERE jid = ?1)                      WHERE jid = ?2 AND pinned_msg_until <                      (SELECT pinned_msg_until FROM chats WHERE jid = ?1)",
                    params![from_jid, into_jid],
                )?;
                for table in ["reactions", "poll_votes", "polls", "receipts"] {
                    tx.execute(
                        &format!("DELETE FROM {table} WHERE chat_jid = ?1"),
                        params![from_jid],
                    )?;
                }

                let (last_ts, last_preview) = if from.last_ts > into.last_ts {
                    (from.last_ts, from.last_preview.or(into.last_preview))
                } else {
                    (into.last_ts, into.last_preview.or(from.last_preview))
                };
                tx.execute(
                    "UPDATE chats SET name = ?2, last_ts = ?3, last_preview = ?4, unread = ?5, \
                     pinned = ?6, archived = ?7, muted_until = ?8, receipts_off = ?9 WHERE jid = ?1",
                    params![
                        into_jid,
                        into.name.filter(|n| !n.is_empty()).or(from.name),
                        last_ts,
                        last_preview,
                        into.unread + from.unread,
                        into.pinned || from.pinned,
                        into.archived || from.archived,
                        into.muted_until.max(from.muted_until),
                        into.receipts_off || from.receipts_off,
                    ],
                )?;
                tx.execute("DELETE FROM chats WHERE jid = ?1", params![from_jid])?;
            }
        }

        let from_contact: Option<ContactRow> = tx
            .query_row(
                &format!("SELECT {CONTACT_COLS} FROM contacts WHERE jid = ?1"),
                params![from_jid],
                contact_from_row,
            )
            .optional()?;
        if let Some(c) = from_contact {
            moved = true;
            tx.execute(
                "INSERT INTO contacts (jid, full_name, push_name, phone, avatar_path, avatar_id, \
                 avatar_checked_at, about, about_set_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
                 ON CONFLICT(jid) DO UPDATE SET \
                 full_name = COALESCE(contacts.full_name, excluded.full_name), \
                 push_name = COALESCE(contacts.push_name, excluded.push_name), \
                 phone = COALESCE(contacts.phone, excluded.phone), \
                 avatar_path = COALESCE(contacts.avatar_path, excluded.avatar_path), \
                 avatar_id = COALESCE(contacts.avatar_id, excluded.avatar_id), \
                 avatar_checked_at = MAX(contacts.avatar_checked_at, excluded.avatar_checked_at), \
                 about = COALESCE(contacts.about, excluded.about), \
                 about_set_at = COALESCE(contacts.about_set_at, excluded.about_set_at)",
                params![
                    into_jid,
                    c.full_name,
                    c.push_name,
                    c.phone,
                    c.avatar_path,
                    c.avatar_id,
                    c.avatar_checked_at,
                    c.about,
                    c.about_set_at
                ],
            )?;
            tx.execute("DELETE FROM contacts WHERE jid = ?1", params![from_jid])?;
        }
        tx.commit()?;
        Ok(moved)
    }

    /// Deletes every message, chat and contact in one transaction.
    pub fn clear_all(&self) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute_batch("DELETE FROM messages; DELETE FROM chats; DELETE FROM contacts; \
             DELETE FROM reactions; DELETE FROM polls; DELETE FROM poll_votes; \n             DELETE FROM receipts;")?;
        tx.commit()?;
        Ok(())
    }

    /// Newest `limit` messages older than `before_ts`, in ascending order.
    pub fn messages_page(
        &self,
        chat_jid: &str,
        before_ts: Option<i64>,
        limit: u32,
    ) -> Result<Vec<MessageRow>> {
        let sql = format!(
            "SELECT {MESSAGE_COLS} FROM messages m \
             LEFT JOIN contacts ct ON ct.jid = m.sender_jid \
             WHERE m.chat_jid = ?1 AND (?2 IS NULL OR m.ts < ?2) \
             ORDER BY m.ts DESC, m.rowid DESC LIMIT ?3"
        );
        let conn = self.lock();
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt
            .query_map(params![chat_jid, before_ts, limit], message_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.reverse();
        Ok(rows)
    }

    /// Newest `limit` messages strictly before the `before` anchor
    /// (`(ts, id)` of a message in this chat), in ascending order. Pages by
    /// `(ts, rowid)` so messages sharing a timestamp are neither skipped nor
    /// repeated. An anchor that no longer exists falls back to its timestamp.
    pub fn messages_page_before(
        &self,
        chat_jid: &str,
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
                // Without a row the timestamp alone bounds the page.
                (Some(*ts), Some(rowid.unwrap_or(i64::MIN)))
            }
        };
        let sql = format!(
            "SELECT {MESSAGE_COLS} FROM messages m              LEFT JOIN contacts ct ON ct.jid = m.sender_jid              WHERE m.chat_jid = ?1 AND (?2 IS NULL OR m.ts < ?2 OR (m.ts = ?2 AND m.rowid < ?3))              ORDER BY m.ts DESC, m.rowid DESC LIMIT ?4"
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt
            .query_map(params![chat_jid, anchor_ts, anchor_rowid, limit], message_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.reverse();
        Ok(rows)
    }

    pub fn mark_chat_read(&self, chat_jid: &str) -> Result<()> {
        self.set_unread(chat_jid, 0)
    }

    pub fn set_unread(&self, chat_jid: &str, n: i64) -> Result<()> {
        self.lock().execute(
            "UPDATE chats SET unread = ?2 WHERE jid = ?1",
            params![chat_jid, n],
        )?;
        Ok(())
    }

    /// Subtracts `n` acknowledged messages from the unread counter in one
    /// statement (never below zero), so a message that arrived after the
    /// acknowledged ones were read stays counted.
    pub fn decrement_unread(&self, chat_jid: &str, n: i64) -> Result<()> {
        self.lock().execute(
            "UPDATE chats SET unread = MAX(0, unread - MAX(0, ?2)) WHERE jid = ?1",
            params![chat_jid, n],
        )?;
        Ok(())
    }

    /// Turns automatic read receipts off (`true`) or on for one chat.
    pub fn set_chat_receipts_off(&self, jid: &str, off: bool) -> Result<()> {
        self.lock().execute(
            "UPDATE chats SET receipts_off = ?2 WHERE jid = ?1",
            params![jid, off],
        )?;
        Ok(())
    }

    /// Sets the disappearing-message timer of a chat (`secs` 0 = off) made at
    /// `ts`. Applied only when `ts` is not older than the stored setting, so a
    /// late-arriving older change cannot override a newer one. Returns whether
    /// it was applied.
    pub fn set_chat_ephemeral(&self, chat_jid: &str, secs: i64, ts: i64) -> Result<bool> {
        let changed = self.lock().execute(
            "UPDATE chats SET ephemeral_secs = ?2, ephemeral_ts = ?3 \
             WHERE jid = ?1 AND ?3 >= ephemeral_ts",
            params![chat_jid, secs, ts],
        )?;
        Ok(changed > 0)
    }

    /// Messages whose expiry is at or before `now`, as `(chat_jid, id,
    /// local_path)`, oldest expiry first. Starred messages are kept.
    pub fn expired_messages(
        &self,
        now: i64,
        limit: u32,
    ) -> Result<Vec<(String, String, Option<String>)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT chat_jid, id, local_path FROM messages \
             WHERE expires_at IS NOT NULL AND expires_at <= ?1 AND starred = 0 \
             ORDER BY expires_at, rowid LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![now, limit], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Reads a value of the small key/value table.
    pub fn meta_get(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .lock()
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| r.get(0))
            .optional()?)
    }

    /// Writes (or replaces) a value of the small key/value table.
    pub fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        self.lock().execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Incoming, not deleted messages of a chat no read receipt was sent for,
    /// at or after `since_ts`, newest first, as `(id, sender_jid)`.
    pub fn unreceipted_incoming(
        &self,
        chat_jid: &str,
        since_ts: i64,
        limit: u32,
    ) -> Result<Vec<(String, String)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, sender_jid FROM messages \
             WHERE chat_jid = ?1 AND from_me = 0 AND revoked = 0 AND receipt_sent = 0 \
               AND ts >= ?2 \
             ORDER BY ts DESC, rowid DESC LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(params![chat_jid, since_ts, limit], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Records that a read receipt went out for these messages.
    pub fn mark_receipts_sent(&self, chat_jid: &str, ids: &[String]) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "UPDATE messages SET receipt_sent = 1 WHERE chat_jid = ?1 AND id = ?2",
            )?;
            for id in ids {
                stmt.execute(params![chat_jid, id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Case-insensitive substring match on the display name (archived included).
    pub fn search_chats(&self, query: &str) -> Result<Vec<ChatRow>> {
        let needle = query.to_lowercase();
        Ok(self
            .query_chats(true, false)?
            .into_iter()
            .filter(|c| c.display_name.to_lowercase().contains(&needle))
            .collect())
    }
}
