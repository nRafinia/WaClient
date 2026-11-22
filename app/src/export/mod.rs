//! Export of one chat's history to HTML, plain text or JSON, optionally with
//! the cached media files. Everything stays on this PC. The store is read in
//! pages and each message is written as soon as it is built, so a long chat
//! never sits in memory. Received view-once media is never kept, so it can
//! never appear in an export; its placeholder text is exported as it is.

mod html;
mod job;
mod json;
mod plain;
mod util;

use chrono::NaiveDateTime;
use wa_store::{MessageKind, MessageRow, PollView, VIEW_ONCE_TEXT};

pub use job::{run, Job, Outcome};
pub use util::{folder_name, group_digits, parse_date, range_bounds, Range};

use crate::models::{is_contact_card, is_media, is_notice, is_poll_row, kind_label, parse_shared_contacts};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Html,
    Text,
    Json,
}

impl Format {
    /// 0 = HTML, 1 = Text, 2 = JSON, as the dialog numbers them.
    pub fn from_index(i: i32) -> Format {
        match i {
            1 => Format::Text,
            2 => Format::Json,
            _ => Format::Html,
        }
    }

    pub fn file_name(self) -> &'static str {
        match self {
            Format::Html => "index.html",
            Format::Text => "chat.txt",
            Format::Json => "chat.json",
        }
    }
}

/// Folder inside the export that holds the copied media files.
pub const MEDIA_DIR: &str = "media";

/// What the chat is called and how it was exported, for the file headers.
#[derive(Debug, Clone)]
pub struct Head {
    pub title: String,
    pub is_group: bool,
    pub exported_at: String,
    pub range_text: String,
}

/// The media of a message as the export shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaRef {
    /// "📷 Photo", or the document's name.
    pub label: String,
    /// Path relative to the export folder ("media/00001-x.jpg"); only set when
    /// the file was copied.
    pub file: Option<String>,
    /// Media was asked for but the file was never downloaded.
    pub missing: bool,
    /// Shown inline as a picture.
    pub inline: bool,
    pub mime: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PollEntry {
    pub question: String,
    pub options: Vec<(String, u32)>,
    pub multi: bool,
}

/// One message, ready to be written in any of the formats.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub id: String,
    pub when: NaiveDateTime,
    /// ISO 8601 with the local UTC offset.
    pub iso: String,
    pub sender: String,
    pub sender_jid: String,
    pub from_me: bool,
    /// The store's kind, or "poll", "contact", "location", "notice", "deleted".
    pub kind: &'static str,
    pub notice: bool,
    pub text: Option<String>,
    pub media: Option<MediaRef>,
    pub quoted_id: Option<String>,
    pub quoted_text: Option<String>,
    pub reactions: Vec<(String, u32)>,
    pub edited: bool,
    pub starred: bool,
    pub poll: Option<PollEntry>,
    pub location: Option<(f64, f64)>,
    /// `(name, phone)` of shared contacts.
    pub contacts: Vec<(String, String)>,
    /// Link preview: `(url, title)`.
    pub link: Option<(String, String)>,
}

/// What the builder needs to know about the chat.
#[derive(Debug, Clone)]
pub struct ChatCtx {
    pub name: String,
    pub is_group: bool,
}

/// Where a message's time shows up and whether its media file was copied.
pub struct Extras {
    pub when: NaiveDateTime,
    pub iso: String,
    pub reactions: Vec<(String, u32)>,
    pub poll: Option<PollView>,
    pub include_media: bool,
    /// Path relative to the export folder of the copied media file.
    pub copied: Option<String>,
}

fn sender_name(m: &MessageRow, ctx: &ChatCtx) -> String {
    if m.from_me {
        return "You".to_string();
    }
    if let Some(n) = m.sender_name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        return n.to_string();
    }
    if !ctx.is_group {
        return ctx.name.clone();
    }
    let user = m.sender_jid.split('@').next().unwrap_or_default();
    if user.is_empty() { "Unknown".to_string() } else { format!("+{user}") }
}

/// Whether the message is the placeholder of a view-once message.
pub fn is_view_once(m: &MessageRow) -> bool {
    m.text.as_deref() == Some(VIEW_ONCE_TEXT)
}

/// Builds the entry of one message. Revoked messages lose their content, and
/// the media of view-once placeholders is dropped whatever the row says.
pub fn entry_from(m: &MessageRow, ctx: &ChatCtx, x: Extras) -> Entry {
    let notice = is_notice(&m.id);
    let mut e = Entry {
        id: m.id.clone(),
        when: x.when,
        iso: x.iso,
        sender: if notice { String::new() } else { sender_name(m, ctx) },
        sender_jid: if notice { String::new() } else { m.sender_jid.clone() },
        from_me: m.from_me,
        kind: m.kind.as_str(),
        notice,
        text: m.text.clone().filter(|t| !t.is_empty()),
        media: None,
        quoted_id: m.quoted_id.clone(),
        quoted_text: m.quoted_preview.clone().filter(|t| !t.is_empty()),
        reactions: x.reactions,
        edited: m.edited,
        starred: m.starred,
        poll: None,
        location: None,
        contacts: Vec::new(),
        link: None,
    };
    if notice {
        e.kind = "notice";
        return e;
    }
    if m.revoked {
        e.kind = "deleted";
        e.text = Some("This message was deleted".to_string());
        e.quoted_id = None;
        e.quoted_text = None;
        e.reactions.clear();
        return e;
    }
    if let (Some(url), true) = (m.link_url.as_deref().filter(|u| !u.is_empty()), m.kind == MessageKind::Text) {
        e.link = Some((url.to_string(), m.link_title.clone().unwrap_or_default()));
    }
    if is_poll_row(m) {
        e.kind = "poll";
        match x.poll {
            Some(p) => {
                e.text = None;
                e.poll = Some(PollEntry {
                    question: p.question,
                    options: p.options.into_iter().map(|(name, votes, _)| (name, votes)).collect(),
                    multi: p.selectable_count != 1,
                });
            }
            None => e.text = e.text.as_deref().map(|t| t.trim_start_matches("📊 ").to_string()),
        }
        return e;
    }
    if is_contact_card(m) {
        e.kind = "contact";
        e.contacts = parse_shared_contacts(m.text.as_deref().unwrap_or_default())
            .into_iter()
            .map(|c| (c.name, c.phone))
            .collect();
        e.text = None;
        return e;
    }
    if let Some(at) = m.geo_lat.zip(m.geo_lon) {
        e.kind = "location";
        e.location = Some(at);
    }
    if is_media(m.kind) && !is_view_once(m) {
        let inline = matches!(m.kind, MessageKind::Image | MessageKind::Sticker) && x.copied.is_some();
        e.media = Some(MediaRef {
            label: kind_label(m),
            missing: x.include_media && x.copied.is_none(),
            file: x.copied,
            inline,
            mime: m.media_mime.clone(),
        });
    }
    e
}

/// Writes the header, each entry and the footer of one format.
pub trait Writer {
    fn start(&mut self, head: &Head) -> std::io::Result<()>;
    fn entry(&mut self, e: &Entry) -> std::io::Result<()>;
    fn finish(&mut self, total: u64) -> std::io::Result<()>;
}

/// The writer of `format` over `out`.
pub fn writer<'a, W: std::io::Write + 'a>(format: Format, out: W) -> Box<dyn Writer + 'a> {
    match format {
        Format::Html => Box::new(html::HtmlWriter::new(out)),
        Format::Text => Box::new(plain::TextWriter::new(out)),
        Format::Json => Box::new(json::JsonWriter::new(out)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    pub(crate) fn when(h: u32, m: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 3, 7).and_then(|d| d.and_hms_opt(h, m, 0)).expect("valid time")
    }

    pub(crate) fn row(id: &str, text: &str) -> MessageRow {
        MessageRow {
            chat_jid: "111@s.whatsapp.net".into(),
            id: id.into(),
            sender_jid: "111@s.whatsapp.net".into(),
            sender_name: Some("Ann".into()),
            ts: 1_772_874_300,
            text: Some(text.into()),
            ..Default::default()
        }
    }

    fn extras(copied: Option<&str>, include: bool) -> Extras {
        Extras {
            when: when(9, 5),
            iso: "2026-03-07T09:05:00+00:00".into(),
            reactions: Vec::new(),
            poll: None,
            include_media: include,
            copied: copied.map(String::from),
        }
    }

    pub(crate) fn entry(m: &MessageRow) -> Entry {
        entry_from(m, &ChatCtx { name: "Ann".into(), is_group: false }, extras(None, false))
    }

    #[test]
    fn own_messages_are_from_you_and_unnamed_group_senders_show_a_number() {
        let mut m = row("a", "hi");
        m.from_me = true;
        assert_eq!(entry(&m).sender, "You");
        let mut g = row("b", "hi");
        g.sender_name = None;
        g.sender_jid = "4915112345678@s.whatsapp.net".into();
        let group = ChatCtx { name: "Team".into(), is_group: true };
        assert_eq!(entry_from(&g, &group, extras(None, false)).sender, "+4915112345678");
        let dm = ChatCtx { name: "Ann".into(), is_group: false };
        assert_eq!(entry_from(&g, &dm, extras(None, false)).sender, "Ann");
    }

    #[test]
    fn view_once_placeholders_never_carry_media() {
        let mut m = row("v", VIEW_ONCE_TEXT);
        m.kind = MessageKind::Image;
        m.local_path = Some("C:\\media\\x.jpg".into());
        let e = entry_from(&m, &ChatCtx { name: "Ann".into(), is_group: false }, extras(Some("media/00001-x.jpg"), true));
        assert!(e.media.is_none());
        assert_eq!(e.text.as_deref(), Some(VIEW_ONCE_TEXT));
    }

    #[test]
    fn deleted_messages_lose_their_content() {
        let mut m = row("d", "secret");
        m.revoked = true;
        m.quoted_id = Some("q".into());
        let e = entry(&m);
        assert_eq!(e.kind, "deleted");
        assert_eq!(e.text.as_deref(), Some("This message was deleted"));
        assert!(e.quoted_id.is_none());
    }

    #[test]
    fn notices_polls_contacts_and_locations_get_their_own_kinds() {
        let n = entry(&row("GRP-1-0", "Ann added Bob"));
        assert!(n.notice && n.kind == "notice" && n.sender.is_empty());
        let mut c = row("c", "👤 Carol\n+4915112345678");
        c.kind = MessageKind::Other;
        c.media_mime = Some("text/vcard".into());
        let c = entry(&c);
        assert_eq!(c.kind, "contact");
        assert_eq!(c.contacts, vec![("Carol".to_string(), "+4915112345678".to_string())]);
        let mut l = row("l", "");
        l.geo_lat = Some(35.7);
        l.geo_lon = Some(51.4);
        assert_eq!(entry(&l).location, Some((35.7, 51.4)));
        let mut p = row("p", "📊 Lunch?");
        p.kind = MessageKind::Other;
        let view = PollView { question: "Lunch?".into(), options: vec![("Pizza".into(), 2, false), ("Soup".into(), 0, true)], selectable_count: 1 };
        let mut x = extras(None, false);
        x.poll = Some(view);
        let e = entry_from(&p, &ChatCtx { name: "Ann".into(), is_group: false }, x);
        assert_eq!(e.kind, "poll");
        assert_eq!(e.poll.as_ref().map(|p| p.options.len()), Some(2));
        assert_eq!(e.poll.map(|p| p.multi), Some(false));
    }

    #[test]
    fn media_is_inline_only_when_copied() {
        let mut m = row("i", "caption");
        m.kind = MessageKind::Image;
        let ctx = ChatCtx { name: "Ann".into(), is_group: false };
        let with = entry_from(&m, &ctx, extras(Some("media/00001-a.jpg"), true)).media.expect("media");
        assert!(with.inline && !with.missing && with.file.is_some());
        let missing = entry_from(&m, &ctx, extras(None, true)).media.expect("media");
        assert!(missing.missing && !missing.inline);
        let skipped = entry_from(&m, &ctx, extras(None, false)).media.expect("media");
        assert!(!skipped.missing && skipped.file.is_none());
    }
}
