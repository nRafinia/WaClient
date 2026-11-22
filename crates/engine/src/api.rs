//! Public contract between the engine and the UI.
//!
//! The UI never touches whatsapp-rust directly: it sends [`Command`]s through
//! [`EngineHandle`], receives [`UiEvent`]s on a channel, and reads chats and
//! messages from the shared [`wa_store::Store`].

use std::path::PathBuf;
use std::sync::Arc;

pub use wa_store::{MessageStatus, PollView, StatusGroup, Store, STATUS_JID};

/// Where the engine keeps its files.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Root data directory, e.g. `%APPDATA%\WaClient`. Contains
    /// `session.db` (protocol state), `messages.db` (our store) and `media/`.
    pub data_dir: PathBuf,
    /// Route all WhatsApp traffic (chat socket, media, link previews,
    /// avatars) through this proxy. Read once when the engine starts: changing
    /// it requires restarting the engine.
    pub proxy: Option<ProxyConfig>,
}

/// Which protocol a proxy speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyKind {
    /// SOCKS5; host names are resolved by the proxy, never locally.
    Socks5,
    /// HTTP proxy used through `CONNECT` tunnels.
    Http,
}

/// A user-configured proxy. `Debug` never prints the password.
#[derive(Clone, PartialEq, Eq)]
pub struct ProxyConfig {
    pub kind: ProxyKind,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl std::fmt::Debug for ProxyConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyConfig")
            .field("kind", &self.kind)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl EngineConfig {
    pub fn session_db(&self) -> PathBuf {
        self.data_dir.join("session.db")
    }
    pub fn messages_db(&self) -> PathBuf {
        self.data_dir.join("messages.db")
    }
    pub fn media_dir(&self) -> PathBuf {
        self.data_dir.join("media")
    }
}

/// User preferences the engine acts on immediately. Persisting them is up to
/// the UI: start the engine with [`crate::start_with_settings`] and the saved
/// settings so no message is handled under the defaults, then send
/// [`Command::UpdateSettings`] on every change. ([`crate::start`] uses the
/// defaults until the first `UpdateSettings`.)
#[derive(Debug, Clone, PartialEq)]
pub struct EngineSettings {
    /// Download incoming photos and stickers on arrival.
    pub auto_download_images: bool,
    /// Download incoming voice notes on arrival.
    pub auto_download_voice: bool,
    /// Download incoming documents on arrival.
    pub auto_download_documents: bool,
    /// Largest file fetched automatically, in MB (capped at 100 by the engine).
    pub auto_download_max_mb: u32,
    /// Send read receipts for chats. When off, opening a chat only resets the
    /// local unread counter (see [`Command::SendReadReceiptsNow`]).
    pub send_read_receipts: bool,
    /// Report that you viewed a status. Independent of `send_read_receipts`.
    pub send_status_receipts: bool,
    /// Send "typing..." indicators.
    pub send_typing: bool,
    /// Emit [`UiEvent::Notify`] for incoming messages.
    pub notifications: bool,
    /// "Show me as online": announce presence when connecting. When off, the
    /// engine stays unavailable (and turns it off right away if it is
    /// switched off while connected).
    ///
    /// Limitation: whatsapp-rust 0.7 has no switch for its own automatic
    /// `available` presence (on every reconnect before `Connected`, after the
    /// first push-name sync, and on a push-name change). The engine answers
    /// each with an immediate `unavailable`, but the server can see a brief
    /// "available" stanza right after connecting that cannot be avoided.
    pub send_presence: bool,
    /// Attach a link preview to outgoing text. Off by default: building one
    /// makes this device fetch the linked page (and its image) directly, which
    /// reveals the user's IP address to that site. The fetch is capped at 3 s;
    /// on any failure the text is sent plain.
    pub send_link_previews: bool,
}

impl Default for EngineSettings {
    fn default() -> Self {
        EngineSettings {
            auto_download_images: true,
            auto_download_voice: true,
            auto_download_documents: false,
            auto_download_max_mb: 16,
            send_read_receipts: true,
            send_status_receipts: true,
            send_typing: true,
            notifications: true,
            send_presence: true,
            send_link_previews: false,
        }
    }
}

/// What the attachment being sent is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutgoingMediaKind {
    Image,
    Video,
    Document,
    /// Voice note (push-to-talk). File must be OGG/Opus.
    Voice,
    Audio,
}

/// One contact card to share with [`Command::SendContact`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SharedContact {
    /// Display name shown on the card.
    pub name: String,
    /// Phone numbers of the card (digits, optionally with a leading "+").
    pub phones: Vec<String>,
}

/// Outcome of looking up one phone number ([`Command::LookupPhones`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PhoneLookupResult {
    /// The number as the UI passed it in.
    pub number: String,
    /// The WhatsApp account of the number; `None` when it is not on WhatsApp.
    pub jid: Option<String>,
    pub is_business: bool,
    /// Verified business name, when the account has one.
    pub verified_name: Option<String>,
}

/// Who gets to see a posted status.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum StatusAudience {
    /// All of our contacts.
    #[default]
    MyContacts,
    /// All contacts except these JIDs.
    Except(Vec<String>),
    /// Only these JIDs.
    OnlyShareWith(Vec<String>),
}

/// Requests from the UI to the engine. Fire-and-forget; results arrive as
/// [`UiEvent`]s and as store changes.
#[derive(Debug, Clone)]
pub enum Command {
    SendText {
        chat_jid: String,
        text: String,
        /// Message id (in the same chat) being replied to.
        quoted_id: Option<String>,
        /// JIDs mentioned with "@" in a group (sent as contextInfo.mentionedJid;
        /// the text should contain "@<digits>" for each).
        mentions: Vec<String>,
    },
    SendMedia {
        chat_jid: String,
        path: PathBuf,
        kind: OutgoingMediaKind,
        caption: Option<String>,
        quoted_id: Option<String>,
        /// Send as a view-once message (the recipient can open it only once).
        view_once: bool,
    },
    /// Share one or more contact cards in a chat.
    SendContact {
        chat_jid: String,
        contacts: Vec<SharedContact>,
        /// Message id (in the same chat) being replied to.
        quoted_id: Option<String>,
    },
    /// Check which phone numbers are on WhatsApp (and whether they are
    /// business accounts). Answered with [`UiEvent::PhoneLookup`].
    LookupPhones { numbers: Vec<String> },
    /// Share a location (map pin) in a chat.
    SendLocation {
        chat_jid: String,
        lat: f64,
        lon: f64,
        /// Place name, when the user picked a named place.
        name: Option<String>,
        address: Option<String>,
        /// Message id (in the same chat) being replied to.
        quoted_id: Option<String>,
    },
    /// Post a text status. `background_argb` is the 0xAARRGGBB background
    /// colour and `font` the WhatsApp font index. Answered with
    /// [`UiEvent::StatusPosted`].
    PostTextStatus {
        text: String,
        background_argb: u32,
        font: u8,
        audience: StatusAudience,
    },
    /// Post an image or video status from a file. Answered with
    /// [`UiEvent::StatusPosted`].
    PostMediaStatus {
        path: PathBuf,
        caption: Option<String>,
        audience: StatusAudience,
    },
    /// Set the disappearing-messages timer of one chat; `secs` = 0 turns it
    /// off (WhatsApp offers 24h / 7 days / 90 days).
    SetChatTimer { chat_jid: String, secs: u32 },
    /// Set the default disappearing-messages timer for new chats; 0 = off.
    SetDefaultTimer { secs: u32 },
    /// Load the default disappearing-messages timer. Answered with
    /// [`UiEvent::DefaultTimer`].
    FetchDefaultTimer,
    /// Send read receipts for all unread incoming messages in the chat and
    /// reset its unread counter.
    MarkChatRead { chat_jid: String },
    /// Allow (`enabled = true`, the default) or suppress automatic read
    /// receipts for one chat. Local only, no network. With `enabled = false`,
    /// `MarkChatRead` only resets the local unread counter. Emits
    /// `ChatsChanged`.
    SetChatReceipts { chat_jid: String, enabled: bool },
    /// "Mark as read now": explicit user action that sends read receipts for
    /// the incoming messages of the last 7 days (at most 200) no receipt went
    /// out for yet, regardless of the global and per-chat settings, then
    /// resets the chat's unread counter. Emits `ChatsChanged`.
    ///
    /// Caveat: in a direct chat the sender only sees blue ticks if the
    /// account-level WhatsApp privacy setting "Read receipts" is on; when it
    /// is off the library sends a `read-self` receipt, which stays silent for
    /// the sender. Status receipts are not affected by that account setting
    /// and follow only [`EngineSettings::send_status_receipts`].
    SendReadReceiptsNow { chat_jid: String },
    /// Archive or unarchive a chat; syncs to the account's other devices via
    /// app state; emits `ChatsChanged`.
    SetArchived { chat_jid: String, archived: bool },
    /// Pin or unpin a chat (at most 3 pinned; archived chats cannot be pinned);
    /// syncs via app state; emits `ChatsChanged`.
    SetPinned { chat_jid: String, pinned: bool },
    /// Mute a chat until `until` (unix seconds; `i64::MAX` = always) or unmute
    /// it with `None`; syncs via app state; emits `ChatsChanged`.
    SetMuted { chat_jid: String, until: Option<i64> },
    /// Typing indicator; `composing = false` sends "paused".
    SetTyping { chat_jid: String, composing: bool },
    /// React to a message with `emoji`; an empty emoji removes our reaction.
    /// The reaction is stored locally right away ([`UiEvent::MessagesChanged`])
    /// and reverted with a [`UiEvent::Error`] if sending fails.
    SendReaction {
        chat_jid: String,
        /// Id of the message reacted to.
        id: String,
        emoji: String,
    },
    /// Mark a status update as viewed (local, always) and, when
    /// `send_status_receipts` is on, tell the poster. The poster is taken from
    /// the stored status; `sender_jid` is ignored (kept for compatibility) so
    /// a wrong value can never address the receipt to someone else. Unknown
    /// ids do nothing. Emits [`UiEvent::StatusesChanged`].
    MarkStatusViewed { sender_jid: String, id: String },
    /// Replace the engine settings; applies immediately to everything that
    /// happens afterwards.
    UpdateSettings(EngineSettings),
    /// Download (and decrypt) the media of a stored message into the media
    /// cache. Statuses are never downloaded automatically; pass
    /// [`STATUS_JID`] as `chat_jid` to fetch one on request. Emits
    /// [`UiEvent::MediaReady`] or [`UiEvent::MediaFailed`].
    DownloadMedia { chat_jid: String, id: String },
    /// Fetch (or refresh) the profile picture. Emits [`UiEvent::AvatarReady`].
    FetchAvatar { jid: String },
    /// Unlink this device and wipe the local session.
    Logout,
    /// Start pairing again with a fresh QR code. The engine restarts pairing
    /// by itself only a few times in a row when QR codes expire unscanned;
    /// after that it emits [`UiEvent::Error`] ("QR code expired") and waits for
    /// this command. Also usable to retry after the connection ended.
    RetryPairing,
    /// Rebuild the connection with the existing session. Only acted on while
    /// the engine is stopped and recoverable (the session was replaced by
    /// another client, or the bot ended by itself); ignored while a bot is
    /// running. Never accepted after a temporary ban or an outdated-client
    /// rejection: reconnecting then risks the account.
    ///
    /// UI: show a "Reconnect" button for `ConnState::Disconnected` states
    /// (including the "session replaced" one) and send this command.
    Reconnect,
    /// Delete one message. `for_everyone = false`: remove it locally and sync
    /// "delete for me" to the account's other devices when supported.
    /// `for_everyone = true`: only for our own messages; sends a revoke so it
    /// shows "This message was deleted" for everyone (WhatsApp enforces a
    /// time limit; on rejection the engine emits [`UiEvent::Error`]).
    /// Emits `MessagesChanged` + `ChatsChanged` on success.
    DeleteMessage {
        chat_jid: String,
        id: String,
        for_everyone: bool,
    },
    /// Load everything shown in the contact / group info panel. Emits
    /// [`UiEvent::ContactInfo`] (possibly twice: first from local data
    /// immediately, then again after network lookups complete).
    FetchContactInfo { jid: String },
    /// Ask for a contact's online / last-seen state. Call when a DM chat is
    /// opened or visible, or when the contact info panel opens; groups are
    /// ignored. Answered with [`UiEvent::Presence`] (immediately from cache
    /// when known, else when WhatsApp reports it). Repeats within 5 minutes
    /// are not sent again.
    SubscribePresence { jid: String },
    /// Edit one of our own text messages. WhatsApp only accepts edits within
    /// its server-side window (about 15 minutes); a rejection arrives as
    /// [`UiEvent::Error`]. Emits `MessagesChanged` on success.
    EditMessage { chat_jid: String, id: String, text: String },
    /// Forward stored messages (text and media; media reused without
    /// re-uploading when possible) to one or more chats, in the given order.
    /// Marked as "Forwarded". Emits `MessagesChanged`/`ChatsChanged` per target.
    ForwardMessages { from_chat: String, ids: Vec<String>, to_chats: Vec<String> },
    /// Pin or unpin a message for everyone in the chat ("pin in chat").
    /// `duration_secs` is one of WhatsApp's choices (24h / 7 days / 30 days);
    /// ignored when unpinning. Emits `MessagesChanged` + `ChatsChanged`.
    PinMessage { chat_jid: String, id: String, pin: bool, duration_secs: u32 },
    /// Star / unstar a message (synced to the account's other devices).
    StarMessage { chat_jid: String, id: String, starred: bool },
    /// Load delivery/read/played receipts per recipient for one of our
    /// messages. Answered with [`UiEvent::MessageInfo`].
    FetchMessageInfo { chat_jid: String, id: String },
    /// Mark a chat as unread (synced via app state). Emits `ChatsChanged`.
    MarkChatUnread { chat_jid: String },
    /// Clear all messages of a chat for this account (synced via app state).
    /// Starred messages are kept when `keep_starred`.
    ClearChat { chat_jid: String, keep_starred: bool },
    /// Block or unblock a contact. Emits [`UiEvent::Blocklist`].
    SetBlocked { jid: String, blocked: bool },
    /// Load the account's block list. Answered with [`UiEvent::Blocklist`].
    FetchBlocklist,
    /// Change our own profile name (push name).
    SetOwnName { name: String },
    /// Change our own "About" text.
    SetOwnAbout { text: String },
    /// Set (Some(path to a JPEG/PNG)) or remove (None) our profile picture.
    SetOwnPicture { path: Option<PathBuf> },
    /// Load our own profile (name, about, picture, phone). Answered with
    /// [`UiEvent::OwnProfile`].
    FetchOwnProfile,
    // ---- CALLS ----
    /// Decline a ringing incoming call (from [`UiEvent::IncomingCall`]). The
    /// caller sees it as declined; unknown or already-ended calls are ignored.
    RejectCall { call_id: String },
    // ---- POLLS ----
    /// Vote on a stored poll with the given option names (they must be names
    /// of the poll's options; at most its selectable count). An empty list
    /// withdraws our vote. Our vote is stored right away
    /// ([`UiEvent::MessagesChanged`]) and reverted with a [`UiEvent::Error`]
    /// if sending fails.
    VotePoll { chat_jid: String, poll_id: String, options: Vec<String> },
    /// Create and send a poll (2..=12 distinct non-empty options).
    /// `multi = false` allows one answer, `true` any number. Stored like any
    /// sent message, so its votes can be decrypted later.
    CreatePoll { chat_jid: String, question: String, options: Vec<String>, multi: bool },
    // ---- GROUPS ----
    /// Create a group with us as admin. `participants` are user JIDs;
    /// `picture` an optional JPEG/PNG for the group photo. Answered with
    /// [`UiEvent::GroupCreated`] (or [`UiEvent::GroupCreateFailed`]).
    CreateGroup { subject: String, participants: Vec<String>, picture: Option<PathBuf> },
    /// Add / remove / promote / demote members (we must be an admin).
    /// Answered with [`UiEvent::GroupMembersResult`]; refreshes the info
    /// panel with a new [`UiEvent::ContactInfo`].
    UpdateGroupMembers { group_jid: String, jids: Vec<String>, action: MemberAction },
    /// Rename the group (1..=100 characters).
    SetGroupSubject { group_jid: String, subject: String },
    /// Set (Some) or clear (None) the group description.
    SetGroupDescription { group_jid: String, description: Option<String> },
    /// Set (Some(path to a JPEG/PNG)) or remove (None) the group photo.
    SetGroupPicture { group_jid: String, path: Option<PathBuf> },
    /// Change one admin setting of the group.
    SetGroupSetting { group_jid: String, setting: GroupSetting },
    /// Exit the group. The chat and its messages stay locally.
    LeaveGroup { group_jid: String },
    /// Load (or with `reset`, revoke and replace) the group's invite link.
    /// Answered with [`UiEvent::InviteLink`].
    FetchInviteLink { group_jid: String, reset: bool },
    /// Look up a `https://chat.whatsapp.com/<code>` link (or bare code)
    /// without joining. Answered with [`UiEvent::InvitePreview`].
    PreviewInvite { link: String },
    /// Join a group via an invite link (or bare code). Answered with
    /// [`UiEvent::GroupJoined`] (or [`UiEvent::GroupJoinFailed`]).
    JoinGroup { link: String },
    /// Disconnect cleanly and stop the engine.
    Shutdown,
}

impl Command {
    /// One-line description for the log: the variant name and, where there is
    /// one, the redacted chat/contact address. Never message content or paths.
    pub fn log_line(&self) -> String {
        use crate::jid::redact;
        let (name, jid): (&str, Option<&str>) = match self {
            Command::SendText { chat_jid, .. } => ("SendText", Some(chat_jid)),
            Command::SendMedia { chat_jid, .. } => ("SendMedia", Some(chat_jid)),
            Command::SendContact { chat_jid, .. } => ("SendContact", Some(chat_jid)),
            Command::LookupPhones { .. } => ("LookupPhones", None),
            Command::SendLocation { chat_jid, .. } => ("SendLocation", Some(chat_jid)),
            Command::PostTextStatus { .. } => ("PostTextStatus", None),
            Command::PostMediaStatus { .. } => ("PostMediaStatus", None),
            Command::SetChatTimer { chat_jid, .. } => ("SetChatTimer", Some(chat_jid)),
            Command::SetDefaultTimer { .. } => ("SetDefaultTimer", None),
            Command::FetchDefaultTimer => ("FetchDefaultTimer", None),
            Command::MarkChatRead { chat_jid } => ("MarkChatRead", Some(chat_jid)),
            Command::SetChatReceipts { chat_jid, .. } => ("SetChatReceipts", Some(chat_jid)),
            Command::SendReadReceiptsNow { chat_jid } => ("SendReadReceiptsNow", Some(chat_jid)),
            Command::SetArchived { chat_jid, .. } => ("SetArchived", Some(chat_jid)),
            Command::SetPinned { chat_jid, .. } => ("SetPinned", Some(chat_jid)),
            Command::SetMuted { chat_jid, .. } => ("SetMuted", Some(chat_jid)),
            Command::SetTyping { chat_jid, .. } => ("SetTyping", Some(chat_jid)),
            Command::SendReaction { chat_jid, .. } => ("SendReaction", Some(chat_jid)),
            Command::MarkStatusViewed { .. } => ("MarkStatusViewed", None),
            Command::UpdateSettings(_) => ("UpdateSettings", None),
            Command::DownloadMedia { chat_jid, .. } => ("DownloadMedia", Some(chat_jid)),
            Command::FetchAvatar { jid } => ("FetchAvatar", Some(jid)),
            Command::Logout => ("Logout", None),
            Command::RetryPairing => ("RetryPairing", None),
            Command::Reconnect => ("Reconnect", None),
            Command::DeleteMessage { chat_jid, .. } => ("DeleteMessage", Some(chat_jid)),
            Command::FetchContactInfo { jid } => ("FetchContactInfo", Some(jid)),
            Command::SubscribePresence { jid } => ("SubscribePresence", Some(jid)),
            Command::EditMessage { chat_jid, .. } => ("EditMessage", Some(chat_jid)),
            Command::ForwardMessages { from_chat, .. } => ("ForwardMessages", Some(from_chat)),
            Command::PinMessage { chat_jid, .. } => ("PinMessage", Some(chat_jid)),
            Command::StarMessage { chat_jid, .. } => ("StarMessage", Some(chat_jid)),
            Command::FetchMessageInfo { chat_jid, .. } => ("FetchMessageInfo", Some(chat_jid)),
            Command::MarkChatUnread { chat_jid } => ("MarkChatUnread", Some(chat_jid)),
            Command::ClearChat { chat_jid, .. } => ("ClearChat", Some(chat_jid)),
            Command::SetBlocked { jid, .. } => ("SetBlocked", Some(jid)),
            Command::FetchBlocklist => ("FetchBlocklist", None),
            Command::SetOwnName { .. } => ("SetOwnName", None),
            Command::SetOwnAbout { .. } => ("SetOwnAbout", None),
            Command::SetOwnPicture { .. } => ("SetOwnPicture", None),
            Command::FetchOwnProfile => ("FetchOwnProfile", None),
            Command::RejectCall { .. } => ("RejectCall", None),
            Command::VotePoll { chat_jid, .. } => ("VotePoll", Some(chat_jid)),
            Command::CreatePoll { chat_jid, .. } => ("CreatePoll", Some(chat_jid)),
            Command::CreateGroup { .. } => ("CreateGroup", None),
            Command::UpdateGroupMembers { group_jid, .. } => ("UpdateGroupMembers", Some(group_jid)),
            Command::SetGroupSubject { group_jid, .. } => ("SetGroupSubject", Some(group_jid)),
            Command::SetGroupDescription { group_jid, .. } => ("SetGroupDescription", Some(group_jid)),
            Command::SetGroupPicture { group_jid, .. } => ("SetGroupPicture", Some(group_jid)),
            Command::SetGroupSetting { group_jid, .. } => ("SetGroupSetting", Some(group_jid)),
            Command::LeaveGroup { group_jid } => ("LeaveGroup", Some(group_jid)),
            Command::FetchInviteLink { group_jid, .. } => ("FetchInviteLink", Some(group_jid)),
            // Invite codes grant access to the group: never log them.
            Command::PreviewInvite { .. } => ("PreviewInvite", None),
            Command::JoinGroup { .. } => ("JoinGroup", None),
            Command::Shutdown => ("Shutdown", None),
        };
        match jid {
            Some(j) => format!("{name} {}", redact(j)),
            None => name.to_string(),
        }
    }
}

/// Connection state shown in the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnState {
    Connecting,
    Connected,
    Disconnected { reason: String },
}

/// Notifications from the engine to the UI.
#[derive(Debug, Clone)]
pub enum UiEvent {
    /// Not paired yet: show this QR payload (render it as a QR image).
    Qr { code: String },
    /// Pairing finished; `jid` is our own account.
    Paired { jid: String },
    /// The session is registered and the engine is (re)connecting; skip the
    /// QR screen.
    SessionRestored,
    Connection(ConnState),
    /// Session ended (by us or from the phone). The UI should go back to the
    /// QR screen; the engine restarts pairing by itself (see
    /// [`Command::RetryPairing`] for when it gives up).
    LoggedOut { reason: String },
    /// The chat list changed (new chat, name, unread, last message...).
    ChatsChanged,
    /// Status updates were added, viewed or purged; reload
    /// [`Store::statuses_since`]. Coalesced like `ChatsChanged`.
    StatusesChanged,
    /// Result of [`Command::PostTextStatus`] / [`Command::PostMediaStatus`];
    /// `message` carries the failure reason when `ok` is false.
    StatusPosted { ok: bool, message: Option<String> },
    /// Reply to [`Command::LookupPhones`], one result per requested number.
    PhoneLookup { results: Vec<PhoneLookupResult> },
    /// Reply to [`Command::FetchDefaultTimer`]; 0 = off.
    DefaultTimer { secs: u32 },
    /// Data for the contact / group info panel (reply to
    /// [`Command::FetchContactInfo`]).
    ContactInfo(ContactInfo),
    /// A message was inserted or changed in this chat (new message, edit,
    /// delete, reaction, poll vote); reload it if open.
    MessagesChanged { chat_jid: String },
    MessageStatus {
        chat_jid: String,
        id: String,
        status: MessageStatus,
    },
    /// Someone is typing (or stopped) in a chat.
    Typing {
        chat_jid: String,
        sender_jid: String,
        composing: bool,
    },
    /// A new incoming live message that deserves a desktop notification.
    Notify {
        chat_jid: String,
        title: String,
        body: String,
    },
    /// History sync progress, 0..=100.
    HistoryProgress { percent: u32 },
    MediaReady {
        chat_jid: String,
        id: String,
        path: PathBuf,
    },
    MediaFailed {
        chat_jid: String,
        id: String,
        error: String,
    },
    AvatarReady { jid: String, path: PathBuf },
    /// WhatsApp temporarily banned this account / the client is outdated /
    /// the server refused the connection for a non-retryable reason. The
    /// engine has stopped connecting and will not retry on its own; the user
    /// can only wait or quit (offer no Reconnect). Show it prominently.
    Fatal { message: String },
    /// A contact came online / went offline (reply to
    /// [`Command::SubscribePresence`]). `last_seen` is unix seconds; `None`
    /// when hidden by privacy settings (the contact's, or ours) or unknown.
    Presence {
        jid: String,
        online: bool,
        last_seen: Option<i64>,
    },
    /// Reply to [`Command::FetchMessageInfo`].
    MessageInfo { chat_jid: String, id: String, receipts: Vec<ReceiptEntry> },
    /// Current block list (reply to [`Command::FetchBlocklist`] and after
    /// [`Command::SetBlocked`]).
    Blocklist { jids: Vec<String> },
    /// Our own profile (reply to [`Command::FetchOwnProfile`] and after a
    /// successful SetOwn* change).
    OwnProfile(OwnProfile),
    /// Someone is calling us (voice or video). Calls can't be answered in
    /// this app: show "answer on your phone" with a Decline button
    /// ([`Command::RejectCall`]). Followed by [`UiEvent::CallEnded`].
    IncomingCall {
        call_id: String,
        /// Chat the call belongs to: the caller's DM chat, or the group for
        /// group calls.
        chat_jid: String,
        /// Best display name of the caller (address book → push name → phone).
        caller_name: String,
        is_video: bool,
        is_group: bool,
    },
    /// A ringing call stopped ringing (any reason); hide its notice.
    CallEnded { call_id: String, reason: CallEndReason },
    /// Declining a call failed ([`Command::RejectCall`]); the call is still
    /// ringing, so the Decline button should work again. Accompanied by a
    /// [`UiEvent::Error`].
    CallDeclineFailed { call_id: String },
    /// Reply to [`Command::CreateGroup`]: the new group's JID (its chat row
    /// exists in the store when this arrives). `failed` lists participants
    /// the server did not add (e.g. privacy settings), empty when unknown.
    GroupCreated { group_jid: String, failed: Vec<MemberChangeResult> },
    /// Reply to [`Command::UpdateGroupMembers`], one entry per requested JID.
    GroupMembersResult { group_jid: String, action: MemberAction, results: Vec<MemberChangeResult> },
    /// Reply to [`Command::FetchInviteLink`]: full `https://chat.whatsapp.com/…` link.
    InviteLink { group_jid: String, link: String },
    /// Reply to [`Command::PreviewInvite`].
    InvitePreview(InvitePreview),
    /// Reply to [`Command::JoinGroup`]. `pending` = an admin must approve us
    /// first (the chat appears once approved).
    GroupJoined { group_jid: String, pending: bool },
    /// Reply to [`Command::CreateGroup`] when the group was not created.
    GroupCreateFailed { message: String },
    /// Reply to [`Command::JoinGroup`] when joining failed; `link` is the
    /// link exactly as the command carried it.
    GroupJoinFailed { link: String, message: String },
    /// Non-fatal error worth surfacing (e.g. a send failed).
    Error { message: String },
}

/// Per-recipient receipt times (unix seconds) for one of our messages.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReceiptEntry {
    pub jid: String,
    pub delivered_at: Option<i64>,
    pub read_at: Option<i64>,
    pub played_at: Option<i64>,
}

/// Our own account profile.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OwnProfile {
    pub jid: String,
    pub phone: Option<String>,
    pub name: Option<String>,
    pub about: Option<String>,
    pub picture_path: Option<PathBuf>,
}

/// Cheap, cloneable handle to a running engine.
#[derive(Clone)]
pub struct EngineHandle {
    pub(crate) cmd_tx: tokio::sync::mpsc::UnboundedSender<Command>,
    pub(crate) store: Arc<Store>,
    pub(crate) config: EngineConfig,
}

impl EngineHandle {
    /// Queue a command. Never blocks; silently ignored after shutdown.
    pub fn send(&self, cmd: Command) {
        let _ = self.cmd_tx.send(cmd);
    }

    /// Shared message store (read it from the UI thread; it's synchronous and
    /// cheap for paged queries).
    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    pub fn config(&self) -> &EngineConfig {
        &self.config
    }
}

/// Receiver side for UI events. Use `blocking_recv` from a dedicated thread
/// and forward to the UI with `slint::invoke_from_event_loop`.
pub type UiEventReceiver = tokio::sync::mpsc::UnboundedReceiver<UiEvent>;

/// Contact or group details for the info panel.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ContactInfo {
    pub jid: String,
    /// Address-book name (app-state contact sync), if known.
    pub name: Option<String>,
    /// The name the user set on their own WhatsApp profile.
    pub push_name: Option<String>,
    /// E.164 phone number without '+', when known (PN jid or LID mapping).
    pub phone: Option<String>,
    /// "About" text and when it was set (unix seconds).
    pub about: Option<String>,
    pub about_set_at: Option<i64>,
    /// Full-resolution profile picture in the media cache, if available.
    pub picture_path: Option<std::path::PathBuf>,
    pub is_business: bool,
    /// WhatsApp Business profile details (only when `is_business` and the
    /// server returned them).
    pub business: Option<BusinessProfile>,
    /// `true` when `phone`/`about`/`picture_path` came from the network in
    /// this reply (the second emission); `false` for the quick local reply.
    pub complete: bool,
    /// Present for groups.
    pub group: Option<GroupInfo>,
}

/// Group details for the info panel.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GroupInfo {
    pub subject: String,
    pub description: Option<String>,
    /// Unix seconds.
    pub created_at: Option<i64>,
    pub creator_jid: Option<String>,
    pub participants: Vec<GroupMember>,
    /// "Only admins can send messages".
    pub announce_only: bool,
    /// "Only admins can edit group info".
    pub locked: bool,
    /// New members need admin approval.
    pub approval_required: bool,
    /// We are an admin (or the super admin) of this group.
    pub i_am_admin: bool,
    /// We are still a participant (false after leaving or being removed).
    pub i_am_member: bool,
}

/// Why a ringing call stopped ringing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallEndReason {
    /// The caller hung up or it timed out (explicit reason): a missed call.
    Missed,
    /// We declined it here.
    Declined,
    /// Answered on another of our devices (e.g. the phone).
    AnsweredElsewhere,
    /// Declined on another of our devices.
    DeclinedElsewhere,
    /// Stopped ringing, outcome unknown (expired, session ended, or an
    /// unrecognised hang-up reason): no missed-call row.
    Ended,
}

/// Membership change requested with [`Command::UpdateGroupMembers`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberAction {
    Add,
    Remove,
    Promote,
    Demote,
}

/// Admin setting changed with [`Command::SetGroupSetting`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupSetting {
    /// "Only admins can send messages".
    AnnounceOnly(bool),
    /// "Only admins can edit group info".
    Locked(bool),
    /// "Approve new members".
    ApprovalRequired(bool),
}

/// Outcome of one membership change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberChangeResult {
    pub jid: String,
    pub ok: bool,
    /// Why it failed, in plain English for the UI ("Can't be added because
    /// of their privacy settings", "Already in the group", ...).
    pub error: Option<String>,
    /// Their privacy settings block direct adds; the UI can offer to send
    /// them the group's invite link in a private chat instead.
    pub invite_instead: bool,
}

/// What an invite link leads to (reply to [`Command::PreviewInvite`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InvitePreview {
    /// The link as given by the UI, so the reply can be matched.
    pub link: String,
    /// `None` when the link is invalid, expired or revoked (see `error`).
    pub subject: Option<String>,
    pub description: Option<String>,
    pub member_count: u32,
    /// We are already a member (the chat can just be opened).
    pub group_jid: Option<String>,
    pub already_member: bool,
    pub approval_required: bool,
    pub error: Option<String>,
}

/// One group participant.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GroupMember {
    pub jid: String,
    /// Best display name (address book → push name → phone).
    pub name: Option<String>,
    pub phone: Option<String>,
    pub is_admin: bool,
    pub is_super_admin: bool,
    pub is_me: bool,
}

/// WhatsApp Business profile shown in the info panel.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BusinessProfile {
    pub description: Option<String>,
    pub category: Option<String>,
    pub address: Option<String>,
    pub email: Option<String>,
    pub websites: Vec<String>,
    /// Human-readable opening hours lines, e.g. "Mon: 09:00–17:00".
    pub hours: Vec<String>,
}
