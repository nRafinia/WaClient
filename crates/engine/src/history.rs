//! History-sync mirroring: conversations, their messages and push names.

use std::sync::Arc;

use whatsapp_rust::prelude::{Client, Jid, wa};
use whatsapp_rust::wacore::types::events::LazyHistorySync;
use wa_store::{ChatKind, ContactPatch, MessageStatus, STATUS_JID, Store};

use crate::api::UiEvent;
use crate::events::{
    ParkedVote, Shared, apply_parked_votes, apply_pending, apply_protocol, apply_reaction, mute_until, now_secs, own_jid,
    record_poll,
};
use crate::status::STATUS_TTL_SECS;
use crate::jid::{SyncResolver, redact};
use crate::convert;

/// `HistorySyncType` values whose unread counters are trustworthy (initial
/// bootstrap, full, recent). On-demand and push-name chunks may carry stale ones.
const SYNC_TYPES_WITH_UNREAD: [i32; 3] = [0, 2, 3];

/// Clock skew allowed before an imported status counts as dated in the future.
const STATUS_FUTURE_SLACK_SECS: i64 = 300;

/// Processes one history chunk in the background; returns immediately.
pub fn spawn(sh: Shared, client: Arc<Client>, lazy: LazyHistorySync) {
    let tasks = sh.tasks.clone();
    tasks.spawn(async move {
        let _serial = sh.history_lock.lock().await;
        let progress = lazy.progress();
        log::info!(
            "history sync chunk: type={} order={:?} progress={:?} size={}B",
            lazy.sync_type(),
            lazy.chunk_order(),
            progress,
            lazy.decompressed_size()
        );
        let Some(own) = own_jid(&client) else {
            log::warn!("history sync chunk skipped: own account is not known yet");
            return;
        };
        let worker_sh = sh.clone();
        let resolver = SyncResolver::new(client.clone());
        let result = tokio::task::spawn_blocking(move || {
            let mut parked = Vec::new();
            let outcome = process(&worker_sh, resolver, &lazy, &own, &mut parked);
            (outcome, parked)
        })
        .await;
        match result {
            Ok((outcome, parked)) => {
                match outcome {
                    Ok(stored) => log::info!("history sync chunk stored {stored} messages"),
                    Err(e) => log::warn!("history sync chunk failed: {e:#}"),
                }
                // Votes that were waiting for a poll of this chunk; they need
                // the client to be decrypted.
                for chat_jid in apply_parked_votes(&sh, &client, parked).await {
                    sh.emit(UiEvent::MessagesChanged { chat_jid });
                }
            }
            Err(e) => log::error!("history sync task panicked: {e}"),
        }
        if let Some(percent) = progress {
            sh.emit(UiEvent::HistoryProgress {
                percent: percent.min(100),
            });
        }
        sh.emit(UiEvent::ChatsChanged);
    });
}

fn process(
    sh: &Shared,
    resolver: SyncResolver,
    lazy: &LazyHistorySync,
    own: &str,
    parked: &mut Vec<ParkedVote>,
) -> anyhow::Result<usize> {
    let apply_unread = SYNC_TYPES_WITH_UNREAD.contains(&lazy.sync_type());
    let mut stored = 0;
    let mut status_msgs: Vec<wa::WebMessageInfo> = Vec::new();
    let mut stream = lazy.stream();
    while let Some(conv) = stream.next_conversation()? {
        // Blocking work cannot be aborted; stop between conversations so the
        // client is released when the generation ends.
        if sh.tasks.is_cancelled() {
            anyhow::bail!("generation ended");
        }
        if conv.id == STATUS_JID {
            status_msgs.extend(conv.messages.iter().filter_map(|hm| hm.message.as_option().cloned()));
            continue;
        }
        match process_conversation(sh, &conv, &resolver, own, apply_unread, parked) {
            Ok(n) => stored += n,
            Err(e) => log::error!("history conversation {} failed: {e:#}", redact(&conv.id)),
        }
    }
    let rest = stream.remainder()?;
    status_msgs.extend(rest.status_v3_messages.iter().cloned());
    match import_statuses(&sh.store, &status_msgs, own, &|j| resolver.resolve(j), now_secs()) {
        Ok(0) => {}
        Ok(n) => {
            stored += n;
            sh.emit(UiEvent::StatusesChanged);
        }
        Err(e) => log::error!("history status import failed: {e:#}"),
    }
    for mapping in &rest.phone_number_to_lid_mappings {
        if let (Some(pn), Some(lid)) = (mapping.pn_jid.as_deref(), mapping.lid_jid.as_deref()) {
            sh.learn_lid(lid, pn);
        }
    }
    for p in &rest.pushnames {
        let (Some(id), Some(name)) = (p.id.as_deref(), p.pushname.as_deref()) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        sh.store.upsert_contact(&ContactPatch {
            jid: resolver.resolve(id),
            full_name: None,
            push_name: Some(name.to_string()),
            phone: None,
        })?;
    }
    Ok(stored)
}

/// Imports the statuses of a history chunk that are still within their 24h
/// lifetime (`now` based) and returns how many were new. They are stored like
/// live statuses (chat [`STATUS_JID`], sender = poster) but never count as
/// unread, notify or download media.
///
/// "Viewed" is taken from `WebMessageInfo.status` (`READ`/`PLAYED`, which the
/// phone sets for statuses it has seen); own statuses are always viewed (see
/// `Store::statuses_since`). Anything else stays unviewed.
fn import_statuses(
    store: &Store,
    wmis: &[wa::WebMessageInfo],
    own: &str,
    resolve: &dyn Fn(&str) -> String,
    now: i64,
) -> anyhow::Result<usize> {
    let cutoff = now.saturating_sub(STATUS_TTL_SECS);
    let mut messages = Vec::new();
    let mut seen_ids = Vec::new();
    let (mut expired, mut skipped) = (0usize, 0usize);
    for wmi in wmis {
        let Some(mut nm) = convert::from_web_message_info(wmi, STATUS_JID, own, resolve) else {
            skipped += 1;
            continue;
        };
        if nm.ts < cutoff {
            expired += 1;
            continue;
        }
        if nm.ts > now.saturating_add(STATUS_FUTURE_SLACK_SECS) {
            log::debug!("history status {} is dated in the future, skipped", nm.id);
            skipped += 1;
            continue;
        }
        if nm.from_me {
            seen_ids.push(nm.id.clone());
        } else {
            let poster = wmi
                .key
                .as_option()
                .and_then(|k| k.participant.as_deref())
                .or(wmi.participant.as_deref())
                .filter(|p| !p.is_empty());
            let Some(poster) = poster else {
                log::debug!("history status {} has no poster, skipped", nm.id);
                skipped += 1;
                continue;
            };
            let Some(resolved) = resolve_poster(poster, resolve) else {
                log::debug!("history status {} has an invalid poster, skipped", nm.id);
                skipped += 1;
                continue;
            };
            // The phone may report our own statuses with `from_me` unset and
            // our JID as participant.
            if resolved == own {
                nm.from_me = true;
                seen_ids.push(nm.id.clone());
            } else if matches!(
                convert::web_status(wmi),
                Some(MessageStatus::Read | MessageStatus::Played)
            ) {
                seen_ids.push(nm.id.clone());
            }
            nm.sender_jid = resolved;
        }
        messages.push(nm);
    }
    let inserted = store.insert_messages_bulk(&messages)?;
    if !wmis.is_empty() {
        log::info!(
            "history statuses: {} in chunk, {inserted} stored, {expired} expired, {skipped} skipped",
            wmis.len()
        );
    }
    for id in &seen_ids {
        if let Err(e) = store.mark_status_viewed(id) {
            log::warn!("marking imported status {id} viewed failed: {e}");
        }
    }
    Ok(inserted)
}

/// Normalizes a status poster; `None` unless it is a user JID (phone number or
/// LID) before and after resolving.
fn resolve_poster(poster: &str, resolve: &dyn Fn(&str) -> String) -> Option<String> {
    let is_user = |j: &str| j.parse::<Jid>().is_ok_and(|j| j.is_pn() || j.is_lid());
    if !is_user(poster) {
        return None;
    }
    let resolved = resolve(poster);
    is_user(&resolved).then_some(resolved)
}

fn chat_jid_for(conv: &wa::Conversation, resolver: &SyncResolver) -> String {
    if conv.id.ends_with("@lid")
        && let Some(pn) = conv
            .pn_jid
            .as_deref()
            .and_then(|j| j.parse::<Jid>().ok())
            .filter(Jid::is_pn)
    {
        return pn.to_non_ad().to_string();
    }
    resolver.resolve(&conv.id)
}

fn process_conversation(
    sh: &Shared,
    conv: &wa::Conversation,
    resolver: &SyncResolver,
    own: &str,
    apply_unread: bool,
    parked: &mut Vec<ParkedVote>,
) -> anyhow::Result<usize> {
    if conv.id == STATUS_JID || conv.id.ends_with("@newsletter") {
        return Ok(0);
    }
    let chat = chat_jid_for(conv, resolver);
    if conv.id.ends_with("@lid") && chat != conv.id {
        sh.learn_lid(&conv.id, &chat);
    }
    // Only chats without local history take the server's counter; for known
    // chats our own count is authoritative and must never be overwritten.
    let is_new_chat = sh
        .store
        .chat(&chat)?
        .is_none_or(|c| c.last_ts == 0 && c.unread == 0);
    let name = conv
        .name
        .as_deref()
        .or(conv.display_name.as_deref())
        .filter(|n| !n.is_empty());
    sh.store
        .upsert_chat(&chat, ChatKind::from_jid(&chat), name)?;

    let pinned = conv.pinned.map(|p| p > 0);
    let muted = conv
        .mute_end_time
        .map(|t| mute_until(i64::try_from(t).unwrap_or(-1)));
    if pinned.is_some() || conv.archived.is_some() || muted.is_some() {
        sh.store.set_chat_flags(&chat, pinned, conv.archived, muted)?;
    }

    // The chat's timer; no notice row, since history only restores state.
    if let Some(secs) = conv.ephemeral_expiration {
        let ts = conv.ephemeral_setting_timestamp.unwrap_or(0);
        sh.store.set_chat_ephemeral(&chat, i64::from(secs), ts)?;
    }

    let mut messages = Vec::with_capacity(conv.messages.len());
    let mut statuses = Vec::new();
    let mut actions = Vec::new();
    let mut reactions = Vec::new();
    let mut votes = Vec::new();
    let mut pins = Vec::new();
    let own_users = vec![own.split('@').next().unwrap_or(own).to_string()];
    for hm in &conv.messages {
        let Some(wmi) = hm.message.as_option() else {
            continue;
        };
        let Some(mut nm) = convert::from_web_message_info(wmi, &chat, own, &|j| resolver.resolve(j))
        else {
            if let Some(change) = wmi.message.as_option().and_then(crate::msgops::pin_change) {
                let ts = i64::try_from(wmi.message_timestamp.unwrap_or(0)).unwrap_or(0);
                pins.push((change, ts));
                continue;
            }
            if let Some(r) =
                convert::web_reaction_message(wmi, &chat, own, &|j| resolver.resolve(j))
            {
                reactions.push(r);
                continue;
            }
            if let Some(action) =
                convert::web_protocol_action(wmi, &chat, own, &|j| resolver.resolve(j))
            {
                actions.push(action);
            }
            continue;
        };
        if !nm.from_me
            && let Some(msg) = wmi.message.as_option()
        {
            nm.mentions_me = crate::msgops::mentions_account(
                &convert::mentioned_jids(msg),
                &own_users,
                &|j| resolver.resolve(j),
            );
        }
        // Synced own messages were sent long ago; without an explicit status
        // they must not linger as "sending" (stale-pending cleanup would mark
        // them failed).
        if nm.from_me {
            let status = convert::web_status(wmi).unwrap_or(wa_store::MessageStatus::Sent);
            statuses.push((nm.id.clone(), status));
        }
        reactions.extend(convert::web_reactions(wmi, &chat, own, &|j| resolver.resolve(j)));
        // The phone already decrypted the votes of synced polls.
        if let Some(poll) = convert::poll_info_of_stored(&nm) {
            for (voter, from_me, hashes, ts) in
                convert::web_poll_votes(wmi, &chat, own, &|j| resolver.resolve(j))
            {
                let names = convert::option_names_for_hashes(&poll.options, &hashes);
                votes.push((nm.id.clone(), voter, from_me, names, ts));
            }
        }
        if let Some(secs) = wmi.message.as_option().and_then(crate::ephemeral::message_expiration) {
            nm.expires_at = crate::ephemeral::expiry_for(nm.ts, i64::from(secs));
        }
        messages.push(nm);
    }
    let inserted = sh.store.insert_messages_bulk(&messages)?;
    for m in &messages {
        record_poll(&sh.store, m);
    }
    for (poll_id, voter, from_me, names, ts) in &votes {
        if !sh
            .store
            .upsert_poll_vote(&chat, poll_id, voter, names, *ts, *from_me)?
        {
            log::warn!(
                "ignoring poll vote in {}: more options selected than the poll allows",
                redact(&chat)
            );
        }
    }
    let mut changed = !votes.is_empty();
    for (sender, from_me, reaction) in &reactions {
        let ts = reaction.ts_ms.map_or(0, |ms| ms / 1000);
        changed |= apply_reaction(&sh.store, &chat, sender, *from_me, reaction, ts);
    }
    // Applied after the bulk insert so a revoke/edit finds its target even when
    // both are part of the same chunk.
    for m in &messages {
        let applied = apply_pending(&sh.store, &sh.pending, &chat, &m.id);
        changed |= applied.changed;
        parked.extend(applied.votes);
    }
    for (sender, from_me, action) in &actions {
        changed |= apply_protocol(&sh.store, &sh.pending, &chat, sender, *from_me, action);
    }
    for (change, ts) in &pins {
        changed |= crate::msgops::apply_pin(&sh.store, &chat, change, *ts, false, now_secs());
    }
    if changed {
        sh.emit(UiEvent::MessagesChanged { chat_jid: chat.clone() });
    }
    for (id, status) in statuses {
        sh.store.update_status(&chat, &id, status)?;
    }
    if apply_unread
        && is_new_chat
        && let Some(n) = conv.unread_count
    {
        sh.store.set_unread(&chat, i64::from(n))?;
    }
    Ok(inserted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use whatsapp_rust::buffa::MessageField;
    use whatsapp_rust::prelude::MessageBuilderExt as _;
    use wa_store::{MessageKind, NewMessage};

    const OWN: &str = "1@s.whatsapp.net";
    const FRIEND: &str = "2@s.whatsapp.net";
    const NOW: i64 = 2_000_000_000;

    fn wmi(id: &str, from_me: bool, poster: Option<&str>, ts: i64, status: Option<wa::web_message_info::Status>) -> wa::WebMessageInfo {
        wa::WebMessageInfo {
            key: MessageField::some(wa::MessageKey {
                remote_jid: Some(STATUS_JID.into()),
                from_me: Some(from_me),
                id: Some(id.into()),
                participant: poster.map(String::from),
            }),
            message: MessageField::some(wa::Message::text("hi")),
            message_timestamp: u64::try_from(ts).ok(),
            status,
            ..Default::default()
        }
    }

    fn chunk() -> Vec<wa::WebMessageInfo> {
        vec![
            wmi("OLD", false, Some(FRIEND), NOW - 30 * 3600, None),
            wmi("MINE", true, None, NOW - 3600, None),
            wmi("THEIRS", false, Some(FRIEND), NOW - 7200, None),
        ]
    }

    #[test]
    fn recent_statuses_are_imported_without_unread() {
        let store = Store::open_in_memory().expect("store");
        let n = import_statuses(&store, &chunk(), OWN, &|j| j.to_string(), NOW).expect("import");
        assert_eq!(n, 2);
        let groups = store.statuses_since(NOW - STATUS_TTL_SECS).expect("groups");
        assert_eq!(groups.len(), 2);
        let mine = groups.iter().find(|g| g.sender_jid == OWN).expect("own group");
        assert!(mine.all_viewed);
        assert_eq!(mine.items[0].id, "MINE");
        let theirs = groups.iter().find(|g| g.sender_jid == FRIEND).expect("friend group");
        assert!(!theirs.all_viewed);
        assert_eq!(theirs.items.len(), 1);
        assert_eq!(theirs.items[0].id, "THEIRS");
        assert!(store.message(STATUS_JID, "OLD").expect("q").is_none());
        assert_eq!(store.chat(STATUS_JID).expect("q").map_or(0, |c| c.unread), 0);
    }

    #[test]
    fn imported_statuses_survive_the_purge_and_are_listed() {
        // Same clock for import, startup purge and the UI window: nothing
        // imported as fresh may be purged, and nothing needs a unit change.
        let store = Store::open_in_memory().expect("store");
        let mut msgs = chunk();
        msgs.push(wmi("EDGE", false, Some(FRIEND), NOW - STATUS_TTL_SECS + 1, None));
        assert_eq!(import_statuses(&store, &msgs, OWN, &|j| j.to_string(), NOW).expect("import"), 3);
        let tmp = tempfile::tempdir().expect("tmp");
        assert_eq!(crate::status::purge_expired(&store, tmp.path(), NOW).expect("purge"), 0);
        let total: usize = store
            .statuses_since(NOW - STATUS_TTL_SECS)
            .expect("groups")
            .iter()
            .map(|g| g.items.len())
            .sum();
        assert_eq!(total, 3);
        // A millisecond timestamp (the unit mix-up) would be "in the future".
        let ms = wmi("MS", false, Some(FRIEND), NOW * 1000, None);
        assert_eq!(import_statuses(&store, &[ms], OWN, &|j| j.to_string(), NOW).expect("import"), 0);
    }

    #[test]
    fn read_status_marks_viewed_and_missing_poster_is_skipped() {
        let store = Store::open_in_memory().expect("store");
        let msgs = vec![
            wmi("SEEN", false, Some(FRIEND), NOW - 60, Some(wa::web_message_info::Status::READ)),
            wmi("NOPOSTER", false, None, NOW - 60, None),
        ];
        assert_eq!(import_statuses(&store, &msgs, OWN, &|j| j.to_string(), NOW).expect("import"), 1);
        let groups = store.statuses_since(0).expect("groups");
        assert_eq!(groups.len(), 1);
        assert!(groups[0].all_viewed);
    }

    #[test]
    fn future_dated_statuses_are_skipped() {
        let store = Store::open_in_memory().expect("store");
        let msgs = vec![
            wmi("FUTURE", false, Some(FRIEND), NOW + 301, None),
            wmi("SKEW", false, Some(FRIEND), NOW + 299, None),
        ];
        assert_eq!(import_statuses(&store, &msgs, OWN, &|j| j.to_string(), NOW).expect("import"), 1);
        assert!(store.message(STATUS_JID, "FUTURE").expect("q").is_none());
        assert!(store.message(STATUS_JID, "SKEW").expect("q").is_some());
    }

    #[test]
    fn malformed_poster_is_skipped() {
        let store = Store::open_in_memory().expect("store");
        let msgs = vec![
            wmi("GARBAGE", false, Some("not a jid"), NOW - 60, None),
            wmi("GROUP", false, Some("123-456@g.us"), NOW - 60, None),
            wmi("OK", false, Some(FRIEND), NOW - 60, None),
        ];
        assert_eq!(import_statuses(&store, &msgs, OWN, &|j| j.to_string(), NOW).expect("import"), 1);
        assert!(store.message(STATUS_JID, "GARBAGE").expect("q").is_none());
        assert!(store.message(STATUS_JID, "GROUP").expect("q").is_none());
    }

    #[test]
    fn own_participant_without_from_me_is_treated_as_own() {
        let store = Store::open_in_memory().expect("store");
        let msgs = vec![wmi("MINE2", false, Some(OWN), NOW - 60, None)];
        assert_eq!(import_statuses(&store, &msgs, OWN, &|j| j.to_string(), NOW).expect("import"), 1);
        let stored = store.message(STATUS_JID, "MINE2").expect("q").expect("stored");
        assert!(stored.from_me);
        let groups = store.statuses_since(0).expect("groups");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].sender_jid, OWN);
        assert!(groups[0].all_viewed);
    }

    #[test]
    fn later_live_delivery_of_the_same_status_is_a_duplicate() {
        let store = Store::open_in_memory().expect("store");
        import_statuses(&store, &chunk(), OWN, &|j| j.to_string(), NOW).expect("import");
        assert_eq!(import_statuses(&store, &chunk(), OWN, &|j| j.to_string(), NOW).expect("again"), 0);
        let live = NewMessage {
            chat_jid: STATUS_JID.into(),
            id: "THEIRS".into(),
            sender_jid: FRIEND.into(),
            from_me: false,
            ts: NOW - 7200,
            kind: MessageKind::Text,
            text: Some("hi".into()),
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
        };
        assert!(!store.insert_message(&live, true).expect("insert"));
        let groups = store.statuses_since(0).expect("groups");
        assert_eq!(groups.iter().map(|g| g.items.len()).sum::<usize>(), 2);
    }

    #[test]
    fn conversation_of_only_business_or_unsupported_messages_lists_the_chat() {
        // The steps of process_conversation: chat row, convert each history
        // message, bulk insert. Unconvertible messages used to leave the chat
        // with no rows, which the chat list hides.
        const BIZ: &str = "9@s.whatsapp.net";
        let store = Store::open_in_memory().expect("store");
        store.upsert_chat(BIZ, ChatKind::Dm, Some("Shop")).expect("chat");
        let template = wa::Message {
            template_message: MessageField::some(wa::message::TemplateMessage {
                format: Some(wa::message::template_message::Format::HydratedFourRowTemplate(Box::new(
                    wa::message::template_message::HydratedFourRowTemplate {
                        hydrated_content_text: Some("Your code is 123".into()),
                        ..Default::default()
                    },
                ))),
                ..Default::default()
            }),
            ..Default::default()
        };
        let unknown = wa::Message {
            group_invite_message: MessageField::some(wa::message::GroupInviteMessage::default()),
            ..Default::default()
        };
        let mut rows = Vec::new();
        for (id, msg, ts) in [("T1", template, 100), ("U1", unknown, 200)] {
            let mut w = wmi(id, false, None, ts, None);
            w.message = MessageField::some(msg);
            rows.push(convert::from_web_message_info(&w, BIZ, OWN, &|j| j.to_string()).expect("converted"));
        }
        store.insert_messages_bulk(&rows).expect("insert");
        let chats = store.list_chats(false).expect("chats");
        assert_eq!(chats.len(), 1);
        assert_eq!(chats[0].jid, BIZ);
        assert_eq!(store.chat(BIZ).expect("q").map(|c| c.last_ts), Some(200));
    }
}
