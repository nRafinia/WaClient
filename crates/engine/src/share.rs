//! Sharing: contact cards, phone-number lookup and locations.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use whatsapp_rust::buffa::MessageField;
use whatsapp_rust::prelude::{Client, Jid, wa};

use crate::api::{PhoneLookupResult, SharedContact, UiEvent};
use crate::commands::{NOT_LOGGED_IN, deliver, parse_jid, reject, resolve_quote, store_pending};
use crate::events::{Shared, now_secs, own_jid};
use crate::jid::redact;
use crate::convert;

const MAX_CONTACTS: usize = 20;
const MAX_NAME_CHARS: usize = 100;
const MAX_PHONES: usize = 5;
const MAX_PHONE_CHARS: usize = 32;
const MAX_PLACE_TEXT_CHARS: usize = 256;

const MIN_DIGITS: usize = 7;
const MAX_DIGITS: usize = 15;
const LOOKUP_BATCH: usize = 50;
const MAX_LOOKUP_NUMBERS: usize = 20;
/// Distinct uncached numbers sent to the server per rolling window.
const LOOKUPS_PER_10_MIN: usize = 60;
const LOOKUPS_PER_DAY: usize = 300;
const SHORT_WINDOW: Duration = Duration::from_secs(10 * 60);
const LONG_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);
const LOOKUP_SPACING: Duration = Duration::from_secs(2);
const LOOKUP_TTL: Duration = Duration::from_secs(10 * 60);
const CACHE_PRUNE_AT: usize = 2_000;

const LOOKUP_FAILED: &str = "Couldn't look up the number";
const LOOKUP_TOO_MANY: &str = "Too many numbers at once";
const LOOKUP_BUDGET_SPENT: &str = "Too many lookups — try again later";

// ---- vCard / contact messages ----

/// Escapes a vCard 3.0 text value.
fn escape_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.push_str("\\\\"),
            ',' => out.push_str("\\,"),
            ';' => out.push_str("\\;"),
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push_str("\\n");
            }
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out
}

/// Digits of a number written as digits with an optional leading "+";
/// `None` for anything else (spaces, dashes, letters).
fn plain_digits(phone: &str) -> Option<&str> {
    let d = phone.strip_prefix('+').unwrap_or(phone);
    (!d.is_empty() && d.chars().all(|c| c.is_ascii_digit())).then_some(d)
}

/// The digits to put in `waid=`: only for numbers that are certainly
/// international, i.e. written with "+" or at least 10 digits, and never with a
/// leading 0 (a local number whose country is unknown).
fn waid_digits(phone: &str) -> Option<&str> {
    let d = plain_digits(phone)?;
    let international = phone.starts_with('+') || d.len() >= 10;
    (international && !d.starts_with('0')).then_some(d)
}

fn vcard(name: &str, phones: &[String]) -> String {
    let name = escape_value(name);
    let mut v = format!("BEGIN:VCARD\r\nVERSION:3.0\r\nN:;{name};;;\r\nFN:{name}\r\n");
    for phone in phones {
        let phone = phone.trim();
        match waid_digits(phone) {
            Some(d) => v.push_str(&format!("TEL;type=CELL;type=VOICE;waid={d}:+{d}\r\n")),
            None => {
                let shown: String = phone.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
                v.push_str(&format!("TEL;type=CELL;type=VOICE:{}\r\n", escape_value(&shown)));
            }
        }
    }
    v.push_str("END:VCARD\r\n");
    v
}

fn validate_contacts(contacts: &[SharedContact]) -> Result<(), String> {
    if contacts.is_empty() {
        return Err("Choose a contact to share".to_string());
    }
    if contacts.len() > MAX_CONTACTS {
        return Err(format!("You can share up to {MAX_CONTACTS} contacts at once"));
    }
    for c in contacts {
        let name = c.name.trim();
        if name.is_empty() {
            return Err("A shared contact needs a name".to_string());
        }
        if name.chars().count() > MAX_NAME_CHARS {
            return Err(format!("Contact names can have up to {MAX_NAME_CHARS} characters"));
        }
        let phones: Vec<&str> = c.phones.iter().map(|p| p.trim()).filter(|p| !p.is_empty()).collect();
        if phones.is_empty() {
            return Err("A shared contact needs a phone number".to_string());
        }
        if phones.len() > MAX_PHONES {
            return Err(format!("A contact can have up to {MAX_PHONES} phone numbers"));
        }
        if phones.iter().any(|p| p.chars().count() > MAX_PHONE_CHARS) {
            return Err("A phone number is too long".to_string());
        }
    }
    Ok(())
}

fn contact_message(c: &SharedContact) -> wa::message::ContactMessage {
    let name = c.name.trim();
    let phones: Vec<String> = c.phones.iter().map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect();
    wa::message::ContactMessage {
        display_name: Some(name.to_string()),
        vcard: Some(vcard(name, &phones)),
        ..Default::default()
    }
}

/// The message carrying `contacts`: a single card or a card array.
fn build_contact_message(contacts: &[SharedContact], context: Option<wa::ContextInfo>) -> Result<wa::Message, String> {
    validate_contacts(contacts)?;
    let context = context.map_or_else(MessageField::none, MessageField::some);
    Ok(match contacts {
        [only] => wa::Message {
            contact_message: MessageField::some(wa::message::ContactMessage {
                context_info: context,
                ..contact_message(only)
            }),
            ..Default::default()
        },
        many => wa::Message {
            contacts_array_message: MessageField::some(wa::message::ContactsArrayMessage {
                display_name: Some(format!("{} contacts", many.len())),
                contacts: many.iter().map(contact_message).collect(),
                context_info: context,
            }),
            ..Default::default()
        },
    })
}

pub(crate) async fn send_contact(
    sh: &Shared,
    client: &Arc<Client>,
    chat_jid: &str,
    contacts: &[SharedContact],
    quoted_id: Option<&str>,
) {
    if let Err(message) = validate_contacts(contacts) {
        reject(sh, message);
        return;
    }
    let Some(chat) = parse_jid(sh, chat_jid) else { return };
    let Some(sender_jid) = own_jid(client) else {
        reject(sh, NOT_LOGGED_IN.to_string());
        return;
    };
    let quote = resolve_quote(sh, &chat, chat_jid, quoted_id);
    let message = match build_contact_message(contacts, quote.context) {
        Ok(m) => m,
        Err(e) => {
            reject(sh, e);
            return;
        }
    };
    let id = client.generate_message_id();
    let Some(pending) = convert::outgoing(&message, &id, chat_jid, &sender_jid, now_secs()) else {
        log::error!("built contact message is not displayable");
        reject(sh, "Could not share the contact".to_string());
        return;
    };
    if !store_pending(sh, &pending) {
        return;
    }
    deliver(sh, client, chat, chat_jid, id, message).await;
}

// ---- phone lookup ----

/// Digits of an international phone number as typed ("+98 912-123 4567");
/// `None` unless it has 7 to 15 digits, nothing but separators besides, and
/// does not start with 0: a leading 0 is a local number whose country code the
/// UI has to add first.
fn normalize_number(raw: &str) -> Option<String> {
    let mut digits = String::new();
    for c in raw.trim().chars() {
        match c {
            '0'..='9' => digits.push(c),
            '+' | ' ' | '-' | '(' | ')' | '.' | '\u{a0}' => {}
            _ => return None,
        }
    }
    ((MIN_DIGITS..=MAX_DIGITS).contains(&digits.len()) &&!digits.starts_with('0')).then_some(digits)
}

/// What the server said about one number.
#[derive(Debug, Clone, PartialEq, Default)]
struct Found {
    jid: Option<String>,
    is_business: bool,
    verified_name: Option<String>,
}

/// Pacing and cache of lookups.
#[derive(Default)]
struct LookupGuard {
    next_slot: Option<Instant>,
    cache: HashMap<String, (Instant, Found)>,
    /// When each number sent to the server was booked, oldest first.
    booked: VecDeque<Instant>,
}

impl LookupGuard {
    /// Books `count` numbers against the rolling 10-minute and 24-hour budgets;
    /// `false` (and nothing booked) when either would be exceeded.
    fn admit(&mut self, count: usize, now: Instant) -> bool {
        while self.booked.front().is_some_and(|t| now.saturating_duration_since(*t) >= LONG_WINDOW) {
            self.booked.pop_front();
        }
        let recent = self.booked.iter().filter(|t| now.saturating_duration_since(**t) < SHORT_WINDOW).count();
        if recent + count > LOOKUPS_PER_10_MIN || self.booked.len() + count > LOOKUPS_PER_DAY {
            return false;
        }
        self.booked.extend(std::iter::repeat_n(now, count));
        true
    }

    fn cached(&self, digits: &str, now: Instant) -> Option<Found> {
        let (at, found) = self.cache.get(digits)?;
        (now.saturating_duration_since(*at) < LOOKUP_TTL).then(|| found.clone())
    }

    fn remember(&mut self, digits: String, found: Found, now: Instant) {
        if self.cache.len() >= CACHE_PRUNE_AT {
            self.cache.retain(|_, (at, _)| now.saturating_duration_since(*at) < LOOKUP_TTL);
        }
        self.cache.insert(digits, (now, found));
    }

    /// Books the next server call and returns how long to wait before making it.
    fn reserve(&mut self, now: Instant) -> Duration {
        let slot = self.next_slot.map_or(now, |s| s.max(now));
        self.next_slot = Some(slot + LOOKUP_SPACING);
        slot.saturating_duration_since(now)
    }
}

static LOOKUPS: std::sync::LazyLock<Mutex<LookupGuard>> = std::sync::LazyLock::new(Default::default);

fn guard() -> std::sync::MutexGuard<'static, LookupGuard> {
    LOOKUPS.lock().unwrap_or_else(|p| p.into_inner())
}

/// Unique digit strings of `numbers` that are valid and not cached, in order.
fn to_fetch(numbers: &[Option<String>], guard: &LookupGuard, now: Instant) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for d in numbers.iter().flatten() {
        if guard.cached(d, now).is_none() && !out.contains(d) {
            out.push(d.clone());
        }
    }
    out
}

/// The phone number and chat of an `is_on_whatsapp` answer; the server may
/// answer with a LID and only mention the phone number as `pn_jid`.
fn answer_key(r: &whatsapp_rust::wacore::iq::usync::IsOnWhatsAppResult) -> Option<(String, Found)> {
    let pn: &Jid = match &r.pn_jid {
        Some(pn) => pn,
        None if r.jid.is_pn() => &r.jid,
        None => return None,
    };
    let found = if r.is_registered {
        Found {
            jid: Some(pn.to_non_ad().to_string()),
            is_business: r.is_business,
            verified_name: r
                .verified_name
                .as_ref()
                .and_then(|v| v.name.clone())
                .filter(|n| !n.is_empty()),
        }
    } else {
        Found::default()
    };
    Some((pn.user.to_string(), found))
}

pub(crate) async fn lookup_phones(sh: &Shared, client: &Arc<Client>, numbers: &[String]) {
    if numbers.len() > MAX_LOOKUP_NUMBERS {
        reject(sh, LOOKUP_TOO_MANY.to_string());
        return;
    }
    let digits: Vec<Option<String>> = numbers.iter().map(|n| normalize_number(n)).collect();
    let pending = {
        let mut g = guard();
        let now = Instant::now();
        let pending = to_fetch(&digits, &g, now);
        if !pending.is_empty() && !g.admit(pending.len(), now) {
            drop(g);
            log::warn!("phone lookup budget spent ({} numbers refused)", pending.len());
            reject(sh, LOOKUP_BUDGET_SPENT.to_string());
            return;
        }
        pending
    };

    for batch in pending.chunks(LOOKUP_BATCH) {
        let wait = guard().reserve(Instant::now());
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        let jids: Vec<Jid> = batch.iter().map(|d| Jid::pn(d.as_str())).collect();
        let answers = match client.contacts().is_on_whatsapp(&jids).await {
            Ok(a) => a,
            Err(e) => {
                crate::jid::warn_lib_error("phone lookup failed", &e);
                reject(sh, LOOKUP_FAILED.to_string());
                return;
            }
        };
        let now = Instant::now();
        let mut g = guard();
        let mut seen: Vec<&String> = Vec::new();
        for (key, found) in answers.iter().filter_map(answer_key) {
            if let Some(d) = batch.iter().find(|d| **d == key) {
                seen.push(d);
                g.remember(key, found, now);
            }
        }
        // Numbers the server did not mention are not on WhatsApp.
        for d in batch.iter().filter(|d| !seen.contains(d)) {
            g.remember(d.clone(), Found::default(), now);
        }
    }

    let now = Instant::now();
    let g = guard();
    let results = numbers
        .iter()
        .zip(&digits)
        .map(|(number, d)| {
            let found = d.as_deref().and_then(|d| g.cached(d, now)).unwrap_or_default();
            PhoneLookupResult {
                number: number.clone(),
                jid: found.jid,
                is_business: found.is_business,
                verified_name: found.verified_name,
            }
        })
        .collect::<Vec<_>>();
    drop(g);
    log::debug!(
        "phone lookup: {} numbers, {} on WhatsApp",
        results.len(),
        results.iter().filter(|r| r.jid.is_some()).count()
    );
    sh.emit(UiEvent::PhoneLookup { results });
}

// ---- location ----

/// A map pin to share.
pub(crate) struct Place {
    pub(crate) lat: f64,
    pub(crate) lon: f64,
    pub(crate) name: Option<String>,
    pub(crate) address: Option<String>,
}

fn clean_text(s: Option<String>) -> Option<String> {
    let t = s?.trim().chars().take(MAX_PLACE_TEXT_CHARS).collect::<String>();
    (!t.is_empty()).then_some(t)
}

fn validate_place(p: &Place) -> Result<(), &'static str> {
    if (-90.0..=90.0).contains(&p.lat) && (-180.0..=180.0).contains(&p.lon) {
        Ok(())
    } else {
        Err("Invalid location")
    }
}

fn build_location_message(p: &Place, context: Option<wa::ContextInfo>) -> wa::Message {
    wa::Message {
        location_message: MessageField::some(wa::message::LocationMessage {
            degrees_latitude: Some(p.lat),
            degrees_longitude: Some(p.lon),
            name: p.name.clone(),
            address: p.address.clone(),
            context_info: context.map_or_else(MessageField::none, MessageField::some),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Text of the stored row: "📍 name" (or "📍 Location") and the address below.
fn location_text(p: &Place) -> String {
    let mut text = format!("📍 {}", p.name.as_deref().unwrap_or("Location"));
    if let Some(address) = &p.address {
        text.push('\n');
        text.push_str(address);
    }
    text
}

pub(crate) async fn send_location(
    sh: &Shared,
    client: &Arc<Client>,
    chat_jid: &str,
    mut place: Place,
    quoted_id: Option<&str>,
) {
    if let Err(message) = validate_place(&place) {
        reject(sh, message.to_string());
        return;
    }
    place.name = clean_text(place.name.take());
    place.address = clean_text(place.address.take());
    let Some(chat) = parse_jid(sh, chat_jid) else { return };
    let Some(sender_jid) = own_jid(client) else {
        reject(sh, NOT_LOGGED_IN.to_string());
        return;
    };
    log::debug!("sending location to {} at {:.2},{:.2}", redact(chat_jid), place.lat, place.lon);
    let quote = resolve_quote(sh, &chat, chat_jid, quoted_id);
    let message = build_location_message(&place, quote.context);
    let id = client.generate_message_id();
    let Some(mut pending) = convert::outgoing(&message, &id, chat_jid, &sender_jid, now_secs()) else {
        log::error!("built location message is not displayable");
        reject(sh, "Could not share the location".to_string());
        return;
    };
    pending.text = Some(location_text(&place));
    pending.geo_lat = Some(place.lat);
    pending.geo_lon = Some(place.lon);
    if !store_pending(sh, &pending) {
        return;
    }
    deliver(sh, client, chat, chat_jid, id, message).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contact(name: &str, phones: &[&str]) -> SharedContact {
        SharedContact { name: name.into(), phones: phones.iter().map(|p| p.to_string()).collect() }
    }

    #[test]
    fn vcard_has_waid_for_plain_numbers_and_crlf() {
        let v = vcard("Ann", &["+989121234567".to_string()]);
        assert_eq!(
            v,
            "BEGIN:VCARD\r\nVERSION:3.0\r\nN:;Ann;;;\r\nFN:Ann\r\n\
             TEL;type=CELL;type=VOICE;waid=989121234567:+989121234567\r\nEND:VCARD\r\n"
        );
    }

    #[test]
    fn vcard_escapes_and_keeps_persian_names() {
        let v = vcard("علی, رضا; \\x\nنو", &["+98 912 123 4567".to_string(), "0912".to_string()]);
        assert!(v.contains("FN:علی\\, رضا\\; \\\\x\\nنو\r\n"), "{v}");
        assert!(v.contains("TEL;type=CELL;type=VOICE:+98 912 123 4567\r\n"), "no waid for spaced numbers");
        assert!(v.contains("TEL;type=CELL;type=VOICE:0912\r\n"), "local numbers get no waid");
        assert_eq!(v.matches("TEL").count(), 2);
    }

    #[test]
    fn waid_only_for_international_numbers() {
        let tel = |p: &str| vcard("A", &[p.to_string()]);
        assert!(tel("+1234567").contains("waid=1234567:+1234567"));
        assert!(tel("9121234567").contains("waid=9121234567:+9121234567"), "10+ digits");
        assert!(!tel("912123456").contains("waid"), "9 digits without +");
        assert!(!tel("09121234567").contains("waid"), "leading 0");
        assert!(!tel("+0912123456").contains("waid"));
        assert!(tel("1;2,3").contains("TEL;type=CELL;type=VOICE:1\\;2\\,3\r\n"), "free text is escaped");
    }

    #[test]
    fn vcard_round_trips_a_persian_name_with_a_comma() {
        let name = "رضایی, علی; ب";
        let m = build_contact_message(&[contact(name, &["+989121234567"])], None).expect("built");
        let row = convert::outgoing(&m, "I", "1@s.whatsapp.net", "2@s.whatsapp.net", 5).expect("row");
        assert_eq!(row.media_name.as_deref(), Some(name));
        assert_eq!(row.text.as_deref(), Some(format!("👤 {name}\n+989121234567").as_str()));
    }

    #[test]
    fn single_contact_is_a_contact_message_the_ui_can_render() {
        let m = build_contact_message(&[contact("Ann", &["+100200300"])], None).expect("built");
        let card = m.contact_message.as_option().expect("single card");
        assert_eq!(card.display_name.as_deref(), Some("Ann"));
        let row = convert::outgoing(&m, "I", "1@s.whatsapp.net", "2@s.whatsapp.net", 5).expect("row");
        assert!(row.from_me);
        assert_eq!(row.media_mime.as_deref(), Some("text/vcard"));
        assert_eq!(row.text.as_deref(), Some("👤 Ann\n+100200300"));
        assert_eq!(row.media_name.as_deref(), Some("Ann"));
    }

    #[test]
    fn several_contacts_become_an_array() {
        let m = build_contact_message(&[contact("Ann", &["+100200300"]), contact("Bob", &["+100200301"])], None)
            .expect("built");
        let array = m.contacts_array_message.as_option().expect("array");
        assert_eq!(array.display_name.as_deref(), Some("2 contacts"));
        assert_eq!(array.contacts.len(), 2);
        let row = convert::outgoing(&m, "I", "1@s.whatsapp.net", "2@s.whatsapp.net", 5).expect("row");
        assert_eq!(row.text.as_deref(), Some("👤 Ann\n+100200300\n\n👤 Bob\n+100200301"));
    }

    #[test]
    fn contact_limits() {
        assert!(build_contact_message(&[], None).is_err());
        let many: Vec<_> = (0..21).map(|i| contact(&format!("c{i}"), &["+1002003"])).collect();
        assert!(build_contact_message(&many, None).is_err());
        assert!(build_contact_message(&many[..20], None).is_ok());
        assert!(build_contact_message(&[contact(&"x".repeat(101), &["+1002003"])], None).is_err());
        assert!(build_contact_message(&[contact(&"ن".repeat(100), &["+1002003"])], None).is_ok());
        assert!(build_contact_message(&[contact("A", &["1", "2", "3", "4", "5", "6"])], None).is_err());
        assert!(build_contact_message(&[contact("A", &["1", "2", "3", "4", "5"])], None).is_ok());
        assert!(build_contact_message(&[contact(" ", &["1"])], None).is_err());
        assert!(build_contact_message(&[contact("A", &[])], None).is_err());
    }

    #[test]
    fn numbers_are_normalised_to_digits() {
        assert_eq!(normalize_number("+98 912-123 4567").as_deref(), Some("989121234567"));
        assert_eq!(normalize_number(" (912) 123.4567 ").as_deref(), Some("9121234567"));
        assert_eq!(normalize_number("09121234567"), None, "local number");
        assert_eq!(normalize_number("+0912 123 4567"), None, "leading 0 after +");
        assert_eq!(normalize_number("+ 0912 123 4567"), None);
        assert_eq!(normalize_number("123456"), None, "too short");
        assert_eq!(normalize_number("1234567").as_deref(), Some("1234567"));
        assert_eq!(normalize_number("1234567890123456"), None, "too long");
        assert_eq!(normalize_number("12345678x"), None);
        assert_eq!(normalize_number(""), None);
    }

    #[test]
    fn lookups_are_cached_for_ten_minutes_and_deduplicated() {
        let t0 = Instant::now();
        let mut g = LookupGuard::default();
        let found = Found { jid: Some("1@s.whatsapp.net".into()), ..Default::default() };
        g.remember("1234567".into(), found.clone(), t0);
        assert_eq!(g.cached("1234567", t0 + Duration::from_secs(599)), Some(found));
        assert_eq!(g.cached("1234567", t0 + LOOKUP_TTL), None);
        assert_eq!(g.cached("7654321", t0), None);

        let numbers = vec![Some("1234567".to_string()), None, Some("7654321".into()), Some("7654321".into())];
        assert_eq!(to_fetch(&numbers, &g, t0), vec!["7654321".to_string()]);
        assert_eq!(to_fetch(&numbers, &g, t0 + LOOKUP_TTL).len(), 2);
    }

    #[test]
    fn server_calls_are_spaced_two_seconds_apart() {
        let t0 = Instant::now();
        let mut g = LookupGuard::default();
        assert_eq!(g.reserve(t0), Duration::ZERO);
        assert_eq!(g.reserve(t0), LOOKUP_SPACING);
        assert_eq!(g.reserve(t0 + Duration::from_secs(1)), Duration::from_secs(3));
        assert_eq!(g.reserve(t0 + Duration::from_secs(60)), Duration::ZERO);
    }

    #[test]
    fn lookup_budget_rolls_over_ten_minutes_and_a_day() {
        let t0 = Instant::now();
        let mut g = LookupGuard::default();
        assert!(g.admit(20, t0));
        assert!(g.admit(20, t0));
        assert!(g.admit(20, t0));
        assert!(!g.admit(1, t0 + Duration::from_secs(599)), "61st in 10 minutes");
        assert!(g.admit(20, t0 + SHORT_WINDOW), "short window has rolled");
        // 80 booked so far; fill the day: 4 more rounds of 60 would exceed 300.
        let mut t = t0 + SHORT_WINDOW;
        let mut total = 80;
        while total + 60 <= LOOKUPS_PER_DAY {
            t += SHORT_WINDOW;
            assert!(g.admit(60, t));
            total += 60;
        }
        t += SHORT_WINDOW;
        assert!(!g.admit(LOOKUPS_PER_DAY - total + 1, t), "daily cap");
        assert!(g.admit(LOOKUPS_PER_DAY - total, t), "exactly the rest");
        assert!(g.admit(60, t + LONG_WINDOW), "everything expired after 24 h");
    }

    #[test]
    fn refused_lookups_book_nothing() {
        let t0 = Instant::now();
        let mut g = LookupGuard::default();
        assert!(!g.admit(61, t0));
        assert!(g.admit(60, t0));
    }

    #[test]
    fn batches_hold_at_most_fifty_numbers() {
        let all: Vec<String> = (0..120).map(|i| format!("{}", 1_000_000 + i)).collect();
        let sizes: Vec<usize> = all.chunks(LOOKUP_BATCH).map(<[String]>::len).collect();
        assert_eq!(sizes, vec![50, 50, 20]);
    }

    fn place(lat: f64, lon: f64) -> Place {
        Place { lat, lon, name: None, address: None }
    }

    #[test]
    fn location_ranges_are_validated() {
        assert!(validate_place(&place(90.0, -180.0)).is_ok());
        assert!(validate_place(&place(90.1, 0.0)).is_err());
        assert!(validate_place(&place(0.0, 180.1)).is_err());
        assert!(validate_place(&place(f64::NAN, 0.0)).is_err());
        assert!(validate_place(&place(0.0, f64::INFINITY)).is_err());
    }

    #[test]
    fn location_row_text_and_message() {
        let mut p = place(35.7, 51.4);
        assert_eq!(location_text(&p), "📍 Location");
        p.name = Some("Azadi".into());
        p.address = Some("Tehran".into());
        assert_eq!(location_text(&p), "📍 Azadi\nTehran");
        let m = build_location_message(&p, None);
        let loc = m.location_message.as_option().expect("location");
        assert_eq!((loc.degrees_latitude, loc.degrees_longitude), (Some(35.7), Some(51.4)));
        let row = convert::outgoing(&m, "I", "1@s.whatsapp.net", "2@s.whatsapp.net", 5).expect("row");
        assert_eq!((row.geo_lat, row.geo_lon), (Some(35.7), Some(51.4)));
        assert_eq!(clean_text(Some("  ".into())), None);
    }
}
