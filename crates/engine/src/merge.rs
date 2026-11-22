//! LID -> phone-number chat merging.
//!
//! A chat or contact may be created under a `<n>@lid` JID before the
//! LID<->PN mapping is known. Whenever a mapping shows up, whatever exists
//! under the LID is folded into the phone-number entry.

use std::collections::HashSet;
use std::sync::Mutex;

use whatsapp_rust::prelude::Jid;
use wa_store::Store;

use crate::jid::redact;
use crate::media::lock;

#[derive(Default)]
pub struct LidMerges {
    /// LIDs already handled, so repeated hints cost a hash lookup only.
    done: Mutex<HashSet<String>>,
}

/// `user@server` form of a JID string without the device suffix.
fn non_ad(jid: &str) -> Option<Jid> {
    jid.parse::<Jid>().ok().map(|j| j.to_non_ad())
}

impl LidMerges {
    /// Merges `lid` into `pn` once per LID. Returns whether stored data moved.
    pub fn learn(&self, store: &Store, lid: &str, pn: &str) -> bool {
        let (Some(lid), Some(pn)) = (non_ad(lid), non_ad(pn)) else {
            return false;
        };
        if !lid.is_lid() || !pn.is_pn() {
            return false;
        }
        let (lid, pn) = (lid.to_string(), pn.to_string());
        if !lock(&self.done).insert(lid.clone()) {
            return false;
        }
        match store.merge_chat(&lid, &pn) {
            Ok(moved) => {
                if moved {
                    log::info!("merged {} into {}", redact(&lid), redact(&pn));
                }
                moved
            }
            Err(e) => {
                log::error!("merge_chat {} -> {} failed: {e}", redact(&lid), redact(&pn));
                // Allow a retry on the next hint.
                lock(&self.done).remove(&lid);
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wa_store::{ChatKind, ContactPatch};

    #[test]
    fn merges_once_and_ignores_bad_input() {
        let store = Store::open_in_memory().expect("store");
        store
            .upsert_chat("100@lid", ChatKind::Dm, Some("Ann"))
            .expect("chat");
        store
            .upsert_contact(&ContactPatch {
                jid: "100@lid".into(),
                full_name: None,
                push_name: Some("Ann".into()),
                phone: None,
            })
            .expect("contact");
        let merges = LidMerges::default();

        assert!(!merges.learn(&store, "100@s.whatsapp.net", "5511@s.whatsapp.net"));
        assert!(!merges.learn(&store, "100@lid", "200@lid"));
        assert!(merges.learn(&store, "100:7@lid", "5511@s.whatsapp.net"));
        assert!(store.chat("100@lid").expect("q").is_none());
        assert_eq!(
            store.chat("5511@s.whatsapp.net").expect("q").expect("chat").display_name,
            "Ann"
        );
        assert!(store.contact("100@lid").expect("q").is_none());

        store
            .upsert_chat("100@lid", ChatKind::Dm, None)
            .expect("chat again");
        assert!(!merges.learn(&store, "100@lid", "5511@s.whatsapp.net"), "already handled");
    }
}
