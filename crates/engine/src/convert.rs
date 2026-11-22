//! Pure conversions from WhatsApp protobuf messages to [`wa_store::NewMessage`].
//!
//! Nothing here touches the network or the store, so it is fully unit-testable.
//! JID normalization (LID -> phone number) needs the client's mapping cache and
//! therefore happens in the callers; these functions receive final JID strings.

use whatsapp_rust::buffa::Message as _;
use whatsapp_rust::prelude::{wa, MessageBuilderExt as _, MessageInfo};
use wa_store::{MessageKind, MessageStatus, NewMessage, VIEW_ONCE_PREVIEW, VIEW_ONCE_TEXT};

const PREVIEW_MAX_CHARS: usize = 200;

/// What a message means to the UI, independent of where it came from.
struct Parsed<'a> {
    kind: MessageKind,
    text: Option<String>,
    mime: Option<String>,
    name: Option<String>,
    size: Option<i64>,
    duration: Option<i64>,
    thumbnail: Option<Vec<u8>>,
    geo: Option<(f64, f64)>,
    context: Option<&'a wa::ContextInfo>,
    /// Message to store as `raw` instead of the original, so media nested in a
    /// business message can be downloaded like top-level media.
    lifted: Option<wa::Message>,
}

impl Parsed<'_> {
    fn new(kind: MessageKind) -> Self {
        Parsed {
            kind,
            text: None,
            mime: None,
            name: None,
            size: None,
            duration: None,
            thumbnail: None,
            geo: None,
            context: None,
            lifted: None,
        }
    }
}

fn non_empty(s: &Option<String>) -> Option<String> {
    s.as_ref().filter(|s| !s.is_empty()).cloned()
}

fn size_of(len: Option<u64>) -> Option<i64> {
    len.and_then(|l| i64::try_from(l).ok())
}

/// Classifies the innermost content of a message. Returns `None` for protocol,
/// reaction, key-distribution and other non-displayable messages.
fn classify(base: &wa::Message) -> Option<Parsed<'_>> {
    if let Some(text) = base.conversation.as_ref().filter(|t| !t.is_empty()) {
        let mut p = Parsed::new(MessageKind::Text);
        p.text = Some(text.clone());
        return Some(p);
    }
    if let Some(ext) = base.extended_text_message.as_option() {
        let mut p = Parsed::new(MessageKind::Text);
        p.text = ext.text.clone();
        p.context = ext.context_info.as_option();
        return Some(p);
    }
    if let Some(img) = base.image_message.as_option() {
        let mut p = Parsed::new(MessageKind::Image);
        p.text = non_empty(&img.caption);
        p.mime = img.mimetype.clone();
        p.size = size_of(img.file_length);
        p.thumbnail = img.jpeg_thumbnail.clone();
        p.context = img.context_info.as_option();
        return Some(p);
    }
    if let Some(vid) = base.video_message.as_option() {
        let mut p = Parsed::new(MessageKind::Video);
        p.text = non_empty(&vid.caption);
        p.mime = vid.mimetype.clone();
        p.size = size_of(vid.file_length);
        p.duration = vid.seconds.map(i64::from);
        p.thumbnail = vid.jpeg_thumbnail.clone();
        p.context = vid.context_info.as_option();
        return Some(p);
    }
    if let Some(ptv) = base.ptv_message.as_option() {
        let mut p = Parsed::new(MessageKind::Video);
        p.mime = ptv.mimetype.clone();
        p.size = size_of(ptv.file_length);
        p.duration = ptv.seconds.map(i64::from);
        p.thumbnail = ptv.jpeg_thumbnail.clone();
        p.context = ptv.context_info.as_option();
        return Some(p);
    }
    if let Some(aud) = base.audio_message.as_option() {
        let kind = if aud.ptt == Some(true) {
            MessageKind::Voice
        } else {
            MessageKind::Audio
        };
        let mut p = Parsed::new(kind);
        p.mime = aud.mimetype.clone();
        p.size = size_of(aud.file_length);
        p.duration = aud.seconds.map(i64::from);
        p.context = aud.context_info.as_option();
        return Some(p);
    }
    if let Some(doc) = base.document_message.as_option() {
        let mut p = Parsed::new(MessageKind::Document);
        p.text = non_empty(&doc.caption);
        p.mime = doc.mimetype.clone();
        p.name = non_empty(&doc.file_name).or_else(|| non_empty(&doc.title));
        p.size = size_of(doc.file_length);
        p.thumbnail = doc.jpeg_thumbnail.clone();
        p.context = doc.context_info.as_option();
        return Some(p);
    }
    if let Some(st) = base.sticker_message.as_option() {
        let mut p = Parsed::new(MessageKind::Sticker);
        p.mime = st.mimetype.clone();
        p.size = size_of(st.file_length);
        p.context = st.context_info.as_option();
        return Some(p);
    }
    if let Some(loc) = base.location_message.as_option() {
        let mut p = Parsed::new(MessageKind::Other);
        p.text = Some(match non_empty(&loc.name) {
            Some(name) => format!("📍 {name}"),
            None => "📍 Location".to_string(),
        });
        p.geo = geo_of(loc.degrees_latitude, loc.degrees_longitude);
        p.context = loc.context_info.as_option();
        return Some(p);
    }
    if let Some(live) = base.live_location_message.as_option() {
        let mut p = Parsed::new(MessageKind::Other);
        p.text = Some(match non_empty(&live.caption) {
            Some(caption) => format!("📍 Live location
{caption}"),
            None => "📍 Live location".to_string(),
        });
        p.geo = geo_of(live.degrees_latitude, live.degrees_longitude);
        p.context = live.context_info.as_option();
        return Some(p);
    }
    if let Some(contact) = base.contact_message.as_option() {
        let card = contact_card(contact);
        let mut p = Parsed::new(MessageKind::Other);
        p.text = Some(card.block);
        p.mime = Some(VCARD_MIME.to_string());
        p.name = Some(card.name);
        p.context = contact.context_info.as_option();
        return Some(p);
    }
    if let Some(array) = base.contacts_array_message.as_option() {
        let cards: Vec<ContactCard> = array.contacts.iter().map(contact_card).collect();
        let name = match cards.as_slice() {
            [] => non_empty(&array.display_name).unwrap_or_else(|| "Contact".to_string()),
            [only] => only.name.clone(),
            [first, rest @ ..] => format!("{} and {} others", first.name, rest.len()),
        };
        let mut p = Parsed::new(MessageKind::Other);
        p.text = Some(if cards.is_empty() {
            format!("👤 {name}")
        } else {
            cards.iter().map(|c| c.block.as_str()).collect::<Vec<_>>().join("\n\n")
        });
        p.mime = Some(VCARD_MIME.to_string());
        p.name = Some(name);
        p.context = array.context_info.as_option();
        return Some(p);
    }
    if let Some(poll) = poll_of(base) {
        let mut p = Parsed::new(MessageKind::Other);
        p.text = Some(match non_empty(&poll.name) {
            Some(name) => format!("📊 {name}"),
            None => "📊 Poll".to_string(),
        });
        p.context = poll.context_info.as_option();
        return Some(p);
    }
    if let Some(p) = business::classify(base) {
        return Some(p);
    }
    // Anything that is not intentionally hidden still gets a row, so a chat whose
    // messages we cannot render yet appears and keeps its order (like the
    // official clients' "this message isn't supported").
    if business::is_hidden_only(base) {
        return None;
    }
    let mut p = Parsed::new(MessageKind::Other);
    p.text = Some(UNSUPPORTED_TEXT.to_string());
    Some(p)
}

/// Text of the row shown for a message type WaClient cannot render yet.
pub const UNSUPPORTED_TEXT: &str = "This message type isn't supported in WaClient yet.";

/// Coordinates of a location message; `None` when missing or out of range.
fn geo_of(lat: Option<f64>, lon: Option<f64>) -> Option<(f64, f64)> {
    let (lat, lon) = (lat?, lon?);
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then_some((lat, lon))
}

const VCARD_MIME: &str = "text/vcard";

/// One shared contact, ready for display.
struct ContactCard {
    name: String,
    /// "👤 name" followed by one line per phone number.
    block: String,
}

fn contact_card(c: &wa::message::ContactMessage) -> ContactCard {
    let card = parse_vcard(c.vcard.as_deref().unwrap_or(""));
    let name = card
        .full_name
        .or_else(|| non_empty(&c.display_name))
        .or(card.structured_name)
        .or_else(|| card.phones.first().cloned())
        .unwrap_or_else(|| "Contact".to_string());
    let mut block = format!("👤 {name}");
    for phone in &card.phones {
        block.push('\n');
        block.push_str(phone);
    }
    ContactCard { name, block }
}

#[derive(Debug, Default, PartialEq)]
struct VCard {
    full_name: Option<String>,
    structured_name: Option<String>,
    /// Normalized to "+<digits>", without duplicates.
    phones: Vec<String>,
}

/// Decodes a quoted-printable value (`=C3=A9`); malformed escapes stay as is.
fn decode_quoted_printable(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = if b[i] == b'=' {
            s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())
        } else {
            None
        };
        match hex {
            Some(v) => {
                out.push(v);
                i += 3;
            }
            None => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Joins folded lines (leading space/tab) and quoted-printable soft breaks.
fn unfold_vcard(v: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut soft_break = false;
    for raw in v.split('\n') {
        let raw = raw.trim_end_matches('\r');
        if soft_break && let Some(last) = lines.last_mut() {
            last.pop(); // the trailing '='
            last.push_str(raw);
        } else if let Some(rest) = raw.strip_prefix([' ', '\t'])
            && let Some(last) = lines.last_mut()
        {
            last.push_str(rest);
        } else {
            lines.push(raw.to_string());
        }
        soft_break = lines.last().is_some_and(|l| {
            l.ends_with('=')
                && l.split(':').next().is_some_and(|h| h.to_ascii_uppercase().contains("QUOTED-PRINTABLE"))
        });
    }
    lines
}

/// Splits a vCard text value at the unescaped `sep` and unescapes each part
/// (`\,` `\;` `\\` `\n` `\N`; any other escaped character stands for itself).
fn split_vcard_value(value: &str, sep: char) -> Vec<String> {
    let mut parts = vec![String::new()];
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        let cur = parts.last_mut().expect("never empty");
        match c {
            '\\' => match chars.next() {
                Some('n' | 'N') => cur.push('\n'),
                Some(other) => cur.push(other),
                None => cur.push('\\'),
            },
            c if c == sep => parts.push(String::new()),
            c => cur.push(c),
        }
    }
    parts
}

fn unescape_vcard(value: &str) -> String {
    // No separator here: every `;` is escaped or literal text.
    split_vcard_value(value, '\0').remove(0)
}

fn parse_vcard(v: &str) -> VCard {
    let mut card = VCard::default();
    for line in unfold_vcard(v) {
        let Some((head, value)) = line.split_once(':') else { continue };
        let mut parts = head.split(';');
        let prop = parts.next().unwrap_or("");
        let prop = prop.rsplit('.').next().unwrap_or(prop).to_ascii_uppercase();
        let params: Vec<&str> = parts.collect();
        let quoted = params.iter().any(|p| p.to_ascii_uppercase().contains("QUOTED-PRINTABLE"));
        let value = if quoted { decode_quoted_printable(value) } else { value.to_string() };
        match prop.as_str() {
            "FN" => {
                let name = unescape_vcard(&value);
                let name = name.trim();
                if card.full_name.is_none() && !name.is_empty() {
                    card.full_name = Some(name.to_string());
                }
            }
            "N" => {
                let fields = split_vcard_value(&value, ';');
                let mut f = fields.iter().map(|s| s.trim());
                let (family, given) = (f.next().unwrap_or(""), f.next().unwrap_or(""));
                let name = format!("{given} {family}").trim().to_string();
                if card.structured_name.is_none() && !name.is_empty() {
                    card.structured_name = Some(name);
                }
            }
            "TEL" => {
                let waid = params.iter().find_map(|p| {
                    let (k, v) = p.split_once('=')?;
                    k.eq_ignore_ascii_case("waid").then_some(v)
                });
                let digits = |s: &str| s.chars().filter(char::is_ascii_digit).collect::<String>();
                let mut d = waid.map(digits).unwrap_or_default();
                if d.is_empty() {
                    d = digits(&unescape_vcard(&value));
                }
                if !d.is_empty() {
                    let phone = format!("+{d}");
                    if !card.phones.contains(&phone) {
                        card.phones.push(phone);
                    }
                }
            }
            _ => {}
        }
    }
    card
}

/// "👤 name" for shared contacts, whose text carries the numbers as well.
fn vcard_preview(mime: Option<&str>, name: Option<&str>) -> Option<String> {
    (mime == Some(VCARD_MIME)).then(|| format!("👤 {}", name.unwrap_or("Contact")))
}

/// The poll definition of a message, whichever protocol version carried it.
fn poll_of(base: &wa::Message) -> Option<&wa::message::PollCreationMessage> {
    [
        &base.poll_creation_message,
        &base.poll_creation_message_v2,
        &base.poll_creation_message_v3,
        &base.poll_creation_message_v5,
        &base.poll_creation_message_v6,
    ]
    .into_iter()
    .find_map(|f| f.as_option())
}

fn truncate(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => format!("{}…", &s[..idx]),
        None => s.to_string(),
    }
}

fn kind_label(kind: MessageKind, name: Option<&str>) -> String {
    match kind {
        MessageKind::Image => "📷 Photo".into(),
        MessageKind::Video => "🎥 Video".into(),
        MessageKind::Voice => "🎤 Voice message".into(),
        MessageKind::Audio => "🎵 Audio".into(),
        MessageKind::Document => format!("📄 {}", name.unwrap_or("Document")),
        MessageKind::Sticker => "Sticker".into(),
        MessageKind::Text | MessageKind::Other => String::new(),
    }
}

/// Short single-line description of a stored message (notifications, quotes).
pub fn preview(msg: &NewMessage) -> String {
    if msg.text.as_deref() == Some(VIEW_ONCE_TEXT) {
        return VIEW_ONCE_PREVIEW.to_string();
    }
    if let Some(p) = vcard_preview(msg.media_mime.as_deref(), msg.media_name.as_deref()) {
        return p;
    }
    match msg.text.as_deref().filter(|t| !t.is_empty()) {
        Some(t) => truncate(t, PREVIEW_MAX_CHARS),
        None => kind_label(msg.kind, msg.media_name.as_deref()),
    }
}

fn preview_of_parsed(p: &Parsed<'_>) -> String {
    if let Some(v) = vcard_preview(p.mime.as_deref(), p.name.as_deref()) {
        return v;
    }
    match p.text.as_deref().filter(|t| !t.is_empty()) {
        Some(t) => truncate(t, PREVIEW_MAX_CHARS),
        None => kind_label(p.kind, p.name.as_deref()),
    }
}

/// Deepest message inside any nesting of the known wrappers. wacore's
/// `get_base_message` peels each wrapper once in a fixed order, so repeated or
/// out-of-order nesting would leave a wrapper and the message would be dropped.
fn base_message(msg: &wa::Message) -> &wa::Message {
    const MAX_DEPTH: usize = 10;
    let mut current = msg;
    for _ in 0..MAX_DEPTH {
        let inner = [
            current.device_sent_message.as_option().map(|w| &w.message),
            current.ephemeral_message.as_option().map(|w| &w.message),
            current.view_once_message.as_option().map(|w| &w.message),
            current.view_once_message_v2.as_option().map(|w| &w.message),
            current.view_once_message_v2_extension.as_option().map(|w| &w.message),
            current.document_with_caption_message.as_option().map(|w| &w.message),
            current.edited_message.as_option().map(|w| &w.message),
            current.group_mentioned_message.as_option().map(|w| &w.message),
            current.bot_invoke_message.as_option().map(|w| &w.message),
            current.bot_forwarded_message.as_option().map(|w| &w.message),
            current.spoiler_message.as_option().map(|w| &w.message),
            current.question_message.as_option().map(|w| &w.message),
            current.question_reply_message.as_option().map(|w| &w.message),
            current.limit_sharing_message.as_option().map(|w| &w.message),
            current.associated_child_message.as_option().map(|w| &w.message),
        ]
        .into_iter()
        .flatten()
        .find_map(|m| m.as_option());
        match inner {
            Some(next) => current = next,
            None => break,
        }
    }
    current
}

/// Whether a message is view-once: wrapped in one of the view-once wrappers
/// (at any depth) or carrying the flag on its media.
pub fn is_view_once(msg: &wa::Message) -> bool {
    const MAX_DEPTH: usize = 10;
    let mut current = msg;
    for _ in 0..MAX_DEPTH {
        if current.view_once_message.is_set()
            || current.view_once_message_v2.is_set()
            || current.view_once_message_v2_extension.is_set()
        {
            return true;
        }
        let flagged = current.image_message.as_option().is_some_and(|m| m.view_once == Some(true))
            || current.video_message.as_option().is_some_and(|m| m.view_once == Some(true))
            || current.audio_message.as_option().is_some_and(|m| m.view_once == Some(true));
        if flagged {
            return true;
        }
        let inner = [
            current.device_sent_message.as_option().map(|w| &w.message),
            current.ephemeral_message.as_option().map(|w| &w.message),
            current.document_with_caption_message.as_option().map(|w| &w.message),
            current.edited_message.as_option().map(|w| &w.message),
        ]
        .into_iter()
        .flatten()
        .find_map(|m| m.as_option());
        match inner {
            Some(next) => current = next,
            None => return false,
        }
    }
    false
}

/// The row stored for a view-once message: only a placeholder, no media, no
/// thumbnail and no raw message (so no key or URL that could fetch the media).
fn view_once_placeholder(id: &str, chat_jid: &str, sender_jid: &str, from_me: bool, ts: i64) -> NewMessage {
    NewMessage {
        chat_jid: chat_jid.to_string(),
        id: id.to_string(),
        sender_jid: sender_jid.to_string(),
        from_me,
        ts,
        kind: MessageKind::Other,
        text: Some(VIEW_ONCE_TEXT.to_string()),
        raw: None,
        ..Default::default()
    }
}

/// Preview text of a message being quoted, or `None` if it has no displayable
/// content.
pub fn quote_preview(quoted: &wa::Message) -> Option<String> {
    if is_view_once(quoted) {
        return Some(VIEW_ONCE_PREVIEW.to_string());
    }
    let parsed = classify(base_message(quoted))?;
    let text = preview_of_parsed(&parsed);
    (!text.is_empty()).then_some(text)
}

/// A protocol message that changes an already stored message.
#[derive(Debug, Clone, PartialEq)]
pub enum ProtocolAction {
    /// The sender deleted the message for everyone.
    Revoke { id: String },
    /// The sender replaced the text (or caption) of the message.
    Edit { id: String, text: String },
}

impl ProtocolAction {
    pub fn target_id(&self) -> &str {
        match self {
            ProtocolAction::Revoke { id } | ProtocolAction::Edit { id, .. } => id,
        }
    }
}

/// Detects `protocolMessage` REVOKE and MESSAGE_EDIT (also inside the
/// `editedMessage` wrapper). Edits without displayable text are ignored.
pub fn protocol_action(msg: &wa::Message) -> Option<ProtocolAction> {
    use wa::message::protocol_message::Type;
    let proto = base_message(msg).protocol_message.as_option()?;
    let id = proto
        .key
        .as_option()?
        .id
        .as_deref()
        .filter(|i| !i.is_empty())?
        .to_string();
    match proto.r#type? {
        Type::REVOKE => Some(ProtocolAction::Revoke { id }),
        Type::MESSAGE_EDIT => {
            let edited = proto.edited_message.as_option()?;
            let text = classify(base_message(edited))?
                .text
                .filter(|t| !t.is_empty())?;
            Some(ProtocolAction::Edit { id, text })
        }
        _ => None,
    }
}

/// A poll definition, as shown (read-only) in the chat.
#[derive(Debug, Clone, PartialEq)]
pub struct PollInfo {
    pub question: String,
    pub options: Vec<String>,
    /// How many options one voter may pick (never 0).
    pub selectable_count: u32,
    /// The correct option of a quiz poll (one of `options`); `None` for ordinary polls.
    pub correct: Option<String>,
}

/// The poll defined by `msg`, if it is a poll creation.
pub fn poll_info(msg: &wa::Message) -> Option<PollInfo> {
    let poll = poll_of(base_message(msg))?;
    let options: Vec<String> = poll
        .options
        .iter()
        .filter_map(|o| non_empty(&o.option_name))
        .collect();
    let selectable_count = match poll.selectable_options_count {
        Some(n) if n > 0 => n,
        // 0 means "any number" on the wire.
        _ => u32::try_from(options.len().max(1)).unwrap_or(u32::MAX),
    };
    let correct = quiz_answer(poll, &options);
    Some(PollInfo {
        question: non_empty(&poll.name).unwrap_or_else(|| "Poll".to_string()),
        options,
        selectable_count,
        correct,
    })
}

/// The correct option of a quiz poll: `correctAnswer` named by its text or,
/// failing that, by the lowercase-hex SHA-256 of the name WA Web sends.
fn quiz_answer(poll: &wa::message::PollCreationMessage, options: &[String]) -> Option<String> {
    if !matches!(poll.poll_type, Some(wa::message::PollType::QUIZ)) {
        return None;
    }
    let answer = poll.correct_answer.as_option()?;
    if let Some(name) = non_empty(&answer.option_name)
        && options.contains(&name)
    {
        return Some(name);
    }
    let hash = answer.option_hash.as_deref().filter(|h| !h.is_empty())?.to_ascii_lowercase();
    options.iter().find(|name| quiz_option_hash(name) == hash).cloned()
}

/// Lowercase hex of SHA-256(name): how `correctAnswer.optionHash` is written.
pub fn quiz_option_hash(name: &str) -> String {
    whatsapp_rust::wacore::poll::compute_option_hash(name).iter().map(|b| format!("{b:02x}")).collect()
}

/// [`poll_info`] for a message we built from `msg` (see [`from_inbound`]);
/// cheap to call on every message because only poll rows are decoded.
pub fn poll_info_of_stored(msg: &NewMessage) -> Option<PollInfo> {
    if msg.kind != MessageKind::Other || !msg.text.as_deref().is_some_and(|t| t.starts_with('📊')) {
        return None;
    }
    let decoded = wa::Message::decode_from_slice(msg.raw.as_deref()?).ok()?;
    poll_info(&decoded)
}

/// The 32-byte secret that keys the votes of a poll message.
pub fn message_secret(msg: &wa::Message) -> Option<Vec<u8>> {
    [msg, base_message(msg)]
        .into_iter()
        .find_map(|m| m.message_context_info.as_option()?.message_secret.clone())
        .filter(|s| !s.is_empty())
}

/// Names of the options whose hash is in `hashes` (in poll order). Votes
/// reference options by SHA-256 of the name; unknown hashes are ignored.
pub fn option_names_for_hashes(options: &[String], hashes: &[Vec<u8>]) -> Vec<String> {
    options
        .iter()
        .filter(|name| {
            let hash = whatsapp_rust::wacore::poll::compute_option_hash(name);
            hashes.iter().any(|h| h.as_slice() == hash.as_slice())
        })
        .cloned()
        .collect()
}

/// An emoji reaction to another message; an empty emoji withdraws it.
#[derive(Debug, Clone, PartialEq)]
pub struct ReactionInfo {
    pub target_id: String,
    pub emoji: String,
    pub ts_ms: Option<i64>,
}

pub fn reaction(msg: &wa::Message) -> Option<ReactionInfo> {
    let r = base_message(msg).reaction_message.as_option()?;
    let target_id = non_empty(&r.key.as_option()?.id)?;
    Some(ReactionInfo {
        target_id,
        emoji: r.text.clone().unwrap_or_default(),
        ts_ms: r.sender_timestamp_ms,
    })
}

/// An encrypted poll vote (decrypted later, once the poll is known).
#[derive(Debug, Clone, PartialEq)]
pub struct PollVoteInfo {
    pub poll_id: String,
    pub enc_payload: Vec<u8>,
    pub enc_iv: Vec<u8>,
    pub ts_ms: Option<i64>,
}

pub fn poll_vote(msg: &wa::Message) -> Option<PollVoteInfo> {
    let u = base_message(msg).poll_update_message.as_option()?;
    let vote = u.vote.as_option()?;
    Some(PollVoteInfo {
        poll_id: non_empty(&u.poll_creation_message_key.as_option()?.id)?,
        enc_payload: vote.enc_payload.clone().filter(|p| !p.is_empty())?,
        enc_iv: vote.enc_iv.clone().filter(|p| !p.is_empty())?,
        ts_ms: u.sender_timestamp_ms,
    })
}

/// Encodes `msg` for storage, replacing any quoted view-once message with a
/// text stub: a quote carries the original's media key, URL and thumbnail,
/// which must never be persisted.
fn raw_without_view_once_quotes(msg: &wa::Message) -> Vec<u8> {
    let mut scrubbed = msg.clone();
    scrub_quotes(&mut scrubbed, 0);
    scrubbed.encode_to_vec()
}

fn scrub_quotes(msg: &mut wa::Message, depth: usize) {
    const MAX_DEPTH: usize = 10;
    if depth >= MAX_DEPTH {
        return;
    }
    macro_rules! scrub_ctx {
        ($($field:ident),* $(,)?) => {$(
            if let Some(ctx) = msg.$field.as_option_mut().and_then(|m| m.context_info.as_option_mut()) {
                scrub_context(ctx);
            }
        )*};
    }
    scrub_ctx!(
        extended_text_message,
        image_message,
        video_message,
        ptv_message,
        audio_message,
        document_message,
        sticker_message,
        location_message,
        live_location_message,
        contact_message,
        contacts_array_message,
    );
    macro_rules! scrub_wrapped {
        ($($field:ident),* $(,)?) => {$(
            if let Some(inner) = msg.$field.as_option_mut().and_then(|w| w.message.as_option_mut()) {
                scrub_quotes(inner, depth + 1);
            }
        )*};
    }
    scrub_wrapped!(
        device_sent_message,
        ephemeral_message,
        view_once_message,
        view_once_message_v2,
        view_once_message_v2_extension,
        document_with_caption_message,
        edited_message,
    );
}

fn scrub_context(ctx: &mut wa::ContextInfo) {
    if ctx.quoted_message.as_option().is_some_and(is_view_once) {
        ctx.quoted_message = whatsapp_rust::buffa::MessageField::some(wa::Message {
            conversation: Some(VIEW_ONCE_PREVIEW.to_string()),
            ..Default::default()
        });
    }
}

fn build(
    msg: &wa::Message,
    id: &str,
    chat_jid: &str,
    sender_jid: &str,
    from_me: bool,
    ts: i64,
) -> Option<NewMessage> {
    let base = base_message(msg);
    let parsed = classify(base)?;
    // Only media is secret; text inside a wrapper is an ordinary message.
    let is_media = !matches!(parsed.kind, MessageKind::Text | MessageKind::Other);
    if is_media && is_view_once(msg) {
        return Some(view_once_placeholder(id, chat_jid, sender_jid, from_me, ts));
    }

    let quoted_id = parsed
        .context
        .and_then(|c| non_empty(&c.stanza_id));
    let quoted_preview = parsed
        .context
        .and_then(|c| c.quoted_message.as_option())
        .and_then(quote_preview);
    let link = crate::preview::preview_from_message(msg);

    Some(NewMessage {
        chat_jid: chat_jid.to_string(),
        id: id.to_string(),
        sender_jid: sender_jid.to_string(),
        from_me,
        ts,
        kind: parsed.kind,
        text: parsed.text,
        quoted_id,
        quoted_preview,
        media_mime: parsed.mime,
        media_name: parsed.name,
        media_size: parsed.size,
        media_duration: parsed.duration,
        thumbnail: parsed.thumbnail,
        local_path: None,
        raw: Some(raw_without_view_once_quotes(parsed.lifted.as_ref().unwrap_or(msg))),
        link_url: link.as_ref().map(|l| l.url.clone()),
        link_title: link.as_ref().map(|l| l.title.clone()).filter(|t| !t.is_empty()),
        link_desc: link.as_ref().map(|l| l.description.clone()).filter(|d| !d.is_empty()),
        link_thumb: link.and_then(|l| l.thumbnail_jpeg),
        mentions_me: false,
        geo_lat: parsed.geo.map(|g| g.0),
        geo_lon: parsed.geo.map(|g| g.1),
        expires_at: None,
    })
}

/// The stored row of a message we send ourselves, derived the same way as for
/// an incoming one so both render identically. `None` for non-displayable content.
pub fn outgoing(msg: &wa::Message, id: &str, chat_jid: &str, sender_jid: &str, ts: i64) -> Option<NewMessage> {
    build(msg, id, chat_jid, sender_jid, true, ts)
}

/// Converts a live/offline message. `chat_jid` and `sender_jid` must already be
/// normalized (see `jid.rs`). Returns `None` for messages that are not stored.
pub fn from_inbound(
    msg: &wa::Message,
    info: &MessageInfo,
    chat_jid: &str,
    sender_jid: &str,
) -> Option<NewMessage> {
    build(
        msg,
        &info.id,
        chat_jid,
        sender_jid,
        info.source.is_from_me,
        info.timestamp.timestamp(),
    )
}

/// Sender of a history message: us, the group participant (through `resolve`),
/// or the chat itself for DMs.
fn web_sender(
    key: &wa::MessageKey,
    wmi: &wa::WebMessageInfo,
    chat_jid: &str,
    own_jid: &str,
    resolve: &dyn Fn(&str) -> String,
) -> Option<String> {
    if key.from_me == Some(true) {
        Some(own_jid.to_string())
    } else if chat_jid.ends_with("@g.us") {
        let participant = key
            .participant
            .as_deref()
            .or(wmi.participant.as_deref())
            .filter(|p| !p.is_empty())?;
        Some(resolve(participant))
    } else {
        Some(chat_jid.to_string())
    }
}

/// Converts a history-sync message.
///
/// `own_jid` is stored as the sender of our own messages. For groups the sender
/// is the key/message participant, passed through `resolve` (LID -> phone
/// number); for DMs it is the chat itself.
pub fn from_web_message_info(
    wmi: &wa::WebMessageInfo,
    chat_jid: &str,
    own_jid: &str,
    resolve: &dyn Fn(&str) -> String,
) -> Option<NewMessage> {
    let key = wmi.key.as_option()?;
    let id = key.id.as_deref().filter(|i| !i.is_empty())?;
    let msg = wmi.message.as_option()?;
    let from_me = key.from_me == Some(true);
    let sender = web_sender(key, wmi, chat_jid, own_jid, resolve)?;
    let ts = i64::try_from(wmi.message_timestamp.unwrap_or(0)).unwrap_or(0);
    build(msg, id, chat_jid, &sender, from_me, ts)
}

/// A REVOKE/EDIT carried by a history message: `(sender, from_me, action)`.
pub fn web_protocol_action(
    wmi: &wa::WebMessageInfo,
    chat_jid: &str,
    own_jid: &str,
    resolve: &dyn Fn(&str) -> String,
) -> Option<(String, bool, ProtocolAction)> {
    let key = wmi.key.as_option()?;
    let action = protocol_action(wmi.message.as_option()?)?;
    let sender = web_sender(key, wmi, chat_jid, own_jid, resolve)?;
    Some((sender, key.from_me == Some(true), action))
}

/// Who a history reaction/vote key belongs to: us, the group participant
/// (through `resolve`) or the chat itself for DMs. `None` when a group key has
/// no participant (it cannot be attributed).
fn key_actor(
    key: &wa::MessageKey,
    chat_jid: &str,
    own_jid: &str,
    resolve: &dyn Fn(&str) -> String,
) -> Option<(String, bool)> {
    if key.from_me == Some(true) {
        return Some((own_jid.to_string(), true));
    }
    if chat_jid.ends_with("@g.us") {
        let participant = key.participant.as_deref().filter(|p| !p.is_empty())?;
        return Some((resolve(participant), false));
    }
    Some((chat_jid.to_string(), false))
}

/// A history message that is itself a reaction: `(reactor, from_me, reaction)`.
pub fn web_reaction_message(
    wmi: &wa::WebMessageInfo,
    chat_jid: &str,
    own_jid: &str,
    resolve: &dyn Fn(&str) -> String,
) -> Option<(String, bool, ReactionInfo)> {
    let key = wmi.key.as_option()?;
    let mut info = reaction(wmi.message.as_option()?)?;
    let actor = web_sender(key, wmi, chat_jid, own_jid, resolve)?;
    let from_me = key.from_me == Some(true);
    info.ts_ms = info
        .ts_ms
        .or_else(|| i64::try_from(wmi.message_timestamp?).ok().map(|s| s * 1000));
    Some((actor, from_me, info))
}

/// The aggregated reactions attached to a history message.
pub fn web_reactions(
    wmi: &wa::WebMessageInfo,
    chat_jid: &str,
    own_jid: &str,
    resolve: &dyn Fn(&str) -> String,
) -> Vec<(String, bool, ReactionInfo)> {
    let Some(target_id) = wmi.key.as_option().and_then(|k| non_empty(&k.id)) else {
        return Vec::new();
    };
    wmi.reactions
        .iter()
        .filter_map(|r| {
            let (actor, from_me) = key_actor(r.key.as_option()?, chat_jid, own_jid, resolve)?;
            Some((
                actor,
                from_me,
                ReactionInfo {
                    target_id: target_id.clone(),
                    emoji: r.text.clone().unwrap_or_default(),
                    ts_ms: r.sender_timestamp_ms,
                },
            ))
        })
        .collect()
}

/// Votes the phone already decrypted for a history poll:
/// `(voter, from_me, selected option hashes, ts seconds)`.
pub fn web_poll_votes(
    wmi: &wa::WebMessageInfo,
    chat_jid: &str,
    own_jid: &str,
    resolve: &dyn Fn(&str) -> String,
) -> Vec<(String, bool, Vec<Vec<u8>>, i64)> {
    wmi.poll_updates
        .iter()
        .filter_map(|u| {
            let (voter, from_me) =
                key_actor(u.poll_update_message_key.as_option()?, chat_jid, own_jid, resolve)?;
            let vote = u.vote.as_option()?;
            let ts = u.sender_timestamp_ms.map_or(0, |ms| ms / 1000);
            Some((voter, from_me, vote.selected_options.clone(), ts))
        })
        .collect()
}

/// Delivery status recorded by the phone for one of our own history messages.
pub fn web_status(wmi: &wa::WebMessageInfo) -> Option<MessageStatus> {
    use wa::web_message_info::Status;
    match wmi.status? {
        Status::SERVER_ACK => Some(MessageStatus::Sent),
        Status::DELIVERY_ACK => Some(MessageStatus::Delivered),
        Status::READ => Some(MessageStatus::Read),
        Status::PLAYED => Some(MessageStatus::Played),
        _ => None,
    }
}

/// Builds the plain-text message we send, optionally quoting another one.
pub fn text_message(text: &str, context: Option<wa::ContextInfo>) -> wa::Message {
    match context {
        Some(ctx) => wa::Message::text_with_context(text, ctx),
        None => wa::Message::text(text),
    }
}

/// The reply / mention / forward context of the message's content.
pub fn context_info_of(msg: &wa::Message) -> Option<&wa::ContextInfo> {
    classify(base_message(msg))?.context
}

/// JIDs mentioned with "@" in the message (as the sender wrote them).
pub fn mentioned_jids(msg: &wa::Message) -> Vec<String> {
    context_info_of(msg).map(|c| c.mentioned_jid.clone()).unwrap_or_default()
}

/// Business / structured messages (templates, interactive, buttons, lists,
/// products, orders and their replies), rendered as plain text plus an optional
/// media header, the way WhatsApp Web shows them. Buttons are not clickable;
/// they are listed as `[label]` lines with their URL, phone number or code.
mod business {
    use super::{non_empty, size_of, Parsed, UNSUPPORTED_TEXT};
    use wa::message as m;
    use whatsapp_rust::buffa::Message as _;
    use whatsapp_rust::buffa::MessageField;
    use whatsapp_rust::prelude::wa;
    use wa_store::MessageKind;

    /// Media header of a business message.
    enum Header<'a> {
        Image(&'a m::ImageMessage),
        Video(&'a m::VideoMessage),
        Document(&'a m::DocumentMessage),
    }

    #[derive(Default)]
    struct Card<'a> {
        blocks: Vec<String>,
        buttons: Vec<String>,
        media: Option<Header<'a>>,
        context: Option<&'a wa::ContextInfo>,
    }

    impl<'a> Card<'a> {
        fn block(&mut self, text: Option<&str>) {
            if let Some(t) = text.map(str::trim).filter(|t| !t.is_empty()) {
                self.blocks.push(t.to_string());
            }
        }

        fn button(&mut self, label: Option<&str>, detail: Option<&str>) {
            let Some(label) = label.map(str::trim).filter(|l| !l.is_empty()) else {
                return;
            };
            match detail.map(str::trim).filter(|d| !d.is_empty()) {
                Some(d) => self.buttons.push(format!("[{label}] {d}")),
                None => self.buttons.push(format!("[{label}]")),
            }
        }

        fn media(&mut self, media: Header<'a>) {
            if self.media.is_none() {
                self.media = Some(media);
            }
        }

        fn text(&self) -> String {
            let mut parts = self.blocks.clone();
            if !self.buttons.is_empty() {
                parts.push(self.buttons.join("\n"));
            }
            parts.join("\n\n")
        }

        fn into_parsed(self) -> Parsed<'a> {
            let text = self.text();
            let text = (!text.is_empty()).then_some(text);
            let mut p = match &self.media {
                Some(Header::Image(img)) => {
                    let mut p = Parsed::new(MessageKind::Image);
                    p.mime = img.mimetype.clone();
                    p.size = size_of(img.file_length);
                    p.thumbnail = img.jpeg_thumbnail.clone();
                    p.lifted = Some(wa::Message {
                        image_message: MessageField::some((*img).clone()),
                        ..Default::default()
                    });
                    p
                }
                Some(Header::Video(vid)) => {
                    let mut p = Parsed::new(MessageKind::Video);
                    p.mime = vid.mimetype.clone();
                    p.size = size_of(vid.file_length);
                    p.duration = vid.seconds.map(i64::from);
                    p.thumbnail = vid.jpeg_thumbnail.clone();
                    p.lifted = Some(wa::Message {
                        video_message: MessageField::some((*vid).clone()),
                        ..Default::default()
                    });
                    p
                }
                Some(Header::Document(doc)) => {
                    let mut p = Parsed::new(MessageKind::Document);
                    p.mime = doc.mimetype.clone();
                    p.name = non_empty(&doc.file_name).or_else(|| non_empty(&doc.title));
                    p.size = size_of(doc.file_length);
                    p.thumbnail = doc.jpeg_thumbnail.clone();
                    p.lifted = Some(wa::Message {
                        document_message: MessageField::some((*doc).clone()),
                        ..Default::default()
                    });
                    p
                }
                None => Parsed::new(MessageKind::Other),
            };
            p.text = match (text, p.kind) {
                (Some(t), _) => Some(t),
                (None, MessageKind::Other) => Some(UNSUPPORTED_TEXT.to_string()),
                (None, _) => None,
            };
            p.context = self.context;
            p
        }
    }

    pub(super) fn classify(base: &wa::Message) -> Option<Parsed<'_>> {
        let mut card = Card::default();
        if let Some(t) = base.template_message.as_option() {
            template(&mut card, t);
        } else if let Some(i) = base.interactive_message.as_option() {
            interactive(&mut card, i, 0);
        } else if let Some(b) = base.buttons_message.as_option() {
            buttons(&mut card, b);
        } else if let Some(l) = base.list_message.as_option() {
            list(&mut card, l);
        } else if let Some(h) = base.highly_structured_message.as_option() {
            card.block(hsm(h).as_deref());
        } else if let Some(pr) = base.product_message.as_option() {
            product(&mut card, pr);
        } else if let Some(o) = base.order_message.as_option() {
            order(&mut card, o);
        } else if let Some(r) = base.buttons_response_message.as_option() {
            let shown = match &r.response {
                Some(m::buttons_response_message::Response::SelectedDisplayText(t)) => Some(t.as_str()),
                None => None,
            };
            card.block(shown.or(r.selected_button_id.as_deref()).or(Some("Selected an option")));
            card.context = r.context_info.as_option();
        } else if let Some(r) = base.list_response_message.as_option() {
            card.block(r.title.as_deref().or(Some("Selected an option")));
            card.block(r.description.as_deref());
            card.context = r.context_info.as_option();
        } else if let Some(r) = base.template_button_reply_message.as_option() {
            card.block(
                r.selected_display_text
                    .as_deref()
                    .or(r.selected_id.as_deref())
                    .or(Some("Selected an option")),
            );
            card.context = r.context_info.as_option();
        } else if let Some(r) = base.interactive_response_message.as_option() {
            let body = r.body.as_option().and_then(|b| b.text.as_deref());
            card.block(body.or(Some("Selected an option")));
            card.context = r.context_info.as_option();
        } else {
            return None;
        }
        Some(card.into_parsed())
    }

    /// Messages that carry no content for the user and must not get a row.
    /// Everything else not recognised becomes an "unsupported" row.
    pub(super) fn is_hidden_only(base: &wa::Message) -> bool {
        let mut rest = base.clone();
        rest.conversation = None;
        rest.sender_key_distribution_message = MessageField::none();
        rest.fast_ratchet_key_sender_key_distribution_message = MessageField::none();
        rest.protocol_message = MessageField::none();
        rest.reaction_message = MessageField::none();
        rest.enc_reaction_message = MessageField::none();
        rest.poll_update_message = MessageField::none();
        rest.keep_in_chat_message = MessageField::none();
        rest.pin_in_chat_message = MessageField::none();
        rest.message_context_info = MessageField::none();
        rest.placeholder_message = MessageField::none();
        rest.secret_encrypted_message = MessageField::none();
        rest.enc_event_response_message = MessageField::none();
        rest.enc_comment_message = MessageField::none();
        rest.root_secret_distribute_message = MessageField::none();
        rest.encode_to_vec().is_empty()
    }

    fn hsm(h: &m::HighlyStructuredMessage) -> Option<String> {
        non_empty(&h.fallback_lg)
            .or_else(|| non_empty(&h.fallback_lc))
            .or_else(|| non_empty(&h.deterministic_lg))
            .or_else(|| {
                h.hydrated_hsm.as_option().and_then(|t| {
                    let mut c = Card::default();
                    template(&mut c, t);
                    let text = c.text();
                    (!text.is_empty()).then_some(text)
                })
            })
    }

    fn template<'a>(card: &mut Card<'a>, t: &'a m::TemplateMessage) {
        use m::template_message::{four_row_template::Title as FourTitle, hydrated_four_row_template::Title, Format};
        card.context = t.context_info.as_option();
        let hydrated = |card: &mut Card<'a>, h: &'a m::template_message::HydratedFourRowTemplate| {
            match &h.title {
                Some(Title::HydratedTitleText(s)) => card.block(Some(s)),
                Some(Title::ImageMessage(i)) => card.media(Header::Image(i)),
                Some(Title::VideoMessage(v)) => card.media(Header::Video(v)),
                Some(Title::DocumentMessage(d)) => card.media(Header::Document(d)),
                _ => {}
            }
            card.block(h.hydrated_content_text.as_deref());
            card.block(h.hydrated_footer_text.as_deref());
            hydrated_buttons(card, &h.hydrated_buttons);
        };
        match &t.format {
            Some(Format::HydratedFourRowTemplate(h)) => hydrated(card, h),
            Some(Format::FourRowTemplate(f)) => {
                match &f.title {
                    Some(FourTitle::HighlyStructuredMessage(h)) => card.block(hsm(h).as_deref()),
                    Some(FourTitle::ImageMessage(i)) => card.media(Header::Image(i)),
                    Some(FourTitle::VideoMessage(v)) => card.media(Header::Video(v)),
                    Some(FourTitle::DocumentMessage(d)) => card.media(Header::Document(d)),
                    _ => {}
                }
                card.block(f.content.as_option().and_then(hsm).as_deref());
                card.block(f.footer.as_option().and_then(hsm).as_deref());
                for b in &f.buttons {
                    use wa::template_button::Button;
                    match &b.button {
                        Some(Button::QuickReplyButton(q)) => {
                            card.button(q.display_text.as_option().and_then(hsm).as_deref(), None)
                        }
                        Some(Button::UrlButton(u)) => card.button(
                            u.display_text.as_option().and_then(hsm).as_deref(),
                            u.url.as_option().and_then(hsm).as_deref(),
                        ),
                        Some(Button::CallButton(c)) => card.button(
                            c.display_text.as_option().and_then(hsm).as_deref(),
                            c.phone_number.as_option().and_then(hsm).as_deref(),
                        ),
                        None => {}
                    }
                }
            }
            Some(Format::InteractiveMessageTemplate(i)) => interactive(card, i, 0),
            None => {}
        }
        // Older senders put the hydrated template in its own field.
        if t.format.is_none()
            && let Some(h) = t.hydrated_template.as_option()
        {
            hydrated(card, h);
        }
    }

    fn hydrated_buttons(card: &mut Card<'_>, buttons: &[wa::HydratedTemplateButton]) {
        use wa::hydrated_template_button::HydratedButton;
        for b in buttons {
            match &b.hydrated_button {
                Some(HydratedButton::QuickReplyButton(q)) => card.button(q.display_text.as_deref(), None),
                Some(HydratedButton::UrlButton(u)) => card.button(u.display_text.as_deref(), u.url.as_deref()),
                Some(HydratedButton::CallButton(c)) => {
                    card.button(c.display_text.as_deref(), c.phone_number.as_deref())
                }
                None => {}
            }
        }
    }

    fn interactive<'a>(card: &mut Card<'a>, i: &'a m::InteractiveMessage, depth: usize) {
        use m::interactive_message::{header::Media, InteractiveMessage as Body};
        if card.context.is_none() {
            card.context = i.context_info.as_option();
        }
        if let Some(h) = i.header.as_option() {
            card.block(h.title.as_deref());
            card.block(h.subtitle.as_deref());
            match &h.media {
                Some(Media::ImageMessage(x)) => card.media(Header::Image(x)),
                Some(Media::VideoMessage(x)) => card.media(Header::Video(x)),
                Some(Media::DocumentMessage(x)) => card.media(Header::Document(x)),
                Some(Media::ProductMessage(p)) => product(card, p),
                _ => {}
            }
        }
        card.block(i.body.as_option().and_then(|b| b.text.as_deref()));
        card.block(i.footer.as_option().and_then(|f| f.text.as_deref()));
        match &i.interactive_message {
            Some(Body::NativeFlowMessage(n)) => {
                for b in &n.buttons {
                    native_flow_button(card, b.name.as_deref(), b.button_params_json.as_deref());
                }
            }
            Some(Body::CarouselMessage(c)) if depth < 2 => {
                for slide in &c.cards {
                    let mut inner = Card::default();
                    interactive(&mut inner, slide, depth + 1);
                    if let Some(media) = inner.media.take() {
                        card.media(media);
                    }
                    let text = inner.text();
                    card.block(Some(&text));
                }
            }
            _ => {}
        }
    }

    /// One native-flow button: `buttonParamsJson` holds the label and target.
    fn native_flow_button(card: &mut Card<'_>, name: Option<&str>, params: Option<&str>) {
        let Some(v) = params.and_then(|p| serde_json::from_str::<serde_json::Value>(p).ok()) else {
            return;
        };
        let get = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
        let label = get("display_text").or_else(|| get("title"));
        let detail = match name {
            Some("cta_url") => get("url").or_else(|| get("merchant_url")),
            Some("cta_call") => get("phone_number"),
            Some("cta_copy") => get("copy_code"),
            _ => None,
        };
        card.button(label.as_deref(), detail.as_deref());
    }

    fn buttons<'a>(card: &mut Card<'a>, b: &'a m::ButtonsMessage) {
        use m::buttons_message::Header as H;
        card.context = b.context_info.as_option();
        match &b.header {
            Some(H::Text(t)) => card.block(Some(t)),
            Some(H::ImageMessage(x)) => card.media(Header::Image(x)),
            Some(H::VideoMessage(x)) => card.media(Header::Video(x)),
            Some(H::DocumentMessage(x)) => card.media(Header::Document(x)),
            _ => {}
        }
        card.block(b.content_text.as_deref());
        card.block(b.footer_text.as_deref());
        for btn in &b.buttons {
            let label = btn.button_text.as_option().and_then(|t| t.display_text.as_deref());
            match (label, btn.native_flow_info.as_option()) {
                (Some(_), _) => card.button(label, None),
                (None, Some(nf)) => native_flow_button(card, nf.name.as_deref(), nf.params_json.as_deref()),
                (None, None) => {}
            }
        }
    }

    fn list<'a>(card: &mut Card<'a>, l: &'a m::ListMessage) {
        card.context = l.context_info.as_option();
        card.block(l.title.as_deref());
        card.block(l.description.as_deref());
        let mut rows = Vec::new();
        for s in &l.sections {
            if let Some(t) = non_empty(&s.title) {
                rows.push(t);
            }
            for r in &s.rows {
                match (non_empty(&r.title), non_empty(&r.description)) {
                    (Some(t), Some(d)) => rows.push(format!("• {t} - {d}")),
                    (Some(t), None) => rows.push(format!("• {t}")),
                    (None, Some(d)) => rows.push(format!("• {d}")),
                    (None, None) => {}
                }
            }
        }
        card.block(Some(&rows.join("\n")));
        card.block(l.footer_text.as_deref());
        card.button(l.button_text.as_deref(), None);
    }

    fn product(card: &mut Card<'_>, p: &m::ProductMessage) {
        if let Some(s) = p.product.as_option() {
            card.block(s.title.as_deref());
            card.block(s.description.as_deref());
            if let Some(amount) = s.price_amount1000 {
                let code = s.currency_code.as_deref().unwrap_or_default();
                card.block(Some(&format!("{code} {:.2}", amount as f64 / 1000.0)));
            }
            card.block(s.url.as_deref());
        }
        card.block(p.body.as_deref());
        card.block(p.footer.as_deref());
    }

    fn order<'a>(card: &mut Card<'a>, o: &'a m::OrderMessage) {
        card.context = o.context_info.as_option();
        card.block(Some(&format!("🛒 {}", o.order_title.as_deref().unwrap_or("Order"))));
        if let Some(n) = o.item_count {
            card.block(Some(&format!("{n} item(s)")));
        }
        card.block(o.message.as_deref());
        if let Some(total) = o.total_amount1000 {
            let code = o.total_currency_code.as_deref().unwrap_or_default();
            card.block(Some(&format!("Total: {code} {:.2}", total as f64 / 1000.0)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use whatsapp_rust::prelude::MessageExt as _;
    use whatsapp_rust::buffa::MessageField;

    fn info(id: &str, from_me: bool) -> MessageInfo {
        let mut info = MessageInfo::default();
        info.id = id.to_string();
        info.source.is_from_me = from_me;
        info.timestamp = whatsapp_rust::chrono::DateTime::from_timestamp(1_700_000_000, 0)
            .unwrap_or_default();
        info
    }

    fn convert(msg: &wa::Message) -> Option<NewMessage> {
        from_inbound(msg, &info("ID1", false), "123@s.whatsapp.net", "123@s.whatsapp.net")
    }

    #[test]
    fn plain_text() {
        let msg = wa::Message::text("hello");
        let nm = convert(&msg).expect("converted");
        assert_eq!(nm.kind, MessageKind::Text);
        assert_eq!(nm.text.as_deref(), Some("hello"));
        assert_eq!(nm.id, "ID1");
        assert_eq!(nm.ts, 1_700_000_000);
        assert!(!nm.from_me);
        assert_eq!(nm.chat_jid, "123@s.whatsapp.net");
        assert!(nm.quoted_id.is_none());
        let raw = nm.raw.expect("raw stored");
        let decoded = wa::Message::decode_from_slice(&raw).expect("raw decodes");
        assert_eq!(decoded.conversation.as_deref(), Some("hello"));
    }

    #[test]
    fn extended_text_with_quote() {
        let quoted = wa::Message {
            image_message: MessageField::some(wa::message::ImageMessage::default()),
            ..Default::default()
        };
        let ctx = wa::ContextInfo {
            stanza_id: Some("QUOTED1".into()),
            participant: Some("456@s.whatsapp.net".into()),
            quoted_message: MessageField::some(quoted),
            ..Default::default()
        };
        let msg = wa::Message::text_with_context("reply", ctx);
        let nm = convert(&msg).expect("converted");
        assert_eq!(nm.kind, MessageKind::Text);
        assert_eq!(nm.text.as_deref(), Some("reply"));
        assert_eq!(nm.quoted_id.as_deref(), Some("QUOTED1"));
        assert_eq!(nm.quoted_preview.as_deref(), Some("📷 Photo"));

        let text_quote = wa::ContextInfo {
            stanza_id: Some("Q2".into()),
            quoted_message: MessageField::some(wa::Message::text("original")),
            ..Default::default()
        };
        let nm = convert(&wa::Message::text_with_context("r2", text_quote)).expect("converted");
        assert_eq!(nm.quoted_preview.as_deref(), Some("original"));
    }

    #[test]
    fn image_with_caption_and_thumbnail() {
        let msg = wa::Message {
            image_message: MessageField::some(wa::message::ImageMessage {
                caption: Some("look".into()),
                mimetype: Some("image/jpeg".into()),
                file_length: Some(2048),
                jpeg_thumbnail: Some(vec![1, 2, 3]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let nm = convert(&msg).expect("converted");
        assert_eq!(nm.kind, MessageKind::Image);
        assert_eq!(nm.text.as_deref(), Some("look"));
        assert_eq!(nm.media_mime.as_deref(), Some("image/jpeg"));
        assert_eq!(nm.media_size, Some(2048));
        assert_eq!(nm.thumbnail, Some(vec![1, 2, 3]));
    }

    #[test]
    fn ptt_audio_is_voice_with_duration() {
        let msg = wa::Message {
            audio_message: MessageField::some(wa::message::AudioMessage {
                ptt: Some(true),
                seconds: Some(12),
                mimetype: Some("audio/ogg; codecs=opus".into()),
                file_length: Some(900),
                ..Default::default()
            }),
            ..Default::default()
        };
        let nm = convert(&msg).expect("converted");
        assert_eq!(nm.kind, MessageKind::Voice);
        assert_eq!(nm.media_duration, Some(12));
        assert_eq!(nm.media_size, Some(900));

        let music = wa::Message {
            audio_message: MessageField::some(wa::message::AudioMessage {
                ptt: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(convert(&music).expect("converted").kind, MessageKind::Audio);
    }

    #[test]
    fn document_with_file_name() {
        let msg = wa::Message {
            document_message: MessageField::some(wa::message::DocumentMessage {
                file_name: Some("report.pdf".into()),
                mimetype: Some("application/pdf".into()),
                file_length: Some(10),
                caption: Some("see attached".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let nm = convert(&msg).expect("converted");
        assert_eq!(nm.kind, MessageKind::Document);
        assert_eq!(nm.media_name.as_deref(), Some("report.pdf"));
        assert_eq!(nm.text.as_deref(), Some("see attached"));
        assert_eq!(preview(&nm), "see attached");
    }

    #[test]
    fn ephemeral_wrapper_is_unwrapped() {
        let inner = wa::Message::text("secret");
        let msg = wa::Message {
            ephemeral_message: MessageField::some(wa::message::FutureProofMessage {
                message: MessageField::some(inner),
                ..Default::default()
            }),
            ..Default::default()
        };
        let nm = convert(&msg).expect("converted");
        assert_eq!(nm.kind, MessageKind::Text);
        assert_eq!(nm.text.as_deref(), Some("secret"));
    }

    #[test]
    fn protocol_and_reaction_messages_are_skipped() {
        let protocol = wa::Message {
            protocol_message: MessageField::some(wa::message::ProtocolMessage::default()),
            ..Default::default()
        };
        assert!(convert(&protocol).is_none());

        let reaction = wa::Message {
            reaction_message: MessageField::some(wa::message::ReactionMessage::default()),
            ..Default::default()
        };
        assert!(convert(&reaction).is_none());

        assert!(convert(&wa::Message::default()).is_none());
    }

    fn web_info(from_me: bool, participant: Option<&str>) -> wa::WebMessageInfo {
        wa::WebMessageInfo {
            key: MessageField::some(wa::MessageKey {
                remote_jid: Some("g1@g.us".into()),
                from_me: Some(from_me),
                id: Some("H1".into()),
                participant: participant.map(String::from),
            }),
            message: MessageField::some(wa::Message::text("old message")),
            message_timestamp: Some(1_600_000_000),
            status: Some(wa::web_message_info::Status::READ),
            ..Default::default()
        }
    }

    #[test]
    fn history_message_from_me() {
        let wmi = web_info(true, None);
        let nm = from_web_message_info(&wmi, "g1@g.us", "999@s.whatsapp.net", &|j| j.to_string())
            .expect("converted");
        assert!(nm.from_me);
        assert_eq!(nm.sender_jid, "999@s.whatsapp.net");
        assert_eq!(nm.ts, 1_600_000_000);
        assert_eq!(nm.id, "H1");
        assert_eq!(web_status(&wmi), Some(MessageStatus::Read));
    }

    #[test]
    fn history_group_message_uses_resolved_participant() {
        let wmi = web_info(false, Some("111@lid"));
        let nm = from_web_message_info(&wmi, "g1@g.us", "999@s.whatsapp.net", &|j| {
            assert_eq!(j, "111@lid");
            "5511@s.whatsapp.net".to_string()
        })
        .expect("converted");
        assert!(!nm.from_me);
        assert_eq!(nm.sender_jid, "5511@s.whatsapp.net");
        assert_eq!(nm.chat_jid, "g1@g.us");
        assert_eq!(nm.text.as_deref(), Some("old message"));
    }

    #[test]
    fn history_dm_message_sender_is_chat() {
        let wmi = web_info(false, None);
        let nm = from_web_message_info(&wmi, "77@s.whatsapp.net", "999@s.whatsapp.net", &|j| {
            j.to_string()
        })
        .expect("converted");
        assert_eq!(nm.sender_jid, "77@s.whatsapp.net");
    }

    #[test]
    fn history_stub_without_message_is_skipped() {
        let mut wmi = web_info(false, None);
        wmi.message = MessageField::none();
        assert!(from_web_message_info(&wmi, "g1@g.us", "x", &|j| j.to_string()).is_none());
    }

    fn protocol_msg(
        kind: wa::message::protocol_message::Type,
        id: &str,
        edited: Option<wa::Message>,
    ) -> wa::Message {
        wa::Message {
            protocol_message: MessageField::some(wa::message::ProtocolMessage {
                key: MessageField::some(wa::MessageKey {
                    id: Some(id.into()),
                    ..Default::default()
                }),
                r#type: Some(kind),
                edited_message: edited.map_or_else(MessageField::none, MessageField::some),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn revoke_is_detected() {
        use wa::message::protocol_message::Type;
        let msg = protocol_msg(Type::REVOKE, "TARGET", None);
        assert_eq!(
            protocol_action(&msg),
            Some(ProtocolAction::Revoke { id: "TARGET".into() })
        );
        assert!(convert(&msg).is_none(), "not stored as a message of its own");
    }

    #[test]
    fn edit_is_detected_directly_and_inside_wrapper() {
        use wa::message::protocol_message::Type;
        let direct = protocol_msg(Type::MESSAGE_EDIT, "T1", Some(wa::Message::text("new text")));
        let expected = Some(ProtocolAction::Edit {
            id: "T1".into(),
            text: "new text".into(),
        });
        assert_eq!(protocol_action(&direct), expected);

        let wrapped = wa::Message {
            edited_message: MessageField::some(wa::message::FutureProofMessage {
                message: MessageField::some(direct),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(protocol_action(&wrapped), expected);
    }

    #[test]
    fn edit_without_text_and_other_protocol_types_are_ignored() {
        use wa::message::protocol_message::Type;
        assert_eq!(protocol_action(&protocol_msg(Type::MESSAGE_EDIT, "T", None)), None);
        assert_eq!(protocol_action(&protocol_msg(Type::EPHEMERAL_SETTING, "T", None)), None);
        assert_eq!(protocol_action(&wa::Message::text("plain")), None);
    }

    #[test]
    fn history_revoke_reports_sender() {
        use wa::message::protocol_message::Type;
        let mut wmi = web_info(false, Some("111@lid"));
        wmi.message = MessageField::some(protocol_msg(Type::REVOKE, "H0", None));
        let (sender, from_me, action) =
            web_protocol_action(&wmi, "g1@g.us", "999@s.whatsapp.net", &|_| "5511@s.whatsapp.net".into())
                .expect("detected");
        assert_eq!(sender, "5511@s.whatsapp.net");
        assert!(!from_me);
        assert_eq!(action, ProtocolAction::Revoke { id: "H0".into() });
        assert!(from_web_message_info(&wmi, "g1@g.us", "x", &|j| j.to_string()).is_none());
    }

    #[test]
    fn preview_truncates_long_text() {
        let long = "a".repeat(500);
        let nm = convert(&wa::Message::text(long)).expect("converted");
        assert_eq!(preview(&nm).chars().count(), PREVIEW_MAX_CHARS + 1);
    }

    // ---- business / structured messages and unknown types ----

    fn field<T: Default>(v: T) -> MessageField<T> {
        MessageField::some(v)
    }

    fn hydrated_template() -> wa::message::TemplateMessage {
        use wa::message::template_message::{hydrated_four_row_template::Title, Format, HydratedFourRowTemplate};
        use wa::hydrated_template_button::{HydratedButton, HydratedQuickReplyButton, HydratedURLButton};
        wa::message::TemplateMessage {
            format: Some(Format::HydratedFourRowTemplate(Box::new(HydratedFourRowTemplate {
                hydrated_content_text: Some("Your order has shipped".into()),
                hydrated_footer_text: Some("Thanks for shopping".into()),
                hydrated_buttons: vec![
                    wa::HydratedTemplateButton {
                        hydrated_button: Some(HydratedButton::UrlButton(Box::new(HydratedURLButton {
                            display_text: Some("Track order".into()),
                            url: Some("https://example.com/track".into()),
                            ..Default::default()
                        }))),
                        ..Default::default()
                    },
                    wa::HydratedTemplateButton {
                        hydrated_button: Some(HydratedButton::QuickReplyButton(Box::new(
                            HydratedQuickReplyButton {
                                display_text: Some("Stop".into()),
                                ..Default::default()
                            },
                        ))),
                        ..Default::default()
                    },
                ],
                title: Some(Title::HydratedTitleText("Order update".into())),
                ..Default::default()
            }))),
            ..Default::default()
        }
    }

    #[test]
    fn hydrated_template_shows_text_footer_and_buttons_with_urls() {
        let msg = wa::Message {
            template_message: field(hydrated_template()),
            ..Default::default()
        };
        let nm = convert(&msg).expect("template is displayed");
        assert_eq!(nm.kind, MessageKind::Other);
        assert_eq!(
            nm.text.as_deref(),
            Some("Order update\n\nYour order has shipped\n\nThanks for shopping\n\n[Track order] https://example.com/track\n[Stop]")
        );
    }

    #[test]
    fn template_with_image_header_is_an_image_row_whose_raw_is_downloadable() {
        use wa::message::template_message::{hydrated_four_row_template::Title, Format, HydratedFourRowTemplate};
        let image = wa::message::ImageMessage {
            mimetype: Some("image/jpeg".into()),
            file_length: Some(1234),
            jpeg_thumbnail: Some(vec![1, 2, 3]),
            media_key: Some(vec![9; 32]),
            ..Default::default()
        };
        let msg = wa::Message {
            template_message: field(wa::message::TemplateMessage {
                format: Some(Format::HydratedFourRowTemplate(Box::new(HydratedFourRowTemplate {
                    hydrated_content_text: Some("Sale".into()),
                    title: Some(Title::ImageMessage(Box::new(image))),
                    ..Default::default()
                }))),
                ..Default::default()
            }),
            ..Default::default()
        };
        let nm = convert(&msg).expect("converted");
        assert_eq!(nm.kind, MessageKind::Image);
        assert_eq!(nm.text.as_deref(), Some("Sale"));
        assert_eq!(nm.media_mime.as_deref(), Some("image/jpeg"));
        assert_eq!(nm.media_size, Some(1234));
        assert_eq!(nm.thumbnail, Some(vec![1, 2, 3]));
        let raw = wa::Message::decode_from_slice(&nm.raw.expect("raw")).expect("decodes");
        assert_eq!(raw.get_base_message().image_message.as_option().and_then(|i| i.media_key.clone()), Some(vec![9; 32]));
    }

    #[test]
    fn legacy_hydrated_template_field_is_read() {
        let hydrated = match hydrated_template().format {
            Some(wa::message::template_message::Format::HydratedFourRowTemplate(h)) => *h,
            _ => unreachable!(),
        };
        let msg = wa::Message {
            template_message: field(wa::message::TemplateMessage {
                hydrated_template: field(hydrated),
                ..Default::default()
            }),
            ..Default::default()
        };
        let nm = convert(&msg).expect("converted");
        assert!(nm.text.as_deref().is_some_and(|t| t.contains("Your order has shipped")));
    }

    #[test]
    fn interactive_message_shows_header_body_footer_and_native_flow_buttons() {
        use wa::message::interactive_message::{
            native_flow_message::NativeFlowButton, Body, Footer, Header, InteractiveMessage, NativeFlowMessage,
        };
        let msg = wa::Message {
            interactive_message: field(wa::message::InteractiveMessage {
                header: field(Header {
                    title: Some("Hello".into()),
                    ..Default::default()
                }),
                body: field(Body { text: Some("Big sale today".into()) }),
                footer: field(Footer {
                    text: Some("Reply STOP to opt out".into()),
                    ..Default::default()
                }),
                interactive_message: Some(InteractiveMessage::NativeFlowMessage(Box::new(NativeFlowMessage {
                    buttons: vec![
                        NativeFlowButton {
                            name: Some("cta_url".into()),
                            button_params_json: Some(r#"{"display_text":"Visit website","url":"https://shop.example"}"#.into()),
                        },
                        NativeFlowButton {
                            name: Some("quick_reply".into()),
                            button_params_json: Some(r#"{"display_text":"Later","id":"1"}"#.into()),
                        },
                    ],
                    ..Default::default()
                }))),
                ..Default::default()
            }),
            ..Default::default()
        };
        let nm = convert(&msg).expect("converted");
        assert_eq!(
            nm.text.as_deref(),
            Some("Hello\n\nBig sale today\n\nReply STOP to opt out\n\n[Visit website] https://shop.example\n[Later]")
        );
    }

    #[test]
    fn buttons_message_and_list_message_are_displayed() {
        use wa::message::buttons_message::{button::ButtonText, Button};
        let buttons = wa::Message {
            buttons_message: field(wa::message::ButtonsMessage {
                content_text: Some("Pick one".into()),
                footer_text: Some("Footer".into()),
                buttons: vec![Button {
                    button_text: field(ButtonText { display_text: Some("Yes".into()) }),
                    ..Default::default()
                }],
                header: Some(wa::message::buttons_message::Header::Text("Title".into())),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(convert(&buttons).expect("converted").text.as_deref(), Some("Title\n\nPick one\n\nFooter\n\n[Yes]"));

        let list = wa::Message {
            list_message: field(wa::message::ListMessage {
                title: Some("Menu".into()),
                description: Some("Choose".into()),
                button_text: Some("Open".into()),
                sections: vec![wa::message::list_message::Section {
                    title: Some("Drinks".into()),
                    rows: vec![wa::message::list_message::Row {
                        title: Some("Tea".into()),
                        description: Some("hot".into()),
                        ..Default::default()
                    }],
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            convert(&list).expect("converted").text.as_deref(),
            Some("Menu\n\nChoose\n\nDrinks\n• Tea - hot\n\n[Open]")
        );
    }

    #[test]
    fn product_order_and_highly_structured_messages_are_displayed() {
        let product = wa::Message {
            product_message: field(wa::message::ProductMessage {
                product: field(wa::message::product_message::ProductSnapshot {
                    title: Some("Shoe".into()),
                    currency_code: Some("USD".into()),
                    price_amount1000: Some(12_500),
                    ..Default::default()
                }),
                body: Some("Limited".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(convert(&product).expect("converted").text.as_deref(), Some("Shoe\n\nUSD 12.50\n\nLimited"));

        let order = wa::Message {
            order_message: field(wa::message::OrderMessage {
                order_title: Some("Order #1".into()),
                item_count: Some(2),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(convert(&order).expect("converted").text.as_deref().is_some_and(|t| t.contains("Order #1")));

        let hsm = wa::Message {
            highly_structured_message: field(wa::message::HighlyStructuredMessage {
                fallback_lg: Some("Hi there".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(convert(&hsm).expect("converted").text.as_deref(), Some("Hi there"));
    }

    #[test]
    fn button_and_list_replies_show_the_selection() {
        let button = wa::Message {
            buttons_response_message: field(wa::message::ButtonsResponseMessage {
                selected_button_id: Some("b1".into()),
                response: Some(wa::message::buttons_response_message::Response::SelectedDisplayText("Yes".into())),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(convert(&button).expect("converted").text.as_deref(), Some("Yes"));
        let list = wa::Message {
            list_response_message: field(wa::message::ListResponseMessage {
                title: Some("Tea".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(convert(&list).expect("converted").text.as_deref(), Some("Tea"));
        let template = wa::Message {
            template_button_reply_message: field(wa::message::TemplateButtonReplyMessage {
                selected_display_text: Some("Stop".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(convert(&template).expect("converted").text.as_deref(), Some("Stop"));
    }

    #[test]
    fn business_message_inside_wrappers_is_unwrapped() {
        let inner = wa::Message {
            template_message: field(hydrated_template()),
            ..Default::default()
        };
        let wrapped = wa::Message {
            ephemeral_message: field(wa::message::FutureProofMessage {
                message: field(wa::Message {
                    bot_invoke_message: field(wa::message::FutureProofMessage {
                        message: field(wa::Message {
                            document_with_caption_message: field(wa::message::FutureProofMessage {
                                message: field(inner),
                            }),
                            ..Default::default()
                        }),
                    }),
                    ..Default::default()
                }),
            }),
            ..Default::default()
        };
        let nm = convert(&wrapped).expect("converted");
        assert!(nm.text.as_deref().is_some_and(|t| t.contains("Your order has shipped")));
    }

    #[test]
    fn unknown_message_types_still_get_a_row() {
        let unknown = wa::Message {
            group_invite_message: field(wa::message::GroupInviteMessage::default()),
            ..Default::default()
        };
        let nm = convert(&unknown).expect("an unsupported row is created");
        assert_eq!(nm.kind, MessageKind::Other);
        assert_eq!(nm.text.as_deref(), Some(UNSUPPORTED_TEXT));
        // A template without any visible text is not dropped either.
        let empty_template = wa::Message {
            template_message: field(wa::message::TemplateMessage::default()),
            ..Default::default()
        };
        assert_eq!(convert(&empty_template).expect("row").text.as_deref(), Some(UNSUPPORTED_TEXT));
    }

    #[test]
    fn hidden_system_messages_do_not_get_a_row() {
        for msg in [
            wa::Message {
                message_context_info: field(wa::MessageContextInfo::default()),
                ..Default::default()
            },
            wa::Message {
                sender_key_distribution_message: field(wa::message::SenderKeyDistributionMessage::default()),
                ..Default::default()
            },
            wa::Message {
                pin_in_chat_message: field(wa::message::PinInChatMessage::default()),
                ..Default::default()
            },
            wa::Message {
                keep_in_chat_message: field(wa::message::KeepInChatMessage::default()),
                ..Default::default()
            },
            wa::Message {
                enc_reaction_message: field(wa::message::EncReactionMessage::default()),
                ..Default::default()
            },
        ] {
            assert!(convert(&msg).is_none());
        }
    }

    #[test]
    fn history_message_with_a_business_template_converts() {
        let mut wmi = web_info(false, None);
        wmi.message = field(wa::Message {
            template_message: field(hydrated_template()),
            ..Default::default()
        });
        let nm = from_web_message_info(&wmi, "5@s.whatsapp.net", "me@s.whatsapp.net", &|j| j.to_string())
            .expect("history template is kept");
        assert!(nm.text.as_deref().is_some_and(|t| t.contains("Track order")));
        wmi.message = field(wa::Message {
            group_invite_message: field(wa::message::GroupInviteMessage::default()),
            ..Default::default()
        });
        let nm = from_web_message_info(&wmi, "5@s.whatsapp.net", "me@s.whatsapp.net", &|j| j.to_string())
            .expect("history unknown type is kept");
        assert_eq!(nm.text.as_deref(), Some(UNSUPPORTED_TEXT));
    }
}

#[cfg(test)]
mod robustness_tests {
    use super::*;
    use whatsapp_rust::buffa::MessageField;
    use wa_store::Store;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            // xorshift64*
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
    }

    fn info_with(id: &str, secs: i64) -> MessageInfo {
        let mut info = MessageInfo::default();
        info.id = id.to_string();
        info.timestamp =
            whatsapp_rust::chrono::DateTime::from_timestamp(secs, 0).unwrap_or_default();
        info
    }

    fn convert(msg: &wa::Message) -> Option<NewMessage> {
        from_inbound(msg, &info_with("ID", 1_700_000_000), "1@s.whatsapp.net", "1@s.whatsapp.net")
    }

    fn secret_image(view_once_flag: bool) -> wa::Message {
        wa::Message {
            image_message: MessageField::some(wa::message::ImageMessage {
                caption: Some("secret".into()),
                mimetype: Some("image/jpeg".into()),
                direct_path: Some("/v/t62/abc".into()),
                media_key: Some(vec![1; 32]),
                jpeg_thumbnail: Some(vec![9; 8]),
                file_length: Some(10),
                view_once: view_once_flag.then_some(true),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn assert_placeholder(nm: &NewMessage) {
        assert_eq!(nm.kind, MessageKind::Other);
        assert_eq!(nm.text.as_deref(), Some(VIEW_ONCE_TEXT));
        assert!(nm.raw.is_none() && nm.thumbnail.is_none());
        assert!(nm.media_mime.is_none() && nm.media_size.is_none() && nm.media_name.is_none());
        assert_eq!(preview(nm), VIEW_ONCE_PREVIEW);
    }

    #[test]
    fn view_once_media_is_stored_as_a_placeholder_without_media_data() {
        for wrapper in 0..3 {
            let f = MessageField::some(wa::message::FutureProofMessage {
                message: MessageField::some(secret_image(false)),
                ..Default::default()
            });
            let mut m = wa::Message::default();
            match wrapper {
                0 => m.view_once_message = f,
                1 => m.view_once_message_v2 = f,
                _ => m.view_once_message_v2_extension = f,
            }
            assert_placeholder(&convert(&m).expect("converted"));
        }
        // Flag on the media itself, without a wrapper.
        assert_placeholder(&convert(&secret_image(true)).expect("converted"));
        // An ordinary image is untouched.
        let normal = convert(&secret_image(false)).expect("converted");
        assert_eq!(normal.kind, MessageKind::Image);
        assert!(normal.raw.is_some());
    }

    #[test]
    fn view_once_history_messages_are_placeholders_too() {
        let wmi = wa::WebMessageInfo {
            key: MessageField::some(wa::MessageKey {
                id: Some("H1".into()),
                from_me: Some(false),
                ..Default::default()
            }),
            message: MessageField::some(secret_image(true)),
            message_timestamp: Some(1_700_000_000),
            ..Default::default()
        };
        let nm = from_web_message_info(&wmi, "1@s.whatsapp.net", "me@s.whatsapp.net", &|s| s.to_string())
            .expect("converted");
        assert_placeholder(&nm);
        assert_eq!(quote_preview(&secret_image(true)).as_deref(), Some(VIEW_ONCE_PREVIEW));
    }

    #[test]
    fn quoting_view_once_media_stores_no_key_url_or_thumbnail() {
        let quoted = wa::Message {
            view_once_message: MessageField::some(wa::message::FutureProofMessage {
                message: MessageField::some(secret_image(false)),
                ..Default::default()
            }),
            ..Default::default()
        };
        for q in [quoted, secret_image(true)] {
            let reply = wa::Message {
                extended_text_message: MessageField::some(wa::message::ExtendedTextMessage {
                    text: Some("reply".into()),
                    context_info: MessageField::some(wa::ContextInfo {
                        stanza_id: Some("Q1".into()),
                        quoted_message: MessageField::some(q),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let nm = convert(&reply).expect("converted");
            assert_eq!(nm.quoted_preview.as_deref(), Some(VIEW_ONCE_PREVIEW));
            let stored = wa::Message::decode_from_slice(nm.raw.as_deref().expect("raw")).expect("decodes");
            let quote = context_info_of(&stored).and_then(|c| c.quoted_message.as_option()).expect("quote");
            assert_eq!(quote.conversation.as_deref(), Some(VIEW_ONCE_PREVIEW));
            assert!(quote.image_message.as_option().is_none());
            assert!(!is_view_once(quote));
        }
    }

    #[test]
    fn view_once_media_is_detected_for_the_download_and_forward_guards() {
        assert!(is_view_once(&secret_image(true)));
        assert!(!is_view_once(&secret_image(false)));
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    fn view_once_variants() -> Vec<wa::Message> {
        let mut out = Vec::new();
        for wrapper in 0..3 {
            let f = MessageField::some(wa::message::FutureProofMessage {
                message: MessageField::some(secret_image(false)),
                ..Default::default()
            });
            let mut m = wa::Message::default();
            match wrapper {
                0 => m.view_once_message = f,
                1 => m.view_once_message_v2 = f,
                _ => m.view_once_message_v2_extension = f,
            }
            out.push(m);
        }
        out.push(secret_image(true));
        out
    }

    #[test]
    fn stored_view_once_rows_have_kind_other_and_no_raw() {
        let store = wa_store::Store::open_in_memory().unwrap();
        for (i, m) in view_once_variants().iter().enumerate() {
            let mut nm = convert(m).expect("converted");
            nm.id = format!("vo{i}");
            store.insert_message(&nm, true).unwrap();
            let row = store.message("1@s.whatsapp.net", &nm.id).unwrap().expect("stored");
            assert_eq!(row.kind, MessageKind::Other, "variant {i}");
            assert_eq!(row.text.as_deref(), Some(VIEW_ONCE_TEXT), "variant {i}");
            assert!(row.thumbnail.is_none(), "variant {i}");
            assert_eq!(store.message_raw("1@s.whatsapp.net", &nm.id).unwrap(), None, "variant {i}");
        }
    }

    #[test]
    fn stored_reply_to_view_once_holds_a_stub_and_no_media_secrets_anywhere() {
        let store = wa_store::Store::open_in_memory().unwrap();
        for (i, q) in view_once_variants().into_iter().enumerate() {
            let reply = wa::Message {
                extended_text_message: MessageField::some(wa::message::ExtendedTextMessage {
                    text: Some("reply".into()),
                    context_info: MessageField::some(wa::ContextInfo {
                        stanza_id: Some("Q1".into()),
                        quoted_message: MessageField::some(q),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let mut nm = convert(&reply).expect("converted");
            nm.id = format!("r{i}");
            store.insert_message(&nm, true).unwrap();
            let raw = store.message_raw("1@s.whatsapp.net", &nm.id).unwrap().expect("raw kept for the reply");
            let stored = wa::Message::decode_from_slice(&raw).expect("decodes");
            let quote = context_info_of(&stored).and_then(|c| c.quoted_message.as_option()).expect("quote stub");
            assert_eq!(quote.conversation.as_deref(), Some(VIEW_ONCE_PREVIEW), "variant {i}");
            assert!(contains(&raw, b"reply"), "the reply itself is kept");
            assert!(!contains(&raw, &[1u8; 32]), "media key leaked, variant {i}");
            assert!(!contains(&raw, &[9u8; 8]), "thumbnail leaked, variant {i}");
            assert!(!contains(&raw, b"/v/t62/abc"), "media path leaked, variant {i}");
            assert!(!contains(&raw, b"secret"), "caption leaked, variant {i}");
        }
    }

    fn wrap(kind: usize, inner: wa::Message) -> wa::Message {
        let f = MessageField::some(wa::message::FutureProofMessage {
            message: MessageField::some(inner),
            ..Default::default()
        });
        let mut m = wa::Message::default();
        match kind % 3 {
            0 => m.ephemeral_message = f,
            1 => m.view_once_message = f,
            _ => m.document_with_caption_message = f,
        }
        m
    }

    #[test]
    fn garbage_bytes_never_panic_on_decode_or_convert() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut errors = 0;
        for _ in 0..2000 {
            let len = (rng.next() % 512) as usize;
            let bytes: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
            match wa::Message::decode_from_slice(&bytes) {
                Ok(m) => {
                    let _ = convert(&m);
                }
                Err(_) => errors += 1,
            }
        }
        // Random bytes are overwhelmingly invalid protobuf.
        assert!(errors > 1000, "only {errors} of 2000 garbage inputs were rejected");
    }

    #[test]
    fn garbage_web_message_info_never_panics() {
        let mut rng = Rng(42);
        for _ in 0..2000 {
            let len = (rng.next() % 512) as usize;
            let bytes: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
            if let Ok(w) = wa::WebMessageInfo::decode_from_slice(&bytes) {
                let _ = from_web_message_info(&w, "g@g.us", "me@s.whatsapp.net", &|j| j.to_string());
                let _ = web_protocol_action(&w, "g@g.us", "me@s.whatsapp.net", &|j| j.to_string());
                let _ = web_status(&w);
            }
        }
    }

    #[test]
    fn empty_message_is_skipped() {
        assert!(convert(&wa::Message::default()).is_none());
        let decoded = wa::Message::decode_from_slice(&[]).expect("empty bytes are a valid message");
        assert!(convert(&decoded).is_none());
    }

    #[test]
    fn media_messages_with_all_optionals_none_convert() {
        let msgs = [
            wa::Message { image_message: MessageField::some(Default::default()), ..Default::default() },
            wa::Message { video_message: MessageField::some(Default::default()), ..Default::default() },
            wa::Message { audio_message: MessageField::some(Default::default()), ..Default::default() },
            wa::Message { document_message: MessageField::some(Default::default()), ..Default::default() },
            wa::Message { sticker_message: MessageField::some(Default::default()), ..Default::default() },
            wa::Message { extended_text_message: MessageField::some(Default::default()), ..Default::default() },
            wa::Message { location_message: MessageField::some(Default::default()), ..Default::default() },
            wa::Message { contact_message: MessageField::some(Default::default()), ..Default::default() },
            wa::Message { poll_creation_message: MessageField::some(Default::default()), ..Default::default() },
        ];
        for m in &msgs {
            let nm = convert(m).expect("default-valued content is still displayable");
            let _ = preview(&nm);
        }
    }

    #[test]
    fn incoming_locations_keep_their_coordinates() {
        let live = wa::Message {
            live_location_message: MessageField::some(wa::message::LiveLocationMessage {
                degrees_latitude: Some(35.7),
                degrees_longitude: Some(51.4),
                caption: Some("on my way".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let nm = convert(&live).expect("converted");
        assert_eq!((nm.geo_lat, nm.geo_lon), (Some(35.7), Some(51.4)));
        assert_eq!(nm.text.as_deref(), Some("📍 Live location
on my way"));

        let fixed = wa::Message {
            location_message: MessageField::some(wa::message::LocationMessage {
                degrees_latitude: Some(-33.86),
                degrees_longitude: Some(151.2),
                name: Some("Opera".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let nm = convert(&fixed).expect("converted");
        assert_eq!((nm.geo_lat, nm.geo_lon), (Some(-33.86), Some(151.2)));
        assert_eq!(nm.text.as_deref(), Some("📍 Opera"));

        let bogus = wa::Message {
            location_message: MessageField::some(wa::message::LocationMessage {
                degrees_latitude: Some(95.0),
                degrees_longitude: Some(10.0),
                ..Default::default()
            }),
            ..Default::default()
        };
        let nm = convert(&bogus).expect("converted");
        assert_eq!((nm.geo_lat, nm.geo_lon), (None, None));
    }

    fn contact(name: Option<&str>, vcard: &str) -> wa::message::ContactMessage {
        wa::message::ContactMessage {
            display_name: name.map(String::from),
            vcard: Some(vcard.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn incoming_link_preview_is_copied_without_network() {
        let mut thumb = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut thumb, 70)
            .encode_image(&image::RgbImage::from_pixel(8, 8, image::Rgb([1, 2, 3])))
            .expect("encoded");
        let p = crate::preview::LinkPreview {
            url: "https://x.test/a".into(),
            title: "Title".into(),
            description: String::new(),
            thumbnail_jpeg: Some(thumb.clone()),
        };
        let nm = convert(&crate::preview::text_message_with_preview("see https://x.test/a", &p)).expect("converted");
        assert_eq!(nm.kind, MessageKind::Text);
        assert_eq!(nm.link_url.as_deref(), Some("https://x.test/a"));
        assert_eq!(nm.link_title.as_deref(), Some("Title"));
        assert_eq!(nm.link_desc, None);
        assert_eq!(nm.link_thumb, Some(thumb));
        let plain = convert(&wa::Message::text("hello")).expect("converted");
        assert_eq!((plain.link_url, plain.link_thumb), (None, None));
    }

    #[test]
    fn single_shared_contact_lists_name_and_normalized_phone() {
        let vc = "BEGIN:VCARD\r\nVERSION:3.0\r\nN:Doe;Jane;;;\r\nFN:Jane Doe\r\nTEL;type=CELL;waid=491701234567:+49 170 1234567\r\nEND:VCARD";
        let m = wa::Message { contact_message: MessageField::some(contact(Some("Jane"), vc)), ..Default::default() };
        let nm = convert(&m).expect("converted");
        assert_eq!(nm.kind, MessageKind::Other);
        assert_eq!(nm.media_mime.as_deref(), Some("text/vcard"));
        assert_eq!(nm.media_name.as_deref(), Some("Jane Doe"));
        assert_eq!(nm.text.as_deref(), Some("👤 Jane Doe\n+491701234567"));
        assert_eq!(preview(&nm), "👤 Jane Doe");
    }

    #[test]
    fn multiple_phones_folded_lines_and_quoted_printable() {
        let vc = "BEGIN:VCARD\nitem1.TEL;type=CELL:+1 (555) 010-0001\nTEL:555 010 0002\nTEL:+15550100001\nFN;CHARSET=UTF-8;ENCODING=QUOTED-PRINTABLE:Ren=C3=A9=\n e\nNOTE:long\n  folded\nEND:VCARD";
        let c = parse_vcard(vc);
        assert_eq!(c.phones, vec!["+15550100001", "+5550100002"]);
        assert_eq!(c.full_name.as_deref(), Some("René e"));
    }

    #[test]
    fn vcard_values_are_unescaped() {
        let c = parse_vcard("BEGIN:VCARD\r\nN:A\\;B;Ali\\, Reza\\\\x;;;\r\nFN:Ali\\, Reza\\; \\\\x\\nY\r\nEND:VCARD");
        assert_eq!(c.full_name.as_deref(), Some("Ali, Reza; \\x\nY"));
        assert_eq!(c.structured_name.as_deref(), Some("Ali, Reza\\x A;B"));
    }

    #[test]
    fn missing_fn_falls_back_to_n_then_phone() {
        let with_n = contact(None, "BEGIN:VCARD\nN:Doe;John;;;\nTEL:+100200\nEND:VCARD");
        assert_eq!(contact_card(&with_n).name, "John Doe");
        let only_phone = contact(None, "BEGIN:VCARD\nTEL:+100 200\nEND:VCARD");
        assert_eq!(contact_card(&only_phone).name, "+100200");
        assert_eq!(contact_card(&contact(None, "")).name, "Contact");
    }

    #[test]
    fn contacts_array_of_three() {
        let cards = ["Ann", "Bob", "Cy"]
            .iter()
            .enumerate()
            .map(|(i, n)| contact(None, &format!("BEGIN:VCARD\nFN:{n}\nTEL:+10{i}\nEND:VCARD")))
            .collect();
        let m = wa::Message {
            contacts_array_message: MessageField::some(wa::message::ContactsArrayMessage {
                contacts: cards,
                ..Default::default()
            }),
            ..Default::default()
        };
        let nm = convert(&m).expect("converted");
        assert_eq!(nm.media_name.as_deref(), Some("Ann and 2 others"));
        assert_eq!(nm.text.as_deref(), Some("👤 Ann\n+100\n\n👤 Bob\n+101\n\n👤 Cy\n+102"));
        assert_eq!(preview(&nm), "👤 Ann and 2 others");
    }

    #[test]
    fn one_megabyte_text_is_preserved_and_preview_is_bounded() {
        let big = "x".repeat(1 << 20);
        let nm = convert(&wa::Message::text(big.clone())).expect("converted");
        assert_eq!(nm.text.as_deref().map(str::len), Some(1 << 20));
        assert_eq!(preview(&nm).chars().count(), PREVIEW_MAX_CHARS + 1);
        let raw = nm.raw.expect("raw");
        let back = wa::Message::decode_from_slice(&raw).expect("decodes");
        assert_eq!(back.conversation.as_deref(), Some(big.as_str()));
    }

    #[test]
    fn realistic_wrapper_chain_ephemeral_viewonce_documentwithcaption_is_unwrapped() {
        let mut m = wa::Message::text("deep");
        for level in [2usize, 1, 0] {
            m = wrap(level, m);
        }
        let nm = convert(&m).expect("converted");
        assert_eq!(nm.text.as_deref(), Some("deep"));
        assert_eq!(nm.kind, MessageKind::Text);
    }

    #[test]
    fn five_level_wrapper_chain_is_unwrapped() {
        let mut m = wa::Message::text("deep");
        for level in [2usize, 1, 0, 1, 0] {
            m = wrap(level, m);
        }
        let nm = convert(&m).expect("converted");
        assert_eq!(nm.text.as_deref(), Some("deep"));
    }

    #[test]
    fn very_deep_wrapper_chain_does_not_panic() {
        let mut m = wa::Message::text("deep");
        for level in 0..60 {
            m = wrap(level, m);
        }
        let _ = convert(&m);
    }

    #[test]
    fn extreme_timestamps_do_not_panic() {
        let msg = wa::Message::text("t");
        for secs in [i64::MIN, -1, 0, 1, i64::MAX, 253_402_300_800] {
            let nm = from_inbound(&msg, &info_with("T", secs), "1@s.whatsapp.net", "1@s.whatsapp.net")
                .expect("converted");
            assert_eq!(nm.id, "T");
        }
        for ts in [0u64, 1, u64::MAX, i64::MAX as u64, i64::MAX as u64 + 1] {
            let wmi = wa::WebMessageInfo {
                key: MessageField::some(wa::MessageKey {
                    id: Some("W".into()),
                    from_me: Some(true),
                    ..Default::default()
                }),
                message: MessageField::some(msg.clone()),
                message_timestamp: Some(ts),
                ..Default::default()
            };
            let nm = from_web_message_info(&wmi, "1@s.whatsapp.net", "me@s.whatsapp.net", &|j| j.into())
                .expect("converted");
            assert!(nm.ts >= 0, "ts {ts} became {}", nm.ts);
        }
    }

    #[test]
    fn huge_file_length_does_not_overflow_size() {
        let m = wa::Message {
            image_message: MessageField::some(wa::message::ImageMessage {
                file_length: Some(u64::MAX),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(convert(&m).expect("converted").media_size, None);
    }

    #[test]
    fn web_message_missing_key_id_or_message_is_skipped() {
        let id = |r: Option<&str>| wa::MessageKey { id: r.map(String::from), ..Default::default() };
        let resolve = |j: &str| j.to_string();
        let msg = MessageField::some(wa::Message::text("x"));
        let none_key = wa::WebMessageInfo { message: msg.clone(), ..Default::default() };
        let no_id = wa::WebMessageInfo { key: MessageField::some(id(None)), message: msg.clone(), ..Default::default() };
        let empty_id = wa::WebMessageInfo { key: MessageField::some(id(Some(""))), message: msg.clone(), ..Default::default() };
        let no_msg = wa::WebMessageInfo { key: MessageField::some(id(Some("A"))), ..Default::default() };
        for w in [&none_key, &no_id, &empty_id, &no_msg, &wa::WebMessageInfo::default()] {
            assert!(from_web_message_info(w, "1@s.whatsapp.net", "me@s.whatsapp.net", &resolve).is_none());
            assert!(web_protocol_action(w, "1@s.whatsapp.net", "me@s.whatsapp.net", &resolve).is_none());
            assert!(web_status(w).is_none());
        }
    }

    #[test]
    fn invalid_jids_do_not_panic() {
        let jids = ["", "@", "abc", "a@b@c", "@g.us", "\0@g.us", "é@g.us"];
        let resolve = |j: &str| j.to_string();
        for jid in jids {
            for from_me in [true, false] {
                let wmi = wa::WebMessageInfo {
                    key: MessageField::some(wa::MessageKey {
                        remote_jid: Some(jid.into()),
                        from_me: Some(from_me),
                        id: Some("J".into()),
                        participant: Some(jid.into()),
                    }),
                    message: MessageField::some(wa::Message::text("x")),
                    ..Default::default()
                };
                let _ = from_web_message_info(&wmi, jid, jid, &resolve);
                let _ = web_protocol_action(&wmi, jid, jid, &resolve);
                let nm = from_inbound(&wa::Message::text("x"), &info_with("J", 1), jid, jid);
                assert_eq!(nm.expect("converted").chat_jid, jid);
            }
        }
    }

    #[test]
    fn group_message_without_participant_is_skipped() {
        let wmi = wa::WebMessageInfo {
            key: MessageField::some(wa::MessageKey {
                id: Some("G".into()),
                from_me: Some(false),
                participant: Some(String::new()),
                ..Default::default()
            }),
            message: MessageField::some(wa::Message::text("x")),
            ..Default::default()
        };
        assert!(from_web_message_info(&wmi, "g@g.us", "me@s.whatsapp.net", &|j| j.into()).is_none());
    }

    #[test]
    fn unicode_text_survives_store_roundtrip() {
        let samples = [
            "سلام دنیا، حالت چطوره؟",
            "مرحبا بالعالم \u{202B}RTL\u{202C} mixed English",
            "👨‍👩‍👧‍👦 🏳️‍🌈 👩🏽‍💻 🇮🇷",
            "a\0b\0\0c",
            "\0",
            "e\u{301} Å \u{200F}\u{200E}",
        ];
        let store = Store::open_in_memory().expect("store");
        let chat = "1@s.whatsapp.net";
        for (i, s) in samples.iter().enumerate() {
            let m = wa::Message::text(*s);
            let nm = from_inbound(&m, &info_with(&format!("U{i}"), 1_700_000_000 + i as i64), chat, chat)
                .expect("converted");
            assert_eq!(nm.text.as_deref(), Some(*s));
            assert!(store.insert_message(&nm, false).expect("insert"));
        }
        let rows = store.messages_page(chat, None, 50).expect("page");
        assert_eq!(rows.len(), samples.len());
        for (row, s) in rows.iter().zip(samples.iter()) {
            assert_eq!(row.text.as_deref(), Some(*s), "text changed in roundtrip");
        }
        let raw = store.message_raw(chat, "U3").expect("raw").expect("some");
        let back = wa::Message::decode_from_slice(&raw).expect("decode");
        assert_eq!(back.conversation.as_deref(), Some("a\0b\0\0c"));
    }

    fn poll_message(name: &str, options: &[&str], selectable: u32) -> wa::message::PollCreationMessage {
        wa::message::PollCreationMessage {
            name: Some(name.into()),
            options: options
                .iter()
                .map(|o| wa::message::poll_creation_message::Option {
                    option_name: Some((*o).into()),
                    ..Default::default()
                })
                .collect(),
            selectable_options_count: Some(selectable),
            ..Default::default()
        }
    }

    #[test]
    fn poll_creation_of_any_version_is_displayed_and_described() {
        let poll = poll_message("Lunch?", &["Pizza", "Sushi", ""], 0);
        for msg in [
            wa::Message {
                poll_creation_message: MessageField::some(poll.clone()),
                ..Default::default()
            },
            wa::Message {
                poll_creation_message_v2: MessageField::some(poll.clone()),
                ..Default::default()
            },
            wa::Message {
                poll_creation_message_v3: MessageField::some(poll.clone()),
                ..Default::default()
            },
        ] {
            let nm = convert(&msg).expect("converted");
            assert_eq!(nm.kind, MessageKind::Other);
            assert_eq!(nm.text.as_deref(), Some("📊 Lunch?"));
            let info = poll_info_of_stored(&nm).expect("poll info");
            assert_eq!(info.question, "Lunch?");
            assert_eq!(info.options, ["Pizza", "Sushi"], "empty option names are dropped");
            assert_eq!(info.selectable_count, 2, "0 means any number of options");
        }
        let single = wa::Message {
            poll_creation_message: MessageField::some(poll_message("Q", &["a", "b", "c"], 1)),
            ..Default::default()
        };
        assert_eq!(poll_info(&single).expect("info").selectable_count, 1);
        assert!(poll_info_of_stored(&convert(&wa::Message::text("📊 not a poll")).expect("text")).is_none());
    }

    #[test]
    fn message_secret_is_read_from_the_context_info() {
        let mut msg = wa::Message::text("x");
        assert!(message_secret(&msg).is_none());
        msg.message_context_info = MessageField::some(wa::MessageContextInfo {
            message_secret: Some(vec![7; 32]),
            ..Default::default()
        });
        assert_eq!(message_secret(&msg), Some(vec![7; 32]));
    }

    #[test]
    fn vote_hashes_map_back_to_option_names() {
        let options = vec!["Pizza".to_string(), "Sushi".to_string(), "Tacos".to_string()];
        let hash = |n: &str| whatsapp_rust::wacore::poll::compute_option_hash(n).to_vec();
        let names = option_names_for_hashes(&options, &[hash("Tacos"), hash("Pizza"), vec![1, 2, 3]]);
        assert_eq!(names, ["Pizza", "Tacos"]);
        assert!(option_names_for_hashes(&options, &[]).is_empty());
    }

    #[test]
    fn reaction_message_is_parsed_and_not_stored_as_a_message() {
        let msg = wa::Message {
            reaction_message: MessageField::some(wa::message::ReactionMessage {
                key: MessageField::some(wa::MessageKey {
                    id: Some("TARGET".into()),
                    ..Default::default()
                }),
                text: Some("👍".into()),
                sender_timestamp_ms: Some(1_700_000_123_456),
                ..Default::default()
            }),
            ..Default::default()
        };
        let r = reaction(&msg).expect("reaction");
        assert_eq!(r.target_id, "TARGET");
        assert_eq!(r.emoji, "👍");
        assert_eq!(r.ts_ms, Some(1_700_000_123_456));
        assert!(convert(&msg).is_none());

        let removal = wa::Message {
            reaction_message: MessageField::some(wa::message::ReactionMessage {
                key: MessageField::some(wa::MessageKey {
                    id: Some("TARGET".into()),
                    ..Default::default()
                }),
                text: None,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(reaction(&removal).expect("reaction").emoji, "");
        assert!(reaction(&wa::Message::text("hi")).is_none());
    }

    #[test]
    fn poll_update_message_yields_the_encrypted_vote() {
        let msg = wa::Message {
            poll_update_message: MessageField::some(wa::message::PollUpdateMessage {
                poll_creation_message_key: MessageField::some(wa::MessageKey {
                    id: Some("POLL".into()),
                    ..Default::default()
                }),
                vote: MessageField::some(wa::message::PollEncValue {
                    enc_payload: Some(vec![1, 2]),
                    enc_iv: Some(vec![3]),
                }),
                sender_timestamp_ms: Some(5000),
                ..Default::default()
            }),
            ..Default::default()
        };
        let v = poll_vote(&msg).expect("vote");
        assert_eq!(v.poll_id, "POLL");
        assert_eq!((v.enc_payload, v.enc_iv, v.ts_ms), (vec![1, 2], vec![3], Some(5000)));
        assert!(convert(&msg).is_none());
        assert!(poll_vote(&wa::Message::text("hi")).is_none());
    }

    #[test]
    fn history_reactions_are_attributed_to_their_reactor() {
        let reaction_of = |from_me: bool, participant: Option<&str>, text: &str| wa::Reaction {
            key: MessageField::some(wa::MessageKey {
                from_me: Some(from_me),
                participant: participant.map(Into::into),
                ..Default::default()
            }),
            text: Some(text.into()),
            sender_timestamp_ms: Some(2000),
            ..Default::default()
        };
        let wmi = wa::WebMessageInfo {
            key: MessageField::some(wa::MessageKey {
                id: Some("T1".into()),
                ..Default::default()
            }),
            reactions: vec![
                reaction_of(true, None, "❤"),
                reaction_of(false, Some("5@lid"), "👍"),
                reaction_of(false, None, "unattributable"),
            ],
            ..Default::default()
        };
        let resolve = |j: &str| if j == "5@lid" { "55@s.whatsapp.net".to_string() } else { j.to_string() };
        let group = web_reactions(&wmi, "1-2@g.us", "me@s.whatsapp.net", &resolve);
        let got: Vec<_> = group.iter().map(|(a, m, r)| (a.as_str(), *m, r.emoji.as_str())).collect();
        assert_eq!(got, [("me@s.whatsapp.net", true, "❤"), ("55@s.whatsapp.net", false, "👍")]);
        assert!(group.iter().all(|(_, _, r)| r.target_id == "T1" && r.ts_ms == Some(2000)));

        let dm = web_reactions(&wmi, "9@s.whatsapp.net", "me@s.whatsapp.net", &resolve);
        assert_eq!(dm.len(), 3);
        assert_eq!(dm[2].0, "9@s.whatsapp.net", "a DM reactor is the chat itself");
    }

    #[test]
    fn history_poll_votes_carry_hashes_and_voters() {
        let wmi = wa::WebMessageInfo {
            poll_updates: vec![wa::PollUpdate {
                poll_update_message_key: MessageField::some(wa::MessageKey {
                    from_me: Some(false),
                    participant: Some("7@s.whatsapp.net".into()),
                    ..Default::default()
                }),
                vote: MessageField::some(wa::message::PollVoteMessage {
                    selected_options: vec![vec![9; 32]],
                }),
                sender_timestamp_ms: Some(42_000),
                ..Default::default()
            }],
            ..Default::default()
        };
        let votes = web_poll_votes(&wmi, "1-2@g.us", "me@s.whatsapp.net", &|j| j.to_string());
        assert_eq!(votes, [("7@s.whatsapp.net".to_string(), false, vec![vec![9; 32]], 42)]);
    }
}
