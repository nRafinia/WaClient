//! Account and chat actions: mark unread, clear a chat, block list and our own
//! profile (name, about, picture).
//!
//! Logs carry the command name and redacted addresses only, never names, about
//! texts, file paths or picture bytes.

use std::io::{Cursor, Read as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use image::ImageDecoder as _;
use image::imageops::FilterType;
use whatsapp_rust::BlockingError;
use whatsapp_rust::prelude::{Client, Jid};
use whatsapp_rust::{message_key, message_range};
use wa_store::ContactPatch;

use crate::api::{OwnProfile, UiEvent};
use crate::avatar::{paced_slot, record_outcome};
use crate::commands::{
    KeyParticipant, app_state_failure, chat_for_flag_change, guarded_revert, log_flag_outcome,
    parse_jid, reject, remove_cached_file, resolve_participant, with_app_state_timeout,
};
use crate::events::{Shared, now_secs, own_jid};
use crate::info::{full_picture, phone_of_jid};
use crate::jid::{normalize, redact, warn_lib_error};
use crate::media::{avatar_full_path, lock, write_atomic_async};

/// Largest picture file accepted for the profile picture.
const MAX_PICTURE_BYTES: u64 = 5 * 1024 * 1024;
/// Side of the square JPEG that is uploaded.
const PICTURE_SIDE: u32 = 640;
const PICTURE_QUALITY: u8 = 85;
/// Decoder limits: a small file can still claim an enormous canvas.
const MAX_SOURCE_SIDE: u32 = 6_000;
const MAX_DECODE_ALLOC: u64 = 256 * 1024 * 1024;

/// WhatsApp's limits for the profile name and the About text, in characters.
const MAX_NAME_CHARS: usize = 25;
const MAX_ABOUT_CHARS: usize = 139;

/// Per-generation state of the account actions.
#[derive(Default)]
pub(crate) struct AccountState {
    blocklist: Mutex<BlocklistCache>,
    /// App-state chat commands running now; an app-state reset waits for zero.
    pub(crate) app_state_inflight: AtomicU32,
}

impl AccountState {
    /// Counts one running app-state command until the guard is dropped.
    pub(crate) fn begin_app_state(&self) -> InflightGuard<'_> {
        self.app_state_inflight.fetch_add(1, Ordering::SeqCst);
        InflightGuard(&self.app_state_inflight)
    }
}

pub(crate) struct InflightGuard<'a>(&'a AtomicU32);

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The last block list seen in this generation, as canonical chat addresses.
#[derive(Default)]
pub(crate) struct BlocklistCache {
    jids: Option<Vec<String>>,
}

impl BlocklistCache {
    /// Replaces the list (sorted, without duplicates) and returns it.
    pub(crate) fn replace(&mut self, mut jids: Vec<String>) -> Vec<String> {
        jids.sort();
        jids.dedup();
        self.jids = Some(jids.clone());
        jids
    }

    pub(crate) fn snapshot(&self) -> Option<Vec<String>> {
        self.jids.clone()
    }

    /// Adds or removes one address; `None` while no list is known (a single
    /// entry would then pass for the whole list).
    pub(crate) fn apply(&mut self, jid: &str, blocked: bool) -> Option<Vec<String>> {
        let list = self.jids.as_mut()?;
        list.retain(|j| j != jid);
        if blocked {
            list.push(jid.to_string());
            list.sort();
        }
        Some(list.clone())
    }
}

// ----------------------------------------------------------------- unread / clear

/// The unread counter after "mark as unread": at least one, so the badge shows.
pub(crate) fn unread_after_mark(current: i64) -> i64 {
    current.max(1)
}

/// The counter to restore after a failed mark-unread: the previous one, but
/// only while the row still holds the value we wrote (a newer message or a
/// read on another device must not be overwritten).
pub(crate) fn unread_revert(current: i64, written: i64, previous: i64) -> Option<i64> {
    guarded_revert(current, written, previous)
}

/// The range other devices use to resolve conflicting chat actions, with the
/// message that ends it (the bound for a local clear).
struct NewestRange {
    range: whatsapp_rust::SyncActionMessageRange,
    ts: i64,
    id: String,
}

/// The newest stored message as a [`NewestRange`]; `None` for an empty chat.
async fn newest_range(
    sh: &Shared,
    client: &Arc<Client>,
    chat: &Jid,
    chat_jid: &str,
) -> Option<NewestRange> {
    let newest = sh.store.messages_page(chat_jid, None, 1).unwrap_or_else(|e| {
        log::error!("newest message lookup failed: {e}");
        Vec::new()
    });
    let row = newest.first()?;
    let messages = match resolve_participant(sh, client, chat, row, false).await {
        KeyParticipant::Omit => vec![(message_key(&row.id, chat, row.from_me, None), row.ts)],
        KeyParticipant::Use(p) => vec![(message_key(&row.id, chat, row.from_me, Some(&p)), row.ts)],
        KeyParticipant::Skip => Vec::new(),
    };
    Some(NewestRange { range: message_range(row.ts, None, messages), ts: row.ts, id: row.id.clone() })
}

pub(crate) async fn mark_chat_unread(sh: &Shared, client: &Arc<Client>, chat_jid: &str) {
    let Some((chat, previous)) = chat_for_flag_change(sh, chat_jid).await else {
        return;
    };
    let written = unread_after_mark(previous.unread);
    if written != previous.unread {
        if let Err(e) = sh.store.set_unread(chat_jid, written) {
            log::error!("storing the unread flag failed: {e}");
            reject(sh, format!("Could not update the chat: {e}"));
            return;
        }
    }
    sh.emit(UiEvent::ChatsChanged);

    let range = newest_range(sh, client, &chat, chat_jid).await.map(|n| n.range);
    let actions = client.chat_actions();
    let result = with_app_state_timeout(actions.mark_chat_as_read(&chat, false, range)).await;
    log_flag_outcome("MarkChatUnread", chat_jid, &result);
    if let Err(e) = result {
        match sh.store.chat(chat_jid) {
            Ok(Some(row)) => {
                if let Some(restore) = unread_revert(row.unread, written, previous.unread) {
                    if let Err(e) = sh.store.set_unread(chat_jid, restore) {
                        log::error!("reverting the unread flag failed: {e}");
                    }
                }
            }
            Ok(None) => {}
            Err(e) => log::error!("re-reading the chat for the unread revert failed: {e}"),
        }
        sh.emit(UiEvent::ChatsChanged);
        reject(sh, app_state_failure(&e, "mark the chat as unread"));
    }
}

pub(crate) async fn clear_chat(sh: &Shared, client: &Arc<Client>, chat_jid: &str, keep_starred: bool) {
    let Some((chat, _)) = chat_for_flag_change(sh, chat_jid).await else {
        return;
    };
    let newest = newest_range(sh, client, &chat, chat_jid).await;
    let range = newest.as_ref().map(|n| n.range.clone());
    let actions = client.chat_actions();
    // Media of other devices is not ours to delete: only the messages go.
    let result =
        with_app_state_timeout(actions.clear_chat(&chat, !keep_starred, false, range)).await;
    log_flag_outcome("ClearChat", chat_jid, &result);
    if let Err(e) = result {
        reject(sh, app_state_failure(&e, "clear the chat"));
        return;
    }
    // Only what the range covered goes: a message that arrived meanwhile stays.
    // An empty chat had nothing to clear.
    let Some(newest) = newest else {
        return;
    };
    let upto = Some((newest.ts, newest.id.as_str()));
    let removed = if keep_starred {
        sh.store.clear_chat_keep_starred(chat_jid, upto)
    } else {
        sh.store.clear_chat(chat_jid, upto)
    };
    match removed {
        Ok(paths) => {
            for path in &paths {
                remove_cached_file(sh, path);
            }
            sh.emit(UiEvent::MessagesChanged { chat_jid: chat_jid.to_string() });
            sh.emit(UiEvent::ChatsChanged);
        }
        Err(e) => {
            log::error!("clearing the chat locally failed: {e}");
            reject(sh, format!("Could not clear the chat: {e}"));
        }
    }
}

// ---------------------------------------------------------------------- blocking

fn cached_blocklist(sh: &Shared) -> Option<Vec<String>> {
    lock(&sh.account.blocklist).snapshot()
}

async fn load_blocklist(sh: &Shared, client: &Arc<Client>) -> Result<Vec<String>, String> {
    let entries = client.blocking().get_blocklist().await.map_err(|e| e.to_string())?;
    let mut jids = Vec::with_capacity(entries.len());
    for entry in &entries {
        jids.push(normalize(client, &entry.jid, None).await);
    }
    Ok(lock(&sh.account.blocklist).replace(jids))
}

pub(crate) async fn fetch_blocklist(sh: &Shared, client: &Arc<Client>) {
    match load_blocklist(sh, client).await {
        Ok(jids) => sh.emit(UiEvent::Blocklist { jids }),
        Err(e) => {
            warn_lib_error("FetchBlocklist failed", &e);
            // The last known list is better than none.
            match cached_blocklist(sh) {
                Some(jids) => sh.emit(UiEvent::Blocklist { jids }),
                None => reject(sh, format!("Could not load the block list: {e}")),
            }
        }
    }
}

pub(crate) async fn set_blocked(sh: &Shared, client: &Arc<Client>, jid: &str, blocked: bool) {
    let Some(target) = parse_jid(sh, jid) else {
        return;
    };
    if !(target.is_pn() || target.is_lid()) {
        reject(sh, "Only contacts can be blocked".to_string());
        return;
    }
    let result = if blocked {
        client.blocking().block(&target).await
    } else {
        client.blocking().unblock(&target).await
    };
    if let Err(e) = result {
        warn_lib_error(&format!("SetBlocked {} failed", redact(jid)), &e);
        reject(sh, block_error_message(blocked, &e));
        return;
    }
    log::info!("SetBlocked {}: ok", redact(jid));
    match load_blocklist(sh, client).await {
        Ok(jids) => sh.emit(UiEvent::Blocklist { jids }),
        Err(e) => {
            warn_lib_error("refreshing the block list failed", &e);
            let canonical = normalize(client, &target, None).await;
            let updated = lock(&sh.account.blocklist).apply(&canonical, blocked);
            match updated {
                Some(jids) => sh.emit(UiEvent::Blocklist { jids }),
                // No list to patch: ask again, which emits the list or an error.
                None => fetch_blocklist(sh, client).await,
            }
        }
    }
}

/// The UI text for a failed block or unblock.
pub(crate) fn block_error_message(blocked: bool, e: &BlockingError) -> String {
    if let BlockingError::InvalidJid(reason) = e
        && reason.contains("mapping")
    {
        return "Can't block this contact yet \u{2014} open a chat with them first and try again".to_string();
    }
    format!("Could not {} the contact: {e}", if blocked { "block" } else { "unblock" })
}

// ------------------------------------------------------------------- own profile

/// Trims and checks a profile name; the error is the text for the UI.
pub(crate) fn validate_name(raw: &str) -> Result<String, &'static str> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("The name cannot be empty");
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err("The name can have at most 25 characters");
    }
    Ok(name.to_string())
}

/// Trims and checks an About text; the error is the text for the UI.
pub(crate) fn validate_about(raw: &str) -> Result<String, &'static str> {
    let text = raw.trim();
    if text.is_empty() {
        return Err("The About text cannot be empty");
    }
    if text.chars().count() > MAX_ABOUT_CHARS {
        return Err("The About text can have at most 139 characters");
    }
    Ok(text.to_string())
}

/// Reads a picture file of at most [`MAX_PICTURE_BYTES`]. The path stays out
/// of the error text.
pub(crate) fn read_picture_file(path: &Path) -> Result<Vec<u8>, String> {
    let unreadable = |e: std::io::Error| {
        log::debug!("profile picture file unreadable: {e}");
        "The picture file cannot be read".to_string()
    };
    let meta = std::fs::metadata(path).map_err(unreadable)?;
    if !meta.is_file() {
        return Err("The picture file cannot be read".to_string());
    }
    let too_large = || "The picture is larger than 5 MB".to_string();
    if meta.len() > MAX_PICTURE_BYTES {
        return Err(too_large());
    }
    let mut data = Vec::new();
    std::fs::File::open(path)
        .map_err(unreadable)?
        .take(MAX_PICTURE_BYTES + 1)
        .read_to_end(&mut data)
        .map_err(unreadable)?;
    if data.len() as u64 > MAX_PICTURE_BYTES {
        return Err(too_large());
    }
    Ok(data)
}

/// Decodes an image (with size limits) and re-encodes it as a 640x640 JPEG,
/// center-cropped to a square.
pub(crate) fn reencode_profile_picture(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut reader = image::ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .map_err(|e| format!("The picture cannot be read: {e}"))?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_SOURCE_SIDE);
    limits.max_image_height = Some(MAX_SOURCE_SIDE);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);
    let unusable = |e: image::ImageError| format!("The picture cannot be used: {e}");
    let mut decoder = reader.into_decoder().map_err(unusable)?;
    // A phone photo is stored rotated and carries the turn in its EXIF data.
    let orientation = decoder.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = image::DynamicImage::from_decoder(decoder).map_err(unusable)?;
    img.apply_orientation(orientation);
    let square = image::DynamicImage::ImageRgb8(over_white(&img))
        .resize_to_fill(PICTURE_SIDE, PICTURE_SIDE, FilterType::Lanczos3)
        .to_rgb8();
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, PICTURE_QUALITY)
        .encode_image(&square)
        .map_err(|e| format!("The picture cannot be encoded: {e}"))?;
    Ok(out)
}

/// Composites transparent pixels over white: JPEG has no alpha, and dropping
/// the channel would turn them black.
fn over_white(img: &image::DynamicImage) -> image::RgbImage {
    let rgba = img.to_rgba8();
    image::RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let [r, g, b, a] = rgba.get_pixel(x, y).0;
        let blend = |c: u8| ((u32::from(c) * u32::from(a) + 255 * (255 - u32::from(a)) + 127) / 255) as u8;
        image::Rgb([blend(r), blend(g), blend(b)])
    })
}

/// What we know about ourselves without asking the server.
fn local_profile(sh: &Shared, client: &Client) -> Option<OwnProfile> {
    let jid = own_jid(client)?;
    let contact = sh.store.contact(&jid).unwrap_or_else(|e| {
        log::error!("own contact lookup failed: {e}");
        None
    });
    let push = Some(client.push_name()).filter(|n| !n.is_empty());
    Some(OwnProfile {
        phone: phone_of_jid(&jid),
        name: push.or_else(|| contact.as_ref().and_then(|c| c.push_name.clone())),
        about: contact.as_ref().and_then(|c| c.about.clone()).filter(|a| !a.is_empty()),
        picture_path: contact
            .as_ref()
            .and_then(|c| c.avatar_path.as_deref())
            .map(PathBuf::from)
            .filter(|p| p.is_file()),
        jid,
    })
}

fn store_own_contact(sh: &Shared, jid: &str, name: Option<&str>) {
    let patch = ContactPatch {
        jid: jid.to_string(),
        full_name: None,
        push_name: name.map(str::to_string),
        phone: phone_of_jid(jid),
    };
    if let Err(e) = sh.store.upsert_contact(&patch) {
        log::error!("storing the own contact failed: {e}");
    }
}

fn emit_local_profile(sh: &Shared, client: &Client) {
    if let Some(profile) = local_profile(sh, client) {
        sh.emit(UiEvent::OwnProfile(profile));
    }
}

pub(crate) async fn fetch_own_profile(sh: &Shared, client: &Arc<Client>) {
    let (Some(jid), Some(own)) = (own_jid(client), client.pn()) else {
        reject(sh, "Not logged in".to_string());
        return;
    };
    let own = own.to_non_ad();
    store_own_contact(sh, &jid, Some(client.push_name()).filter(|n| !n.is_empty()).as_deref());

    let mut picture_id = sh.store.contact(&jid).ok().flatten().and_then(|c| c.avatar_id);
    let mut has_picture = None;
    if let Some(_slot) = paced_slot(sh).await {
        let result = client.contacts().get_user_info(std::slice::from_ref(&own)).await;
        record_outcome(sh, result.is_ok());
        match result {
            Ok(map) => {
                if let Some(user) = map.into_values().next() {
                    if user.status_error.is_none() {
                        let about = user.status.filter(|s| !s.is_empty());
                        if let Err(e) = sh.store.set_contact_about(&jid, about.as_deref(), None) {
                            log::error!("storing the own about failed: {e}");
                        }
                    }
                    has_picture = Some(user.picture_id.is_some());
                    picture_id = user.picture_id.or(picture_id);
                }
            }
            Err(e) => warn_lib_error("own profile lookup failed", &e),
        }
    } else {
        log::debug!("own profile lookup skipped: requests paused");
    }

    if has_picture == Some(false) {
        if let Err(e) = sh.store.clear_contact_avatar(&jid) {
            log::error!("clearing the own avatar failed: {e}");
        }
    } else {
        match full_picture(sh, client, &own, &jid, picture_id.as_deref()).await {
            Ok(Some(path)) => {
                if let Some(id) = picture_id.as_deref() {
                    if let Err(e) = sh.store.set_contact_avatar(&jid, id, &path.to_string_lossy()) {
                        log::error!("storing the own avatar failed: {e}");
                    }
                }
            }
            Ok(None) => {}
            Err(e) => log::warn!("own picture lookup failed: {e:#}"),
        }
    }
    emit_local_profile(sh, client);
}

pub(crate) async fn set_own_name(sh: &Shared, client: &Arc<Client>, name: &str) {
    let name = match validate_name(name) {
        Ok(n) => n,
        Err(msg) => return reject(sh, msg.to_string()),
    };
    if let Err(e) = client.profile().set_push_name(&name).await {
        warn_lib_error("SetOwnName failed", &e);
        return reject(sh, format!("Could not change the name: {e}"));
    }
    log::info!("SetOwnName: ok");
    if let Some(jid) = own_jid(client) {
        store_own_contact(sh, &jid, Some(&name));
    }
    emit_local_profile(sh, client);
    sh.emit(UiEvent::ChatsChanged);
}

pub(crate) async fn set_own_about(sh: &Shared, client: &Arc<Client>, text: &str) {
    let text = match validate_about(text) {
        Ok(t) => t,
        Err(msg) => return reject(sh, msg.to_string()),
    };
    if let Err(e) = client.profile().set_status_text(&text).await {
        warn_lib_error("SetOwnAbout failed", &e);
        return reject(sh, format!("Could not change the About text: {e}"));
    }
    log::info!("SetOwnAbout: ok");
    if let Some(jid) = own_jid(client) {
        if let Err(e) = sh.store.set_contact_about(&jid, Some(&text), Some(now_secs())) {
            log::error!("storing the own about failed: {e}");
        }
    }
    emit_local_profile(sh, client);
}

pub(crate) async fn set_own_picture(sh: &Shared, client: &Arc<Client>, path: Option<PathBuf>) {
    let Some(jid) = own_jid(client) else {
        return reject(sh, "Not logged in".to_string());
    };
    let Some(path) = path else {
        return remove_own_picture(sh, client, &jid).await;
    };
    let encoded = tokio::task::spawn_blocking(move || {
        read_picture_file(&path).and_then(|data| reencode_profile_picture(&data))
    })
    .await;
    let jpeg = match encoded {
        Ok(Ok(jpeg)) => jpeg,
        Ok(Err(msg)) => return reject(sh, msg),
        Err(e) => {
            log::error!("picture encoding task failed: {e}");
            return reject(sh, "The picture could not be processed".to_string());
        }
    };
    let id = match client.profile().set_profile_picture(jpeg.clone()).await {
        Ok(response) => response.id,
        Err(e) => {
            warn_lib_error("SetOwnPicture failed", &e);
            return reject(sh, format!("Could not change the picture: {e}"));
        }
    };
    log::info!("SetOwnPicture: ok");
    let file = avatar_full_path(&sh.media.dir, &jid, &id);
    match write_atomic_async(file.clone(), jpeg).await {
        Ok(()) => {
            if let Err(e) = sh.store.set_contact_avatar(&jid, &id, &file.to_string_lossy()) {
                log::error!("storing the own avatar failed: {e}");
            }
            if let Err(e) = sh.store.mark_avatar_checked(&jid, now_secs()) {
                log::error!("mark_avatar_checked failed: {e}");
            }
            sh.emit(UiEvent::AvatarReady { jid: jid.clone(), path: file });
        }
        Err(e) => log::error!("caching the own picture failed: {e:#}"),
    }
    emit_local_profile(sh, client);
}

async fn remove_own_picture(sh: &Shared, client: &Arc<Client>, jid: &str) {
    if let Err(e) = client.profile().remove_profile_picture().await {
        warn_lib_error("SetOwnPicture (remove) failed", &e);
        return reject(sh, format!("Could not remove the picture: {e}"));
    }
    log::info!("SetOwnPicture (remove): ok");
    let old = sh.store.contact(jid).ok().flatten().and_then(|c| c.avatar_path);
    if let Err(e) = sh.store.clear_contact_avatar(jid) {
        log::error!("clearing the own avatar failed: {e}");
    }
    if let Some(old) = old {
        remove_cached_file(sh, &old);
    }
    emit_local_profile(sh, client);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marking_unread_shows_a_badge_without_lowering_a_count() {
        assert_eq!(unread_after_mark(0), 1);
        assert_eq!(unread_after_mark(1), 1);
        assert_eq!(unread_after_mark(7), 7);
    }

    #[test]
    fn a_failed_mark_unread_is_reverted_only_while_the_row_is_unchanged() {
        // Still the value we wrote: back to what it was.
        assert_eq!(unread_revert(1, 1, 0), Some(0));
        // A message arrived (or it was read elsewhere) meanwhile: leave it.
        assert_eq!(unread_revert(2, 1, 0), None);
        assert_eq!(unread_revert(0, 1, 0), None);
    }

    #[test]
    fn names_and_about_texts_are_trimmed_and_bounded() {
        assert_eq!(validate_name("  Ada  ").as_deref(), Ok("Ada"));
        assert!(validate_name("   ").is_err());
        assert!(validate_name(&"x".repeat(26)).is_err());
        assert!(validate_name(&"\u{e9}".repeat(25)).is_ok());
        assert_eq!(validate_about(" busy ").as_deref(), Ok("busy"));
        assert!(validate_about("").is_err());
        assert!(validate_about(&"x".repeat(140)).is_err());
    }

    fn encoded(width: u32, height: u32, format: image::ImageFormat) -> Vec<u8> {
        let img = image::RgbImage::from_fn(width, height, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        });
        let mut out = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img).write_to(&mut out, format).unwrap();
        out.into_inner()
    }

    #[test]
    fn pictures_become_square_640_jpegs() {
        for (w, h, format) in [
            (1200, 800, image::ImageFormat::Png),
            (300, 900, image::ImageFormat::Jpeg),
            (64, 64, image::ImageFormat::Png),
        ] {
            let out = reencode_profile_picture(&encoded(w, h, format)).unwrap();
            assert_eq!(image::guess_format(&out).unwrap(), image::ImageFormat::Jpeg);
            let img = image::load_from_memory(&out).unwrap();
            assert_eq!((img.width(), img.height()), (640, 640));
        }
    }

    fn decoded(jpeg: &[u8]) -> image::RgbImage {
        image::load_from_memory(jpeg).unwrap().to_rgb8()
    }

    #[test]
    fn transparent_pixels_become_white_not_black() {
        let img = image::RgbaImage::from_fn(64, 64, |x, _| {
            if x < 32 { image::Rgba([0, 0, 0, 0]) } else { image::Rgba([0, 0, 255, 255]) }
        });
        let mut png = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img).write_to(&mut png, image::ImageFormat::Png).unwrap();
        let out = decoded(&reencode_profile_picture(&png.into_inner()).unwrap());
        let clear = out.get_pixel(100, 320).0;
        assert!(clear.iter().all(|c| *c > 240), "{clear:?}");
        let solid = out.get_pixel(540, 320).0;
        assert!(solid[2] > 200 && solid[0] < 60, "{solid:?}");
    }

    /// Inserts an EXIF APP1 segment carrying `orientation` after the SOI marker.
    fn with_exif_orientation(jpeg: &[u8], orientation: u8) -> Vec<u8> {
        let mut seg = vec![0xFF, 0xE1, 0x00, 34];
        seg.extend_from_slice(b"Exif\0\0");
        seg.extend_from_slice(&[b'M', b'M', 0, 42, 0, 0, 0, 8]);
        seg.extend_from_slice(&[0, 1, 0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, orientation, 0, 0]);
        seg.extend_from_slice(&[0, 0, 0, 0]);
        let mut out = jpeg[..2].to_vec();
        out.extend(seg);
        out.extend_from_slice(&jpeg[2..]);
        out
    }

    #[test]
    fn exif_orientation_is_applied_before_the_crop() {
        // Left half red, right half blue.
        let img = image::RgbImage::from_fn(200, 100, |x, _| {
            if x < 100 { image::Rgb([255, 0, 0]) } else { image::Rgb([0, 0, 255]) }
        });
        let mut jpeg = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img).write_to(&mut jpeg, image::ImageFormat::Jpeg).unwrap();
        let jpeg = jpeg.into_inner();

        let plain = decoded(&reencode_profile_picture(&jpeg).unwrap());
        assert!(plain.get_pixel(100, 320).0[0] > 200, "left stays red");

        // Orientation 6: rotate 90 degrees clockwise, so the left half ends up on top.
        let turned = decoded(&reencode_profile_picture(&with_exif_orientation(&jpeg, 6)).unwrap());
        let top = turned.get_pixel(320, 50).0;
        let bottom = turned.get_pixel(320, 590).0;
        assert!(top[0] > 200 && top[2] < 80, "{top:?}");
        assert!(bottom[2] > 200 && bottom[0] < 80, "{bottom:?}");
    }

    #[test]
    fn a_missing_lid_pn_mapping_gets_an_actionable_message() {
        let e = BlockingError::InvalidJid("no LID\u{2194}PN mapping for provided jid".into());
        assert_eq!(
            block_error_message(true, &e),
            "Can't block this contact yet \u{2014} open a chat with them first and try again"
        );
        let other = BlockingError::InvalidJid("jid is neither PN nor LID".into());
        assert!(block_error_message(false, &other).starts_with("Could not unblock the contact"));
    }

    #[test]
    fn app_state_commands_are_counted_while_they_run() {
        let state = AccountState::default();
        let a = state.begin_app_state();
        let b = state.begin_app_state();
        assert_eq!(state.app_state_inflight.load(Ordering::SeqCst), 2);
        drop(a);
        assert_eq!(state.app_state_inflight.load(Ordering::SeqCst), 1);
        drop(b);
        assert_eq!(state.app_state_inflight.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn undecodable_and_oversized_canvases_are_refused() {
        assert!(reencode_profile_picture(b"not an image").is_err());
        // Within the file-size limit, but a canvas beyond the decoder limit.
        let wide = encoded(MAX_SOURCE_SIDE + 1, 1, image::ImageFormat::Png);
        assert!(reencode_profile_picture(&wide).is_err());
    }

    #[test]
    fn picture_files_over_five_megabytes_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big.bin");
        std::fs::write(&big, vec![0u8; (MAX_PICTURE_BYTES + 1) as usize]).unwrap();
        let err = read_picture_file(&big).unwrap_err();
        assert!(err.contains("5 MB"), "{err}");
        assert!(!err.contains("big.bin"));
        let ok = dir.path().join("ok.bin");
        std::fs::write(&ok, b"abc").unwrap();
        assert_eq!(read_picture_file(&ok).unwrap(), b"abc");
        assert!(read_picture_file(&dir.path().join("missing")).is_err());
        assert!(read_picture_file(dir.path()).is_err());
    }

    #[test]
    fn the_blocklist_cache_is_sorted_deduplicated_and_updated_in_place() {
        let mut cache = BlocklistCache::default();
        assert_eq!(cache.snapshot(), None);
        // Without a known list a change cannot be applied.
        assert_eq!(cache.apply("a@s.whatsapp.net", true), None);
        let list = cache.replace(vec!["b@s.whatsapp.net".into(), "a@s.whatsapp.net".into(), "b@s.whatsapp.net".into()]);
        assert_eq!(list, ["a@s.whatsapp.net", "b@s.whatsapp.net"]);
        assert_eq!(cache.apply("c@s.whatsapp.net", true).unwrap(), ["a@s.whatsapp.net", "b@s.whatsapp.net", "c@s.whatsapp.net"]);
        assert_eq!(cache.apply("a@s.whatsapp.net", false).unwrap(), ["b@s.whatsapp.net", "c@s.whatsapp.net"]);
        // Blocking twice does not duplicate.
        assert_eq!(cache.apply("b@s.whatsapp.net", true).unwrap().len(), 2);
        assert_eq!(cache.snapshot().unwrap().len(), 2);
    }
}
