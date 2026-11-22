//! Sending a sticker that exists as a stored message (received or sent
//! before). The stored upload is reused when it is recent enough; otherwise
//! the cached WebP is uploaded again. The pure parts are separate functions so
//! they can be tested without a client.

use std::sync::Arc;

use whatsapp_rust::buffa::{Message as _, MessageField};
use whatsapp_rust::download::MediaType;
use whatsapp_rust::prelude::{Client, MessageExt as _, wa};
use whatsapp_rust::upload::{UploadOptions, UploadResponse};
use wa_store::{MessageKind, MessageRow, MessageStatus, NewMessage};

use crate::commands::{
    NOT_LOGGED_IN, deliver, finish, parse_jid, reject, remove_cached_file, resolve_quote, store_pending,
};
use crate::events::{Shared, now_secs, own_jid};
use crate::jid::warn_lib_error;
use crate::media as cache;
use crate::msgops;

const STICKER_MIME: &str = "image/webp";

/// What a fresh upload cannot tell us about the sticker.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Facts {
    pub(crate) dims: Option<(u32, u32)>,
    pub(crate) animated: bool,
}

/// Whether the WebP in `data` is animated (extended format with the animation flag).
pub(crate) fn is_animated_webp(data: &[u8]) -> bool {
    data.len() > 20
        && data.starts_with(b"RIFF")
        && &data[8..12] == b"WEBP"
        && &data[12..16] == b"VP8X"
        && data[20] & 0x02 != 0
}

/// The stored sticker sent again as is: same upload (url, keys, hashes), a new
/// context (the quote, if any). `None` when the message is not a sticker or
/// its upload data is incomplete. The message secret of the original is not
/// carried over.
pub(crate) fn sticker_from_stored(base: &wa::Message, context: Option<wa::ContextInfo>) -> Option<wa::Message> {
    let mut sticker = base.sticker_message.as_option()?.clone();
    if !msgops::media_reusable(base) {
        return None;
    }
    sticker.context_info = context.map(MessageField::some).unwrap_or_default();
    Some(wa::Message { sticker_message: MessageField::some(sticker), ..Default::default() })
}

/// The fields of an upload result a sticker message needs (`UploadResponse`
/// cannot be built outside the library, so tests use this).
pub(crate) struct Uploaded {
    pub(crate) url: String,
    pub(crate) direct_path: String,
    pub(crate) media_key: [u8; 32],
    pub(crate) file_enc_sha256: [u8; 32],
    pub(crate) file_sha256: [u8; 32],
    pub(crate) file_length: u64,
    pub(crate) media_key_timestamp: i64,
}

impl From<UploadResponse> for Uploaded {
    fn from(u: UploadResponse) -> Self {
        Uploaded {
            url: u.url,
            direct_path: u.direct_path,
            media_key: u.media_key,
            file_enc_sha256: u.file_enc_sha256,
            file_sha256: u.file_sha256,
            file_length: u.file_length,
            media_key_timestamp: u.media_key_timestamp,
        }
    }
}

/// A sticker message for a fresh upload of a WebP file.
pub(crate) fn sticker_from_upload(
    upload: Uploaded,
    facts: Facts,
    context: Option<wa::ContextInfo>,
) -> wa::Message {
    let sticker = wa::message::StickerMessage {
        url: Some(upload.url),
        direct_path: Some(upload.direct_path),
        media_key: Some(upload.media_key.to_vec()),
        file_sha256: Some(upload.file_sha256.to_vec()),
        file_enc_sha256: Some(upload.file_enc_sha256.to_vec()),
        file_length: Some(upload.file_length),
        media_key_timestamp: Some(upload.media_key_timestamp),
        mimetype: Some(STICKER_MIME.to_string()),
        width: facts.dims.map(|d| d.0),
        height: facts.dims.map(|d| d.1),
        is_animated: Some(facts.animated),
        context_info: context.map(MessageField::some).unwrap_or_default(),
        ..Default::default()
    };
    wa::Message { sticker_message: MessageField::some(sticker), ..Default::default() }
}

/// Sends the sticker of `source_chat`/`source_id` to `chat_jid`.
pub(crate) async fn send(
    sh: &Shared,
    client: &Arc<Client>,
    chat_jid: &str,
    source_chat: &str,
    source_id: &str,
    quoted_id: Option<&str>,
) {
    let Some(chat) = parse_jid(sh, chat_jid) else {
        return;
    };
    let Some(own) = own_jid(client) else {
        reject(sh, NOT_LOGGED_IN.to_string());
        return;
    };
    let row = match sh.store.message(source_chat, source_id) {
        Ok(Some(row)) if row.kind == MessageKind::Sticker && !row.revoked => row,
        Ok(_) => {
            reject(sh, "The sticker was not found".to_string());
            return;
        }
        Err(e) => {
            log::error!("sticker lookup failed: {e}");
            reject(sh, "Could not read the sticker".to_string());
            return;
        }
    };
    let stored = msgops::load_stored(&sh.store, source_chat, source_id);
    let base = stored.as_ref().map(|m| m.get_base_message());
    if base.is_some_and(crate::convert::is_view_once) {
        reject(sh, "View once media can only be opened on your phone".to_string());
        return;
    }

    let quote = resolve_quote(sh, &chat, chat_jid, quoted_id);
    let fresh_enough = msgops::upload_fresh(base, row.ts, now_secs());
    let reused = base
        .filter(|_| fresh_enough)
        .and_then(|b| sticker_from_stored(b, quote.context.clone()));

    let id = client.generate_message_id();
    let local_path = msgops::copy_cached_media(sh, &row, chat_jid, &id).await;
    let pending = NewMessage {
        chat_jid: chat_jid.to_string(),
        id: id.clone(),
        sender_jid: own,
        from_me: true,
        ts: now_secs(),
        kind: MessageKind::Sticker,
        quoted_id: quote.id.clone(),
        quoted_preview: quote.preview.clone(),
        media_mime: Some(STICKER_MIME.to_string()),
        media_size: row.media_size,
        thumbnail: row.thumbnail.clone(),
        local_path,
        raw: reused.as_ref().map(|m| m.encode_to_vec()),
        ..Default::default()
    };
    if !store_pending(sh, &pending) {
        if let Some(path) = &pending.local_path {
            remove_cached_file(sh, path);
        }
        return;
    }

    let message = match reused {
        Some(m) => m,
        None => {
            let known = base.and_then(|b| b.sticker_message.as_option());
            let facts = known.map(|s| Facts { dims: s.width.zip(s.height), animated: s.is_animated.unwrap_or(false) });
            match reupload(sh, client, &row, facts, quote.context).await {
                Ok(m) => {
                    if let Err(e) = sh.store.set_raw(chat_jid, &id, &m.encode_to_vec()) {
                        log::error!("storing raw sticker message failed: {e}");
                    }
                    m
                }
                Err(msg) => {
                    reject(sh, msg);
                    finish(sh, chat_jid, id, MessageStatus::Failed);
                    return;
                }
            }
        }
    };
    deliver(sh, client, chat, chat_jid, id, message).await;
}

/// Uploads the cached WebP of `row` again (downloading it first when needed).
async fn reupload(
    sh: &Shared,
    client: &Arc<Client>,
    row: &MessageRow,
    known: Option<Facts>,
    context: Option<wa::ContextInfo>,
) -> Result<wa::Message, String> {
    let path = msgops::local_media_file(sh, client, row).await?;
    let job = move || -> Result<(Vec<u8>, Facts), String> {
        let data = msgops::read_capped(&path)?;
        if cache::sniff_image_mime(&data) != Some(STICKER_MIME) {
            return Err("Only WebP stickers can be sent".to_string());
        }
        let facts = Facts {
            dims: known.and_then(|f| f.dims).or_else(|| cache::image_info(&data).map(|(w, h, _)| (w, h))),
            animated: known.map_or_else(|| is_animated_webp(&data), |f| f.animated),
        };
        Ok((data, facts))
    };
    let (data, facts) = tokio::task::spawn_blocking(job).await.map_err(|e| {
        log::error!("sticker read task failed: {e}");
        "Could not read the sticker".to_string()
    })??;
    let upload = client.upload(data, MediaType::Sticker, UploadOptions::new()).await.map_err(|e| {
        warn_lib_error("uploading a sticker failed", &e);
        format!("Upload failed: {e}")
    })?;
    Ok(sticker_from_upload(upload.into(), facts, context))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored_sticker() -> wa::Message {
        wa::Message {
            sticker_message: MessageField::some(wa::message::StickerMessage {
                url: Some("https://mmg.example/s".into()),
                direct_path: Some("/v/s".into()),
                media_key: Some(vec![1; 32]),
                file_sha256: Some(vec![2; 32]),
                file_enc_sha256: Some(vec![3; 32]),
                file_length: Some(1234),
                mimetype: Some("image/webp".into()),
                width: Some(512),
                height: Some(512),
                is_animated: Some(true),
                context_info: MessageField::some(wa::ContextInfo {
                    is_forwarded: Some(true),
                    stanza_id: Some("OLD".into()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            message_context_info: MessageField::some(Default::default()),
            ..Default::default()
        }
    }

    #[test]
    fn stored_upload_is_reused_with_a_new_context() {
        let quote = wa::ContextInfo { stanza_id: Some("Q1".into()), ..Default::default() };
        let msg = sticker_from_stored(&stored_sticker(), Some(quote)).expect("reusable");
        let st = msg.sticker_message.as_option().expect("sticker");
        assert_eq!(st.url.as_deref(), Some("https://mmg.example/s"));
        assert_eq!(st.direct_path.as_deref(), Some("/v/s"));
        assert_eq!(st.media_key.as_deref(), Some(&[1u8; 32][..]));
        assert_eq!(st.file_sha256.as_deref(), Some(&[2u8; 32][..]));
        assert_eq!(st.file_enc_sha256.as_deref(), Some(&[3u8; 32][..]));
        assert_eq!((st.width, st.height, st.is_animated), (Some(512), Some(512), Some(true)));
        let ctx = st.context_info.as_option().expect("context");
        assert_eq!(ctx.stanza_id.as_deref(), Some("Q1"));
        assert_eq!(ctx.is_forwarded, None);
        assert!(msg.message_context_info.as_option().is_none());
    }

    #[test]
    fn sticker_reuse_age_follows_the_upload_time_and_survives_reuse() {
        let now = 1_700_000_000;
        let max = msgops::MEDIA_REUSE_MAX_AGE_SECS;
        let mut m = stored_sticker();
        let set_ts = |m: &mut wa::Message, ts| {
            if let Some(s) = m.sticker_message.as_option_mut() {
                s.media_key_timestamp = ts;
            }
        };
        set_ts(&mut m, Some(now - max - 1));
        assert!(!msgops::upload_fresh(Some(&m), now - 5, now), "stale upload, new row");
        set_ts(&mut m, Some(now - 100));
        assert!(msgops::upload_fresh(Some(&m), now - max - 1, now), "fresh upload, old row");
        set_ts(&mut m, None);
        assert!(msgops::upload_fresh(Some(&m), now - 100, now));
        assert!(!msgops::upload_fresh(Some(&m), now - max - 1, now), "row time fallback");
        set_ts(&mut m, Some(42));
        let copy = sticker_from_stored(&m, None).expect("reusable");
        assert_eq!(copy.sticker_message.as_option().and_then(|s| s.media_key_timestamp), Some(42));
    }

    #[test]
    fn reuse_without_a_quote_has_no_context() {
        let msg = sticker_from_stored(&stored_sticker(), None).expect("reusable");
        assert!(msg.sticker_message.as_option().is_some_and(|s| s.context_info.as_option().is_none()));
    }

    #[test]
    fn incomplete_or_foreign_messages_are_not_reusable() {
        let mut broken = stored_sticker();
        if let Some(s) = broken.sticker_message.as_option_mut() {
            s.media_key = None;
        }
        assert!(sticker_from_stored(&broken, None).is_none());
        let image = wa::Message { image_message: MessageField::some(Default::default()), ..Default::default() };
        assert!(sticker_from_stored(&image, None).is_none());
    }

    #[test]
    fn fresh_upload_builds_a_webp_sticker() {
        let upload = Uploaded {
            url: "https://mmg.example/n".into(),
            direct_path: "/v/n".into(),
            media_key: [9; 32],
            file_enc_sha256: [8; 32],
            file_sha256: [7; 32],
            file_length: 4321,
            media_key_timestamp: 1_700_000_000,
        };
        let facts = Facts { dims: Some((256, 128)), animated: false };
        let msg = sticker_from_upload(upload, facts, None);
        let st = msg.sticker_message.as_option().expect("sticker");
        assert_eq!(st.mimetype.as_deref(), Some("image/webp"));
        assert_eq!((st.width, st.height), (Some(256), Some(128)));
        assert_eq!(st.is_animated, Some(false));
        assert_eq!(st.file_length, Some(4321));
        assert_eq!(st.file_sha256.as_deref(), Some(&[7u8; 32][..]));
        assert_eq!(st.media_key_timestamp, Some(1_700_000_000));
        assert!(msgops::media_reusable(&msg));
    }

    #[test]
    fn animation_flag_is_read_from_the_webp_header() {
        let mut header = b"RIFF\x24\0\0\0WEBPVP8X\x0a\0\0\0".to_vec();
        header.extend_from_slice(&[0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert!(is_animated_webp(&header));
        header[20] = 0x10;
        assert!(!is_animated_webp(&header));
        assert!(!is_animated_webp(b"RIFF\0\0\0\0WEBPVP8 \0\0\0\0\0\0\0\0\0\0"));
        assert!(!is_animated_webp(b"short"));
    }
}
