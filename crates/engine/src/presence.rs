//! Contacts' online / last-seen state.
//!
//! WhatsApp only pushes a contact's presence after we subscribed to it, and
//! may withhold `last_seen` when the contact's (or our own) privacy settings
//! hide it; the UI then gets `last_seen: None`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use whatsapp_rust::prelude::{Client, Jid};
use whatsapp_rust::wacore::types::events::PresenceUpdate;
use whatsapp_rust::wacore_binary::JidExt as _;

use crate::api::UiEvent;
use crate::events::Shared;
use crate::jid::{self, redact};

/// A jid is not subscribed again within this window.
const RESUBSCRIBE_AFTER: Duration = Duration::from_secs(5 * 60);
/// Minimum spacing between two subscribe stanzas.
const SUBSCRIBE_GAP: Duration = Duration::from_millis(300);

#[derive(Default)]
struct Inner {
    /// Last known `(online, last_seen)` per jid.
    known: HashMap<String, (bool, Option<i64>)>,
    /// When each jid was last subscribed.
    subscribed: HashMap<String, Instant>,
    /// Earliest start of the next subscribe stanza.
    next_slot: Option<Instant>,
}

/// Presence bookkeeping of one engine generation.
#[derive(Default)]
pub struct PresenceState {
    inner: Mutex<Inner>,
}

impl PresenceState {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Whether a subscription made at `last` is due again at `now`.
pub fn subscribe_due(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|t| now.saturating_duration_since(t) >= RESUBSCRIBE_AFTER)
}

/// Reserves the next pacing slot: returns `(start, next_earliest)`.
pub fn pace_slot(now: Instant, earliest: Option<Instant>) -> (Instant, Instant) {
    let start = earliest.map_or(now, |e| e.max(now));
    (start, start + SUBSCRIBE_GAP)
}

/// The UI event for a presence update; `online` is the opposite of
/// `unavailable`.
pub fn ui_event(jid: String, unavailable: bool, last_seen: Option<i64>) -> UiEvent {
    UiEvent::Presence {
        jid,
        online: !unavailable,
        last_seen,
    }
}

/// Handles an incoming presence update: remembers it and tells the UI.
pub async fn on_presence(sh: &Shared, client: &Client, u: &PresenceUpdate) {
    let who = jid::normalize(client, &u.from, None).await;
    let last_seen = u.last_seen.map(|t| t.timestamp());
    sh.presence.lock().known.insert(who.clone(), (!u.unavailable, last_seen));
    sh.emit(ui_event(who, u.unavailable, last_seen));
}

/// Subscribes to a DM contact's presence (paced, at most every 5 minutes per
/// jid) and replays the last known state at once.
pub async fn subscribe(sh: &Shared, client: &Arc<Client>, raw: &str) {
    let Ok(parsed) = raw.parse::<Jid>() else {
        log::warn!("presence subscribe: invalid jid {}", redact(raw));
        return;
    };
    if parsed.is_group() {
        return;
    }
    let who = jid::normalize(client, &parsed, None).await;
    let wait = {
        let mut st = sh.presence.lock();
        if let Some(&(online, last_seen)) = st.known.get(&who) {
            sh.emit(ui_event(who.clone(), !online, last_seen));
        }
        let now = Instant::now();
        if !subscribe_due(st.subscribed.get(&who).copied(), now) {
            return;
        }
        st.subscribed.insert(who.clone(), now);
        let (start, next) = pace_slot(now, st.next_slot);
        st.next_slot = Some(next);
        start.saturating_duration_since(now)
    };
    if !wait.is_zero() {
        tokio::time::sleep(wait).await;
    }
    let target = who.parse::<Jid>().unwrap_or(parsed);
    if let Err(e) = client.presence().subscribe(target).await {
        log::debug!("presence subscribe for {} failed: {e}", redact(&who));
        // Let a later request retry instead of waiting out the window.
        sh.presence.lock().subscribed.remove(&who);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resubscribe_only_after_the_window() {
        let t0 = Instant::now();
        assert!(subscribe_due(None, t0));
        assert!(!subscribe_due(Some(t0), t0 + Duration::from_secs(299)));
        assert!(subscribe_due(Some(t0), t0 + Duration::from_secs(300)));
    }

    #[test]
    fn subscribes_are_spaced_by_the_gap() {
        let t0 = Instant::now();
        let (s1, n1) = pace_slot(t0, None);
        assert_eq!(s1, t0);
        let (s2, n2) = pace_slot(t0, Some(n1));
        assert_eq!(s2 - s1, SUBSCRIBE_GAP);
        let later = t0 + Duration::from_secs(10);
        let (s3, _) = pace_slot(later, Some(n2));
        assert_eq!(s3, later, "an idle limiter does not delay");
    }

    #[test]
    fn maps_updates_to_ui_events() {
        let UiEvent::Presence { jid, online, last_seen } = ui_event("1@s.whatsapp.net".into(), false, Some(42)) else {
            panic!("wrong event");
        };
        assert_eq!((jid.as_str(), online, last_seen), ("1@s.whatsapp.net", true, Some(42)));
        let UiEvent::Presence { online, last_seen, .. } = ui_event("1@s.whatsapp.net".into(), true, None) else {
            panic!("wrong event");
        };
        assert!(!online && last_seen.is_none());
    }
}
