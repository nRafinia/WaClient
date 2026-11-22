//! Phone numbers in message text: a click on one (a `wa-phone:` link, see
//! `links.rs`) opens the bubble's small menu; this file answers which number
//! the link stands for and handles "Look up profile".

use slint::ComponentHandle;

use super::{App, with_app};
use crate::{MainWindow, PhoneLinks, links};

pub(super) fn wire(win: &MainWindow) {
    win.global::<PhoneLinks>().on_phone_of(|target| {
        let mut digits = String::new();
        with_app(|a| digits = a.phone_of(target.as_str()));
        digits.into()
    });
    win.on_phone_lookup(|digits| with_app(|a| a.lookup_phone(digits.as_str())));
}

/// Length of the country code at the start of `digits` (an E.164 number
/// without `+`). Country codes are prefix-free, so the first digits decide.
fn country_code_len(digits: &str) -> Option<usize> {
    let b = digits.as_bytes();
    let two = |s: &str| digits.get(..2).is_some_and(|d| s.split(' ').any(|c| c == d));
    match *b.first()? {
        b'1' | b'7' => Some(1),
        b'2' => Some(if two("20 27") { 2 } else { 3 }),
        b'3' => Some(if two("30 31 32 33 34 36 39") { 2 } else { 3 }),
        b'4' => Some(if two("40 41 43 44 45 46 47 48 49") { 2 } else { 3 }),
        b'5' => Some(if two("51 52 53 54 55 56 57 58") { 2 } else { 3 }),
        b'6' => Some(if two("60 61 62 63 64 65 66") { 2 } else { 3 }),
        b'8' => Some(if two("81 82 84 86") { 2 } else { 3 }),
        b'9' => Some(if two("90 91 92 93 94 95 98") { 2 } else { 3 }),
        _ => None,
    }
}

/// Country code of the account's own number, from its JID
/// (`98912…:7@s.whatsapp.net`).
fn own_country_code(own_jid: &str) -> Option<&str> {
    let user = own_jid.split(['@', ':']).next()?;
    if user.is_empty() || !user.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    user.get(..country_code_len(user)?)
}

impl App {
    /// The number behind a phone link target, with the own country code
    /// applied to local numbers; empty when `target` is not a phone link.
    pub(super) fn phone_of(&self, target: &str) -> String {
        let Some(raw) = links::phone_of_target(target) else { return String::new() };
        let own_jid = self.p().meta.borrow().own_jid.clone();
        let cc = own_jid.as_deref().and_then(own_country_code);
        links::normalize_phone(raw, cc).unwrap_or_default()
    }

    /// "Look up profile": asks the engine whether the number is on WhatsApp.
    pub(super) fn lookup_phone(&self, digits: &str) {
        if digits.starts_with('0') {
            self.show_toast("Add the country code to look this number up");
            return;
        }
        self.lookup_and_open(digits);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn country_code_of_own_jid() {
        assert_eq!(own_country_code("989123456789@s.whatsapp.net"), Some("98"));
        assert_eq!(own_country_code("989123456789:12@s.whatsapp.net"), Some("98"));
        assert_eq!(own_country_code("14155550100@s.whatsapp.net"), Some("1"));
        assert_eq!(own_country_code("447911123456@s.whatsapp.net"), Some("44"));
        assert_eq!(own_country_code("971501234567@s.whatsapp.net"), Some("971"));
        assert_eq!(own_country_code("79123456789@s.whatsapp.net"), Some("7"));
        assert_eq!(own_country_code("0123@s.whatsapp.net"), None);
        assert_eq!(own_country_code("abc@lid"), None);
        assert_eq!(own_country_code(""), None);
    }
}
