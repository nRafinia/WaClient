//! UI commands executed against a connected client.

use std::sync::Arc;

use std::path::{Path, PathBuf};

use whatsapp_rust::buffa::{Message as _, MessageField};
use whatsapp_rust::download::MediaType;
use whatsapp_rust::media::{self, AudioOptions, DocumentOptions, ImageOptions, VideoOptions};
use whatsapp_rust::prelude::{Client, Jid, SendOptions, wa};
use whatsapp_rust::{RevokeType, message_key, message_range};
use whatsapp_rust::upload::UploadOptions;
use whatsapp_rust::wacore::proto_helpers::build_quote_context_with_info;
use whatsapp_rust::wacore::types::message::AddressingMode;
use whatsapp_rust::wacore_binary::JidExt as _;
use wa_store::{MessageKind, MessageStatus, NewMessage, STATUS_JID};

use crate::api::{Command, OutgoingMediaKind, UiEvent};
use crate::{avatar, convert, ephemeral, info, media as cache, msgops, preview, settings, share};
use crate::events::{Shared, now_secs, own_jid};
use crate::jid::redact;

pub(crate) const NOT_LOGGED_IN: &str = "Not logged in";

/// Upper bound of messages acknowledged by one `MarkChatRead`.
const MARK_READ_MAX: i64 = 100;

/// `SendReadReceiptsNow` covers messages of the last seven days...
const RECEIPTS_NOW_WINDOW_SECS: i64 = 7 * 24 * 60 * 60;
/// ...and at most this many of them.
const RECEIPTS_NOW_MAX: u32 = 200;

/// Our own messages older than this are not offered "delete for everyone"
/// (WhatsApp's limit is about two days; the server has the final say).
const REVOKE_MAX_AGE_SECS: i64 = 60 * 60 * 60;

pub(crate) fn parse_jid(sh: &Shared, raw: &str) -> Option<Jid> {
    match raw.parse::<Jid>() {
        Ok(j) => Some(j),
        Err(e) => {
            log::warn!("invalid chat jid {}: {e}", redact(raw));
            sh.emit(UiEvent::Error {
                message: format!("Invalid chat address: {raw}"),
            });
            None
        }
    }
}

/// Executes a command that does not touch the session lifecycle.
/// (`Logout`, `Shutdown`, `RetryPairing` and `Reconnect` are handled by the supervisor.)
pub async fn run(sh: Shared, client: Arc<Client>, cmd: Command) {
    match cmd {
        Command::SendText {
            chat_jid,
            text,
            quoted_id,
            mentions,
        } => send_text(&sh, &client, &chat_jid, &text, quoted_id.as_deref(), &mentions).await,
        Command::EditMessage { chat_jid, id, text } => {
            msgops::edit_message(&sh, &client, &chat_jid, &id, &text).await
        }
        Command::ForwardMessages { from_chat, ids, to_chats } => {
            msgops::forward_messages(&sh, &client, &from_chat, &ids, &to_chats).await
        }
        Command::PinMessage { chat_jid, id, pin, duration_secs } => {
            msgops::pin_message(&sh, &client, &chat_jid, &id, pin, duration_secs).await
        }
        Command::StarMessage { chat_jid, id, starred } => {
            let _busy = sh.account.begin_app_state();
            msgops::star_message(&sh, &client, &chat_jid, &id, starred).await
        }
        Command::FetchMessageInfo { chat_jid, id } => msgops::fetch_message_info(&sh, &chat_jid, &id),
        Command::MarkChatUnread { chat_jid } => {
            let _busy = sh.account.begin_app_state();
            Box::pin(crate::account::mark_chat_unread(&sh, &client, &chat_jid)).await
        }
        Command::ClearChat { chat_jid, keep_starred } => {
            let _busy = sh.account.begin_app_state();
            Box::pin(crate::account::clear_chat(&sh, &client, &chat_jid, keep_starred)).await
        }
        Command::SetBlocked { jid, blocked } => crate::account::set_blocked(&sh, &client, &jid, blocked).await,
        Command::FetchBlocklist => crate::account::fetch_blocklist(&sh, &client).await,
        Command::SetOwnName { name } => crate::account::set_own_name(&sh, &client, &name).await,
        Command::SetOwnAbout { text } => crate::account::set_own_about(&sh, &client, &text).await,
        Command::SetOwnPicture { path } => crate::account::set_own_picture(&sh, &client, path).await,
        Command::FetchOwnProfile => crate::account::fetch_own_profile(&sh, &client).await,
        Command::MarkChatRead { chat_jid } => mark_chat_read(&sh, &client, &chat_jid).await,
        Command::SubscribePresence { jid } => crate::presence::subscribe(&sh, &client, &jid).await,
        Command::SendReadReceiptsNow { chat_jid } => send_read_receipts_now(&sh, &client, &chat_jid).await,
        Command::SetChatReceipts { chat_jid, enabled } => set_chat_receipts(&sh, &chat_jid, enabled),
        Command::SetArchived { chat_jid, archived } => {
            let _busy = sh.account.begin_app_state();
            Box::pin(set_archived(&sh, &client, &chat_jid, archived)).await
        }
        Command::SetPinned { chat_jid, pinned } => {
            let _busy = sh.account.begin_app_state();
            Box::pin(set_pinned(&sh, &client, &chat_jid, pinned)).await
        }
        Command::SetMuted { chat_jid, until } => {
            let _busy = sh.account.begin_app_state();
            Box::pin(set_muted(&sh, &client, &chat_jid, until)).await
        }
        Command::SetTyping {
            chat_jid,
            composing,
        } => set_typing(&sh, &client, &chat_jid, composing).await,
        Command::SendMedia {
            chat_jid,
            path,
            kind,
            caption,
            quoted_id,
            view_once,
        } => {
            send_media(
                &sh,
                &client,
                &chat_jid,
                &path,
                kind,
                caption,
                quoted_id.as_deref(),
                view_once,
            )
            .await
        }
        // ---- SHARE: contacts / phone lookup / location ----
        Command::SendContact { chat_jid, contacts, quoted_id } => {
            share::send_contact(&sh, &client, &chat_jid, &contacts, quoted_id.as_deref()).await
        }
        Command::LookupPhones { numbers } => share::lookup_phones(&sh, &client, &numbers).await,
        Command::SendLocation { chat_jid, lat, lon, name, address, quoted_id } => {
            share::send_location(
                &sh,
                &client,
                &chat_jid,
                share::Place { lat, lon, name, address },
                quoted_id.as_deref(),
            )
            .await
        }
        // ---- STATUS POST (placeholder) ----
        Command::PostTextStatus { text, background_argb, font, audience } => {
            crate::status_post::post_text(&sh, &client, &text, background_argb, font, &audience).await
        }
        Command::PostMediaStatus { path, caption, audience } => {
            crate::status_post::post_media(&sh, &client, &path, caption, &audience).await
        }
        // ---- DISAPPEARING TIMERS ----
        Command::SetChatTimer { chat_jid, secs } => {
            ephemeral::set_chat_timer(&sh, &client, &chat_jid, secs).await
        }
        Command::SetDefaultTimer { secs } => ephemeral::set_default_timer(&sh, &client, secs).await,
        Command::FetchDefaultTimer => ephemeral::fetch_default_timer(&sh),
        Command::SendReaction {
            chat_jid,
            id,
            emoji,
        } => send_reaction(&sh, &client, &chat_jid, &id, &emoji).await,
        Command::DeleteMessage {
            chat_jid,
            id,
            for_everyone,
        } => delete_message(&sh, &client, &chat_jid, &id, for_everyone).await,
        Command::FetchContactInfo { jid } => info::fetch(sh, client, jid).await,
        // The poster comes from the stored row, never from the UI.
        Command::MarkStatusViewed { sender_jid: _, id } => {
            mark_status_viewed(&sh, &client, &id).await
        }
        // The supervisor applies it inline; kept so the match stays exhaustive.
        Command::UpdateSettings(new) => sh.set_settings(new),
        Command::DownloadMedia { chat_jid, id } => {
            cache::download(sh, client, chat_jid, id, None).await
        }
        Command::FetchAvatar { jid } => avatar::fetch(sh, client, jid).await,
        // ---- TRANSLATION ----
        Command::TranslateMessage { chat_jid, id } => crate::translate::translate_message(&sh, &client, &chat_jid, &id).await,
        Command::FetchTranslations { chat_jid, ids } => crate::translate::fetch_cached(&sh, &chat_jid, &ids),
        Command::TranslateText { request_id, text, target } => {
            crate::translate::translate_text(&sh, &client, request_id, &text, &target).await
        }
        // ---- CALLS ----
        Command::RejectCall { call_id } => crate::calls::reject(&sh, &client, &call_id).await,
        // ---- POLLS ----
        Command::VotePoll { chat_jid, poll_id, options } => {
            crate::polls::vote(&sh, &client, &chat_jid, &poll_id, &options).await
        }
        Command::CreatePoll { chat_jid, question, options, multi, quiz_correct } => {
            crate::polls::create(&sh, &client, &chat_jid, &question, &options, multi, quiz_correct).await
        }
        // ---- GROUP ADMIN ----
        Command::CreateGroup { subject, participants, picture } => {
            crate::group_admin::create_group(&sh, &client, &subject, &participants, picture).await
        }
        Command::UpdateGroupMembers { group_jid, jids, action } => {
            crate::group_admin::update_members(&sh, &client, &group_jid, &jids, action).await
        }
        Command::FetchMembershipRequests { group_jid } => {
            crate::group_admin::fetch_requests(&sh, &client, &group_jid).await
        }
        Command::AnswerMembershipRequests { group_jid, jids, approve } => {
            crate::group_admin::answer_requests(&sh, &client, &group_jid, &jids, approve).await
        }
        Command::SetGroupSubject { group_jid, subject } => {
            crate::group_admin::set_subject(&sh, &client, &group_jid, &subject).await
        }
        Command::SetGroupDescription { group_jid, description } => {
            crate::group_admin::set_description(&sh, &client, &group_jid, description).await
        }
        Command::SetGroupPicture { group_jid, path } => {
            crate::group_admin::set_picture(&sh, &client, &group_jid, path).await
        }
        Command::SetGroupSetting { group_jid, setting } => {
            crate::group_admin::set_setting(&sh, &client, &group_jid, setting).await
        }
        Command::LeaveGroup { group_jid } => crate::group_admin::leave(&sh, &client, &group_jid).await,
        Command::FetchInviteLink { group_jid, reset } => {
            crate::group_admin::invite_link(&sh, &client, &group_jid, reset).await
        }
        Command::PreviewInvite { link } => crate::group_admin::preview_invite(&sh, &client, &link).await,
        Command::JoinGroup { link } => crate::group_admin::join(&sh, &client, &link).await,
        Command::SendSticker { chat_jid, source_chat, source_id, quoted_id } => {
            crate::sticker::send(&sh, &client, &chat_jid, &source_chat, &source_id, quoted_id.as_deref()).await
        }
        Command::Logout | Command::Shutdown | Command::RetryPairing | Command::Reconnect => {
            log::error!("lifecycle command reached the command runner");
        }
    }
}

/// Builds the quote context for a reply from what the store knows about the
/// quoted message, plus the preview shown in our own copy.
fn quote_for(
    sh: &Shared,
    chat: &Jid,
    chat_jid: &str,
    quoted_id: &str,
) -> Option<(wa::ContextInfo, Option<String>)> {
    let row = sh.store.message(chat_jid, quoted_id).ok().flatten()?;
    let raw = sh.store.message_raw(chat_jid, quoted_id).ok().flatten()?;
    let quoted = wa::Message::decode_from_slice(&raw)
        .map_err(|e| log::warn!("stored message {quoted_id} does not decode: {e}"))
        .ok()?;
    let sender = row.sender_jid.parse::<Jid>().ok()?;
    let ctx = build_quote_context_with_info(quoted_id, &sender, chat, chat, &quoted);
    Some((ctx, convert::quote_preview(&quoted)))
}

/// Quote context, preview and id for a reply; all `None` when the quoted
/// message is not known locally (the message is then sent without a quote).
pub(crate) struct Quote {
    pub(crate) context: Option<wa::ContextInfo>,
    pub(crate) preview: Option<String>,
    pub(crate) id: Option<String>,
}

pub(crate) fn resolve_quote(sh: &Shared, chat: &Jid, chat_jid: &str, quoted_id: Option<&str>) -> Quote {
    let found = quoted_id.and_then(|qid| {
        let q = quote_for(sh, chat, chat_jid, qid);
        if q.is_none() {
            log::warn!("quoted message {qid} not found locally, sending without quote");
        }
        q.map(|(ctx, preview)| (ctx, preview, qid.to_string()))
    });
    match found {
        Some((context, preview, id)) => Quote {
            context: Some(context),
            preview,
            id: Some(id),
        },
        None => Quote {
            context: None,
            preview: None,
            id: None,
        },
    }
}

async fn send_text(
    sh: &Shared,
    client: &Arc<Client>,
    chat_jid: &str,
    text: &str,
    quoted_id: Option<&str>,
    mentions: &[String],
) {
    let Some(chat) = parse_jid(sh, chat_jid) else {
        return;
    };
    let Some(sender_jid) = own_jid(client) else {
        reject(sh, NOT_LOGGED_IN.to_string());
        return;
    };
    let id = client.generate_message_id();

    let quote = resolve_quote(sh, &chat, chat_jid, quoted_id);
    let mentioned = msgops::normalize_mentions(sh, client, &chat, mentions).await;
    let context = msgops::context_with_mentions(quote.context, mentioned);
    let pending = NewMessage {
        chat_jid: chat_jid.to_string(),
        id: id.clone(),
        sender_jid,
        from_me: true,
        ts: now_secs(),
        kind: MessageKind::Text,
        text: Some(text.to_string()),
        quoted_id: quote.id,
        quoted_preview: quote.preview,
        raw: Some(convert::text_message(text, context.clone()).encode_to_vec()),
        ..Default::default()
    };
    // Stored before the preview fetch (up to LINK_PREVIEW_BUDGET) so the
    // bubble appears immediately; the card is attached after sending.
    if !store_pending(sh, &pending) {
        return;
    }

    let link = outgoing_preview(sh, client, text).await;
    let message = match &link {
        Some(p) => {
            let mut m = preview::text_message_with_preview(text, p);
            if let (Some(ctx), Some(ext)) = (context, m.extended_text_message.as_option_mut()) {
                ext.context_info = MessageField::some(ctx);
            }
            m
        }
        None => convert::text_message(text, context),
    };
    deliver(sh, client, chat, chat_jid, id.clone(), message).await;

    if let Some(p) = link {
        let title = (!p.title.is_empty()).then_some(p.title.as_str());
        let desc = (!p.description.is_empty()).then_some(p.description.as_str());
        match sh.store.set_link_preview(chat_jid, &id, Some(&p.url), title, desc, p.thumbnail_jpeg.as_deref()) {
            Ok(()) => sh.emit(UiEvent::MessagesChanged { chat_jid: chat_jid.to_string() }),
            Err(e) => log::error!("storing link preview failed: {e}"),
        }
    }
}

/// Longest a send waits for the preview of its link.
const LINK_PREVIEW_BUDGET: std::time::Duration = std::time::Duration::from_secs(3);

/// Preview for the first link of an outgoing text, if the user enabled
/// previews and the page answers within [`LINK_PREVIEW_BUDGET`]. Any failure
/// means "send plain text"; the URL is not logged.
async fn outgoing_preview(sh: &Shared, client: &Arc<Client>, text: &str) -> Option<preview::LinkPreview> {
    if !sh.settings().send_link_previews {
        return None;
    }
    let url = preview::first_url(text)?;
    match tokio::time::timeout(LINK_PREVIEW_BUDGET, preview::fetch_preview(&*client.http_client, url, sh.proxied)).await {
        Ok(Ok(p)) => Some(p),
        Ok(Err(e)) => {
            log::debug!("no link preview: {e:#}");
            None
        }
        Err(_) => {
            log::debug!("link preview timed out");
            None
        }
    }
}

/// Stores the outgoing row and tells the UI; `false` if it could not be stored.
///
/// Every chat send stores its row here, so this is where a chat's disappearing
/// timer sets `expires_at` on our own copy (the wire side is in [`deliver`]).
pub(crate) fn store_pending(sh: &Shared, pending: &NewMessage) -> bool {
    let timed = ephemeral::with_outgoing_expiry(sh, pending);
    let row = timed.as_ref().unwrap_or(pending);
    if let Err(e) = sh.store.insert_outgoing(row) {
        log::warn!(
            "storing outgoing message in {} failed: {e}",
            crate::jid::redact(&pending.chat_jid)
        );
        sh.emit(UiEvent::Error {
            message: "Could not store the message".to_string(),
        });
        return false;
    }
    sh.emit(UiEvent::MessagesChanged {
        chat_jid: pending.chat_jid.clone(),
    });
    sh.emit(UiEvent::ChatsChanged);
    true
}

/// Sends an already stored message and records the outcome.
pub(crate) async fn deliver(
    sh: &Shared,
    client: &Arc<Client>,
    chat: Jid,
    chat_jid: &str,
    id: String,
    mut message: wa::Message,
) {
    let mut options = SendOptions::default().with_message_id(id.clone());
    // The one place every chat message passes: the chat's disappearing timer
    // is applied here (never to statuses; reactions and protocol messages
    // take another path and do not expire).
    if let Some(secs) = ephemeral::outgoing_expiration(sh, chat_jid) {
        ephemeral::make_expirable(&mut message);
        options = options.with_ephemeral_expiration(secs);
    }
    let result = client.send_message_with_options(chat, message, options).await;
    let status = match result {
        Ok(_) => MessageStatus::Sent,
        Err(e) => {
            crate::jid::warn_lib_error(
                &format!("sending message {id} to {} failed", crate::jid::redact(chat_jid)),
                &e,
            );
            sh.emit(UiEvent::Error {
                message: match crate::jid::classify_error(&e) {
                    "timeout" => "Message not sent: WhatsApp did not answer in time",
                    "not connected" => "Message not sent: not connected to WhatsApp",
                    "rate limited" => "Message not sent: too many requests, try again in a moment",
                    "not authorized" => "Message not sent: you are not allowed to send here",
                    _ => "Message not sent",
                }
                .to_string(),
            });
            MessageStatus::Failed
        }
    };
    finish(sh, chat_jid, id, status);
}

pub(crate) fn finish(sh: &Shared, chat_jid: &str, id: String, status: MessageStatus) {
    if let Err(e) = sh.store.update_status(chat_jid, &id, status) {
        log::error!("update_status failed: {e}");
    }
    sh.emit(UiEvent::MessageStatus {
        chat_jid: chat_jid.to_string(),
        id,
        status,
    });
}

pub(crate) fn reject(sh: &Shared, message: String) {
    sh.emit(UiEvent::Error { message });
}

/// What the pending row and the upload need to know about the attachment.
pub(crate) struct Prepared {
    pub(crate) kind: MessageKind,
    pub(crate) media_type: MediaType,
    pub(crate) mime: String,
    pub(crate) file_name: Option<String>,
    pub(crate) duration: Option<u32>,
    pub(crate) dims: Option<(u32, u32)>,
    pub(crate) thumbnail: Option<Vec<u8>>,
}

fn prepare(
    kind: OutgoingMediaKind,
    path: &Path,
    file_name: &str,
    data: &[u8],
) -> Result<Prepared, String> {
    let ext_mime = cache::mime_for_path(path);
    let document = |mime: Option<&str>| Prepared {
        kind: MessageKind::Document,
        media_type: MediaType::Document,
        mime: mime.unwrap_or("application/octet-stream").to_string(),
        file_name: Some(file_name.to_string()),
        duration: None,
        dims: None,
        thumbnail: None,
    };
    match kind {
        OutgoingMediaKind::Image => {
            match (cache::image_info(data), cache::sniff_image_mime(data)) {
                (Some((w, h, thumb)), Some(mime)) => Ok(Prepared {
                    kind: MessageKind::Image,
                    media_type: MediaType::Image,
                    mime: mime.to_string(),
                    file_name: None,
                    duration: None,
                    dims: Some((w, h)),
                    thumbnail: Some(thumb),
                }),
                _ => Ok(document(ext_mime)),
            }
        }
        OutgoingMediaKind::Video => Ok(Prepared {
            kind: MessageKind::Video,
            media_type: MediaType::Video,
            mime: ext_mime
                .filter(|m| m.starts_with("video/"))
                .unwrap_or("video/mp4")
                .to_string(),
            file_name: None,
            duration: None,
            dims: None,
            thumbnail: None,
        }),
        OutgoingMediaKind::Voice => {
            let duration = cache::ogg_opus_duration_secs(data)
                .ok_or_else(|| "Voice messages must be OGG/Opus files".to_string())?;
            Ok(Prepared {
                kind: MessageKind::Voice,
                media_type: MediaType::Audio,
                mime: "audio/ogg; codecs=opus".to_string(),
                file_name: None,
                duration: Some(duration),
                dims: None,
                thumbnail: None,
            })
        }
        OutgoingMediaKind::Audio => Ok(Prepared {
            kind: MessageKind::Audio,
            media_type: MediaType::Audio,
            mime: ext_mime
                .filter(|m| m.starts_with("audio/"))
                .unwrap_or("audio/mpeg")
                .to_string(),
            duration: cache::ogg_opus_duration_secs(data),
            file_name: None,
            dims: None,
            thumbnail: None,
        }),
        OutgoingMediaKind::Document => Ok(document(ext_mime)),
    }
}

pub(crate) fn build_media_message(
    p: &Prepared,
    upload: whatsapp_rust::upload::UploadResponse,
    caption: Option<String>,
    context: Option<wa::ContextInfo>,
) -> wa::Message {
    let context_info = context.map(Box::new);
    match p.kind {
        MessageKind::Image => {
            let mut msg = media::image_message(
                upload,
                ImageOptions {
                    caption,
                    mimetype: Some(p.mime.clone()),
                    jpeg_thumbnail: p.thumbnail.clone(),
                    context_info,
                },
            );
            // `ImageOptions` has no width/height fields.
            if let (Some(im), Some((w, h))) = (msg.image_message.as_option_mut(), p.dims) {
                im.width = Some(w);
                im.height = Some(h);
            }
            msg
        }
        MessageKind::Video => media::video_message(
            upload,
            VideoOptions {
                caption,
                mimetype: Some(p.mime.clone()),
                context_info,
                ..Default::default()
            },
        ),
        MessageKind::Voice | MessageKind::Audio => media::audio_message(
            upload,
            AudioOptions {
                mimetype: Some(p.mime.clone()),
                duration_seconds: p.duration,
                ptt: Some(p.kind == MessageKind::Voice),
                context_info,
                ..Default::default()
            },
        ),
        _ => media::document_message(
            upload,
            DocumentOptions {
                mimetype: Some(p.mime.clone()),
                file_name: p.file_name.clone(),
                caption,
                context_info,
                ..Default::default()
            },
        ),
    }
}

async fn send_media(
    sh: &Shared,
    client: &Arc<Client>,
    chat_jid: &str,
    path: &Path,
    kind: OutgoingMediaKind,
    caption: Option<String>,
    quoted_id: Option<&str>,
    view_once: bool,
) {
    let Some(chat) = parse_jid(sh, chat_jid) else {
        return;
    };
    let Some(sender_jid) = own_jid(client) else {
        reject(sh, NOT_LOGGED_IN.to_string());
        return;
    };
    if view_once && !matches!(kind, OutgoingMediaKind::Image | OutgoingMediaKind::Video) {
        reject(sh, "Only photos and videos can be sent as view once".to_string());
        return;
    }
    let caption = caption.filter(|c| !c.is_empty());
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file")
        .to_string();

    // Up to 100 MiB of file I/O, decoding and hashing: keep it off the async
    // threads, and copy into the cache from the same buffer the upload consumes.
    let id = client.generate_message_id();
    let job = {
        let (path, file_name, media_dir, chat_jid, id) = (
            path.to_path_buf(),
            file_name.clone(),
            sh.media.dir.clone(),
            chat_jid.to_string(),
            id.clone(),
        );
        move || -> Result<(Prepared, Vec<u8>, Option<PathBuf>), String> {
            let too_big = || format!("{file_name} is larger than 100 MB");
            let unreadable = |e: std::io::Error| format!("Cannot read {file_name}: {e}");
            let meta = std::fs::metadata(&path).map_err(unreadable)?;
            if meta.len() > cache::MAX_SEND_BYTES {
                return Err(too_big());
            }
            let data = std::fs::read(&path).map_err(unreadable)?;
            // The file may have grown between the metadata call and the read.
            if u64::try_from(data.len()).map_or(true, |n| n > cache::MAX_SEND_BYTES) {
                return Err(too_big());
            }
            let prepared = prepare(kind, &path, &file_name, &data)?;
            // An image we cannot decode would go out as a document.
            if view_once && !ephemeral::view_once_allowed(prepared.kind) {
                return Err("Only photos and videos can be sent as view once".to_string());
            }
            // A view-once message keeps no copy of the media on our side.
            if view_once {
                return Ok((prepared, data, None));
            }
            let ext = cache::extension_for(
                prepared.kind,
                Some(&prepared.mime),
                prepared.file_name.as_deref(),
            );
            let cached = cache::cache_path(&media_dir, &chat_jid, &id, &ext);
            cache::write_atomic(&cached, &data)
                .map_err(|e| format!("Cannot copy {file_name}: {e}"))?;
            Ok((prepared, data, Some(cached)))
        }
    };
    let (prepared, data, cached) = match tokio::task::spawn_blocking(job).await {
        Ok(Ok(done)) => done,
        Ok(Err(msg)) => {
            reject(sh, msg);
            return;
        }
        Err(e) => {
            log::error!("media preparation task failed: {e}");
            reject(sh, "Could not prepare the file".to_string());
            return;
        }
    };

    let quote = resolve_quote(sh, &chat, chat_jid, quoted_id);
    let pending = NewMessage {
        chat_jid: chat_jid.to_string(),
        id: id.clone(),
        sender_jid,
        from_me: true,
        ts: now_secs(),
        kind: prepared.kind,
        text: caption.clone(),
        quoted_id: quote.id,
        quoted_preview: quote.preview,
        media_mime: Some(prepared.mime.clone()),
        media_name: prepared.file_name.clone(),
        media_size: i64::try_from(data.len()).ok(),
        media_duration: prepared.duration.map(i64::from),
        thumbnail: prepared.thumbnail.clone(),
        local_path: cached.map(|c| c.to_string_lossy().into_owned()),
        raw: None,
        ..Default::default()
    };
    let pending = if view_once {
        ephemeral::view_once_placeholder_row(&pending)
    } else {
        pending
    };
    if !store_pending(sh, &pending) {
        return;
    }

    let upload = match client
        .upload(data, prepared.media_type, UploadOptions::new())
        .await
    {
        Ok(u) => u,
        Err(e) => {
            log::warn!("uploading {file_name} failed: {e:#}");
            sh.emit(UiEvent::Error {
                message: format!("Upload failed: {e}"),
            });
            finish(sh, chat_jid, id, MessageStatus::Failed);
            return;
        }
    };
    let mut message = build_media_message(&prepared, upload, caption, quote.context);
    if view_once {
        // No raw copy: it would hold the key that could fetch the media again.
        if !ephemeral::set_view_once(&mut message) {
            reject(sh, "Could not send as view once".to_string());
            finish(sh, chat_jid, id, MessageStatus::Failed);
            return;
        }
    } else if let Err(e) = sh.store.set_raw(chat_jid, &id, &message.encode_to_vec()) {
        log::error!("storing raw media message failed: {e}");
    }
    deliver(sh, client, chat, chat_jid, id, message).await;
}

/// The key WhatsApp needs to attribute a reaction to `id`. Groups and
/// statuses require the author of the target message.
fn reaction_key(
    chat_jid: &str,
    id: &str,
    from_me: bool,
    author_jid: &str,
    needs_author: bool,
) -> wa::MessageKey {
    wa::MessageKey {
        remote_jid: Some(chat_jid.to_string()),
        from_me: Some(from_me),
        id: Some(id.to_string()),
        participant: needs_author.then(|| author_jid.to_string()),
    }
}

async fn send_reaction(sh: &Shared, client: &Arc<Client>, chat_jid: &str, id: &str, emoji: &str) {
    let Some(chat) = parse_jid(sh, chat_jid) else {
        return;
    };
    let Some(own) = own_jid(client) else {
        reject(sh, NOT_LOGGED_IN.to_string());
        return;
    };
    let row = match sh.store.message(chat_jid, id) {
        Ok(Some(row)) => row,
        Ok(None) => {
            reject(sh, "The message to react to was not found".to_string());
            return;
        }
        Err(e) => {
            log::error!("message lookup failed: {e}");
            reject(sh, "Could not read the message".to_string());
            return;
        }
    };
    let key = reaction_key(
        chat_jid,
        id,
        row.from_me,
        &row.sender_jid,
        chat.is_group() || chat.is_status_broadcast(),
    );

    let previous = sh.store.reaction_of(chat_jid, id, &own).unwrap_or_else(|e| {
        log::error!("reaction lookup failed: {e}");
        None
    });
    let ts = now_secs();
    if let Err(e) = sh.store.upsert_reaction(chat_jid, id, &own, emoji, ts, true) {
        log::error!("storing reaction failed: {e}");
        reject(sh, format!("Could not store the reaction: {e}"));
        return;
    }
    sh.emit(UiEvent::MessagesChanged {
        chat_jid: chat_jid.to_string(),
    });

    if let Err(e) = client.send_reaction(chat, key, emoji).await {
        crate::jid::warn_lib_error("sending reaction failed", &e);
        // Only undo our own optimistic write: if the stored reaction changed
        // meanwhile (e.g. a newer one arrived), it is the truth now.
        let optimistic = (!emoji.is_empty()).then(|| (emoji.to_string(), ts));
        let current = sh.store.reaction_of(chat_jid, id, &own).unwrap_or_else(|e| {
            log::error!("reaction lookup failed: {e}");
            None
        });
        if current == optimistic {
            let restored = sh.store.remove_reaction(chat_jid, id, &own).and_then(|()| match &previous {
                Some((old, old_ts)) => sh.store.upsert_reaction(chat_jid, id, &own, old, *old_ts, true),
                None => Ok(()),
            });
            if let Err(e) = restored {
                log::error!("restoring reaction failed: {e}");
            }
        }
        sh.emit(UiEvent::MessagesChanged {
            chat_jid: chat_jid.to_string(),
        });
        reject(sh, format!("Reaction not sent: {e}"));
    }
}

/// Local pre-checks of "delete for everyone"; `Err` is the message for the UI.
fn check_revoke(from_me: bool, revoked: bool, ts: i64, now: i64) -> Result<(), &'static str> {
    if !from_me {
        return Err("You can only delete your own messages for everyone");
    }
    if revoked {
        return Err("This message was already deleted");
    }
    if now.saturating_sub(ts) > REVOKE_MAX_AGE_SECS {
        return Err("Too old to delete for everyone");
    }
    Ok(())
}

/// Removes a cached media file, but only one that lies inside the media cache.
///
/// The filesystem work runs on the blocking pool; nothing waits for it.
pub(crate) fn remove_cached_file(sh: &Shared, path: &str) {
    let path = PathBuf::from(path);
    let dir = sh.media.dir.clone();
    drop(tokio::task::spawn_blocking(move || {
        if !crate::status::is_inside_cache(&path, &dir) {
            return;
        }
        if let Err(e) = std::fs::remove_file(&path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!("cannot delete cached media {}: {e}", path.display());
        }
    }));
}

async fn delete_message(
    sh: &Shared,
    client: &Arc<Client>,
    chat_jid: &str,
    id: &str,
    for_everyone: bool,
) {
    let row = match sh.store.message(chat_jid, id) {
        Ok(Some(row)) => row,
        Ok(None) => {
            reject(sh, "The message to delete was not found".to_string());
            return;
        }
        Err(e) => {
            log::error!("message lookup failed: {e}");
            reject(sh, "Could not read the message".to_string());
            return;
        }
    };
    if for_everyone {
        if let Err(msg) = check_revoke(row.from_me, row.revoked, row.ts, now_secs()) {
            reject(sh, msg.to_string());
            return;
        }
        let Some(chat) = parse_jid(sh, chat_jid) else {
            return;
        };
        if let Err(e) = client.revoke_message(chat, id, RevokeType::Sender).await {
            crate::jid::warn_lib_error(&format!("revoking message {id} failed"), &e);
            reject(sh, format!("Could not delete the message for everyone: {e}"));
            return;
        }
        if let Err(e) = sh.store.revoke_message(chat_jid, id) {
            log::error!("storing revoke failed: {e}");
            sh.emit(UiEvent::Error {
                message: "Deleted for everyone, but the local copy could not be updated".to_string(),
            });
        }
        if let Some(path) = &row.local_path {
            remove_cached_file(sh, path);
        }
    } else {
        match sh.store.delete_message(chat_jid, id) {
            Ok(path) => {
                if let Some(path) = path {
                    remove_cached_file(sh, &path);
                }
            }
            Err(e) => {
                log::error!("deleting message failed: {e}");
                reject(sh, format!("Could not delete the message: {e}"));
                return;
            }
        }
    }
    sh.emit(UiEvent::MessagesChanged {
        chat_jid: chat_jid.to_string(),
    });
    sh.emit(UiEvent::ChatsChanged);
    if !for_everyone {
        sync_delete_for_me(sh, client, chat_jid, id, &row).await;
    }
}

/// What to send as the author of a message in a delete-for-me key.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum KeyParticipant {
    /// The key carries no participant (DMs, our own messages).
    Omit,
    Use(Jid),
    /// The required address form is unknown; sending a guess would target the
    /// wrong message, so nothing is synced.
    Skip,
}

/// Picks the participant of a delete-for-me key. In a group the key must use
/// the group's own address form for the author: the LID form in LID-addressed
/// groups, the phone-number form otherwise. `pn` / `lid` are the two forms of
/// the stored sender, when known.
pub(crate) fn key_participant(
    group: bool,
    from_me: bool,
    mode: Option<AddressingMode>,
    pn: Option<Jid>,
    lid: Option<Jid>,
) -> KeyParticipant {
    if !group || from_me {
        return KeyParticipant::Omit;
    }
    let form = match mode {
        Some(AddressingMode::Lid) => lid,
        Some(AddressingMode::Pn) => pn,
        None => None,
    };
    form.map_or(KeyParticipant::Skip, KeyParticipant::Use)
}

/// Both address forms of `sender`, using the library's LID<->PN mapping for
/// the one the stored value does not already have.
pub(crate) async fn sender_forms(client: &Client, sender: &Jid) -> (Option<Jid>, Option<Jid>) {
    let base = sender.to_non_ad();
    let (mut pn, mut lid) = (base.is_pn().then(|| base.clone()), base.is_lid().then(|| base.clone()));
    if pn.is_some() == lid.is_some() {
        return (pn, lid);
    }
    match client.get_lid_pn_entry(&base).await {
        Ok(Some(entry)) => {
            if lid.is_none() {
                lid = Some(Jid::lid(&*entry.lid));
            } else {
                pn = Some(Jid::pn(&*entry.phone_number));
            }
        }
        Ok(None) => {}
        Err(e) => log::debug!("LID lookup failed for {}: {e}", redact(&base.to_string())),
    }
    (pn, lid)
}

/// The participant of `row`'s message key in `chat`, in the form the chat uses.
///
/// With `allow_fetch` false only cached group addressing is used, so no
/// metadata request is made; an unknown mode then yields `Skip`.
pub(crate) async fn resolve_participant(
    sh: &Shared,
    client: &Client,
    chat: &Jid,
    row: &wa_store::MessageRow,
    allow_fetch: bool,
) -> KeyParticipant {
    if !chat.is_group() || row.from_me {
        return KeyParticipant::Omit;
    }
    let Ok(sender) = row.sender_jid.parse::<Jid>() else {
        log::warn!("invalid sender {} in a message key", redact(&row.sender_jid));
        return KeyParticipant::Skip;
    };
    let mode = if allow_fetch {
        info::group_addressing(sh, client, chat).await
    } else {
        info::cached_group_addressing(sh, chat)
    };
    let (pn, lid) = sender_forms(client, &sender).await;
    key_participant(true, false, mode, pn, lid)
}

/// Longest wait for the phone-side half of a chat setting. The library waits
/// for the app-state collection (and for missing keys to be re-shared) without
/// a bound of its own.
const APP_STATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

const KEYS_MISSING_MESSAGE: &str = "Chat settings aren't synced with your phone yet \u{2014} try again in a minute";

/// Runs the network half of a chat-setting change with a bound; the error is
/// the text to classify with [`app_state_failure`].
pub(crate) async fn with_app_state_timeout<E: std::fmt::Display>(
    fut: impl std::future::Future<Output = Result<(), E>>,
) -> Result<(), String> {
    match tokio::time::timeout(APP_STATE_TIMEOUT, fut).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("timed out waiting for your phone".to_string()),
    }
}

/// The message for the UI when a chat setting could not be synced. Missing
/// app-state keys (the phone has not shared them yet) get their own wording.
pub(crate) fn app_state_failure(raw: &str, what: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("sync key") || lower.contains("keynotfound") || lower.contains("key not found") {
        KEYS_MISSING_MESSAGE.to_string()
    } else {
        format!("Could not {what}: {raw}")
    }
}

/// One info line per chat-setting attempt: variant, redacted address, outcome.
pub(crate) fn log_flag_outcome(variant: &str, chat_jid: &str, result: &Result<(), String>) {
    match result {
        Ok(()) => log::info!("{variant} {}: ok", redact(chat_jid)),
        Err(e) => crate::jid::warn_lib_error(&format!("{variant} {}: failed", redact(chat_jid)), e),
    }
}

/// WhatsApp's limit of pinned chats.
const MAX_PINNED: usize = 3;

/// Local pre-checks of pinning; `Err` is the message for the UI.
fn check_pin(pinned_others: usize, archived: bool) -> Result<(), &'static str> {
    if archived {
        return Err("Unarchive the chat to pin it");
    }
    if pinned_others >= MAX_PINNED {
        return Err("You can only pin up to 3 chats");
    }
    Ok(())
}

/// What an app-state mute mutation has to say.
#[derive(Debug, PartialEq)]
enum MuteChange {
    Unmute,
    Forever,
    /// Expiry in milliseconds.
    UntilMs(i64),
}

/// Maps the UI's `until` (unix seconds, `i64::MAX` = always, `None` = unmute)
/// to a mutation; an expiry that already passed means unmute.
fn mute_change(until: Option<i64>, now: i64) -> MuteChange {
    match until {
        None => MuteChange::Unmute,
        Some(i64::MAX) => MuteChange::Forever,
        Some(t) if t <= now => MuteChange::Unmute,
        Some(t) => MuteChange::UntilMs(t.saturating_mul(1000)),
    }
}

pub(crate) async fn chat_for_flag_change(sh: &Shared, chat_jid: &str) -> Option<(Jid, wa_store::ChatRow)> {
    if chat_jid == STATUS_JID {
        return None;
    }
    let chat = parse_jid(sh, chat_jid)?;
    match sh.store.chat(chat_jid) {
        Ok(Some(row)) => Some((chat, row)),
        Ok(None) => {
            reject(sh, "The chat was not found".to_string());
            None
        }
        Err(e) => {
            log::error!("chat lookup failed: {e}");
            reject(sh, "Could not read the chat".to_string());
            None
        }
    }
}

async fn set_pinned(sh: &Shared, client: &Arc<Client>, chat_jid: &str, pinned: bool) {
    let Some((chat, previous)) = chat_for_flag_change(sh, chat_jid).await else {
        return;
    };
    if previous.pinned == pinned {
        sh.emit(UiEvent::ChatsChanged);
        return;
    }
    if pinned {
        let others = match sh.store.list_chats(true) {
            Ok(rows) => rows.iter().filter(|c| c.pinned && c.jid != chat_jid).count(),
            Err(e) => {
                log::error!("listing chats failed: {e}");
                reject(sh, "Could not read the chats".to_string());
                return;
            }
        };
        if let Err(msg) = check_pin(others, previous.archived) {
            reject(sh, msg.to_string());
            return;
        }
    }
    if let Err(e) = sh.store.set_chat_flags(chat_jid, Some(pinned), None, None) {
        log::error!("storing pin flag failed: {e}");
        reject(sh, format!("Could not update the chat: {e}"));
        return;
    }
    sh.emit(UiEvent::ChatsChanged);
    let actions = client.chat_actions();
    let result = with_app_state_timeout(async {
        if pinned { actions.pin_chat(&chat).await } else { actions.unpin_chat(&chat).await }
    })
    .await;
    log_flag_outcome("SetPinned", chat_jid, &result);
    if let Err(e) = result {
        match sh.store.chat(chat_jid) {
            Ok(Some(row)) => {
                if let Some(restore) = guarded_revert(row.pinned, pinned, previous.pinned) {
                    if let Err(e) = sh.store.set_chat_flags(chat_jid, Some(restore), None, None) {
                        log::error!("reverting pin flag failed: {e}");
                    }
                }
            }
            Ok(None) => {}
            Err(e) => log::error!("re-reading the chat for the pin revert failed: {e}"),
        }
        sh.emit(UiEvent::ChatsChanged);
        reject(sh, app_state_failure(&e, if pinned { "pin the chat" } else { "unpin the chat" }));
    }
}

async fn set_muted(sh: &Shared, client: &Arc<Client>, chat_jid: &str, until: Option<i64>) {
    let Some((chat, previous)) = chat_for_flag_change(sh, chat_jid).await else {
        return;
    };
    let change = mute_change(until, now_secs());
    let stored = match change {
        MuteChange::Unmute => 0,
        MuteChange::Forever => i64::MAX,
        MuteChange::UntilMs(ms) => ms / 1000,
    };
    if mute_is_noop(previous.muted_until, stored, now_secs()) {
        // An expired mute is "not muted" remotely, but the stale timestamp
        // would stay in the row; write the 0 explicitly.
        if needs_local_unmute(previous.muted_until, stored) {
            if let Err(e) = sh.store.set_chat_flags(chat_jid, None, None, Some(0)) {
                log::error!("clearing the expired mute failed: {e}");
            }
        }
        sh.emit(UiEvent::ChatsChanged);
        return;
    }
    if let Err(e) = sh.store.set_chat_flags(chat_jid, None, None, Some(stored)) {
        log::error!("storing mute flag failed: {e}");
        reject(sh, format!("Could not update the chat: {e}"));
        return;
    }
    sh.emit(UiEvent::ChatsChanged);
    let actions = client.chat_actions();
    let result = with_app_state_timeout(async {
        match change {
            MuteChange::Unmute => actions.unmute_chat(&chat).await,
            MuteChange::Forever => actions.mute_chat(&chat).await,
            MuteChange::UntilMs(ms) => actions.mute_chat_until(&chat, ms).await,
        }
    })
    .await;
    log_flag_outcome("SetMuted", chat_jid, &result);
    if let Err(e) = result {
        match sh.store.chat(chat_jid) {
            Ok(Some(row)) => {
                if let Some(restore) = guarded_revert(row.muted_until, stored, previous.muted_until) {
                    if let Err(e) = sh.store.set_chat_flags(chat_jid, None, None, Some(restore)) {
                        log::error!("reverting mute flag failed: {e}");
                    }
                }
            }
            Ok(None) => {}
            Err(e) => log::error!("re-reading the chat for the mute revert failed: {e}"),
        }
        sh.emit(UiEvent::ChatsChanged);
        reject(sh, app_state_failure(&e, "change the mute setting"));
    }
}

/// The value to restore after a failed mutation: `previous`, but only while
/// the stored value is still the optimistic one we wrote. Anything else is a
/// newer change (e.g. from another device) that must not be overwritten.
pub(crate) fn guarded_revert<T: PartialEq>(current: T, optimistic: T, previous: T) -> Option<T> {
    (current == optimistic).then_some(previous)
}

/// Whether setting the mute state to `target` (as stored: 0 unmuted, `i64::MAX`
/// forever, else unix seconds) would change nothing from `current`. Expiries
/// within 60 s count as equal.
fn mute_is_noop(current: i64, target: i64, now: i64) -> bool {
    match target {
        0 => current == 0 || (current != i64::MAX && current <= now),
        i64::MAX => current == i64::MAX,
        t => current != 0 && current != i64::MAX && current.abs_diff(t) <= 60,
    }
}

/// Whether an unmute that needs no remote change must still reset the row.
fn needs_local_unmute(current: i64, target: i64) -> bool {
    target == 0 && current != 0
}

/// Archiving a pinned chat also has to unpin it on the other devices: they
/// keep the pin otherwise, as the archive mutation does not touch it.
fn archive_needs_unpin(archive: bool, was_pinned: bool) -> bool {
    archive && was_pinned
}

/// Local flags to write when archiving or unarchiving: `(pinned, archived)`.
/// Archiving also unpins, as the official clients do.
fn archive_flags(archive: bool) -> (Option<bool>, Option<bool>) {
    if archive { (Some(false), Some(true)) } else { (None, Some(false)) }
}

/// Flags that restore the state before a failed archive change. `written` is
/// what [`archive_flags`] stored, `current` what the row holds now, `previous`
/// what it held before; a flag we did not write, or that changed since, is
/// left alone.
fn archive_revert_flags(
    written: (Option<bool>, Option<bool>),
    current: (bool, bool),
    previous: (bool, bool),
) -> (Option<bool>, Option<bool>) {
    let revert = |written: Option<bool>, current: bool, previous: bool| {
        written.and_then(|w| guarded_revert(current, w, previous))
    };
    (revert(written.0, current.0, previous.0), revert(written.1, current.1, previous.1))
}

async fn set_archived(sh: &Shared, client: &Arc<Client>, chat_jid: &str, archived: bool) {
    if chat_jid == STATUS_JID {
        return;
    }
    let Some(chat) = parse_jid(sh, chat_jid) else {
        return;
    };
    let previous = match sh.store.chat(chat_jid) {
        Ok(Some(c)) => c,
        Ok(None) => {
            reject(sh, "The chat was not found".to_string());
            return;
        }
        Err(e) => {
            log::error!("chat lookup failed: {e}");
            reject(sh, "Could not read the chat".to_string());
            return;
        }
    };
    if previous.archived == archived {
        sh.emit(UiEvent::ChatsChanged);
        return;
    }
    let (pinned, flag) = archive_flags(archived);
    if let Err(e) = sh.store.set_chat_flags(chat_jid, pinned, flag, None) {
        log::error!("storing archive flag failed: {e}");
        reject(sh, format!("Could not update the chat: {e}"));
        return;
    }
    sh.emit(UiEvent::ChatsChanged);

    // The newest message lets other devices resolve conflicting changes.
    let newest = sh.store.messages_page(chat_jid, None, 1).unwrap_or_else(|e| {
        log::error!("newest message lookup failed: {e}");
        Vec::new()
    });
    let range = match newest.first() {
        Some(row) => {
            let messages = match resolve_participant(sh, client, &chat, row, false).await {
                KeyParticipant::Omit => vec![(message_key(&row.id, &chat, row.from_me, None), row.ts)],
                KeyParticipant::Use(p) => vec![(message_key(&row.id, &chat, row.from_me, Some(&p)), row.ts)],
                KeyParticipant::Skip => Vec::new(),
            };
            Some(message_range(row.ts, None, messages))
        }
        None => None,
    };
    let actions = client.chat_actions();
    let result = with_app_state_timeout(async {
        if archived {
            actions.archive_chat(&chat, range).await
        } else {
            actions.unarchive_chat(&chat, range).await
        }
    })
    .await;
    log_flag_outcome("SetArchived", chat_jid, &result);
    if let Err(e) = result {
        match sh.store.chat(chat_jid) {
            Ok(Some(row)) => {
                let (pinned, flag) = archive_revert_flags(
                    archive_flags(archived),
                    (row.pinned, row.archived),
                    (previous.pinned, previous.archived),
                );
                if pinned.is_some() || flag.is_some() {
                    if let Err(e) = sh.store.set_chat_flags(chat_jid, pinned, flag, None) {
                        log::error!("reverting archive flag failed: {e}");
                    }
                }
            }
            Ok(None) => {}
            Err(e) => log::error!("re-reading the chat for the archive revert failed: {e}"),
        }
        sh.emit(UiEvent::ChatsChanged);
        reject(sh, app_state_failure(&e, if archived { "archive the chat" } else { "unarchive the chat" }));
        return;
    }
    if archive_needs_unpin(archived, previous.pinned) {
        if let Err(e) = actions.unpin_chat(&chat).await {
            crate::jid::warn_lib_error("unpinning the archived chat on other devices failed", &e);
        }
    }
}

/// Tells the account's other devices about a local delete (app-state
/// `deleteMessageForMe`). Best effort: the local delete already happened.
///
/// DMs keep the phone-number chat key the store uses: the library exposes no
/// addressing signal for a one-to-one chat (only groups carry one), so there
/// is nothing to decide the LID form on and a guess is not sent.
async fn sync_delete_for_me(
    sh: &Shared,
    client: &Arc<Client>,
    chat_jid: &str,
    id: &str,
    row: &wa_store::MessageRow,
) {
    if chat_jid == STATUS_JID {
        return;
    }
    let Ok(chat) = chat_jid.parse::<Jid>() else {
        return;
    };
    let participant = match resolve_participant(sh, client, &chat, row, true).await {
        KeyParticipant::Omit => None,
        KeyParticipant::Use(p) => Some(p),
        KeyParticipant::Skip => {
            log::debug!(
                "delete-for-me sync skipped: address form of {} unknown in {}",
                redact(&row.sender_jid),
                redact(chat_jid)
            );
            return;
        }
    };
    if let Err(e) = client
        .chat_actions()
        .delete_message_for_me(&chat, participant.as_ref(), id, row.from_me, true, Some(row.ts))
        .await
    {
        crate::jid::warn_lib_error("syncing delete-for-me to other devices failed", &e);
    }
}

async fn mark_status_viewed(sh: &Shared, client: &Arc<Client>, id: &str) {
    let row = match sh.store.message(STATUS_JID, id) {
        Ok(Some(row)) => row,
        Ok(None) => return,
        Err(e) => {
            log::error!("status lookup failed: {e}");
            return;
        }
    };
    let first_view = match sh.store.mark_status_viewed(id) {
        Ok(first) => first,
        Err(e) => {
            log::error!("mark_status_viewed failed: {e}");
            return;
        }
    };
    sh.emit(UiEvent::StatusesChanged);
    if !first_view || row.from_me || !settings::may_send_status_receipts(&sh.settings()) {
        return;
    }
    let (Ok(chat), Ok(poster)) = (STATUS_JID.parse::<Jid>(), row.sender_jid.parse::<Jid>()) else {
        log::warn!("status receipt skipped: invalid poster {}", redact(&row.sender_jid));
        return;
    };
    if let Err(e) = client.mark_as_read(&chat, Some(&poster), &[id]).await {
        crate::jid::warn_lib_error("status read receipt failed", &e);
    }
}

async fn mark_chat_read(sh: &Shared, client: &Arc<Client>, chat_jid: &str) {
    // Statuses are acknowledged one by one via `MarkStatusViewed`; a bulk
    // receipt for the status feed would tell every poster at once.
    if chat_jid == STATUS_JID {
        return;
    }
    let (unread, receipts_off) = match sh.store.chat(chat_jid) {
        Ok(Some(c)) => (c.unread, c.receipts_off),
        Ok(None) => return,
        Err(e) => {
            log::error!("chat lookup failed: {e}");
            return;
        }
    };
    if unread <= 0 {
        return;
    }
    if !settings::effective_read_receipts(&sh.settings(), receipts_off) {
        // Read receipts are off (globally or for this chat): the chat counts
        // as read here only; its messages stay unreceipted for a later
        // `SendReadReceiptsNow`.
        if let Err(e) = sh.store.mark_chat_read(chat_jid) {
            log::error!("mark_chat_read failed: {e}");
        }
        sh.emit(UiEvent::ChatsChanged);
        return;
    }
    let Some(chat) = parse_jid(sh, chat_jid) else {
        reset_unread_locally(sh, chat_jid);
        return;
    };

    let recent = match sh.store.messages_page(chat_jid, None, MARK_READ_MAX as u32) {
        Ok(rows) => rows,
        Err(e) => {
            log::error!("messages_page failed: {e}");
            return;
        }
    };
    let wanted = usize::try_from(unread.min(MARK_READ_MAX)).unwrap_or(0);
    let items: Vec<(String, String)> = recent
        .iter()
        .rev()
        .filter(|r| !r.from_me)
        .take(wanted)
        .map(|r| (r.id.clone(), r.sender_jid.clone()))
        .collect();
    let (acked, complete) = send_receipts(&chat, chat_jid, client, &items).await;
    record_receipts_sent(sh, chat_jid, &acked);
    // Nothing to acknowledge (no incoming rows, unparseable senders): the
    // badge must not stick just because no receipt could be sent.
    if needs_local_reset(acked.len(), complete) {
        reset_unread_locally(sh, chat_jid);
        return;
    }
    // On a partial failure the rest stays counted so the user can retry by
    // reopening the chat. Messages that arrived while the receipts were in
    // flight stay unread as well.
    if !acked.is_empty() {
        let n = i64::try_from(acked.len()).unwrap_or(i64::MAX);
        if let Err(e) = sh.store.decrement_unread(chat_jid, n) {
            log::error!("decrement_unread failed: {e}");
        }
        sh.emit(UiEvent::ChatsChanged);
    }
}

/// True when every batch went out but no receipt was acknowledged, so the
/// receipts path will never clear the unread counter by itself.
fn needs_local_reset(acked: usize, complete: bool) -> bool {
    complete && acked == 0
}

fn reset_unread_locally(sh: &Shared, chat_jid: &str) {
    if let Err(e) = sh.store.mark_chat_read(chat_jid) {
        log::error!("mark_chat_read failed: {e}");
    }
    sh.emit(UiEvent::ChatsChanged);
}

fn record_receipts_sent(sh: &Shared, chat_jid: &str, ids: &[String]) {
    if ids.is_empty() {
        return;
    }
    if let Err(e) = sh.store.mark_receipts_sent(chat_jid, ids) {
        log::error!("mark_receipts_sent failed: {e}");
    }
}

/// Sends read receipts for `(id, sender_jid)` pairs. Receipts are per sender
/// in groups; a DM has a single (implicit) sender. Returns the ids that were
/// acknowledged and whether every batch went out.
async fn send_receipts(
    chat: &Jid,
    chat_jid: &str,
    client: &Arc<Client>,
    items: &[(String, String)],
) -> (Vec<String>, bool) {
    let is_group = chat.is_group();
    let mut groups: Vec<(Option<Jid>, Vec<String>)> = Vec::new();
    for (id, sender_jid) in items {
        let sender = if is_group {
            match sender_jid.parse::<Jid>() {
                Ok(j) => Some(j),
                Err(_) => continue,
            }
        } else {
            None
        };
        match groups.iter_mut().find(|(s, _)| *s == sender) {
            Some((_, ids)) => ids.push(id.clone()),
            None => groups.push((sender, vec![id.clone()])),
        }
    }
    let mut acked = Vec::new();
    for (sender, ids) in groups {
        let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        if let Err(e) = client.mark_as_read(chat, sender.as_ref(), &id_refs).await {
            crate::jid::warn_lib_error(&format!("mark_as_read failed for {}", redact(chat_jid)), &e);
            return (acked, false);
        }
        acked.extend(ids);
    }
    (acked, true)
}

/// Explicit "mark as read now": sends the receipts that were withheld, no
/// matter the global or per-chat setting.
async fn send_read_receipts_now(sh: &Shared, client: &Arc<Client>, chat_jid: &str) {
    if chat_jid == STATUS_JID {
        return;
    }
    let Some(chat) = parse_jid(sh, chat_jid) else {
        return;
    };
    let since = now_secs() - RECEIPTS_NOW_WINDOW_SECS;
    let items = match sh.store.unreceipted_incoming(chat_jid, since, RECEIPTS_NOW_MAX) {
        Ok(rows) => rows,
        Err(e) => {
            log::error!("unreceipted_incoming failed: {e}");
            return;
        }
    };
    let (acked, complete) = send_receipts(&chat, chat_jid, client, &items).await;
    record_receipts_sent(sh, chat_jid, &acked);
    log::info!("SendReadReceiptsNow {}: {} receipts sent", redact(chat_jid), acked.len());
    if !complete {
        sh.emit(UiEvent::Error {
            message: "Could not send read receipts".to_string(),
        });
        return;
    }
    if let Err(e) = sh.store.mark_chat_read(chat_jid) {
        log::error!("mark_chat_read failed: {e}");
    }
    sh.emit(UiEvent::ChatsChanged);
}

fn set_chat_receipts(sh: &Shared, chat_jid: &str, enabled: bool) {
    match sh.store.set_chat_receipts_off(chat_jid, !enabled) {
        Ok(()) => {
            log::info!("SetChatReceipts {}: enabled={enabled}", redact(chat_jid));
            sh.emit(UiEvent::ChatsChanged);
        }
        Err(e) => log::error!("set_chat_receipts_off failed: {e}"),
    }
}

async fn set_typing(sh: &Shared, client: &Arc<Client>, chat_jid: &str, composing: bool) {
    if !settings::may_send_typing(&sh.settings()) {
        return;
    }
    let Some(chat) = parse_jid(sh, chat_jid) else {
        return;
    };
    let result = if composing {
        client.chatstate().send_composing(&chat).await
    } else {
        client.chatstate().send_paused(&chat).await
    };
    if let Err(e) = result {
        log::debug!("typing indicator for {} failed: {e}", redact(chat_jid));
    }
}

/// Brings our presence in line with the current settings.
pub async fn sync_presence(sh: Shared, client: Arc<Client>) {
    let result = if settings::may_send_presence(&sh.settings()) {
        client.presence().set_available().await
    } else {
        client.presence().set_unavailable().await
    };
    if let Err(e) = result {
        crate::jid::warn_lib_error("failed to update presence", &e);
    }
}

/// When "show me as online" is off, answers an automatic `available` from the
/// library with `unavailable`: right away, and once more shortly after, since
/// the library's own send may land after this call. Settings are re-read
/// before the second send so a switch-on in between is respected.
pub fn enforce_presence_off(sh: &Shared, client: &Arc<Client>) {
    if !settings::must_assert_unavailable(&sh.settings()) {
        return;
    }
    let (sh, client) = (sh.clone(), client.clone());
    let tasks = sh.tasks.clone();
    tasks.spawn(async move {
        for round in 0..2 {
            if round > 0 {
                tokio::time::sleep(settings::PRESENCE_REASSERT_DELAY).await;
            }
            if !settings::must_assert_unavailable(&sh.settings()) {
                return;
            }
            if let Err(e) = client.presence().set_unavailable().await {
                crate::jid::warn_lib_error("failed to keep presence unavailable", &e);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unread_is_reset_locally_only_when_nothing_could_be_acknowledged() {
        assert!(needs_local_reset(0, true), "no candidates");
        assert!(!needs_local_reset(3, true));
        assert!(!needs_local_reset(0, false), "failed send stays retryable");
        assert!(!needs_local_reset(2, false));
    }

    #[test]
    fn delete_for_me_participant_follows_the_group_addressing_mode() {
        let pn = Jid::pn("4917011");
        let lid = Jid::lid("100");
        let both = |mode| key_participant(true, false, mode, Some(pn.clone()), Some(lid.clone()));
        assert_eq!(both(Some(AddressingMode::Lid)), KeyParticipant::Use(lid.clone()));
        assert_eq!(both(Some(AddressingMode::Pn)), KeyParticipant::Use(pn.clone()));
        assert_eq!(both(None), KeyParticipant::Skip, "mode unknown");

        // Mapping unknown: only the form that is not needed may be missing.
        let only_pn = |mode| key_participant(true, false, mode, Some(pn.clone()), None);
        assert_eq!(only_pn(Some(AddressingMode::Lid)), KeyParticipant::Skip);
        assert_eq!(only_pn(Some(AddressingMode::Pn)), KeyParticipant::Use(pn.clone()));
        let only_lid = |mode| key_participant(true, false, mode, None, Some(lid.clone()));
        assert_eq!(only_lid(Some(AddressingMode::Lid)), KeyParticipant::Use(lid.clone()));
        assert_eq!(only_lid(Some(AddressingMode::Pn)), KeyParticipant::Skip);
        assert_eq!(key_participant(true, false, Some(AddressingMode::Pn), None, None), KeyParticipant::Skip);

        // Own messages and DMs carry no participant, whatever else is known.
        assert_eq!(
            key_participant(true, true, Some(AddressingMode::Lid), None, None),
            KeyParticipant::Omit
        );
        assert_eq!(key_participant(false, false, None, None, None), KeyParticipant::Omit);
    }

    #[test]
    fn pinning_is_limited_and_refused_for_archived_chats() {
        assert_eq!(check_pin(2, false), Ok(()));
        assert_eq!(check_pin(3, false), Err("You can only pin up to 3 chats"));
        assert_eq!(check_pin(0, true), Err("Unarchive the chat to pin it"));
    }

    #[test]
    fn mute_until_maps_to_app_state_values() {
        let now = 1_000;
        assert_eq!(mute_change(None, now), MuteChange::Unmute);
        assert_eq!(mute_change(Some(i64::MAX), now), MuteChange::Forever);
        assert_eq!(mute_change(Some(now + 60), now), MuteChange::UntilMs((now + 60) * 1000));
        assert_eq!(mute_change(Some(now), now), MuteChange::Unmute, "already expired");
    }

    #[test]
    fn archiving_unpins_and_a_failure_restores_the_previous_flags() {
        assert_eq!(archive_flags(true), (Some(false), Some(true)));
        assert_eq!(archive_flags(false), (None, Some(false)), "unarchiving keeps the pin state");
    }

    #[test]
    fn a_revert_only_undoes_our_own_unchanged_write() {
        assert_eq!(guarded_revert(true, true, false), Some(false));
        assert_eq!(guarded_revert(false, true, false), None, "a newer change won");

        let archiving = archive_flags(true);
        assert_eq!(
            archive_revert_flags(archiving, (false, true), (true, false)),
            (Some(true), Some(false))
        );
        assert_eq!(
            archive_revert_flags(archiving, (true, true), (true, false)),
            (None, Some(false)),
            "a pin set meanwhile stays"
        );
        assert_eq!(
            archive_revert_flags(archiving, (false, false), (true, false)),
            (Some(true), None),
            "an unarchive that arrived meanwhile stays"
        );
        assert_eq!(
            archive_revert_flags(archive_flags(false), (true, false), (true, true)),
            (None, Some(true)),
            "unarchiving never wrote the pin flag"
        );
    }

    #[test]
    fn a_mute_request_that_changes_nothing_is_detected() {
        let now = 1_000;
        assert!(needs_local_unmute(now - 5, 0), "an expired mute is reset locally");
        assert!(!needs_local_unmute(0, 0));
        assert!(!needs_local_unmute(now + 5, i64::MAX));
        assert_eq!(mute_change(None, now), MuteChange::Unmute);
        assert!(mute_is_noop(0, 0, now), "unmute when not muted");
        assert!(mute_is_noop(now - 5, 0, now), "an expired mute is not muted");
        assert!(!mute_is_noop(now + 500, 0, now));
        assert!(!mute_is_noop(i64::MAX, 0, now));
        assert!(mute_is_noop(i64::MAX, i64::MAX, now));
        assert!(!mute_is_noop(now + 500, i64::MAX, now));
        assert!(mute_is_noop(now + 500, now + 530, now), "within 60 s");
        assert!(!mute_is_noop(now + 500, now + 600, now));
        assert!(!mute_is_noop(0, now + 500, now));
        assert!(!mute_is_noop(i64::MAX, now + 500, now));
    }

    #[test]
    fn archiving_a_pinned_chat_requests_an_unpin() {
        assert!(archive_needs_unpin(true, true));
        assert!(!archive_needs_unpin(true, false));
        assert!(!archive_needs_unpin(false, true), "unarchive does not touch the pin");
    }

    #[test]
    fn delete_for_everyone_requires_an_own_recent_message() {
        let now = 1_000_000;
        assert_eq!(check_revoke(true, false, now - 60, now), Ok(()));
        assert_eq!(
            check_revoke(true, false, now - REVOKE_MAX_AGE_SECS, now),
            Ok(()),
            "exactly at the limit is still allowed"
        );
        assert_eq!(
            check_revoke(false, false, now, now),
            Err("You can only delete your own messages for everyone")
        );
        assert_eq!(
            check_revoke(true, false, now - REVOKE_MAX_AGE_SECS - 1, now),
            Err("Too old to delete for everyone")
        );
        assert_eq!(
            check_revoke(false, false, now - REVOKE_MAX_AGE_SECS - 1, now),
            Err("You can only delete your own messages for everyone"),
            "ownership is checked first"
        );
        assert_eq!(check_revoke(true, true, now, now), Err("This message was already deleted"));
        assert_eq!(check_revoke(true, false, now + 5, now), Ok(()), "clock skew");
    }

    #[test]
    fn reaction_key_carries_the_author_only_where_required() {
        let dm = reaction_key("1@s.whatsapp.net", "M1", false, "1@s.whatsapp.net", false);
        assert_eq!(dm.remote_jid.as_deref(), Some("1@s.whatsapp.net"));
        assert_eq!(dm.id.as_deref(), Some("M1"));
        assert_eq!(dm.from_me, Some(false));
        assert_eq!(dm.participant, None);

        let group = reaction_key("1-2@g.us", "M2", false, "9@s.whatsapp.net", true);
        assert_eq!(group.participant.as_deref(), Some("9@s.whatsapp.net"));
        let own = reaction_key("1-2@g.us", "M3", true, "me@s.whatsapp.net", true);
        assert_eq!(own.from_me, Some(true));
        assert_eq!(own.participant.as_deref(), Some("me@s.whatsapp.net"));
    }
}
