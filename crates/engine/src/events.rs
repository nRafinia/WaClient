//! whatsapp-rust events -> `wa_store` mirror + [`UiEvent`]s.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc::UnboundedSender;
use whatsapp_rust::buffa::Message as _;
use whatsapp_rust::prelude::{
    wa, BatchOrigin, Client, Event, InboundMessage, Jid, MessageBatch,
};
use whatsapp_rust::wacore::poll::PollVoteCiphertext;
use whatsapp_rust::types::presence::{ChatPresence, ReceiptType};
use whatsapp_rust::wacore_binary::JidExt as _;
use wa_store::{ChatKind, ContactPatch, MessageStatus, Store, STATUS_JID};

use crate::api::{ConnState, EngineSettings, UiEvent};
use crate::coalesce::Coalescer;
use crate::convert::{PollVoteInfo, ProtocolAction, ReactionInfo};
use crate::groups::{self, GroupNames};
use crate::media::{self, MediaState};
use crate::merge::LidMerges;
use crate::pending::{Pending, PendingAction, PendingActions};
use crate::tasks::GenTasks;
use crate::jid::redact;
use crate::{avatar, commands, convert, history, jid, settings};

/// Notifications from the event handlers to the supervisor loop.
#[derive(Debug)]
pub enum Signal {
    /// The session ended (server-side or because we logged out).
    LoggedOut { reason: String },
    /// The QR codes ran out; pairing must be restarted from scratch.
    RestartPairing,
    /// WhatsApp refused or displaced this client; the bot must stop and must
    /// not connect again on its own. `recoverable` allows a manual
    /// [`crate::api::Command::Reconnect`] (another client took over the
    /// session); bans and outdated versions never do.
    Halt { recoverable: bool },
}

/// Runs `job` on a store handle while also holding a UI sender clone, and
/// releases the store first. The UI treats "event channel closed" as "no store
/// handle is left" (it deletes the profile then), so a blocking closure that
/// holds the `Arc<Store>` must keep the channel open until it dropped it.
pub fn with_store_and_ui<T>(
    store: Arc<Store>,
    ui: UnboundedSender<UiEvent>,
    job: impl FnOnce(&Store) -> T,
) -> T {
    let out = job(&store);
    drop(store);
    drop(ui);
    out
}

/// State shared by every event handler of one bot generation.
#[derive(Clone)]
pub struct Shared {
    pub store: Arc<Store>,
    pub ui: UnboundedSender<UiEvent>,
    pub signals: UnboundedSender<Signal>,
    /// Serializes history chunks so they cannot interleave their writes.
    pub history_lock: Arc<tokio::sync::Mutex<()>>,
    /// Media cache, download queue and avatar throttling.
    pub media: Arc<MediaState>,
    /// Rate limiter for `ChatsChanged` / `MessagesChanged`.
    coalescer: Coalescer,
    /// Group subject lookups of this generation.
    pub(crate) groups: Arc<GroupNames>,
    /// LIDs already folded into their phone-number chat.
    pub(crate) lids: Arc<LidMerges>,
    /// Revokes/edits waiting for a message that is not stored yet.
    pub(crate) pending: Arc<PendingActions>,
    /// Background tasks holding the client; aborted when the generation ends.
    pub(crate) tasks: Arc<GenTasks>,
    /// All traffic goes through a proxy, so hosts must not be resolved locally.
    pub(crate) proxied: bool,
    /// For a generation that follows an outage: the messages that were pending
    /// when it started (a snapshot, so nothing queued afterwards is in it).
    /// Taken on the first `Connected` and re-delivered.
    pub(crate) requeue_pending: Arc<std::sync::Mutex<Option<Vec<wa_store::PendingRow>>>>,
    /// Contact / group info lookups of this generation.
    pub(crate) info: Arc<crate::info::InfoCache>,
    /// Last known contact presence and subscription pacing.
    pub(crate) presence: Arc<crate::presence::PresenceState>,
    /// App-state resyncs scheduled in this generation (see `appstate`).
    pub(crate) appstate_resyncs: Arc<std::sync::atomic::AtomicU32>,
    /// Block list cache of this generation (see `account`).
    pub(crate) account: Arc<crate::account::AccountState>,
    /// Calls that are ringing (see `calls`).
    pub(crate) calls: Arc<crate::calls::CallState>,
    /// User preferences; shared by every generation.
    settings: Arc<std::sync::Mutex<EngineSettings>>,
}

impl Shared {
    pub fn new(
        store: Arc<Store>,
        ui: UnboundedSender<UiEvent>,
        signals: UnboundedSender<Signal>,
        history_lock: Arc<tokio::sync::Mutex<()>>,
        media: Arc<MediaState>,
        settings: Arc<std::sync::Mutex<EngineSettings>>,
    ) -> Self {
        Shared {
            store,
            coalescer: Coalescer::new(ui.clone()),
            ui,
            signals,
            history_lock,
            media,
            groups: Arc::new(GroupNames::default()),
            lids: Arc::new(LidMerges::default()),
            pending: Arc::new(PendingActions::default()),
            tasks: Arc::new(GenTasks::default()),
            proxied: false,
            requeue_pending: Arc::new(std::sync::Mutex::new(None)),
            info: Arc::new(crate::info::InfoCache::default()),
            presence: Arc::new(crate::presence::PresenceState::default()),
            appstate_resyncs: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            account: Arc::new(crate::account::AccountState::default()),
            calls: Arc::new(crate::calls::CallState::default()),
            settings,
        }
    }

    /// Snapshot of the current settings.
    pub fn settings(&self) -> EngineSettings {
        media::lock(&self.settings).clone()
    }

    pub fn set_settings(&self, new: EngineSettings) {
        *media::lock(&self.settings) = new;
    }

    /// Sends an event to the UI. `ChatsChanged`, `StatusesChanged` and
    /// `MessagesChanged` are coalesced; everything else is delivered immediately.
    pub fn emit(&self, ev: UiEvent) {
        match ev {
            UiEvent::ChatsChanged => self.coalescer.chats_changed(),
            UiEvent::StatusesChanged => self.coalescer.statuses_changed(),
            UiEvent::MessagesChanged { chat_jid } => self.coalescer.messages_changed(chat_jid),
            other => {
                if self.ui.send(other).is_err() {
                    log::debug!("UI receiver is gone, dropping event");
                }
            }
        }
    }

    /// Records a LID<->PN mapping and merges any data stored under the LID.
    pub fn learn_lid(&self, lid: &str, pn: &str) {
        if self.lids.learn(&self.store, lid, pn) {
            self.emit(UiEvent::ChatsChanged);
        }
    }

    fn signal(&self, sig: Signal) {
        if self.signals.send(sig).is_err() {
            log::debug!("supervisor is gone, dropping signal");
        }
    }
}

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Our own account as stored in `sender_jid` of our messages; `None` while the
/// session has no phone number yet (not paired / not logged in).
pub fn own_jid(client: &Client) -> Option<String> {
    client.pn().map(|j| j.to_non_ad().to_string())
}

fn describe_duration(d: Duration) -> String {
    let mins = d.as_secs() / 60;
    match mins {
        0 => "less than a minute".to_string(),
        1..=119 => format!("{mins} minutes"),
        _ if mins < 48 * 60 => format!("{} hours", mins / 60),
        _ => format!("{} days", mins / (24 * 60)),
    }
}

pub async fn handle(sh: Shared, event: Arc<Event>, client: Arc<Client>) {
    match &*event {
        Event::PairingQrCode(qr) => {
            log::info!("pairing QR received (valid {}s)", qr.timeout.as_secs());
            sh.emit(UiEvent::Qr { code: qr.code.clone() });
        }
        Event::PairSuccess(ok) => {
            log::debug!("paired as {}", ok.id.to_non_ad());
            log::info!("pairing succeeded");
            sh.learn_lid(&ok.lid.to_string(), &ok.id.to_string());
            sh.emit(UiEvent::Paired {
                jid: ok.id.to_non_ad().to_string(),
            });
        }
        Event::PairError(e) => {
            log::warn!("pairing failed: {}", e.error);
            sh.emit(UiEvent::Error {
                message: format!("Pairing failed: {}", e.error),
            });
        }
        Event::PairingQrCodesExhausted(_) => {
            log::info!("QR codes exhausted, restarting pairing");
            sh.signal(Signal::RestartPairing);
        }
        Event::Connected(_) => {
            log::info!("connected");
            sh.emit(UiEvent::Connection(ConnState::Connected));
            if settings::may_send_presence(&sh.settings()) {
                if let Err(e) = client.presence().set_available().await {
                    crate::jid::warn_lib_error("failed to set presence available", &e);
                }
            } else {
                // The library already announced `available` before this event.
                commands::enforce_presence_off(&sh, &client);
            }
            groups::spawn_initial_fetch(sh.clone(), client.clone());
            // Once per generation, and only after an outage: a normal library
            // reconnect leaves the live sender tasks in charge of their messages.
            let snapshot = media::lock(&sh.requeue_pending).take();
            if let Some(rows) = snapshot {
                sh.tasks.spawn(crate::pending::requeue_outgoing(sh.clone(), client.clone(), rows));
            }
        }
        // The library sends `available` by itself after a push-name sync or
        // change; undo that when presence is off.
        Event::SelfPushNameUpdated(_) => commands::enforce_presence_off(&sh, &client),
        Event::Disconnected(d) => {
            // The library reconnects by itself (and the supervisor's watchdog
            // rebuilds the generation if that drags on), so this is "Connecting",
            // not a dead end with a manual button. Details stay at debug.
            let class = crate::runtime::classify_disconnect(&d.reason);
            log::warn!("connection lost ({}), reconnecting", class.label());
            log::debug!("disconnect detail: {}", d.reason);
            // Only a paired session is watched and rebuilt; while pairing the
            // old behaviour (state with reason, manual button) stays.
            sh.emit(UiEvent::Connection(if client.pn().is_some() {
                ConnState::Connecting
            } else {
                ConnState::Disconnected { reason: d.reason.to_string() }
            }));
        }
        Event::LoggedOut(lo) => {
            let reason = lo
                .logout_message
                .as_ref()
                .and_then(|m| m.header.clone())
                .unwrap_or_else(|| format!("Logged out ({:?})", lo.reason));
            log::info!("logged out: {:?}", lo.reason);
            crate::calls::clear(&sh);
            sh.signal(Signal::LoggedOut { reason });
        }
        Event::TemporaryBan(ban) => {
            log::error!("temporary ban: {}", ban.code);
            sh.signal(Signal::Halt { recoverable: false });
            sh.emit(UiEvent::Fatal {
                message: format!(
                    "WhatsApp temporarily banned this account for {}: {}",
                    describe_duration(ban.expire.to_std().unwrap_or_default()),
                    ban.code
                ),
            });
        }
        Event::ClientOutdated(_) => {
            log::error!("client version rejected as outdated");
            sh.signal(Signal::Halt { recoverable: false });
            sh.emit(UiEvent::Fatal {
                message: "WhatsApp rejected this client version as outdated. Update the app."
                    .to_string(),
            });
        }
        Event::StreamReplaced(_) => {
            log::warn!("stream replaced by another connection");
            crate::calls::clear(&sh);
            sh.signal(Signal::Halt { recoverable: true });
            sh.emit(UiEvent::Connection(ConnState::Disconnected {
                reason: "This session was replaced by another connection. Close the other \
                         instance, then reconnect."
                    .to_string(),
            }));
        }
        // The library retries only the reasons it classifies as retryable;
        // for those we stay out of its way.
        Event::ConnectFailure(f) if !f.reason.is_logged_out() && f.reason.should_reconnect() => {
            log::warn!("connect failure, the library will retry: {:?}", f.reason);
        }
        Event::ConnectFailure(f) if !f.reason.is_logged_out() => {
            log::error!("connect failure: {:?} {:?}", f.reason, f.message);
            sh.signal(Signal::Halt { recoverable: false });
            sh.emit(UiEvent::Fatal {
                message: format!(
                    "Connection to WhatsApp failed ({:?}){}",
                    f.reason,
                    f.message
                        .as_deref()
                        .map(|m| format!(": {m}"))
                        .unwrap_or_default()
                ),
            });
        }
        Event::Messages(batch) => on_messages(&sh, &client, batch).await,
        Event::Receipt(r) => on_receipt(&sh, &client, r).await,
        Event::ChatPresence(p) => {
            if p.source.is_from_me {
                return;
            }
            let chat = jid::normalize(&client, &p.source.chat, None).await;
            let sender = jid::normalize(&client, &p.source.sender, None).await;
            sh.emit(UiEvent::Typing {
                chat_jid: chat,
                sender_jid: sender,
                composing: p.state == ChatPresence::Composing,
            });
        }
        Event::Presence(u) => crate::presence::on_presence(&sh, &client, &u).await,
        Event::PushNameUpdate(u) => {
            if u.new_push_name.is_empty() {
                return;
            }
            let who = jid::normalize(&client, &u.jid, None).await;
            upsert_contact(
                &sh.store,
                ContactPatch {
                    jid: who,
                    full_name: None,
                    push_name: Some(u.new_push_name.clone()),
                    phone: None,
                },
            );
            sh.emit(UiEvent::ChatsChanged);
        }
        Event::ContactUpdate(u) => on_contact_update(&sh, &client, u).await,
        Event::GroupUpdate(u) => {
            if let whatsapp_rust::wacore::stanza::groups::GroupNotificationAction::Ephemeral { expiration, .. } = &u.action {
                crate::ephemeral::on_group_setting(&sh, &u.group_jid, *expiration, u.timestamp.timestamp());
            }
            // Our own PN/LID, so notices can say "You" (cheap; refreshed per event
            // so it is right after pairing or a relogin).
            sh.groups.set_own(own_jid(&client).into_iter().chain(client.lid().map(|j| j.to_non_ad().to_string())));
            groups::on_group_update(&sh, u);
            // Changes that alter what we may do in the group: refresh its info
            // so the UI (composer gate, open panel) updates without a reopen.
            if affects_our_rights(&client, &u.action) {
                sh.tasks.spawn(crate::info::fetch(sh.clone(), client.clone(), u.group_jid.to_string()));
            }
        }
        Event::DisappearingModeChanged(ev) => crate::ephemeral::on_mode_changed(&sh, &client, ev).await,
        Event::PictureUpdate(u) => {
            let who = jid::normalize(&client, &u.jid, None).await;
            sh.info.invalidate(&who);
            avatar::invalidate_and_refetch(&sh, &client, who);
        }
        Event::PinUpdate(u) => {
            let chat = jid::normalize(&client, &u.jid, None).await;
            set_flags(&sh, &chat, u.action.pinned, None, None);
        }
        Event::StarUpdate(u) => crate::msgops::on_star_update(&sh, &client, u).await,
        Event::ArchiveUpdate(u) => {
            let chat = jid::normalize(&client, &u.jid, None).await;
            set_flags(&sh, &chat, None, u.action.archived, None);
        }
        Event::MuteUpdate(u) => {
            let chat = jid::normalize(&client, &u.jid, None).await;
            let until = if u.action.muted == Some(true) {
                mute_until(u.action.mute_end_timestamp.unwrap_or(-1))
            } else {
                0
            };
            set_flags(&sh, &chat, None, None, Some(until));
        }
        Event::MarkChatAsReadUpdate(u) => {
            let chat = jid::normalize(&client, &u.jid, None).await;
            match u.action.read {
                Some(true) => {
                    if let Err(e) = sh.store.mark_chat_read(&chat) {
                        log::error!("mark_chat_read failed: {e}");
                    }
                    sh.emit(UiEvent::ChatsChanged);
                }
                Some(false) => match sh.store.chat(&chat) {
                    Ok(Some(row)) => {
                        let wanted = unread_after_phone_mark(row.unread);
                        if wanted != row.unread {
                            if let Err(e) = sh.store.set_unread(&chat, wanted) {
                                log::error!("storing the unread flag failed: {e}");
                            }
                            sh.emit(UiEvent::ChatsChanged);
                        }
                    }
                    Ok(None) => {}
                    Err(e) => log::error!("reading the chat for a synced unread mark failed: {e}"),
                },
                None => {}
            }
        }
        // App-state events only ever describe the current account: the store
        // and the session are per account.
        Event::DeleteMessageForMeUpdate(u) => {
            let chat = jid::normalize(&client, &u.chat_jid, None).await;
            apply_deletion(&sh, &chat, ChatDeletion::Message(u.message_id.clone()));
        }
        Event::ClearChatUpdate(u) => {
            let chat = jid::normalize(&client, &u.jid, None).await;
            apply_deletion(&sh, &chat, ChatDeletion::clear(u.delete_starred));
        }
        Event::DeleteChatUpdate(u) => {
            let chat = jid::normalize(&client, &u.jid, None).await;
            apply_deletion(&sh, &chat, ChatDeletion::Chat);
        }
        Event::HistorySync(lazy) => history::spawn(sh.clone(), client.clone(), (**lazy).clone()),
        Event::AppStateSyncFailed(f) => crate::appstate::on_sync_failed(&sh, &client, f),
        Event::OfflineSyncCompleted(_) => {
            log::info!("offline sync completed");
            sh.emit(UiEvent::ChatsChanged);
        }
        Event::IncomingCall(call) => crate::calls::on_incoming(&sh, &client, call).await,
        Event::MissedCall(m) => crate::calls::on_missed(&sh, &client, m).await,
        Event::CallEndedElsewhere(e) => crate::calls::on_ended_elsewhere(&sh, e),
        _ => {}
    }
}

/// The unread counter after the phone marked a chat unread: at least one.
pub fn unread_after_phone_mark(current: i64) -> i64 {
    current.max(1)
}

/// A local removal requested by another device through app-state sync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatDeletion {
    /// "Delete for me" of one message.
    Message(String),
    /// All messages go, the chat stays.
    Clear,
    /// All messages but the starred ones go, the chat stays.
    ClearKeepStarred,
    /// The chat and everything in it goes.
    Chat,
}

impl ChatDeletion {
    /// Maps a `ClearChat` payload: starred messages stay unless the phone
    /// asked to delete them too.
    pub fn clear(delete_starred: bool) -> Self {
        if delete_starred { ChatDeletion::Clear } else { ChatDeletion::ClearKeepStarred }
    }
}

/// Applies a deletion to the store. Returns the cached media paths of the
/// removed messages; `None` when the store failed or a message was unknown.
fn run_deletion(store: &Store, chat: &str, d: &ChatDeletion) -> Option<Vec<String>> {
    let result = match d {
        ChatDeletion::Message(id) => store.message(chat, id).and_then(|row| match row {
            Some(_) => store.delete_message(chat, id).map(|p| Some(p.into_iter().collect())),
            None => Ok(None),
        }),
        ChatDeletion::Clear => store.clear_chat(chat, None).map(Some),
        ChatDeletion::ClearKeepStarred => store.clear_chat_keep_starred(chat, None).map(Some),
        ChatDeletion::Chat => store.delete_chat(chat).map(Some),
    };
    match result {
        Ok(paths) => paths,
        Err(e) => {
            log::error!("applying {d:?} to {} failed: {e}", redact(chat));
            None
        }
    }
}

fn apply_deletion(sh: &Shared, chat: &str, d: ChatDeletion) {
    log::debug!("synced deletion {d:?} for {}", redact(chat));
    let Some(paths) = run_deletion(&sh.store, chat, &d) else {
        return;
    };
    for path in &paths {
        crate::commands::remove_cached_file(sh, path);
    }
    sh.emit(UiEvent::MessagesChanged {
        chat_jid: chat.to_string(),
    });
    sh.emit(UiEvent::ChatsChanged);
}

/// Converts WhatsApp's mute end (seconds, or milliseconds on some paths; `-1`
/// or a huge value means forever) to unix seconds for the store.
pub fn mute_until(raw: i64) -> i64 {
    if raw < 0 || raw > 4_000_000_000_000 {
        i64::MAX
    } else if raw > 100_000_000_000 {
        raw / 1000
    } else {
        raw
    }
}

fn upsert_contact(store: &Store, patch: ContactPatch) {
    if let Err(e) = store.upsert_contact(&patch) {
        log::error!("upsert_contact failed: {e}");
    }
}

fn set_flags(
    sh: &Shared,
    chat: &str,
    pinned: Option<bool>,
    archived: Option<bool>,
    muted_until: Option<i64>,
) {
    if let Err(e) = sh.store.set_chat_flags(chat, pinned, archived, muted_until) {
        log::error!("set_chat_flags failed: {e}");
    }
    sh.emit(UiEvent::ChatsChanged);
}

async fn on_contact_update(
    sh: &Shared,
    client: &Client,
    u: &whatsapp_rust::wacore::types::events::ContactUpdate,
) {
    let pn = u.action.pn_jid.as_deref().and_then(|j| j.parse::<Jid>().ok());
    if u.jid.is_lid()
        && let Some(pn) = &pn
    {
        sh.learn_lid(&u.jid.to_string(), &pn.to_string());
    }
    let who = jid::normalize(client, &u.jid, pn.as_ref()).await;
    let phone = who
        .strip_suffix("@s.whatsapp.net")
        .map(|user| format!("+{user}"));
    let full_name = u
        .action
        .full_name
        .clone()
        .filter(|n| !n.is_empty())
        .or_else(|| u.action.first_name.clone().filter(|n| !n.is_empty()));
    if full_name.is_none() && phone.is_none() {
        return;
    }
    upsert_contact(
        &sh.store,
        ContactPatch {
            jid: who,
            full_name,
            push_name: None,
            phone,
        },
    );
    sh.emit(UiEvent::ChatsChanged);
}

async fn on_receipt(
    sh: &Shared,
    client: &Client,
    r: &whatsapp_rust::wacore::types::events::Receipt,
) {
    let status = match r.r#type {
        ReceiptType::Delivered => MessageStatus::Delivered,
        ReceiptType::Read => MessageStatus::Read,
        ReceiptType::Played => MessageStatus::Played,
        _ => return,
    };
    if r.source.is_from_me {
        return;
    }
    let chat = jid::normalize(client, &r.source.chat, None).await;
    let group_who = crate::msgops::receipt_sender(client, r, &chat).await;
    let is_group = chat.ends_with("@g.us");
    let keys = crate::msgops::receipt_chat_keys(client, &r.source.chat, &chat).await;
    for id in &r.message_ids {
        // The row may live under another form of the chat's address.
        let chat = crate::msgops::receipt_chat(&sh.store, &keys, id).unwrap_or_else(|| chat.clone());
        let who = if is_group { &group_who } else { &chat };
        crate::msgops::record_receipt(&sh.store, &chat, id, who, status, r.timestamp.timestamp());
        if let Err(e) = sh.store.update_status(&chat, id, status) {
            log::error!("update_status failed: {e}");
            continue;
        }
        sh.emit(UiEvent::MessageStatus {
            chat_jid: chat.clone(),
            id: id.clone(),
            status,
        });
    }
}

async fn on_messages(sh: &Shared, client: &Arc<Client>, batch: &MessageBatch) {
    let live = matches!(batch.origin, BatchOrigin::Live);
    let changed = if live {
        let mut changed: Vec<String> = Vec::new();
        for m in batch.iter() {
            if let Some(chat) = store_live(sh, client, m).await
                && !changed.contains(&chat)
            {
                changed.push(chat);
            }
        }
        changed
    } else {
        store_offline_batch(sh, client, batch).await
    };
    if changed.is_empty() {
        return;
    }
    let mut chats_touched = false;
    for chat_jid in changed {
        if chat_jid == STATUS_JID {
            sh.emit(UiEvent::StatusesChanged);
        } else {
            sh.emit(UiEvent::MessagesChanged { chat_jid });
            chats_touched = true;
        }
    }
    if chats_touched {
        sh.emit(UiEvent::ChatsChanged);
    }
}

/// A decoded inbound message, ready for the store.
enum Inbound {
    Message { msg: wa_store::NewMessage, push_name: String },
    Action { chat: String, sender: String, from_me: bool, action: ProtocolAction },
    Reaction { chat: String, sender: String, from_me: bool, reaction: ReactionInfo, ts: i64 },
    PollVote { chat: String, voter: String, from_me: bool, vote: PollVoteInfo, ts: i64 },
    Pin { chat: String, change: crate::msgops::PinChange, ts: i64 },
    /// A 1:1 chat's disappearing timer was changed (`EPHEMERAL_SETTING`).
    Timer { chat: String, sender: String, from_me: bool, secs: i64, ts: i64 },
}

/// Resolves JIDs, records the sender's push name and converts the message.
/// `None` for things we do not store (newsletters, unsupported kinds).
async fn prepare_inbound(sh: &Shared, client: &Arc<Client>, m: &InboundMessage) -> Option<Inbound> {
    let info = &m.info;
    let src = &info.source;
    if src.chat.is_newsletter() {
        log::debug!("ignoring newsletter message {}", info.id);
        return None;
    }

    let hints = jid::hints(info);
    learn_from_hints(sh, &src.chat, hints.chat.as_ref());
    learn_from_hints(sh, &src.sender, hints.sender.as_ref());
    let chat = jid::normalize(client, &src.chat, hints.chat.as_ref()).await;
    let sender = if src.is_from_me {
        let Some(own) = own_jid(client) else {
            log::warn!("dropping own message {}: account is unknown", info.id);
            return None;
        };
        own
    } else if src.is_group || src.chat.is_group() || src.chat.is_status_broadcast() {
        // The poster of a status is the sender, as in a group.
        jid::normalize(client, &src.sender, hints.sender.as_ref()).await
    } else {
        chat.clone()
    };

    if !src.is_from_me && !info.push_name.is_empty() {
        upsert_contact(
            &sh.store,
            ContactPatch {
                jid: sender.clone(),
                full_name: None,
                push_name: Some(info.push_name.clone()),
                phone: None,
            },
        );
    }

    if let Some(action) = convert::protocol_action(&m.message) {
        return Some(Inbound::Action {
            chat,
            sender,
            from_me: src.is_from_me,
            action,
        });
    }
    let ts = info.timestamp.timestamp();
    if !src.chat.is_group()
        && let Some((secs, setting_ts)) = crate::ephemeral::setting_change(&m.message)
    {
        return Some(Inbound::Timer {
            chat,
            sender,
            from_me: src.is_from_me,
            secs: i64::from(secs),
            ts: setting_ts.filter(|t| *t > 0).unwrap_or(ts),
        });
    }
    if let Some(change) = crate::msgops::pin_change(&m.message) {
        return Some(Inbound::Pin { chat, change, ts });
    }
    if let Some(reaction) = convert::reaction(&m.message) {
        let ts = reaction.ts_ms.map_or(ts, |ms| ms / 1000);
        return Some(Inbound::Reaction {
            chat,
            sender,
            from_me: src.is_from_me,
            reaction,
            ts,
        });
    }
    if let Some(vote) = convert::poll_vote(&m.message) {
        let ts = vote.ts_ms.map_or(ts, |ms| ms / 1000);
        return Some(Inbound::PollVote {
            chat,
            voter: sender,
            from_me: src.is_from_me,
            vote,
            ts,
        });
    }
    let Some(mut msg) = convert::from_inbound(&m.message, info, &chat, &sender) else {
        log::debug!("skipping non-displayable message {} in {chat}", info.id);
        return None;
    };
    if msg.chat_jid != STATUS_JID
        && let Some(secs) = info
            .ephemeral_expiration
            .or_else(|| crate::ephemeral::message_expiration(&m.message))
    {
        msg.expires_at = crate::ephemeral::expiry_for(msg.ts, i64::from(secs));
    }
    msg.mentions_me = !msg.from_me
        && crate::msgops::mentions_account(
            &convert::mentioned_jids(&m.message),
            &crate::msgops::own_users(client),
            &|j| j.to_string(),
        );
    Some(Inbound::Message {
        msg,
        push_name: info.push_name.clone(),
    })
}

/// Stores one live message; returns the chat JID if something changed. Live
/// messages count as unread, raise a notification and may auto-download.
async fn store_live(sh: &Shared, client: &Arc<Client>, m: &InboundMessage) -> Option<String> {
    let (new_msg, push_name) = match prepare_inbound(sh, client, m).await? {
        Inbound::Action {
            chat,
            sender,
            from_me,
            action,
        } => {
            return apply_protocol(&sh.store, &sh.pending, &chat, &sender, from_me, &action)
                .then_some(chat);
        }
        Inbound::Reaction {
            chat,
            sender,
            from_me,
            reaction,
            ts,
        } => {
            return apply_reaction(&sh.store, &chat, &sender, from_me, &reaction, ts)
                .then_some(chat);
        }
        Inbound::PollVote {
            chat,
            voter,
            from_me,
            vote,
            ts,
        } => {
            return apply_poll_vote(sh, client, &chat, &voter, from_me, &vote, ts)
                .await
                .then_some(chat);
        }
        Inbound::Pin { chat, change, ts } => {
            return crate::msgops::apply_pin(&sh.store, &chat, &change, ts, true, now_secs())
                .then_some(chat);
        }
        Inbound::Timer { chat, sender, from_me, secs, ts } => {
            return crate::ephemeral::apply_setting(&sh.store, &chat, &sender, from_me, secs, ts)
                .then_some(chat);
        }
        Inbound::Message { msg, push_name } => (msg, push_name),
    };
    let chat = new_msg.chat_jid.clone();
    match sh.store.insert_message(&new_msg, true) {
        Ok(true) => {}
        Ok(false) => return None,
        Err(e) => {
            log::error!("insert_message failed: {e}");
            return None;
        }
    }
    if !new_msg.from_me
        && let Some(secs) = new_msg.expires_at.and_then(|e| u32::try_from(e - new_msg.ts).ok())
    {
        crate::ephemeral::adopt_message_timer(&sh.store, &chat, secs, new_msg.ts);
    }
    let waiting = apply_pending(&sh.store, &sh.pending, &chat, &new_msg.id);
    record_poll(&sh.store, &new_msg);
    apply_parked_votes(sh, client, waiting.votes).await;
    if new_msg.from_me
        && let Err(e) = sh.store.update_status(&chat, &new_msg.id, MessageStatus::Sent)
    {
        log::error!("update_status failed: {e}");
    }

    // Statuses are a read-only feed: no group lookup, no notification and
    // never an automatic download.
    if chat == STATUS_JID {
        log::info!("stored 1 live status");
        return Some(chat);
    }
    groups::ensure_name(sh, client, &chat);

    if !new_msg.from_me {
        let current = sh.settings();
        notify(sh, &new_msg, &push_name);
        crate::translate::auto_translate(sh, client, &new_msg, &current);
        if settings::should_auto_download(new_msg.kind, new_msg.media_size, &current) {
            sh.tasks.spawn(media::download(
                sh.clone(),
                client.clone(),
                new_msg.chat_jid.clone(),
                new_msg.id.clone(),
                Some(settings::auto_download_cap_bytes(&current)),
            ));
        }
    }
    Some(chat)
}

/// Stores an offline-drain batch (can be thousands of messages) in a single
/// transaction off the async threads. Messages count as unread like live ones
/// (they arrived while we were away) but never notify or auto-download.
/// Returns the chats that changed.
async fn store_offline_batch(
    sh: &Shared,
    client: &Arc<Client>,
    batch: &MessageBatch,
) -> Vec<String> {
    let mut messages = Vec::new();
    let mut actions = Vec::new();
    let mut reactions = Vec::new();
    let mut votes = Vec::new();
    let mut pins = Vec::new();
    let mut timers = Vec::new();
    for m in batch.iter() {
        match prepare_inbound(sh, client, m).await {
            Some(Inbound::Pin { chat, change, ts }) => pins.push((chat, change, ts)),
            Some(Inbound::Timer { chat, sender, from_me, secs, ts }) => timers.push((chat, sender, from_me, secs, ts)),
            Some(Inbound::Message { msg, .. }) => messages.push(msg),
            Some(Inbound::Action {
                chat,
                sender,
                from_me,
                action,
            }) => actions.push((chat, sender, from_me, action)),
            Some(Inbound::Reaction {
                chat,
                sender,
                from_me,
                reaction,
                ts,
            }) => reactions.push((chat, sender, from_me, reaction, ts)),
            Some(Inbound::PollVote {
                chat,
                voter,
                from_me,
                vote,
                ts,
            }) => votes.push((chat, voter, from_me, vote, ts)),
            None => {}
        }
    }
    if messages.is_empty() && actions.is_empty() && reactions.is_empty() && votes.is_empty() && pins.is_empty() && timers.is_empty() {
        return Vec::new();
    }

    let (store, ui) = (sh.store.clone(), sh.ui.clone());
    let pending = sh.pending.clone();
    let stored = tokio::task::spawn_blocking(move || with_store_and_ui(store, ui, |store| {
        let inserted = store.insert_messages_bulk_counting(&messages)?;
        let statuses = messages
            .iter()
            .zip(&inserted)
            .filter(|(m, new)| **new && m.chat_jid == STATUS_JID)
            .count();
        if statuses > 0 {
            log::info!("stored {statuses} statuses from an offline batch");
        }
        let mut changed: Vec<String> = Vec::new();
        let mut parked: Vec<ParkedVote> = Vec::new();
        let mut note = |chat: &str| {
            if !changed.iter().any(|c| c == chat) {
                changed.push(chat.to_string());
            }
        };
        for (m, new) in messages.iter().zip(inserted) {
            if !new {
                continue;
            }
            if m.from_me
                && let Err(e) = store.update_status(&m.chat_jid, &m.id, MessageStatus::Sent)
            {
                log::error!("update_status failed: {e}");
            }
            parked.extend(apply_pending(&store, &pending, &m.chat_jid, &m.id).votes);
            record_poll(&store, m);
            note(&m.chat_jid);
        }
        for (chat, sender, from_me, reaction, ts) in &reactions {
            if apply_reaction(&store, chat, sender, *from_me, reaction, *ts) {
                note(chat);
            }
        }
        // After the inserts so a revoke/edit finds its target in the same batch.
        for (chat, sender, from_me, action) in &actions {
            if apply_protocol(&store, &pending, chat, sender, *from_me, action) {
                note(chat);
            }
        }
        for (chat, change, ts) in &pins {
            if crate::msgops::apply_pin(&store, chat, change, *ts, false, now_secs()) {
                note(chat);
            }
        }
        for (chat, sender, from_me, secs, ts) in &timers {
            if crate::ephemeral::apply_setting(&store, chat, sender, *from_me, *secs, *ts) {
                note(chat);
            }
        }
        Ok::<_, wa_store::StoreError>((changed, parked))
    }))
    .await;
    match stored {
        Ok(Ok((mut changed, parked))) => {
            for chat in &changed {
                groups::ensure_name(sh, client, chat);
            }
            // Votes that were waiting for a poll of this batch (the polls are
            // recorded by now).
            for chat in apply_parked_votes(sh, client, parked).await {
                if !changed.contains(&chat) {
                    changed.push(chat);
                }
            }
            // After the inserts so the polls the votes belong to are stored.
            for (chat, voter, from_me, vote, ts) in &votes {
                if apply_poll_vote(sh, client, chat, voter, *from_me, vote, *ts).await
                    && !changed.contains(chat)
                {
                    changed.push(chat.clone());
                }
            }
            changed
        }
        Ok(Err(e)) => {
            log::error!("storing offline message batch failed: {e}");
            Vec::new()
        }
        Err(e) => {
            log::error!("offline batch task failed: {e}");
            Vec::new()
        }
    }
}

fn learn_from_hints(sh: &Shared, who: &Jid, pn_hint: Option<&Jid>) {
    if who.is_lid()
        && let Some(pn) = pn_hint
    {
        sh.learn_lid(&who.to_string(), &pn.to_string());
    }
}

/// What happened to a delete/edit.
enum Outcome {
    Applied,
    /// The target is not stored (yet).
    UnknownTarget,
    /// Not applied: failed authorization or a store error.
    Dropped,
}

fn try_apply(
    store: &Store,
    chat: &str,
    sender: &str,
    from_me: bool,
    action: &ProtocolAction,
) -> Outcome {
    let id = action.target_id();
    let stored = match store.message(chat, id) {
        Ok(Some(m)) => m,
        Ok(None) => return Outcome::UnknownTarget,
        Err(e) => {
            log::error!("message lookup failed: {e}");
            return Outcome::Dropped;
        }
    };
    if !from_me && stored.sender_jid != sender {
        log::warn!(
            "ignoring protocol action for {id} in {}: sent by {}, message is from {}",
            redact(chat),
            redact(sender),
            redact(&stored.sender_jid)
        );
        return Outcome::Dropped;
    }
    let result = match action {
        ProtocolAction::Revoke { .. } => store.revoke_message(chat, id),
        ProtocolAction::Edit { text, .. } => store.edit_message(chat, id, text),
    };
    match result {
        Ok(()) => Outcome::Applied,
        Err(e) => {
            log::error!("applying protocol action to {id} failed: {e}");
            Outcome::Dropped
        }
    }
}

/// Applies a delete/edit to the stored message. Only the original sender (or
/// ourselves) may change a message; anything else is logged and dropped. When
/// the target is not stored yet the action is kept in `pending` and applied by
/// [`apply_pending`] once it arrives. Returns whether the store was modified.
pub fn apply_protocol(
    store: &Store,
    pending: &PendingActions,
    chat: &str,
    sender: &str,
    from_me: bool,
    action: &ProtocolAction,
) -> bool {
    match try_apply(store, chat, sender, from_me, action) {
        Outcome::Applied => true,
        Outcome::UnknownTarget => {
            log::debug!(
                "{action:?} targets unknown message in {chat}, waiting for it"
            );
            pending.push(
                chat,
                Pending {
                    sender: sender.to_string(),
                    from_me,
                    action: PendingAction::Protocol(action.clone()),
                },
            );
            false
        }
        Outcome::Dropped => false,
    }
}

/// A poll vote that waited for its poll; decrypting needs the client, so the
/// caller applies it with [`apply_parked_votes`].
#[derive(Debug, PartialEq)]
pub struct ParkedVote {
    pub chat: String,
    pub voter: String,
    pub from_me: bool,
    pub vote: PollVoteInfo,
    pub ts: i64,
}

/// What [`apply_pending`] found waiting for a message.
#[derive(Debug, Default, PartialEq)]
pub struct Applied {
    /// Whether the store was modified by a delete/edit.
    pub changed: bool,
    /// Votes for the message (a poll), still to be applied.
    pub votes: Vec<ParkedVote>,
}

/// Applies the actions that were waiting for the freshly stored message
/// `(chat, id)`, with the same authorization as [`apply_protocol`]. Waiting
/// poll votes are handed back instead, to be applied once the poll definition
/// is recorded.
pub fn apply_pending(store: &Store, pending: &PendingActions, chat: &str, id: &str) -> Applied {
    let mut applied = Applied::default();
    for p in pending.take(chat, id) {
        match p.action {
            PendingAction::Protocol(action) => {
                applied.changed |= matches!(
                    try_apply(store, chat, &p.sender, p.from_me, &action),
                    Outcome::Applied
                );
            }
            PendingAction::Vote { vote, ts } => applied.votes.push(ParkedVote {
                chat: chat.to_string(),
                voter: p.sender,
                from_me: p.from_me,
                vote,
                ts,
            }),
            PendingAction::Star { id, starred, .. } => match store.set_starred(chat, &id, starred) {
                Ok(changed) => applied.changed |= changed,
                Err(e) => log::error!("applying a parked star failed: {e}"),
            },
        }
    }
    applied
}

/// Applies votes released by [`apply_pending`]. Returns the chats that changed.
pub async fn apply_parked_votes(sh: &Shared, client: &Client, votes: Vec<ParkedVote>) -> Vec<String> {
    let mut changed: Vec<String> = Vec::new();
    for v in votes {
        if apply_poll_vote(sh, client, &v.chat, &v.voter, v.from_me, &v.vote, v.ts).await
            && !changed.contains(&v.chat)
        {
            changed.push(v.chat);
        }
    }
    changed
}

/// Stores a reaction (or its removal). The target message may not have
/// arrived yet; the reaction then attaches when it does. Returns whether the
/// store was modified.
pub fn apply_reaction(
    store: &Store,
    chat: &str,
    sender: &str,
    from_me: bool,
    reaction: &ReactionInfo,
    ts: i64,
) -> bool {
    match store.upsert_reaction(chat, &reaction.target_id, sender, &reaction.emoji, ts, from_me) {
        Ok(()) => true,
        Err(e) => {
            log::error!("storing reaction failed: {e}");
            false
        }
    }
}

/// Records the poll definition of a stored poll message.
pub fn record_poll(store: &Store, msg: &wa_store::NewMessage) {
    let Some(poll) = convert::poll_info_of_stored(msg) else {
        return;
    };
    if let Err(e) = store.upsert_poll(
        &msg.chat_jid,
        &msg.id,
        &poll.question,
        &poll.options,
        poll.selectable_count,
    ) {
        log::error!("storing poll failed: {e}");
        return;
    }
    if let Some(correct) = &poll.correct
        && let Err(e) = store.set_poll_correct(&msg.chat_jid, &msg.id, Some(correct))
    {
        log::error!("storing the quiz answer failed: {e}");
    }
}

/// What decrypting a vote needs from the stored poll message.
pub(crate) struct PollSecrets {
    pub(crate) creator: String,
    pub(crate) secret: Vec<u8>,
    pub(crate) options: Vec<String>,
}

pub(crate) fn load_poll_secrets(
    store: &Store,
    chat: &str,
    poll_id: &str,
) -> Result<Option<PollSecrets>, wa_store::StoreError> {
    let Some(row) = store.message(chat, poll_id)? else {
        return Ok(None);
    };
    let Some(raw) = store.message_raw(chat, poll_id)? else {
        return Ok(None);
    };
    let Ok(poll_msg) = wa::Message::decode_from_slice(&raw) else {
        return Ok(None);
    };
    let Some(secret) = convert::message_secret(&poll_msg) else {
        return Ok(None);
    };
    let options = match store.poll_options(chat, poll_id)? {
        Some(options) => options,
        None => {
            let Some(p) = convert::poll_info(&poll_msg) else {
                return Ok(None);
            };
            store.upsert_poll(chat, poll_id, &p.question, &p.options, p.selectable_count)?;
            store.set_poll_correct(chat, poll_id, p.correct.as_deref())?;
            p.options
        }
    };
    Ok(Some(PollSecrets {
        creator: row.sender_jid,
        secret,
        options,
    }))
}

/// Decrypts a vote with the poll's `messageSecret` (kept in the stored poll
/// message) and stores the selected option names. Returns whether the store
/// was modified; votes for polls we do not have, or cannot decrypt, are
/// dropped.
async fn apply_poll_vote(
    sh: &Shared,
    client: &Client,
    chat: &str,
    voter: &str,
    from_me: bool,
    vote: &PollVoteInfo,
    ts: i64,
) -> bool {
    let secrets = match load_poll_secrets(&sh.store, chat, &vote.poll_id) {
        Ok(Some(s)) => s,
        Ok(None) => {
            // Only a poll that has not arrived yet is worth waiting for; one
            // that is stored but unusable never will be.
            if matches!(sh.store.message(chat, &vote.poll_id), Ok(None)) {
                log::debug!("poll vote for unknown poll {} in {}, waiting", vote.poll_id, redact(chat));
                sh.pending.push(
                    chat,
                    Pending {
                        sender: voter.to_string(),
                        from_me,
                        action: PendingAction::Vote {
                            vote: vote.clone(),
                            ts,
                        },
                    },
                );
            } else {
                log::debug!("poll vote for unusable poll {} in {}", vote.poll_id, redact(chat));
            }
            return false;
        }
        Err(e) => {
            log::error!("loading poll for vote failed: {e}");
            return false;
        }
    };
    let (Ok(creator_jid), Ok(voter_jid)) =
        (secrets.creator.parse::<Jid>(), voter.parse::<Jid>())
    else {
        log::warn!("poll vote with unparsable addresses in {}", redact(chat));
        return false;
    };
    let hashes = client
        .polls()
        .decrypt_vote(
            PollVoteCiphertext {
                enc_payload: &vote.enc_payload,
                enc_iv: &vote.enc_iv,
            },
            &secrets.secret,
            &vote.poll_id,
            &creator_jid,
            &voter_jid,
        )
        .await;
    let hashes = match hashes {
        Ok(h) => h,
        Err(e) => {
            crate::jid::warn_lib_error(&format!("poll vote in {} could not be decrypted", redact(chat)), &e);
            return false;
        }
    };
    let names = convert::option_names_for_hashes(&secrets.options, &hashes);
    match sh
        .store
        .upsert_poll_vote(chat, &vote.poll_id, voter, &names, ts, from_me)
    {
        Ok(true) => true,
        Ok(false) => {
            log::warn!(
                "ignoring poll vote in {}: more options selected than the poll allows",
                redact(chat)
            );
            false
        }
        Err(e) => {
            log::error!("storing poll vote failed: {e}");
            false
        }
    }
}

fn notify(sh: &Shared, msg: &wa_store::NewMessage, push_name: &str) {
    if !settings::may_notify(&sh.settings()) {
        return;
    }
    let chat = match sh.store.chat(&msg.chat_jid) {
        Ok(Some(c)) => c,
        Ok(None) => return,
        Err(e) => {
            log::error!("chat lookup failed: {e}");
            return;
        }
    };
    let muted = chat.muted_until > now_secs();
    let title = if chat.kind == ChatKind::Group {
        format!("{} @ {}", sender_name(sh, msg, push_name), chat.display_name)
    } else {
        chat.display_name
    };
    sh.emit(UiEvent::Notify {
        chat_jid: msg.chat_jid.clone(),
        title,
        body: convert::preview(msg),
        muted,
    });
}

fn sender_name(sh: &Shared, msg: &wa_store::NewMessage, push_name: &str) -> String {
    display_name(sh, &msg.sender_jid, push_name)
}

/// Address-book name, then stored push name, then `push_name`, then the phone.
pub(crate) fn display_name(sh: &Shared, sender_jid: &str, push_name: &str) -> String {
    let from_store = sh.store.contact(sender_jid).ok().flatten().and_then(|c| {
        c.full_name
            .filter(|n| !n.is_empty())
            .or(c.push_name.filter(|n| !n.is_empty()))
            .or(c.phone)
    });
    from_store
        .or_else(|| (!push_name.is_empty()).then(|| push_name.to_string()))
        .unwrap_or_else(|| {
            let user = sender_jid.split('@').next().unwrap_or(sender_jid);
            format!("+{user}")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    use wa_store::MessageKind;

    const CHAT: &str = "111@s.whatsapp.net";

    fn new_msg(id: &str, sender: &str) -> wa_store::NewMessage {
        wa_store::NewMessage {
            chat_jid: CHAT.into(),
            id: id.into(),
            sender_jid: sender.into(),
            from_me: false,
            ts: 1,
            kind: MessageKind::Text,
            text: Some("original".into()),
            quoted_id: None,
            quoted_preview: None,
            media_mime: None,
            media_name: None,
            media_size: None,
            media_duration: None,
            thumbnail: None,
            local_path: None,
            raw: None,
            ..Default::default()
        }
    }

    #[test]
    fn notify_flags_muted_chats_instead_of_dropping_them() {
        let store = Arc::new(Store::open_in_memory().expect("store"));
        let (ui, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (sig, _sig_rx) = tokio::sync::mpsc::unbounded_channel();
        let sh = Shared::new(
            store.clone(),
            ui,
            sig,
            Arc::new(tokio::sync::Mutex::new(())),
            Arc::new(MediaState::new(std::env::temp_dir())),
            Arc::new(std::sync::Mutex::new(EngineSettings::default())),
        );
        let m = new_msg("N1", CHAT);
        store.insert_message(&m, true).expect("insert");

        notify(&sh, &m, "");
        assert!(matches!(rx.try_recv(), Ok(UiEvent::Notify { muted: false, .. })));

        store.set_chat_flags(CHAT, None, None, Some(i64::MAX)).expect("mute");
        notify(&sh, &m, "");
        assert!(matches!(rx.try_recv(), Ok(UiEvent::Notify { muted: true, .. })));

        let off = EngineSettings { notifications: false, ..EngineSettings::default() };
        sh.set_settings(off);
        notify(&sh, &m, "");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn synced_deletions_map_to_store_operations() {
        let store = Store::open_in_memory().expect("store");
        let mut m = new_msg("A", CHAT);
        m.local_path = Some("/cache/a".into());
        store.insert_message(&m, true).expect("insert");
        store.insert_message(&new_msg("B", CHAT), true).expect("insert");
        store.insert_message(&new_msg("C", CHAT), true).expect("insert");

        let d = ChatDeletion::Message("A".into());
        assert_eq!(run_deletion(&store, CHAT, &d), Some(vec!["/cache/a".to_string()]));
        assert_eq!(store.message(CHAT, "A").expect("query"), None);
        // Unknown id: nothing to do, nothing reported.
        assert_eq!(run_deletion(&store, CHAT, &d), None);

        assert_eq!(ChatDeletion::clear(true), ChatDeletion::Clear);
        assert_eq!(ChatDeletion::clear(false), ChatDeletion::ClearKeepStarred);
        assert_eq!(unread_after_phone_mark(0), 1);
        assert_eq!(unread_after_phone_mark(5), 5);
        assert!(store.set_starred(CHAT, "B", true).expect("star"));
        assert_eq!(run_deletion(&store, CHAT, &ChatDeletion::ClearKeepStarred), Some(vec![]));
        assert!(store.message(CHAT, "B").expect("query").is_some());
        assert_eq!(store.message(CHAT, "C").expect("query"), None);
        assert_eq!(run_deletion(&store, CHAT, &ChatDeletion::Clear), Some(vec![]));
        assert_eq!(store.message(CHAT, "B").expect("query"), None);
        assert!(store.chat(CHAT).expect("query").is_some());

        assert_eq!(run_deletion(&store, CHAT, &ChatDeletion::Chat), Some(vec![]));
        assert!(store.chat(CHAT).expect("query").is_none());
    }

    #[test]
    fn star_arriving_first_is_applied_after_a_live_or_bulk_insert() {
        let store = Store::open_in_memory().expect("store");
        let pending = PendingActions::default();
        assert!(!crate::msgops::apply_star(&store, &pending, CHAT, "A", true, 10));
        assert!(!crate::msgops::apply_star(&store, &pending, CHAT, "B", true, 10));

        store.insert_message(&new_msg("A", CHAT), true).expect("insert");
        assert!(apply_pending(&store, &pending, CHAT, "A").changed);
        assert!(store.message(CHAT, "A").expect("query").expect("row").starred);

        store.insert_messages_bulk(&[new_msg("B", CHAT)]).expect("bulk");
        assert!(apply_pending(&store, &pending, CHAT, "B").changed);
        assert!(store.message(CHAT, "B").expect("query").expect("row").starred);
    }

    #[test]
    fn star_then_unstar_before_arrival_leaves_the_message_unstarred() {
        let store = Store::open_in_memory().expect("store");
        let pending = PendingActions::default();
        crate::msgops::apply_star(&store, &pending, CHAT, "A", true, 10);
        crate::msgops::apply_star(&store, &pending, CHAT, "A", false, 20);
        // An older event arriving late must not undo the newer unstar.
        crate::msgops::apply_star(&store, &pending, CHAT, "A", true, 15);

        store.insert_message(&new_msg("A", CHAT), true).expect("insert");
        assert!(!apply_pending(&store, &pending, CHAT, "A").changed);
        assert!(!store.message(CHAT, "A").expect("query").expect("row").starred);
    }

    #[test]
    fn star_of_a_stored_message_applies_at_once() {
        let store = Store::open_in_memory().expect("store");
        let pending = PendingActions::default();
        store.insert_message(&new_msg("A", CHAT), true).expect("insert");
        assert!(crate::msgops::apply_star(&store, &pending, CHAT, "A", true, 10));
        assert!(!crate::msgops::apply_star(&store, &pending, CHAT, "A", true, 11));
        assert!(apply_pending(&store, &pending, CHAT, "A").votes.is_empty());
    }

    #[test]
    fn edit_arriving_first_is_applied_after_the_insert() {
        let store = Store::open_in_memory().expect("store");
        let pending = PendingActions::default();
        let edit = ProtocolAction::Edit { id: "A".into(), text: "edited".into() };
        assert!(!apply_protocol(&store, &pending, CHAT, CHAT, false, &edit));

        store.insert_message(&new_msg("A", CHAT), true).expect("insert");
        assert!(apply_pending(&store, &pending, CHAT, "A").changed);
        let row = store.message(CHAT, "A").expect("query").expect("row");
        assert_eq!(row.text.as_deref(), Some("edited"));
        // Consumed: a second pass does nothing.
        assert!(!apply_pending(&store, &pending, CHAT, "A").changed);
    }

    #[test]
    fn parked_votes_are_released_with_the_poll() {
        let store = Store::open_in_memory().expect("store");
        let pending = PendingActions::default();
        let vote = PollVoteInfo {
            poll_id: "P".into(),
            enc_payload: vec![1],
            enc_iv: vec![2],
            ts_ms: None,
        };
        pending.push(
            CHAT,
            Pending {
                sender: "v@s.whatsapp.net".into(),
                from_me: false,
                action: PendingAction::Vote { vote: vote.clone(), ts: 9 },
            },
        );
        assert!(apply_pending(&store, &pending, CHAT, "other").votes.is_empty());

        store.insert_message(&new_msg("P", CHAT), true).expect("insert");
        let applied = apply_pending(&store, &pending, CHAT, "P");
        assert!(!applied.changed);
        assert_eq!(
            applied.votes,
            vec![ParkedVote {
                chat: CHAT.into(),
                voter: "v@s.whatsapp.net".into(),
                from_me: false,
                vote,
                ts: 9,
            }]
        );
        assert!(apply_pending(&store, &pending, CHAT, "P").votes.is_empty());
    }

    #[test]
    fn buffered_action_from_the_wrong_sender_is_not_applied() {
        let store = Store::open_in_memory().expect("store");
        let pending = PendingActions::default();
        let revoke = ProtocolAction::Revoke { id: "A".into() };
        apply_protocol(&store, &pending, CHAT, "999@s.whatsapp.net", false, &revoke);

        store.insert_message(&new_msg("A", CHAT), true).expect("insert");
        assert!(!apply_pending(&store, &pending, CHAT, "A").changed);
        let row = store.message(CHAT, "A").expect("query").expect("row");
        assert_eq!(row.text.as_deref(), Some("original"));
    }

    #[test]
    fn own_buffered_revoke_is_applied() {
        let store = Store::open_in_memory().expect("store");
        let pending = PendingActions::default();
        let revoke = ProtocolAction::Revoke { id: "A".into() };
        apply_protocol(&store, &pending, CHAT, "me@s.whatsapp.net", true, &revoke);

        store.insert_message(&new_msg("A", CHAT), true).expect("insert");
        assert!(apply_pending(&store, &pending, CHAT, "A").changed);
    }
}

/// Group changes after which our own send/admin rights may differ: the
/// "only admins can send" switch, or us being removed, promoted or demoted.
fn affects_our_rights(client: &Client, action: &whatsapp_rust::wacore::stanza::groups::GroupNotificationAction) -> bool {
    use whatsapp_rust::wacore::stanza::groups::GroupNotificationAction as A;
    let participants = match action {
        A::Announce | A::NotAnnounce => return true,
        A::Remove { participants, .. } | A::Promote { participants } | A::Demote { participants } => participants,
        _ => return false,
    };
    let me: Vec<String> = own_jid(client)
        .into_iter()
        .chain(client.lid().map(|j| j.to_non_ad().to_string()))
        .collect();
    participants.iter().any(|p| {
        std::iter::once(&p.jid)
            .chain(p.phone_number.as_ref())
            .any(|j| me.contains(&j.to_non_ad().to_string()))
    })
}
