//! Background thread, tokio runtime and the supervisor loop that owns the bot.
//!
//! One "generation" is one `Bot` instance. A generation ends on shutdown, on
//! logout (we then wipe the session and start a fresh generation, which shows a
//! new QR), or when the QR codes are exhausted (fresh generation, session kept,
//! at most a few times in a row). Connection drops are first left to the
//! library's own reconnect logic (keepalive pings, dead-socket detection,
//! Fibonacci backoff), except when WhatsApp bans, outdates or displaces the
//! client: then we stop the bot and go idle. The library's backoff grows up to
//! 15 minutes and cannot be bounded from outside, so a [`Recovery`] watchdog
//! rebuilds the generation (fresh transport, fresh backoff) when a paired
//! session stays offline too long. We never rebuild against a session we
//! failed to wipe.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::oneshot;
use whatsapp_rust::prelude::{Bot, BotHandle, Client, EventDelivery, SqliteStore};
use whatsapp_rust::wacore::net::DisconnectReason;

use crate::api::{Command, ConnState, EngineConfig, EngineSettings, UiEvent};
use crate::events::{self, Shared, Signal};
use crate::media::MediaState;
use crate::tasks::TASK_SHUTDOWN_TIMEOUT;
use crate::{commands, identity, proxy, settings, status};
use wa_store::Store;

/// Everything the supervisor needs; moved onto the engine thread.
pub struct Params {
    pub config: EngineConfig,
    pub store: Arc<Store>,
    pub cmd_rx: UnboundedReceiver<Command>,
    pub ui: UnboundedSender<UiEvent>,
    /// In force before the first generation starts.
    pub settings: EngineSettings,
}

/// The settings cell shared by every generation.
fn seed_settings(initial: EngineSettings) -> Arc<std::sync::Mutex<EngineSettings>> {
    Arc::new(std::sync::Mutex::new(initial))
}

/// How long the engine thread waits, after the supervisor finished, for
/// leftover tasks and `spawn_blocking` work (history chunks poll the cancel
/// flag between units) before the runtime is torn down regardless.
const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

/// Owns what must be torn down, in this order, when the engine thread ends
/// for any reason (normal shutdown, early return or a panic unwinding):
///
/// 1. the tokio runtime is shut down: every task still alive is dropped (those
///    hold `Arc<Client>`, hence the `SqliteStore` r2d2 pool of `session.db`,
///    and `Arc<Store>` of `messages.db`) and `spawn_blocking` work is waited
///    for, bounded by [`RUNTIME_SHUTDOWN_TIMEOUT`];
/// 2. only then `_ui_last`, the final clone of the UI sender, is dropped.
///
/// The UI treats "the event channel is closed" as "the engine released its
/// files" and deletes the profile folder on that signal. Every other sender
/// clone lives in a task or in `Shared` and is gone after step 1, so this
/// clone is what holds the channel open until the files are released. Any
/// task that outlives the timeout keeps the channel open even longer, which
/// is the safe direction.
struct Teardown {
    rt: Option<tokio::runtime::Runtime>,
    _ui_last: UnboundedSender<UiEvent>,
}

impl Drop for Teardown {
    fn drop(&mut self) {
        if let Some(rt) = self.rt.take() {
            rt.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
        }
        // `_ui_last` is dropped after this body returns, i.e. after the runtime.
    }
}

fn build_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        // The app may run several engines at once; keep each one small.
        .worker_threads(2)
        .max_blocking_threads(8)
        .thread_name("wa-engine-rt")
        .build()
}

/// Runs the supervisor on its own OS thread. The runtime is built on that
/// thread, so it is never dropped on the caller's thread (which may be inside
/// an async context, where dropping a runtime panics) even if spawning fails.
/// A runtime build error is returned once the thread reported it.
pub fn spawn_thread(params: Params) -> anyhow::Result<()> {
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<std::io::Result<()>>(1);
    std::thread::Builder::new()
        .name("wa-engine".into())
        .spawn(move || {
            let rt = match build_runtime() {
                Ok(rt) => {
                    if ready_tx.send(Ok(())).is_err() {
                        log::debug!("engine starter is gone");
                    }
                    rt
                }
                Err(e) => {
                    if ready_tx.send(Err(e)).is_err() {
                        log::debug!("engine starter is gone");
                    }
                    return;
                }
            };
            let teardown = Teardown {
                _ui_last: params.ui.clone(),
                rt: Some(rt),
            };
            let Some(rt) = teardown.rt.as_ref() else {
                return;
            };
            // `supervise` consumes `params` and releases the client, bot,
            // generation state and its own store/UI handles before returning.
            rt.block_on(supervise(params));
            drop(teardown);
            log::info!("engine thread finished");
        })?;
    match ready_rx.recv() {
        Ok(built) => Ok(built?),
        Err(_) => anyhow::bail!("engine thread ended before it started"),
    }
}

/// Events buffered before the library starts dropping them; handlers are fast
/// (history chunks are offloaded), so this is only a burst allowance.
const EVENT_MAILBOX: usize = 4096;

enum Outcome {
    Exit,
    Rebuild,
    /// Rebuild after an outage: messages still `Pending` are kept and the next
    /// generation re-delivers them once it is logged in.
    Resume,
}

/// Automatic pairing restarts (QR codes expired unscanned) allowed in a row
/// before the engine waits for [`Command::RetryPairing`].
const MAX_AUTO_PAIRING_RESTARTS: u32 = 3;
/// How long `client.logout()` may take before we unlink locally anyway.
const LOGOUT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long stopping the bot (disconnect + run loop exit) may take.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// Total time spent retrying to delete the session files after logout.
const WIPE_RETRY_WINDOW: Duration = Duration::from_secs(5);
const WIPE_RETRY_INTERVAL: Duration = Duration::from_millis(300);
/// How long a bot that ended by itself waits for the event handler's final
/// signal (logout, ban...) before the end is treated as a plain disconnect.
const SIGNAL_GRACE: Duration = Duration::from_millis(500);

/// How often the offline watchdog looks at the connection.
const WATCHDOG_TICK: Duration = Duration::from_secs(2);
/// A paired session offline for this long gets a fresh generation. The library
/// retries at 1, 1, 2, 3, 5, 8 s meanwhile, so short outages (a proxy app
/// restarting, a network change) heal inside this window without a rebuild.
const OFFLINE_LIMIT: Duration = Duration::from_secs(20);
/// Online this long counts as recovered: the retry schedule starts over.
const STABLE_ONLINE: Duration = Duration::from_secs(30);
/// Waits before successive watchdog rebuilds, in seconds (the last repeats).
const RETRY_SCHEDULE_SECS: [u64; 6] = [1, 2, 5, 10, 30, 60];

/// Wait before the `attempt`-th (0-based) rebuild: [`RETRY_SCHEDULE_SECS`]
/// with +-20 % jitter; `jitter` is in `0.0..=1.0`.
fn backoff_delay(attempt: u32, jitter: f64) -> Duration {
    let idx = (attempt as usize).min(RETRY_SCHEDULE_SECS.len() - 1);
    let base = RETRY_SCHEDULE_SECS[idx] as f64;
    Duration::from_secs_f64(base * (0.8 + 0.4 * jitter.clamp(0.0, 1.0)))
}

/// Cheap jitter source (sub-second clock noise); not for anything secret.
fn jitter_unit() -> f64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    f64::from(nanos % 1_000_000) / 1_000_000.0
}

/// Tracks how long the connection has been down; lives across generations.
#[derive(Debug, Default)]
struct Recovery {
    /// Rebuilds since the connection was last stable.
    attempts: u32,
    offline_since: Option<Instant>,
    online_since: Option<Instant>,
}

impl Recovery {
    /// A new generation starts: its own retries get a fresh [`OFFLINE_LIMIT`].
    fn new_generation(&mut self) {
        self.offline_since = None;
        self.online_since = None;
    }

    /// Feeds one observation; `true` when the session has been offline for
    /// longer than [`OFFLINE_LIMIT`] and the generation should be rebuilt.
    fn observe(&mut self, now: Instant, online: bool) -> bool {
        if online {
            self.offline_since = None;
            let since = *self.online_since.get_or_insert(now);
            if now.saturating_duration_since(since) >= STABLE_ONLINE {
                self.attempts = 0;
            }
            return false;
        }
        self.online_since = None;
        let since = *self.offline_since.get_or_insert(now);
        now.saturating_duration_since(since) >= OFFLINE_LIMIT
    }

    /// Takes the next rebuild slot and returns how long to wait before it.
    fn next_delay(&mut self, jitter: f64) -> Duration {
        let delay = backoff_delay(self.attempts, jitter);
        self.attempts = self.attempts.saturating_add(1);
        self.new_generation();
        delay
    }
}

/// What kind of connection loss the library reported. Every kind is retried;
/// the class only decides how the log describes it (no host, no proxy details).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DisconnectClass {
    /// The peer cut the TLS stream without `close_notify` (rustls
    /// `UnexpectedEof`): what proxies and NAT boxes do to idle tunnels.
    MissingCloseNotify,
    StreamEnded,
    ServerClose,
    ReadError,
    Unknown,
}

impl DisconnectClass {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::MissingCloseNotify => "closed without TLS close_notify",
            Self::StreamEnded => "stream ended",
            Self::ServerClose => "closed by server",
            Self::ReadError => "read error",
            Self::Unknown => "unknown",
        }
    }
}

pub(crate) fn classify_disconnect(reason: &DisconnectReason) -> DisconnectClass {
    match reason {
        DisconnectReason::ReadError(e) => {
            let e = e.to_ascii_lowercase();
            if e.contains("close_notify") || e.contains("unexpected-eof") || e.contains("unexpectedeof") {
                DisconnectClass::MissingCloseNotify
            } else {
                DisconnectClass::ReadError
            }
        }
        DisconnectReason::StreamEnded => DisconnectClass::StreamEnded,
        DisconnectReason::ServerClose { .. } => DisconnectClass::ServerClose,
        DisconnectReason::Unknown => DisconnectClass::Unknown,
    }
}

/// Counts consecutive automatic pairing restarts.
#[derive(Debug, Default)]
struct RestartBudget {
    used: u32,
}

impl RestartBudget {
    /// Takes one restart; `false` once the cap is reached.
    fn try_restart(&mut self) -> bool {
        if self.used >= MAX_AUTO_PAIRING_RESTARTS {
            return false;
        }
        self.used += 1;
        true
    }

    fn reset(&mut self) {
        self.used = 0;
    }
}

/// Why a running generation stopped.
enum End {
    Shutdown,
    LoggedOut { reason: String },
    /// The QR codes ran out.
    Restart,
    /// The UI asked for a fresh QR.
    Retry,
    /// The bot task finished by itself (fatal condition); wait for commands.
    BotEnded,
    /// WhatsApp banned, outdated or displaced this client: stop connecting.
    Halt { recoverable: bool },
    /// The paired session stayed offline past [`OFFLINE_LIMIT`].
    Offline,
    /// The user asked to reconnect while offline.
    Reconnect,
}

impl End {
    /// Whether the messages still `Pending` are marked failed when this end
    /// is handled. After an outage they are kept and re-delivered instead.
    fn fails_pending(&self) -> bool {
        !matches!(self, End::Offline | End::Reconnect)
    }
}

async fn supervise(params: Params) {
    let Params {
        config,
        store,
        mut cmd_rx,
        ui,
        settings,
    } = params;
    let history_lock = Arc::new(tokio::sync::Mutex::new(()));
    let media = Arc::new(MediaState::new(config.media_dir()));
    let mut budget = RestartBudget::default();
    let mut recovery = Recovery::default();
    // Settings outlive bot generations; seeded before the first bot exists so
    // no event is ever handled under settings the UI did not choose.
    let settings = seed_settings(settings);
    let purge_task = tokio::spawn(status::purge_loop(
        store.clone(),
        config.media_dir(),
        ui.clone(),
    ));

    // The first generation of an engine announces a restored session; a
    // rebuild after an outage must not make the UI re-enter its main screen.
    let mut restored_announced = false;
    // The previous generation ended in an outage with messages still pending.
    let mut resume = false;
    loop {
        let (sig_tx, sig_rx) = unbounded_channel();
        let mut sh = Shared::new(
            store.clone(),
            ui.clone(),
            sig_tx,
            history_lock.clone(),
            media.clone(),
            settings.clone(),
        );
        sh.proxied = config.proxy.is_some();
        if resume {
            // Taken before the new bot exists, so a message the user queues
            // in this generation can never be in it (it is being sent live).
            match sh.store.pending_outgoing() {
                Ok(rows) if !rows.is_empty() => *crate::media::lock(&sh.requeue_pending) = Some(rows),
                Ok(_) => {}
                Err(e) => log::error!("listing pending messages failed: {e}"),
            }
        }
        sh.emit(UiEvent::Connection(ConnState::Connecting));

        let outcome = match start_bot(&config, &sh).await {
            Ok((handle, client, restored)) => {
                if restored && !restored_announced {
                    restored_announced = true;
                    sh.emit(UiEvent::SessionRestored);
                }
                run_generation(
                    &config,
                    &sh,
                    &mut cmd_rx,
                    sig_rx,
                    handle,
                    client,
                    &mut budget,
                    &mut recovery,
                )
                .await
            }
            Err(e) => {
                log::error!("failed to start WhatsApp client: {e:#}");
                sh.emit(UiEvent::Fatal {
                    message: format!("Could not start the WhatsApp client: {e:#}"),
                });
                idle(&config, &sh, &mut cmd_rx, &mut budget, true, None).await
            }
        };
        resume = matches!(outcome, Outcome::Resume);
        if matches!(outcome, Outcome::Exit) {
            purge_task.abort();
            // Awaiting an aborted task drops its future (and its store/UI
            // handles) before we return; the result is just `Cancelled`.
            let _ = purge_task.await;
            // The generation's tasks were awaited by `end_generation`; this
            // drops the last `Shared` (store, UI sender, coalescer) held here.
            drop(sh);
            return;
        }
    }
}

async fn start_bot(
    config: &EngineConfig,
    sh: &Shared,
) -> anyhow::Result<(BotHandle, Arc<Client>, bool)> {
    let db_path = config.session_db();
    let db_url = db_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("session database path is not valid UTF-8"))?;
    let backend = SqliteStore::new(db_url).await?;

    let handler_sh = sh.clone();
    let builder = Bot::builder()
        .with_backend(backend)
        .with_device_props(identity::device_props())
        .with_event_delivery(EventDelivery::Ordered { capacity: EVENT_MAILBOX })
        .on_event(move |event, client| {
            let sh = handler_sh.clone();
            async move { events::handle(sh, event, client).await }
        });
    // Without a proxy the library's own transport and HTTP client are kept.
    let bot = match &config.proxy {
        Some(proxy) => {
            proxy.log_in_use();
            builder
                .with_transport_factory(proxy::ProxyTransportFactory::new(proxy.clone())?)
                .with_http_client(proxy::http_client(proxy)?)
                .build()
                .await?
        }
        None => builder.build().await?,
    };

    let client = bot.client();
    // Not persisted by the library: must be set on every start.
    client.set_client_profile(identity::client_profile()).await;
    let restored = client.pn().is_some();
    Ok((bot.spawn(), client, restored))
}

/// Runs the commands sent to it strictly in submission order.
fn spawn_ordered_worker(
    sh: &Shared,
    client: &Arc<Client>,
) -> (UnboundedSender<Command>, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = unbounded_channel::<Command>();
    let (sh, client) = (sh.clone(), client.clone());
    let task = tokio::spawn(async move {
        while let Some(cmd) = rx.recv().await {
            commands::run(sh.clone(), client.clone(), cmd).await;
        }
    });
    (tx, task)
}

/// Resolves when the pending logout finishes (`true`) or times out (`false`);
/// never resolves while no logout is pending.
async fn logout_finished(pending: &mut Option<oneshot::Receiver<bool>>) -> bool {
    match pending {
        Some(rx) => {
            let completed = rx.await.unwrap_or(false);
            *pending = None;
            completed
        }
        None => std::future::pending().await,
    }
}

/// Stops the bot after its generation ended, bounded by [`SHUTDOWN_TIMEOUT`];
/// on timeout the bot task is aborted.
///
/// This is `BotHandle::shutdown()` spelled out (disconnect, then wait for the
/// run loop) because `shutdown()` consumes the handle and we need it to abort.
/// We skip it once the handle has resolved so we never touch a dead client or
/// re-await a consumed handle.
async fn stop_bot(mut handle: BotHandle, client: &Arc<Client>, finished: bool) {
    if finished {
        return;
    }
    let stopped = tokio::time::timeout(SHUTDOWN_TIMEOUT, async {
        client.disconnect().await;
        (&mut handle).await;
    })
    .await;
    if stopped.is_err() {
        log::warn!("bot did not stop within {SHUTDOWN_TIMEOUT:?}, aborting it");
        handle.abort();
    }
}

/// Everything that must be over before the session can be wiped or rebuilt:
/// background tasks holding the client are aborted and awaited, then the bot
/// is stopped. The caller drops its own `Arc<Client>` afterwards.
async fn end_generation(
    sh: &Shared,
    handle: BotHandle,
    client: &Arc<Client>,
    finished: bool,
) {
    sh.tasks.shutdown(TASK_SHUTDOWN_TIMEOUT).await;
    stop_bot(handle, client, finished).await;
}

/// Marks messages that were still being sent as failed, so the UI never shows
/// "sending" for a message whose sender task is gone.
async fn fail_pending(sh: &Shared) {
    let (store, ui) = (sh.store.clone(), sh.ui.clone());
    match tokio::task::spawn_blocking(move || events::with_store_and_ui(store, ui, Store::fail_stale_pending)).await {
        Ok(Ok(chats)) => {
            if chats.is_empty() {
                return;
            }
            for chat_jid in chats {
                sh.emit(UiEvent::MessagesChanged { chat_jid });
            }
            sh.emit(UiEvent::ChatsChanged);
        }
        Ok(Err(e)) => log::error!("failing stale pending messages failed: {e}"),
        Err(e) => log::error!("stale pending task failed: {e}"),
    }
}

async fn run_generation(
    config: &EngineConfig,
    sh: &Shared,
    cmd_rx: &mut UnboundedReceiver<Command>,
    mut sig_rx: UnboundedReceiver<Signal>,
    mut handle: BotHandle,
    client: Arc<Client>,
    budget: &mut RestartBudget,
    recovery: &mut Recovery,
) -> Outcome {
    // Sends run strictly in submission order (an upload can take minutes);
    // typing indicators and read receipts have their own ordered worker so a
    // slow upload never delays them and "paused" can never overtake "composing".
    let (send_tx, sender_task) = spawn_ordered_worker(sh, &client);
    let (ctl_tx, ctl_task) = spawn_ordered_worker(sh, &client);
    let (flags_tx, flags_task) = spawn_ordered_worker(sh, &client);
    sh.tasks.spawn(crate::ephemeral::purger(sh.clone()));

    let mut explicit_logout = false;
    let mut logout_wait: Option<oneshot::Receiver<bool>> = None;
    let mut handle_finished = false;
    recovery.new_generation();
    let mut watchdog = tokio::time::interval(WATCHDOG_TICK);
    watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let end = loop {
        tokio::select! {
            _ = &mut handle => {
                handle_finished = true;
                break End::BotEnded;
            }
            sig = sig_rx.recv() => match sig {
                Some(Signal::LoggedOut { reason }) => break End::LoggedOut { reason },
                Some(Signal::RestartPairing) => break End::Restart,
                Some(Signal::Halt { recoverable }) => break End::Halt { recoverable },
                None => break End::BotEnded,
            },
            _ = watchdog.tick() => {
                // Pairing (no phone number yet) is not a connection to keep alive.
                let online = client.pn().is_none() || client.is_logged_in();
                if recovery.observe(Instant::now(), online) {
                    break End::Offline;
                }
            }
            completed = logout_finished(&mut logout_wait) => {
                if !completed {
                    log::warn!("logout did not finish within {LOGOUT_TIMEOUT:?}, unlinking locally");
                    sh.emit(UiEvent::Error {
                        message: "Could not reach WhatsApp to unlink this device; it may still \
                                  be listed under linked devices on your phone."
                            .to_string(),
                    });
                    break End::LoggedOut { reason: "Logged out".to_string() };
                }
                // Event::LoggedOut was dispatched and arrives as a signal.
            }
            cmd = cmd_rx.recv() => {
                if let Some(c) = &cmd {
                    log::info!("command: {}", c.log_line());
                }
                match cmd {
                None | Some(Command::Shutdown) => break End::Shutdown,
                Some(Command::Logout) => {
                    if logout_wait.is_none() {
                        explicit_logout = true;
                        let (tx, rx) = oneshot::channel();
                        let client = client.clone();
                        tokio::spawn(async move {
                            let completed =
                                tokio::time::timeout(LOGOUT_TIMEOUT, client.logout()).await.is_ok();
                            let _ = tx.send(completed);
                        });
                        logout_wait = Some(rx);
                    }
                }
                Some(Command::Reconnect) => {
                    if client.pn().is_some() && !client.is_logged_in() {
                        // Offline: skip the library's backoff and start over now.
                        recovery.attempts = 0;
                        break End::Reconnect;
                    }
                    log::debug!("Reconnect ignored: already connected");
                }
                Some(Command::RetryPairing) => {
                    if client.pn().is_none() {
                        break End::Retry;
                    }
                    log::debug!("RetryPairing ignored: the session is paired");
                }
                Some(
                    c @ (Command::SendText { .. }
                    | Command::SendMedia { .. }
                    | Command::SendContact { .. }
                    | Command::SendLocation { .. }
                    | Command::PostTextStatus { .. }
                    | Command::PostMediaStatus { .. }
                    | Command::ForwardMessages { .. }
                    | Command::SendSticker { .. }
                    | Command::VotePoll { .. }
                    | Command::CreatePoll { .. }),
                ) => {
                    if send_tx.send(c).is_err() {
                        log::error!("send worker is gone");
                    }
                }
                Some(Command::UpdateSettings(new)) => {
                    let presence = settings::presence_change(&sh.settings(), &new);
                    sh.set_settings(new);
                    if presence.is_some() && client.is_connected() {
                        sh.tasks.spawn(commands::sync_presence(sh.clone(), client.clone()));
                    }
                }
                Some(
                    c @ (Command::SetTyping { .. }
                    | Command::MarkChatRead { .. }
                    | Command::SetChatReceipts { .. }
                    | Command::SendReadReceiptsNow { .. }
                    | Command::SendReaction { .. }
                    | Command::DeleteMessage { .. }
                    | Command::EditMessage { .. }
                    | Command::PinMessage { .. }
                    | Command::MarkStatusViewed { .. }
                    | Command::SetChatTimer { .. }
                    | Command::SetDefaultTimer { .. }
                    | Command::FetchDefaultTimer),
                ) => {
                    if ctl_tx.send(c).is_err() {
                        log::error!("control worker is gone");
                    }
                }
                // Chat settings wait on the app-state collections of the
                // phone; they get their own ordered worker so a slow or stuck
                // sync cannot hold up read receipts and reactions (or the
                // other way round).
                Some(c @ (Command::SetArchived { .. } | Command::SetPinned { .. } | Command::SetMuted { .. } | Command::StarMessage { .. })) => {
                    if flags_tx.send(c).is_err() {
                        log::error!("chat settings worker is gone");
                    }
                }
                Some(c) => {
                    sh.tasks.spawn(commands::run(sh.clone(), client.clone(), c));
                }
                }
            }
        }
    };
    // A queued signal (logout, ban, ...) wins over an outage end.
    let end = match end {
        End::Offline | End::Reconnect => match sig_rx.try_recv() {
            Ok(Signal::LoggedOut { reason }) => End::LoggedOut { reason },
            Ok(Signal::RestartPairing) => End::Restart,
            Ok(Signal::Halt { recoverable }) => End::Halt { recoverable },
            Err(_) => end,
        },
        other => other,
    };
    drop(send_tx);
    drop(ctl_tx);
    drop(flags_tx);
    sender_task.abort();
    ctl_task.abort();
    flags_task.abort();
    // Wait until the workers are really gone so none of them can update a
    // message after we failed it below.
    let _ = sender_task.await;
    let _ = ctl_task.await;
    let _ = flags_task.await;
    if end.fails_pending() {
        fail_pending(sh).await;
    }

    // When the bot ended by itself the logout/restart signal may still be queued.
    let end = match end {
        End::BotEnded => {
            let pending = match sig_rx.try_recv() {
                Ok(sig) => Some(sig),
                Err(_) => tokio::time::timeout(SIGNAL_GRACE, sig_rx.recv())
                    .await
                    .ok()
                    .flatten(),
            };
            match pending {
                Some(Signal::LoggedOut { reason }) => End::LoggedOut { reason },
                Some(Signal::RestartPairing) => End::Restart,
                Some(Signal::Halt { recoverable }) => End::Halt { recoverable },
                None if explicit_logout => End::LoggedOut {
                    reason: "Logged out".to_string(),
                },
                None => End::BotEnded,
            }
        }
        other => other,
    };
    if !matches!(end, End::Restart) {
        budget.reset();
    }

    end_generation(sh, handle, &client, handle_finished).await;
    match end {
        End::Shutdown => {
            // The bot is stopped and its tasks are gone: release our client
            // (and with it the session database backend) before reporting exit.
            drop(client);
            Outcome::Exit
        }
        End::Retry => {
            drop(client);
            Outcome::Rebuild
        }
        End::Reconnect => {
            drop(client);
            Outcome::Resume
        }
        End::Restart => {
            drop(client);
            if budget.try_restart() {
                return Outcome::Rebuild;
            }
            log::info!("QR codes expired {MAX_AUTO_PAIRING_RESTARTS} times, waiting for a retry");
            sh.emit(UiEvent::Error {
                message: "QR code expired".to_string(),
            });
            idle(config, sh, cmd_rx, budget, true, None).await
        }
        End::LoggedOut { reason } => {
            drop(client);
            if logged_out(config, sh, explicit_logout, reason).await {
                Outcome::Rebuild
            } else {
                idle(config, sh, cmd_rx, budget, false, None).await
            }
        }
        End::Halt { recoverable } => {
            log::warn!("bot stopped on purpose (recoverable: {recoverable}), not reconnecting");
            drop(client);
            idle(config, sh, cmd_rx, budget, recoverable, None).await
        }
        End::Offline => {
            drop(client);
            let delay = recovery.next_delay(jitter_unit());
            log::warn!("offline for over {OFFLINE_LIMIT:?}, reconnecting in {delay:?}");
            // The UI keeps showing "Connecting..."; no manual button is needed.
            idle(config, sh, cmd_rx, budget, true, Some(delay)).await
        }
        End::BotEnded => {
            log::warn!("bot task ended unexpectedly");
            drop(client);
            sh.emit(UiEvent::Connection(ConnState::Disconnected {
                reason: "The connection to WhatsApp ended".to_string(),
            }));
            idle(config, sh, cmd_rx, budget, true, None).await
        }
    }
}

/// No running bot: only lifecycle commands make sense. `allow_retry` is
/// `false` when the session could not be wiped (rebuilding would reconnect
/// with a revoked session) or when WhatsApp banned / outdated the client
/// (reconnecting would endanger the account); no automatic retry ever happens
/// from here, except `auto_retry_in`: the watchdog's scheduled rebuild after
/// that delay (commands, notably shutdown, are still served while waiting).
async fn idle(
    config: &EngineConfig,
    sh: &Shared,
    cmd_rx: &mut UnboundedReceiver<Command>,
    budget: &mut RestartBudget,
    allow_retry: bool,
    auto_retry_in: Option<Duration>,
) -> Outcome {
    let deadline = auto_retry_in.map(|d| tokio::time::Instant::now() + d);
    // After an outage the pending messages survive the rebuild (see `Outcome::Resume`).
    let rebuild = if deadline.is_some() { Outcome::Resume } else { Outcome::Rebuild };
    loop {
        let received = match deadline {
            Some(at) => tokio::select! {
                _ = tokio::time::sleep_until(at) => return rebuild,
                r = cmd_rx.recv() => r,
            },
            None => cmd_rx.recv().await,
        };
        if let Some(c) = &received {
            log::info!("command (not connected): {}", c.log_line());
        }
        match received {
            None | Some(Command::Shutdown) => return Outcome::Exit,
            Some(Command::Logout) => {
                if deadline.is_some() {
                    fail_pending(sh).await;
                }
                if logged_out(config, sh, true, "Logged out".to_string()).await {
                    return Outcome::Rebuild;
                }
            }
            Some(Command::UpdateSettings(new)) => sh.set_settings(new),
            // Purely local: must not be dropped while offline, or receipts
            // could later go out for a chat the user excluded.
            Some(Command::SetChatReceipts { chat_jid, enabled }) => {
                match sh.store.set_chat_receipts_off(&chat_jid, !enabled) {
                    Ok(()) => sh.emit(UiEvent::ChatsChanged),
                    Err(e) => log::error!("set_chat_receipts_off failed: {e}"),
                }
            }
            Some(Command::RetryPairing | Command::Reconnect) if allow_retry => {
                budget.reset();
                return rebuild;
            }
            Some(_) => sh.emit(UiEvent::Error {
                message: "Not connected to WhatsApp".to_string(),
            }),
        }
    }
}

/// Wipes the session (and, for an explicit logout, the local history) and tells
/// the UI. Server-initiated logouts keep `messages.db`.
///
/// Returns `false` (after emitting `Fatal`) when the session files could not be
/// deleted; the caller must then not start a new generation, which would
/// reconnect with the revoked session.
async fn logged_out(config: &EngineConfig, sh: &Shared, explicit: bool, reason: String) -> bool {
    let session = config.session_db();
    // The sqlite pool may still be closing; Windows refuses to delete open files.
    sh.media.clear_avatar_state();
    sh.groups.clear();
    sh.info.clear();
    let mut wiped = false;
    let deadline = tokio::time::Instant::now() + WIPE_RETRY_WINDOW;
    let mut attempt = 0u32;
    loop {
        match remove_session_files(&session) {
            Ok(()) => {
                wiped = true;
                break;
            }
            Err(e) => {
                log::debug!("session wipe attempt {attempt} failed: {e}");
                attempt += 1;
                if tokio::time::Instant::now() + WIPE_RETRY_INTERVAL > deadline {
                    break;
                }
                tokio::time::sleep(WIPE_RETRY_INTERVAL).await;
            }
        }
    }
    if !wiped {
        log::error!("could not delete the session database");
        sh.emit(UiEvent::Fatal {
            message: "Could not delete the old session files. Close other instances of the app \
                      and restart."
                .to_string(),
        });
    }

    if explicit {
        let (store, ui) = (sh.store.clone(), sh.ui.clone());
        let media_dir = config.media_dir();
        let wipe = move || {
            events::with_store_and_ui(store, ui, |store| {
                store.clear_all()?;
                clear_dir_contents(&media_dir);
                Ok::<(), wa_store::StoreError>(())
            })
        };
        match tokio::task::spawn_blocking(wipe).await {
            Ok(Ok(())) => sh.emit(UiEvent::ChatsChanged),
            Ok(Err(e)) => {
                log::error!("wiping local history failed: {e}");
                sh.emit(UiEvent::Error {
                    message: format!("Could not clear local history: {e}"),
                });
            }
            Err(e) => log::error!("history wipe task failed: {e}"),
        }
    }
    if wiped {
        sh.emit(UiEvent::LoggedOut { reason });
    }
    wiped
}

/// Deletes everything inside `dir` but keeps `dir` itself. Failures are logged;
/// a leftover file must not block the logout.
fn clear_dir_contents(dir: &Path) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            log::warn!("cannot list media dir {}: {e}", dir.display());
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let result = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        if let Err(e) = result {
            log::warn!("cannot delete {}: {e}", path.display());
        }
    }
}

fn remove_session_files(session: &Path) -> std::io::Result<()> {
    let base = session.as_os_str().to_owned();
    for suffix in ["", "-wal", "-shm"] {
        let mut name = base.clone();
        name.push(suffix);
        match std::fs::remove_file(&name) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_outage_ends_keep_pending_messages() {
        let kept = [End::Offline, End::Reconnect];
        let failed = [
            End::Shutdown,
            End::LoggedOut { reason: String::new() },
            End::Restart,
            End::Retry,
            End::BotEnded,
            End::Halt { recoverable: true },
            End::Halt { recoverable: false },
        ];
        assert!(kept.iter().all(|e| !e.fails_pending()));
        assert!(failed.iter().all(End::fails_pending));
    }

    #[test]
    fn backoff_follows_the_schedule_and_caps_at_a_minute() {
        let mid = |n| backoff_delay(n, 0.5).as_secs_f64();
        assert_eq!([mid(0), mid(1), mid(2), mid(3), mid(4), mid(5)], [1.0, 2.0, 5.0, 10.0, 30.0, 60.0]);
        assert_eq!(mid(6), 60.0);
        assert_eq!(mid(u32::MAX), 60.0);
        assert_eq!(backoff_delay(4, 0.0), Duration::from_secs(24));
        assert_eq!(backoff_delay(4, 1.0), Duration::from_secs(36));
        assert!(backoff_delay(5, 99.0) <= Duration::from_secs(72));
        assert!((0.0..=1.0).contains(&jitter_unit()));
    }

    #[test]
    fn watchdog_rebuilds_only_after_the_offline_limit() {
        let t0 = Instant::now();
        let mut r = Recovery::default();
        assert!(!r.observe(t0, false));
        assert!(!r.observe(t0 + OFFLINE_LIMIT - Duration::from_secs(1), false));
        assert!(r.observe(t0 + OFFLINE_LIMIT, false));
    }

    #[test]
    fn coming_back_online_restarts_the_offline_clock() {
        let t0 = Instant::now();
        let mut r = Recovery::default();
        assert!(!r.observe(t0, false));
        assert!(!r.observe(t0 + Duration::from_secs(15), true));
        assert!(!r.observe(t0 + Duration::from_secs(16), false));
        assert!(!r.observe(t0 + Duration::from_secs(30), false));
        assert!(r.observe(t0 + Duration::from_secs(16) + OFFLINE_LIMIT, false));
    }

    #[test]
    fn schedule_resets_only_after_a_stable_connection() {
        let t0 = Instant::now();
        let mut r = Recovery::default();
        r.next_delay(0.5);
        r.next_delay(0.5);
        assert_eq!(r.attempts, 2);
        assert!(!r.observe(t0, true));
        assert!(!r.observe(t0 + STABLE_ONLINE - Duration::from_secs(1), true));
        assert_eq!(r.attempts, 2, "a short-lived connection must not reset the backoff");
        assert!(!r.observe(t0 + STABLE_ONLINE, true));
        assert_eq!(r.attempts, 0);
    }

    #[test]
    fn next_delay_advances_through_the_schedule() {
        let mut r = Recovery::default();
        let secs: Vec<u64> = (0..7).map(|_| r.next_delay(0.5).as_secs()).collect();
        assert_eq!(secs, [1, 2, 5, 10, 30, 60, 60]);
    }

    #[test]
    fn missing_close_notify_is_recognised_and_every_disconnect_is_retried_not_fatal() {
        let eof = DisconnectReason::ReadError(
            "peer closed connection without sending TLS close_notify: https://docs.rs/rustls/latest/rustls/manual/_03_howto/index.html#unexpected-eof".into(),
        );
        assert_eq!(classify_disconnect(&eof), DisconnectClass::MissingCloseNotify);
        assert_eq!(
            classify_disconnect(&DisconnectReason::ReadError("connection reset".into())),
            DisconnectClass::ReadError
        );
        assert_eq!(classify_disconnect(&DisconnectReason::StreamEnded), DisconnectClass::StreamEnded);
        assert_eq!(
            classify_disconnect(&DisconnectReason::ServerClose { code: Some(1006), reason: String::new() }),
            DisconnectClass::ServerClose
        );
        assert_eq!(classify_disconnect(&DisconnectReason::Unknown), DisconnectClass::Unknown);
        // The label is what reaches the log: no error text, so no host or address.
        assert!(!DisconnectClass::MissingCloseNotify.label().contains("docs.rs"));
    }

    #[test]
    fn clear_dir_contents_keeps_the_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("media");
        std::fs::create_dir_all(dir.join("chat1")).expect("mkdir");
        std::fs::write(dir.join("chat1").join("a.jpg"), b"x").expect("write");
        std::fs::write(dir.join("b.ogg"), b"y").expect("write");

        clear_dir_contents(&dir);
        assert!(dir.is_dir());
        assert_eq!(std::fs::read_dir(&dir).expect("read").count(), 0);

        clear_dir_contents(&tmp.path().join("missing"));
    }

    #[test]
    fn provided_settings_are_in_force_before_any_generation_starts() {
        let custom = EngineSettings {
            auto_download_images: false,
            send_read_receipts: false,
            send_presence: false,
            ..EngineSettings::default()
        };
        let (ui, _rx) = unbounded_channel();
        let (sig, _sig_rx) = unbounded_channel();
        let sh = Shared::new(
            Arc::new(Store::open_in_memory().expect("store")),
            ui,
            sig,
            Arc::new(tokio::sync::Mutex::new(())),
            Arc::new(MediaState::new(std::env::temp_dir())),
            seed_settings(custom.clone()),
        );
        assert_eq!(sh.settings(), custom);
        assert_ne!(sh.settings(), EngineSettings::default());
    }

    /// The UI sender stays open until the runtime has dropped every task, so a
    /// closed channel really means the files those tasks held are released.
    #[test]
    fn ui_channel_closes_only_after_runtime_tasks_released_their_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file_path = tmp.path().join("held.db");
        let file = std::fs::File::create(&file_path).expect("create");

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("runtime");
        let (ui, mut ui_rx) = unbounded_channel::<UiEvent>();
        // A leftover task owning an open file and a sender clone, like a
        // generation task holding the client and the store.
        let task_ui = ui.clone();
        rt.spawn(async move {
            let _held = (file, task_ui);
            std::future::pending::<()>().await;
        });
        drop(ui.clone());

        let thread = std::thread::spawn(move || {
            drop(Teardown { rt: Some(rt), _ui_last: ui });
        });
        // Blocks until every sender is gone, i.e. until the channel is closed.
        assert!(ui_rx.blocking_recv().is_none());
        std::fs::remove_dir_all(tmp.path()).expect("files are released once the channel closed");
        thread.join().expect("teardown thread");
    }

    /// Full engine round trip: start, shut down at once, wait for the UI
    /// channel to close, then delete the profile folder (fails on Windows while
    /// `session.db` or `messages.db` is still open).
    ///
    /// Needs network access (the bot starts connecting before it is stopped),
    /// so it is not part of the default run:
    /// `cargo test -p wa_engine delete_after_close -- --ignored`
    #[test]
    #[ignore = "starts the real WhatsApp client and touches the network"]
    fn profile_folder_can_be_deleted_once_the_ui_channel_closes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("profile");
        let (handle, mut ui_rx) = crate::start(EngineConfig { data_dir: dir.clone(), proxy: None }).expect("start");
        handle.send(Command::Shutdown);
        // The app drops its own clone of the store when it drops the profile.
        drop(handle);

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            while ui_rx.blocking_recv().is_some() {}
            let _ = done_tx.send(());
        });
        done_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("UI channel closed after shutdown");
        std::fs::remove_dir_all(&dir).expect("profile folder deletable after the channel closed");
    }

    #[test]
    fn pairing_restarts_are_capped_until_reset() {
        let mut budget = RestartBudget::default();
        for _ in 0..MAX_AUTO_PAIRING_RESTARTS {
            assert!(budget.try_restart());
        }
        assert!(!budget.try_restart());
        assert!(!budget.try_restart());
        budget.reset();
        assert!(budget.try_restart());
    }
}
