//! Posting our own status (text, image or video).
//!
//! The library only labels a status with a privacy setting; it does not filter
//! by it. So the recipient list is computed here: the address-book contacts
//! (the phone's own audience setting cannot be read) with the audience of the
//! command applied, minus groups, newsletters, ourselves and blocked accounts.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use whatsapp_rust::download::MediaType;
use whatsapp_rust::media::{self, VideoOptions};
use whatsapp_rust::prelude::{Client, Jid, wa};
use whatsapp_rust::upload::UploadOptions;
use whatsapp_rust::buffa::Message as _;
use whatsapp_rust::{StatusPrivacySetting, StatusSendOptions};
use wa_store::{MessageKind, NewMessage, STATUS_JID};

use crate::api::{StatusAudience, UiEvent};
use crate::commands::{NOT_LOGGED_IN, Prepared, build_media_message, sender_forms};
use crate::events::{Shared, now_secs, own_jid};
use crate::jid::{classify_error, normalize, normalize_str, warn_lib_error};
use crate::media as cache;

type FontType = wa::message::extended_text_message::FontType;

/// WhatsApp's limit for the text of a text status, in characters.
const MAX_TEXT_CHARS: usize = 700;
/// Caption limit of an image or video status, in characters.
const MAX_CAPTION_CHARS: usize = 1024;
/// Longest video WhatsApp accepts as a status.
const MAX_VIDEO_SECS: u64 = 60;
/// Most recipients one status is fanned out to.
const MAX_RECIPIENTS: usize = 5000;

const NO_CONTACTS: &str = "No contacts to share with";

// ------------------------------------------------------------------ pure parts

/// The status font for WhatsApp's font index; unknown indexes use the system font.
pub(crate) fn font_for(index: u8) -> FontType {
    match index {
        1 => FontType::SYSTEM_TEXT,
        2 => FontType::FB_SCRIPT,
        6 => FontType::SYSTEM_BOLD,
        7 => FontType::MORNINGBREEZE_REGULAR,
        8 => FontType::CALISTOGA_REGULAR,
        9 => FontType::EXO2_EXTRABOLD,
        10 => FontType::COURIERPRIME_BOLD,
        _ => FontType::SYSTEM,
    }
}

/// The text to post: non-blank and at most 700 characters.
pub(crate) fn validate_text(text: &str) -> Result<&str, String> {
    if text.trim().is_empty() {
        return Err("The status text is empty".to_string());
    }
    if text.chars().count() > MAX_TEXT_CHARS {
        return Err(format!("A text status can have at most {MAX_TEXT_CHARS} characters"));
    }
    Ok(text)
}

/// A background without transparency: a transparent one would make the text
/// unreadable for everyone.
pub(crate) fn opaque_background(argb: u32) -> u32 {
    argb | 0xFF00_0000
}

/// `media_mime` of our own text status row, so our copy renders like the
/// recipients' one: "text/status;bg=AARRGGBB;font=N" with the opaque background
/// as 8 upper-case hex digits and N the WhatsApp font index as sent by the UI.
pub(crate) fn text_status_mime(background_argb: u32, font: u8) -> String {
    format!("text/status;bg={:08X};font={font}", opaque_background(background_argb))
}

/// `jid` as a bare user address (no device) when it is a person (PN or LID).
fn user_jid(jid: &str) -> Option<String> {
    let parsed = jid.parse::<Jid>().ok()?;
    (parsed.is_pn() || parsed.is_lid()).then(|| parsed.to_non_ad().to_string())
}

/// Valid, de-duplicated user addresses of `jids`, without `excluded` ones.
fn valid_users<'a>(
    jids: impl IntoIterator<Item = &'a String>,
    excluded: &HashSet<String>,
) -> Vec<String> {
    let mut seen = HashSet::new();
    jids.into_iter()
        .filter_map(|j| user_jid(j))
        .filter(|j| !excluded.contains(j) && seen.insert(j.clone()))
        .collect()
}

/// Who a status goes to, and the privacy label that matches it.
#[derive(Debug, PartialEq)]
pub(crate) struct Recipients {
    pub(crate) jids: Vec<String>,
    pub(crate) privacy: StatusPrivacySetting,
    /// More than `MAX_RECIPIENTS` qualified; `jids` holds only the first ones.
    pub(crate) truncated: bool,
}

/// The recipients of a status. `address_book` are our synced contacts;
/// `blocked` and `own` (our PN and LID) are never included, whatever the audience.
///
/// Fail closed: callers put both the PN and the LID form of every blocked
/// account into `blocked`, so a recipient is excluded whichever form it is
/// written in. When the other form of a blocked account is unknown (no LID<->PN
/// mapping yet) only the known form is compared.
pub(crate) fn recipients_for(
    audience: &StatusAudience,
    address_book: &[String],
    blocked: &[String],
    own: &[String],
) -> Recipients {
    let mut never: HashSet<String> = blocked.iter().filter_map(|j| user_jid(j)).collect();
    never.extend(own.iter().filter_map(|j| user_jid(j)));
    let (mut jids, privacy) = match audience {
        StatusAudience::MyContacts => (valid_users(address_book, &never), StatusPrivacySetting::Contacts),
        StatusAudience::Except(list) => {
            never.extend(list.iter().filter_map(|j| user_jid(j)));
            (valid_users(address_book, &never), StatusPrivacySetting::DenyList)
        }
        StatusAudience::OnlyShareWith(list) => (valid_users(list, &never), StatusPrivacySetting::AllowList),
    };
    let truncated = jids.len() > MAX_RECIPIENTS;
    if truncated {
        log::warn!("status recipients capped: {} qualified, {MAX_RECIPIENTS} kept", jids.len());
        jids.truncate(MAX_RECIPIENTS);
    }
    Recipients { jids, privacy, truncated }
}

/// Finds the box `name` among the boxes in `data` (an MP4 box sequence) and
/// returns its payload.
fn find_box<'a>(mut data: &'a [u8], name: &[u8; 4]) -> Option<&'a [u8]> {
    while data.len() >= 8 {
        let size32 = u32::from_be_bytes(data[0..4].try_into().ok()?);
        let (header, size) = match size32 {
            0 => (8, data.len()),
            1 => {
                let big = u64::from_be_bytes(data.get(8..16)?.try_into().ok()?);
                (16, usize::try_from(big).ok()?)
            }
            n => (8, usize::try_from(n).ok()?),
        };
        if size < header || size > data.len() {
            return None;
        }
        if &data[4..8] == name {
            return Some(&data[header..size]);
        }
        data = &data[size..];
    }
    None
}

/// `(duration, timescale)` of an MP4/MOV/3GP file, from its movie header.
fn mp4_duration(data: &[u8]) -> Option<(u64, u32)> {
    let moov = find_box(data, b"moov")?;
    let mvhd = find_box(moov, b"mvhd")?;
    let be32 = |at: usize| Some(u32::from_be_bytes(mvhd.get(at..at + 4)?.try_into().ok()?));
    match mvhd.first()? {
        0 => Some((u64::from(be32(16)?), be32(12)?)),
        1 => Some((u64::from_be_bytes(mvhd.get(24..32)?.try_into().ok()?), be32(20)?)),
        _ => None,
    }
    .filter(|&(_, scale)| scale > 0)
}

/// What a media status file turned out to be.
#[derive(Debug)]
pub(crate) struct MediaPlan {
    pub(crate) kind: MessageKind,
    pub(crate) mime: String,
    pub(crate) dims: Option<(u32, u32)>,
    pub(crate) thumbnail: Option<Vec<u8>>,
    pub(crate) duration_secs: Option<u32>,
}

/// Classifies the file by extension, checks it is what it claims and that a
/// video is at most 60 seconds long.
pub(crate) fn plan_media(path: &Path, data: &[u8]) -> Result<MediaPlan, String> {
    let mime = cache::mime_for_path(path).unwrap_or("");
    if mime.starts_with("image/") {
        return match (cache::image_info(data), cache::sniff_image_mime(data)) {
            (Some((w, h, thumb)), Some(real)) => Ok(MediaPlan {
                kind: MessageKind::Image,
                mime: real.to_string(),
                dims: Some((w, h)),
                thumbnail: Some(thumb),
                duration_secs: None,
            }),
            _ => Err("Only JPEG, PNG or WebP images can be posted as a status".to_string()),
        };
    }
    if matches!(mime, "video/mp4" | "video/quicktime" | "video/3gpp") {
        let (duration, scale) = mp4_duration(data)
            .ok_or_else(|| "Could not read the length of the video".to_string())?;
        if duration > MAX_VIDEO_SECS.saturating_mul(u64::from(scale)) {
            return Err(format!("A video status can be at most {MAX_VIDEO_SECS} seconds long"));
        }
        return Ok(MediaPlan {
            kind: MessageKind::Video,
            mime: mime.to_string(),
            dims: None,
            // No video decoder is bundled, so a video goes without a preview frame.
            thumbnail: None,
            duration_secs: Some(u32::try_from(duration.div_ceil(u64::from(scale))).unwrap_or(u32::MAX)),
        });
    }
    Err("Only images and MP4 videos can be posted as a status".to_string())
}

// ----------------------------------------------------------------- async parts

fn reply(sh: &Shared, ok: bool, message: Option<String>) {
    sh.emit(UiEvent::StatusPosted { ok, message });
}

fn fail(sh: &Shared, message: impl Into<String>) {
    reply(sh, false, Some(message.into()));
}

/// Works out whom to post to. `Err` carries the message for the UI.
async fn resolve_recipients(
    sh: &Shared,
    client: &Arc<Client>,
    audience: &StatusAudience,
) -> Result<(Vec<Jid>, StatusPrivacySetting, Option<String>), String> {
    let book = sh.store.address_book_contacts().map_err(|e| {
        log::error!("reading the address book failed: {e}");
        "Could not read your contacts".to_string()
    })?;
    // Posting to someone we blocked would be worse than not posting at all.
    let entries = client.blocking().get_blocklist().await.map_err(|e| {
        warn_lib_error("status: block list lookup failed", &e);
        "Could not check the block list".to_string()
    })?;
    // Both address forms of every blocked account: contacts may be stored as either.
    let mut blocked = Vec::with_capacity(entries.len() * 2);
    for entry in &entries {
        blocked.push(normalize(client, &entry.jid, None).await);
        let (pn, lid) = sender_forms(client, &entry.jid).await;
        blocked.extend([pn, lid].into_iter().flatten().map(|j| j.to_string()));
    }
    let own: Vec<String> = own_jid(client)
        .into_iter()
        .chain(client.lid().map(|l| l.to_non_ad().to_string()))
        .collect();

    // The UI may name a person by LID or phone number; contacts are stored canonically.
    let canonical = async |list: &[String]| {
        let mut out = Vec::with_capacity(list.len());
        for j in list {
            out.push(normalize_str(client, j).await);
        }
        out
    };
    let audience = match audience {
        StatusAudience::MyContacts => StatusAudience::MyContacts,
        StatusAudience::Except(list) => StatusAudience::Except(canonical(list).await),
        StatusAudience::OnlyShareWith(list) => StatusAudience::OnlyShareWith(canonical(list).await),
    };
    let Recipients { jids, privacy, truncated } = recipients_for(&audience, &book, &blocked, &own);
    if jids.is_empty() {
        return Err(NO_CONTACTS.to_string());
    }
    log::info!("posting a status to {} recipients", jids.len());
    let parsed = jids.iter().filter_map(|j| j.parse::<Jid>().ok()).collect();
    let note = truncated.then(|| format!("Shared with the first {MAX_RECIPIENTS} contacts"));
    Ok((parsed, privacy, note))
}

/// Our own status as a row of the status chat, shaped like a received one.
fn own_row(sender_jid: String, id: String, kind: MessageKind, text: Option<String>) -> NewMessage {
    NewMessage {
        chat_jid: STATUS_JID.to_string(),
        id,
        sender_jid,
        from_me: true,
        ts: now_secs(),
        kind,
        text,
        ..Default::default()
    }
}

fn record(sh: &Shared, row: &NewMessage) {
    match sh.store.insert_message(row, false) {
        Ok(_) => sh.emit(UiEvent::StatusesChanged),
        // The post itself went out; only our local copy is missing.
        Err(e) => log::error!("storing our own status failed: {e}"),
    }
}

pub(crate) async fn post_text(
    sh: &Shared,
    client: &Arc<Client>,
    text: &str,
    background_argb: u32,
    font: u8,
    audience: &StatusAudience,
) {
    let text = match validate_text(text) {
        Ok(t) => t,
        Err(m) => return fail(sh, m),
    };
    let Some(sender) = own_jid(client) else {
        return fail(sh, NOT_LOGGED_IN);
    };
    let (recipients, privacy, note) = match resolve_recipients(sh, client, audience).await {
        Ok(r) => r,
        Err(m) => return fail(sh, m),
    };
    let options = StatusSendOptions { privacy, ..Default::default() };
    let sent = client
        .status()
        .send_text(text, opaque_background(background_argb), font_for(font), &recipients, options)
        .await;
    match sent {
        Ok(res) => {
            let mut row = own_row(sender, res.message_id, MessageKind::Text, Some(text.to_string()));
            row.media_mime = Some(text_status_mime(background_argb, font));
            record(sh, &row);
            reply(sh, true, note);
        }
        Err(e) => {
            warn_lib_error("posting a text status failed", &e);
            fail(sh, format!("Could not post the status ({})", classify_error(&e)));
        }
    }
}

/// Reads and checks the file and copies it into the media cache under `id`.
fn prepare_media(
    path: &Path,
    media_dir: &Path,
    id: &str,
) -> Result<(MediaPlan, Vec<u8>, PathBuf), String> {
    let unreadable = |e: std::io::Error| format!("Cannot read the file: {e}");
    let meta = std::fs::metadata(path).map_err(unreadable)?;
    let too_big = || "The file is larger than 100 MB".to_string();
    if meta.len() > cache::MAX_SEND_BYTES {
        return Err(too_big());
    }
    let data = std::fs::read(path).map_err(unreadable)?;
    if u64::try_from(data.len()).map_or(true, |n| n > cache::MAX_SEND_BYTES) {
        return Err(too_big());
    }
    let plan = plan_media(path, &data)?;
    let ext = cache::extension_for(plan.kind, Some(&plan.mime), None);
    let cached = cache::cache_path(media_dir, STATUS_JID, id, &ext);
    cache::write_atomic(&cached, &data).map_err(|e| format!("Cannot copy the file: {e}"))?;
    Ok((plan, data, cached))
}

pub(crate) async fn post_media(
    sh: &Shared,
    client: &Arc<Client>,
    path: &Path,
    caption: Option<String>,
    audience: &StatusAudience,
) {
    let caption = caption.filter(|c| !c.is_empty());
    if caption.as_deref().is_some_and(|c| c.chars().count() > MAX_CAPTION_CHARS) {
        return fail(sh, format!("A caption can have at most {MAX_CAPTION_CHARS} characters"));
    }
    let Some(sender) = own_jid(client) else {
        return fail(sh, NOT_LOGGED_IN);
    };
    let (recipients, privacy, note) = match resolve_recipients(sh, client, audience).await {
        Ok(r) => r,
        Err(m) => return fail(sh, m),
    };

    let id = client.generate_message_id();
    let job = {
        let (path, media_dir, id) = (path.to_path_buf(), sh.media.dir.clone(), id.clone());
        move || prepare_media(&path, &media_dir, &id)
    };
    let (plan, data, cached) = match tokio::task::spawn_blocking(job).await {
        Ok(Ok(done)) => done,
        Ok(Err(m)) => return fail(sh, m),
        Err(e) => {
            log::error!("status media preparation task failed: {e}");
            return fail(sh, "Could not prepare the file");
        }
    };
    let discard_copy = |cached: &Path| {
        if let Err(e) = std::fs::remove_file(cached) {
            log::warn!("cannot remove the cached copy of an unposted status: {e}");
        }
    };

    let size = i64::try_from(data.len()).ok();
    let media_type = if plan.kind == MessageKind::Video { MediaType::Video } else { MediaType::Image };
    let upload = match client.upload(data, media_type, UploadOptions::new()).await {
        Ok(u) => u,
        Err(e) => {
            warn_lib_error("uploading a status failed", &e);
            discard_copy(&cached);
            return fail(sh, format!("Upload failed ({})", classify_error(&e)));
        }
    };
    let message = match plan.kind {
        MessageKind::Video => media::video_message(
            upload,
            VideoOptions {
                caption: caption.clone(),
                mimetype: Some(plan.mime.clone()),
                duration_seconds: plan.duration_secs,
                ..Default::default()
            },
        ),
        _ => build_media_message(
            &Prepared {
                kind: MessageKind::Image,
                media_type: MediaType::Image,
                mime: plan.mime.clone(),
                file_name: None,
                duration: None,
                dims: plan.dims,
                thumbnail: plan.thumbnail.clone(),
            },
            upload,
            caption.clone(),
            None,
        ),
    };
    let raw = message.encode_to_vec();
    let options = StatusSendOptions {
        privacy,
        message_id: Some(id.clone()),
        ..Default::default()
    };
    match client.status().send_raw(message, &recipients, options).await {
        Ok(res) => {
            let mut row = own_row(sender, res.message_id, plan.kind, caption);
            row.media_mime = Some(plan.mime);
            row.media_size = size;
            row.media_duration = plan.duration_secs.map(i64::from);
            row.thumbnail = plan.thumbnail;
            row.local_path = Some(cached.to_string_lossy().into_owned());
            row.raw = Some(raw);
            record(sh, &row);
            reply(sh, true, note);
        }
        Err(e) => {
            warn_lib_error("posting a media status failed", &e);
            discard_copy(&cached);
            fail(sh, format!("Could not post the status ({})", classify_error(&e)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(list: &[&str]) -> Vec<String> {
        list.iter().map(|j| (*j).to_string()).collect()
    }

    const ME: &str = "1@s.whatsapp.net";
    const MY_LID: &str = "100@lid";

    fn book() -> Vec<String> {
        s(&[
            "2@s.whatsapp.net",
            "3@s.whatsapp.net",
            "3@s.whatsapp.net",
            "4:7@s.whatsapp.net",
            "5@lid",
            "9-9@g.us",
            "6@newsletter",
            "status@broadcast",
            ME,
            MY_LID,
            "7@s.whatsapp.net",
            "garbage",
        ])
    }

    fn pick(audience: &StatusAudience) -> Recipients {
        recipients_for(audience, &book(), &s(&["7@s.whatsapp.net"]), &s(&[ME, MY_LID]))
    }

    #[test]
    fn my_contacts_excludes_groups_newsletters_self_blocked_and_duplicates() {
        let r = pick(&StatusAudience::MyContacts);
        assert_eq!(r.privacy, StatusPrivacySetting::Contacts);
        assert_eq!(r.jids, ["2@s.whatsapp.net", "3@s.whatsapp.net", "4@s.whatsapp.net", "5@lid"]);
    }

    #[test]
    fn except_removes_the_listed_contacts_and_is_a_deny_list() {
        let r = pick(&StatusAudience::Except(s(&["3@s.whatsapp.net", "4:2@s.whatsapp.net", "nobody@s.whatsapp.net"])));
        assert_eq!(r.privacy, StatusPrivacySetting::DenyList);
        assert_eq!(r.jids, ["2@s.whatsapp.net", "5@lid"]);
    }

    #[test]
    fn only_share_with_keeps_valid_users_and_is_an_allow_list() {
        let r = pick(&StatusAudience::OnlyShareWith(s(&[
            "8@s.whatsapp.net",
            "9-9@g.us",
            "6@newsletter",
            ME,
            "7@s.whatsapp.net",
            "8@s.whatsapp.net",
            "x",
            "5@lid",
        ])));
        assert_eq!(r.privacy, StatusPrivacySetting::AllowList);
        // Not limited to the address book; blocked and own accounts still drop out.
        assert_eq!(r.jids, ["8@s.whatsapp.net", "5@lid"]);
    }

    #[test]
    fn an_empty_audience_yields_no_recipients() {
        assert!(recipients_for(&StatusAudience::MyContacts, &[], &[], &[]).jids.is_empty());
        let r = recipients_for(&StatusAudience::OnlyShareWith(s(&[ME])), &book(), &[], &s(&[ME]));
        assert!(r.jids.is_empty());
    }

    #[test]
    fn recipients_are_capped() {
        let many: Vec<String> = (0..MAX_RECIPIENTS + 10).map(|i| format!("{}@s.whatsapp.net", 1000 + i)).collect();
        let r = recipients_for(&StatusAudience::MyContacts, &many, &[], &[]);
        assert_eq!(r.jids.len(), MAX_RECIPIENTS);
        assert!(r.truncated);
        let r = recipients_for(&StatusAudience::MyContacts, &many[..MAX_RECIPIENTS], &[], &[]);
        assert!(!r.truncated);
    }

    #[test]
    fn blocked_accounts_are_excluded_in_both_address_forms() {
        let book = s(&["7@lid", "8@s.whatsapp.net", "9@lid"]);
        // Blocked as PN 8 / LID 7 and their known alternates 7 PN / 8 LID.
        let blocked = s(&["8@s.whatsapp.net", "8@lid", "7@lid", "7@s.whatsapp.net"]);
        let r = recipients_for(&StatusAudience::MyContacts, &book, &blocked, &[]);
        assert_eq!(r.jids, ["9@lid"]);
        let only = StatusAudience::OnlyShareWith(s(&["7@s.whatsapp.net", "8@lid", "9@lid"]));
        assert_eq!(recipients_for(&only, &[], &blocked, &[]).jids, ["9@lid"]);
        // Unknown alternate form: only what is known can be compared.
        let r = recipients_for(&StatusAudience::MyContacts, &book, &s(&["8@s.whatsapp.net"]), &[]);
        assert_eq!(r.jids, ["7@lid", "9@lid"]);
    }

    #[test]
    fn text_status_row_encodes_background_and_font() {
        assert_eq!(text_status_mime(0x001E_6E4F, 7), "text/status;bg=FF1E6E4F;font=7");
    }

    #[test]
    fn text_is_validated() {
        assert!(validate_text("").is_err());
        assert!(validate_text("  \n ").is_err());
        assert_eq!(validate_text("hi"), Ok("hi"));
        assert!(validate_text(&"a".repeat(700)).is_ok());
        assert!(validate_text(&"a".repeat(701)).is_err());
        // Characters, not bytes.
        assert!(validate_text(&"é".repeat(700)).is_ok());
    }

    #[test]
    fn background_is_made_opaque() {
        assert_eq!(opaque_background(0x001E_6E4F), 0xFF1E_6E4F);
        assert_eq!(opaque_background(0xFF1E_6E4F), 0xFF1E_6E4F);
    }

    #[test]
    fn fonts_map_with_a_system_fallback() {
        assert_eq!(font_for(0), FontType::SYSTEM);
        assert_eq!(font_for(2), FontType::FB_SCRIPT);
        assert_eq!(font_for(6), FontType::SYSTEM_BOLD);
        assert_eq!(font_for(10), FontType::COURIERPRIME_BOLD);
        for unknown in [3, 4, 5, 11, 200, 255] {
            assert_eq!(font_for(unknown), FontType::SYSTEM);
        }
    }

    fn mp4(duration: u32, scale: u32) -> Vec<u8> {
        let mut mvhd = vec![0u8; 4];
        mvhd.extend([0u8; 8]);
        mvhd.extend(scale.to_be_bytes());
        mvhd.extend(duration.to_be_bytes());
        mvhd.extend([0u8; 80]);
        let boxed = |name: &[u8; 4], body: &[u8]| {
            let mut b = u32::try_from(body.len() + 8).unwrap().to_be_bytes().to_vec();
            b.extend(name);
            b.extend(body);
            b
        };
        let mut file = boxed(b"ftyp", b"isom\0\0\0\0");
        file.extend(boxed(b"moov", &boxed(b"mvhd", &mvhd)));
        file.extend(boxed(b"mdat", b"data"));
        file
    }

    #[test]
    fn videos_up_to_a_minute_are_accepted_with_their_duration() {
        let plan = plan_media(Path::new("clip.mp4"), &mp4(60_000, 1000)).expect("60 s is fine");
        assert_eq!(plan.kind, MessageKind::Video);
        assert_eq!(plan.duration_secs, Some(60));
        let plan = plan_media(Path::new("clip.MOV"), &mp4(12_500, 1000)).expect("12.5 s");
        assert_eq!(plan.mime, "video/quicktime");
        assert_eq!(plan.duration_secs, Some(13));
    }

    #[test]
    fn videos_longer_than_a_minute_are_rejected() {
        let err = plan_media(Path::new("clip.mp4"), &mp4(60_001, 1000)).unwrap_err();
        assert!(err.contains("60 seconds"), "{err}");
        assert!(plan_media(Path::new("clip.mp4"), &mp4(5 * 60 * 600, 600)).is_err());
    }

    #[test]
    fn unreadable_videos_and_other_files_are_rejected() {
        assert!(plan_media(Path::new("clip.mp4"), b"not a video").is_err());
        assert!(plan_media(Path::new("clip.mp4"), &mp4(10, 0)).is_err());
        assert!(plan_media(Path::new("clip.webm"), &mp4(10, 1)).is_err());
        assert!(plan_media(Path::new("doc.pdf"), b"%PDF").is_err());
        assert!(plan_media(Path::new("pic.jpg"), b"not an image").is_err());
    }

    #[test]
    fn images_get_dimensions_and_a_thumbnail() {
        let img = image::RgbImage::new(40, 20);
        let mut png = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .expect("encode");
        let plan = plan_media(Path::new("a.png"), &png).expect("image");
        assert_eq!(plan.kind, MessageKind::Image);
        assert_eq!(plan.mime, "image/png");
        assert_eq!(plan.dims, Some((40, 20)));
        assert!(plan.thumbnail.is_some());
    }
}
