//! Message operations: edit, forward, pin, star, mentions, and per-recipient
//! receipts. The pure parts (checks, message building) are separate functions
//! so they can be unit-tested without a client.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use whatsapp_rust::PinDuration;
use whatsapp_rust::buffa::{Message as _, MessageField};
use whatsapp_rust::prelude::{Client, Jid, MessageExt as _, wa};
use whatsapp_rust::upload::UploadOptions;
use whatsapp_rust::wacore::types::events::StarUpdate;
use whatsapp_rust::wacore_binary::JidExt as _;
use wa_store::{MessageKind, MessageRow, MessageStatus, NewMessage, ReceiptKind, STATUS_JID, Store};

use crate::api::{ReceiptEntry, UiEvent};
use crate::commands::{
    KeyParticipant, NOT_LOGGED_IN, Prepared, app_state_failure, build_media_message, deliver, finish,
    guarded_revert, key_participant, log_flag_outcome, parse_jid, reject, remove_cached_file,
    resolve_participant, sender_forms, store_pending, with_app_state_timeout,
};
use crate::events::{Shared, now_secs, own_jid};
use crate::pending::{Pending, PendingAction, PendingActions};
use crate::jid::{self, redact, warn_lib_error};
use crate::{info, media as cache};

/// WhatsApp only accepts an edit this long after the message was sent.
const EDIT_MAX_AGE_SECS: i64 = 15 * 60;
/// Most messages / target chats of one forward command.
const FORWARD_MAX_MESSAGES: usize = 20;
const FORWARD_MAX_CHATS: usize = 5;
/// Uploaded media stays on WhatsApp's servers for a limited time; reusing the
/// stored upload of an older message would hand the recipients a dead link,
/// so those are downloaded and uploaded again instead.
pub(crate) const MEDIA_REUSE_MAX_AGE_SECS: i64 = 14 * 24 * 60 * 60;

const HOUR_SECS: u32 = 60 * 60;
const PIN_24H_SECS: u32 = 24 * HOUR_SECS;
const PIN_7D_SECS: u32 = 7 * PIN_24H_SECS;

// ---------------------------------------------------------------- edit

/// Local pre-checks of an edit; `Err` is the message for the UI. The server
/// enforces the time limit too, this only avoids a pointless round trip.
pub(crate) fn check_edit(
    from_me: bool,
    revoked: bool,
    kind: MessageKind,
    status: MessageStatus,
    ts: i64,
    now: i64,
    new_text: &str,
) -> Result<(), &'static str> {
    if !from_me {
        return Err("You can only edit your own messages");
    }
    if !matches!(
        status,
        MessageStatus::Sent | MessageStatus::Delivered | MessageStatus::Read | MessageStatus::Played
    ) {
        return Err("The message has not been sent");
    }
    if revoked {
        return Err("This message was deleted");
    }
    if !matches!(
        kind,
        MessageKind::Text | MessageKind::Image | MessageKind::Video | MessageKind::Document
    ) {
        return Err("Only text and captions can be edited");
    }
    if now.saturating_sub(ts) > EDIT_MAX_AGE_SECS {
        return Err("WhatsApp only allows editing within 15 minutes");
    }
    if kind == MessageKind::Text && new_text.trim().is_empty() {
        return Err("A message cannot be empty");
    }
    Ok(())
}

/// The replacement content of an edit. Text keeps the stored context (quote,
/// mentions); a caption edit is the stored media message with the new caption.
pub(crate) fn edited_content(
    stored: Option<&wa::Message>,
    kind: MessageKind,
    text: &str,
) -> Result<wa::Message, &'static str> {
    // Without the stored message the quote and mentions would be dropped.
    let stored = stored.ok_or("The original message data is not available")?;
    if kind == MessageKind::Text {
        let context = crate::convert::context_info_of(stored).cloned();
        return Ok(crate::convert::text_message(text, context));
    }
    let mut msg = stored.get_base_message().clone();
    msg.message_context_info = MessageField::none();
    let caption = (!text.is_empty()).then(|| text.to_string());
    let done = if let Some(m) = msg.image_message.as_option_mut() {
        m.caption = caption;
        true
    } else if let Some(m) = msg.video_message.as_option_mut() {
        m.caption = caption;
        true
    } else if let Some(m) = msg.document_message.as_option_mut() {
        m.caption = caption;
        true
    } else {
        false
    };
    if done { Ok(msg) } else { Err("This message has no caption to edit") }
}

fn load_row(sh: &Shared, chat_jid: &str, id: &str, what: &str) -> Option<MessageRow> {
    match sh.store.message(chat_jid, id) {
        Ok(Some(row)) => Some(row),
        Ok(None) => {
            reject(sh, format!("The message to {what} was not found"));
            None
        }
        Err(e) => {
            log::error!("message lookup failed: {e}");
            reject(sh, "Could not read the message".to_string());
            None
        }
    }
}

pub(crate) fn load_stored(store: &Store, chat_jid: &str, id: &str) -> Option<wa::Message> {
    let raw = store.message_raw(chat_jid, id).ok().flatten()?;
    wa::Message::decode_from_slice(&raw)
        .map_err(|e| log::warn!("stored message {id} does not decode: {e}"))
        .ok()
}

pub(crate) async fn edit_message(sh: &Shared, client: &Arc<Client>, chat_jid: &str, id: &str, text: &str) {
    let Some(row) = load_row(sh, chat_jid, id, "edit") else {
        return;
    };
    if let Err(msg) = check_edit(row.from_me, row.revoked, row.kind, row.status, row.ts, now_secs(), text) {
        reject(sh, msg.to_string());
        return;
    }
    let Some(chat) = parse_jid(sh, chat_jid) else {
        return;
    };
    let stored = load_stored(&sh.store, chat_jid, id);
    let content = match edited_content(stored.as_ref(), row.kind, text) {
        Ok(c) => c,
        Err(msg) => {
            reject(sh, msg.to_string());
            return;
        }
    };
    if let Err(e) = client.edit_message(chat, id.to_string(), content).await {
        warn_lib_error(&format!("editing message {id} failed"), &e);
        reject(sh, format!("Could not edit the message: {e}"));
        return;
    }
    if let Err(e) = sh.store.edit_message(chat_jid, id, text) {
        log::error!("storing edit failed: {e}");
        reject(sh, "Edited, but the local copy could not be updated".to_string());
    }
    sh.emit(UiEvent::MessagesChanged { chat_jid: chat_jid.to_string() });
    sh.emit(UiEvent::ChatsChanged);
}

// ------------------------------------------------------------- forward

/// Rejects forwards beyond what WhatsApp accepts in one go.
pub(crate) fn check_forward_limits(messages: usize, chats: usize) -> Result<(), String> {
    if messages == 0 || chats == 0 {
        return Err("Nothing to forward".to_string());
    }
    if messages > FORWARD_MAX_MESSAGES {
        return Err(format!("You can forward at most {FORWARD_MAX_MESSAGES} messages at once"));
    }
    if chats > FORWARD_MAX_CHATS {
        return Err(format!("You can forward to at most {FORWARD_MAX_CHATS} chats at once"));
    }
    Ok(())
}

/// Context of a forwarded message: marked as forwarded with the score one
/// above the original's. Quote and mentions of the original do not carry over.
pub(crate) fn forward_context(previous_score: u32) -> wa::ContextInfo {
    wa::ContextInfo {
        is_forwarded: Some(true),
        forwarding_score: Some(previous_score.saturating_add(1)),
        ..Default::default()
    }
}

/// Runs `$body` on whichever media sub-message `$base` carries (all of them
/// share the fields used by the callers); `None` for non-media messages.
macro_rules! with_media {
    ($base:expr, $method:ident, |$m:ident| $body:expr) => {
        if let Some($m) = $base.image_message.$method() {
            Some($body)
        } else if let Some($m) = $base.video_message.$method() {
            Some($body)
        } else if let Some($m) = $base.ptv_message.$method() {
            Some($body)
        } else if let Some($m) = $base.audio_message.$method() {
            Some($body)
        } else if let Some($m) = $base.document_message.$method() {
            Some($body)
        } else if let Some($m) = $base.sticker_message.$method() {
            Some($body)
        } else {
            None
        }
    };
}

/// Whether the stored upload of this media message can be sent again as is.
pub(crate) fn media_reusable(base: &wa::Message) -> bool {
    let filled = |s: &Option<String>| s.as_deref().is_some_and(|s| !s.is_empty());
    let bytes = |b: &Option<Vec<u8>>| b.as_deref().is_some_and(|b| !b.is_empty());
    with_media!(base, as_option, |m| {
        (filled(&m.direct_path) || filled(&m.url))
            && bytes(&m.media_key)
            && bytes(&m.file_sha256)
            && bytes(&m.file_enc_sha256)
    })
    .unwrap_or(false)
}

/// Whether a stored upload is still young enough to be sent again. Its age is
/// counted from the media's own upload time (`media_key_timestamp`, kept by
/// every reused copy), not from the message row: a copy re-sent late gets a new
/// row but still points at the old upload. The row time is only the fallback.
pub(crate) fn upload_fresh(base: Option<&wa::Message>, row_ts: i64, now: i64) -> bool {
    let uploaded = base
        .and_then(|b| with_media!(b, as_option, |m| m.media_key_timestamp).flatten())
        .filter(|ts| *ts > 0)
        .unwrap_or(row_ts);
    now.saturating_sub(uploaded) <= MEDIA_REUSE_MAX_AGE_SECS
}

/// The message to send when forwarding a stored one: text becomes a new text
/// message, media keeps its upload (url, keys, hashes) under a fresh context.
/// `base` is the stored message (unwrapped), `text` the current text/caption.
/// `None` for kinds that cannot be forwarded.
pub(crate) fn forwarded_message(
    base: Option<&wa::Message>,
    kind: MessageKind,
    text: Option<&str>,
) -> Option<wa::Message> {
    match kind {
        MessageKind::Text => {
            let text = text.filter(|t| !t.is_empty())?;
            let previous = base
                .and_then(crate::convert::context_info_of)
                .and_then(|c| c.forwarding_score)
                .unwrap_or(0);
            Some(crate::convert::text_message(text, Some(forward_context(previous))))
        }
        MessageKind::Image
        | MessageKind::Video
        | MessageKind::Audio
        | MessageKind::Voice
        | MessageKind::Document
        | MessageKind::Sticker => {
            let mut msg = base?.clone();
            // The message secret belongs to the original; the copy is a new message.
            msg.message_context_info = MessageField::none();
            with_media!(msg, as_option_mut, |m| {
                let previous = m.context_info.as_option().and_then(|c| c.forwarding_score).unwrap_or(0);
                m.context_info = MessageField::some(forward_context(previous));
            })?;
            // View-once media is never stored, but a forward must not carry the flag either.
            if let Some(m) = msg.image_message.as_option_mut() {
                m.view_once = None;
            }
            if let Some(m) = msg.video_message.as_option_mut() {
                m.view_once = None;
            }
            if let Some(m) = msg.audio_message.as_option_mut() {
                m.view_once = None;
            }
            if let Some(text) = text.filter(|t| !t.is_empty()) {
                if let Some(m) = msg.image_message.as_option_mut() {
                    m.caption = Some(text.to_string());
                } else if let Some(m) = msg.video_message.as_option_mut() {
                    m.caption = Some(text.to_string());
                } else if let Some(m) = msg.document_message.as_option_mut() {
                    m.caption = Some(text.to_string());
                }
            }
            Some(msg)
        }
        MessageKind::Other => None,
    }
}

/// Legacy rows may still hold the key of view-once media; those must never be
/// forwarded (text inside a view-once wrapper is an ordinary message).
fn forward_blocked_view_once(kind: MessageKind, stored: Option<&wa::Message>) -> bool {
    !matches!(kind, MessageKind::Text | MessageKind::Other) && stored.is_some_and(crate::convert::is_view_once)
}

/// Whether a forward can succeed at all, decided before anything is stored so
/// no row that can only fail lands in a target chat: the stored message is
/// sent again (`reuse`, which needs `built`), or it is media that can be
/// uploaded again.
fn forwardable(kind: MessageKind, reuse: bool, built: bool) -> bool {
    if reuse {
        return built;
    }
    matches!(
        kind,
        MessageKind::Image | MessageKind::Video | MessageKind::Voice | MessageKind::Audio | MessageKind::Document
    )
}

/// A message picked for forwarding.
struct ForwardSource {
    row: MessageRow,
    stored: Option<wa::Message>,
}

impl ForwardSource {
    fn base(&self) -> Option<&wa::Message> {
        self.stored.as_ref().map(|m| m.get_base_message())
    }

    /// Whether the stored upload can go out again unchanged.
    fn reuse_upload(&self, now: i64) -> bool {
        match self.row.kind {
            MessageKind::Text | MessageKind::Other => true,
            _ => {
                upload_fresh(self.base(), self.row.ts, now) && self.base().is_some_and(media_reusable)
            }
        }
    }
}

pub(crate) async fn forward_messages(
    sh: &Shared,
    client: &Arc<Client>,
    from_chat: &str,
    ids: &[String],
    to_chats: &[String],
) {
    if let Err(msg) = check_forward_limits(ids.len(), to_chats.len()) {
        reject(sh, msg);
        return;
    }
    let mut sources: Vec<(i64, ForwardSource)> = Vec::with_capacity(ids.len());
    let mut skipped = 0usize;
    for id in ids {
        match sh.store.message(from_chat, id) {
            Ok(Some(row)) if !row.revoked => {
                let stored = load_stored(&sh.store, from_chat, id);
                let rowid = sh.store.message_rowid(from_chat, id).ok().flatten().unwrap_or(i64::MAX);
                sources.push((rowid, ForwardSource { row, stored }));
            }
            Ok(_) => skipped += 1,
            Err(e) => {
                log::error!("message lookup failed: {e}");
                skipped += 1;
            }
        }
    }
    if skipped > 0 {
        reject(sh, format!("{skipped} message(s) could not be forwarded because they no longer exist"));
    }
    // Chat order, whatever order the UI selected them in.
    sources.sort_by_key(|(rowid, src)| (src.row.ts, *rowid));
    let mut unsupported = 0usize;
    for (_, src) in &sources {
        for to in to_chats {
            if to == STATUS_JID {
                continue;
            }
            if !forward_one(sh, client, src, to).await {
                unsupported += 1;
            }
        }
    }
    if unsupported > 0 {
        log::info!("{unsupported} forwards were not possible");
    }
}

/// Forwards one message to one chat. `false` when the message kind cannot be
/// forwarded (nothing is stored or sent then); send failures are reported
/// through the row status like any other send.
async fn forward_one(sh: &Shared, client: &Arc<Client>, src: &ForwardSource, to_chat_jid: &str) -> bool {
    let Some(chat) = parse_jid(sh, to_chat_jid) else {
        return true;
    };
    let Some(own) = own_jid(client) else {
        reject(sh, NOT_LOGGED_IN.to_string());
        return true;
    };
    let row = &src.row;
    if forward_blocked_view_once(row.kind, src.stored.as_ref()) {
        reject(sh, "View once media can only be opened on your phone".to_string());
        return false;
    }
    let text = row.text.as_deref();
    let reuse = src.reuse_upload(now_secs());
    // Validates the kind (and, for reuse, that the stored data is there)
    // before anything is stored.
    let reused = forwarded_message(src.base(), row.kind, text);
    if !forwardable(row.kind, reuse, reused.is_some()) {
        reject(sh, "This kind of message cannot be forwarded".to_string());
        return false;
    }

    let id = client.generate_message_id();
    let local_path = copy_cached_media(sh, row, to_chat_jid, &id).await;
    let pending = NewMessage {
        chat_jid: to_chat_jid.to_string(),
        id: id.clone(),
        sender_jid: own,
        from_me: true,
        ts: now_secs(),
        kind: row.kind,
        text: row.text.clone(),
        media_mime: row.media_mime.clone(),
        media_name: row.media_name.clone(),
        media_size: row.media_size,
        media_duration: row.media_duration,
        thumbnail: row.thumbnail.clone(),
        local_path,
        raw: reuse.then(|| reused.as_ref().map(|m| m.encode_to_vec())).flatten(),
        ..Default::default()
    };
    if !store_pending(sh, &pending) {
        if let Some(path) = &pending.local_path {
            remove_cached_file(sh, path);
        }
        return true;
    }

    let message = match reused.filter(|_| reuse) {
        Some(m) => m,
        None => match reupload_forward(sh, client, src).await {
            Ok(m) => {
                if let Err(e) = sh.store.set_raw(to_chat_jid, &id, &m.encode_to_vec()) {
                    log::error!("storing raw forwarded message failed: {e}");
                }
                m
            }
            Err(msg) => {
                reject(sh, msg);
                finish(sh, to_chat_jid, id, MessageStatus::Failed);
                return true;
            }
        },
    };
    deliver(sh, client, chat, to_chat_jid, id, message).await;
    true
}

/// Gives the forwarded copy its own cache file, so deleting one message never
/// removes the media of the other. `None` when the source has no cached file.
pub(crate) async fn copy_cached_media(sh: &Shared, row: &MessageRow, to_chat_jid: &str, new_id: &str) -> Option<String> {
    let from = PathBuf::from(row.local_path.as_deref()?);
    let ext = cache::extension_for(row.kind, row.media_mime.as_deref(), row.media_name.as_deref());
    let to = cache::cache_path(&sh.media.dir, to_chat_jid, new_id, &ext);
    let dest = to.clone();
    let copied = tokio::task::spawn_blocking(move || -> std::io::Result<bool> {
        if !from.is_file() {
            return Ok(false);
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&from, &dest).map(|_| true)
    })
    .await;
    match copied {
        Ok(Ok(true)) => Some(to.to_string_lossy().into_owned()),
        Ok(Ok(false)) => None,
        Ok(Err(e)) => {
            log::warn!("copying cached media for a forward failed: {e}");
            None
        }
        Err(e) => {
            log::error!("media copy task failed: {e}");
            None
        }
    }
}

/// Fallback for media whose stored upload is too old (or incomplete):
/// downloads it (or takes the cached file) and uploads it again.
async fn reupload_forward(sh: &Shared, client: &Arc<Client>, src: &ForwardSource) -> Result<wa::Message, String> {
    use whatsapp_rust::download::MediaType;
    let row = &src.row;
    let media_type = match row.kind {
        MessageKind::Image => MediaType::Image,
        MessageKind::Video => MediaType::Video,
        MessageKind::Voice | MessageKind::Audio => MediaType::Audio,
        MessageKind::Document => MediaType::Document,
        _ => return Err("This kind of media can no longer be forwarded".to_string()),
    };
    let path = local_media_file(sh, client, row).await?;
    let data = tokio::task::spawn_blocking(move || read_capped(&path))
        .await
        .map_err(|e| {
            log::error!("media read task failed: {e}");
            "Could not read the media".to_string()
        })??;
    let dims = src
        .base()
        .and_then(|b| b.image_message.as_option())
        .and_then(|im| im.width.zip(im.height));
    let prepared = Prepared {
        kind: row.kind,
        media_type,
        mime: row.media_mime.clone().unwrap_or_else(|| "application/octet-stream".to_string()),
        file_name: row.media_name.clone(),
        duration: row.media_duration.and_then(|d| u32::try_from(d).ok()),
        dims,
        thumbnail: row.thumbnail.clone(),
    };
    let upload = client.upload(data, prepared.media_type, UploadOptions::new()).await.map_err(|e| {
        warn_lib_error("re-uploading media for a forward failed", &e);
        format!("Upload failed: {e}")
    })?;
    let previous = src
        .base()
        .and_then(crate::convert::context_info_of)
        .and_then(|c| c.forwarding_score)
        .unwrap_or(0);
    let caption = row.text.clone().filter(|t| !t.is_empty());
    Ok(build_media_message(&prepared, upload, caption, Some(forward_context(previous))))
}

pub(crate) fn read_capped(path: &Path) -> Result<Vec<u8>, String> {
    let unreadable = |e: std::io::Error| format!("Cannot read the media: {e}");
    let len = std::fs::metadata(path).map_err(unreadable)?.len();
    if len > cache::MAX_SEND_BYTES {
        return Err("The media is larger than 100 MB".to_string());
    }
    std::fs::read(path).map_err(unreadable)
}

/// The decrypted media of `row` on disk, downloading it first when needed.
pub(crate) async fn local_media_file(sh: &Shared, client: &Arc<Client>, row: &MessageRow) -> Result<PathBuf, String> {
    let cached = |p: Option<String>| p.map(PathBuf::from).filter(|p| p.is_file());
    if let Some(p) = cached(row.local_path.clone()) {
        return Ok(p);
    }
    cache::download(sh.clone(), client.clone(), row.chat_jid.clone(), row.id.clone(), None).await;
    match sh.store.message(&row.chat_jid, &row.id) {
        Ok(Some(fresh)) => cached(fresh.local_path),
        _ => None,
    }
    .ok_or_else(|| "The media is no longer available".to_string())
}

// ----------------------------------------------------------------- pin

/// WhatsApp's three pin lengths; any other value maps to the nearest one up.
pub(crate) fn pin_duration(secs: u32) -> PinDuration {
    if secs <= PIN_24H_SECS {
        PinDuration::Hours24
    } else if secs <= PIN_7D_SECS {
        PinDuration::Days7
    } else {
        PinDuration::Days30
    }
}

/// Seconds a [`PinDuration`] lasts.
pub(crate) fn pin_secs(d: PinDuration) -> u32 {
    match d {
        PinDuration::Hours24 => PIN_24H_SECS,
        PinDuration::Days7 => PIN_7D_SECS,
        _ => 30 * PIN_24H_SECS,
    }
}

/// A pin / unpin received from a chat member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinChange {
    pub id: String,
    pub pin: bool,
    pub duration_secs: u32,
}

/// Reads a pin-in-chat protocol message.
pub fn pin_change(msg: &wa::Message) -> Option<PinChange> {
    use wa::message::pin_in_chat_message::Type;
    let base = msg.get_base_message();
    let pin = base.pin_in_chat_message.as_option()?;
    let id = pin.key.as_option()?.id.as_deref().filter(|i| !i.is_empty())?.to_string();
    let pin_it = match pin.r#type? {
        Type::PIN_FOR_ALL => true,
        Type::UNPIN_FOR_ALL => false,
        _ => return None,
    };
    let duration_secs = base
        .message_context_info
        .as_option()
        .and_then(|c| c.message_add_on_duration_in_secs)
        .unwrap_or(0);
    Some(PinChange { id, pin: pin_it, duration_secs })
}

/// When a pin that ends at `until` was made, as far as it can be told: only the
/// end is stored, and it is one of three lengths after the start. The shortest
/// length that does not put the start in the future is taken.
fn pin_started_estimate(until: i64, now: i64) -> i64 {
    let lengths = [PIN_24H_SECS, PIN_7D_SECS, 30 * PIN_24H_SECS].map(i64::from);
    let length = lengths
        .into_iter()
        .find(|l| until.saturating_sub(*l) <= now)
        .unwrap_or(lengths[2]);
    until.saturating_sub(length)
}

/// Applies a received pin change to the store; returns whether it changed.
/// `ts` is when the pin was made (unix seconds). A replayed pin (offline
/// drain, history sync: `live = false`) never replaces a longer-lived pin
/// that is already in place.
pub fn apply_pin(store: &Store, chat: &str, change: &PinChange, ts: i64, live: bool, now: i64) -> bool {
    let result = if !change.pin {
        // A replayed unpin may predate a newer pin of the same message.
        if !live {
            match store.pinned_message_at(chat, now) {
                Ok(Some((id, until))) if id == change.id && pin_started_estimate(until, now) > ts => {
                    return false;
                }
                Ok(_) => {}
                Err(e) => {
                    log::error!("reading the pinned message failed: {e}");
                    return false;
                }
            }
        }
        store.clear_pinned_message_if(chat, &change.id)
    } else {
        let until = ts.saturating_add(i64::from(change.duration_secs));
        if until <= now {
            return false;
        }
        let newer = live
            || match store.pinned_message_at(chat, now) {
                Ok(current) => current.is_none_or(|(_, current_until)| until > current_until),
                Err(e) => {
                    log::error!("reading the pinned message failed: {e}");
                    false
                }
            };
        if !newer {
            return false;
        }
        store.set_pinned_message(chat, Some(&change.id), until)
    };
    match result {
        Ok(changed) => changed,
        Err(e) => {
            log::error!("applying a pin to {} failed: {e}", redact(chat));
            false
        }
    }
}

pub(crate) async fn pin_message(
    sh: &Shared,
    client: &Arc<Client>,
    chat_jid: &str,
    id: &str,
    pin: bool,
    duration_secs: u32,
) {
    if chat_jid == STATUS_JID {
        return;
    }
    let Some(row) = load_row(sh, chat_jid, id, "pin") else {
        return;
    };
    if row.revoked {
        reject(sh, "This message was deleted".to_string());
        return;
    }
    let Some(chat) = parse_jid(sh, chat_jid) else {
        return;
    };
    let participant = match resolve_participant(sh, client, &chat, &row, true).await {
        KeyParticipant::Omit => None,
        KeyParticipant::Use(p) => Some(p.to_string()),
        KeyParticipant::Skip => {
            log::debug!("pin skipped: address form of {} unknown", redact(&row.sender_jid));
            reject(sh, "Could not pin the message: the sender's address is not known yet".to_string());
            return;
        }
    };
    let key = wa::MessageKey {
        remote_jid: Some(chat_jid.to_string()),
        from_me: Some(row.from_me),
        id: Some(id.to_string()),
        participant,
    };
    let duration = pin_duration(duration_secs);
    let result = if pin {
        client.pin_message(chat, key, duration).await
    } else {
        client.unpin_message(chat, key).await
    };
    if let Err(e) = result {
        warn_lib_error(if pin { "pinning a message failed" } else { "unpinning a message failed" }, &e);
        reject(sh, format!("Could not {} the message: {e}", if pin { "pin" } else { "unpin" }));
        return;
    }
    let stored = if pin {
        let until = now_secs().saturating_add(i64::from(pin_secs(duration)));
        sh.store.set_pinned_message(chat_jid, Some(id), until)
    } else {
        sh.store.clear_pinned_message_if(chat_jid, id)
    };
    if let Err(e) = stored {
        log::error!("storing the pin failed: {e}");
        reject(sh, "Pinned, but the local copy could not be updated".to_string());
    }
    sh.emit(UiEvent::MessagesChanged { chat_jid: chat_jid.to_string() });
    sh.emit(UiEvent::ChatsChanged);
}

// ---------------------------------------------------------------- star

pub(crate) async fn star_message(sh: &Shared, client: &Arc<Client>, chat_jid: &str, id: &str, starred: bool) {
    if chat_jid == STATUS_JID {
        return;
    }
    let Some(row) = load_row(sh, chat_jid, id, "star") else {
        return;
    };
    let previous = row.starred;
    if previous == starred {
        sh.emit(UiEvent::MessagesChanged { chat_jid: chat_jid.to_string() });
        return;
    }
    let Some(chat) = parse_jid(sh, chat_jid) else {
        return;
    };
    let participant = match resolve_participant(sh, client, &chat, &row, true).await {
        KeyParticipant::Omit => None,
        KeyParticipant::Use(p) => Some(p),
        KeyParticipant::Skip => {
            log::debug!("star skipped: address form of {} unknown", redact(&row.sender_jid));
            reject(sh, "Could not star the message: the sender's address is not known yet".to_string());
            return;
        }
    };
    // Optimistic: the UI shows the star at once.
    if let Err(e) = sh.store.set_starred(chat_jid, id, starred) {
        log::error!("storing the star failed: {e}");
        reject(sh, format!("Could not update the message: {e}"));
        return;
    }
    sh.emit(UiEvent::MessagesChanged { chat_jid: chat_jid.to_string() });

    let actions = client.chat_actions();
    let result = with_app_state_timeout(async {
        if starred {
            actions.star_message(&chat, participant.as_ref(), id, row.from_me).await
        } else {
            actions.unstar_message(&chat, participant.as_ref(), id, row.from_me).await
        }
    })
    .await;
    log_flag_outcome("StarMessage", chat_jid, &result);
    if let Err(e) = result {
        // Only undo our own write: a newer change from another device wins.
        match sh.store.message(chat_jid, id) {
            Ok(Some(now)) => {
                if let Some(restore) = guarded_revert(now.starred, starred, previous)
                    && let Err(e) = sh.store.set_starred(chat_jid, id, restore)
                {
                    log::error!("reverting the star failed: {e}");
                }
            }
            Ok(None) => {}
            Err(e) => log::error!("re-reading the message for the star revert failed: {e}"),
        }
        sh.emit(UiEvent::MessagesChanged { chat_jid: chat_jid.to_string() });
        reject(sh, app_state_failure(&e, if starred { "star the message" } else { "unstar the message" }));
    }
}

/// A star / unstar made on another device.
pub(crate) async fn on_star_update(sh: &Shared, client: &Client, u: &StarUpdate) {
    let chat = jid::normalize(client, &u.chat_jid, None).await;
    let starred = u.action.starred == Some(true);
    if apply_star(&sh.store, &sh.pending, &chat, &u.message_id, starred, u.timestamp.timestamp()) {
        sh.emit(UiEvent::MessagesChanged { chat_jid: chat });
    }
}

/// Applies a synced star, or parks it when the message is not stored yet (the
/// phone's stars arrive before the history sync). Returns whether the store
/// was modified.
pub(crate) fn apply_star(
    store: &Store,
    pending: &PendingActions,
    chat: &str,
    id: &str,
    starred: bool,
    ts: i64,
) -> bool {
    match store.message(chat, id) {
        Ok(Some(_)) => match store.set_starred(chat, id, starred) {
            Ok(changed) => changed,
            Err(e) => {
                log::error!("applying a synced star failed: {e}");
                false
            }
        },
        Ok(None) => {
            pending.push(
                chat,
                Pending {
                    sender: String::new(),
                    from_me: true,
                    action: PendingAction::Star { id: id.to_string(), starred, ts },
                },
            );
            false
        }
        Err(e) => {
            log::error!("message lookup for a synced star failed: {e}");
            false
        }
    }
}

// ------------------------------------------------------------ mentions

/// Adds the mentioned JIDs (deduplicated, in order) to a message context.
pub(crate) fn context_with_mentions(ctx: Option<wa::ContextInfo>, mentioned: Vec<String>) -> Option<wa::ContextInfo> {
    let mut unique: Vec<String> = Vec::with_capacity(mentioned.len());
    for m in mentioned {
        if !unique.contains(&m) {
            unique.push(m);
        }
    }
    if unique.is_empty() {
        return ctx;
    }
    let mut c = ctx.unwrap_or_default();
    c.mentioned_jid = unique;
    Some(c)
}

/// Mentioned JIDs in the address form the group uses (LID or phone number),
/// like the participant of a message key. Only groups have mentions; an
/// address that cannot be converted is sent as given.
pub(crate) async fn normalize_mentions(sh: &Shared, client: &Client, chat: &Jid, mentions: &[String]) -> Vec<String> {
    if !chat.is_group() || mentions.is_empty() {
        return Vec::new();
    }
    let mode = info::group_addressing(sh, client, chat).await;
    let mut out = Vec::with_capacity(mentions.len());
    for raw in mentions {
        let Ok(who) = raw.parse::<Jid>() else {
            log::warn!("ignoring an invalid mention {}", redact(raw));
            continue;
        };
        let (pn, lid) = sender_forms(client, &who).await;
        out.push(match key_participant(true, false, mode, pn, lid) {
            KeyParticipant::Use(j) => j.to_non_ad().to_string(),
            _ => who.to_non_ad().to_string(),
        });
    }
    out
}

fn user_part(jid: &str) -> &str {
    let user = jid.split('@').next().unwrap_or(jid);
    user.split(':').next().unwrap_or(user)
}

/// Whether one of `mentioned` is our account. `own_users` are the user parts
/// of our phone-number and LID addresses; `resolve` maps an address to its
/// canonical form first (history sync resolves LIDs to phone numbers).
pub(crate) fn mentions_account(mentioned: &[String], own_users: &[String], resolve: &dyn Fn(&str) -> String) -> bool {
    mentioned.iter().any(|m| {
        let resolved = resolve(m);
        own_users.iter().any(|u| !u.is_empty() && (user_part(m) == u || user_part(&resolved) == u))
    })
}

/// The user parts of our own phone-number and LID addresses.
pub(crate) fn own_users(client: &Client) -> Vec<String> {
    [client.pn(), client.lid()].into_iter().flatten().map(|j| j.user.to_string()).collect()
}

// ------------------------------------------------------------ receipts

/// Who a receipt for a message of `chat` came from: the participant in a
/// group, the chat itself in a direct chat.
pub(crate) async fn receipt_sender(
    client: &Client,
    r: &whatsapp_rust::wacore::types::events::Receipt,
    chat: &str,
) -> String {
    if !chat.ends_with("@g.us") {
        return chat.to_string();
    }
    let hint = r.source.sender_alt.as_ref().filter(|j| j.is_pn());
    jid::normalize(client, &r.source.sender, hint).await
}

/// Every key the chat of a receipt may be stored under: the normalised one
/// first, then the address as the server wrote it and its phone-number and
/// LID forms. The LID<->PN mapping is not always known when the receipt
/// arrives (a receipt carries no phone-number hint outside groups), in which
/// case normalising leaves the LID and the stored phone-number chat is missed.
pub(crate) async fn receipt_chat_keys(client: &Client, chat: &Jid, normalized: &str) -> Vec<String> {
    let mut keys = vec![normalized.to_string()];
    if chat.is_group() {
        return keys;
    }
    let (pn, lid) = sender_forms(client, chat).await;
    for form in [Some(chat.to_non_ad()), pn, lid].into_iter().flatten() {
        let key = form.to_string();
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    keys
}

/// The first of `keys` under which the store holds message `id`.
pub(crate) fn receipt_chat(store: &Store, keys: &[String], id: &str) -> Option<String> {
    keys.iter().find(|k| matches!(store.message(k, id), Ok(Some(row)) if row.from_me)).cloned()
}

/// Records a delivered / read / played receipt for one of OUR messages.
pub(crate) fn record_receipt(store: &Store, chat: &str, id: &str, who: &str, status: MessageStatus, ts: i64) {
    let kind = match status {
        MessageStatus::Delivered => ReceiptKind::Delivered,
        MessageStatus::Read => ReceiptKind::Read,
        MessageStatus::Played => ReceiptKind::Played,
        _ => return,
    };
    match store.message(chat, id) {
        Ok(Some(row)) if row.from_me => {}
        Ok(_) => return,
        Err(e) => {
            log::error!("message lookup failed: {e}");
            return;
        }
    }
    if let Err(e) = store.record_receipt(chat, id, who, kind, ts) {
        log::error!("recording a receipt failed: {e}");
    }
}

/// Maps the stored rows to the API type (store order is kept).
pub(crate) fn receipt_entries(rows: Vec<wa_store::ReceiptRow>) -> Vec<ReceiptEntry> {
    rows.into_iter()
        .map(|r| ReceiptEntry {
            jid: r.jid,
            delivered_at: r.delivered_at,
            read_at: r.read_at,
            played_at: r.played_at,
        })
        .collect()
}

pub(crate) fn fetch_message_info(sh: &Shared, chat_jid: &str, id: &str) {
    match sh.store.message_receipts(chat_jid, id) {
        Ok(rows) => sh.emit(UiEvent::MessageInfo {
            chat_jid: chat_jid.to_string(),
            id: id.to_string(),
            receipts: receipt_entries(rows),
        }),
        Err(e) => {
            log::error!("reading receipts failed: {e}");
            reject(sh, "Could not read the message info".to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_700_000_000;

    #[test]
    fn edit_is_refused_after_fifteen_minutes_or_for_foreign_messages() {
        let sent = MessageStatus::Sent;
        let ok = |from_me, revoked, kind, ts| check_edit(from_me, revoked, kind, sent, ts, NOW, "new");
        assert_eq!(ok(true, false, MessageKind::Text, NOW - 14 * 60), Ok(()));
        assert_eq!(ok(true, false, MessageKind::Text, NOW - 15 * 60), Ok(()));
        assert_eq!(
            ok(true, false, MessageKind::Text, NOW - 15 * 60 - 1),
            Err("WhatsApp only allows editing within 15 minutes")
        );
        assert_eq!(ok(false, false, MessageKind::Text, NOW), Err("You can only edit your own messages"));
        assert_eq!(ok(true, true, MessageKind::Text, NOW), Err("This message was deleted"));
        assert_eq!(ok(true, false, MessageKind::Voice, NOW), Err("Only text and captions can be edited"));
        assert_eq!(ok(true, false, MessageKind::Image, NOW), Ok(()));
        assert_eq!(
            check_edit(true, false, MessageKind::Text, sent, NOW, NOW, "  "),
            Err("A message cannot be empty")
        );
        // A caption may be removed.
        assert_eq!(check_edit(true, false, MessageKind::Image, sent, NOW, NOW, ""), Ok(()));
    }

    #[test]
    fn only_sent_messages_can_be_edited() {
        let with = |status| check_edit(true, false, MessageKind::Text, status, NOW, NOW, "new");
        for status in [MessageStatus::Sent, MessageStatus::Delivered, MessageStatus::Read, MessageStatus::Played] {
            assert_eq!(with(status), Ok(()));
        }
        for status in [MessageStatus::Pending, MessageStatus::Failed] {
            assert_eq!(with(status), Err("The message has not been sent"));
        }
    }

    #[test]
    fn edited_text_keeps_the_quote_and_edited_caption_keeps_the_media() {
        let quote = wa::ContextInfo { stanza_id: Some("Q".into()), ..Default::default() };
        let stored = crate::convert::text_message("old", Some(quote));
        let edited = edited_content(Some(&stored), MessageKind::Text, "new").expect("text edit");
        let ext = edited.extended_text_message.as_option().expect("extended text");
        assert_eq!(ext.text.as_deref(), Some("new"));
        assert_eq!(ext.context_info.as_option().and_then(|c| c.stanza_id.as_deref()), Some("Q"));

        let image = wa::Message {
            image_message: MessageField::some(wa::message::ImageMessage {
                caption: Some("old".into()),
                direct_path: Some("/p".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let edited = edited_content(Some(&image), MessageKind::Image, "cap").expect("caption edit");
        let im = edited.image_message.as_option().expect("image kept");
        assert_eq!(im.caption.as_deref(), Some("cap"));
        assert_eq!(im.direct_path.as_deref(), Some("/p"));
        assert!(edited_content(None, MessageKind::Image, "x").is_err());
        // Without the stored message the quote and mentions would be lost.
        assert!(edited_content(None, MessageKind::Text, "x").is_err());
    }

    #[test]
    fn unforwardable_messages_are_detected_before_anything_is_stored() {
        assert!(forwardable(MessageKind::Text, true, true));
        assert!(!forwardable(MessageKind::Image, true, false));
        assert!(!forwardable(MessageKind::Other, true, false));
        // Stale media is uploaded again, which only works for these kinds.
        assert!(forwardable(MessageKind::Video, false, false));
        assert!(forwardable(MessageKind::Document, false, false));
        assert!(!forwardable(MessageKind::Sticker, false, false));
        assert!(!forwardable(MessageKind::Other, false, false));
    }

    #[test]
    fn legacy_view_once_media_is_not_forwardable() {
        let mut src = image_with_upload(None);
        if let Some(im) = src.image_message.as_option_mut() {
            im.view_once = Some(true);
        }
        assert!(forward_blocked_view_once(MessageKind::Image, Some(&src)));
        assert!(!forward_blocked_view_once(MessageKind::Text, Some(&src)));
        assert!(!forward_blocked_view_once(MessageKind::Image, Some(&image_with_upload(None))));
        assert!(!forward_blocked_view_once(MessageKind::Image, None));
    }

    #[test]
    fn forwarded_media_drops_view_once_and_an_empty_caption() {
        let mut src = image_with_upload(None);
        if let Some(im) = src.image_message.as_option_mut() {
            im.view_once = Some(true);
        }
        let fwd = forwarded_message(Some(&src), MessageKind::Image, Some("")).expect("media");
        let im = fwd.image_message.as_option().expect("image");
        assert_eq!(im.view_once, None);
        assert_eq!(im.caption.as_deref(), Some("old caption"), "an empty text never replaces the caption");
    }

    fn image_with_upload(score: Option<u32>) -> wa::Message {
        wa::Message {
            image_message: MessageField::some(wa::message::ImageMessage {
                caption: Some("old caption".into()),
                direct_path: Some("/v/t62/abc".into()),
                media_key: Some(vec![1; 32]),
                file_sha256: Some(vec![2; 32]),
                file_enc_sha256: Some(vec![3; 32]),
                file_length: Some(10),
                context_info: MessageField::some(wa::ContextInfo {
                    stanza_id: Some("QUOTED".into()),
                    forwarding_score: score,
                    is_forwarded: score.map(|_| true),
                    mentioned_jid: vec!["1@s.whatsapp.net".into()],
                    ..Default::default()
                }),
                ..Default::default()
            }),
            message_context_info: MessageField::some(wa::MessageContextInfo::default()),
            ..Default::default()
        }
    }

    #[test]
    fn forwarded_text_is_marked_and_scored_one_higher() {
        let fresh = forwarded_message(None, MessageKind::Text, Some("hi")).expect("text");
        let ctx = fresh.extended_text_message.as_option().and_then(|e| e.context_info.as_option()).expect("ctx");
        assert_eq!((ctx.is_forwarded, ctx.forwarding_score), (Some(true), Some(1)));
        assert_eq!(fresh.extended_text_message.as_option().and_then(|e| e.text.as_deref()), Some("hi"));

        let previous = crate::convert::text_message("hi", Some(forward_context(3)));
        let again = forwarded_message(Some(&previous), MessageKind::Text, Some("hi")).expect("text");
        let ctx = again.extended_text_message.as_option().and_then(|e| e.context_info.as_option()).expect("ctx");
        assert_eq!(ctx.forwarding_score, Some(5));
        assert!(forwarded_message(None, MessageKind::Text, Some("")).is_none());
        assert!(forwarded_message(None, MessageKind::Other, Some("x")).is_none());
    }

    #[test]
    fn forwarded_media_reuses_the_upload_under_a_clean_forward_context() {
        let src = image_with_upload(Some(2));
        assert!(media_reusable(&src));
        let fwd = forwarded_message(Some(&src), MessageKind::Image, Some("new caption")).expect("media");
        let im = fwd.image_message.as_option().expect("image");
        assert_eq!(im.direct_path.as_deref(), Some("/v/t62/abc"));
        assert_eq!(im.media_key.as_deref(), Some(&[1u8; 32][..]));
        assert_eq!(im.file_enc_sha256.as_deref(), Some(&[3u8; 32][..]));
        assert_eq!(im.caption.as_deref(), Some("new caption"));
        let ctx = im.context_info.as_option().expect("ctx");
        assert_eq!((ctx.is_forwarded, ctx.forwarding_score), (Some(true), Some(3)));
        assert!(ctx.stanza_id.is_none() && ctx.mentioned_jid.is_empty(), "no quote or mentions carried over");
        assert!(fwd.message_context_info.as_option().is_none(), "the original's secret is dropped");
        // The source is untouched.
        assert_eq!(
            src.image_message.as_option().and_then(|m| m.caption.as_deref()),
            Some("old caption")
        );
    }

    #[test]
    fn forwarded_message_drops_the_inherited_expiration() {
        let mut src = image_with_upload(None);
        if let Some(ctx) = src.image_message.as_option_mut().and_then(|m| m.context_info.as_option_mut()) {
            ctx.expiration = Some(604_800);
        }
        let fwd = forwarded_message(Some(&src), MessageKind::Image, None).expect("media");
        assert_eq!(crate::ephemeral::message_expiration(&fwd), None);

        let mut text = crate::convert::text_message("hi", Some(forward_context(0)));
        if let Some(ctx) = text.extended_text_message.as_option_mut().and_then(|m| m.context_info.as_option_mut()) {
            ctx.expiration = Some(604_800);
        }
        let fwd = forwarded_message(Some(&text), MessageKind::Text, Some("hi")).expect("text");
        assert_eq!(crate::ephemeral::message_expiration(&fwd), None);
    }

    #[test]
    fn media_without_upload_data_is_not_reusable() {
        let mut m = image_with_upload(None);
        if let Some(im) = m.image_message.as_option_mut() {
            im.media_key = None;
        }
        assert!(!media_reusable(&m));
        assert!(!media_reusable(&crate::convert::text_message("x", None)));
        assert!(forwarded_message(None, MessageKind::Image, None).is_none());
    }

    #[test]
    fn stale_media_is_not_reused_but_text_always_is() {
        let row = |kind, ts| MessageRow { kind, ts, ..Default::default() };
        let src = |kind, ts| ForwardSource { row: row(kind, ts), stored: Some(image_with_upload(None)) };
        assert!(src(MessageKind::Image, NOW - 1000).reuse_upload(NOW));
        assert!(!src(MessageKind::Image, NOW - MEDIA_REUSE_MAX_AGE_SECS - 1).reuse_upload(NOW));
        assert!(src(MessageKind::Text, 0).reuse_upload(NOW));
    }

    #[test]
    fn upload_age_comes_from_the_media_timestamp_not_the_row() {
        let with_ts = |ts: Option<i64>| {
            let mut m = image_with_upload(None);
            if let Some(im) = m.image_message.as_option_mut() {
                im.media_key_timestamp = ts;
            }
            m
        };
        let old = NOW - MEDIA_REUSE_MAX_AGE_SECS - 1;
        // A new row (re-sent copy) of an old upload is stale.
        assert!(!upload_fresh(Some(&with_ts(Some(old))), NOW - 10, NOW));
        // A fresh upload is accepted even when the row is old.
        assert!(upload_fresh(Some(&with_ts(Some(NOW - 1000))), old, NOW));
        // No timestamp: the row time decides.
        assert!(upload_fresh(Some(&with_ts(None)), NOW - 1000, NOW));
        assert!(!upload_fresh(Some(&with_ts(None)), old, NOW));
        assert!(!upload_fresh(None, old, NOW));
        // The forwarded copy keeps the original upload time.
        let fwd = forwarded_message(Some(&with_ts(Some(old))), MessageKind::Image, None).expect("media");
        assert_eq!(fwd.image_message.as_option().and_then(|m| m.media_key_timestamp), Some(old));
    }

    fn stored_source(store: &Store, chat: &str, id: &str, now_ts: i64) -> ForwardSource {
        let row = store.message(chat, id).unwrap().expect("row");
        let stored = load_stored(store, chat, id);
        assert_eq!(row.ts, now_ts);
        ForwardSource { row, stored }
    }

    fn media_row(chat: &str, id: &str, ts: i64, raw: Option<Vec<u8>>) -> NewMessage {
        NewMessage {
            chat_jid: chat.into(),
            id: id.into(),
            sender_jid: chat.into(),
            ts,
            kind: MessageKind::Image,
            text: Some("old caption".into()),
            raw,
            ..Default::default()
        }
    }

    #[test]
    fn forward_sources_from_the_store_follow_the_guards() {
        const C: &str = "1@s.whatsapp.net";
        let store = Store::open_in_memory().unwrap();
        let clean = image_with_upload(None).encode_to_vec();
        let mut legacy = image_with_upload(None);
        if let Some(im) = legacy.image_message.as_option_mut() {
            im.view_once = Some(true);
        }
        store.insert_message(&media_row(C, "ok", 100, Some(clean)), true).unwrap();
        store.insert_message(&media_row(C, "legacy", 101, Some(legacy.encode_to_vec())), true).unwrap();
        store.insert_message(&media_row(C, "noraw", 102, None), true).unwrap();
        store.insert_message(&media_row(C, "gone", 103, Some(image_with_upload(None).encode_to_vec())), true).unwrap();
        store
            .insert_message(&NewMessage { kind: MessageKind::Other, ..media_row(C, "other", 104, None) }, true)
            .unwrap();
        store.revoke_message(C, "gone").unwrap();

        // Fresh, uploaded media is sent again as is.
        let ok = stored_source(&store, C, "ok", 100);
        assert!(!forward_blocked_view_once(ok.row.kind, ok.stored.as_ref()));
        assert!(ok.reuse_upload(100 + 1000));
        let fwd = forwarded_message(ok.base(), ok.row.kind, ok.row.text.as_deref()).expect("forwardable");
        assert_eq!(fwd.image_message.as_option().and_then(|m| m.caption.as_deref()), Some("old caption"));

        // A legacy row that still holds a view-once key is refused.
        let legacy = stored_source(&store, C, "legacy", 101);
        assert!(forward_blocked_view_once(legacy.row.kind, legacy.stored.as_ref()));

        // No stored message: cannot be reused, and a media kind must be uploaded again.
        let noraw = stored_source(&store, C, "noraw", 102);
        assert!(noraw.stored.is_none() && !noraw.reuse_upload(102));
        assert!(forwarded_message(noraw.base(), noraw.row.kind, noraw.row.text.as_deref()).is_none());

        // Deleted messages are filtered by the caller on `revoked`.
        assert!(store.message(C, "gone").unwrap().expect("row").revoked);

        // The view-once placeholder (kind Other) is never forwardable.
        let other = stored_source(&store, C, "other", 104);
        assert!(forwarded_message(other.base(), other.row.kind, other.row.text.as_deref()).is_none());
        assert!(!forwardable(other.row.kind, other.reuse_upload(104), false));
    }

    #[test]
    fn media_reuse_boundary_is_exactly_fourteen_days() {
        let src = |ts| ForwardSource {
            row: MessageRow { kind: MessageKind::Image, ts, ..Default::default() },
            stored: Some(image_with_upload(None)),
        };
        assert_eq!(MEDIA_REUSE_MAX_AGE_SECS, 14 * 24 * 3600);
        assert!(src(NOW - MEDIA_REUSE_MAX_AGE_SECS).reuse_upload(NOW));
        assert!(!src(NOW - MEDIA_REUSE_MAX_AGE_SECS - 1).reuse_upload(NOW));
        // A stale source of an uploadable kind is still forwardable (re-upload).
        assert!(forwardable(MessageKind::Image, false, true));
        assert!(!forwardable(MessageKind::Sticker, false, true));
    }

    #[test]
    fn forward_sources_sort_by_timestamp_then_insertion_order() {
        const C: &str = "1@s.whatsapp.net";
        let store = Store::open_in_memory().unwrap();
        for (id, ts) in [("late", 50), ("tie_a", 10), ("tie_b", 10), ("early", 5)] {
            store.insert_message(&media_row(C, id, ts, None), true).unwrap();
        }
        // The order the UI selected them in.
        let mut keyed: Vec<(i64, i64, &str)> = ["tie_b", "late", "early", "tie_a"]
            .into_iter()
            .map(|id| {
                let row = store.message(C, id).unwrap().unwrap();
                (row.ts, store.message_rowid(C, id).unwrap().unwrap(), id)
            })
            .collect();
        keyed.sort_by_key(|(ts, rowid, _)| (*ts, *rowid));
        assert_eq!(keyed.iter().map(|k| k.2).collect::<Vec<_>>(), ["early", "tie_a", "tie_b", "late"]);
    }

    #[test]
    fn forward_limits() {
        assert!(check_forward_limits(20, 5).is_ok());
        assert!(check_forward_limits(21, 1).is_err());
        assert!(check_forward_limits(1, 6).is_err());
        assert!(check_forward_limits(0, 1).is_err());
        assert!(check_forward_limits(1, 0).is_err());
    }

    #[test]
    fn pin_durations_map_to_the_three_choices() {
        assert_eq!(pin_duration(0), PinDuration::Hours24);
        assert_eq!(pin_duration(86_400), PinDuration::Hours24);
        assert_eq!(pin_duration(604_800), PinDuration::Days7);
        assert_eq!(pin_duration(2_592_000), PinDuration::Days30);
        assert_eq!(pin_duration(u32::MAX), PinDuration::Days30);
        assert_eq!(pin_secs(PinDuration::Days30), 2_592_000);
    }

    fn pin_message_of(id: &str, kind: wa::message::pin_in_chat_message::Type, secs: u32) -> wa::Message {
        wa::Message {
            pin_in_chat_message: MessageField::some(wa::message::PinInChatMessage {
                key: MessageField::some(wa::MessageKey { id: Some(id.into()), ..Default::default() }),
                r#type: Some(kind),
                sender_timestamp_ms: Some(0),
            }),
            message_context_info: MessageField::some(wa::MessageContextInfo {
                message_add_on_duration_in_secs: Some(secs),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn pin_messages_are_read_and_applied_with_expiry() {
        use wa::message::pin_in_chat_message::Type;
        let pin = pin_change(&pin_message_of("M1", Type::PIN_FOR_ALL, 86_400)).expect("pin");
        assert_eq!(pin, PinChange { id: "M1".into(), pin: true, duration_secs: 86_400 });
        let unpin = pin_change(&pin_message_of("M1", Type::UNPIN_FOR_ALL, 0)).expect("unpin");
        assert!(!unpin.pin);
        assert!(pin_change(&crate::convert::text_message("x", None)).is_none());

        let store = Store::open_in_memory().expect("store");
        let chat = "1@s.whatsapp.net";
        store.upsert_chat(chat, wa_store::ChatKind::Dm, None).expect("chat");
        assert!(apply_pin(&store, chat, &pin, NOW, true, NOW + 10));
        assert_eq!(store.pinned_message_at(chat, NOW + 10).expect("read"), Some(("M1".into(), NOW + 86_400)));
        // Expired once the duration has passed.
        assert_eq!(store.pinned_message_at(chat, NOW + 86_400).expect("read"), None);
        // Unpinning another message does nothing, the pinned one clears it.
        let other = PinChange { id: "M2".into(), pin: false, duration_secs: 0 };
        assert!(!apply_pin(&store, chat, &other, NOW, true, NOW));
        assert!(apply_pin(&store, chat, &unpin, NOW, true, NOW));
        assert_eq!(store.pinned_message_at(chat, NOW).expect("read"), None);
        // A pin that already expired is ignored.
        assert!(!apply_pin(&store, chat, &pin, NOW - 200_000, true, NOW));
    }

    #[test]
    fn a_replayed_unpin_does_not_clear_a_newer_pin() {
        let store = Store::open_in_memory().expect("store");
        let chat = "1@s.whatsapp.net";
        store.upsert_chat(chat, wa_store::ChatKind::Dm, None).expect("chat");
        let pin = PinChange { id: "M1".into(), pin: true, duration_secs: 86_400 };
        let unpin = PinChange { id: "M1".into(), pin: false, duration_secs: 0 };
        assert!(apply_pin(&store, chat, &pin, NOW, true, NOW + 10));
        // The unpin is older than the pin: replaying it leaves the pin alone.
        assert!(!apply_pin(&store, chat, &unpin, NOW - 100, false, NOW + 10));
        assert!(store.pinned_message_at(chat, NOW + 10).expect("read").is_some());
        // A live unpin, or a replayed one that is newer, still clears it.
        assert!(apply_pin(&store, chat, &unpin, NOW + 5, false, NOW + 10));
        assert_eq!(store.pinned_message_at(chat, NOW + 10).expect("read"), None);
        assert!(apply_pin(&store, chat, &pin, NOW, true, NOW + 10));
        assert!(apply_pin(&store, chat, &unpin, NOW - 100, true, NOW + 10));
    }

    #[test]
    fn replayed_pins_do_not_replace_a_longer_one() {
        let store = Store::open_in_memory().expect("store");
        let chat = "1@s.whatsapp.net";
        store.upsert_chat(chat, wa_store::ChatKind::Dm, None).expect("chat");
        let long = PinChange { id: "LONG".into(), pin: true, duration_secs: 30 * 86_400 };
        let short = PinChange { id: "SHORT".into(), pin: true, duration_secs: 86_400 };
        assert!(apply_pin(&store, chat, &long, NOW, true, NOW));
        assert!(!apply_pin(&store, chat, &short, NOW, false, NOW));
        assert_eq!(store.pinned_message_at(chat, NOW).expect("read").map(|p| p.0), Some("LONG".into()));
        // A live pin always wins.
        assert!(apply_pin(&store, chat, &short, NOW, true, NOW));
        assert_eq!(store.pinned_message_at(chat, NOW).expect("read").map(|p| p.0), Some("SHORT".into()));
    }

    #[test]
    fn mentions_are_deduplicated_and_added_to_the_context() {
        let ctx = wa::ContextInfo { stanza_id: Some("Q".into()), ..Default::default() };
        let out = context_with_mentions(
            Some(ctx),
            vec!["1@lid".into(), "2@lid".into(), "1@lid".into()],
        )
        .expect("context");
        assert_eq!(out.mentioned_jid, ["1@lid", "2@lid"]);
        assert_eq!(out.stanza_id.as_deref(), Some("Q"));
        assert!(context_with_mentions(None, Vec::new()).is_none());
        let msg = crate::convert::text_message("hi @1", context_with_mentions(None, vec!["1@lid".into()]));
        let ext = msg.extended_text_message.as_option().expect("extended text");
        assert_eq!(ext.context_info.as_option().map(|c| c.mentioned_jid.clone()), Some(vec!["1@lid".to_string()]));
    }

    #[test]
    fn own_account_is_found_among_mentions_by_pn_or_lid() {
        let own = vec!["491".to_string(), "9988".to_string()];
        let same = |s: &str| s.to_string();
        assert!(mentions_account(&["491@s.whatsapp.net".into()], &own, &same));
        assert!(mentions_account(&["x@lid".into(), "9988:12@lid".into()], &own, &same));
        assert!(!mentions_account(&["492@s.whatsapp.net".into()], &own, &same));
        assert!(!mentions_account(&[], &own, &same));
        assert!(!mentions_account(&["491@s.whatsapp.net".into()], &[], &same));
        // History resolves LIDs to phone numbers first.
        let resolve = |s: &str| if s == "9988@lid" { "491@s.whatsapp.net".to_string() } else { s.to_string() };
        assert!(mentions_account(&["9988@lid".into()], &["491".to_string()], &resolve));
    }

    #[test]
    fn receipts_are_recorded_for_own_messages_only() {
        let store = Store::open_in_memory().expect("store");
        let chat = "1@g.us";
        let msg = |id: &str, from_me| NewMessage {
            chat_jid: chat.into(),
            id: id.into(),
            sender_jid: "me@s.whatsapp.net".into(),
            from_me,
            ts: 10,
            text: Some("t".into()),
            ..Default::default()
        };
        store.insert_message(&msg("mine", true), false).expect("insert");
        store.insert_message(&msg("theirs", false), false).expect("insert");
        let who = "2@s.whatsapp.net";
        record_receipt(&store, chat, "mine", who, MessageStatus::Delivered, 100);
        record_receipt(&store, chat, "mine", who, MessageStatus::Read, 200);
        record_receipt(&store, chat, "mine", who, MessageStatus::Sent, 300);
        record_receipt(&store, chat, "theirs", who, MessageStatus::Read, 200);
        record_receipt(&store, chat, "unknown", who, MessageStatus::Read, 200);
        let entries = receipt_entries(store.message_receipts(chat, "mine").expect("rows"));
        assert_eq!(
            entries,
            vec![ReceiptEntry { jid: who.into(), delivered_at: Some(100), read_at: Some(200), played_at: None }]
        );
        assert!(store.message_receipts(chat, "theirs").expect("rows").is_empty());
    }

    #[test]
    fn a_receipt_finds_the_chat_under_the_other_address_form() {
        // The chat is stored under the phone number; the receipt came from the
        // LID and the mapping was not known, so normalising kept the LID.
        let store = Store::open_in_memory().expect("store");
        let pn = "5511@s.whatsapp.net";
        let lid = "100@lid";
        let sent = NewMessage {
            chat_jid: pn.into(),
            id: "m1".into(),
            sender_jid: "me@s.whatsapp.net".into(),
            from_me: true,
            ts: 10,
            text: Some("t".into()),
            ..Default::default()
        };
        store.insert_message(&sent, false).expect("insert");
        let keys = vec![lid.to_string(), pn.to_string()];
        let chat = receipt_chat(&store, &keys, "m1").expect("found under the PN form");
        assert_eq!(chat, pn);
        assert_eq!(receipt_chat(&store, &keys, "other"), None);

        record_receipt(&store, &chat, "m1", &chat, MessageStatus::Read, 200);
        assert_eq!(store.message_receipts(pn, "m1").expect("rows").len(), 1);
        assert!(store.message_receipts(lid, "m1").expect("rows").is_empty());
    }

    #[test]
    fn a_receipt_only_matches_our_own_messages() {
        let store = Store::open_in_memory().expect("store");
        let theirs = NewMessage {
            chat_jid: "5511@s.whatsapp.net".into(),
            id: "t1".into(),
            sender_jid: "5511@s.whatsapp.net".into(),
            from_me: false,
            ts: 10,
            text: Some("t".into()),
            ..Default::default()
        };
        store.insert_message(&theirs, false).expect("insert");
        let keys = vec!["5511@s.whatsapp.net".to_string()];
        assert_eq!(receipt_chat(&store, &keys, "t1"), None);
    }
}
