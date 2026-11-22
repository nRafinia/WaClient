//! Incoming call notices, missed-call rows and declining calls (no media:
//! calls are answered on the phone).
//!
//! The library is built without the `voip` feature, so it never synthesises
//! `MissedCall` / `CallEndedElsewhere` for a caller hanging up; those arrive as
//! plain `IncomingCall` events carrying a terminate/reject/accept action, which
//! [`classify_end`] maps. The dedicated events are still handled (the offline
//! replay of an offer comes as `MissedCall`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use whatsapp_rust::prelude::{Client, Jid};
use whatsapp_rust::wacore::types::call::{CallAction, CallEndedElsewhere, ElsewhereOutcome, IncomingCall, MissedCall};
use whatsapp_rust::wacore_binary::JidExt as _;
use wa_store::{ChatKind, ContactPatch, MessageKind, NewMessage, Store};

use crate::api::{CallEndReason, UiEvent};
use crate::events::{Shared, now_secs};
use crate::jid::{self, redact};
use crate::settings;

/// A ringing call is forgotten (and its banner hidden) after this long.
const RING_TTL: Duration = Duration::from_secs(120);

/// A call we are ringing for.
#[derive(Clone)]
struct Ringing {
    incoming: IncomingCall,
    chat_jid: String,
    sender_jid: String,
    is_video: bool,
    is_group: bool,
    started: Instant,
}

/// Ringing calls of one engine generation.
#[derive(Default)]
pub struct CallState {
    ringing: Mutex<HashMap<String, Ringing>>,
}

impl CallState {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Ringing>> {
        self.ringing.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `false` when the call is already known (the entry is left untouched).
    fn insert(&self, call_id: &str, entry: Ringing) -> bool {
        let mut map = self.lock();
        if map.contains_key(call_id) {
            return false;
        }
        map.insert(call_id.to_string(), entry);
        true
    }

    fn get(&self, call_id: &str) -> Option<Ringing> {
        self.lock().get(call_id).cloned()
    }

    fn take(&self, call_id: &str) -> Option<Ringing> {
        self.lock().remove(call_id)
    }

    fn contains(&self, call_id: &str) -> bool {
        self.lock().contains_key(call_id)
    }

    /// Removes and returns the calls ringing for [`RING_TTL`] or longer.
    fn take_expired(&self, now: Instant) -> Vec<(String, Ringing)> {
        let mut map = self.lock();
        let expired: Vec<String> = map
            .iter()
            .filter(|(_, r)| now.saturating_duration_since(r.started) >= RING_TTL)
            .map(|(id, _)| id.clone())
            .collect();
        expired
            .into_iter()
            .filter_map(|id| map.remove(&id).map(|r| (id, r)))
            .collect()
    }

    /// Puts a call back after a failed decline. `false` (and nothing is stored)
    /// when it rang past [`RING_TTL`] meanwhile or the call is known again.
    fn restore(&self, call_id: &str, entry: Ringing, now: Instant) -> bool {
        if now.saturating_duration_since(entry.started) >= RING_TTL {
            return false;
        }
        self.insert(call_id, entry)
    }

    fn take_all(&self) -> Vec<String> {
        self.lock().drain().map(|(id, _)| id).collect()
    }
}

/// How a ringing call ended, as far as the signalling tells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ending {
    Missed,
    AnsweredElsewhere,
    DeclinedElsewhere,
    /// Stopped ringing for a reason we do not know: no row.
    Ended,
}

impl Ending {
    fn reason(self) -> CallEndReason {
        match self {
            Ending::Missed => CallEndReason::Missed,
            Ending::AnsweredElsewhere => CallEndReason::AnsweredElsewhere,
            Ending::DeclinedElsewhere => CallEndReason::DeclinedElsewhere,
            Ending::Ended => CallEndReason::Ended,
        }
    }
}

/// Only a plain hang-up or timeout is certainly a missed call; any other
/// reason leaves the outcome unknown.
fn terminate_ending(reason: Option<&str>) -> Ending {
    match reason {
        None | Some("timeout" | "group_call_ended") => Ending::Missed,
        Some("accepted_elsewhere") => Ending::AnsweredElsewhere,
        Some("rejected_elsewhere") => Ending::DeclinedElsewhere,
        Some(_) => Ending::Ended,
    }
}

/// What a non-offer call action means for a call we are ringing for; `None`
/// when it does not end the ring (transport, video state, ...). A reject or
/// accept stanza speaks for one participant of a group call, and a `busy`
/// reject for one device, so neither ends the ring there.
fn classify_end(action: &CallAction, is_group: bool) -> Option<Ending> {
    match action {
        CallAction::Terminate { reason, .. } => Some(terminate_ending(reason.as_deref())),
        CallAction::Reject { reason, .. } if !is_group && reason.as_deref() != Some("busy") => {
            Some(Ending::DeclinedElsewhere)
        }
        CallAction::Accept { .. } if !is_group => Some(Ending::AnsweredElsewhere),
        _ => None,
    }
}

fn kind_word(is_video: Option<bool>) -> &'static str {
    match is_video {
        Some(true) => " video",
        Some(false) => " voice",
        None => "",
    }
}

/// Chat text of a missed call. `is_video` is `None` when the signalling does
/// not say (an offer replayed from the offline queue).
fn missed_text(is_video: Option<bool>, is_group: bool) -> String {
    let group = if is_group { " group" } else { "" };
    format!("Missed{group}{} call", kind_word(is_video))
}

fn declined_text(is_video: bool) -> String {
    format!("Declined{} call", kind_word(Some(is_video)))
}

fn notify_body(is_video: bool) -> &'static str {
    if is_video { "Incoming video call" } else { "Incoming voice call" }
}

fn call_row(call_id: &str, chat_jid: &str, sender_jid: &str, ts: i64, text: String) -> NewMessage {
    NewMessage {
        chat_jid: chat_jid.to_string(),
        id: format!("CALL-{call_id}"),
        sender_jid: sender_jid.to_string(),
        from_me: false,
        ts,
        kind: MessageKind::Other,
        text: Some(text),
        ..Default::default()
    }
}

/// Inserts a call row, creating the chat when missing. `Ok(false)` when the
/// row already exists (the insert is idempotent per call id).
fn insert_call_row(store: &Store, row: &NewMessage, unread: bool) -> Result<bool, wa_store::StoreError> {
    if store.chat(&row.chat_jid)?.is_none() {
        store.upsert_chat(&row.chat_jid, ChatKind::from_jid(&row.chat_jid), None)?;
    }
    store.insert_message(row, unread)
}

fn write_row(sh: &Shared, client: &Arc<Client>, row: &NewMessage, unread: bool, is_group: bool) {
    match insert_call_row(&sh.store, row, unread) {
        Ok(true) => {
            if is_group {
                crate::groups::ensure_name(sh, client, &row.chat_jid);
            }
            sh.emit(UiEvent::MessagesChanged { chat_jid: row.chat_jid.clone() });
            sh.emit(UiEvent::ChatsChanged);
        }
        Ok(false) => {}
        Err(e) => log::error!("storing a call row failed: {e}"),
    }
}

fn end(sh: &Shared, call_id: &str, reason: CallEndReason) {
    sh.emit(UiEvent::CallEnded { call_id: call_id.to_string(), reason });
}

/// Hides the banners of calls that rang too long (no row: we never learned how
/// they ended).
fn expire(sh: &Shared) {
    for (id, _) in sh.calls.take_expired(Instant::now()) {
        log::debug!("ringing call expired");
        end(sh, &id, CallEndReason::Ended);
    }
}

/// Session ended: nothing rings any more. No rows, the call state is unknown.
/// (A plain reconnect keeps ringing calls; the TTL handles stale ones.)
pub fn clear(sh: &Shared) {
    for id in sh.calls.take_all() {
        end(sh, &id, CallEndReason::Ended);
    }
}

pub async fn on_incoming(sh: &Shared, client: &Arc<Client>, call: &IncomingCall) {
    expire(sh);
    let call_id = call.action.call_id();
    match &call.action {
        CallAction::Offer { call_creator, caller_pn, is_video, group_jid, .. } => {
            // An offline replay is reported as `MissedCall` instead.
            if call.offline {
                return;
            }
            ring(sh, client, call, call_creator, caller_pn.as_ref(), *is_video, group_jid.clone()).await;
        }
        CallAction::OfferNotice { call_creator, is_video, is_group, .. } => {
            if call.offline || sh.calls.contains(call_id) {
                return;
            }
            let group = if *is_group {
                [Some(&call.from), call.recipient.as_ref(), call.participant.as_ref()]
                    .into_iter()
                    .flatten()
                    .find(|j| j.is_group())
                    .cloned()
            } else {
                None
            };
            if *is_group && group.is_none() {
                log::debug!("group call notice without a group address, ignored");
                return;
            }
            ring(sh, client, call, call_creator, None, *is_video, group).await;
        }
        other => {
            let Some(ringing) = sh.calls.get(call_id) else {
                return;
            };
            let Some(ending) = classify_end(other, ringing.is_group) else {
                return;
            };
            finish(sh, client, call_id, ending, call.timestamp.timestamp());
        }
    }
}

async fn ring(
    sh: &Shared,
    client: &Arc<Client>,
    call: &IncomingCall,
    creator: &Jid,
    caller_pn: Option<&Jid>,
    is_video: bool,
    group: Option<Jid>,
) {
    let call_id = call.action.call_id();
    if call_id.is_empty() || sh.calls.contains(call_id) {
        return;
    }
    if creator.is_lid()
        && let Some(pn) = caller_pn
    {
        sh.learn_lid(&creator.to_string(), &pn.to_string());
    }
    let sender = jid::normalize(client, creator, caller_pn).await;
    let push_name = call.notify.clone().filter(|n| !n.is_empty());
    if let Some(name) = &push_name
        && let Err(e) = sh.store.upsert_contact(&ContactPatch {
            jid: sender.clone(),
            full_name: None,
            push_name: Some(name.clone()),
            phone: None,
        })
    {
        log::error!("upsert_contact failed: {e}");
    }
    let is_group = group.is_some();
    let chat_jid = group.as_ref().map_or_else(|| sender.clone(), |g| g.to_non_ad().to_string());
    let entry = Ringing {
        incoming: call.clone(),
        chat_jid: chat_jid.clone(),
        sender_jid: sender.clone(),
        is_video,
        is_group,
        started: Instant::now(),
    };
    if !sh.calls.insert(call_id, entry) {
        return;
    }
    log::info!("incoming {} call", if is_video { "video" } else { "voice" });
    log::debug!("incoming call in {}", redact(&chat_jid));
    let caller_name = crate::events::display_name(sh, &sender, push_name.as_deref().unwrap_or(""));
    sh.emit(UiEvent::IncomingCall {
        call_id: call_id.to_string(),
        chat_jid: chat_jid.clone(),
        caller_name: caller_name.clone(),
        is_video,
        is_group,
    });
    notify(sh, &chat_jid, caller_name, is_video);

    let sh2 = sh.clone();
    sh.tasks.spawn(async move {
        tokio::time::sleep(RING_TTL + Duration::from_secs(1)).await;
        expire(&sh2);
    });
}

fn notify(sh: &Shared, chat_jid: &str, title: String, is_video: bool) {
    if !settings::may_notify(&sh.settings()) {
        return;
    }
    // Same contract as message notifications: a muted chat is flagged, the UI decides.
    let muted = match sh.store.chat(chat_jid) {
        Ok(Some(c)) => c.muted_until > now_secs(),
        Ok(None) => false,
        Err(e) => {
            log::error!("chat lookup failed: {e}");
            false
        }
    };
    sh.emit(UiEvent::Notify {
        chat_jid: chat_jid.to_string(),
        title,
        body: notify_body(is_video).to_string(),
        muted,
    });
}

/// A ringing call ended; removes it, tells the UI and (when missed) records it.
fn finish(sh: &Shared, client: &Arc<Client>, call_id: &str, ending: Ending, ts: i64) {
    let Some(r) = sh.calls.take(call_id) else {
        return;
    };
    end(sh, call_id, ending.reason());
    if ending == Ending::Missed {
        let row = call_row(call_id, &r.chat_jid, &r.sender_jid, ts, missed_text(Some(r.is_video), r.is_group));
        write_row(sh, client, &row, true, r.is_group);
    }
}

pub async fn on_missed(sh: &Shared, client: &Arc<Client>, m: &MissedCall) {
    expire(sh);
    if sh.calls.contains(&m.call_id) {
        finish(sh, client, &m.call_id, Ending::Missed, m.timestamp.timestamp());
        return;
    }
    // Not ringing: an offer replayed from the offline queue. Type of call unknown.
    let is_group = m.from.is_group();
    let sender = jid::normalize(client, &m.from, None).await;
    let row = call_row(&m.call_id, &sender, &sender, m.timestamp.timestamp(), missed_text(None, is_group));
    write_row(sh, client, &row, true, is_group);
}

pub fn on_ended_elsewhere(sh: &Shared, e: &CallEndedElsewhere) {
    if sh.calls.take(&e.call_id).is_none() {
        return;
    }
    let reason = match e.outcome {
        ElsewhereOutcome::Accepted => CallEndReason::AnsweredElsewhere,
        _ => CallEndReason::DeclinedElsewhere,
    };
    end(sh, &e.call_id, reason);
}

/// Result of a decline attempt, once the send has finished.
enum DeclineOutcome {
    Declined(Box<Ringing>),
    /// Failed; `restored` = the call is ringing again.
    Failed { restored: bool },
}

/// Settles a decline whose entry was taken out of the state before the send.
fn settle_decline(state: &CallState, call_id: &str, r: Ringing, sent: bool, now: Instant) -> DeclineOutcome {
    if sent {
        DeclineOutcome::Declined(Box::new(r))
    } else {
        DeclineOutcome::Failed { restored: state.restore(call_id, r, now) }
    }
}

pub async fn reject(sh: &Shared, client: &Arc<Client>, call_id: &str) {
    // Taken before the send (like the library consumes its ringing flag): a
    // hang-up arriving meanwhile finds nothing and writes no missed row, and a
    // second decline is ignored.
    let Some(r) = sh.calls.take(call_id) else {
        log::debug!("RejectCall for an unknown, ended or already declined call");
        return;
    };
    let result = client.voip().reject(&r.incoming).await;
    if let Err(e) = &result {
        jid::warn_lib_error("declining a call failed", e);
    }
    match settle_decline(&sh.calls, call_id, r, result.is_ok(), Instant::now()) {
        DeclineOutcome::Declined(r) => {
            log::info!("declined a call");
            end(sh, call_id, CallEndReason::Declined);
            let row = call_row(call_id, &r.chat_jid, &r.sender_jid, now_secs(), declined_text(r.is_video));
            write_row(sh, client, &row, false, r.is_group);
        }
        DeclineOutcome::Failed { restored } => {
            if !restored {
                end(sh, call_id, CallEndReason::Ended);
            }
            sh.emit(UiEvent::CallDeclineFailed { call_id: call_id.to_string() });
            sh.emit(UiEvent::Error { message: "Couldn't decline the call".to_string() });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHAT: &str = "111@s.whatsapp.net";

    fn jid() -> Jid {
        CHAT.parse().expect("jid")
    }

    fn terminate(reason: Option<&str>) -> CallAction {
        CallAction::Terminate {
            call_id: "c".into(),
            call_creator: jid(),
            reason: reason.map(String::from),
            duration: None,
            audio_duration: None,
        }
    }

    fn ringing(started: Instant) -> Ringing {
        let incoming = IncomingCall::builder()
            .from(jid())
            .stanza_id("s".to_string())
            .timestamp(whatsapp_rust::chrono::DateTime::from_timestamp(5, 0).expect("ts"))
            .offline(false)
            .action(terminate(None))
            .build();
        Ringing {
            incoming,
            chat_jid: CHAT.into(),
            sender_jid: CHAT.into(),
            is_video: false,
            is_group: false,
            started,
        }
    }

    #[test]
    fn terminate_reasons_map_to_endings() {
        assert_eq!(classify_end(&terminate(None), false), Some(Ending::Missed));
        assert_eq!(classify_end(&terminate(Some("timeout")), false), Some(Ending::Missed));
        assert_eq!(classify_end(&terminate(Some("group_call_ended")), true), Some(Ending::Missed));
        assert_eq!(classify_end(&terminate(Some("accepted_elsewhere")), false), Some(Ending::AnsweredElsewhere));
        assert_eq!(classify_end(&terminate(Some("rejected_elsewhere")), false), Some(Ending::DeclinedElsewhere));
        assert_eq!(classify_end(&terminate(Some("something_new")), false), Some(Ending::Ended));
        assert_eq!(Ending::Ended.reason(), CallEndReason::Ended);
        assert_eq!(terminate_ending(Some("")), Ending::Ended);
    }

    #[test]
    fn decline_ok_leaves_no_entry_and_yields_the_call() {
        let state = CallState::default();
        let t0 = Instant::now();
        state.insert("a", ringing(t0));
        let r = state.take("a").expect("taken before the send");
        assert!(state.take("a").is_none(), "a second decline finds nothing");
        assert!(matches!(settle_decline(&state, "a", r, true, t0), DeclineOutcome::Declined(_)));
        assert!(!state.contains("a"));
    }

    #[test]
    fn decline_error_restores_the_call_unless_expired() {
        let state = CallState::default();
        let t0 = Instant::now();
        state.insert("a", ringing(t0));
        let r = state.take("a").expect("taken");
        let out = settle_decline(&state, "a", r, false, t0 + Duration::from_secs(5));
        assert!(matches!(out, DeclineOutcome::Failed { restored: true }));
        assert!(state.contains("a"));

        let r = state.take("a").expect("taken again");
        let out = settle_decline(&state, "a", r, false, t0 + RING_TTL);
        assert!(matches!(out, DeclineOutcome::Failed { restored: false }));
        assert!(!state.contains("a"));
    }

    #[test]
    fn terminate_during_a_decline_in_flight_finds_no_entry() {
        let state = CallState::default();
        state.insert("a", ringing(Instant::now()));
        let _in_flight = state.take("a").expect("taken");
        // `finish` and `on_incoming` start from this lookup and stop on `None`,
        // so no row and no event are produced.
        assert!(state.get("a").is_none());
        assert!(state.take("a").is_none());
    }

    #[test]
    fn reject_and_accept_end_only_one_to_one_rings() {
        let reject = |reason: Option<&str>| CallAction::Reject {
            call_id: "c".into(),
            call_creator: jid(),
            reason: reason.map(String::from),
        };
        assert_eq!(classify_end(&reject(None), false), Some(Ending::DeclinedElsewhere));
        assert_eq!(classify_end(&reject(Some("busy")), false), None);
        assert_eq!(classify_end(&reject(None), true), None);
        let accept = CallAction::Accept { call_id: "c".into(), call_creator: jid(), audio: vec![] };
        assert_eq!(classify_end(&accept, false), Some(Ending::AnsweredElsewhere));
        assert_eq!(classify_end(&accept, true), None);
        let relay = CallAction::RelayLatency { call_id: "c".into(), call_creator: jid() };
        assert_eq!(classify_end(&relay, false), None);
    }

    #[test]
    fn row_texts() {
        assert_eq!(missed_text(Some(false), false), "Missed voice call");
        assert_eq!(missed_text(Some(true), false), "Missed video call");
        assert_eq!(missed_text(Some(false), true), "Missed group voice call");
        assert_eq!(missed_text(Some(true), true), "Missed group video call");
        assert_eq!(missed_text(None, false), "Missed call");
        assert_eq!(missed_text(None, true), "Missed group call");
        assert_eq!(declined_text(false), "Declined voice call");
        assert_eq!(declined_text(true), "Declined video call");
        assert_eq!(notify_body(true), "Incoming video call");
        assert_eq!(notify_body(false), "Incoming voice call");
    }

    #[test]
    fn ringing_calls_expire_after_two_minutes() {
        let state = CallState::default();
        let t0 = Instant::now();
        assert!(state.insert("a", ringing(t0)));
        assert!(!state.insert("a", ringing(t0)), "known call is ignored");
        assert!(state.insert("b", ringing(t0 + Duration::from_secs(100))));
        assert!(state.take_expired(t0 + Duration::from_secs(119)).is_empty());
        let gone = state.take_expired(t0 + Duration::from_secs(120));
        assert_eq!(gone.len(), 1);
        assert_eq!(gone[0].0, "a");
        assert!(state.contains("b") && !state.contains("a"));
        assert_eq!(state.take_all(), vec!["b".to_string()]);
        assert!(state.get("b").is_none());
    }

    #[test]
    fn missed_row_is_inserted_once_and_counts_unread() {
        let store = Store::open_in_memory().expect("store");
        let row = call_row("X1", CHAT, CHAT, 50, missed_text(Some(true), false));
        assert_eq!(row.id, "CALL-X1");
        assert!(insert_call_row(&store, &row, true).expect("insert"));
        assert!(!insert_call_row(&store, &row, true).expect("again"));
        let chat = store.chat(CHAT).expect("query").expect("chat created");
        assert_eq!(chat.unread, 1);
        let stored = store.message(CHAT, "CALL-X1").expect("query").expect("row");
        assert_eq!(stored.text.as_deref(), Some("Missed video call"));
    }

    #[test]
    fn declined_row_does_not_count_unread() {
        let store = Store::open_in_memory().expect("store");
        let row = call_row("X2", CHAT, CHAT, 60, declined_text(false));
        assert!(insert_call_row(&store, &row, false).expect("insert"));
        assert_eq!(store.chat(CHAT).expect("query").expect("chat").unread, 0);
    }
}
