//! Media cache layout, mime/extension tables, Ogg and thumbnail helpers and the
//! download pipeline. The helpers are pure so they can be unit-tested.

use std::collections::{HashMap, HashSet};
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use sha2::{Digest as _, Sha256};
use tokio::sync::Semaphore;
use whatsapp_rust::buffa::Message as _;
use whatsapp_rust::download::{DownloadWriter, Downloadable};
use whatsapp_rust::features::{MediaReuploadRequest, MediaRetryResult};
use whatsapp_rust::http::HttpStatusError;
use whatsapp_rust::prelude::{Client, Jid, MessageExt as _, wa};
use whatsapp_rust::wacore_binary::JidExt as _;
use wa_store::MessageKind;

use crate::api::UiEvent;
use crate::events::Shared;

/// Largest file we accept for sending.
pub const MAX_SEND_BYTES: u64 = 100 * 1024 * 1024;
/// Sanity bound for downloads the user asked for.
const MANUAL_DOWNLOAD_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_CONCURRENT_DOWNLOADS: usize = 3;
const MAX_CONCURRENT_AVATARS: usize = 2;
const THUMBNAIL_MAX_SIDE: u32 = 100;
const THUMBNAIL_QUALITY: u8 = 60;

/// Process-wide state of the media and avatar pipelines (survives bot
/// generations).
pub struct MediaState {
    pub dir: PathBuf,
    downloads: Semaphore,
    in_flight: Mutex<HashSet<(String, String)>>,
    pub(crate) avatar_permits: Semaphore,
    /// jid -> earliest moment the avatar may be fetched again.
    pub(crate) avatar_next: Mutex<HashMap<String, Instant>>,
    /// jid -> consecutive failed avatar lookups.
    pub(crate) avatar_errors: Mutex<HashMap<String, u32>>,
    /// Earliest start of the next avatar request, shared by all of them.
    pub(crate) avatar_pace: tokio::sync::Mutex<Instant>,
    /// Pauses every avatar request after repeated consecutive failures.
    pub(crate) avatar_breaker: Mutex<crate::avatar::Breaker>,
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl MediaState {
    pub fn new(dir: PathBuf) -> Self {
        MediaState {
            dir,
            downloads: Semaphore::new(MAX_CONCURRENT_DOWNLOADS),
            in_flight: Mutex::new(HashSet::new()),
            avatar_permits: Semaphore::new(MAX_CONCURRENT_AVATARS),
            avatar_next: Mutex::new(HashMap::new()),
            avatar_errors: Mutex::new(HashMap::new()),
            avatar_pace: tokio::sync::Mutex::new(Instant::now()),
            avatar_breaker: Mutex::new(crate::avatar::Breaker::default()),
        }
    }

    /// Forgets all in-memory avatar bookkeeping (logout).
    pub fn clear_avatar_state(&self) {
        lock(&self.avatar_next).clear();
        lock(&self.avatar_errors).clear();
        lock(&self.avatar_breaker).reset();
    }
}

/// Removes the message from the in-flight set when the download ends, however
/// it ends.
struct InFlight<'a> {
    state: &'a MediaState,
    key: (String, String),
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        lock(&self.state.in_flight).remove(&self.key);
    }
}

// ---------------------------------------------------------------- pure helpers

/// First `chars` hex digits of SHA-256 over `parts` joined by NUL. Cache names
/// are derived from this so a chat or message id chosen by a remote party can
/// neither alias another file nor escape the cache directory.
fn hash_hex(parts: &[&str], chars: usize) -> String {
    let mut hasher = Sha256::new();
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            hasher.update([0u8]);
        }
        hasher.update(part.as_bytes());
    }
    let mut out = String::with_capacity(chars + 2);
    for byte in hasher.finalize().iter() {
        if out.len() >= chars {
            break;
        }
        out.push_str(&format!("{byte:02x}"));
    }
    out.truncate(chars);
    out
}

/// `media_dir/<16 hex of sha256(chat)>/<32 hex of sha256(chat NUL id)>.<ext>`
pub fn cache_path(media_dir: &Path, chat_jid: &str, id: &str, ext: &str) -> PathBuf {
    media_dir
        .join(hash_hex(&[chat_jid], 16))
        .join(format!("{}.{}", hash_hex(&[chat_jid, id], 32), ext))
}

/// `media_dir/avatars/<32 hex of sha256(jid)>.jpg`
pub fn avatar_path(media_dir: &Path, jid: &str) -> PathBuf {
    media_dir
        .join("avatars")
        .join(format!("{}.jpg", hash_hex(&[jid], 32)))
}

/// `media_dir/avatars/<32 hex of sha256(jid NUL picture id)>-full.jpg`: the
/// full-resolution picture; the picture id is part of the name so a changed
/// picture never serves a stale file.
pub fn avatar_full_path(media_dir: &Path, jid: &str, picture_id: &str) -> PathBuf {
    media_dir
        .join("avatars")
        .join(format!("{}-full.jpg", hash_hex(&[jid, picture_id], 32)))
}

/// Extensions Windows (or a script host) would run on double click.
const DANGEROUS_EXTENSIONS: [&str; 32] = [
    "exe", "com", "scr", "bat", "cmd", "ps1", "psm1", "vbs", "vbe", "js", "jse", "wsf", "wsh",
    "hta", "msi", "msp", "lnk", "url", "reg", "jar", "cpl", "dll", "pif", "scf", "application",
    "appref-ms", "msix", "appx", "iso", "vhd", "vhdx", "chm",
];

/// Appends `_` to an executable extension so the cached file is inert but the
/// user can still rename it if they really want the file.
fn neutralize_ext(ext: String) -> String {
    if DANGEROUS_EXTENSIONS.contains(&ext.as_str()) {
        format!("{ext}_")
    } else {
        ext
    }
}

fn clean_ext(ext: &str) -> Option<String> {
    let ext = ext.trim().to_ascii_lowercase();
    let ok = !ext.is_empty() && ext.len() <= 10 && ext.chars().all(|c| c.is_ascii_alphanumeric());
    ok.then_some(ext)
}

fn file_name_ext(name: &str) -> Option<String> {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .and_then(clean_ext)
}

fn ext_from_mime(mime: &str) -> Option<&'static str> {
    let base = mime.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    if base.starts_with("audio/ogg") {
        return Some("ogg");
    }
    Some(match base.as_str() {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "video/mp4" => "mp4",
        "audio/mpeg" => "mp3",
        "audio/mp4" => "m4a",
        "application/pdf" => "pdf",
        _ => return None,
    })
}

/// File extension for a cached media file; never an executable one.
pub fn extension_for(kind: MessageKind, mime: Option<&str>, file_name: Option<&str>) -> String {
    neutralize_ext(raw_extension_for(kind, mime, file_name))
}

fn raw_extension_for(kind: MessageKind, mime: Option<&str>, file_name: Option<&str>) -> String {
    if kind == MessageKind::Document {
        if let Some(ext) = file_name.and_then(file_name_ext) {
            return ext;
        }
        return mime.and_then(ext_from_mime).unwrap_or("bin").to_string();
    }
    if let Some(ext) = mime.and_then(ext_from_mime) {
        return ext.to_string();
    }
    match kind {
        MessageKind::Image => "jpg",
        MessageKind::Sticker => "webp",
        MessageKind::Video => "mp4",
        MessageKind::Voice => "ogg",
        MessageKind::Audio => "m4a",
        _ => "bin",
    }
    .to_string()
}

/// Mime type from a file extension; `None` when unknown.
pub fn mime_for_path(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "3gp" => "video/3gpp",
        "avi" => "video/x-msvideo",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "wav" => "audio/wav",
        "pdf" => "application/pdf",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "txt" => "text/plain",
        "csv" => "text/csv",
        "json" => "application/json",
        "zip" => "application/zip",
        "rar" => "application/vnd.rar",
        "7z" => "application/x-7z-compressed",
        _ => return None,
    })
}

/// Duration in whole seconds of an Ogg/Opus stream, from the granule position
/// of its last page (Opus granules tick at 48 kHz).
pub fn ogg_opus_duration_secs(data: &[u8]) -> Option<u32> {
    if !data.starts_with(b"OggS") {
        return None;
    }
    let mut end = data.len();
    while let Some(pos) = data[..end].windows(4).rposition(|w| w == b"OggS") {
        // Page header: "OggS", version(0), flags, granule u64 LE at offset 6.
        if let (Some(0), Some(raw)) = (data.get(pos + 4), data.get(pos + 6..pos + 14)) {
            let mut le = [0u8; 8];
            le.copy_from_slice(raw);
            let granule = u64::from_le_bytes(le);
            // All ones means "no packet ends on this page".
            if granule != u64::MAX {
                let secs = granule.saturating_add(24_000) / 48_000;
                return u32::try_from(secs).ok();
            }
        }
        end = pos;
    }
    None
}

/// Decodes an image and returns `(width, height, jpeg_thumbnail)`.
pub fn image_info(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let img = image::load_from_memory(data)
        .map_err(|e| log::debug!("image does not decode: {e}"))
        .ok()?;
    let (width, height) = (img.width(), img.height());
    let thumb = img.thumbnail(THUMBNAIL_MAX_SIDE, THUMBNAIL_MAX_SIDE).to_rgb8();
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, THUMBNAIL_QUALITY)
        .encode_image(&thumb)
        .map_err(|e| log::debug!("thumbnail encoding failed: {e}"))
        .ok()?;
    Some((width, height, out))
}

/// Mime type of an image from its content.
pub fn sniff_image_mime(data: &[u8]) -> Option<&'static str> {
    match image::guess_format(data).ok()? {
        image::ImageFormat::Jpeg => Some("image/jpeg"),
        image::ImageFormat::Png => Some("image/png"),
        image::ImageFormat::WebP => Some("image/webp"),
        _ => None,
    }
}

/// Writes `data` next to `path` and renames it into place so readers never see
/// a partial file.
pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(".part");
    let tmp = PathBuf::from(tmp_name);
    let result = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

pub async fn write_atomic_async(path: PathBuf, data: Vec<u8>) -> anyhow::Result<()> {
    tokio::task::spawn_blocking(move || write_atomic(&path, &data))
        .await
        .map_err(|e| anyhow::anyhow!("write task failed: {e}"))?
        .map_err(Into::into)
}

// -------------------------------------------------------------------- download

/// The downloadable part of a stored message (cloned so that a reupload can
/// swap its `direct_path`).
enum Target {
    Image(wa::message::ImageMessage),
    Video(wa::message::VideoMessage),
    Audio(wa::message::AudioMessage),
    Document(wa::message::DocumentMessage),
    Sticker(wa::message::StickerMessage),
}

impl Target {
    fn pick(base: &wa::Message) -> Option<Target> {
        if let Some(m) = base.image_message.as_option() {
            return Some(Target::Image(m.clone()));
        }
        if let Some(m) = base.video_message.as_option() {
            return Some(Target::Video(m.clone()));
        }
        if let Some(m) = base.ptv_message.as_option() {
            return Some(Target::Video(m.clone()));
        }
        if let Some(m) = base.audio_message.as_option() {
            return Some(Target::Audio(m.clone()));
        }
        if let Some(m) = base.document_message.as_option() {
            return Some(Target::Document(m.clone()));
        }
        base.sticker_message
            .as_option()
            .map(|m| Target::Sticker(m.clone()))
    }

    fn downloadable(&self) -> &dyn Downloadable {
        match self {
            Target::Image(m) => m,
            Target::Video(m) => m,
            Target::Audio(m) => m,
            Target::Document(m) => m,
            Target::Sticker(m) => m,
        }
    }

    fn set_direct_path(&mut self, path: String) {
        let slot = match self {
            Target::Image(m) => &mut m.direct_path,
            Target::Video(m) => &mut m.direct_path,
            Target::Audio(m) => &mut m.direct_path,
            Target::Document(m) => &mut m.direct_path,
            Target::Sticker(m) => &mut m.direct_path,
        };
        *slot = Some(path);
    }
}

/// Sink for a streaming download that refuses to take more than `cap` bytes.
///
/// The library hands it *decrypted* bytes as soon as each block is decrypted
/// (the MAC / sha256 is verified only at the end), so the cap bounds plaintext
/// and a sender lying about `media_size` cannot make us store more than `cap`.
/// A write error aborts the attempt; the library then truncates the sink and
/// tries the next CDN host (each attempt is bounded by the cap again).
///
/// The byte count follows the position: the library truncates to 0 and rewinds
/// before every attempt, which restarts the count.
struct CappedWriter<W> {
    inner: W,
    cap: u64,
    written: u64,
    exceeded: Arc<AtomicBool>,
}

impl<W> CappedWriter<W> {
    fn new(inner: W, cap: u64, exceeded: Arc<AtomicBool>) -> Self {
        CappedWriter {
            inner,
            cap,
            written: 0,
            exceeded,
        }
    }
}

impl<W: Write> Write for CappedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let len = u64::try_from(buf.len()).unwrap_or(u64::MAX);
        if self.written.saturating_add(len) > self.cap {
            self.exceeded.store(true, Ordering::Relaxed);
            return Err(std::io::Error::other("download exceeds the size limit"));
        }
        let n = self.inner.write(buf)?;
        self.written += u64::try_from(n).unwrap_or(0);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl<W: Seek> Seek for CappedWriter<W> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let new = self.inner.seek(pos)?;
        self.written = new;
        Ok(new)
    }
}

impl<W: DownloadWriter> DownloadWriter for CappedWriter<W> {
    fn truncate(&mut self, len: u64) -> std::io::Result<()> {
        self.inner.truncate(len)?;
        self.written = self.written.min(len);
        Ok(())
    }
}

/// Streams the media into `part` (created or emptied first), at most `cap`
/// bytes, and syncs it. The caller renames or deletes `part`.
async fn download_into(
    client: &Client,
    downloadable: &dyn Downloadable,
    part: &Path,
    cap: u64,
    exceeded: &Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let file = {
        let part = part.to_path_buf();
        tokio::task::spawn_blocking(move || {
            if let Some(parent) = part.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::File::create(&part)
        })
        .await
        .map_err(|e| anyhow::anyhow!("file task failed: {e}"))??
    };
    // Block-sized writes from the decryptor would otherwise be one syscall each.
    let writer = CappedWriter::new(BufWriter::new(file), cap, exceeded.clone());
    let mut writer = match client.download_to_writer(downloadable, writer).await {
        Ok(w) => w,
        Err(e) if exceeded.load(Ordering::Relaxed) => {
            log::debug!("download aborted at the size limit: {e:#}");
            anyhow::bail!("The file is too large to download");
        }
        Err(e) => return Err(e),
    };
    tokio::task::spawn_blocking(move || {
        writer.flush()?;
        writer.inner.get_ref().sync_all()
    })
    .await
    .map_err(|e| anyhow::anyhow!("file task failed: {e}"))?
    .map_err(Into::into)
}

/// Whether the CDN says the file is gone, which a reupload request can fix.
/// 403 counts: the CDN answers it for an expired or rotated signed URL
/// (the `oh`/`oe` parameters), exactly like 404/410.
fn is_expired(e: &anyhow::Error) -> bool {
    e.chain()
        .find_map(|c| c.downcast_ref::<HttpStatusError>())
        .is_some_and(|s| matches!(s.status, 403 | 404 | 410))
}

/// Downloads the media of a stored message into the cache and reports the
/// result to the UI. Concurrent requests for the same message collapse into one.
/// Automatic downloads (nobody asked for them) pass `auto_cap` and are limited
/// to that many bytes of actual downloaded data, whatever the sender claimed
/// in the message; `None` is a user request.
pub async fn download(
    sh: Shared,
    client: std::sync::Arc<Client>,
    chat_jid: String,
    id: String,
    auto_cap: Option<u64>,
) {
    let existing = match sh.store.message(&chat_jid, &id) {
        Ok(Some(row)) => row.local_path,
        Ok(None) => {
            fail(&sh, chat_jid, id, "Message not found");
            return;
        }
        Err(e) => {
            log::error!("message lookup failed: {e}");
            fail(&sh, chat_jid, id, "Could not read the message");
            return;
        }
    };
    if let Some(path) = existing.map(PathBuf::from).filter(|p| p.is_file()) {
        sh.emit(UiEvent::MediaReady { chat_jid, id, path });
        return;
    }

    let key = (chat_jid.clone(), id.clone());
    if !lock(&sh.media.in_flight).insert(key.clone()) {
        log::debug!("download of {id} already in flight");
        return;
    }
    let _in_flight = InFlight {
        state: &sh.media,
        key,
    };
    let Ok(_permit) = sh.media.downloads.acquire().await else {
        log::error!("download semaphore closed");
        return;
    };

    match fetch_to_cache(&sh, &client, &chat_jid, &id, auto_cap).await {
        Ok(path) => {
            if let Err(e) = sh.store.set_local_path(&chat_jid, &id, &path.to_string_lossy()) {
                log::error!("set_local_path failed: {e}");
            }
            sh.emit(UiEvent::MediaReady {
                chat_jid: chat_jid.clone(),
                id,
                path,
            });
            sh.emit(UiEvent::MessagesChanged { chat_jid });
        }
        Err(e) => {
            log::warn!("media download of {id} failed: {e:#}");
            fail(&sh, chat_jid, id, &short_error(&e));
        }
    }
}

fn fail(sh: &Shared, chat_jid: String, id: String, error: &str) {
    sh.emit(UiEvent::MediaFailed {
        chat_jid,
        id,
        error: error.to_string(),
    });
}

fn short_error(e: &anyhow::Error) -> String {
    if is_expired(e) {
        return "The media is no longer available".to_string();
    }
    let msg = e.to_string();
    match msg.char_indices().nth(120) {
        Some((i, _)) => format!("{}…", &msg[..i]),
        None => msg,
    }
}

async fn fetch_to_cache(
    sh: &Shared,
    client: &Client,
    chat_jid: &str,
    id: &str,
    auto_cap: Option<u64>,
) -> anyhow::Result<PathBuf> {
    let row = sh
        .store
        .message(chat_jid, id)?
        .ok_or_else(|| anyhow::anyhow!("Message not found"))?;
    let raw = sh
        .store
        .message_raw(chat_jid, id)?
        .ok_or_else(|| anyhow::anyhow!("The original message data is not available"))?;
    let msg = wa::Message::decode_from_slice(&raw)
        .map_err(|e| anyhow::anyhow!("stored message does not decode: {e}"))?;
    // Rows stored before view-once media became a placeholder may still hold keys.
    if crate::convert::is_view_once(&msg) {
        anyhow::bail!("View once media can only be opened on your phone");
    }
    let mut target = Target::pick(msg.get_base_message())
        .ok_or_else(|| anyhow::anyhow!("This message has no downloadable media"))?;

    let ext = extension_for(row.kind, row.media_mime.as_deref(), row.media_name.as_deref());
    let path = cache_path(&sh.media.dir, chat_jid, id, &ext);
    let mut part_name = path.as_os_str().to_owned();
    part_name.push(".part");
    let part = PathBuf::from(part_name);
    // Enforced while streaming: `media_size` in the message is the sender's claim.
    let cap = auto_cap.unwrap_or(MANUAL_DOWNLOAD_MAX_BYTES);
    let exceeded = Arc::new(AtomicBool::new(false));

    let mut result = download_into(client, target.downloadable(), &part, cap, &exceeded).await;
    if let Err(e) = &result
        && is_expired(e)
    {
        log::info!("media URL of {id} expired, requesting reupload");
        result = match reupload(client, &row, id, chat_jid, &target).await {
            Ok(direct_path) => {
                target.set_direct_path(direct_path);
                download_into(client, target.downloadable(), &part, cap, &exceeded).await
            }
            Err(e) => Err(e),
        };
    }
    let result = match result {
        Ok(()) => {
            let (from, to) = (part.clone(), path.clone());
            tokio::task::spawn_blocking(move || std::fs::rename(from, to))
                .await
                .map_err(|e| anyhow::anyhow!("file task failed: {e}"))
                .and_then(|r| r.map_err(Into::into))
        }
        Err(e) => Err(e),
    };
    if let Err(e) = result {
        if let Err(rm) = std::fs::remove_file(&part)
            && rm.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!("could not remove partial download: {rm}");
        }
        return Err(e);
    }
    Ok(path)
}

async fn reupload(
    client: &Client,
    row: &wa_store::MessageRow,
    id: &str,
    chat_jid: &str,
    target: &Target,
) -> anyhow::Result<String> {
    let media_key = target
        .downloadable()
        .media_key()
        .ok_or_else(|| anyhow::anyhow!("The media cannot be re-requested"))?
        .to_vec();
    let chat: Jid = chat_jid
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid chat address: {e}"))?;
    // The poster of a status is the participant, as in a group.
    let participant = (chat.is_group() || chat.is_status_broadcast())
        .then(|| row.sender_jid.parse::<Jid>().ok())
        .flatten();
    let request = MediaReuploadRequest {
        msg_id: id,
        chat_jid: &chat,
        media_key: &media_key,
        is_from_me: row.from_me,
        participant: participant.as_ref(),
    };
    match client.media_reupload().request(&request).await {
        Ok(MediaRetryResult::Success { direct_path }) => Ok(direct_path),
        Ok(MediaRetryResult::NotFound) => {
            Err(anyhow::anyhow!("The media is no longer available on the phone"))
        }
        Ok(other) => Err(anyhow::anyhow!("The phone could not re-upload the media ({other:?})")),
        Err(e) => Err(anyhow::anyhow!("Media re-upload request failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotated_or_expired_cdn_urls_are_reuploadable() {
        let http = |status| anyhow::Error::new(HttpStatusError { status }).context("Download failed");
        for status in [403, 404, 410] {
            assert!(is_expired(&http(status)), "{status} means the URL is no longer valid");
        }
        for status in [400, 401, 429, 500, 503] {
            assert!(!is_expired(&http(status)), "{status} is not a reupload case");
        }
        assert!(!is_expired(&anyhow::anyhow!("connection reset")));
    }

    #[test]
    fn hash_is_stable_hex_prefix() {
        // sha256("abc")
        assert_eq!(hash_hex(&["abc"], 32), "ba7816bf8f01cfea414140de5dae2223");
        assert_eq!(hash_hex(&["abc"], 16), "ba7816bf8f01cfea");
        // The NUL separator keeps ("a", "bc") and ("ab", "c") apart.
        assert_ne!(hash_hex(&["a", "bc"], 32), hash_hex(&["ab", "c"], 32));
    }

    #[test]
    fn cache_paths_are_hashed_and_confined() {
        let dir = Path::new("media");
        let p = cache_path(dir, "g1@g.us", "ABC", "jpg");
        assert_eq!(p.parent(), Some(dir.join(hash_hex(&["g1@g.us"], 16)).as_path()));
        let name = p.file_name().and_then(|n| n.to_str()).expect("name");
        assert_eq!(name, format!("{}.jpg", hash_hex(&["g1@g.us", "ABC"], 32)));

        // Hostile ids cannot escape the chat directory or collide with others.
        let evil = cache_path(dir, "g1@g.us", "../../x\\y", "jpg");
        assert_eq!(evil.parent(), p.parent());
        assert_ne!(evil, p);
        assert_ne!(
            cache_path(dir, "g1@g.us", "a", "jpg"),
            cache_path(dir, "g2@g.us", "a", "jpg")
        );

        let av = avatar_path(dir, "5@s.whatsapp.net");
        assert_eq!(
            av,
            dir.join("avatars")
                .join(format!("{}.jpg", hash_hex(&["5@s.whatsapp.net"], 32)))
        );
    }

    #[test]
    fn dangerous_extensions_are_neutralized() {
        use MessageKind::Document;
        for ext in DANGEROUS_EXTENSIONS {
            assert_eq!(neutralize_ext(ext.to_string()), format!("{ext}_"), "{ext}");
            // Names whose extension clean_ext rejects fall back to "bin".
            let got = extension_for(Document, None, Some(&format!("setup.{ext}")));
            assert!(got == format!("{ext}_") || got == "bin", "{ext}: {got}");
        }
        assert_eq!(extension_for(Document, None, Some("run.EXE")), "exe_");
        assert_eq!(extension_for(Document, None, Some("a.pdf")), "pdf");
    }

    #[test]
    fn capped_writer_stops_at_the_limit() {
        let flag = Arc::new(AtomicBool::new(false));
        let mut w = CappedWriter::new(std::io::Cursor::new(Vec::new()), 10, flag.clone());
        w.write_all(b"12345").expect("within cap");
        w.write_all(b"67890").expect("exactly at cap");
        assert!(!flag.load(Ordering::Relaxed));
        assert!(w.write_all(b"1").is_err());
        assert!(flag.load(Ordering::Relaxed));
        assert_eq!(w.inner.get_ref().len(), 10);
    }

    #[test]
    fn capped_writer_rejects_an_oversized_single_write() {
        let flag = Arc::new(AtomicBool::new(false));
        let mut w = CappedWriter::new(std::io::Cursor::new(Vec::new()), 4, flag.clone());
        assert!(w.write_all(b"12345").is_err());
        assert!(flag.load(Ordering::Relaxed));
        assert!(w.inner.get_ref().is_empty());
    }

    #[test]
    fn capped_writer_restarts_the_count_on_retry() {
        let flag = Arc::new(AtomicBool::new(false));
        let mut w = CappedWriter::new(std::io::Cursor::new(Vec::new()), 8, flag.clone());
        w.write_all(b"12345678").expect("fills the cap");
        // What the library does before every attempt.
        w.truncate(0).expect("truncate");
        w.rewind().expect("rewind");
        w.write_all(b"abcdefgh").expect("fresh attempt fits again");
        assert!(!flag.load(Ordering::Relaxed));
        assert_eq!(w.inner.get_ref().as_slice(), b"abcdefgh");
    }

    #[test]
    fn extension_mapping() {
        use MessageKind::*;
        assert_eq!(extension_for(Image, Some("image/jpeg"), None), "jpg");
        assert_eq!(extension_for(Image, Some("image/png"), None), "png");
        assert_eq!(extension_for(Sticker, Some("image/webp"), None), "webp");
        assert_eq!(extension_for(Video, Some("video/mp4"), None), "mp4");
        assert_eq!(extension_for(Voice, Some("audio/ogg; codecs=opus"), None), "ogg");
        assert_eq!(extension_for(Audio, Some("audio/mpeg"), None), "mp3");
        assert_eq!(extension_for(Audio, Some("audio/mp4"), None), "m4a");
        assert_eq!(extension_for(Document, Some("application/pdf"), None), "pdf");
        assert_eq!(extension_for(Document, Some("application/pdf"), Some("Report.DOCX")), "docx");
        assert_eq!(extension_for(Document, None, Some("noext")), "bin");
        assert_eq!(extension_for(Document, Some("application/x-unknown"), None), "bin");
        assert_eq!(extension_for(Image, None, None), "jpg");
        assert_eq!(extension_for(Document, None, Some("evil.a/../b")), "bin");
    }

    #[test]
    fn mime_detection() {
        assert_eq!(mime_for_path(Path::new("a/b/photo.JPG")), Some("image/jpeg"));
        assert_eq!(mime_for_path(Path::new("x.mp4")), Some("video/mp4"));
        assert_eq!(mime_for_path(Path::new("x.opus")), Some("audio/ogg"));
        assert_eq!(mime_for_path(Path::new("x.pdf")), Some("application/pdf"));
        assert_eq!(mime_for_path(Path::new("x.unknownext")), None);
        assert_eq!(mime_for_path(Path::new("noext")), None);
    }

    fn ogg_page(granule: u64) -> Vec<u8> {
        let mut p = b"OggS".to_vec();
        p.push(0); // version
        p.push(0); // flags
        p.extend_from_slice(&granule.to_le_bytes());
        p.extend_from_slice(&[0u8; 12]); // serial, sequence, crc
        p.push(0); // zero segments
        p
    }

    #[test]
    fn ogg_duration_uses_last_page() {
        let mut data = ogg_page(0);
        data.extend_from_slice(&ogg_page(48_000));
        data.extend_from_slice(&ogg_page(48_000 * 7 + 100));
        assert_eq!(ogg_opus_duration_secs(&data), Some(7));
    }

    #[test]
    fn ogg_duration_skips_pages_without_granule() {
        let mut data = ogg_page(48_000 * 3);
        data.extend_from_slice(&ogg_page(u64::MAX));
        assert_eq!(ogg_opus_duration_secs(&data), Some(3));
    }

    #[test]
    fn ogg_duration_rejects_non_ogg() {
        assert_eq!(ogg_opus_duration_secs(b"ID3\x03not ogg"), None);
        assert_eq!(ogg_opus_duration_secs(b""), None);
        assert_eq!(ogg_opus_duration_secs(b"OggS\0\0"), None);
    }

    fn png_bytes(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(w, h, image::Rgb([200, 30, 30]));
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).expect("png encodes");
        out.into_inner()
    }

    #[test]
    fn thumbnail_is_small_jpeg() {
        let png = png_bytes(400, 200);
        let (w, h, thumb) = image_info(&png).expect("decodes");
        assert_eq!((w, h), (400, 200));
        assert_eq!(image::guess_format(&thumb).expect("format"), image::ImageFormat::Jpeg);
        let decoded = image::load_from_memory(&thumb).expect("thumbnail decodes");
        assert!(decoded.width() <= 100 && decoded.height() <= 100);
        assert_eq!(decoded.width(), 100);
        assert_eq!(sniff_image_mime(&png), Some("image/png"));
    }

    #[test]
    fn undecodable_image_is_rejected() {
        assert!(image_info(b"definitely not an image").is_none());
        assert_eq!(sniff_image_mime(b"definitely not an image"), None);
    }

    #[test]
    fn atomic_write_creates_dirs_and_replaces() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = cache_path(dir.path(), "1@s.whatsapp.net", "ID", "bin");
        write_atomic(&path, b"one").expect("first write");
        write_atomic(&path, b"two").expect("overwrite");
        assert_eq!(std::fs::read(&path).expect("read"), b"two");
        let mut part = path.as_os_str().to_owned();
        part.push(".part");
        assert!(!PathBuf::from(part).exists());
    }
}
