//! Rate limiting of the "something changed" UI events.
//!
//! History sync and offline drains produce bursts of `ChatsChanged` and
//! `MessagesChanged`; each makes the UI reload from the store. Per key (the
//! chat list, or one chat's messages) at most one event is delivered per
//! [`INTERVAL`]: the first goes out immediately, later ones within the window
//! collapse into a single trailing event, so the final state is always
//! announced.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::Instant;

use crate::api::UiEvent;
use crate::media::lock;

pub const INTERVAL: Duration = Duration::from_millis(250);

/// Above this many tracked keys, idle ones are dropped on insert.
const PRUNE_ABOVE: usize = 128;

#[derive(Clone, PartialEq, Eq, Hash)]
enum Key {
    Chats,
    Statuses,
    Messages(String),
}

struct Slot {
    last: Instant,
    /// A trailing event is scheduled.
    pending: bool,
}

struct Inner {
    ui: UnboundedSender<UiEvent>,
    slots: Mutex<HashMap<Key, Slot>>,
}

impl Inner {
    fn send(&self, key: Key) {
        let ev = match key {
            Key::Chats => UiEvent::ChatsChanged,
            Key::Statuses => UiEvent::StatusesChanged,
            Key::Messages(chat_jid) => UiEvent::MessagesChanged { chat_jid },
        };
        if self.ui.send(ev).is_err() {
            log::debug!("UI receiver is gone, dropping event");
        }
    }
}

#[derive(Clone)]
pub struct Coalescer {
    inner: Arc<Inner>,
}

impl Coalescer {
    pub fn new(ui: UnboundedSender<UiEvent>) -> Self {
        Coalescer {
            inner: Arc::new(Inner {
                ui,
                slots: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub fn chats_changed(&self) {
        self.fire(Key::Chats);
    }

    pub fn statuses_changed(&self) {
        self.fire(Key::Statuses);
    }

    pub fn messages_changed(&self, chat_jid: String) {
        self.fire(Key::Messages(chat_jid));
    }

    fn fire(&self, key: Key) {
        let now = Instant::now();
        let delay = {
            let mut slots = lock(&self.inner.slots);
            match slots.get_mut(&key) {
                Some(slot) if slot.pending => return,
                Some(slot) => {
                    let elapsed = now.saturating_duration_since(slot.last);
                    if elapsed >= INTERVAL {
                        slot.last = now;
                        None
                    } else {
                        slot.pending = true;
                        Some(INTERVAL - elapsed)
                    }
                }
                None => {
                    if slots.len() > PRUNE_ABOVE {
                        slots.retain(|_, s| {
                            s.pending || now.saturating_duration_since(s.last) < INTERVAL
                        });
                    }
                    slots.insert(
                        key.clone(),
                        Slot {
                            last: now,
                            pending: false,
                        },
                    );
                    None
                }
            }
        };
        let Some(delay) = delay else {
            self.inner.send(key);
            return;
        };
        let Ok(handle) = Handle::try_current() else {
            // Without a runtime there is nobody to wake us later: deliver now.
            self.finish(&key);
            return;
        };
        let this = self.clone();
        handle.spawn(async move {
            tokio::time::sleep(delay).await;
            this.finish(&key);
        });
    }

    fn finish(&self, key: &Key) {
        if let Some(slot) = lock(&self.inner.slots).get_mut(key) {
            slot.last = Instant::now();
            slot.pending = false;
        }
        self.inner.send(key.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    fn drain(rx: &mut UnboundedReceiver<UiEvent>) -> Vec<UiEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    fn count_chats(evs: &[UiEvent]) -> usize {
        evs.iter()
            .filter(|e| matches!(e, UiEvent::ChatsChanged))
            .count()
    }

    #[tokio::test(start_paused = true)]
    async fn burst_collapses_to_leading_plus_trailing() {
        let (tx, mut rx) = unbounded_channel();
        let c = Coalescer::new(tx);
        for _ in 0..50 {
            c.chats_changed();
        }
        assert_eq!(count_chats(&drain(&mut rx)), 1, "leading edge only");

        tokio::time::sleep(INTERVAL + Duration::from_millis(10)).await;
        assert_eq!(count_chats(&drain(&mut rx)), 1, "one trailing event");

        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(drain(&mut rx).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn single_event_after_quiet_period_is_immediate() {
        let (tx, mut rx) = unbounded_channel();
        let c = Coalescer::new(tx);
        c.chats_changed();
        assert_eq!(count_chats(&drain(&mut rx)), 1);
        tokio::time::sleep(Duration::from_secs(1)).await;
        c.chats_changed();
        assert_eq!(count_chats(&drain(&mut rx)), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn statuses_are_coalesced_like_chats() {
        let (tx, mut rx) = unbounded_channel();
        let c = Coalescer::new(tx);
        for _ in 0..20 {
            c.statuses_changed();
        }
        let count = |evs: Vec<UiEvent>| {
            evs.iter().filter(|e| matches!(e, UiEvent::StatusesChanged)).count()
        };
        assert_eq!(count(drain(&mut rx)), 1);
        tokio::time::sleep(INTERVAL + Duration::from_millis(10)).await;
        assert_eq!(count(drain(&mut rx)), 1);
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(drain(&mut rx).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn messages_are_coalesced_per_chat() {
        let (tx, mut rx) = unbounded_channel();
        let c = Coalescer::new(tx);
        for _ in 0..10 {
            c.messages_changed("a@s.whatsapp.net".into());
            c.messages_changed("b@s.whatsapp.net".into());
        }
        let first = drain(&mut rx);
        assert_eq!(first.len(), 2, "one leading event per chat");

        tokio::time::sleep(INTERVAL + Duration::from_millis(10)).await;
        let second = drain(&mut rx);
        let chats: Vec<_> = second
            .iter()
            .filter_map(|e| match e {
                UiEvent::MessagesChanged { chat_jid } => Some(chat_jid.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(chats.len(), 2);
        assert!(chats.contains(&"a@s.whatsapp.net") && chats.contains(&"b@s.whatsapp.net"));
    }
}
