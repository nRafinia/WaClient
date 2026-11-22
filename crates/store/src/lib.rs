//! SQLite store for chats, contacts and messages of the client.
//!
//! Protocol-agnostic: JIDs are plain strings. The store is synchronous and
//! thread-safe (a single connection behind a mutex).

mod gallery;
mod model;
mod msgops;
mod queries;
mod search;
mod social;
mod storage;
mod translations;

#[cfg(test)]
mod tests;

use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

use rusqlite::Connection;

pub use model::{
    ChatKind, ChatRow, ContactPatch, ContactRow, MessageKind, MessageRow, MessageStatus,
    NewMessage, PollView, ReceiptKind, ReceiptRow, StatusGroup, StatusPurge, STATUS_JID,
    VIEW_ONCE_PREVIEW, VIEW_ONCE_TEXT,
};
pub use storage::{is_cache_file, plan_deletion, DeletionPlan, Doomed, MediaClass, MediaFile, MediaFilter};
pub use search::{GlobalFilter, SearchCursor, SearchKind, SenderFilter, SenderRow};
pub use translations::CachedTranslation;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("database schema version {found} is newer than supported {supported}")]
    SchemaTooNew { found: i64, supported: i64 },
}

pub type Result<T> = std::result::Result<T, StoreError>;

const MIGRATIONS: &[&str] = &[
    // 1: initial schema
    "
    CREATE TABLE chats (
        jid TEXT PRIMARY KEY,
        kind TEXT NOT NULL CHECK (kind IN ('dm','group','other')),
        name TEXT NULL,
        last_ts INTEGER NOT NULL DEFAULT 0,
        unread INTEGER NOT NULL DEFAULT 0,
        pinned INTEGER NOT NULL DEFAULT 0,
        archived INTEGER NOT NULL DEFAULT 0,
        muted_until INTEGER NOT NULL DEFAULT 0,
        last_preview TEXT NULL
    );
    CREATE TABLE contacts (
        jid TEXT PRIMARY KEY,
        full_name TEXT NULL,
        push_name TEXT NULL,
        phone TEXT NULL,
        avatar_path TEXT NULL,
        avatar_id TEXT NULL
    );
    CREATE TABLE messages (
        chat_jid TEXT NOT NULL,
        id TEXT NOT NULL,
        sender_jid TEXT NOT NULL,
        from_me INTEGER NOT NULL,
        ts INTEGER NOT NULL,
        kind TEXT NOT NULL CHECK (kind IN ('text','image','video','audio','voice','document','sticker','other')),
        text TEXT NULL,
        quoted_id TEXT NULL,
        quoted_preview TEXT NULL,
        media_mime TEXT NULL,
        media_name TEXT NULL,
        media_size INTEGER NULL,
        media_duration INTEGER NULL,
        thumbnail BLOB NULL,
        local_path TEXT NULL,
        status INTEGER NOT NULL DEFAULT 0,
        raw BLOB NULL,
        PRIMARY KEY (chat_jid, id)
    );
    CREATE INDEX idx_messages_chat_ts ON messages(chat_jid, ts DESC);
    CREATE INDEX idx_chats_last_ts ON chats(last_ts DESC);
    ",
    // 2: deleted / edited message markers
    "
    ALTER TABLE messages ADD COLUMN revoked INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE messages ADD COLUMN edited INTEGER NOT NULL DEFAULT 0;
    ",
    // 3: when the profile picture was last looked up (unix seconds, 0 = never)
    "
    ALTER TABLE contacts ADD COLUMN avatar_checked_at INTEGER NOT NULL DEFAULT 0;
    ",
    // 4: reactions, polls (+ votes) and the "status viewed" marker
    "
    CREATE TABLE reactions (
        chat_jid TEXT NOT NULL,
        msg_id TEXT NOT NULL,
        sender_jid TEXT NOT NULL,
        emoji TEXT NOT NULL,
        ts INTEGER NOT NULL,
        from_me INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (chat_jid, msg_id, sender_jid)
    );
    CREATE TABLE polls (
        chat_jid TEXT NOT NULL,
        msg_id TEXT NOT NULL,
        question TEXT NOT NULL,
        options_json TEXT NOT NULL,
        selectable_count INTEGER NOT NULL DEFAULT 1,
        PRIMARY KEY (chat_jid, msg_id)
    );
    CREATE TABLE poll_votes (
        chat_jid TEXT NOT NULL,
        msg_id TEXT NOT NULL,
        voter_jid TEXT NOT NULL,
        options_json TEXT NOT NULL,
        ts INTEGER NOT NULL,
        from_me INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (chat_jid, msg_id, voter_jid)
    );
    ALTER TABLE messages ADD COLUMN viewed INTEGER NOT NULL DEFAULT 0;
    ",
    // 5: the contact's "about" text and when it was set (unix seconds)
    "
    ALTER TABLE contacts ADD COLUMN about TEXT NULL;
    ALTER TABLE contacts ADD COLUMN about_set_at INTEGER NULL;
    ",
    // 6: link preview card of text messages
    "
    ALTER TABLE messages ADD COLUMN link_url TEXT NULL;
    ALTER TABLE messages ADD COLUMN link_title TEXT NULL;
    ALTER TABLE messages ADD COLUMN link_desc TEXT NULL;
    ALTER TABLE messages ADD COLUMN link_thumb BLOB NULL;
    ",
    // 7: per-chat "no read receipts" and whether a read receipt went out for
    // an incoming message
    "
    ALTER TABLE chats ADD COLUMN receipts_off INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE messages ADD COLUMN receipt_sent INTEGER NOT NULL DEFAULT 0;
    ",
    // 8: incoming messages of chats already fully read count as receipted, so
    // an explicit "send read receipts now" does not re-send old ones
    "
    UPDATE messages SET receipt_sent = 1
    WHERE from_me = 0 AND chat_jid IN (SELECT jid FROM chats WHERE unread = 0);
    ",
    // 9: message operations: the message pinned in a chat (with its expiry),
    // starred and "mentions me" flags, and per-recipient receipts
    "
    ALTER TABLE chats ADD COLUMN pinned_msg_id TEXT NULL;
    ALTER TABLE chats ADD COLUMN pinned_msg_until INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE messages ADD COLUMN starred INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE messages ADD COLUMN mentions_me INTEGER NOT NULL DEFAULT 0;
    CREATE INDEX idx_messages_starred ON messages(ts DESC) WHERE starred = 1;
    CREATE TABLE receipts (
        chat_jid TEXT NOT NULL,
        msg_id TEXT NOT NULL,
        jid TEXT NOT NULL,
        delivered_at INTEGER NULL,
        read_at INTEGER NULL,
        played_at INTEGER NULL,
        PRIMARY KEY (chat_jid, msg_id, jid)
    );
    ",
    // 10: shared locations, disappearing messages (per-message expiry and the
    // per-chat timer with the time it was set) and a small key/value table
    "
    ALTER TABLE messages ADD COLUMN geo_lat REAL NULL;
    ALTER TABLE messages ADD COLUMN geo_lon REAL NULL;
    ALTER TABLE messages ADD COLUMN expires_at INTEGER NULL;
    CREATE INDEX idx_messages_expires ON messages(expires_at) WHERE expires_at IS NOT NULL;
    ALTER TABLE chats ADD COLUMN ephemeral_secs INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE chats ADD COLUMN ephemeral_ts INTEGER NOT NULL DEFAULT 0;
    CREATE TABLE meta (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    ",
    // 11: cached message translations (one row per message and target language)
    "
    CREATE TABLE translations (
        chat_jid TEXT NOT NULL,
        msg_id TEXT NOT NULL,
        target_lang TEXT NOT NULL,
        text TEXT NOT NULL,
        source_lang TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        PRIMARY KEY (chat_jid, msg_id, target_lang)
    );
    ",
    // 12: cross-chat search (newest first by the full keyset, by sender, by kind)
    "
    CREATE INDEX idx_messages_ts ON messages(ts, chat_jid, id);
    CREATE INDEX idx_messages_sender ON messages(sender_jid, from_me, ts, chat_jid, id);
    CREATE INDEX idx_messages_kind ON messages(kind, ts, chat_jid, id);
    ",
];

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Store> {
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Store> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Store> {
        // journal_mode returns a row, so it must be read rather than executed.
        conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
        conn.execute_batch("PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;")?;
        migrate(&mut conn)?;
        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, Connection> {
        // A panic in another thread leaves at most a rolled-back transaction,
        // so recover from poisoning instead of propagating the panic.
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn migrate(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let supported = MIGRATIONS.len() as i64;
    if current > supported {
        return Err(StoreError::SchemaTooNew {
            found: current,
            supported,
        });
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.execute_batch(&format!("PRAGMA user_version = {}", i + 1))?;
        tx.commit()?;
    }
    Ok(())
}
