//! Plain data types exchanged with the store.

/// Pseudo chat that holds status updates; never listed as a chat.
pub const STATUS_JID: &str = "status@broadcast";

/// Body of the placeholder stored for a view-once message: its media is never
/// kept, so it cannot be viewed, forwarded or saved here.
pub const VIEW_ONCE_TEXT: &str = "\u{1f512} View once message \u{2014} open it on your phone";
/// Short form of [`VIEW_ONCE_TEXT`] for the chat list, notifications and quotes.
pub const VIEW_ONCE_PREVIEW: &str = "\u{1f512} View once";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatKind {
    Dm,
    Group,
    Other,
}

impl ChatKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ChatKind::Dm => "dm",
            ChatKind::Group => "group",
            ChatKind::Other => "other",
        }
    }

    pub fn parse(s: &str) -> ChatKind {
        match s {
            "dm" => ChatKind::Dm,
            "group" => ChatKind::Group,
            _ => ChatKind::Other,
        }
    }

    /// Infers the kind from a JID suffix.
    pub fn from_jid(jid: &str) -> ChatKind {
        if jid.ends_with("@g.us") {
            ChatKind::Group
        } else if jid.ends_with("@s.whatsapp.net") || jid.ends_with("@lid") {
            ChatKind::Dm
        } else {
            ChatKind::Other
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MessageKind {
    #[default]
    Text,
    Image,
    Video,
    Audio,
    Voice,
    Document,
    Sticker,
    Other,
}

impl MessageKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            MessageKind::Text => "text",
            MessageKind::Image => "image",
            MessageKind::Video => "video",
            MessageKind::Audio => "audio",
            MessageKind::Voice => "voice",
            MessageKind::Document => "document",
            MessageKind::Sticker => "sticker",
            MessageKind::Other => "other",
        }
    }

    pub fn parse(s: &str) -> MessageKind {
        match s {
            "text" => MessageKind::Text,
            "image" => MessageKind::Image,
            "video" => MessageKind::Video,
            "audio" => MessageKind::Audio,
            "voice" => MessageKind::Voice,
            "document" => MessageKind::Document,
            "sticker" => MessageKind::Sticker,
            _ => MessageKind::Other,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MessageStatus {
    #[default]
    Pending = 0,
    Sent = 1,
    Delivered = 2,
    Read = 3,
    Played = 4,
    Failed = -1,
}

impl MessageStatus {
    pub fn to_i64(self) -> i64 {
        self as i64
    }

    /// Unknown values map to `Pending`.
    pub fn from_i64(v: i64) -> MessageStatus {
        match v {
            1 => MessageStatus::Sent,
            2 => MessageStatus::Delivered,
            3 => MessageStatus::Read,
            4 => MessageStatus::Played,
            -1 => MessageStatus::Failed,
            _ => MessageStatus::Pending,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct NewMessage {
    pub chat_jid: String,
    pub id: String,
    pub sender_jid: String,
    pub from_me: bool,
    /// Unix seconds.
    pub ts: i64,
    pub kind: MessageKind,
    /// Body or caption.
    pub text: Option<String>,
    pub quoted_id: Option<String>,
    pub quoted_preview: Option<String>,
    pub media_mime: Option<String>,
    pub media_name: Option<String>,
    pub media_size: Option<i64>,
    pub media_duration: Option<i64>,
    pub thumbnail: Option<Vec<u8>>,
    pub local_path: Option<String>,
    /// Opaque serialized protobuf owned by the engine.
    pub raw: Option<Vec<u8>>,
    /// Link preview card of a text message (all `None` without one).
    pub link_url: Option<String>,
    pub link_title: Option<String>,
    pub link_desc: Option<String>,
    pub link_thumb: Option<Vec<u8>>,
    /// An incoming message that mentions our own account.
    pub mentions_me: bool,
    /// Coordinates of a shared location.
    pub geo_lat: Option<f64>,
    pub geo_lon: Option<f64>,
    /// Unix seconds the message disappears at (disappearing messages).
    pub expires_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct MessageRow {
    pub chat_jid: String,
    pub id: String,
    pub sender_jid: String,
    pub sender_name: Option<String>,
    pub from_me: bool,
    pub ts: i64,
    pub kind: MessageKind,
    pub text: Option<String>,
    pub quoted_id: Option<String>,
    pub quoted_preview: Option<String>,
    pub media_mime: Option<String>,
    pub media_name: Option<String>,
    pub media_size: Option<i64>,
    pub media_duration: Option<i64>,
    pub thumbnail: Option<Vec<u8>>,
    pub local_path: Option<String>,
    pub status: MessageStatus,
    /// Deleted for everyone by the sender; content has been cleared.
    pub revoked: bool,
    /// Text was replaced by a later edit.
    pub edited: bool,
    /// Link preview card of a text message (all `None` without one).
    pub link_url: Option<String>,
    pub link_title: Option<String>,
    pub link_desc: Option<String>,
    pub link_thumb: Option<Vec<u8>>,
    /// Starred by the user (on any device).
    pub starred: bool,
    /// The message mentions our own account.
    pub mentions_me: bool,
    /// Coordinates of a shared location.
    pub geo_lat: Option<f64>,
    pub geo_lon: Option<f64>,
    /// Unix seconds the message disappears at (disappearing messages).
    pub expires_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatRow {
    pub jid: String,
    pub kind: ChatKind,
    pub display_name: String,
    pub last_ts: i64,
    pub unread: i64,
    pub pinned: bool,
    pub archived: bool,
    pub muted_until: i64,
    pub last_preview: Option<String>,
    /// Never send read receipts for this chat automatically.
    pub receipts_off: bool,
    /// Disappearing-message timer in seconds; 0 = off.
    pub ephemeral_secs: i64,
    /// Unix seconds the timer setting was made at (orders concurrent changes).
    pub ephemeral_ts: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContactPatch {
    pub jid: String,
    pub full_name: Option<String>,
    pub push_name: Option<String>,
    pub phone: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContactRow {
    pub jid: String,
    pub full_name: Option<String>,
    pub push_name: Option<String>,
    pub phone: Option<String>,
    pub avatar_path: Option<String>,
    pub avatar_id: Option<String>,
    /// Unix seconds of the last profile-picture lookup; 0 = never.
    pub avatar_checked_at: i64,
    /// The contact's "about" text, as last seen.
    pub about: Option<String>,
    /// Unix seconds the about text was set, when known.
    pub about_set_at: Option<i64>,
}

/// Read-only poll as shown in a chat.
#[derive(Debug, Clone, PartialEq)]
pub struct PollView {
    pub question: String,
    /// `(option name, votes, voted by us)` in the poll's own order.
    pub options: Vec<(String, u32, bool)>,
    pub selectable_count: u32,
}

/// Statuses of one poster, oldest item first.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusGroup {
    pub sender_jid: String,
    pub sender_name: Option<String>,
    pub items: Vec<MessageRow>,
    /// Every item was viewed (our own statuses count as viewed).
    pub all_viewed: bool,
}

/// Result of [`crate::Store::purge_statuses`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct StatusPurge {
    pub removed: usize,
    /// Cached media files of the removed statuses (for the caller to delete).
    pub media_paths: Vec<String>,
}

/// Which kind of receipt a recipient sent for one of our messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiptKind {
    Delivered,
    Read,
    Played,
}

/// Receipt times (unix seconds) of one recipient for one of our messages.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReceiptRow {
    pub jid: String,
    pub delivered_at: Option<i64>,
    pub read_at: Option<i64>,
    pub played_at: Option<i64>,
}
