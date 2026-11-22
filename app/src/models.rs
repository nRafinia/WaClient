//! Store rows -> Slint structs, plus formatting helpers.

use chrono::{DateTime, Datelike, Local, TimeZone};
use slint::{Color, Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, StyledText, VecModel};
use wa_store::{ChatKind, ChatRow, MessageKind, MessageRow, MessageStatus};

use crate::audio::PlayState;
use crate::emoji::ReactionChip;
use crate::status_view::{PollViewModel, QuizMark, QuizReveal, quiz_reveal};
use crate::{ChatItem, ContactEntry, MsgItem, PollOptionItem, ReactionChipItem};

const PALETTE: [(u8, u8, u8); 8] = [
    (0x00, 0xa8, 0x84),
    (0x53, 0xbd, 0xeb),
    (0xe5, 0x6b, 0x6f),
    (0xf5, 0xa6, 0x23),
    (0xa5, 0x7b, 0xe6),
    (0x4c, 0xc2, 0x8a),
    (0xe6, 0x7e, 0xb8),
    (0x6a, 0x8f, 0xf0),
];

fn local(ts: i64) -> Option<DateTime<Local>> {
    Local.timestamp_opt(ts, 0).single()
}

/// Chat-list style timestamp: "HH:MM", "Yesterday", weekday, "DD/MM/YYYY".
pub fn format_chat_time(ts: i64, now: DateTime<Local>) -> String {
    let Some(t) = local(ts) else {
        return String::new();
    };
    let days = (now.date_naive() - t.date_naive()).num_days();
    match days {
        d if d <= 0 => t.format("%H:%M").to_string(),
        1 => "Yesterday".to_string(),
        2..=6 => t.format("%A").to_string(),
        _ => t.format("%d/%m/%Y").to_string(),
    }
}

/// Header subtitle for a contact that is offline: "last seen today at HH:MM",
/// "last seen yesterday at HH:MM" or "last seen DD/MM/YYYY".
pub fn format_last_seen(ts: i64, now: DateTime<Local>) -> String {
    let Some(t) = local(ts) else {
        return String::new();
    };
    let days = (now.date_naive() - t.date_naive()).num_days();
    match days {
        d if d <= 0 => format!("last seen today at {}", t.format("%H:%M")),
        1 => format!("last seen yesterday at {}", t.format("%H:%M")),
        _ => format!("last seen {}", t.format("%d/%m/%Y")),
    }
}

fn format_clock(ts: i64) -> String {
    local(ts)
        .map(|t| t.format("%H:%M").to_string())
        .unwrap_or_default()
}

fn day_key(ts: i64) -> Option<(i32, u32)> {
    local(ts).map(|t| (t.year(), t.ordinal()))
}

fn separator_text(ts: i64, now: DateTime<Local>) -> String {
    let Some(t) = local(ts) else {
        return String::new();
    };
    let days = (now.date_naive() - t.date_naive()).num_days();
    match days {
        d if d <= 0 => "Today".to_string(),
        1 => "Yesterday".to_string(),
        2..=6 => t.format("%A").to_string(),
        _ => t.format("%d/%m/%Y").to_string(),
    }
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn initials(name: &str) -> String {
    let mut out = String::new();
    for word in name
        .split_whitespace()
        .filter(|w| w.chars().next().is_some_and(char::is_alphanumeric))
        .take(2)
    {
        if let Some(c) = word.chars().next() {
            out.extend(c.to_uppercase());
        }
    }
    if out.is_empty() {
        out.push('#');
    }
    out
}

pub fn tint_for(key: &str) -> Color {
    let h = key
        .bytes()
        .fold(5381u32, |h, b| h.wrapping_mul(33).wrapping_add(b as u32));
    let (r, g, b) = PALETTE[(h as usize) % PALETTE.len()];
    Color::from_rgb_u8(r, g, b)
}

pub fn chat_item(row: &ChatRow, avatar: Option<Image>, now: DateTime<Local>) -> ChatItem {
    ChatItem {
        jid: row.jid.as_str().into(),
        name: row.display_name.as_str().into(),
        preview: one_line(&crate::format::strip(row.last_preview.as_deref().unwrap_or(""))).into(),
        time: if row.last_ts > 0 {
            format_chat_time(row.last_ts, now).into()
        } else {
            SharedString::default()
        },
        unread: row.unread.clamp(0, i32::MAX as i64) as i32,
        pinned: row.pinned,
        archived: row.archived,
        muted: is_muted_until(row.muted_until, now.timestamp()),
        receipts_off: row.receipts_off,
        disappearing: row.ephemeral_secs > 0,
        is_dm: ChatKind::from_jid(&row.jid) == ChatKind::Dm,
        blocked: false,
        saved: false,
        draft: false,
        has_avatar: avatar.is_some(),
        avatar: avatar.unwrap_or_default(),
        initials: initials(&row.display_name).into(),
        tint: tint_for(&row.jid),
    }
}

/// `muted_until` is a unix timestamp; `i64::MAX` and negative values (the
/// legacy "forever") mean muted indefinitely.
pub fn is_muted_until(muted_until: i64, now: i64) -> bool {
    muted_until < 0 || muted_until > now
}

/// Mute durations of the chat menu: 1 eight hours, 2 one week, 3 always;
/// anything else unmutes.
pub fn mute_until(choice: i32, now: i64) -> Option<i64> {
    match choice {
        1 => Some(now.saturating_add(8 * 3600)),
        2 => Some(now.saturating_add(7 * 86_400)),
        3 => Some(i64::MAX),
        _ => None,
    }
}

/// A chat address for logs: only the last two characters of the user part.
pub fn redact_jid(jid: &str) -> String {
    let user = jid.split('@').next().unwrap_or(jid);
    let tail: Vec<char> = user.chars().rev().take(2).collect();
    let tail: String = tail.into_iter().rev().collect();
    format!("…{tail}")
}

/// One person of a shared-contact message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedContact {
    pub name: String,
    /// As shown, "+<digits>"; empty when the card carries no number.
    pub phone: String,
    /// Digits only: the user part of the contact's chat address.
    pub digits: String,
}

/// Whether `m` is a shared-contact card (the engine stores vCards as kind
/// `Other` with this MIME type).
pub fn is_contact_card(m: &MessageRow) -> bool {
    !m.revoked && m.kind == MessageKind::Other && m.media_mime.as_deref() == Some("text/vcard")
}

/// A message cannot make the card list grow without bound.
const MAX_SHARED_CONTACTS: usize = 50;
/// E.164 numbers have at most 15 digits.
const MAX_PHONE_DIGITS: usize = 15;

/// "+49 151 1234", "0151-123456"; not "12 Main St".
fn is_phone_line(line: &str) -> bool {
    line.starts_with('+')
        || (line.chars().any(|c| c.is_ascii_digit())
            && line.chars().all(|c| c.is_ascii_digit() || matches!(c, ' ' | '-' | '(' | ')' | '.')))
}

/// Parses the engine's text for shared contacts: per contact a "👤 <name>"
/// line followed by "+<digits>" lines, contacts separated by blank lines. Only
/// the first number of a contact is kept; a contact without a name falls back
/// to its number.
pub fn parse_shared_contacts(text: &str) -> Vec<SharedContact> {
    fn finish(out: &mut Vec<SharedContact>, cur: Option<SharedContact>) {
        if out.len() >= MAX_SHARED_CONTACTS {
            return;
        }
        if let Some(mut c) = cur {
            if c.name.is_empty() {
                c.name = c.phone.clone();
            }
            if !c.name.is_empty() {
                out.push(c);
            }
        }
    }
    let empty = || SharedContact { name: String::new(), phone: String::new(), digits: String::new() };
    let mut out = Vec::new();
    let mut cur: Option<SharedContact> = None;
    for line in text.lines().map(str::trim) {
        if line.is_empty() {
            finish(&mut out, cur.take());
        } else if let Some(name) = line.strip_prefix('\u{1F464}') {
            finish(&mut out, cur.take());
            cur = Some(SharedContact { name: name.trim().to_string(), ..empty() });
        } else if is_phone_line(line) {
            let digits: String = line.chars().filter(char::is_ascii_digit).take(MAX_PHONE_DIGITS).collect();
            let c = cur.get_or_insert_with(empty);
            if c.digits.is_empty() && !digits.is_empty() {
                c.phone = format!("+{digits}");
                c.digits = digits;
            }
        }
    }
    finish(&mut out, cur);
    out
}

/// The chat address a shared contact opens; `None` for implausible numbers.
pub fn contact_chat_jid(digits: &str) -> Option<String> {
    let ok = (5..=15).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit());
    ok.then(|| format!("{digits}@s.whatsapp.net"))
}

pub fn is_media(kind: MessageKind) -> bool {
    !matches!(kind, MessageKind::Text | MessageKind::Other)
}

pub fn kind_label(m: &MessageRow) -> String {
    match m.kind {
        MessageKind::Text => String::new(),
        MessageKind::Image => "📷 Photo".into(),
        MessageKind::Video => "🎥 Video".into(),
        MessageKind::Audio => "🎵 Audio".into(),
        MessageKind::Voice => "🎤 Voice message".into(),
        MessageKind::Document => format!(
            "📄 {}",
            m.media_name.as_deref().filter(|n| !n.is_empty()).unwrap_or("Document")
        ),
        MessageKind::Sticker => "Sticker".into(),
        MessageKind::Other => "Unsupported message".into(),
    }
}

/// Short text used for the reply bar.
pub fn reply_preview(m: &MessageRow) -> String {
    match m.text.as_deref().filter(|t| !t.is_empty()) {
        Some(t) => one_line(&crate::format::strip(t)),
        None => kind_label(m),
    }
}

fn format_size(bytes: i64) -> String {
    let b = bytes.max(0) as f64;
    if b >= 1024.0 * 1024.0 {
        format!("{:.1} MB", b / (1024.0 * 1024.0))
    } else if b >= 1024.0 {
        format!("{:.0} KB", b / 1024.0)
    } else {
        format!("{} B", bytes.max(0))
    }
}

fn status_code(m: &MessageRow) -> i32 {
    if !m.from_me {
        return 0;
    }
    match m.status {
        MessageStatus::Pending => 1,
        MessageStatus::Sent => 2,
        MessageStatus::Delivered => 3,
        MessageStatus::Read | MessageStatus::Played => 4,
        MessageStatus::Failed => 5,
    }
}

pub fn decode_image_bytes(bytes: &[u8]) -> Option<Image> {
    crate::thumbs::decode_thumbnail(bytes).map(Image::from_rgba8)
}


/// Visuals resolved by the caller (they come from caches).
pub struct MsgImages {
    pub thumb: Option<Image>,
    pub full: Option<Image>,
    /// Preview image of a link card.
    pub link_thumb: Option<Image>,
}

/// `https://www.example.com:8080/a?b` -> `example.com:8080`, for the link card.
pub fn link_host(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = rest.split(['/', '\\', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or(authority);
    host.strip_prefix("www.").unwrap_or(host).to_string()
}

/// Playback state of the one voice note that is currently loaded.
pub struct PlayView {
    pub id: String,
    pub state: PlayState,
    pub pos: f32,
    pub dur: f32,
}

/// Per-row UI state that does not live in the store.
pub struct RowUi<'a> {
    pub downloading: bool,
    pub failed: bool,
    pub play: Option<&'a PlayView>,
    pub poll: Option<&'a PollViewModel>,
    pub reactions: &'a [ReactionChip],
}

/// Poll messages are stored as `Other` messages whose text is "📊 <question>".
pub fn is_poll_row(m: &MessageRow) -> bool {
    m.kind == MessageKind::Other && m.text.as_deref().is_some_and(|t| t.starts_with("📊 "))
}

fn poll_options(poll: Option<&PollViewModel>, reveal: Option<&QuizReveal>) -> ModelRc<PollOptionItem> {
    let items: Vec<PollOptionItem> = poll
        .map(|p| {
            p.options
                .iter()
                .enumerate()
                .map(|(i, o)| PollOptionItem {
                    mark: match reveal.and_then(|r| r.marks.get(i)) {
                        Some(QuizMark::Correct) => 1,
                        Some(QuizMark::Wrong) => 2,
                        _ => 0,
                    },
                    name: o.name.as_str().into(),
                    votes: i32::try_from(o.votes).unwrap_or(i32::MAX),
                    percent: i32::try_from(o.percent).unwrap_or(100),
                    mine: o.mine,
                })
                .collect()
        })
        .unwrap_or_default();
    ModelRc::new(VecModel::from(items))
}

fn reaction_chips(chips: &[ReactionChip]) -> ModelRc<ReactionChipItem> {
    let items: Vec<ReactionChipItem> = chips
        .iter()
        .map(|c| ReactionChipItem {
            emoji: c.emoji.as_str().into(),
            count: i32::try_from(c.count).unwrap_or(i32::MAX),
            mine: c.mine,
        })
        .collect();
    ModelRc::new(VecModel::from(items))
}

/// How a received file may be handled from a chat bubble.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenPolicy {
    /// Hand to the default app.
    Open,
    /// Only offer "Show in folder".
    RevealOnly,
}

/// Extensions that are opened directly. Anything not listed is reveal-only:
/// file names come from the remote sender, so this is an allowlist. Script
/// capable formats (svg, html, macro Office) are deliberately absent.
const OPEN_EXTS: &[&str] = &[
    "pdf", "txt", "jpg", "jpeg", "png", "gif", "webp", "bmp", "heic", "mp4", "mov", "m4v", "webm",
    "mkv", "avi", "mp3", "m4a", "aac", "ogg", "opus", "wav", "flac", "doc", "docx", "xls", "xlsx",
    "ppt", "pptx", "odt", "ods", "odp", "rtf", "csv", "zip", "7z", "rar",
];

pub fn open_policy(path: &str) -> OpenPolicy {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    // ':' in a file name means an NTFS alternate data stream.
    if name.contains(':') {
        return OpenPolicy::RevealOnly;
    }
    // Windows ignores trailing dots and spaces in file names.
    let name = name.trim_end_matches(['.', ' ']);
    match name.rsplit_once('.') {
        Some((_, ext)) if OPEN_EXTS.iter().any(|a| ext.eq_ignore_ascii_case(a)) => OpenPolicy::Open,
        _ => OpenPolicy::RevealOnly,
    }
}

pub fn fmt_secs(secs: f32) -> String {
    let total = secs.max(0.0).round() as u64;
    format!("{}:{:02}", total / 60, total % 60)
}

fn media_kind(kind: MessageKind) -> i32 {
    match kind {
        MessageKind::Image => 1,
        MessageKind::Sticker => 2,
        MessageKind::Video => 3,
        MessageKind::Document => 4,
        MessageKind::Voice | MessageKind::Audio => 5,
        MessageKind::Text | MessageKind::Other => 0,
    }
}

fn file_ext(m: &MessageRow) -> String {
    let from_name = m
        .media_name
        .as_deref()
        .and_then(|n| n.rsplit_once('.'))
        .map(|(_, e)| e)
        .filter(|e| !e.is_empty() && e.len() <= 5);
    let from_mime = m
        .media_mime
        .as_deref()
        .and_then(|t| t.split(';').next())
        .and_then(|t| t.rsplit_once('/'))
        .map(|(_, sub)| sub)
        .filter(|e| !e.is_empty() && e.len() <= 5);
    from_name.or(from_mime).unwrap_or("file").to_uppercase()
}

/// (play state 0 idle / 1 playing / 2 paused, progress 0..1, text).
pub fn voice_view(m: &MessageRow, play: Option<&PlayView>) -> (i32, f32, String) {
    let known = m.media_duration.filter(|d| *d > 0).map(|d| d as f32);
    match play.filter(|p| p.id == m.id) {
        Some(p) => {
            let dur = known.unwrap_or(p.dur);
            let progress = if dur > 0.0 { (p.pos / dur).clamp(0.0, 1.0) } else { 0.0 };
            let state = if p.state == PlayState::Playing { 1 } else { 2 };
            (state, progress, format!("{} / {}", fmt_secs(p.pos), fmt_secs(dur)))
        }
        None => (0, 0.0, known.map(fmt_secs).unwrap_or_default()),
    }
}

pub fn msg_item(m: &MessageRow, is_group: bool, images: MsgImages, ui: RowUi<'_>) -> MsgItem {
    // A deleted message keeps its kind but has no content to show.
    let media = is_media(m.kind) && !m.revoked;
    let kind = if m.revoked { 0 } else { media_kind(m.kind) };
    let detail = if kind == 5 {
        String::new()
    } else if media {
        let mut parts = Vec::new();
        if m.kind == MessageKind::Document {
            parts.push(file_ext(m));
        } else if let Some(n) = m.media_name.as_deref().filter(|n| !n.is_empty()) {
            parts.push(n.to_string());
        }
        if let Some(s) = m.media_size.filter(|s| *s > 0) {
            parts.push(format_size(s));
        }
        parts.join(" · ")
    } else {
        String::new()
    };
    let sender = m
        .sender_name
        .clone()
        .unwrap_or_else(|| m.sender_jid.split('@').next().unwrap_or("").to_string());
    let (play_state, play_progress, play_text) = voice_view(m, ui.play);
    let contacts = if is_contact_card(m) { parse_shared_contacts(m.text.as_deref().unwrap_or("")) } else { Vec::new() };
    let is_contact = !contacts.is_empty();
    let styled = if m.revoked || ui.poll.is_some() || is_contact {
        None
    } else {
        m.text.as_deref().and_then(crate::format::styled_text)
    };
    let has_links = styled.is_some();
    let styled = styled.unwrap_or_default();
    let link_url = m.link_url.as_deref().filter(|u| !m.revoked && crate::links::is_safe_link(u));
    // Replies, media, polls, contacts, locations and link cards keep the normal bubble.
    let big_emoji = if media
        || m.revoked
        || ui.poll.is_some()
        || is_contact
        || link_url.is_some()
        || m.geo_lat.is_some()
        || m.quoted_preview.as_deref().is_some_and(|q| !q.is_empty())
        || is_notice(&m.id)
    {
        None
    } else {
        m.text.as_deref().and_then(crate::emoji::emoji_only_count)
    };
    let reveal = ui.poll.and_then(|p| quiz_reveal(p, m.from_me));
    MsgItem {
        id: m.id.as_str().into(),
        is_sep: false,
        sep_text: SharedString::default(),
        mine: m.from_me,
        show_sender: is_group && !m.from_me,
        sender_color: tint_for(&m.sender_jid),
        sender: sender.into(),
        text: m.text.as_deref().unwrap_or("").into(),
        quoted: one_line(&crate::format::strip(m.quoted_preview.as_deref().unwrap_or(""))).into(),
        time: format_clock(m.ts).into(),
        status: status_code(m),
        media_label: if m.revoked || ui.poll.is_some() || is_contact { String::new() } else { kind_label(m) }.into(),
        media_detail: detail.into(),
        has_thumb: images.thumb.is_some(),
        thumb: images.thumb.unwrap_or_default(),
        has_full: images.full.is_some(),
        full: images.full.unwrap_or_default(),
        can_download: media && m.local_path.is_none(),
        downloading: ui.downloading,
        media_kind: kind,
        has_local: m.local_path.is_some(),
        failed: ui.failed,
        unsafe_file: m.local_path.as_deref().is_some_and(|p| open_policy(p) == OpenPolicy::RevealOnly),
        file_ext: file_ext(m).into(),
        file_name: m
            .media_name
            .as_deref()
            .filter(|n| !n.is_empty())
            .unwrap_or("Document")
            .into(),
        play_state,
        play_progress,
        play_text: play_text.into(),
        revoked: m.revoked,
        edited: m.edited && !m.revoked,
        poll: ui.poll.is_some() && !m.revoked,
        poll_question: ui.poll.map(|p| p.question.as_str()).unwrap_or_default().into(),
        poll_options: poll_options(ui.poll, reveal.as_ref()),
        poll_total: ui.poll.map_or(0, |p| i32::try_from(p.total_votes).unwrap_or(i32::MAX)),
        poll_multi: ui.poll.is_some_and(|p| p.multi),
        poll_quiz: ui.poll.is_some_and(|p| p.correct.is_some()),
        poll_correct: reveal.map(|r| format!("Correct answer: {}", r.correct_name)).unwrap_or_default().into(),
        reactions: reaction_chips(if m.revoked { &[] } else { ui.reactions }),
        styled,
        has_links,
        big_emoji: big_emoji.map_or(0, i32::from),
        can_copy: !m.revoked && ui.poll.is_none() && m.text.as_deref().is_some_and(|t| !t.trim().is_empty()),
        can_delete_everyone: can_delete_for_everyone(m, chrono::Utc::now().timestamp()),
        starred: m.starred,
        mentions_me: m.mentions_me,
        selected: false,
        pinned: false,
        can_edit: can_edit(m, chrono::Utc::now().timestamp()),
        can_share: can_share(m),
        geo: m.geo_lat.zip(m.geo_lon).filter(|_| !m.revoked).map(|(la, lo)| crate::geo::coords_text(la, lo)).unwrap_or_default().into(),
        expires: m.expires_at.is_some(),
        is_notice: is_notice(&m.id),
        can_translate: can_translate(m, ui.poll.is_some() || is_contact),
        tr_state: 0,
        tr_text: SharedString::default(),
        tr_note: SharedString::default(),
        contact: is_contact,
        contact_name: contacts.first().map(|c| c.name.as_str()).unwrap_or_default().into(),
        contact_phone: contacts.first().map(|c| c.phone.as_str()).unwrap_or_default().into(),
        contact_digits: contacts.first().map(|c| c.digits.as_str()).unwrap_or_default().into(),
        contact_initials: contacts.first().map(|c| initials(&c.name)).unwrap_or_default().into(),
        contact_tint: tint_for(contacts.first().map_or("", |c| c.name.as_str())),
        contact_count: i32::try_from(contacts.len()).unwrap_or(i32::MAX),
        has_link_card: link_url.is_some(),
        link_url: link_url.unwrap_or_default().into(),
        link_title: if link_url.is_some() { one_line(m.link_title.as_deref().unwrap_or("")) } else { String::new() }.into(),
        link_desc: if link_url.is_some() { one_line(m.link_desc.as_deref().unwrap_or("")) } else { String::new() }.into(),
        link_host: link_url.map(link_host).unwrap_or_default().into(),
        link_has_thumb: link_url.is_some() && images.link_thumb.is_some(),
        link_thumb: images.link_thumb.unwrap_or_default(),
        contacts: ModelRc::new(VecModel::from(
            contacts
                .iter()
                .map(|c| ContactEntry {
                    name: c.name.as_str().into(),
                    phone: c.phone.as_str().into(),
                    digits: c.digits.as_str().into(),
                    initials: initials(&c.name).into(),
                    tint: tint_for(&c.name),
                })
                .collect::<Vec<_>>(),
        )),
    }
}

/// WhatsApp lets the sender revoke a message for everyone for a limited time.
const REVOKE_WINDOW_SECS: i64 = 60 * 60 * 60;

/// Own, not yet deleted and younger than 60 hours.
pub fn can_delete_for_everyone(m: &MessageRow, now: i64) -> bool {
    m.from_me && !m.revoked && now.saturating_sub(m.ts) < REVOKE_WINDOW_SECS
}

/// WhatsApp accepts an edit for about 15 minutes after sending.
pub const EDIT_WINDOW_SECS: i64 = 15 * 60;

/// Own, sent, not deleted, plain text and younger than the edit window.
pub fn can_edit(m: &MessageRow, now: i64) -> bool {
    m.from_me
        && !m.revoked
        && m.kind == MessageKind::Text
        && m.text.as_deref().is_some_and(|t| !t.trim().is_empty())
        && !matches!(m.status, MessageStatus::Pending | MessageStatus::Failed)
        && now.saturating_sub(m.ts) < EDIT_WINDOW_SECS
}

/// Text messages and media captions can be translated; polls, shared contacts,
/// locations and system notices cannot (`special`: poll or contact card).
pub fn can_translate(m: &MessageRow, special: bool) -> bool {
    !m.revoked
        && !special
        && !is_notice(&m.id)
        && m.geo_lat.is_none()
        && m.text.as_deref().is_some_and(|t| !t.trim().is_empty())
}

/// Text and media can be forwarded; polls and other system rows cannot.
pub fn can_share(m: &MessageRow) -> bool {
    !m.revoked && m.kind != MessageKind::Other
}

/// Rows the engine writes for "disappearing messages turned on/off" carry an
/// `EPH-` id prefix, group changes ("Ali added Sara") a `GRP-` prefix, missed
/// calls a `CALL-` prefix; they are shown as centred notices, not bubbles.
pub fn is_notice(id: &str) -> bool {
    id.starts_with("EPH-") || id.starts_with("GRP-") || id.starts_with("CALL-")
}

pub fn separator_item(ts: i64, now: DateTime<Local>) -> MsgItem {
    MsgItem {
        id: SharedString::default(),
        is_sep: true,
        sep_text: separator_text(ts, now).into(),
        mine: false,
        show_sender: false,
        sender: SharedString::default(),
        sender_color: Color::default(),
        text: SharedString::default(),
        quoted: SharedString::default(),
        time: SharedString::default(),
        status: 0,
        media_label: SharedString::default(),
        media_detail: SharedString::default(),
        thumb: Image::default(),
        has_thumb: false,
        full: Image::default(),
        has_full: false,
        can_download: false,
        downloading: false,
        media_kind: 0,
        has_local: false,
        failed: false,
        unsafe_file: false,
        file_ext: SharedString::default(),
        file_name: SharedString::default(),
        play_state: 0,
        play_progress: 0.0,
        play_text: SharedString::default(),
        revoked: false,
        edited: false,
        poll: false,
        poll_question: SharedString::default(),
        poll_options: ModelRc::default(),
        poll_total: 0,
        poll_multi: false,
        poll_quiz: false,
        poll_correct: SharedString::default(),
        reactions: ModelRc::default(),
        styled: StyledText::default(),
        has_links: false,
        big_emoji: 0,
        can_copy: false,
        can_delete_everyone: false,
        contact: false,
        contact_name: SharedString::default(),
        contact_phone: SharedString::default(),
        contact_digits: SharedString::default(),
        contact_initials: SharedString::default(),
        contact_tint: Color::default(),
        contact_count: 0,
        contacts: ModelRc::default(),
        has_link_card: false,
        link_url: SharedString::default(),
        link_title: SharedString::default(),
        link_desc: SharedString::default(),
        link_host: SharedString::default(),
        link_thumb: Image::default(),
        link_has_thumb: false,
        starred: false,
        mentions_me: false,
        selected: false,
        pinned: false,
        can_edit: false,
        can_share: false,
        geo: SharedString::default(),
        expires: false,
        is_notice: false,
        can_translate: false,
        tr_state: 0,
        tr_text: SharedString::default(),
        tr_note: SharedString::default(),
    }
}

/// Longest side accepted when decoding a file, and the decoder's allocation cap.
const MAX_DECODE_SIDE: u32 = 8192;
const MAX_DECODE_ALLOC: u64 = 128 * 1024 * 1024;

/// Decodes an image file (meant for a worker thread); large images are
/// shrunk so the cache stays small.
pub fn decode_file_scaled(
    path: &std::path::Path,
    max_dim: u32,
) -> Result<SharedPixelBuffer<Rgba8Pixel>, String> {
    let open = || {
        image::ImageReader::open(path)
            .map_err(|e| e.to_string())?
            .with_guessed_format()
            .map_err(|e| e.to_string())
    };
    let (w, h) = open()?.into_dimensions().map_err(|e| e.to_string())?;
    if w == 0 || h == 0 || w > MAX_DECODE_SIDE || h > MAX_DECODE_SIDE {
        return Err(format!("image dimensions {w}x{h} out of range"));
    }
    let mut reader = open()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DECODE_SIDE);
    limits.max_image_height = Some(MAX_DECODE_SIDE);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);
    let img = reader.decode().map_err(|e| e.to_string())?;
    let img = if img.width() > max_dim || img.height() > max_dim {
        img.thumbnail(max_dim, max_dim)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Ok(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(rgba.as_raw(), w, h))
}

/// True when `a` and `b` fall on different local calendar days.
pub fn new_day(prev_ts: Option<i64>, ts: i64) -> bool {
    match prev_ts {
        None => true,
        Some(p) => day_key(p) != day_key(ts),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notice_rows_are_told_by_id_prefix() {
        assert!(is_notice("EPH-1700000000"));
        assert!(!is_notice("3EB0ABCD"));
        assert!(!is_notice("eph-1"));
        assert!(!is_notice(""));
    }

    #[test]
    fn last_seen_text_by_day() {
        let at = |d, h, m| Local.with_ymd_and_hms(2026, 3, d, h, m, 0).unwrap();
        let now = at(10, 15, 0);
        assert_eq!(format_last_seen(at(10, 9, 5).timestamp(), now), "last seen today at 09:05");
        assert_eq!(format_last_seen(at(9, 23, 59).timestamp(), now), "last seen yesterday at 23:59");
        assert_eq!(format_last_seen(at(1, 8, 0).timestamp(), now), "last seen 01/03/2026");
    }

    #[test]
    fn link_host_treats_backslash_as_a_path_separator() {
        assert_eq!(link_host("https://good.com\\@evil.com/"), "good.com");
        assert_eq!(link_host("https://user:pw@www.example.com:8080/a?b"), "example.com:8080");
        assert_eq!(link_host("https://good.com/x@evil.com"), "good.com");
    }

    #[test]
    fn shared_contacts_single_and_multiple() {
        let one = parse_shared_contacts("👤 Alice Johnson\n+4915112345678");
        assert_eq!(
            one,
            vec![SharedContact {
                name: "Alice Johnson".into(),
                phone: "+4915112345678".into(),
                digits: "4915112345678".into()
            }]
        );
        let many = parse_shared_contacts("👤 Bob\n+111 22\n+999\n\n👤 Carol\n+333\n");
        assert_eq!(many.len(), 2);
        assert_eq!(many[0].name, "Bob");
        assert_eq!(many[0].digits, "11122");
        assert_eq!(many[1].name, "Carol");
        assert_eq!(many[1].phone, "+333");
    }

    #[test]
    fn shared_contacts_edge_cases() {
        assert!(parse_shared_contacts("").is_empty());
        assert!(parse_shared_contacts("hello there").is_empty());
        // No number: still a card; no name: the number stands in.
        let c = parse_shared_contacts("👤 Dave\n\n+555123");
        assert_eq!(c[0].name, "Dave");
        assert_eq!(c[0].digits, "");
        assert_eq!(c[1].name, "+555123");
        // Contacts without a blank line between them.
        assert_eq!(parse_shared_contacts("👤 A\n+1000\n👤 B\n+2000").len(), 2);
    }

    #[test]
    fn shared_contacts_are_bounded_and_phone_lines_strict() {
        let many: String = (0..80).map(|i| format!("👤 P{i}\n+1555{i}\n\n")).collect();
        assert_eq!(parse_shared_contacts(&many).len(), 50);
        let long = parse_shared_contacts("👤 Z\n+1234567890123456789012");
        assert_eq!(long[0].digits, "123456789012345");
        let mixed = parse_shared_contacts("👤 Y\n12 Main St\n(030) 123-4567");
        assert_eq!(mixed[0].digits, "0301234567");
        assert!(parse_shared_contacts("👤 X\n12 Main St")[0].phone.is_empty());
    }

    #[test]
    fn contact_jid_needs_a_plausible_number() {
        assert_eq!(contact_chat_jid("4915112345678").as_deref(), Some("4915112345678@s.whatsapp.net"));
        assert_eq!(contact_chat_jid(""), None);
        assert_eq!(contact_chat_jid("12"), None);
        assert_eq!(contact_chat_jid("1234567890123456"), None);
        assert_eq!(contact_chat_jid("12a45"), None);
    }

    #[test]
    fn redacted_jid_keeps_two_characters() {
        assert_eq!(redact_jid("491701234567@s.whatsapp.net"), "…67");
        assert_eq!(redact_jid("12345-678@g.us"), "…78");
        assert_eq!(redact_jid("7@s.whatsapp.net"), "…7");
        assert_eq!(redact_jid(""), "…");
    }

    #[test]
    fn open_policy_is_an_allowlist() {
        for p in [
            "C:\\x\\report.pdf",
            "photo.JPG",
            "UPPER.PDF",
            "a/b/notes.txt",
            "archive.zip",
            "sheet.xlsx",
            "clip.mp4",
            "trailing.pdf.",
            "trailing.pdf ",
        ] {
            assert_eq!(open_policy(p), OpenPolicy::Open, "{p}");
        }
        for p in [
            "x.exe::$DATA",
            "C:\\dir\\x.pdf:evil.exe",
            "x.pdf:evil.exe",
            "x.exe.",
            "x.exe ",
            "pic.svg",
            "page.html",
            "page.htm",
            "m.docm",
            "m.xlsm",
            "m.pptm",
            "x.exe_",
            "noext",
            "a.pdf.exe",
            "dir.pdf\\file",
            "",
        ] {
            assert_eq!(open_policy(p), OpenPolicy::RevealOnly, "{p}");
        }
    }

    fn row(from_me: bool, revoked: bool, ts: i64) -> MessageRow {
        MessageRow {
            chat_jid: "1@s.whatsapp.net".into(),
            id: "A".into(),
            sender_jid: "1@s.whatsapp.net".into(),
            sender_name: None,
            from_me,
            ts,
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
            status: MessageStatus::Sent,
            revoked,
            edited: false,
            ..Default::default()
        }
    }

    #[test]
    fn delete_for_everyone_needs_own_live_recent_message() {
        let now = 1_000_000_000;
        let h = 3600;
        assert!(can_delete_for_everyone(&row(true, false, now), now));
        assert!(can_delete_for_everyone(&row(true, false, now - 59 * h), now));
        assert!(!can_delete_for_everyone(&row(true, false, now - 60 * h), now));
        assert!(!can_delete_for_everyone(&row(true, false, now - 61 * h), now));
        assert!(!can_delete_for_everyone(&row(false, false, now), now));
        assert!(!can_delete_for_everyone(&row(true, true, now), now));
        // A clock that is behind the message's timestamp still counts as recent.
        assert!(can_delete_for_everyone(&row(true, false, now + h), now));
    }

    #[test]
    fn edit_needs_own_sent_live_text_within_fifteen_minutes() {
        let now = 1_000_000_000;
        assert!(can_edit(&row(true, false, now - 14 * 60), now));
        assert!(!can_edit(&row(true, false, now - 15 * 60), now));
        assert!(!can_edit(&row(false, false, now), now));
        assert!(!can_edit(&row(true, true, now), now));
        let mut r = row(true, false, now);
        r.status = MessageStatus::Failed;
        assert!(!can_edit(&r, now));
        r.status = MessageStatus::Pending;
        assert!(!can_edit(&r, now));
        r.status = MessageStatus::Read;
        assert!(can_edit(&r, now));
        r.kind = MessageKind::Image;
        assert!(!can_edit(&r, now));
        r.kind = MessageKind::Text;
        r.text = Some("   ".into());
        assert!(!can_edit(&r, now));
    }

    #[test]
    fn only_live_text_and_media_can_be_forwarded() {
        let mut r = row(false, false, 0);
        assert!(can_share(&r));
        r.kind = MessageKind::Voice;
        assert!(can_share(&r));
        r.kind = MessageKind::Other;
        assert!(!can_share(&r));
        assert!(!can_share(&row(true, true, 0)));
    }

    #[test]
    fn mute_choices_map_to_deadlines() {
        assert_eq!(mute_until(1, 1000), Some(1000 + 8 * 3600));
        assert_eq!(mute_until(2, 1000), Some(1000 + 7 * 86_400));
        assert_eq!(mute_until(3, 1000), Some(i64::MAX));
        assert_eq!(mute_until(0, 1000), None);
        assert_eq!(mute_until(9, 1000), None);
    }

    #[test]
    fn muted_covers_forever_values_and_expiry() {
        assert!(is_muted_until(i64::MAX, 1000));
        assert!(is_muted_until(-1, 1000));
        assert!(is_muted_until(1001, 1000));
        assert!(!is_muted_until(1000, 1000));
        assert!(!is_muted_until(0, 1000));
    }

    #[test]
    fn seconds_format_as_m_ss() {
        assert_eq!(fmt_secs(0.0), "0:00");
        assert_eq!(fmt_secs(75.4), "1:15");
    }
}
