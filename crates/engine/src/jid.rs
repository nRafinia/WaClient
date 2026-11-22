//! JID normalization: one person = one chat.
//!
//! WhatsApp increasingly addresses users by LID (`<n>@lid`) instead of phone
//! number (`<n>@s.whatsapp.net`). The store keys chats and contacts by string,
//! so every user JID is canonicalised to the phone-number form whenever the
//! library's LID<->PN mapping knows it. Device suffixes (`:12`) are always
//! dropped. When no mapping is known yet the LID is kept as is.

use std::cell::RefCell;
use std::collections::HashMap;

use tokio::runtime::Handle;
use whatsapp_rust::prelude::{Client, Jid, MessageInfo};
use whatsapp_rust::wacore_binary::JidExt as _;

/// Phone-number hints carried by an inbound message itself (no lookup needed).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Hints {
    /// PN form of the chat, for DMs.
    pub chat: Option<Jid>,
    /// PN form of the sender.
    pub sender: Option<Jid>,
}

/// Extracts the PN counterparts the stanza already told us about.
pub fn hints(info: &MessageInfo) -> Hints {
    let src = &info.source;
    let pn = |j: &Option<Jid>| j.as_ref().filter(|j| j.is_pn()).map(Jid::to_non_ad);
    let sender = pn(&src.sender_alt);
    let chat = if src.is_group || src.chat.is_group() {
        None
    } else if src.is_from_me {
        pn(&src.recipient_alt)
    } else {
        sender.clone()
    };
    Hints { chat, sender }
}

/// Log-safe form of a JID string: keeps the server part and only the last two
/// characters of the user (device suffix dropped), e.g. `"…42@s.whatsapp.net"`.
/// Users of two characters or fewer are fully hidden.
pub fn redact(jid: &str) -> String {
    let (user, server) = match jid.split_once('@') {
        Some((user, server)) => (user, Some(server)),
        None => (jid, None),
    };
    let user = user.split(':').next().unwrap_or(user);
    let count = user.chars().count();
    let tail: String = if count > 2 {
        user.chars().skip(count - 2).collect()
    } else {
        String::new()
    };
    match server {
        Some(server) => format!("…{tail}@{server}"),
        None => format!("…{tail}"),
    }
}

/// Coarse, JID-free class of a library error, for logs at info and above:
/// library error texts can embed the addresses involved.
pub fn classify_error(e: &impl std::fmt::Display) -> &'static str {
    let text = e.to_string().to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| text.contains(n));
    if has(&["timed out", "timeout"]) {
        "timeout"
    } else if has(&["not connected", "disconnected", "connection closed", "socket"]) {
        "not connected"
    } else if has(&["rate-limit", "rate limit", "429", "overlimit"]) {
        "rate limited"
    } else if has(&["not found", "404", "item-not-found"]) {
        "not found"
    } else if has(&["forbidden", "not-authorized", "401", "403"]) {
        "not authorized"
    } else if has(&["decrypt", "no session", "invalid message", "bad mac"]) {
        "crypto"
    } else {
        "other"
    }
}

/// Logs a library failure: the short class at warn, the full text at debug.
pub fn warn_lib_error(context: &str, e: &impl std::fmt::Display) {
    log::warn!("{context}: {}", classify_error(e));
    log::debug!("{context}: {e}");
}

/// Canonical string for a user/chat JID, using `hint` before the library cache.
pub async fn normalize(client: &Client, jid: &Jid, hint: Option<&Jid>) -> String {
    let base = jid.to_non_ad();
    if !base.is_lid() {
        return base.to_string();
    }
    if let Some(pn) = hint.filter(|h| h.is_pn()) {
        return pn.to_non_ad().to_string();
    }
    match client.get_lid_pn_entry(&base).await {
        Ok(Some(entry)) => Jid::pn(&*entry.phone_number).to_string(),
        Ok(None) => base.to_string(),
        Err(e) => {
            log::debug!("LID lookup failed for {}: {e}", base.user);
            base.to_string()
        }
    }
}

/// Same as [`normalize`] for a JID given as a string; unparsable input is
/// returned unchanged.
pub async fn normalize_str(client: &Client, jid: &str) -> String {
    match jid.parse::<Jid>() {
        Ok(j) => normalize(client, &j, None).await,
        Err(_) => jid.to_string(),
    }
}

/// Blocking resolver for use inside `spawn_blocking` (history sync), with a
/// per-instance cache so each LID is looked up once.
pub struct SyncResolver {
    client: std::sync::Arc<Client>,
    handle: Handle,
    cache: RefCell<HashMap<String, String>>,
}

impl SyncResolver {
    /// Must be created from within the runtime; `resolve` must be called from a
    /// blocking (non-async) thread.
    pub fn new(client: std::sync::Arc<Client>) -> Self {
        SyncResolver {
            client,
            handle: Handle::current(),
            cache: RefCell::new(HashMap::new()),
        }
    }

    pub fn resolve(&self, jid: &str) -> String {
        if !jid.ends_with("@lid") && !jid.contains(':') {
            return jid.to_string();
        }
        if let Some(hit) = self.cache.borrow().get(jid) {
            return hit.clone();
        }
        let out = self.handle.block_on(normalize_str(&self.client, jid));
        self.cache.borrow_mut().insert(jid.to_string(), out.clone());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jid(s: &str) -> Jid {
        s.parse().expect("valid jid")
    }

    #[test]
    fn dm_from_other_uses_sender_alt_for_chat_and_sender() {
        let mut info = MessageInfo::default();
        info.source.chat = jid("100@lid");
        info.source.sender = jid("100@lid");
        info.source.sender_alt = Some(jid("5511@s.whatsapp.net"));
        let h = hints(&info);
        assert_eq!(h.chat, Some(jid("5511@s.whatsapp.net")));
        assert_eq!(h.sender, Some(jid("5511@s.whatsapp.net")));
    }

    #[test]
    fn dm_from_me_uses_recipient_alt_for_chat() {
        let mut info = MessageInfo::default();
        info.source.is_from_me = true;
        info.source.chat = jid("100@lid");
        info.source.sender = jid("200@lid");
        info.source.sender_alt = Some(jid("999@s.whatsapp.net"));
        info.source.recipient_alt = Some(jid("5511@s.whatsapp.net"));
        let h = hints(&info);
        assert_eq!(h.chat, Some(jid("5511@s.whatsapp.net")));
    }

    #[test]
    fn group_has_no_chat_hint() {
        let mut info = MessageInfo::default();
        info.source.is_group = true;
        info.source.chat = jid("g1@g.us");
        info.source.sender = jid("100@lid");
        info.source.sender_alt = Some(jid("5511@s.whatsapp.net"));
        let h = hints(&info);
        assert_eq!(h.chat, None);
        assert_eq!(h.sender, Some(jid("5511@s.whatsapp.net")));
    }

    #[test]
    fn error_class_never_echoes_the_text() {
        assert_eq!(classify_error(&"request to 4917011@s.whatsapp.net timed out"), "timeout");
        assert_eq!(classify_error(&"client is not connected"), "not connected");
        assert_eq!(classify_error(&"item-not-found 4917011@s.whatsapp.net"), "not found");
        assert_eq!(classify_error(&"weird 4917011@s.whatsapp.net"), "other");
        assert!(!classify_error(&"weird 4917011@s.whatsapp.net").contains("4917"));
    }

    #[test]
    fn redact_keeps_server_and_last_two_characters() {
        assert_eq!(redact("491701234542@s.whatsapp.net"), "…42@s.whatsapp.net");
        assert_eq!(redact("491701234542:7@s.whatsapp.net"), "…42@s.whatsapp.net");
        assert_eq!(redact("12345-6789@g.us"), "…89@g.us");
        assert_eq!(redact("100@lid"), "…00@lid");
        assert_eq!(redact("42@lid"), "…@lid");
        assert_eq!(redact("491701234542"), "…42");
        assert_eq!(redact(""), "…");
        assert!(!redact("491701234542@s.whatsapp.net").contains("4917"));
    }
}
