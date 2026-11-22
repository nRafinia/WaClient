//! Disappearing messages: per-chat and default timers, expiry of stored
//! messages and the purger that deletes them locally.
//!
//! WhatsApp has no per-message timer; the per-message variant is "view once"
//! for photos and videos (see [`view_once_placeholder_row`]).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use whatsapp_rust::prelude::{Client, Jid, wa};
use whatsapp_rust::wacore::proto_helpers::MessageExt as _;
use whatsapp_rust::wacore_binary::JidExt as _;
use wa_store::{ChatKind, MessageKind, MessageStatus, NewMessage, STATUS_JID, Store, VIEW_ONCE_TEXT};

use crate::api::UiEvent;
use crate::commands::{NOT_LOGGED_IN, parse_jid, reject};
use crate::events::{Shared, now_secs, own_jid};
use crate::jid::{classify_error, redact, warn_lib_error};

/// The durations WhatsApp offers: 24 hours, 7 days, 90 days.
pub(crate) const TIMER_OPTIONS: [u32; 3] = [86_400, 604_800, 7_776_000];

const META_DEFAULT_TIMER: &str = "default_timer";
const PURGE_INTERVAL: Duration = Duration::from_secs(60);
const PURGE_BATCH: u32 = 500;
/// Bounds one purge run so a huge backlog cannot monopolise the store.
const PURGE_MAX_BATCHES: usize = 20;

/// `0` (off) or one of [`TIMER_OPTIONS`].
pub(crate) fn is_valid_timer(secs: u32) -> bool {
    secs == 0 || TIMER_OPTIONS.contains(&secs)
}

/// When a message sent at `ts` under a timer of `secs` expires; `None` when
/// the timer is off.
pub(crate) fn expiry_for(ts: i64, secs: i64) -> Option<i64> {
    (secs > 0).then(|| ts.saturating_add(secs))
}

/// The expiration to put on an outgoing message of `chat_jid` whose timer is
/// `chat_secs`. Statuses have their own lifetime and never take a chat timer.
pub(crate) fn chat_expiration(chat_jid: &str, chat_secs: i64) -> Option<u32> {
    if chat_jid == STATUS_JID {
        return None;
    }
    u32::try_from(chat_secs).ok().filter(|s| *s > 0)
}

/// Timer of the chat an outgoing message goes to (see [`chat_expiration`]).
pub(crate) fn outgoing_expiration(sh: &Shared, chat_jid: &str) -> Option<u32> {
    if chat_jid == STATUS_JID {
        return None;
    }
    match sh.store.chat(chat_jid) {
        Ok(chat) => chat.and_then(|c| chat_expiration(chat_jid, c.ephemeral_secs)),
        Err(e) => {
            log::error!("reading the chat timer failed: {e}");
            None
        }
    }
}

/// A copy of `row` with `expires_at` set from the chat's timer; `None` when
/// the row needs no change.
pub(crate) fn with_outgoing_expiry(sh: &Shared, row: &NewMessage) -> Option<NewMessage> {
    if row.expires_at.is_some() {
        return None;
    }
    let secs = outgoing_expiration(sh, &row.chat_jid)?;
    let expires_at = expiry_for(row.ts, i64::from(secs))?;
    Some(NewMessage {
        expires_at: Some(expires_at),
        ..row.clone()
    })
}

/// Makes `message` able to carry `contextInfo.expiration`: a bare
/// `conversation` has no context field, so it becomes an extended text.
pub(crate) fn make_expirable(message: &mut wa::Message) {
    if let Some(text) = message.conversation.take() {
        message.extended_text_message = whatsapp_rust::buffa::MessageField::some(wa::message::ExtendedTextMessage {
            text: Some(text),
            ..Default::default()
        });
    }
}

/// The expiration a stored/received message carries in its contextInfo.
pub(crate) fn message_expiration(msg: &wa::Message) -> Option<u32> {
    let mut current = msg;
    for _ in 0..4 {
        if let Some(exp) = current.get_ephemeral_expiration() {
            return Some(exp);
        }
        match current
            .ephemeral_message
            .as_option()
            .and_then(|w| w.message.as_option())
            .or_else(|| current.device_sent_message.as_option().and_then(|w| w.message.as_option()))
        {
            Some(inner) => current = inner,
            None => return None,
        }
    }
    None
}

/// Human text of a timer length.
pub(crate) fn duration_label(secs: i64) -> String {
    match secs {
        86_400 => "24 hours".to_string(),
        604_800 => "7 days".to_string(),
        7_776_000 => "90 days".to_string(),
        s if s >= 86_400 && s % 86_400 == 0 => format!("{} days", s / 86_400),
        s if s >= 3_600 && s % 3_600 == 0 => format!("{} hours", s / 3_600),
        s => format!("{s} seconds"),
    }
}

/// The notice shown in the chat when the timer changes.
pub(crate) fn notice_text(secs: i64) -> String {
    if secs > 0 {
        format!("Disappearing messages were turned on \u{2014} {}", duration_label(secs))
    } else {
        "Disappearing messages were turned off".to_string()
    }
}

/// The system-notice row (kind Other); its id makes repeats of the same change
/// collapse into one row.
pub(crate) fn notice_row(chat_jid: &str, sender_jid: &str, from_me: bool, secs: i64, ts: i64) -> NewMessage {
    NewMessage {
        chat_jid: chat_jid.to_string(),
        id: format!("EPH-{ts}-{secs}"),
        sender_jid: sender_jid.to_string(),
        from_me,
        ts,
        kind: MessageKind::Other,
        text: Some(notice_text(secs)),
        ..Default::default()
    }
}

/// Applies a timer change to the chat, honouring the change order. Inserts the
/// notice when the effective timer changed. Returns whether anything changed
/// (the caller then reports the chat as changed).
pub(crate) fn apply_setting(store: &Store, chat_jid: &str, sender_jid: &str, from_me: bool, secs: i64, ts: i64) -> bool {
    if chat_jid == STATUS_JID || secs < 0 {
        return false;
    }
    let result = (|| -> Result<bool, wa_store::StoreError> {
        let previous = store.chat(chat_jid)?;
        let (prev_secs, prev_ts) = previous.as_ref().map_or((0, 0), |c| (c.ephemeral_secs, c.ephemeral_ts));
        if ts < prev_ts {
            return Ok(false);
        }
        if previous.is_none() {
            store.upsert_chat(chat_jid, ChatKind::from_jid(chat_jid), None)?;
        }
        if !store.set_chat_ephemeral(chat_jid, secs, ts)? {
            return Ok(false);
        }
        if prev_secs == secs {
            return Ok(false);
        }
        let notice = notice_row(chat_jid, sender_jid, from_me, secs, ts);
        if store.insert_message(&notice, false)? && from_me {
            store.update_status(chat_jid, &notice.id, MessageStatus::Sent)?;
        }
        Ok(true)
    })();
    match result {
        Ok(changed) => changed,
        Err(e) => {
            log::error!("applying the chat timer failed: {e}");
            false
        }
    }
}

/// Adopts the timer a live message carries in `contextInfo.expiration` when
/// the explicit setting change never reached us (e.g. sent while offline or
/// before linking). Conservative on purpose: only a positive expiration that
/// differs from the stored timer and whose message is newer than the stored
/// setting is taken, silently (no notice row). A message without an
/// expiration never switches the timer off, because old clients and some
/// message types omit it; turning off only comes from an explicit change.
/// Returns whether the chat's timer changed.
pub(crate) fn adopt_message_timer(store: &Store, chat_jid: &str, secs: u32, ts: i64) -> bool {
    if chat_jid == STATUS_JID || secs == 0 {
        return false;
    }
    let result = (|| -> Result<bool, wa_store::StoreError> {
        let Some(chat) = store.chat(chat_jid)? else {
            return Ok(false);
        };
        if chat.ephemeral_secs == i64::from(secs) || ts <= chat.ephemeral_ts {
            return Ok(false);
        }
        store.set_chat_ephemeral(chat_jid, i64::from(secs), ts)
    })();
    result.unwrap_or_else(|e| {
        log::error!("adopting the message timer failed: {e}");
        false
    })
}

/// `(expiration, timestamp)` of an `EPHEMERAL_SETTING` protocol message.
pub(crate) fn setting_change(msg: &wa::Message) -> Option<(u32, Option<i64>)> {
    use wa::message::protocol_message::Type;
    let proto = msg
        .protocol_message
        .as_option()
        .or_else(|| {
            msg.device_sent_message
                .as_option()
                .and_then(|d| d.message.as_option())
                .and_then(|m| m.protocol_message.as_option())
        })?;
    if proto.r#type != Some(Type::EPHEMERAL_SETTING) {
        return None;
    }
    Some((proto.ephemeral_expiration.unwrap_or(0), proto.ephemeral_setting_timestamp))
}

/// A contact's default mode applies to chats not started yet; it must never
/// rewrite the timer of a chat that already has history.
pub(crate) fn apply_contact_default(store: &Store, chat_jid: &str, secs: i64, ts: i64) -> bool {
    // A chat not started yet may have no row: create the minimal one so the
    // first message we send already carries the contact's default (no notice).
    let result = (|| -> Result<bool, wa_store::StoreError> {
        match store.chat(chat_jid)? {
            Some(chat) if chat.last_ts != 0 => Ok(false),
            Some(_) => store.set_chat_ephemeral(chat_jid, secs, ts),
            None => {
                store.upsert_chat(chat_jid, ChatKind::from_jid(chat_jid), None)?;
                store.set_chat_ephemeral(chat_jid, secs, ts)
            }
        }
    })();
    result.unwrap_or_else(|e| {
        log::error!("applying the contact default timer failed: {e}");
        false
    })
}

/// `DisappearingModeChanged` of a contact.
pub(crate) async fn on_mode_changed(
    sh: &Shared,
    client: &Client,
    ev: &whatsapp_rust::wacore::types::events::DisappearingModeChanged,
) {
    let who = crate::jid::normalize(client, &ev.from, None).await;
    if apply_contact_default(&sh.store, &who, i64::from(ev.duration), ev.setting_timestamp.timestamp()) {
        sh.emit(UiEvent::ChatsChanged);
    }
}

/// A group's timer changed (by us elsewhere or by an admin).
pub(crate) fn on_group_setting(sh: &Shared, group: &Jid, expiration: u32, ts: i64) {
    let chat = group.to_string();
    if apply_setting(&sh.store, &chat, &chat, false, i64::from(expiration), ts) {
        sh.emit(UiEvent::MessagesChanged { chat_jid: chat });
        sh.emit(UiEvent::ChatsChanged);
    }
}

/// Friendly, JID-free message for a failed timer change.
pub(crate) fn timer_error_message(is_group: bool, class: &str) -> String {
    match class {
        "not authorized" if is_group => "Only admins can change this setting".to_string(),
        "not authorized" => "You are not allowed to change this setting".to_string(),
        "timeout" => "WhatsApp did not answer in time. Try again".to_string(),
        "not connected" => "Not connected to WhatsApp".to_string(),
        "rate limited" => "Too many requests. Try again in a moment".to_string(),
        _ => "Could not change the disappearing messages timer".to_string(),
    }
}

pub(crate) async fn set_chat_timer(sh: &Shared, client: &Arc<Client>, chat_jid: &str, secs: u32) {
    if !is_valid_timer(secs) {
        reject(sh, "Choose 24 hours, 7 days, 90 days or off".to_string());
        return;
    }
    let Some(chat) = parse_jid(sh, chat_jid) else {
        return;
    };
    let Some(own) = own_jid(client) else {
        reject(sh, NOT_LOGGED_IN.to_string());
        return;
    };
    if chat.is_status_broadcast() || chat.is_newsletter() || chat_jid == STATUS_JID {
        reject(sh, "Disappearing messages are not available for this chat".to_string());
        return;
    }
    let is_group = chat.is_group();
    let failure = if is_group {
        client.groups().set_ephemeral(chat, secs).await.err().map(|e| {
            warn_lib_error("setting the group timer failed", &e);
            classify_error(&e)
        })
    } else {
        client.set_chat_disappearing_timer(chat, secs).await.err().map(|e| {
            warn_lib_error("setting the chat timer failed", &e);
            classify_error(&e)
        })
    };
    if let Some(class) = failure {
        reject(sh, timer_error_message(is_group, class));
        return;
    }
    log::info!("chat timer set to {secs}s");
    if apply_setting(&sh.store, chat_jid, &own, true, i64::from(secs), now_secs()) {
        sh.emit(UiEvent::MessagesChanged { chat_jid: chat_jid.to_string() });
    }
    sh.emit(UiEvent::ChatsChanged);
}

pub(crate) async fn set_default_timer(sh: &Shared, client: &Arc<Client>, secs: u32) {
    if !is_valid_timer(secs) {
        reject(sh, "Choose 24 hours, 7 days, 90 days or off".to_string());
        return;
    }
    if let Err(e) = client.set_default_disappearing_mode(secs).await {
        warn_lib_error("setting the default timer failed", &e);
        reject(sh, timer_error_message(false, classify_error(&e)));
        return;
    }
    if let Err(e) = sh.store.meta_set(META_DEFAULT_TIMER, &secs.to_string()) {
        log::error!("storing the default timer failed: {e}");
    }
    sh.emit(UiEvent::DefaultTimer { secs });
}

/// The library offers no way to read the account default, so the value we last
/// set is the answer.
pub(crate) fn fetch_default_timer(sh: &Shared) {
    let secs = match sh.store.meta_get(META_DEFAULT_TIMER) {
        Ok(v) => parse_default(v.as_deref()),
        Err(e) => {
            log::error!("reading the default timer failed: {e}");
            0
        }
    };
    sh.emit(UiEvent::DefaultTimer { secs });
}

fn parse_default(raw: Option<&str>) -> u32 {
    raw.and_then(|v| v.parse::<u32>().ok()).filter(|s| is_valid_timer(*s)).unwrap_or(0)
}

/// The row kept for a view-once photo/video we sent: the placeholder only,
/// like received ones (no media, no raw message, no cache file).
pub(crate) fn view_once_placeholder_row(pending: &NewMessage) -> NewMessage {
    NewMessage {
        chat_jid: pending.chat_jid.clone(),
        id: pending.id.clone(),
        sender_jid: pending.sender_jid.clone(),
        from_me: true,
        ts: pending.ts,
        kind: MessageKind::Other,
        text: Some(VIEW_ONCE_TEXT.to_string()),
        expires_at: pending.expires_at,
        raw: None,
        ..Default::default()
    }
}

/// Whether a media kind may be sent as view once.
pub(crate) fn view_once_allowed(kind: MessageKind) -> bool {
    matches!(kind, MessageKind::Image | MessageKind::Video)
}

/// Sets the view-once flag on the media of `message`; `false` when it has none.
pub(crate) fn set_view_once(message: &mut wa::Message) -> bool {
    if let Some(im) = message.image_message.as_option_mut() {
        im.view_once = Some(true);
        true
    } else if let Some(v) = message.video_message.as_option_mut() {
        v.view_once = Some(true);
        true
    } else {
        false
    }
}

/// What one purge run removed.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Purged {
    pub(crate) messages: usize,
    pub(crate) chats: Vec<String>,
    pub(crate) files: Vec<String>,
}

/// Deletes expired, unstarred messages (and their reactions, receipts, pins)
/// in batches. Stops at the first store error so a failing row cannot loop.
pub(crate) fn purge_expired(store: &Store, now: i64, batch: u32, max_batches: usize) -> Purged {
    let mut out = Purged::default();
    for _ in 0..max_batches {
        let rows = match store.expired_messages(now, batch) {
            Ok(rows) => rows,
            Err(e) => {
                log::error!("listing expired messages failed: {e}");
                break;
            }
        };
        let full = rows.len() >= usize::try_from(batch).unwrap_or(usize::MAX);
        let mut failed = false;
        for (chat, id, _) in &rows {
            match store.delete_message(chat, id) {
                Ok(path) => {
                    out.messages += 1;
                    out.files.extend(path);
                    if !out.chats.contains(chat) {
                        out.chats.push(chat.clone());
                    }
                }
                Err(e) => {
                    log::error!("deleting an expired message failed: {e}");
                    failed = true;
                    break;
                }
            }
        }
        if failed || !full {
            break;
        }
    }
    out
}

/// Deletes cached files, but only those inside `media_dir`.
pub(crate) fn remove_files_in_cache(files: &[String], media_dir: &Path) -> usize {
    let mut removed = 0;
    for file in files {
        let path = Path::new(file);
        if !crate::status::is_inside_cache(path, media_dir) {
            continue;
        }
        match std::fs::remove_file(path) {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log::warn!("cannot delete cached media of an expired message: {e}"),
        }
    }
    removed
}

async fn purge_once(sh: &Shared) {
    let (store, dir) = (sh.store.clone(), sh.media.dir.clone());
    let done = tokio::task::spawn_blocking(move || {
        let purged = purge_expired(&store, now_secs(), PURGE_BATCH, PURGE_MAX_BATCHES);
        let files = remove_files_in_cache(&purged.files, &dir);
        (purged, files)
    })
    .await;
    match done {
        Ok((purged, files)) if purged.messages > 0 => {
            log::info!("purged {} expired messages and {files} media files", purged.messages);
            for chat_jid in purged.chats {
                log::debug!("expired messages removed from {}", redact(&chat_jid));
                sh.emit(UiEvent::MessagesChanged { chat_jid });
            }
            sh.emit(UiEvent::ChatsChanged);
        }
        Ok(_) => {}
        Err(e) => log::error!("expiry purge task failed: {e}"),
    }
}

/// Runs for the whole generation: once at start, then every minute.
pub(crate) async fn purger(sh: Shared) {
    let mut tick = tokio::time::interval(PURGE_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        purge_once(&sh).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use whatsapp_rust::buffa::MessageField;
    use whatsapp_rust::prelude::MessageBuilderExt as _;

    const C: &str = "1@s.whatsapp.net";

    fn row(id: &str, ts: i64, expires_at: Option<i64>, local_path: Option<String>) -> NewMessage {
        NewMessage {
            chat_jid: C.into(),
            id: id.into(),
            sender_jid: C.into(),
            ts,
            text: Some("hi".into()),
            expires_at,
            local_path,
            ..Default::default()
        }
    }

    #[test]
    fn duration_validation() {
        for ok in [0, 86_400, 604_800, 7_776_000] {
            assert!(is_valid_timer(ok));
        }
        for bad in [1, 3_600, 86_401, 31_536_000] {
            assert!(!is_valid_timer(bad));
        }
    }

    #[test]
    fn expiry_computation() {
        assert_eq!(expiry_for(1_000, 86_400), Some(87_400));
        assert_eq!(expiry_for(1_000, 0), None);
        assert_eq!(expiry_for(i64::MAX, 86_400), Some(i64::MAX));
    }

    #[test]
    fn expiration_applies_to_chats_with_a_timer_but_not_to_status_or_off() {
        assert_eq!(chat_expiration(C, 604_800), Some(604_800));
        assert_eq!(chat_expiration(C, 0), None);
        assert_eq!(chat_expiration(STATUS_JID, 604_800), None);
    }

    #[test]
    fn notice_texts() {
        assert_eq!(notice_text(86_400), "Disappearing messages were turned on \u{2014} 24 hours");
        assert!(notice_text(604_800).ends_with("7 days"));
        assert!(notice_text(7_776_000).ends_with("90 days"));
        assert_eq!(notice_text(0), "Disappearing messages were turned off");
    }

    #[test]
    fn setting_order_is_honoured_and_notice_added_once() {
        let store = Store::open_in_memory().unwrap();
        assert!(apply_setting(&store, C, C, false, 86_400, 100));
        assert_eq!(store.chat(C).unwrap().unwrap().ephemeral_secs, 86_400);
        // Older change loses.
        assert!(!apply_setting(&store, C, C, false, 0, 50));
        assert_eq!(store.chat(C).unwrap().unwrap().ephemeral_secs, 86_400);
        // Same value again: no second notice.
        assert!(!apply_setting(&store, C, C, false, 86_400, 200));
        // Newer change wins.
        assert!(apply_setting(&store, C, C, false, 0, 300));
        let chat = store.chat(C).unwrap().unwrap();
        assert_eq!((chat.ephemeral_secs, chat.ephemeral_ts), (0, 300));
        let notices = [store.message(C, "EPH-100-86400").unwrap(), store.message(C, "EPH-300-0").unwrap()];
        assert!(notices.iter().all(|n| n.as_ref().is_some_and(|n| n.kind == MessageKind::Other)));
        assert!(store.message(C, "EPH-200-86400").unwrap().is_none());
    }

    #[test]
    fn every_source_of_change_stores_the_timer() {
        let store = Store::open_in_memory().unwrap();
        // The other person turned it on in a DM.
        assert!(apply_setting(&store, C, C, false, 604_800, 100));
        let chat = store.chat(C).unwrap().unwrap();
        assert_eq!((chat.ephemeral_secs, chat.ephemeral_ts), (604_800, 100));
        // We change it ourselves later.
        assert!(apply_setting(&store, C, "me@s.whatsapp.net", true, 86_400, 200));
        assert_eq!(store.chat(C).unwrap().unwrap().ephemeral_secs, 86_400);
        // History sync restoring an older state does not win, a newer one does.
        assert!(!store.set_chat_ephemeral(C, 0, 150).unwrap());
        assert_eq!(store.chat(C).unwrap().unwrap().ephemeral_secs, 86_400);
        assert!(store.set_chat_ephemeral(C, 7_776_000, 300).unwrap());
        assert_eq!(store.chat(C).unwrap().unwrap().ephemeral_secs, 7_776_000);
        // A group admin changes it (no chat row yet: it is created).
        let group = "123-456@g.us";
        assert!(apply_setting(&store, group, group, false, 86_400, 50));
        assert_eq!(store.chat(group).unwrap().unwrap().ephemeral_secs, 86_400);
        assert!(apply_setting(&store, group, group, false, 0, 60));
        assert_eq!(store.chat(group).unwrap().unwrap().ephemeral_secs, 0);
    }

    #[test]
    fn message_expiration_is_adopted_conservatively() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_chat(C, ChatKind::Dm, None).unwrap();
        // Unknown timer: a newer message that carries one sets it, silently.
        assert!(adopt_message_timer(&store, C, 86_400, 100));
        assert_eq!(store.chat(C).unwrap().unwrap().ephemeral_secs, 86_400);
        assert!(store.message(C, "EPH-100-86400").unwrap().is_none());
        // Same value or an older message: nothing.
        assert!(!adopt_message_timer(&store, C, 86_400, 200));
        assert!(!adopt_message_timer(&store, C, 604_800, 50));
        // Messages never switch the timer off; only explicit changes do.
        assert!(!adopt_message_timer(&store, C, 0, 500));
        assert_eq!(store.chat(C).unwrap().unwrap().ephemeral_secs, 86_400);
        // A newer different value is taken; status and unknown chats are not.
        assert!(adopt_message_timer(&store, C, 604_800, 300));
        assert!(!adopt_message_timer(&store, STATUS_JID, 86_400, 300));
        assert!(!adopt_message_timer(&store, "9@s.whatsapp.net", 86_400, 300));
    }

    #[test]
    fn contact_default_never_rewrites_a_chat_with_history() {
        let store = Store::open_in_memory().unwrap();
        store.insert_message(&row("m", 10, None, None), true).unwrap();
        assert!(!apply_contact_default(&store, C, 86_400, 100));
        assert_eq!(store.chat(C).unwrap().unwrap().ephemeral_secs, 0);
    }

    #[test]
    fn contact_default_creates_a_missing_chat_silently() {
        let store = Store::open_in_memory().unwrap();
        let other = "2@s.whatsapp.net";
        assert!(apply_contact_default(&store, other, 86_400, 100));
        let chat = store.chat(other).unwrap().expect("chat row created");
        assert_eq!((chat.ephemeral_secs, chat.ephemeral_ts), (86_400, 100));
        assert!(store.message(other, "EPH-100-86400").unwrap().is_none(), "no notice");
    }

    #[test]
    fn ephemeral_setting_message_is_recognised() {
        use wa::message::protocol_message::Type;
        let msg = wa::Message {
            protocol_message: MessageField::some(wa::message::ProtocolMessage {
                r#type: Some(Type::EPHEMERAL_SETTING),
                ephemeral_expiration: Some(604_800),
                ephemeral_setting_timestamp: Some(77),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(setting_change(&msg), Some((604_800, Some(77))));
        assert_eq!(setting_change(&wa::Message::text("x")), None);
    }

    #[test]
    fn plain_text_becomes_expirable_and_carries_the_expiration() {
        let mut m = wa::Message::text("hello");
        make_expirable(&mut m);
        assert!(m.conversation.is_none());
        assert!(m.set_ephemeral_expiration(86_400));
        assert_eq!(message_expiration(&m), Some(86_400));
        assert_eq!(message_expiration(&wa::Message::text("x")), None);
    }

    #[test]
    fn purger_selects_expired_unstarred_and_deletes_only_inside_the_cache() {
        let store = Store::open_in_memory().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let inside_file = cache.path().join("a.jpg");
        let outside_file = outside.path().join("b.jpg");
        std::fs::write(&inside_file, b"x").unwrap();
        std::fs::write(&outside_file, b"x").unwrap();
        let p = |f: &std::path::Path| Some(f.to_string_lossy().into_owned());
        store.insert_message(&row("old", 10, Some(100), p(&inside_file)), true).unwrap();
        store.insert_message(&row("odd", 11, Some(101), p(&outside_file)), true).unwrap();
        store.insert_message(&row("star", 12, Some(102), None), true).unwrap();
        store.insert_message(&row("future", 13, Some(10_000), None), true).unwrap();
        store.insert_message(&row("plain", 14, None, None), true).unwrap();
        store.set_starred(C, "star", true).unwrap();

        let purged = purge_expired(&store, 500, 500, 3);
        assert_eq!(purged.messages, 2);
        assert_eq!(purged.chats, vec![C.to_string()]);
        assert_eq!(remove_files_in_cache(&purged.files, cache.path()), 1);
        assert!(!inside_file.exists());
        assert!(outside_file.exists(), "files outside the cache are never touched");
        assert!(store.message(C, "old").unwrap().is_none());
        for kept in ["star", "future", "plain"] {
            assert!(store.message(C, kept).unwrap().is_some(), "{kept}");
        }
    }

    #[test]
    fn purger_works_through_a_backlog_in_batches() {
        let store = Store::open_in_memory().unwrap();
        for i in 0..5 {
            store.insert_message(&row(&format!("m{i}"), i, Some(1), None), true).unwrap();
        }
        assert_eq!(purge_expired(&store, 10, 2, 10).messages, 5);
    }

    #[test]
    fn view_once_rows_and_kinds() {
        assert!(view_once_allowed(MessageKind::Image));
        assert!(view_once_allowed(MessageKind::Video));
        for k in [MessageKind::Audio, MessageKind::Voice, MessageKind::Document, MessageKind::Text] {
            assert!(!view_once_allowed(k));
        }
        let mut image = wa::Message {
            image_message: MessageField::some(wa::message::ImageMessage::default()),
            ..Default::default()
        };
        assert!(set_view_once(&mut image));
        assert_eq!(image.image_message.as_option().and_then(|i| i.view_once), Some(true));
        let mut video = wa::Message {
            video_message: MessageField::some(wa::message::VideoMessage::default()),
            ..Default::default()
        };
        assert!(set_view_once(&mut video));
        assert!(!set_view_once(&mut wa::Message::text("x")));

        let pending = NewMessage {
            kind: MessageKind::Image,
            local_path: Some("p".into()),
            thumbnail: Some(vec![1]),
            raw: Some(vec![1]),
            expires_at: Some(5),
            ..row("v", 9, None, None)
        };
        let placeholder = view_once_placeholder_row(&pending);
        assert_eq!(placeholder.text.as_deref(), Some(VIEW_ONCE_TEXT));
        assert!(placeholder.from_me && placeholder.raw.is_none() && placeholder.local_path.is_none());
        assert!(placeholder.thumbnail.is_none());
        assert_eq!(placeholder.expires_at, Some(5));
    }

    #[test]
    fn default_timer_parsing_and_error_texts() {
        assert_eq!(parse_default(Some("604800")), 604_800);
        assert_eq!(parse_default(Some("12")), 0);
        assert_eq!(parse_default(None), 0);
        assert_eq!(timer_error_message(true, "not authorized"), "Only admins can change this setting");
        assert!(!timer_error_message(false, "other").is_empty());
    }
}
