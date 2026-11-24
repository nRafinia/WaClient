//! Revokes, edits, poll votes and stars that arrived before the message they target (history
//! sync and offline drains do not preserve ordering). They wait here, in memory,
//! until the target is stored.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use std::sync::Arc;

use whatsapp_rust::buffa::Message as _;
use whatsapp_rust::prelude::{Client, wa};
use wa_store::{MessageStatus, PendingRow, STATUS_JID};

use crate::commands::{deliver, finish, parse_jid};
use crate::convert::{PollVoteInfo, ProtocolAction};
use crate::events::{Shared, now_secs};
use crate::media::lock;

const MAX_ENTRIES: usize = 2_000;
const TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// Actions kept per target; a message is revoked once and edited a few times.
const MAX_PER_TARGET: usize = 8;

/// Stars wait longer and in a larger buffer than the rest: the phone's star
/// list reaches us through app-state sync, long before a history sync may
/// deliver the starred messages themselves.
const STAR_MAX_ENTRIES: usize = 5_000;
const STAR_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// What is waiting for its target message.
#[derive(Debug, Clone, PartialEq)]
pub enum PendingAction {
    Protocol(ProtocolAction),
    /// An encrypted poll vote (decrypted once the poll is stored); `ts` is in
    /// seconds.
    Vote { vote: PollVoteInfo, ts: i64 },
    /// A star / unstar of message `id`; `ts` (seconds) is the app-state
    /// timestamp, the newest one wins.
    Star { id: String, starred: bool, ts: i64 },
}

impl PendingAction {
    /// Id of the message this action refers to.
    pub fn target_id(&self) -> &str {
        match self {
            PendingAction::Protocol(a) => a.target_id(),
            PendingAction::Vote { vote, .. } => &vote.poll_id,
            PendingAction::Star { id, .. } => id,
        }
    }
}

/// An action waiting for its target, with the authorization data needed to
/// apply it later (`sender` is the voter for votes).
#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    pub sender: String,
    pub from_me: bool,
    pub action: PendingAction,
}

struct Slot {
    actions: Vec<Pending>,
    /// Last time an action was added; drives expiry and LRU eviction.
    touched: Instant,
}

struct StarSlot {
    starred: bool,
    ts: i64,
    touched: Instant,
}

/// Bounded buffer keyed by `(chat_jid, message id)`.
#[derive(Default)]
pub struct PendingActions {
    slots: Mutex<HashMap<(String, String), Slot>>,
    /// One entry per message: only the newest star state matters.
    stars: Mutex<HashMap<(String, String), StarSlot>>,
}

impl PendingActions {
    pub fn push(&self, chat: &str, pending: Pending) {
        self.push_at(chat, pending, Instant::now());
    }

    /// Removes and returns the live actions waiting for `(chat, id)`.
    pub fn take(&self, chat: &str, id: &str) -> Vec<Pending> {
        self.take_at(chat, id, Instant::now())
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        lock(&self.slots).len()
    }

    #[cfg(test)]
    fn star_len(&self) -> usize {
        lock(&self.stars).len()
    }

    fn push_at(&self, chat: &str, pending: Pending, now: Instant) {
        if let PendingAction::Star { id, starred, ts } = pending.action {
            self.push_star_at(chat, id, starred, ts, now);
            return;
        }
        let mut slots = lock(&self.slots);
        slots.retain(|_, s| now.saturating_duration_since(s.touched) < TTL);
        let key = (chat.to_string(), pending.action.target_id().to_string());
        if !slots.contains_key(&key) && slots.len() >= MAX_ENTRIES {
            let oldest = slots
                .iter()
                .min_by_key(|(_, s)| s.touched)
                .map(|(k, _)| k.clone());
            if let Some(oldest) = oldest {
                slots.remove(&oldest);
            }
        }
        let slot = slots.entry(key).or_insert_with(|| Slot {
            actions: Vec::new(),
            touched: now,
        });
        if slot.actions.len() >= MAX_PER_TARGET {
            slot.actions.remove(0);
        }
        slot.actions.push(pending);
        slot.touched = now;
    }

    fn push_star_at(&self, chat: &str, id: String, starred: bool, ts: i64, now: Instant) {
        let mut stars = lock(&self.stars);
        stars.retain(|_, s| now.saturating_duration_since(s.touched) < STAR_TTL);
        let key = (chat.to_string(), id);
        if let Some(existing) = stars.get(&key)
            && existing.ts > ts
        {
            return;
        }
        if !stars.contains_key(&key) && stars.len() >= STAR_MAX_ENTRIES {
            let oldest = stars
                .iter()
                .min_by_key(|(_, s)| s.touched)
                .map(|(k, _)| k.clone());
            if let Some(oldest) = oldest {
                stars.remove(&oldest);
            }
        }
        stars.insert(key, StarSlot { starred, ts, touched: now });
    }

    fn take_at(&self, chat: &str, id: &str, now: Instant) -> Vec<Pending> {
        let key = (chat.to_string(), id.to_string());
        let mut taken = {
            let mut slots = lock(&self.slots);
            match slots.remove(&key) {
                Some(slot) if now.saturating_duration_since(slot.touched) < TTL => slot.actions,
                _ => Vec::new(),
            }
        };
        let star = lock(&self.stars).remove(&key);
        if let Some(s) = star
            && now.saturating_duration_since(s.touched) < STAR_TTL
        {
            // The star is the local user's own flag, not a sender's claim.
            taken.push(Pending {
                sender: String::new(),
                from_me: true,
                action: PendingAction::Star { id: id.to_string(), starred: s.starred, ts: s.ts },
            });
        }
        taken
    }
}

// ---------------------------------------------------------------------------
// Outgoing messages that were pending when the connection was rebuilt
// ---------------------------------------------------------------------------

/// A message pending for longer than this is failed instead of re-sent after an
/// outage: it is stale for the user, and a late delivery would surprise them.
const REQUEUE_MAX_AGE_SECS: i64 = 24 * 60 * 60;

/// What happens to one outgoing message left pending by an outage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Requeue {
    /// Deliver it again under the same id (the server drops a duplicate).
    Send,
    Fail,
}

/// Only rows that have an encoded message, are not statuses and are young
/// enough are re-sent. Others (a media upload cut off before it finished,
/// view-once media that deliberately keeps no copy, polls) cannot be rebuilt.
fn plan_requeue(row: &PendingRow, now: i64) -> Requeue {
    let has_raw = row.raw.as_ref().is_some_and(|r| !r.is_empty());
    if has_raw && row.chat_jid != STATUS_JID && now.saturating_sub(row.ts) <= REQUEUE_MAX_AGE_SECS {
        Requeue::Send
    } else {
        Requeue::Fail
    }
}

/// Re-delivers the outgoing messages that stayed `Pending` across a rebuild
/// after an outage, oldest first, through the normal `deliver` path with their
/// original ids. Runs once, right after the new generation logged in; `rows`
/// is the snapshot taken when that generation started.
pub(crate) async fn requeue_outgoing(sh: Shared, client: Arc<Client>, rows: Vec<PendingRow>) {    let now = now_secs();
    let mut sent = 0usize;
    for row in rows {
        let message = match plan_requeue(&row, now) {
            Requeue::Send => row.raw.as_deref().and_then(|raw| {
                wa::Message::decode_from_slice(raw)
                    .map_err(|e| log::warn!("pending message {} does not decode: {e}", row.id))
                    .ok()
            }),
            Requeue::Fail => None,
        };
        let chat = message.as_ref().and_then(|_| parse_jid(&sh, &row.chat_jid));
        match (message, chat) {
            (Some(message), Some(chat)) => {
                sent += 1;
                deliver(&sh, &client, chat, &row.chat_jid, row.id, message).await;
            }
            _ => finish(&sh, &row.chat_jid, row.id, MessageStatus::Failed),
        }
    }
    if sent > 0 {
        log::info!("re-delivered {sent} message(s) that were pending during the outage");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(chat: &str, ts: i64, raw: Option<&[u8]>) -> PendingRow {
        PendingRow { chat_jid: chat.into(), id: "id".into(), ts, raw: raw.map(<[u8]>::to_vec) }
    }

    #[test]
    fn requeue_resends_only_young_rows_with_an_encoded_message() {
        let now = 1_000_000;
        let chat = "1@s.whatsapp.net";
        assert_eq!(plan_requeue(&row(chat, now - 60, Some(b"x")), now), Requeue::Send);
        assert_eq!(plan_requeue(&row(chat, now - REQUEUE_MAX_AGE_SECS, Some(b"x")), now), Requeue::Send);
        assert_eq!(plan_requeue(&row(chat, now - REQUEUE_MAX_AGE_SECS - 1, Some(b"x")), now), Requeue::Fail);
        assert_eq!(plan_requeue(&row(chat, now, None), now), Requeue::Fail, "upload never finished");
        assert_eq!(plan_requeue(&row(chat, now, Some(b"")), now), Requeue::Fail);
        assert_eq!(plan_requeue(&row(STATUS_JID, now, Some(b"x")), now), Requeue::Fail);
    }

    fn revoke(id: &str) -> Pending {
        Pending {
            sender: "s@s.whatsapp.net".into(),
            from_me: false,
            action: PendingAction::Protocol(ProtocolAction::Revoke { id: id.into() }),
        }
    }

    fn edit(id: &str, text: &str) -> Pending {
        Pending {
            sender: "s@s.whatsapp.net".into(),
            from_me: false,
            action: PendingAction::Protocol(ProtocolAction::Edit {
                id: id.into(),
                text: text.into(),
            }),
        }
    }

    fn vote(poll_id: &str) -> Pending {
        Pending {
            sender: "v@s.whatsapp.net".into(),
            from_me: false,
            action: PendingAction::Vote {
                vote: PollVoteInfo {
                    poll_id: poll_id.into(),
                    enc_payload: vec![1],
                    enc_iv: vec![2],
                    ts_ms: None,
                },
                ts: 7,
            },
        }
    }

    #[test]
    fn votes_wait_for_their_poll_like_other_actions() {
        let p = PendingActions::default();
        p.push("c", vote("P"));
        p.push("c", revoke("P"));
        assert!(p.take("c", "Q").is_empty());
        assert_eq!(p.take("c", "P"), vec![vote("P"), revoke("P")]);

        let t0 = Instant::now();
        p.push_at("c", vote("P"), t0);
        assert!(p.take_at("c", "P", t0 + TTL + Duration::from_secs(1)).is_empty());
        for _ in 0..(MAX_PER_TARGET + 2) {
            p.push("c", vote("P"));
        }
        assert_eq!(p.take("c", "P").len(), MAX_PER_TARGET);
    }

    #[test]
    fn take_returns_what_was_buffered_once() {
        let p = PendingActions::default();
        p.push("c", edit("A", "one"));
        p.push("c", revoke("A"));
        assert!(p.take("c", "B").is_empty());
        assert!(p.take("other", "A").is_empty());
        assert_eq!(p.take("c", "A"), vec![edit("A", "one"), revoke("A")]);
        assert!(p.take("c", "A").is_empty());
        assert_eq!(p.len(), 0);
    }

    #[test]
    fn expired_entries_are_dropped() {
        let p = PendingActions::default();
        let t0 = Instant::now();
        p.push_at("c", revoke("A"), t0);
        p.push_at("c", revoke("B"), t0);
        let late = t0 + TTL + Duration::from_secs(1);
        assert!(p.take_at("c", "A", late).is_empty());
        // A later push sweeps the other expired entry too.
        p.push_at("c", revoke("C"), late);
        assert_eq!(p.len(), 1);
        assert_eq!(p.take_at("c", "C", late), vec![revoke("C")]);
    }

    #[test]
    fn entries_within_ttl_survive() {
        let p = PendingActions::default();
        let t0 = Instant::now();
        p.push_at("c", revoke("A"), t0);
        let soon = t0 + TTL - Duration::from_secs(1);
        assert_eq!(p.take_at("c", "A", soon), vec![revoke("A")]);
    }

    #[test]
    fn oldest_entry_is_evicted_at_the_cap() {
        let p = PendingActions::default();
        let t0 = Instant::now();
        for i in 0..MAX_ENTRIES {
            let at = t0 + Duration::from_millis(u64::try_from(i).expect("fits"));
            p.push_at("c", revoke(&format!("id{i}")), at);
        }
        assert_eq!(p.len(), MAX_ENTRIES);
        // Touching id0 again makes id1 the least recently used.
        let later = t0 + Duration::from_secs(10);
        p.push_at("c", edit("id0", "x"), later);
        p.push_at("c", revoke("new"), later);
        assert_eq!(p.len(), MAX_ENTRIES);
        assert!(p.take_at("c", "id1", later).is_empty());
        assert_eq!(p.take_at("c", "id0", later).len(), 2);
        assert_eq!(p.take_at("c", "new", later), vec![revoke("new")]);
    }

    #[test]
    fn per_target_actions_are_bounded() {
        let p = PendingActions::default();
        for i in 0..(MAX_PER_TARGET + 3) {
            p.push("c", edit("A", &format!("v{i}")));
        }
        let got = p.take("c", "A");
        assert_eq!(got.len(), MAX_PER_TARGET);
        assert_eq!(got.last(), Some(&edit("A", &format!("v{}", MAX_PER_TARGET + 2))));
    }

    fn star(id: &str, starred: bool, ts: i64) -> Pending {
        Pending {
            sender: String::new(),
            from_me: true,
            action: PendingAction::Star { id: id.into(), starred, ts },
        }
    }

    #[test]
    fn newest_star_state_wins() {
        let p = PendingActions::default();
        p.push("c", star("A", true, 10));
        p.push("c", star("A", false, 20));
        assert_eq!(p.take("c", "A"), vec![star("A", false, 20)]);

        p.push("c", star("A", false, 20));
        p.push("c", star("A", true, 10));
        assert_eq!(p.take("c", "A"), vec![star("A", false, 20)]);
        assert!(p.take("c", "A").is_empty());
    }

    #[test]
    fn stars_outlive_other_actions_but_expire_after_a_week() {
        let p = PendingActions::default();
        let t0 = Instant::now();
        p.push_at("c", star("A", true, 1), t0);
        p.push_at("c", revoke("B"), t0);
        let day2 = t0 + TTL + Duration::from_secs(1);
        assert!(p.take_at("c", "B", day2).is_empty());
        assert_eq!(p.take_at("c", "A", day2), vec![star("A", true, 1)]);

        p.push_at("c", star("A", true, 1), t0);
        p.push_at("c", star("B", true, 1), t0);
        let late = t0 + STAR_TTL + Duration::from_secs(1);
        assert!(p.take_at("c", "A", late).is_empty());
        // A later push sweeps the other expired star too.
        p.push_at("c", star("C", true, 1), late);
        assert_eq!(p.star_len(), 1);
    }

    #[test]
    fn oldest_star_is_evicted_at_the_cap() {
        let p = PendingActions::default();
        let t0 = Instant::now();
        for i in 0..STAR_MAX_ENTRIES {
            let at = t0 + Duration::from_millis(u64::try_from(i).expect("fits"));
            p.push_at("c", star(&format!("id{i}"), true, 1), at);
        }
        assert_eq!(p.star_len(), STAR_MAX_ENTRIES);
        let later = t0 + Duration::from_secs(10);
        p.push_at("c", star("new", true, 1), later);
        assert_eq!(p.star_len(), STAR_MAX_ENTRIES);
        assert!(p.take_at("c", "id0", later).is_empty());
        assert_eq!(p.take_at("c", "new", later), vec![star("new", true, 1)]);
    }
}
