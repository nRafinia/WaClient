//! UI controller: owns the Slint models, reacts to engine events and
//! user callbacks. Everything here runs on the Slint (main) thread.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Display;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use chrono::Local;
use slint::{
    ComponentHandle, Image, Model, ModelRc, SharedString, Timer, TimerMode,
    VecModel, Weak,
};
use wa_engine::{Command, ConnState, MessageStatus, OutgoingMediaKind, STATUS_JID, Store, UiEvent, UiEventReceiver};
use wa_store::{ChatKind, ChatRow, MessageKind, MessageRow};

mod account_ui;
mod attach_ui;
mod call_ui;
mod chat_search;
mod composer;
mod contact_info;
mod emoji_ui;
mod esc;
mod folders_ui;
mod group_admin_ui;
mod group_new_ui;
mod info_ui;
mod location_ui;
mod lock_ui;
mod msg_actions;
mod msgops_ui;
mod phone_ui;
mod poll_ui;
mod prefs;
mod privacy;
pub mod proxy_ui;
mod removal;
mod save_as;
mod search_ui;
mod share_ui;
mod shortcuts;
mod status;
mod status_post_ui;
mod timer_ui;

use crate::audio::{PlayState, Player, PlayerEvent, Recorder};
use crate::decode::{DecodeDone, DecodeJob, DecodePool};
use crate::desktop::{Desktop, DesktopEvent};
use crate::emoji::ReactionChip;
use crate::models::{self, MsgImages, OpenPolicy, PlayView, RowUi};
use crate::profiles::{self, Profile, Registry};
use crate::settings::{self, AppSettings};
use crate::status_view::{PollViewModel, poll_view_model};
use crate::{ChatItem, MainWindow, MsgItem, ProfileItem, bridge, qr, shell};

const PAGE: usize = 50;
const TYPING_TTL: Duration = Duration::from_secs(6);
const TYPING_RESEND: Duration = Duration::from_secs(5);
const TYPING_PAUSE: Duration = Duration::from_secs(3);
const ERROR_BANNER_TTL: Duration = Duration::from_secs(6);
/// After rebuilding the message list the ListView re-lays out and may report
/// a transient viewport-y of 0; ignore load-older requests for this long.
const LAYOUT_GUARD: Duration = Duration::from_millis(450);
const REC_TICK: Duration = Duration::from_millis(100);
/// How often window focus is sampled (there is no focus event in Slint).
const FOCUS_POLL: Duration = Duration::from_millis(400);
/// Without a fresh QR code for this long the login screen offers a reload.
const QR_STALE: Duration = Duration::from_secs(70);
/// How long to wait for all engines to disconnect before quitting anyway.
const QUIT_GRACE: Duration = Duration::from_millis(1500);
/// Removing a profile: how long the engine may take to confirm the logout,
/// and then to stop, before the next step is forced.
const REMOVE_LOGOUT_GRACE: Duration = Duration::from_secs(15);
const REMOVE_STOP_GRACE: Duration = Duration::from_secs(10);
/// The engine's files may stay locked briefly after it stops.
const DELETE_RETRY_WINDOW: Duration = Duration::from_secs(10);
const DELETE_RETRY_INTERVAL: Duration = Duration::from_millis(300);
/// Scroll corrections after a model change: how often the layout is sampled,
/// how many consecutive samples with the same content height count as settled,
/// and the longest the correction keeps going.
const FIX_TICK: Duration = Duration::from_millis(40);
const FIX_STABLE_CHECKS: u8 = 2;
const FIX_MAX: Duration = Duration::from_secs(1);
/// Pixels of difference that count as a change in height / a user scroll.
const FIX_EPS: f32 = 1.0;
/// Id of the stand-in profile shown when the registry could not be read.
const PLACEHOLDER_ID: &str = "none";
/// Recordings shorter than this are discarded.
const MIN_VOICE_SECS: f32 = 1.0;
/// Longest edge of a decoded in-chat image.
const FULL_IMAGE_MAX_DIM: u32 = 1024;
/// Avatars are shown at most ~40px; 96px keeps them sharp on HiDPI screens.
const AVATAR_DIM: u32 = 96;
const DECODE_THREADS: usize = 2;
/// Coalesces the list refreshes caused by avatars finishing in a burst.
const AVATAR_REFRESH: Duration = Duration::from_millis(120);
/// A voice note is sent automatically at this length.
const MAX_VOICE_SECS: f32 = 15.0 * 60.0;
/// After a failed "load older" the chat stops retrying (and warning) for this long.
const OLDER_BACKOFF: Duration = Duration::from_secs(5);

thread_local! {
    static APP: RefCell<Option<Rc<App>>> = const { RefCell::new(None) };
}

/// Runs `f` with the controller if it is installed (UI thread only).
pub fn with_app(f: impl FnOnce(&App)) {
    let app = APP.with(|a| a.borrow().clone());
    if let Some(app) = app {
        f(&app);
    }
}

#[derive(Clone, Copy)]
enum LayoutFix {
    Bottom,
    Restore { y: f32, h: f32 },
}

/// A running scroll correction (see [`App::schedule_fix`]).
struct FixRun {
    fix: LayoutFix,
    started: Instant,
    last_h: Option<f32>,
    /// Scroll position right after our last correction; a different one with
    /// an unchanged height means the user scrolled.
    set_y: Option<f32>,
    stable: u8,
}

/// Per-profile state of the chat UI. Only the active profile's is rendered.
struct State {
    /// Every chat of the profile (archived included), independent of what the
    /// list currently shows (archive view, search results).
    chats: Vec<ChatRow>,
    query: String,
    selected: Option<String>,
    sel_row: Option<ChatRow>,
    msgs: Vec<MessageRow>,
    limit: usize,
    exhausted: bool,
    conn: ConnState,
    typing: HashMap<String, Instant>,
    /// Last online / last-seen report per contact.
    presence: HashMap<String, privacy::PresenceEntry>,
    downloading: HashSet<String>,
    /// Messages whose last download failed (cleared on retry).
    failed: HashSet<String>,
    /// Messages to open/play as soon as their download completes.
    after_download: HashSet<String>,
    /// Messages to "Save as…" once their download completes.
    save_after: HashSet<String>,
    play: Option<PlayView>,
    /// Full-size images of the open chat queued or being decoded.
    decoding: HashSet<String>,
    reply: Option<(String, String, String)>,
    last_typing_sent: Option<Instant>,
    composing_in: Option<String>,
    /// jid -> avatar file (None: the contact has none).
    avatar_paths: HashMap<String, Option<String>>,
    /// Decoded avatars by file path (None: could not be decoded).
    avatar_imgs: HashMap<String, Option<Image>>,
    avatar_pending: HashSet<String>,
    avatar_requested: HashSet<String>,
    /// Chats still to ask a profile picture for, drained by the batch timer.
    avatar_queue: VecDeque<String>,
    media: HashMap<String, Option<Image>>,
    guard_until: Instant,
    force_bottom: bool,
    /// Chat whose "load older" failed recently, and until when to leave it alone.
    older_backoff: Option<(String, Instant)>,
    /// The engine reported a non-retryable failure: no reconnect offer.
    fatal: bool,
    /// Poll tallies and reaction chips of the loaded messages, refreshed with
    /// every page load.
    polls: HashMap<String, PollViewModel>,
    reactions: HashMap<String, Vec<ReactionChip>>,
    /// Media files already known to exist, so a rebuild does not stat them
    /// again. Emptied when the media cache is cleared.
    seen_paths: HashSet<String>,
}

impl State {
    fn new() -> Self {
        State {
            chats: Vec::new(),
            query: String::new(),
            selected: None,
            sel_row: None,
            msgs: Vec::new(),
            limit: PAGE,
            exhausted: false,
            conn: ConnState::Connecting,
            typing: HashMap::new(),
            presence: HashMap::new(),
            downloading: HashSet::new(),
            failed: HashSet::new(),
            after_download: HashSet::new(),
            save_after: HashSet::new(),
            play: None,
            decoding: HashSet::new(),
            reply: None,
            last_typing_sent: None,
            composing_in: None,
            avatar_paths: HashMap::new(),
            avatar_imgs: HashMap::new(),
            avatar_pending: HashSet::new(),
            avatar_requested: HashSet::new(),
            avatar_queue: VecDeque::new(),
            media: HashMap::new(),
            guard_until: Instant::now(),
            force_bottom: false,
            older_backoff: None,
            fatal: false,
            polls: HashMap::new(),
            reactions: HashMap::new(),
            seen_paths: HashSet::new(),
        }
    }
}

/// What the login screen of a profile shows.
struct Login {
    qr: Option<Image>,
    status: String,
    show_retry: bool,
    /// Paired: the main screen is shown instead of the QR code.
    logged_in: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Removal {
    No,
    /// `Logout` was sent; waiting for the engine to confirm.
    LoggingOut,
    /// `Shutdown` was sent; waiting for the engine's event channel to close.
    Stopping,
}

/// Everything that belongs to one WhatsApp account: its engine connection,
/// store, chat state and login screen. All profiles stay connected; only the
/// active one is rendered into the window.
struct ProfileState {
    id: String,
    meta: RefCell<Profile>,
    store: Arc<Store>,
    send: Rc<dyn Fn(Command)>,
    st: RefCell<State>,
    login: RefCell<Login>,
    sync_text: RefCell<String>,
    /// Unread messages in chats that are neither muted nor archived.
    unread: Cell<u32>,
    qr_timer: Timer,
    remove_timer: Timer,
    removal: Cell<Removal>,
    fatal_msg: RefCell<Option<String>>,
    /// Removal is going ahead although the engine never confirmed the logout.
    unlink_unconfirmed: Cell<bool>,
    settings: RefCell<AppSettings>,
    /// Closes once the engine has shut down (see [`crate::bridge::try_spawn`]).
    done: RefCell<Option<mpsc::Receiver<()>>>,
}

impl ProfileState {
    fn name(&self) -> String {
        self.meta.borrow().name.clone()
    }
}

/// What a [`Starter`] returns for one profile.
pub struct ProfileRuntime {
    pub store: Arc<Store>,
    pub send: Rc<dyn Fn(Command)>,
    /// `None` for demo profiles, which have no engine.
    pub events: Option<UiEventReceiver>,
}

/// Starts the engine (or a stand-in) for a profile whose data lives in the given directory.
/// The third argument is the profile's saved settings, in force from the first event.
pub type Starter = Box<dyn Fn(&Profile, &Path, &AppSettings) -> anyhow::Result<ProfileRuntime>>;

pub struct App {
    win: Weak<MainWindow>,
    root: PathBuf,
    registry: RefCell<Registry>,
    /// False in demo mode: the registry is never written.
    persist: bool,
    starter: Starter,
    profs: RefCell<Vec<Rc<ProfileState>>>,
    active: RefCell<Rc<ProfileState>>,
    rail_model: Rc<VecModel<ProfileItem>>,
    total_unread: Cell<Option<u32>>,
    quitting: Cell<bool>,
    chats_model: Rc<VecModel<ChatItem>>,
    msgs_model: Rc<VecModel<MsgItem>>,
    pool: DecodePool,
    avatar_timer: Timer,
    player: Option<Player>,
    recorder: RefCell<Option<Recorder>>,
    rec_timer: Timer,
    typing_tick: Timer,
    banner_timer: Timer,
    compose_timer: Timer,
    fix_timer: Timer,
    fix_run: RefCell<Option<FixRun>>,
    /// Removed profiles whose data is deleted once their UI state is dropped.
    deletes: RefCell<Vec<removal::DeleteJob<ProfileState>>>,
    delete_timer: Timer,
    desktop: RefCell<Option<Desktop>>,
    was_active: Cell<bool>,
    focus_timer: Timer,
    status: status::StatusState,
    emoji: emoji_ui::EmojiState,
    search: search_ui::SearchState,
    staged: attach_ui::AttachState,
    close_to_tray: Cell<bool>,
    cache_busy: Cell<bool>,
    settings_dirty: RefCell<HashSet<String>>,
    settings_timer: Timer,
    lock: lock_ui::LockCtl,
    proxy: proxy_ui::ProxyCtl,
}

fn new_state(
    profile: &Profile,
    store: Arc<Store>,
    send: Rc<dyn Fn(Command)>,
    done: Option<mpsc::Receiver<()>>,
    saved: AppSettings,
    unread: u32,
) -> Rc<ProfileState> {
    Rc::new(ProfileState {
        id: profile.id.clone(),
        meta: RefCell::new(profile.clone()),
        store,
        send,
        st: RefCell::new(State::new()),
        login: RefCell::new(Login {
            qr: None,
            status: "Connecting…".to_string(),
            show_retry: false,
            logged_in: false,
        }),
        sync_text: RefCell::new(String::new()),
        unread: Cell::new(unread),
        qr_timer: Timer::default(),
        remove_timer: Timer::default(),
        removal: Cell::new(Removal::No),
        fatal_msg: RefCell::new(None),
        unlink_unconfirmed: Cell::new(false),
        settings: RefCell::new(saved),
        done: RefCell::new(done),
    })
}

/// A profile that is shown but never started (broken, mid-migration, or the
/// registry could not be read): no engine, an empty in-memory store, and the
/// reason in place of a QR code.
fn inert_state(profile: &Profile, reason: &str) -> anyhow::Result<Rc<ProfileState>> {
    let store = Store::open_in_memory().map(Arc::new).map_err(|e| anyhow::anyhow!("{e}"))?;
    let state = new_state(profile, store, Rc::new(|_| {}), None, AppSettings::default(), 0);
    {
        let mut st = state.st.borrow_mut();
        st.conn = ConnState::Disconnected { reason: reason.to_string() };
        st.fatal = true;
    }
    *state.fatal_msg.borrow_mut() = Some(reason.to_string());
    state.login.borrow_mut().status = reason.to_string();
    Ok(state)
}

/// The profile shown when the registry could not be read. It is not in the
/// registry (and its id is not a valid folder name), so it never touches disk.
fn stand_in_profile() -> Profile {
    Profile {
        id: PLACEHOLDER_ID.to_string(),
        name: "WaClient".to_string(),
        color: "#8696a0".to_string(),
        created_at: 0,
        order: 0,
        own_jid: None,
        in_root: false,
        migrating: false,
        broken: None,
    }
}

/// Why a profile must not be started, or `None` when it may be.
fn unready_reason(profile: &Profile) -> Option<String> {
    if profile.is_ready() {
        return None;
    }
    Some(match &profile.broken {
        Some(why) => format!("The profile \"{}\" cannot be opened: {why}", profile.name),
        None => format!(
            "The profile \"{}\" is still being migrated and cannot be opened yet. Restart WaClient.",
            profile.name
        ),
    })
}

/// Starts a profile's engine and its event bridge; the result is not yet
/// registered with any [`App`]. A profile that is not ready is never started.
fn start_runtime(
    root: &Path,
    persist: bool,
    starter: &Starter,
    win: &Weak<MainWindow>,
    profile: &Profile,
) -> anyhow::Result<Rc<ProfileState>> {
    if let Some(reason) = unready_reason(profile) {
        log::error!("not starting profile {}: {reason}", profile.id);
        return inert_state(profile, &reason);
    }
    let dir = profiles::data_dir(root, profile);
    let saved = settings::load(&prefs::settings_dir(root, persist, profile));
    let rt = starter(profile, &dir, &saved)?;
    let done = match rt.events {
        Some(events) => match bridge::try_spawn(profile.id.clone(), events, win.clone()) {
            Ok(done) => Some(done),
            Err(e) => {
                // The engine runs without a working event path: stop it.
                (rt.send)(Command::Shutdown);
                anyhow::bail!("could not start the event bridge: {e}");
            }
        },
        None => None,
    };
    shell::spawn_outgoing_cleanup(&dir);
    let unread = match rt.store.list_chats(false) {
        Ok(rows) => unread_total(&rows),
        Err(e) => {
            log::warn!("initial unread count of {} failed: {e}", profile.id);
            0
        }
    };
    let state = new_state(profile, rt.store, rt.send, done, saved, unread);
    arm_qr_timer(&state);
    Ok(state)
}

impl App {
    /// Creates the controller, starts the profiles of `registry` (creating one
    /// if there is none), wires every Slint callback and registers it for
    /// [`with_app`]. Fails only if no profile could be started.
    pub fn install(
        win: &MainWindow,
        root: PathBuf,
        mut registry: Registry,
        persist: bool,
        starter: Starter,
    ) -> anyhow::Result<Rc<App>> {
        let weak = win.as_weak();
        // Before any engine: an unusable proxy setting holds every profile back.
        let proxy = proxy_ui::ProxyCtl::new(persist.then(|| root.clone()));
        let mut notes: Vec<String> = Vec::new();
        let mut started: Vec<Rc<ProfileState>> = Vec::new();
        let mut failures = Vec::new();
        if let Some(why) = registry.load_error.clone() {
            // Nothing is started or added on a registry that could not be read:
            // a stand-in profile carries the message and quitting still works.
            let message = "profiles.json could not be read — nothing was changed.                            Close other programs that may lock it and restart.";
            log::error!("profiles.json unreadable: {why}");
            started.push(inert_state(&stand_in_profile(), message)?);
        } else {
            if registry.profiles.is_empty() {
                registry.add_in(&root, None, chrono::Utc::now().timestamp())?;
            }
            // The profile used last time goes first so it becomes the active one.
            let mut order: Vec<Profile> = registry.profiles.clone();
            if let Some(last) = registry.last_active.clone() {
                order.sort_by_key(|p| p.id != last);
            }
            for profile in &order {
                let runtime = if proxy.blocked().is_some() && profile.is_ready() {
                    inert_state(profile, proxy_ui::HELD_TEXT)
                } else {
                    start_runtime(&root, persist, &starter, &weak, profile)
                };
                match runtime {
                    Ok(p) => {
                        if let Some(reason) = unready_reason(profile) {
                            notes.push(reason);
                        }
                        started.push(p);
                    }
                    Err(e) => {
                        log::error!("could not start profile {}: {e:#}", profile.id);
                        failures.push(format!("{}: {e:#}", profile.name));
                    }
                }
            }
            registry.last_active = started.first().map(|p| p.id.clone());
        }
        let Some(first) = started.first().cloned() else {
            anyhow::bail!("no profile could be started ({})", failures.join("; "));
        };
        if !registry.unadopted.is_empty() {
            notes.push(format!(
                "Profile folders left untouched (profile limit reached): {}.",
                registry.unadopted.join(", ")
            ));
        }
        if !registry.sweep_failed.is_empty() {
            notes.push(format!(
                "Data of removed profiles could not be deleted yet ({}); this is retried at the next start.",
                registry.sweep_failed.join(", ")
            ));
        }

        let chats_model = Rc::new(VecModel::<ChatItem>::default());
        let msgs_model = Rc::new(VecModel::<MsgItem>::default());
        let rail_model = Rc::new(VecModel::<ProfileItem>::default());
        win.set_chats(ModelRc::from(chats_model.clone()));
        win.set_messages(ModelRc::from(msgs_model.clone()));
        win.set_profiles(ModelRc::from(rail_model.clone()));

        let root_for_recents = root.clone();
        let app = Rc::new(App {
            win: weak,
            root,
            registry: RefCell::new(registry),
            persist,
            starter,
            profs: RefCell::new(started),
            active: RefCell::new(first),
            rail_model,
            total_unread: Cell::new(None),
            quitting: Cell::new(false),
            chats_model,
            msgs_model,
            pool: DecodePool::new(DECODE_THREADS, |done| {
                let queued = slint::invoke_from_event_loop(move || with_app(|a| a.on_decoded(done)));
                if let Err(e) = queued {
                    log::debug!("decoded image not delivered: {e}");
                }
            }),
            avatar_timer: Timer::default(),
            player: Player::new(|ev| {
                let queued = slint::invoke_from_event_loop(move || with_app(|a| a.on_player(ev)));
                if let Err(e) = queued {
                    log::debug!("player event dropped: {e}");
                }
            })
            .map_err(|e| log::error!("audio playback unavailable: {e:#}"))
            .ok(),
            recorder: RefCell::new(None),
            rec_timer: Timer::default(),
            typing_tick: Timer::default(),
            banner_timer: Timer::default(),
            compose_timer: Timer::default(),
            fix_timer: Timer::default(),
            fix_run: RefCell::new(None),
            deletes: RefCell::new(Vec::new()),
            delete_timer: Timer::default(),
            desktop: RefCell::new(None),
            was_active: Cell::new(false),
            focus_timer: Timer::default(),
            status: status::StatusState::new(),
            emoji: emoji_ui::EmojiState::new(persist.then(|| root_for_recents.join("emoji_recents.json"))),
            search: search_ui::SearchState::new(),
            staged: attach_ui::AttachState::new(),
            close_to_tray: Cell::new(true),
            cache_busy: Cell::new(false),
            settings_dirty: RefCell::new(HashSet::new()),
            settings_timer: Timer::default(),
            lock: lock_ui::LockCtl::new(persist.then(|| root_for_recents.clone())),
            proxy,
        });
        app.status.attach(win);
        app.emoji.attach(win);
        app.staged.attach(win);
        composer::wire(win);
        msgops_ui::wire(win);
        share_ui::wire(win);
        group_admin_ui::wire(win);
        group_new_ui::wire(win);
        folders_ui::wire(win);
        timer_ui::wire(win);
        phone_ui::wire(win);
        call_ui::wire(win);
        lock_ui::wire(win);
        proxy_ui::wire(win);

        win.on_search(|q| with_app(|a| a.search(q.as_str())));
        win.on_chat_selected(|jid| with_app(|a| a.select_chat(jid.as_str())));
        win.on_send_message(|t| with_app(|a| a.send_text(t.as_str())));
        win.on_composing(|t| with_app(|a| a.on_edit(t.as_str())));
        win.on_attach(|| with_app(App::attach));
        win.on_reply_to(|id| with_app(|a| a.reply_to(id.as_str())));
        win.on_cancel_reply(|| with_app(App::cancel_reply));
        win.on_step_chat(|forward| with_app(|a| a.step_chat(forward)));
        win.on_step_unread_chat(|forward| with_app(|a| a.step_unread_chat(forward)));
        win.on_edit_last(|| with_app(App::edit_last_message));
        win.on_escape(|| {
            let mut used = false;
            with_app(|a| used = a.is_locked() || a.escape_stopped_recording() || a.escape());
            used
        });
        win.on_download(|id| with_app(|a| a.download(id.as_str())));
        win.on_save_as(|id| with_app(|a| a.save_as(id.as_str())));
        win.on_toast_action_clicked(|| with_app(App::toast_action));
        win.on_open(|id| with_app(|a| a.open_item(id.as_str())));
        win.on_show_in_folder(|id| with_app(|a| a.show_in_folder(id.as_str())));
        win.on_toggle_play(|id| with_app(|a| a.toggle_play(id.as_str())));
        win.on_start_record(|| with_app(App::start_record));
        win.on_cancel_record(|| with_app(App::cancel_record));
        win.on_send_record(|| with_app(App::send_record));
        win.on_load_older(|| with_app(|a| a.load_older(false)));
        win.on_jump_latest(|| with_app(App::jump_latest));
        win.on_delete_selected(|| with_app(App::confirm_select_delete));
        win.on_cancel_delete_selected(|| with_app(App::cancel_select_delete));
        win.on_unblock_chat(|| with_app(App::unblock_open_chat));
        win.on_dismiss_banner(|| with_app(App::hide_banner));
        win.on_retry_qr(|| with_app(App::retry_qr));
        win.on_reconnect(|| with_app(App::reconnect));
        win.on_profile_selected(|id| {
            with_app(|a| {
                if !a.profile_switch_refused() {
                    a.activate(id.as_str());
                }
            })
        });
        win.on_add_profile(|| with_app(App::add_profile));
        win.on_move_profile(|id, to| with_app(|a| a.move_profile(id.as_str(), to)));
        win.on_profile_at(|i| with_app(|a| a.activate_at(i)));
        win.on_rename_profile(|id, name| with_app(|a| a.rename_profile(id.as_str(), name.as_str())));
        win.on_remove_profile(|id| with_app(|a| a.request_remove(id.as_str())));
        win.on_settings_opened(|| with_app(App::open_settings));
        win.on_settings_changed(|m| {
            with_app(|a| {
                a.settings_changed(m);
                a.refresh_privacy_ui();
            })
        });
        win.on_incognito_toggled(|| with_app(App::toggle_incognito));
        win.on_chat_receipts(|jid, enabled| with_app(|a| a.set_chat_receipts(jid.as_str(), enabled)));
        win.on_chat_read_now(|jid| with_app(|a| a.read_receipts_now(jid.as_str())));
        win.on_clear_cache(|| with_app(App::clear_cache));
        win.on_settings_logout(|| with_app(App::settings_logout));
        win.on_status_tab_changed(|on| {
            if on {
                with_app(App::refresh_statuses);
            }
        });
        win.on_status_open(|i| with_app(|a| a.open_status(i)));
        win.on_status_next(|| with_app(|a| a.status_nav(true)));
        win.on_status_prev(|| with_app(|a| a.status_nav(false)));
        win.on_status_close(|| with_app(App::status_close));
        win.on_status_download(|| with_app(App::status_download));
        win.on_status_open_media(|| with_app(App::status_open_media));
        win.on_status_pause(|p| with_app(|a| a.status_pause(p)));
        win.on_react(|id, e| with_app(|a| a.react(id.as_str(), e.as_str())));
        win.on_emoji_search(|q| with_app(|a| a.emoji_search(q.as_str())));
        win.on_emoji_picked(|e| with_app(|a| a.emoji_picked(e.as_str())));
        win.on_emoji_insert(|e| with_app(|a| a.emoji_insert(e.as_str())));
        win.on_copy_text(|id| with_app(|a| a.copy_message_or_coords(id.as_str())));
        win.on_delete_message(|id, everyone| with_app(|a| a.delete_message(id.as_str(), everyone)));
        win.on_open_link(|url| with_app(|a| a.open_link(url.as_str())));
        win.on_contact_message(|digits| with_app(|a| a.contact_message(digits.as_str())));
        win.on_contact_copy(|phone| with_app(|a| a.copy_text(phone.as_str())));
        win.on_chat_mark_read(|jid| with_app(|a| a.send(Command::MarkChatRead { chat_jid: jid.to_string() })));
        win.on_chat_pin(|jid, on| {
            with_app(|a| {
                log::info!("chat menu: SetPinned({on}) for {}", models::redact_jid(jid.as_str()));
                a.send(Command::SetPinned { chat_jid: jid.to_string(), pinned: on });
            });
        });
        win.on_chat_mute(|jid, choice| {
            with_app(|a| {
                let until = models::mute_until(choice, chrono::Utc::now().timestamp());
                log::info!("chat menu: SetMuted(choice {choice}) for {}", models::redact_jid(jid.as_str()));
                a.send(Command::SetMuted { chat_jid: jid.to_string(), until });
            });
        });
        win.on_chat_archive(|jid, on| {
            with_app(|a| {
                log::info!("chat menu: SetArchived({on}) for {}", models::redact_jid(jid.as_str()));
                a.send(Command::SetArchived { chat_jid: jid.to_string(), archived: on });
            });
        });
        win.on_archived_toggled(|| with_app(App::refresh_chats));
        win.on_info_open(|| with_app(App::open_info));
        win.on_info_close(|| with_app(App::info_close));
        win.on_info_open_photo(|| with_app(App::info_open_photo));
        win.on_info_open_member(|jid| with_app(|a| a.info_open_member(jid.as_str())));
        win.on_info_copy_phone(|| with_app(App::info_copy_phone));
        win.on_info_member_search(|q| with_app(|a| a.info_member_search(q.as_str())));
        info_ui::wire_gallery(&win);
        location_ui::wire(&win);
        poll_ui::wire(&win);
        status_post_ui::wire(&win);
        account_ui::wire(&win);
        win.on_chats_visible(|first, count| with_app(|a| a.chats_visible(first, count)));
        win.on_msg_search(|q| with_app(|a| a.search_input(q.as_str())));
        win.on_msg_search_next(|| with_app(App::search_next));
        win.on_msg_search_prev(|| with_app(App::search_prev));
        win.on_msg_search_close(|| with_app(App::search_close));
        win.on_paste_requested(|| {
            let mut handled = false;
            with_app(|a| handled = a.paste_from_clipboard());
            handled
        });
        win.on_paste_unfocused(|| with_app(App::paste_unfocused));
        win.on_attach_send(|caption| with_app(|a| a.attach_send(caption.as_str())));
        win.on_attach_cancel(|| with_app(App::attach_cancel));
        win.on_attach_remove(|i| with_app(|a| a.attach_remove(i)));

        app.focus_timer.start(TimerMode::Repeated, FOCUS_POLL, || with_app(App::poll_focus));
        APP.with(|a| *a.borrow_mut() = Some(app.clone()));
        app.lock_start();
        app.proxy_start();

        notes.extend(failures.into_iter().map(|f| format!("Could not start profile {f}")));
        app.save_registry();
        app.render_active();
        // After render_active (which clears the banner); a persistent fatal
        // banner of the active profile is not replaced.
        if !notes.is_empty() && app.p().fatal_msg.borrow().is_none() {
            app.show_banner(1, &notes.join("  "));
        }
        Ok(app)
    }

    fn win(&self) -> Option<MainWindow> {
        self.win.upgrade()
    }

    fn ok<T, E: Display>(&self, ctx: &str, r: Result<T, E>) -> Option<T> {
        match r {
            Ok(v) => Some(v),
            Err(e) => {
                log::error!("{ctx}: {e}");
                self.show_banner(1, &format!("{ctx}: {e}"));
                None
            }
        }
    }

    // ---- profiles ------------------------------------------------------

    /// The profile that is rendered in the window.
    fn p(&self) -> Rc<ProfileState> {
        self.active.borrow().clone()
    }

    fn find(&self, id: &str) -> Option<Rc<ProfileState>> {
        self.profs.borrow().iter().find(|p| p.id == id).cloned()
    }

    fn is_active(&self, p: &ProfileState) -> bool {
        self.active.borrow().id == p.id
    }

    /// Sends a command to the active profile's engine.
    fn send(&self, cmd: Command) {
        (self.p().send)(cmd);
    }

    fn multi(&self) -> bool {
        self.profs.borrow().len() > 1
    }

    fn save_registry(&self) {
        if !self.persist || self.registry.borrow().load_error.is_some() {
            return;
        }
        let saved = self.registry.borrow().save(&self.root);
        if let Err(e) = saved {
            log::error!("could not save the profile registry: {e}");
            self.show_banner(1, &format!("Could not save the profile list: {e}"));
        }
    }

    /// Ids in rail order, for the debug switch of the demo.
    #[cfg(debug_assertions)]
    pub fn profile_ids(&self) -> Vec<String> {
        self.registry.borrow().profiles.iter().map(|p| p.id.clone()).collect()
    }

    /// Switches the window to another profile; every profile keeps running.
    pub fn activate(&self, id: &str) {
        let Some(next) = self.find(id) else { return };
        if self.is_active(&next) {
            return;
        }
        // The outgoing profile's chat is closed: a profile is always entered
        // with no chat open.
        self.status_close();
        self.close_chat();
        // Pickers and dialogs were opened for the outgoing profile.
        self.share_profile_changed();
        self.group_new_profile_changed();
        self.folders_profile_changed();
        self.p().st.borrow_mut().query.clear();
        *self.active.borrow_mut() = next.clone();
        self.registry.borrow_mut().last_active = Some(next.id.clone());
        self.save_registry();
        self.render_active();
        // "Other profile" labels depend on which profile is active.
        self.calls_render();
    }

    /// Fills the window from the active profile's state.
    fn render_active(&self) {
        let p = self.p();
        let Some(win) = self.win() else { return };
        self.hide_banner();
        win.set_show_settings(false);
        self.apply_profile_prefs(&p);
        self.refresh_privacy_ui();
        p.st.borrow_mut().query.clear();
        win.invoke_clear_search();
        // The archive view belongs to the profile that opened it.
        win.set_archived_mode(false);
        self.ops_profile_changed();
        self.search_reset();
        win.set_sync_text(p.sync_text.borrow().as_str().into());
        self.render_login(&p);
        self.update_reconnect();
        let fatal = p.st.borrow().fatal;
        if let Some(msg) = p.fatal_msg.borrow().as_ref().filter(|_| fatal) {
            self.show_banner(2, msg);
        }
        self.pool.set_open_chat(p.st.borrow().selected.as_deref());
        self.sync_reply();
        if p.login.borrow().logged_in {
            self.refresh_chats();
            self.reload_open_chat();
        } else {
            self.chats_model.set_vec(Vec::new());
            self.msgs_model.set_vec(Vec::new());
            win.set_has_chat(false);
            win.set_selected_jid(SharedString::default());
        }
        self.refresh_statuses();
        self.refresh_rail();
        self.update_total_badge();
    }

    /// Loads the messages of the active profile's open chat (kept while the
    /// profile was in the background) and shows them.
    fn reload_open_chat(&self) {
        let Some(win) = self.win() else { return };
        let p = self.p();
        let Some(jid) = p.st.borrow().selected.clone() else {
            self.msgs_model.set_vec(Vec::new());
            win.set_has_chat(false);
            win.set_selected_jid(SharedString::default());
            return;
        };
        let limit = p.st.borrow().limit.max(PAGE);
        let Some(page) = self.ok("load messages", p.store.messages_page(&jid, None, limit as u32))
        else {
            return;
        };
        {
            let mut st = p.st.borrow_mut();
            st.exhausted = page.len() < limit;
            st.msgs = page;
            st.force_bottom = false;
        }
        win.set_selected_jid(jid.into());
        win.set_has_chat(true);
        win.invoke_clear_composer();
        self.rebuild_rows();
        self.update_header();
        self.schedule_fix(LayoutFix::Bottom);
        if self.window_active() {
            self.mark_open_chat_read();
        }
    }

    fn render_login(&self, p: &ProfileState) {
        let Some(win) = self.win() else { return };
        let l = p.login.borrow();
        win.set_logged_in(l.logged_in);
        match &l.qr {
            Some(img) => {
                win.set_qr(img.clone());
                win.set_has_qr(true);
            }
            None => win.set_has_qr(false),
        }
        win.set_login_status(l.status.as_str().into());
        win.set_show_retry_qr(l.show_retry);
        let name = if self.multi() { p.name() } else { String::new() };
        win.set_login_profile(name.into());
    }

    fn refresh_rail(&self) {
        let Some(win) = self.win() else { return };
        let active_id = self.active.borrow().id.clone();
        let (order, can_add) = {
            let reg = self.registry.borrow();
            (
                reg.profiles.iter().map(|m| m.id.clone()).collect::<Vec<_>>(),
                reg.can_add() && reg.load_error.is_none(),
            )
        };
        let mut items = Vec::with_capacity(order.len());
        for id in &order {
            if let Some(p) = self.find(id) {
                items.push(self.rail_item(&p, p.id == active_id));
            }
        }
        if let Some(active) = items.iter().find(|i| i.active) {
            win.set_active_profile(active.clone());
        }
        self.rail_model.set_vec(items);
        self.refresh_title();
        win.set_can_add_profile(can_add);
        let tip = if can_add {
            format!("Add a profile ({} of {} used)", order.len(), profiles::MAX_PROFILES)
        } else {
            format!("Profile limit reached: at most {} profiles", profiles::MAX_PROFILES)
        };
        win.set_add_profile_tip(tip.into());
    }

    fn rail_item(&self, p: &ProfileState, active: bool) -> ProfileItem {
        let meta = p.meta.borrow().clone();
        let avatar = meta.own_jid.as_deref().and_then(|jid| {
            let mut st = p.st.borrow_mut();
            self.avatar(p, &mut st, jid)
        });
        let (r, g, b) = profiles::parse_color(&meta.color);
        let state = match p.st.borrow().conn {
            ConnState::Connected => 0,
            ConnState::Connecting => 1,
            ConnState::Disconnected { .. } => 2,
        };
        ProfileItem {
            id: meta.id.as_str().into(),
            name: meta.name.as_str().into(),
            initials: models::initials(&meta.name).into(),
            color: slint::Color::from_rgb_u8(r, g, b),
            has_avatar: avatar.is_some(),
            avatar: avatar.unwrap_or_default(),
            unread: i32::try_from(p.unread.get()).unwrap_or(i32::MAX),
            state,
            active,
        }
    }

    pub fn add_profile(&self) {
        if self.proxy.blocked().is_some() {
            self.show_banner(1, "Choose how to connect first: the proxy settings could not be read");
            return;
        }
        if self.registry.borrow().load_error.is_some() {
            self.show_banner(1, "Profiles cannot be added while profiles.json is unreadable");
            return;
        }
        let added = self.registry.borrow_mut().add_in(&self.root, None, chrono::Utc::now().timestamp());
        let profile = match added {
            Ok(p) => p,
            Err(e) => {
                self.show_banner(1, &e.to_string());
                return;
            }
        };
        match start_runtime(&self.root, self.persist, &self.starter, &self.win, &profile) {
            Ok(p) => {
                self.profs.borrow_mut().push(p.clone());
                self.save_registry();
                // The new engine emits its QR code shortly; the login screen
                // of the new profile shows "Connecting…" until then.
                self.activate(&p.id);
                self.refresh_rail();
            }
            Err(e) => {
                log::error!("could not start the new profile: {e:#}");
                self.discard_failed_profile(&profile);
                self.show_banner(1, &format!("Could not start the new profile: {e:#}"));
            }
        }
    }

    /// Takes back a profile whose start failed. The engine may already have
    /// created its folder, so the folder is queued for deletion too.
    fn discard_failed_profile(&self, profile: &Profile) {
        if !self.persist {
            self.registry.borrow_mut().remove(&profile.id);
            return;
        }
        let marked = self.registry.borrow_mut().mark_pending_delete(&self.root, &profile.id);
        match marked {
            Ok(_) => self.queue_delete(profile.clone(), std::rc::Weak::new()),
            Err(e) => {
                log::error!("could not queue the folder of profile {} for deletion: {e}", profile.id);
                self.registry.borrow_mut().remove(&profile.id);
                if let Err(e) = profiles::remove_data(&self.root, profile) {
                    log::error!("could not delete the folder of profile {}: {e}", profile.id);
                }
            }
        }
    }

    /// Moves a profile to `to` in the rail. The registry order is persisted and
    /// `profs` follows it, so the tray and notifications use the same order.
    fn move_profile(&self, id: &str, to: i32) {
        let to = usize::try_from(to).unwrap_or(0);
        if !self.registry.borrow_mut().reorder(id, to) {
            return;
        }
        let order: Vec<String> = self.registry.borrow().profiles.iter().map(|p| p.id.clone()).collect();
        self.profs
            .borrow_mut()
            .sort_by_key(|p| order.iter().position(|i| *i == p.id).unwrap_or(usize::MAX));
        self.save_registry();
        self.refresh_rail();
    }

    /// Ctrl+1..5: activates the profile at rail position `i`.
    fn activate_at(&self, i: i32) {
        if self.profile_switch_refused() {
            return;
        }
        let id = usize::try_from(i)
            .ok()
            .and_then(|i| self.registry.borrow().profiles.get(i).map(|p| p.id.clone()));
        if let Some(id) = id {
            self.activate(&id);
        }
    }

    /// While a voice message is being recorded, switching profile would orphan
    /// the recording, so the switch is refused with a toast.
    fn profile_switch_refused(&self) -> bool {
        let refused = switch_blocked_by_recording(self.recorder.borrow().is_some());
        if refused {
            self.show_toast("Finish or cancel the voice message first");
        }
        refused
    }

    fn rename_profile(&self, id: &str, name: &str) {
        let Some(p) = self.find(id) else { return };
        if !self.registry.borrow_mut().rename(id, name) {
            self.show_banner(1, "A profile name cannot be empty");
            return;
        }
        let new_name = self.registry.borrow().get(id).map(|m| m.name.clone());
        if let Some(new_name) = new_name {
            p.meta.borrow_mut().name = new_name;
        }
        self.save_registry();
        self.refresh_rail();
        if self.is_active(&p) {
            self.render_login(&p);
        }
    }

    /// "Log out & remove": unlink the device, stop the engine, then delete the
    /// profile's data (see [`App::finalize_remove`]).
    fn request_remove(&self, id: &str) {
        let Some(p) = self.find(id) else { return };
        if p.removal.get() != Removal::No {
            return;
        }
        if p.done.borrow().is_none() {
            // No engine to wind down (demo and inert profiles).
            self.finalize_remove(&p);
            return;
        }
        if self.engine_gone(&p) {
            // Nothing left to wait for. A paired device could not be unlinked.
            (p.send)(Command::Shutdown);
            p.unlink_unconfirmed.set(p.login.borrow().logged_in);
            self.finalize_remove(&p);
            return;
        }
        // A profile that never paired has nothing to unlink.
        if p.login.borrow().logged_in {
            p.removal.set(Removal::LoggingOut);
            (p.send)(Command::Logout);
        } else {
            p.removal.set(Removal::Stopping);
            (p.send)(Command::Shutdown);
        }
        self.arm_remove_watchdog(&p);
        self.show_banner(1, &format!("Removing \"{}\"…", p.name()));
    }

    /// The engine already failed or closed its channel.
    fn engine_gone(&self, p: &ProfileState) -> bool {
        p.fatal_msg.borrow().is_some()
            || p.done
                .borrow()
                .as_ref()
                .is_some_and(|d| matches!(d.try_recv(), Err(mpsc::TryRecvError::Disconnected)))
    }

    fn arm_remove_watchdog(&self, p: &ProfileState) {
        let id = p.id.clone();
        let grace = if p.removal.get() == Removal::LoggingOut { REMOVE_LOGOUT_GRACE } else { REMOVE_STOP_GRACE };
        p.remove_timer.start(TimerMode::SingleShot, grace, move || {
            with_app(|a| a.remove_watchdog(&id));
        });
    }

    /// The engine did not finish in time: force the next step.
    fn remove_watchdog(&self, id: &str) {
        let Some(p) = self.find(id) else { return };
        match p.removal.get() {
            Removal::LoggingOut => {
                log::warn!("logout of profile {id} did not finish; stopping its engine");
                p.unlink_unconfirmed.set(true);
                p.removal.set(Removal::Stopping);
                (p.send)(Command::Shutdown);
                self.arm_remove_watchdog(&p);
            }
            Removal::Stopping => {
                // The folder is queued in the registry, so if it stays locked
                // it is deleted at the next start; the user is told then.
                log::warn!("engine of profile {id} did not stop; removing it anyway");
                self.finalize_remove(&p);
            }
            Removal::No => {}
        }
    }

    /// The engine's event channel closed.
    pub fn on_engine_closed(&self, id: &str) {
        if self.quitting.get() {
            return;
        }
        let Some(p) = self.find(id) else { return };
        if p.removal.get() != Removal::No {
            if p.removal.get() == Removal::LoggingOut {
                p.unlink_unconfirmed.set(true);
            }
            self.finalize_remove(&p);
            return;
        }
        let message = "The connection engine of this profile stopped unexpectedly. Restart WaClient.";
        log::error!("engine of profile {id} closed its event channel");
        {
            let mut st = p.st.borrow_mut();
            st.conn = ConnState::Disconnected { reason: "engine stopped".to_string() };
            st.fatal = true;
        }
        *p.fatal_msg.borrow_mut() = Some(message.to_string());
        if self.is_active(&p) {
            self.update_header();
            self.update_reconnect();
            self.show_banner(2, message);
        } else {
            self.show_banner(1, &format!("[{}] {message}", p.name()));
        }
        self.refresh_rail();
    }

    /// Drops the profile from the UI and the registry and queues its files for
    /// deletion.
    ///
    /// The registry change is persisted first (`mark_pending_delete`); if that
    /// fails nothing is removed. The files are deleted later by
    /// [`App::delete_tick`], once the profile's `ProfileState` (which owns the
    /// UI's `Arc<Store>` and the engine's command closure) has been dropped.
    fn finalize_remove(&self, p: &Rc<ProfileState>) {
        p.remove_timer.stop();
        p.qr_timer.stop();
        if self.profs.borrow().len() <= 1 {
            // The app always keeps one profile: start a fresh one first.
            self.add_profile();
            if self.profs.borrow().len() <= 1 {
                p.removal.set(Removal::No);
                self.show_banner(1, "Could not create a replacement profile; nothing was removed");
                return;
            }
        }
        let neighbour = if self.is_active(p) {
            let reg = self.registry.borrow();
            let idx = reg.profiles.iter().position(|m| m.id == p.id).unwrap_or(0);
            let pick = if idx > 0 { idx - 1 } else { idx + 1 };
            reg.profiles.get(pick).map(|m| m.id.clone())
        } else {
            None
        };
        let meta = p.meta.borrow().clone();
        let marked = if self.persist {
            self.registry.borrow_mut().mark_pending_delete(&self.root, &p.id)
        } else {
            Ok(self.registry.borrow_mut().remove(&p.id))
        };
        if let Err(e) = marked {
            log::error!("could not record the removal of profile {}: {e}", p.id);
            p.removal.set(Removal::No);
            self.show_banner(1, &format!("Could not remove \"{}\": {e}. Nothing was deleted.", meta.name));
            return;
        }
        if let Some(id) = neighbour {
            self.activate(&id);
        }
        // From here on nothing may save this profile's settings again: that
        // would recreate its folder.
        self.settings_dirty.borrow_mut().remove(&p.id);
        let holder = Rc::downgrade(p);
        self.profs.borrow_mut().retain(|q| q.id != p.id);
        self.update_total_badge();
        self.refresh_rail();
        self.hide_banner();
        if p.unlink_unconfirmed.get() {
            self.show_banner(
                2,
                "The device may still appear under Linked devices on your phone — remove it there.",
            );
        }
        if self.persist {
            self.queue_delete(meta, holder);
        }
    }

    /// Starts deleting a removed profile's files as soon as `holder` is gone.
    /// The first attempt runs on a timer tick, after the callers (which still
    /// hold their own `Rc` of the profile) have returned.
    fn queue_delete(&self, profile: Profile, holder: std::rc::Weak<ProfileState>) {
        self.deletes.borrow_mut().push(removal::DeleteJob {
            profile,
            holder,
            deadline: Instant::now() + DELETE_RETRY_WINDOW,
        });
        self.delete_timer.start(TimerMode::Repeated, DELETE_RETRY_INTERVAL, || with_app(App::delete_tick));
    }

    fn delete_tick(&self) {
        let gave_up = {
            let mut reg = self.registry.borrow_mut();
            removal::step(&mut reg, &self.root, &mut self.deletes.borrow_mut(), Instant::now())
        };
        if self.deletes.borrow().is_empty() {
            self.delete_timer.stop();
        }
        for profile in gave_up {
            let text = if profile.in_root {
                format!("Could not delete the files of \"{}\" in the data folder; delete them by hand.", profile.name)
            } else {
                format!(
                    "Could not delete the data folder of \"{}\" yet (it is still in use). It is deleted the next time WaClient starts.",
                    profile.name
                )
            };
            self.show_banner(2, &text);
        }
    }

    // ---- desktop integration -------------------------------------------

    /// Takes ownership of the tray/toast integration (it must live for the
    /// whole run) and routes its events onto the UI thread.
    pub fn attach_desktop(&self, desktop: Desktop) {
        desktop.on_event(|ev| {
            let queued = slint::invoke_from_event_loop(move || with_app(|a| a.on_desktop(ev)));
            if let Err(e) = queued {
                log::debug!("desktop event dropped: {e}");
            }
        });
        *self.desktop.borrow_mut() = Some(desktop);
        self.total_unread.set(None);
        self.update_total_badge();
    }

    fn window_active(&self) -> bool {
        self.win().is_some_and(|w| shell::is_active(&w))
    }

    fn on_desktop(&self, ev: DesktopEvent) {
        match ev {
            DesktopEvent::OpenWindow => self.show_window(),
            DesktopEvent::OpenChat { profile, chat_jid } => {
                self.show_window();
                // A locked app only comes to the front; the chat opens after unlocking.
                if self.is_locked() {
                    return;
                }
                if self.find(&profile).is_some() {
                    if self.profile_switch_refused() {
                        return;
                    }
                    self.activate(&profile);
                    self.select_chat(&chat_jid);
                }
            }
            DesktopEvent::Quit => self.quit(),
        }
    }

    pub fn show_window(&self) {
        if let Some(win) = self.win() {
            shell::bring_to_front(&win);
        }
    }

    /// Asks every engine to stop and returns the channels that close once each
    /// one has finished.
    fn shutdown_all(&self) -> Vec<(String, mpsc::Receiver<()>)> {
        self.quitting.set(true);
        self.flush_settings();
        self.folders_flush();
        composer::flush_drafts();
        let profs = self.profs.borrow().clone();
        profs
            .iter()
            .filter_map(|p| {
                (p.send)(Command::Shutdown);
                p.done.borrow_mut().take().map(|d| (p.id.clone(), d))
            })
            .collect()
    }

    /// Asks every engine to stop, hides the window and ends the event loop once
    /// the engines are gone (or after a short grace period).
    pub fn quit(&self) {
        let dones = self.shutdown_all();
        if let Some(win) = self.win() {
            if let Err(e) = win.hide() {
                log::warn!("could not hide the window: {e}");
            }
        }
        let spawned = std::thread::Builder::new().name("quit".into()).spawn(move || {
            wait_engines(dones);
            let queued = slint::invoke_from_event_loop(|| {
                // Remove the tray icon before the process exits.
                with_app(|a| {
                    let desktop = a.desktop.borrow_mut().take();
                    drop(desktop);
                });
                if let Err(e) = slint::quit_event_loop() {
                    log::error!("could not stop the event loop: {e}");
                }
            });
            if let Err(e) = queued {
                log::error!("could not deliver quit to the UI: {e}");
            }
        });
        if let Err(e) = spawned {
            log::error!("could not start the quit thread: {e}");
            if let Err(e) = slint::quit_event_loop() {
                log::error!("could not stop the event loop: {e}");
            }
        }
    }

    /// Closing the window without a tray: ask the engines to stop and wait
    /// (bounded, all together) for them to close their event channels.
    pub fn shutdown_and_wait(&self) {
        let dones = self.shutdown_all();
        wait_engines(dones);
    }

    fn poll_focus(&self) {
        let active = self.window_active();
        if active && !self.was_active.replace(true) {
            self.mark_open_chat_read();
            self.subscribe_open_presence();
        } else if !active {
            self.was_active.set(false);
        }
    }

    /// Sends the read receipt for the active profile's open chat if it has
    /// unread messages. Chats of other profiles never get receipts.
    fn mark_open_chat_read(&self) {
        let p = self.p();
        let jid = {
            let st = p.st.borrow();
            let Some(sel) = st.selected.as_deref() else { return };
            unread_chat(&st.chats, sel).map(|r| r.jid.clone())
        };
        if let Some(chat_jid) = jid {
            (p.send)(Command::MarkChatRead { chat_jid });
        }
    }

    fn update_unread(&self, p: &ProfileState, rows: &[ChatRow]) {
        let total = unread_total(rows);
        if p.unread.replace(total) != total {
            self.update_total_badge();
            self.refresh_rail();
        }
    }

    /// Recounts a profile's unread messages from its store.
    fn refresh_unread(&self, p: &ProfileState) {
        match p.store.list_chats(false) {
            Ok(rows) => self.update_unread(p, &rows),
            Err(e) => log::warn!("unread count of {} failed: {e}", p.id),
        }
    }

    /// Tray badge and window title show the unread total of all profiles.
    fn update_total_badge(&self) {
        let total = self
            .profs
            .borrow()
            .iter()
            .fold(0u32, |sum, p| sum.saturating_add(p.unread.get()));
        if self.total_unread.get() == Some(total) {
            return;
        }
        self.total_unread.set(Some(total));
        if let Some(d) = self.desktop.borrow().as_ref() {
            d.set_unread_badge(total);
        }
        self.refresh_title();
    }

    /// "WaClient — <active profile>", plus the unread total when above zero.
    fn refresh_title(&self) {
        let Some(win) = self.win() else { return };
        let name = self.active.borrow().meta.borrow().name.clone();
        let total = self.total_unread.get().unwrap_or(0);
        let title = if total > 0 {
            format!("WaClient — {name} ({total})")
        } else {
            format!("WaClient — {name}")
        };
        win.set_window_title(title.into());
    }

    // ---- QR retry --------------------------------------------------------

    fn offer_qr_retry(&self, id: &str) {
        let Some(p) = self.find(id) else { return };
        if p.login.borrow().logged_in {
            return;
        }
        p.login.borrow_mut().show_retry = true;
        if self.is_active(&p) {
            self.render_login(&p);
        }
    }

    fn retry_qr(&self) {
        let p = self.p();
        (p.send)(Command::RetryPairing);
        {
            let mut l = p.login.borrow_mut();
            l.show_retry = false;
            l.qr = None;
            l.status = "Requesting a new QR code…".to_string();
        }
        self.render_login(&p);
        arm_qr_timer(&p);
    }

    // ---- reconnect -------------------------------------------------------

    fn reconnect(&self) {
        self.send(Command::Reconnect);
    }

    /// The strip is offered for `Disconnected` unless the engine gave up for good.
    fn update_reconnect(&self) {
        let Some(win) = self.win() else { return };
        let p = self.p();
        let st = p.st.borrow();
        match &st.conn {
            ConnState::Disconnected { .. } if !st.fatal => {
                win.set_reconnect_text(conn_text(&st.conn).into());
                win.set_show_reconnect(true);
            }
            _ => win.set_show_reconnect(false),
        }
    }

    // ---- screens -------------------------------------------------------

    fn enter_main(&self, p: &Rc<ProfileState>) {
        p.qr_timer.stop();
        {
            let mut l = p.login.borrow_mut();
            l.show_retry = false;
            l.logged_in = true;
        }
        if self.is_active(p) {
            self.render_login(p);
            self.refresh_chats();
            self.refresh_statuses();
        } else {
            self.refresh_unread(p);
        }
        self.refresh_rail();
    }

    fn leave_main(&self, p: &Rc<ProfileState>, status: &str) {
        let active = self.is_active(p);
        if active {
            self.attach_cancel();
            self.search_reset();
            self.stop_composing();
            self.cancel_record();
            if let Some(pl) = &self.player {
                pl.stop();
            }
        }
        if active {
            self.status_close();
        }
        {
            let mut st = p.st.borrow_mut();
            st.selected = None;
            st.sel_row = None;
            st.msgs.clear();
            st.chats.clear();
            st.reply = None;
            st.media.clear();
            st.decoding.clear();
            st.query.clear();
            st.composing_in = None;
        }
        {
            let mut l = p.login.borrow_mut();
            l.logged_in = false;
            l.qr = None;
            l.show_retry = false;
            l.status = status.to_string();
        }
        arm_qr_timer(p);
        p.unread.set(0);
        self.update_total_badge();
        if active {
            self.pool.set_open_chat(None);
            self.chats_model.set_vec(Vec::new());
            self.msgs_model.set_vec(Vec::new());
            if let Some(win) = self.win() {
                win.set_has_chat(false);
                win.set_has_reply(false);
                win.set_selected_jid(SharedString::default());
            }
            self.render_login(p);
            self.refresh_statuses();
        }
        self.refresh_rail();
    }

    /// Asks for our own profile picture once per connection, for the profile
    /// rail. Only a connected engine can fetch it: a request sent earlier
    /// (right after pairing) is dropped and would be marked as done.
    fn request_own_avatar(&self, p: &ProfileState) {
        let Some(jid) = p.meta.borrow().own_jid.clone() else { return };
        let fetch = {
            let mut st = p.st.borrow_mut();
            if st.conn != ConnState::Connected {
                return;
            }
            let has = st
                .avatar_paths
                .get(&jid)
                .and_then(Option::as_ref)
                .and_then(|path| st.avatar_imgs.get(path))
                .is_some_and(Option::is_some);
            !has && st.avatar_requested.insert(jid.clone())
        };
        if fetch {
            (p.send)(Command::FetchAvatar { jid });
        }
    }

    fn remember_own_jid(&self, p: &ProfileState, jid: &str) {
        p.meta.borrow_mut().own_jid = Some(jid.to_string());
        if self.registry.borrow_mut().set_own_jid(&p.id, jid) {
            self.save_registry();
        }
    }

    // ---- engine events ---------------------------------------------------

    /// Handles an event of profile `id` (only the active profile touches the window).
    pub fn on_event(&self, id: &str, ev: UiEvent) {
        let Some(p) = self.find(id) else { return };
        let active = self.is_active(&p);
        match ev {
            UiEvent::Qr { code } => match qr::render(&code) {
                Some(img) => {
                    {
                        let mut l = p.login.borrow_mut();
                        l.qr = Some(img);
                        l.show_retry = false;
                        l.logged_in = false;
                    }
                    arm_qr_timer(&p);
                    if active {
                        self.render_login(&p);
                    }
                }
                None => log::error!("could not render the pairing QR code"),
            },
            UiEvent::Paired { jid } => {
                self.remember_own_jid(&p, &jid);
                p.st.borrow_mut().fatal = false;
                self.enter_main(&p);
                // Usually still connecting; the Connected event asks then.
                self.request_own_avatar(&p);
            }
            UiEvent::SessionRestored => {
                p.st.borrow_mut().fatal = false;
                self.enter_main(&p);
                self.request_own_avatar(&p);
            }
            UiEvent::Connection(c) => {
                let connected = c == ConnState::Connected;
                p.login.borrow_mut().status = conn_text(&c);
                {
                    let mut st = p.st.borrow_mut();
                    if !matches!(c, ConnState::Disconnected { .. }) {
                        st.fatal = false;
                    }
                    st.conn = c;
                    if !connected {
                        // A failed or dropped fetch is retried on the next connection.
                        if let Some(jid) = p.meta.borrow().own_jid.as_ref() {
                            st.avatar_requested.remove(jid);
                        }
                    }
                }
                if active {
                    self.render_login(&p);
                    self.update_header();
                    self.update_reconnect();
                }
                self.account_connection(&p, connected);
                if !connected {
                    self.clear_presence(&p);
                }
                self.refresh_rail();
                if connected {
                    if active {
                        self.subscribe_open_presence();
                    }
                    self.request_own_avatar(&p);
                    if active {
                        self.prefetch_avatars();
                    }
                }
            }
            UiEvent::LoggedOut { reason } => {
                if p.removal.get() == Removal::LoggingOut {
                    // Unlinked: now stop the engine so its files can be deleted.
                    p.removal.set(Removal::Stopping);
                    (p.send)(Command::Shutdown);
                    self.arm_remove_watchdog(&p);
                    return;
                }
                let status = if reason.is_empty() {
                    "Logged out".to_string()
                } else {
                    format!("Logged out: {reason}")
                };
                self.leave_main(&p, &status);
            }
            UiEvent::ChatsChanged => {
                if active {
                    self.refresh_chats();
                } else {
                    self.refresh_unread(&p);
                }
            }
            // A running viewer keeps its own snapshot; only the list refreshes.
            UiEvent::ContactInfo(info) => {
                self.mentions_members(&p.id, &info);
                self.record_send_gate(&p.id, &info);
                self.on_contact_info(&p.id, active, info);
            }
            UiEvent::MessageInfo { chat_jid, id, receipts } => self.on_message_info(active, &chat_jid, &id, receipts),
            UiEvent::Blocklist { jids } => self.on_blocklist(&p, active, jids),
            UiEvent::OwnProfile(own) => self.on_own_profile(&p, active, own),
            UiEvent::PhoneLookup { results } => self.on_phone_lookup(&p.id, active, &results),
            UiEvent::DefaultTimer { secs } => {
                if active {
                    self.on_default_timer(secs);
                }
            }
            UiEvent::StatusPosted { ok, message } => self.on_status_posted(&p, ok, message),
            UiEvent::StatusesChanged => {
                if active {
                    self.refresh_statuses();
                }
            }
            UiEvent::MessagesChanged { chat_jid } => {
                if active {
                    self.starred_changed(&chat_jid);
                    self.on_messages_changed(&chat_jid);
                }
            }
            UiEvent::MessageStatus { chat_jid, id, status } => {
                if active {
                    self.on_status(&chat_jid, &id, status);
                }
            }
            UiEvent::Typing { chat_jid, composing, .. } => self.on_typing(&p, chat_jid, composing),
            UiEvent::Presence { jid, online, last_seen } => self.on_presence(&p, jid, online, last_seen),
            UiEvent::Notify { chat_jid, title, body } => self.on_notify(&p, &chat_jid, &title, &body),
            UiEvent::HistoryProgress { percent } => {
                let text = if percent < 100 {
                    format!("Syncing history… {percent}%")
                } else {
                    String::new()
                };
                if active {
                    if let Some(win) = self.win() {
                        win.set_sync_text(text.as_str().into());
                    }
                }
                *p.sync_text.borrow_mut() = text;
            }
            UiEvent::MediaReady { chat_jid, id, path } => {
                self.on_media_ready(&p, &chat_jid, &id, &path);
            }
            UiEvent::MediaFailed { chat_jid, id, error } => {
                self.on_media_failed(&p, &chat_jid, &id, &error);
            }
            UiEvent::AvatarReady { jid, .. } => {
                {
                    let mut st = p.st.borrow_mut();
                    if let Some(Some(old)) = st.avatar_paths.remove(&jid) {
                        st.avatar_imgs.remove(&old);
                    }
                }
                if active {
                    self.sync_chat_avatars(Some(&jid));
                    self.info_member_avatars(Some(&jid));
                    self.update_header();
                }
                self.refresh_rail();
            }
            UiEvent::Fatal { message } => {
                p.st.borrow_mut().fatal = true;
                *p.fatal_msg.borrow_mut() = Some(message.clone());
                if active {
                    self.update_reconnect();
                    self.show_banner(2, &message);
                } else {
                    self.show_banner(1, &format!("[{}] {message}", p.name()));
                }
            }
            // ---- GROUPS / INVITES (placeholder until the group UI lands) ----
            UiEvent::GroupMembersResult { group_jid, action, results } => {
                self.on_group_members_result(&p, active, &group_jid, action, &results);
            }
            UiEvent::InviteLink { group_jid, link } => self.on_invite_link(&p, active, &group_jid, &link),
            UiEvent::GroupCreated { group_jid, failed } => self.on_group_created(&p, active, &group_jid, &failed),
            UiEvent::InvitePreview(preview) => self.on_invite_preview(&p, active, preview),
            UiEvent::GroupJoined { group_jid, pending } => self.on_group_joined(&p, active, &group_jid, pending),
            UiEvent::GroupCreateFailed { message } => self.on_group_create_failed(&p, active, &message),
            UiEvent::GroupJoinFailed { link, message } => self.on_group_join_failed(&p, active, &link, &message),
            UiEvent::IncomingCall { call_id, chat_jid, caller_name, is_video, is_group } => {
                self.on_incoming_call(&p, call_id, chat_jid, caller_name, is_video, is_group);
            }
            UiEvent::CallEnded { call_id, reason } => self.on_call_ended(&p, &call_id, reason),
            UiEvent::CallDeclineFailed { call_id } => self.on_call_decline_failed(&p, &call_id),
            UiEvent::Error { message } => {
                self.account_error(&p, active, &message);
                self.invite_request_failed(&p);
                if message.contains("QR") && !p.login.borrow().logged_in {
                    p.login.borrow_mut().show_retry = true;
                    if active {
                        self.render_login(&p);
                    }
                }
                if active {
                    self.show_banner(1, &message);
                } else {
                    self.show_banner(1, &format!("[{}] {message}", p.name()));
                }
            }
        }
    }

    fn on_notify(&self, p: &ProfileState, chat_jid: &str, title: &str, body: &str) {
        let locked = self.is_locked();
        let looking = !locked
            && self.is_active(p)
            && p.st.borrow().selected.as_deref() == Some(chat_jid)
            && self.window_active();
        if looking {
            return;
        }
        let sound = p.settings.borrow().notification_sound;
        if locked {
            // Nothing about the chat, the sender or the text while locked.
            if let Some(d) = self.desktop.borrow().as_ref() {
                d.notify("WaClient", "New message", &p.id, chat_jid, sound);
            }
            return;
        }
        let title = if self.multi() { format!("[{}] {title}", p.name()) } else { title.to_string() };
        let (preview, sound) = {
            let s = p.settings.borrow();
            (s.notification_preview, s.notification_sound)
        };
        let body = if preview { body } else { "New message" };
        if let Some(d) = self.desktop.borrow().as_ref() {
            d.notify(&title, body, &p.id, chat_jid, sound);
        }
    }

    fn on_messages_changed(&self, chat_jid: &str) {
        if self.p().st.borrow().selected.as_deref() != Some(chat_jid) {
            return;
        }
        let Some(win) = self.win() else { return };
        if self.search_refresh_window(chat_jid) {
            self.windowed_news(chat_jid);
            return;
        }
        let stick = win.invoke_at_bottom() || self.p().st.borrow().force_bottom;
        let limit = self.p().st.borrow().limit.max(PAGE);
        let Some(page) = self.ok(
            "load messages",
            self.p().store.messages_page(chat_jid, None, limit as u32),
        ) else {
            return;
        };
        let last_incoming = page.last().is_some_and(|m| !m.from_me);
        {
            let p = self.p();
            let mut st = p.st.borrow_mut();
            st.exhausted = page.len() < limit;
            st.msgs = page;
            st.force_bottom = false;
            st.polls.clear();
        }
        self.search.leave_window();
        self.rebuild_rows();
        if stick {
            self.schedule_fix(LayoutFix::Bottom);
        }
        if last_incoming && self.window_active() {
            self.send(Command::MarkChatRead { chat_jid: chat_jid.to_string() });
        }
    }

    /// A live change while the view holds an old window: when the chat has
    /// messages past the window, return to the newest page if the user is at
    /// the window's end, otherwise offer the "New messages" pill.
    fn windowed_news(&self, chat_jid: &str) {
        let Some(win) = self.win() else { return };
        let p = self.p();
        let Some(newest) = self.ok("load messages", p.store.messages_page(chat_jid, None, 1)) else { return };
        let behind = {
            let st = p.st.borrow();
            match (newest.last(), st.msgs.last()) {
                (Some(n), Some(shown)) => n.id != shown.id,
                _ => false,
            }
        };
        if !behind {
            return;
        }
        if win.invoke_at_bottom() {
            self.jump_latest();
        } else {
            win.set_new_below(true);
        }
    }

    /// Leaves the window around a jump target and shows the newest messages.
    fn jump_latest(&self) {
        let p = self.p();
        let Some(jid) = p.st.borrow().selected.clone() else { return };
        let Some(page) = self.ok("load messages", p.store.messages_page(&jid, None, PAGE as u32)) else { return };
        {
            let mut st = p.st.borrow_mut();
            st.exhausted = page.len() < PAGE;
            st.limit = PAGE;
            st.msgs = page;
            st.polls.clear();
            st.force_bottom = false;
        }
        self.search.leave_window();
        self.rebuild_rows();
        self.schedule_fix(LayoutFix::Bottom);
    }

    fn on_status(&self, chat_jid: &str, id: &str, status: MessageStatus) {
        {
            let p = self.p();
            let mut st = p.st.borrow_mut();
            if st.selected.as_deref() != Some(chat_jid) {
                return;
            }
            match st.msgs.iter_mut().find(|m| m.id == id) {
                Some(m) => m.status = status,
                None => return,
            }
        }
        self.refresh_row(id);
    }

    fn on_typing(&self, p: &ProfileState, chat_jid: String, composing: bool) {
        let is_open = {
            let mut st = p.st.borrow_mut();
            if composing {
                st.typing.insert(chat_jid.clone(), Instant::now());
            } else {
                st.typing.remove(&chat_jid);
            }
            st.selected.as_deref() == Some(chat_jid.as_str())
        };
        if composing && !self.typing_tick.running() {
            self.typing_tick.start(TimerMode::Repeated, Duration::from_secs(1), || {
                with_app(App::tick_typing);
            });
        }
        if is_open && self.is_active(p) {
            self.update_header();
        }
    }

    /// Expires stale "typing…" entries of every profile.
    fn tick_typing(&self) {
        let profs = self.profs.borrow().clone();
        let mut expired_open = false;
        let mut empty = true;
        for p in &profs {
            let mut st = p.st.borrow_mut();
            let now = Instant::now();
            let before = st.typing.len();
            st.typing.retain(|_, t| now.duration_since(*t) < TYPING_TTL);
            let changed = st.typing.len() != before;
            let open_typing = st
                .selected
                .as_ref()
                .is_some_and(|s| st.typing.contains_key(s));
            if self.is_active(p) && changed && !open_typing {
                expired_open = true;
            }
            empty &= st.typing.is_empty();
        }
        if expired_open {
            self.update_header();
        }
        if empty {
            self.typing_tick.stop();
        }
    }

    fn on_media_ready(&self, p: &ProfileState, chat_jid: &str, id: &str, path: &Path) {
        if chat_jid == STATUS_JID {
            self.on_status_media_ready(p, id, path);
            return;
        }
        self.info_media_ready(&p.id, chat_jid, id, path);
        let mut st = p.st.borrow_mut();
        st.downloading.remove(id);
        st.failed.remove(id);
        let follow = st.after_download.remove(id);
        // Dropped below when the chat is no longer open: the save is cancelled.
        let save = st.save_after.remove(id);
        if st.selected.as_deref() != Some(chat_jid) {
            return;
        }
        let mut opened = None;
        let mut known = None;
        if let Some(m) = st.msgs.iter_mut().find(|m| m.id == id) {
            if m.local_path.is_none() {
                m.local_path = Some(path.to_string_lossy().into_owned());
            }
            known.clone_from(&m.local_path);
            opened = m.local_path.clone().map(|p| (m.kind, p));
        }
        if let Some(f) = known {
            st.seen_paths.insert(f);
        }
        drop(st);
        if !self.is_active(p) {
            return;
        }
        self.refresh_row(id);
        if save {
            self.save_as(id);
        }
        if let (true, Some((kind, local))) = (follow, opened) {
            match kind {
                MessageKind::Voice | MessageKind::Audio => self.play_path(id, &local),
                _ => self.open_path(&local),
            }
        }
    }

    fn on_media_failed(&self, p: &ProfileState, chat_jid: &str, id: &str, error: &str) {
        if chat_jid == STATUS_JID {
            self.on_status_media_failed(p, id, error);
            return;
        }
        self.info_media_failed(&p.id, chat_jid, id);
        {
            let mut st = p.st.borrow_mut();
            st.downloading.remove(id);
            st.after_download.remove(id);
            st.save_after.remove(id);
            st.failed.insert(id.to_string());
        }
        if !self.is_active(p) {
            log::warn!("download failed in profile {}: {error}", p.id);
            return;
        }
        if p.st.borrow().selected.as_deref() == Some(chat_jid) {
            self.refresh_row(id);
        }
        self.show_banner(1, &format!("Download failed: {error}"));
    }

    // ---- chat list -------------------------------------------------------

    fn search(&self, q: &str) {
        self.p().st.borrow_mut().query = q.trim().to_string();
        self.refresh_chats();
    }

    fn refresh_chats(&self) {
        let p = self.p();
        let query = p.st.borrow().query.clone();
        let Some(full) = self.ok("load chats", p.store.list_chats(true)) else { return };
        let has_archived = full.iter().any(|r| r.archived);
        self.folders_sync(&p, &full);
        let folders = self.folders_view(&p);
        let archived_unread = full.iter().filter(|r| r.archived && r.unread > 0).count();
        let win = self.win();
        // The archive view stays open when it is empty (it shows an empty state).
        let archived_mode = win.as_ref().is_some_and(|w| w.get_archived_mode());
        if let Some(w) = &win {
            w.set_archived_mode(archived_mode);
            w.set_has_archived(has_archived);
            w.set_archived_unread(i32::try_from(archived_unread).unwrap_or(i32::MAX));
        }
        // Search covers the archive too (its hits carry an "Archived" tag).
        let rows = if query.is_empty() {
            Some(
                full.iter()
                    .filter(|r| r.archived == archived_mode && (archived_mode || folders.shows(r)))
                    .cloned()
                    .collect::<Vec<ChatRow>>(),
            )
        } else {
            self.ok("load chats", p.store.search_chats(&query))
        };
        let Some(rows) = rows else { return };
        let now = Local::now();
        let mut items = Vec::with_capacity(rows.len());
        {
            let mut st = p.st.borrow_mut();
            let blocked = self.blocked_matcher(&p);
            for r in &rows {
                let avatar = self.avatar(&p, &mut st, &r.jid);
                let mut item = models::chat_item(r, avatar, now);
                item.blocked = item.is_dm && blocked(&r.jid);
                if let Some(draft) = self.draft_preview(&r.jid) {
                    item.preview = draft.into();
                    item.draft = true;
                }
                items.push(item);
            }
            if let Some(sel) = st.selected.clone() {
                if let Some(r) = find_chat(&full, &sel) {
                    st.sel_row = Some(r.clone());
                }
            }
        }
        // Archived and muted chats never count (see `unread_total`).
        self.update_unread(&p, &full);
        p.st.borrow_mut().chats = full;
        self.chats_model.set_vec(items);
        self.update_header();
        self.prefetch_avatars();
    }

    /// Returns the decoded avatar if ready; otherwise queues the decode on the
    /// worker pool and returns `None` (the list refreshes when it finishes).
    fn avatar(&self, p: &ProfileState, st: &mut State, jid: &str) -> Option<Image> {
        if !st.avatar_paths.contains_key(jid) {
            let path = match p.store.contact(jid) {
                Ok(c) => c.and_then(|c| c.avatar_path),
                Err(e) => {
                    log::warn!("contact lookup for {} failed: {e}", models::redact_jid(jid));
                    None
                }
            };
            st.avatar_paths.insert(jid.to_string(), path);
        }
        let path = st.avatar_paths.get(jid).and_then(Option::as_ref)?.clone();
        if let Some(img) = st.avatar_imgs.get(&path) {
            return img.clone();
        }
        if st.avatar_pending.insert(path.clone()) {
            let queued = self.pool.submit(DecodeJob::Avatar {
                path: PathBuf::from(&path),
                max_dim: AVATAR_DIM,
            });
            if !queued {
                st.avatar_pending.remove(&path);
                st.avatar_imgs.insert(path, None);
            }
        }
        None
    }

    // ---- chat view ---------------------------------------------------------

    /// Deselects the open chat of the active profile (Esc, profile switch) and
    /// shows the empty state. Everything scoped to the chat ends with it.
    fn close_chat(&self) {
        self.ops_chat_leaving(false);
        self.draft_touch();
        self.close_mentions();
        self.poll_chat_changed();
        self.info_close();
        self.attach_cancel();
        self.search_reset();
        self.stop_composing();
        self.cancel_record();
        if let Some(pl) = &self.player {
            pl.stop();
        }
        {
            let p = self.p();
            let mut st = p.st.borrow_mut();
            st.selected = None;
            st.sel_row = None;
            st.msgs.clear();
            st.reply = None;
            st.media.clear();
            st.decoding.clear();
            st.polls.clear();
            st.reactions.clear();
            st.play = None;
            st.save_after.clear();
            st.force_bottom = false;
        }
        self.pool.set_open_chat(None);
        self.msgs_model.set_vec(Vec::new());
        if let Some(win) = self.win() {
            win.set_has_chat(false);
            win.set_has_reply(false);
            win.set_selected_jid(SharedString::default());
            win.invoke_clear_composer();
        }
    }

    fn select_chat(&self, jid: &str) {
        if self.p().st.borrow().selected.as_deref() == Some(jid) {
            return;
        }
        self.ops_chat_leaving(true);
        self.draft_touch();
        self.poll_chat_changed();
        self.info_close();
        self.attach_cancel();
        self.search_reset();
        self.stop_composing();
        // A recording belongs to the chat it was started in.
        self.cancel_record();
        let Some(page) = self.ok("load messages", self.p().store.messages_page(jid, None, PAGE as u32))
        else {
            return;
        };
        let unread = {
            let p = self.p();
            let mut st = p.st.borrow_mut();
            // Chats without messages are not in the list yet (a new chat started from a shared contact).
            let row = find_chat(&st.chats, jid)
                .cloned()
                .or_else(|| self.p().store.chat(jid).ok().flatten());
            let unread = row.as_ref().map_or(0, |r| r.unread);
            st.selected = Some(jid.to_string());
            st.sel_row = row;
            st.exhausted = page.len() < PAGE;
            st.limit = PAGE;
            st.msgs = page;
            st.reply = None;
            st.media.clear();
            st.decoding.clear();
            st.polls.clear();
            st.save_after.clear();
            st.force_bottom = false;
            unread
        };
        self.pool.set_open_chat(Some(jid));
        if let Some(win) = self.win() {
            win.set_selected_jid(jid.into());
            win.set_has_chat(true);
            win.set_has_reply(false);
            win.invoke_focus_composer();
        }
        self.draft_restore(jid);
        self.rebuild_rows();
        self.update_header();
        self.subscribe_presence(jid);
        self.ensure_send_gate(jid);
        self.schedule_fix(LayoutFix::Bottom);
        if unread > 0 && self.window_active() {
            self.send(Command::MarkChatRead { chat_jid: jid.to_string() });
        }
        let fetch_avatar = {
            let p = self.p();
            let mut st = p.st.borrow_mut();
            let has = st
                .avatar_paths
                .get(jid)
                .and_then(Option::as_ref)
                .and_then(|p| st.avatar_imgs.get(p))
                .is_some_and(Option::is_some);
            // Offline requests are dropped by the engine; ask again after reconnecting.
            st.conn == ConnState::Connected && !has && st.avatar_requested.insert(jid.to_string())
        };
        if fetch_avatar {
            self.send(Command::FetchAvatar { jid: jid.to_string() });
        }
    }

    fn update_header(&self) {
        let Some(win) = self.win() else { return };
        let p = self.p();
        let mut st = p.st.borrow_mut();
        let Some(row) = st.sel_row.clone() else {
            return;
        };
        let avatar = self.avatar(&p, &mut st, &row.jid);
        let typing = st
            .typing
            .get(&row.jid)
            .is_some_and(|t| t.elapsed() < TYPING_TTL);
        let subtitle = if typing {
            "typing…".to_string()
        } else if st.conn == ConnState::Connected {
            privacy::presence_subtitle(st.presence.get(&row.jid), Local::now())
        } else {
            conn_text(&st.conn)
        };
        drop(st);
        win.set_header_name(row.display_name.as_str().into());
        win.set_header_initials(models::initials(&row.display_name).into());
        win.set_header_tint(models::tint_for(&row.jid));
        win.set_header_has_avatar(avatar.is_some());
        win.set_header_avatar(avatar.unwrap_or_default());
        win.set_header_subtitle(subtitle.into());
        win.set_header_receipts_off(row.receipts_off);
        self.timer_sync(row.ephemeral_secs);
        win.set_chat_blocked(self.is_blocked_dm(&row.jid));
        win.set_composer_blocked_text(self.gate_notice(&p.id, &row.jid).into());
    }

    /// Per page load: forgets files deleted from disk (each file is checked
    /// once), and looks up the missing poll tallies and the reactions of the
    /// loaded messages.
    fn prepare_rows(&self) {
        let p = self.p();
        let mut st = p.st.borrow_mut();
        let st = &mut *st;
        let Some(jid) = st.selected.clone() else { return };
        for m in &mut st.msgs {
            let Some(f) = m.local_path.as_deref() else { continue };
            if st.seen_paths.contains(f) {
                continue;
            }
            if Path::new(f).exists() {
                st.seen_paths.insert(f.to_string());
            } else {
                m.local_path = None;
            }
        }
        // Poll tallies are cached per message; MessagesChanged and switching
        // chats clear the cache.
        let missing: Vec<String> = st
            .msgs
            .iter()
            .filter(|m| models::is_poll_row(m) && !st.polls.contains_key(&m.id))
            .map(|m| m.id.clone())
            .collect();
        for id in missing {
            match p.store.poll_for(&jid, &id) {
                Ok(Some(view)) => {
                    st.polls.insert(id, poll_view_model(&view));
                }
                Ok(None) => {}
                Err(e) => log::warn!("poll lookup for {id} failed: {e}"),
            }
        }
        let ids: Vec<String> = st.msgs.iter().map(|m| m.id.clone()).collect();
        st.reactions = match p.store.reactions_for(&jid, &ids) {
            Ok(map) => map
                .into_iter()
                .map(|(id, list)| (id, crate::emoji::aggregate_label(&list)))
                .collect(),
            Err(e) => {
                log::warn!("reaction lookup failed: {e}");
                HashMap::new()
            }
        };
    }

    fn rebuild_rows(&self) {
        if let Some(win) = self.win() {
            win.set_new_below(false);
        }
        self.prepare_rows();
        let now = Local::now();
        let mut items = Vec::new();
        {
            let p = self.p();
            let mut st = p.st.borrow_mut();
            let st = &mut *st;
            let is_group = st
                .selected
                .as_deref()
                .is_some_and(|j| ChatKind::from_jid(j) == ChatKind::Group);
            let mut prev_ts = None;
            for m in &st.msgs {
                if models::new_day(prev_ts, m.ts) {
                    items.push(models::separator_item(m.ts, now));
                }
                prev_ts = Some(m.ts);
                let images = images_for(&mut st.media, &mut st.decoding, &self.pool, m);
                let ui = RowUi {
                    downloading: st.downloading.contains(&m.id),
                    failed: st.failed.contains(&m.id),
                    play: st.play.as_ref(),
                    poll: st.polls.get(&m.id),
                    reactions: st.reactions.get(&m.id).map_or(&[], Vec::as_slice),
                };
                items.push(models::msg_item(m, is_group, images, ui));
            }
            st.guard_until = Instant::now() + LAYOUT_GUARD;
        }
        self.ops_decorate(&mut items);
        self.msgs_model.set_vec(items);
    }

    /// Re-renders one message row in place (status, download state...).
    fn refresh_row(&self, id: &str) {
        let Some(idx) = self
            .msgs_model
            .iter()
            .position(|m| !m.is_sep && m.id.as_str() == id)
        else {
            return;
        };
        let item = {
            let p = self.p();
            let mut st = p.st.borrow_mut();
            let st = &mut *st;
            let is_group = st
                .selected
                .as_deref()
                .is_some_and(|j| ChatKind::from_jid(j) == ChatKind::Group);
            st.msgs.iter().find(|m| m.id == id).map(|m| {
                let images = images_for(&mut st.media, &mut st.decoding, &self.pool, m);
                let ui = RowUi {
                    downloading: st.downloading.contains(id),
                    failed: st.failed.contains(id),
                    play: st.play.as_ref(),
                    poll: st.polls.get(id),
                    reactions: st.reactions.get(id).map_or(&[], Vec::as_slice),
                };
                models::msg_item(m, is_group, images, ui)
            })
        };
        if let Some(mut item) = item {
            self.ops_decorate_one(&mut item);
            self.msgs_model.set_row_data(idx, item);
        }
    }

    /// Loads the previous page. `fill` is used when the content is shorter
    /// than the viewport (no scrolling possible) and bypasses the layout guard.
    fn load_older(&self, fill: bool) {
        let Some(win) = self.win() else { return };
        let (jid, before) = {
            let p = self.p();
            let st = p.st.borrow();
            if st.exhausted || (!fill && Instant::now() < st.guard_until) {
                return;
            }
            let (Some(jid), Some(first)) = (st.selected.clone(), st.msgs.first()) else {
                return;
            };
            if let Some((chat, until)) = &st.older_backoff {
                if *chat == jid && Instant::now() < *until {
                    return;
                }
            }
            (jid, (first.ts, first.id.clone()))
        };
        let page = self.p().store.messages_page_before(&jid, Some(before), PAGE as u32);
        let mut page = match page {
            Ok(p) => p,
            Err(e) => {
                log::error!("load older messages: {e}");
                // Backing off also rate-limits the banner: it shows once per window.
                self.p().st.borrow_mut().older_backoff = Some((jid, Instant::now() + OLDER_BACKOFF));
                self.show_banner(1, &format!("load older messages: {e}"));
                return;
            }
        };
        let y = win.invoke_scroll_y();
        let h = win.invoke_content_h();
        {
            let p = self.p();
            let mut st = p.st.borrow_mut();
            if st.selected.as_deref() != Some(jid.as_str()) {
                return;
            }
            if page.len() < PAGE {
                st.exhausted = true;
            }
            if page.is_empty() {
                return;
            }
            st.limit += page.len();
            page.append(&mut st.msgs);
            st.msgs = page;
        }
        self.rebuild_rows();
        self.schedule_fix(if fill { LayoutFix::Bottom } else { LayoutFix::Restore { y, h } });
    }

    /// The ListView only knows real row heights after layout (a tall last row,
    /// such as a poll, settles late), so the correction is re-applied on every
    /// layout change until the content height has been the same for
    /// [`FIX_STABLE_CHECKS`] checks in a row, for at most [`FIX_MAX`]. It stops
    /// at once when the user scrolls.
    fn schedule_fix(&self, fix: LayoutFix) {
        *self.fix_run.borrow_mut() =
            Some(FixRun { fix, started: Instant::now(), last_h: None, set_y: None, stable: 0 });
        self.fix_timer.start(TimerMode::Repeated, FIX_TICK, || with_app(App::fix_tick));
    }

    fn fix_tick(&self) {
        let Some(win) = self.win() else {
            self.finish_fix(false);
            return;
        };
        let (y, h) = (win.invoke_scroll_y(), win.invoke_content_h());
        let done = {
            let mut guard = self.fix_run.borrow_mut();
            let Some(run) = guard.as_mut() else {
                drop(guard);
                self.finish_fix(false);
                return;
            };
            let unchanged = run.last_h.is_some_and(|lh| (h - lh).abs() <= FIX_EPS);
            let user_scrolled = unchanged && run.set_y.is_some_and(|sy| (y - sy).abs() > FIX_EPS);
            if user_scrolled {
                Some(false)
            } else {
                run.stable = if unchanged { run.stable + 1 } else { 0 };
                match run.fix {
                    LayoutFix::Bottom => win.invoke_scroll_to_bottom(),
                    LayoutFix::Restore { y, h } => win.invoke_restore_scroll(y, h),
                }
                run.set_y = Some(win.invoke_scroll_y());
                run.last_h = Some(h);
                (run.stable >= FIX_STABLE_CHECKS || run.started.elapsed() >= FIX_MAX).then_some(true)
            }
        };
        if let Some(applied) = done {
            self.finish_fix(applied);
        }
    }

    fn finish_fix(&self, fill: bool) {
        self.fix_timer.stop();
        *self.fix_run.borrow_mut() = None;
        if fill {
            self.fill_if_short();
        }
    }

    fn fill_if_short(&self) {
        if self.win().is_some_and(|w| w.invoke_content_short()) {
            self.load_older(true);
        }
    }

    // ---- composer ------------------------------------------------------------

    fn send_text(&self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        if self.chat_is_blocked_dm() {
            self.show_toast("Unblock this contact to send messages");
            return;
        }
        if self.refuse_gated() {
            return;
        }
        if self.ops_edit_send(text) {
            return;
        }
        let (jid, quoted) = {
            let p = self.p();
            let mut st = p.st.borrow_mut();
            let Some(jid) = st.selected.clone() else { return };
            st.force_bottom = true;
            (jid, st.reply.take().map(|r| r.0))
        };
        let mentions = self.mentions_for_send(&jid, text);
        self.send(Command::SendText { chat_jid: jid.clone(), text: text.to_string(), quoted_id: quoted, mentions });
        self.stop_composing();
        self.close_mentions();
        // The composer is emptied by the view once this returns; the draft must not outlive the send.
        if let Some(win) = self.win() {
            win.invoke_clear_composer();
        }
        self.draft_sent(&jid);
        self.sync_reply();
    }

    fn on_edit(&self, text: &str) {
        let Some(jid) = self.p().st.borrow().selected.clone() else { return };
        self.draft_touch();
        if text.is_empty() {
            self.stop_composing();
            return;
        }
        let send_now = {
            let p = self.p();
            let mut st = p.st.borrow_mut();
            let due = st
                .last_typing_sent
                .is_none_or(|t| t.elapsed() >= TYPING_RESEND);
            if due {
                st.last_typing_sent = Some(Instant::now());
                st.composing_in = Some(jid.clone());
            }
            due
        };
        if send_now {
            self.send(Command::SetTyping { chat_jid: jid, composing: true });
        }
        self.compose_timer.start(TimerMode::SingleShot, TYPING_PAUSE, || {
            with_app(App::stop_composing);
        });
    }

    fn stop_composing(&self) {
        self.compose_timer.stop();
        let chat = {
            let p = self.p();
            let mut st = p.st.borrow_mut();
            st.last_typing_sent = None;
            st.composing_in.take()
        };
        if let Some(chat_jid) = chat {
            self.send(Command::SetTyping { chat_jid, composing: false });
        }
    }

    fn attach(&self) {
        let Some(jid) = self.p().st.borrow().selected.clone() else { return };
        // The native dialog blocks, so it runs on its own thread.
        let spawned = std::thread::Builder::new().name("file-dialog".into()).spawn(move || {
            let Some(path) = rfd::FileDialog::new().set_title("Send file").pick_file() else {
                return;
            };
            let queued = slint::invoke_from_event_loop(move || {
                with_app(|a| a.send_media(jid, path));
            });
            if let Err(e) = queued {
                log::error!("could not deliver the picked file to the UI: {e}");
            }
        });
        if let Err(e) = spawned {
            self.show_banner(1, &format!("Could not open the file dialog: {e}"));
        }
    }

    fn send_media(&self, chat_jid: String, path: PathBuf) {
        let kind = media_kind_for(&path);
        self.send_media_as(chat_jid, path, kind);
    }

    fn send_media_as(&self, chat_jid: String, path: PathBuf, kind: OutgoingMediaKind) {
        if self.refuse_blocked(&chat_jid) {
            return;
        }
        let quoted = {
            let p = self.p();
            let mut st = p.st.borrow_mut();
            if st.selected.as_deref() == Some(chat_jid.as_str()) {
                st.force_bottom = true;
                st.reply.take().map(|r| r.0)
            } else {
                None
            }
        };
        self.send(Command::SendMedia { chat_jid, path, kind, caption: None, quoted_id: quoted, view_once: false });
        self.sync_reply();
    }

    fn reply_to(&self, id: &str) {
        {
            let p = self.p();
            let mut st = p.st.borrow_mut();
            let Some(m) = st.msgs.iter().find(|m| m.id == id && !m.revoked) else { return };
            let sender = if m.from_me {
                "You".to_string()
            } else {
                m.sender_name
                    .clone()
                    .unwrap_or_else(|| m.sender_jid.split('@').next().unwrap_or("").to_string())
            };
            let reply = (m.id.clone(), sender, models::reply_preview(m));
            st.reply = Some(reply);
        }
        self.sync_reply();
        if let Some(win) = self.win() {
            win.invoke_focus_composer();
        }
    }

    fn cancel_reply(&self) {
        self.p().st.borrow_mut().reply = None;
        self.sync_reply();
    }

    fn sync_reply(&self) {
        let Some(win) = self.win() else { return };
        match &self.p().st.borrow().reply {
            Some((_, sender, text)) => {
                win.set_reply_sender(sender.as_str().into());
                win.set_reply_text(text.as_str().into());
                win.set_has_reply(true);
            }
            None => win.set_has_reply(false),
        }
        self.draft_touch();
    }

    fn download(&self, id: &str) {
        let Some(jid) = self.p().st.borrow().selected.clone() else { return };
        {
            let p = self.p();
            let mut st = p.st.borrow_mut();
            if !st.downloading.insert(id.to_string()) {
                return;
            }
            st.failed.remove(id);
        }
        self.send(Command::DownloadMedia { chat_jid: jid, id: id.to_string() });
        self.refresh_row(id);
    }

    // ---- opening files and voice playback ------------------------------------------

    /// Click on a video/document/image: open it, downloading first if needed.
    fn open_item(&self, id: &str) {
        let local = {
            let p = self.p();
            let st = p.st.borrow();
            match st.msgs.iter().find(|m| m.id == id) {
                Some(m) => m.local_path.clone(),
                None => return,
            }
        };
        match local {
            Some(p) => self.open_path(&p),
            None => {
                self.p().st.borrow_mut().after_download.insert(id.to_string());
                self.download(id);
            }
        }
    }

    fn open_path(&self, path: &str) {
        if models::open_policy(path) == OpenPolicy::RevealOnly {
            self.show_banner(1, "This file type is not opened from chat. Use \"Show in folder\".");
            return;
        }
        self.launch_checked(path, false);
    }

    fn show_in_folder(&self, id: &str) {
        let local = self
            .p()
            .st
            .borrow()
            .msgs
            .iter()
            .find(|m| m.id == id)
            .and_then(|m| m.local_path.clone());
        if let Some(p) = local {
            self.launch_checked(&p, true);
        }
    }

    /// The media folders of every profile.
    fn media_roots(&self) -> Vec<PathBuf> {
        self.registry
            .borrow()
            .profiles
            .iter()
            .flat_map(|m| {
                let media = profiles::data_dir(&self.root, m).join("media");
                [media.join("outgoing"), media]
            })
            .collect()
    }

    /// Only files inside the app's media folders are ever handed to the OS.
    fn launch_checked(&self, path: &str, reveal: bool) {
        match validate_launch_path(path, &self.media_roots()) {
            // A link inside the folder may point at a file with another extension.
            Ok(real) if !reveal && models::open_policy(&real.to_string_lossy()) == OpenPolicy::RevealOnly => {
                self.show_banner(1, "This file type is not opened from chat. Use \"Show in folder\".");
            }
            Ok(real) => run_launcher(real, reveal),
            Err(e) => {
                log::warn!("refused to open {path}: {e}");
                self.show_banner(1, "That file is outside the app's media folder and was not opened");
            }
        }
    }

    fn toggle_play(&self, id: &str) {
        let (local, is_playing) = {
            let p = self.p();
            let st = p.st.borrow();
            let Some(m) = st.msgs.iter().find(|m| m.id == id) else { return };
            let is_playing = st
                .play
                .as_ref()
                .is_some_and(|p| p.id == id && p.state == PlayState::Playing);
            (m.local_path.clone(), is_playing)
        };
        match local {
            None => {
                self.p().st.borrow_mut().after_download.insert(id.to_string());
                self.download(id);
            }
            Some(_) if is_playing => {
                if let Some(p) = &self.player {
                    p.pause();
                }
            }
            Some(path) => self.play_path(id, &path),
        }
    }

    fn play_path(&self, id: &str, path: &str) {
        let is_opus = Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| ["ogg", "opus", "oga"].iter().any(|o| e.eq_ignore_ascii_case(o)));
        if !is_opus {
            // Only Ogg/Opus can be decoded in-app; hand anything else to the OS.
            self.open_path(path);
            return;
        }
        match &self.player {
            Some(p) => p.play(PathBuf::from(path), id.to_string()),
            None => self.show_banner(1, "Audio playback is unavailable"),
        }
    }

    fn on_player(&self, ev: PlayerEvent) {
        let (touched, failure) = {
            let p = self.p();
            let mut st = p.st.borrow_mut();
            let old = st.play.as_ref().map(|p| p.id.clone());
            let (new, failure) = match ev {
                PlayerEvent::State { id, state, position, duration } => {
                    st.play = (state != PlayState::Stopped).then(|| PlayView {
                        id: id.clone(),
                        state,
                        pos: position,
                        dur: duration,
                    });
                    (id, None)
                }
                PlayerEvent::Failed { id, error } => {
                    st.play = None;
                    (id, Some(error))
                }
            };
            ([old, Some(new)], failure)
        };
        let [old, new] = touched;
        if let Some(old) = old.filter(|o| Some(o) != new.as_ref()) {
            self.update_play(&old);
        }
        if let Some(new) = new {
            self.update_play(&new);
        }
        if let Some(e) = failure {
            self.show_banner(1, &format!("Cannot play voice message: {e}"));
        }
    }

    /// Updates only the playback fields of one row (runs ~10x/s while playing).
    fn update_play(&self, id: &str) {
        let Some(idx) = self
            .msgs_model
            .iter()
            .position(|m| !m.is_sep && m.id.as_str() == id)
        else {
            return;
        };
        let Some(mut item) = self.msgs_model.row_data(idx) else { return };
        let (state, progress, text) = {
            let p = self.p();
            let st = p.st.borrow();
            match st.msgs.iter().find(|m| m.id == id) {
                Some(m) => models::voice_view(m, st.play.as_ref()),
                None => return,
            }
        };
        if item.play_state == state && item.play_text.as_str() == text && item.play_progress == progress {
            return;
        }
        item.play_state = state;
        item.play_progress = progress;
        item.play_text = text.into();
        self.msgs_model.set_row_data(idx, item);
    }

    fn on_decoded(&self, done: DecodeDone) {
        match done {
            DecodeDone::Full { chat_jid, key, msg_id, buf } => {
                {
                    let p = self.p();
                    let mut st = p.st.borrow_mut();
                    // The user moved on while this was decoding.
                    if st.selected.as_deref() != Some(chat_jid.as_str()) {
                        return;
                    }
                    // Not queued by the open chat of this profile (another
                    // profile's job for a chat with the same jid, or a render
                    // that was reset since).
                    if !st.decoding.remove(&key) {
                        return;
                    }
                    st.media.insert(key, buf.map(Image::from_rgba8));
                }
                self.refresh_row(&msg_id);
            }
            DecodeDone::Media { path, buf } => self.on_status_decoded(&path, buf),
            DecodeDone::Avatar { path, buf } => {
                let img = buf.map(Image::from_rgba8);
                // Paths are unique per profile, so at most one profile asked.
                for p in self.profs.borrow().iter() {
                    let mut st = p.st.borrow_mut();
                    if st.avatar_pending.remove(&path) {
                        st.avatar_imgs.insert(path.clone(), img.clone());
                    }
                }
                self.avatar_timer.start(TimerMode::SingleShot, AVATAR_REFRESH, || {
                    with_app(|a| {
                        a.sync_chat_avatars(None);
                        a.info_member_avatars(None);
                        a.update_header();
                        a.refresh_rail();
                    });
                });
            }
        }
    }

    // ---- voice recording ---------------------------------------------------------------

    fn start_record(&self) {
        if self.recorder.borrow().is_some() || self.p().st.borrow().selected.is_none() {
            return;
        }
        if self.chat_is_blocked_dm() {
            self.show_toast("Unblock this contact to send messages");
            return;
        }
        if self.refuse_gated() {
            return;
        }
        if let Some(p) = &self.player {
            p.stop();
        }
        let dir = {
            let p = self.p();
            let meta = p.meta.borrow();
            profiles::data_dir(&self.root, &meta).join("media").join("outgoing")
        };
        let Some(rec) = self.ok("start recording", Recorder::start(&dir)) else { return };
        *self.recorder.borrow_mut() = Some(rec);
        if let Some(win) = self.win() {
            win.set_rec_time("0:00".into());
            win.set_rec_level(0.0);
            win.set_recording(true);
        }
        self.rec_timer.start(TimerMode::Repeated, REC_TICK, || with_app(App::tick_record));
    }

    fn tick_record(&self) {
        let (error, elapsed, level, ended_early) = match self.recorder.borrow().as_ref() {
            Some(r) => (r.take_error(), r.elapsed_secs(), r.level(), r.ended_early()),
            None => {
                self.rec_timer.stop();
                return;
            }
        };
        if let Some(e) = error {
            self.cancel_record();
            self.show_banner(1, &format!("Voice recording failed: {e}"));
            return;
        }
        if let Some(win) = self.win() {
            win.set_rec_time(models::fmt_secs(elapsed.floor()).into());
            win.set_rec_level(level);
        }
        if ended_early {
            // Capture is over and the file is kept; the bar offers Send / Cancel.
            self.rec_timer.stop();
            if let Some(win) = self.win() {
                win.set_rec_time(models::fmt_secs(elapsed.floor()).into());
                win.set_rec_level(0.0);
                win.set_rec_stopped(true);
            }
            self.show_banner(1, "Microphone stopped — review your voice message");
            return;
        }
        if elapsed >= MAX_VOICE_SECS {
            self.send_record();
            self.show_banner(1, "Maximum voice message length reached; sending it now");
        }
    }

    /// Esc after the microphone stopped only dismisses the banner; the
    /// recording stays for Send / Cancel.
    fn escape_stopped_recording(&self) -> bool {
        let stopped = self.recorder.borrow().is_some() && self.win().is_some_and(|w| w.get_rec_stopped());
        if stopped {
            self.hide_banner();
        }
        stopped
    }

    fn end_record_ui(&self) {
        self.rec_timer.stop();
        if let Some(win) = self.win() {
            win.set_recording(false);
            win.set_rec_stopped(false);
            win.set_rec_level(0.0);
        }
    }

    fn cancel_record(&self) {
        let rec = self.recorder.borrow_mut().take();
        if let Some(rec) = rec {
            rec.cancel();
            self.end_record_ui();
        }
    }

    fn send_record(&self) {
        let rec = self.recorder.borrow_mut().take();
        let Some(rec) = rec else { return };
        self.end_record_ui();
        let Some(jid) = self.p().st.borrow().selected.clone() else {
            rec.cancel();
            return;
        };
        // Finishing the file blocks on the encoder thread, so do it off the UI thread.
        let spawned = std::thread::Builder::new().name("voice-finish".into()).spawn(move || {
            let result = rec.stop().map_err(|e| format!("{e:#}"));
            let queued = slint::invoke_from_event_loop(move || {
                with_app(|a| a.finish_record(jid, result));
            });
            if let Err(e) = queued {
                log::error!("could not deliver the finished recording to the UI: {e}");
            }
        });
        if let Err(e) = spawned {
            self.show_banner(1, &format!("Could not finish the recording: {e}"));
        }
    }

    fn finish_record(&self, jid: String, result: Result<(PathBuf, f32), String>) {
        match result {
            Err(e) => self.show_banner(1, &format!("Voice recording failed: {e}")),
            Ok((path, secs)) if secs < MIN_VOICE_SECS => {
                if let Err(e) = std::fs::remove_file(&path) {
                    log::warn!("could not delete the short recording {}: {e}", path.display());
                }
                self.show_banner(1, "Recording too short");
            }
            Ok((path, _)) => self.send_media_as(jid, path, OutgoingMediaKind::Voice),
        }
    }

    // ---- banner ----------------------------------------------------------------

    fn show_banner(&self, kind: i32, text: &str) {
        let Some(win) = self.win() else { return };
        win.set_banner_text(text.into());
        win.set_banner_kind(kind);
        if kind == 1 {
            self.banner_timer.start(TimerMode::SingleShot, ERROR_BANNER_TTL, || {
                with_app(App::hide_banner);
            });
        } else {
            self.banner_timer.stop();
        }
    }

    fn hide_banner(&self) {
        self.banner_timer.stop();
        if let Some(win) = self.win() {
            win.set_banner_kind(0);
        }
    }
}

#[cfg(debug_assertions)]
impl App {
    /// Debug aid for the demo (`WACLIENT_DEMO_UI`): opens one of the new
    /// screens without any input.
    pub fn demo_open(&self, what: &str) {
        let Some(win) = self.win() else { return };
        match what {
            "settings" => {
                self.open_settings();
                win.set_show_settings(true);
            }
            "status" => {
                win.set_status_mode(true);
                self.refresh_statuses();
            }
            "viewer" => {
                win.set_status_mode(true);
                self.refresh_statuses();
                self.open_status(0);
            }
            "settings-save" => {
                self.open_settings();
                let mut m = win.get_settings();
                m.notification_sound = false;
                m.font_scale_index = 2;
                self.settings_changed(m);
                self.flush_settings();
                log::warn!("demo: settings saved to {}", self.profile_dir(&self.p()).display());
            }
            "chat" => {
                self.select_chat("111@s.whatsapp.net");
                Timer::single_shot(Duration::from_millis(1500), || {
                    with_app(|a| {
                        if let Some(w) = a.win() {
                            w.invoke_scroll_to_bottom();
                        }
                    });
                });
            }
            "group-announce" => self.demo_group_announce(),
            "call-incoming" | "call-incoming-video-group" => self.demo_call(what),
            "locked" => self.demo_lock(),
            "lock-damaged" => self.demo_lock_damaged(),
            "proxy-blocked" => self.demo_proxy_blocked(),
            "settings-proxy" => {
                self.open_settings();
                win.set_settings_start_category(6);
                win.set_show_settings(true);
                self.demo_proxy();
            }
            "sendloc" => {
                self.select_chat("111@s.whatsapp.net");
                self.demo_send_location(false);
            }
            "sendloc-fix" => {
                self.select_chat("111@s.whatsapp.net");
                self.demo_send_location(true);
            }
            "post-text" => {
                win.set_status_mode(true);
                self.refresh_statuses();
                self.demo_status_post(false);
            }
            "post-audience" => {
                win.set_status_mode(true);
                self.refresh_statuses();
                self.demo_status_post(true);
            }
            "mine" => {
                win.set_status_mode(true);
                self.refresh_statuses();
                self.open_status(-1);
            }
            w if w.starts_with("v04-") => self.demo_v04(w),
            w if w.starts_with("info") => self.demo_info(w),
            "folders" | "folders-form" => self.demo_folders(what == "folders-form"),
            "archived" => {
                win.set_archived_mode(true);
                self.refresh_chats();
            }
            "copy" => {
                self.select_chat("111@s.whatsapp.net");
                self.copy_message("a16");
                match arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
                    Ok(t) => log::warn!("demo: clipboard now holds {} chars: {t}", t.chars().count()),
                    Err(e) => log::warn!("demo: clipboard read failed: {e}"),
                }
            }
            "poll-create" => self.demo_poll_create(),
            "new-group" => self.demo_new_group(),
            "join-preview" => self.demo_join_preview(),
            "delete" | "delete-own" => {
                self.select_chat("111@s.whatsapp.net");
                win.invoke_show_delete("a17".into(), what == "delete-own");
            }
            // `seq:r300,150;l320,200`: real input clicks (r right, l left) 700 ms apart.
            seq if seq.starts_with("seq:") => {
                // A leading `chat;` opens Alice's chat first.
                if seq.starts_with("seq:chat;") {
                    self.select_chat("111@s.whatsapp.net");
                }
                // Anything that is not a click is an action: `open:222`, `search:foo`,
                // `paste-image`, `drop`, `drop-bad`, `state` (see `demo_action`).
                let steps: Vec<Result<(bool, i32, i32), String>> = seq
                    .trim_start_matches("seq:")
                    .split(';')
                    .filter(|t| !t.is_empty() && *t != "chat")
                    .map(|t| {
                        let click = (|| {
                            let right = t.starts_with('r');
                            let (x, y) = t.get(1..)?.split_once(',')?;
                            Some((right, x.parse().ok()?, y.parse().ok()?))
                        })();
                        click.ok_or_else(|| t.to_string())
                    })
                    .collect();
                for (i, step) in steps.into_iter().enumerate() {
                    let delay = Duration::from_millis(700 * (i as u64 + 1));
                    match step {
                        Ok((right, x, y)) => {
                            Timer::single_shot(delay, move || with_app(|a| a.demo_click(x, y, right)));
                        }
                        Err(action) => {
                            Timer::single_shot(delay, move || with_app(|a| a.demo_action(&action)));
                        }
                    }
                }
            }
            click if click.starts_with("rclick:") => {
                self.select_chat("111@s.whatsapp.net");
                let pos = click.trim_start_matches("rclick:").split_once(',');
                let xy = pos.and_then(|(x, y)| Some((x.parse::<i32>().ok()?, y.parse::<i32>().ok()?)));
                Timer::single_shot(Duration::from_millis(1200), || {
                    with_app(|a| {
                        if let Some(w) = a.win() {
                            w.invoke_scroll_to_bottom();
                        }
                    });
                });
                match xy {
                    Some((x, y)) => Timer::single_shot(Duration::from_millis(2000), move || {
                        with_app(|a| a.demo_right_click(x, y));
                    }),
                    None => log::warn!("demo: expected rclick:X,Y"),
                }
            }
            other => log::warn!("unknown WACLIENT_DEMO_UI value: {other}"),
        }
    }

    /// One step of a `WACLIENT_DEMO_UI=seq:` script that is not a mouse click.
    /// Each goes through the same code the window's callbacks use.
    fn demo_action(&self, action: &str) {
        let Some(win) = self.win() else { return };
        match action {
            a if a.starts_with("open:") => self.select_chat(&format!("{}@s.whatsapp.net", a.trim_start_matches("open:"))),
            a if a.starts_with("search:") => {
                let q = a.trim_start_matches("search:");
                win.set_search_open(true);
                win.set_search_query(q.into());
                self.search_input(q);
            }
            "esc" => self.demo_esc(),
            a if a.starts_with("block:") => {
                self.set_blocked(&format!("{}@s.whatsapp.net", a.trim_start_matches("block:")), true);
            }
            a if a.starts_with("v04-") => self.demo_v04(a),
            "info-open" => self.open_info(),
            "switch" => {
                let ids = self.profile_ids();
                let cur = self.p().id.clone();
                if let Some(next) = ids.iter().find(|i| **i != cur) {
                    self.activate(next);
                }
            }
            "rail-down" => {
                if let Some(first) = self.profile_ids().first() {
                    self.move_profile(first, 1);
                }
            }
            "search-next" => self.search_next(),
            "search-prev" => self.search_prev(),
            "search-close" => {
                win.set_search_open(false);
                win.set_search_query(Default::default());
                self.search_close();
            }
            "paste-image" => {
                let (w, h) = (96usize, 64usize);
                let mut bytes = Vec::with_capacity(w * h * 4);
                for y in 0..h {
                    for x in 0..w {
                        bytes.extend_from_slice(&[(x * 255 / w) as u8, (y * 255 / h) as u8, 160, 255]);
                    }
                }
                let data = arboard::ImageData { width: w, height: h, bytes: bytes.into() };
                match arboard::Clipboard::new().and_then(|mut c| c.set_image(data)) {
                    Ok(()) => log::warn!("demo: paste handled = {}", self.paste_from_clipboard()),
                    Err(e) => log::warn!("demo: could not put an image on the clipboard: {e}"),
                }
            }
            "drop" | "drop-bad" => {
                let dir = std::env::temp_dir().join("waclient-demo-drop");
                if let Err(e) = std::fs::create_dir_all(&dir) {
                    log::warn!("demo: {e}");
                    return;
                }
                let png = dir.join("holiday.png");
                let note = dir.join("notes.txt");
                let empty = dir.join("empty.dat");
                let written = image::RgbImage::from_fn(160, 100, |x, y| image::Rgb([(x * 2) as u8, (y * 2) as u8, 90]))
                    .save(&png)
                    .map_err(|e| e.to_string())
                    .and_then(|()| std::fs::write(&note, "hello").map_err(|e| e.to_string()))
                    .and_then(|()| std::fs::write(&empty, b"").map_err(|e| e.to_string()));
                if let Err(e) = written {
                    log::warn!("demo: {e}");
                    return;
                }
                let paths = if action == "drop" { vec![png, note] } else { vec![png, empty, dir] };
                self.drop_files(paths);
            }
            a if a.starts_with("rename:") => {
                let id = win.get_active_profile().id;
                win.invoke_rename_profile(id, a.trim_start_matches("rename:").into());
            }
            a if a.starts_with("compose:") => self.demo_composer(a.trim_start_matches("compose:")),
            a if a.starts_with("ops:") => self.demo_msgops(a.trim_start_matches("ops:")),
            "incognito" => win.invoke_incognito_toggled(),
            // `receipts-off:222` / `receipts-on:222`, `read-now:222`.
            a if a.starts_with("receipts-off:") || a.starts_with("receipts-on:") || a.starts_with("read-now:") => {
                let (verb, digits) = a.split_once(':').unwrap_or((a, ""));
                let jid = SharedString::from(format!("{digits}@s.whatsapp.net"));
                match verb {
                    "receipts-off" => win.invoke_chat_receipts(jid, false),
                    "receipts-on" => win.invoke_chat_receipts(jid, true),
                    _ => win.invoke_chat_read_now(jid),
                }
            }
            // `presence:222:online`, `presence:222:seen:90` (minutes ago), `presence:222:hidden`.
            a if a.starts_with("presence:") => {
                let parts: Vec<&str> = a.split(':').collect();
                let (online, last_seen) = match parts.as_slice() {
                    [_, _, "online"] => (true, None),
                    [_, _, "hidden"] => (false, None),
                    [_, _, "seen", mins] => (false, mins.parse::<i64>().ok().map(|m| Local::now().timestamp() - m * 60)),
                    _ => {
                        log::warn!("demo: expected presence:ID:online|hidden|seen:MINUTES");
                        return;
                    }
                };
                let id = self.p().id.clone();
                let jid = format!("{}@s.whatsapp.net", parts.get(1).copied().unwrap_or(""));
                self.on_event(&id, UiEvent::Presence { jid, online, last_seen });
            }
            "state-privacy" => log::warn!(
                "demo: incognito={} receipts_global_off={} header_receipts_off={} subtitle={:?}",
                win.get_incognito(),
                win.get_receipts_global_off(),
                win.get_header_receipts_off(),
                win.get_header_subtitle(),
            ),
            "state" => log::warn!(
                "demo: has_chat={} info_open={} title={:?} header={:?} rail={:?} search total={} current={} highlight={:?} attach_open={} attach_items={} rows={}",
                win.get_has_chat(),
                win.get_contact_info_open(),
                win.get_window_title(),
                win.get_active_profile().name,
                slint::Model::iter(&win.get_profiles()).map(|p| p.name.to_string()).collect::<Vec<_>>(),
                win.get_search_total(),
                win.get_search_current(),
                win.get_highlight_id(),
                win.get_attach_open(),
                slint::Model::row_count(&win.get_attach_items()),
                self.msgs_model.row_count(),
            ),
            other => log::warn!("demo: unknown action {other}"),
        }
    }
}

/// `muted_until` is a unix timestamp; negative means muted indefinitely.
/// Profile switches (rail click, Ctrl+1..5) wait while a voice message is recorded.
fn switch_blocked_by_recording(recording: bool) -> bool {
    recording
}

fn is_muted(r: &ChatRow, now: i64) -> bool {
    r.muted_until < 0 || r.muted_until > now
}

/// The chat `jid` in the full chat list.
fn find_chat<'a>(rows: &'a [ChatRow], jid: &str) -> Option<&'a ChatRow> {
    rows.iter().find(|r| r.jid == jid)
}

/// The chat `jid` when it has unread messages.
fn unread_chat<'a>(rows: &'a [ChatRow], jid: &str) -> Option<&'a ChatRow> {
    find_chat(rows, jid).filter(|r| r.unread > 0)
}

/// Unread messages of the chats that count towards the badge.
fn unread_total(rows: &[ChatRow]) -> u32 {
    let now = chrono::Utc::now().timestamp();
    let total: i64 = rows
        .iter()
        .filter(|r| !r.archived && !is_muted(r, now))
        .map(|r| r.unread.max(0))
        .sum();
    u32::try_from(total).unwrap_or(u32::MAX)
}

/// Without a fresh QR code for [`QR_STALE`] the login screen offers a reload.
fn arm_qr_timer(p: &ProfileState) {
    let id = p.id.clone();
    p.qr_timer.start(TimerMode::SingleShot, QR_STALE, move || {
        with_app(|a| a.offer_qr_retry(&id));
    });
}

/// Waits for every engine to close its event channel. They were all told to
/// stop already, so [`QUIT_GRACE`] is a shared deadline, not a per-engine one.
fn wait_engines(dones: Vec<(String, mpsc::Receiver<()>)>) {
    let deadline = Instant::now() + QUIT_GRACE;
    for (id, done) in dones {
        let left = deadline.saturating_duration_since(Instant::now());
        // The sender is dropped when the engine closes its event channel.
        if let Err(mpsc::RecvTimeoutError::Timeout) = done.recv_timeout(left) {
            log::warn!("engine of profile {id} did not stop within {QUIT_GRACE:?}");
        }
    }
}

fn conn_text(c: &ConnState) -> String {
    let text = match c {
        ConnState::Connecting => "Connecting…".to_string(),
        ConnState::Connected => "Connected".to_string(),
        ConnState::Disconnected { reason } if reason.is_empty() => "Disconnected".to_string(),
        ConnState::Disconnected { reason } => format!("Disconnected: {reason}"),
    };
    if proxy_ui::in_use() { format!("{text} (via proxy)") } else { text }
}

fn media_kind_for(path: &Path) -> OutgoingMediaKind {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" | "png" | "webp" => OutgoingMediaKind::Image,
        "mp4" | "mov" => OutgoingMediaKind::Video,
        "ogg" | "opus" => OutgoingMediaKind::Audio,
        _ => OutgoingMediaKind::Document,
    }
}

/// Accepts only absolute paths of existing files that, once symlinks and `..`
/// are resolved, live under one of `roots`. Returns the resolved path.
fn validate_launch_path(path: &str, roots: &[PathBuf]) -> Result<PathBuf, String> {
    let p = Path::new(path);
    if !p.is_absolute() {
        return Err("not an absolute path".to_string());
    }
    let real = std::fs::canonicalize(p).map_err(|e| format!("cannot resolve the path: {e}"))?;
    let inside = roots
        .iter()
        .filter_map(|r| std::fs::canonicalize(r).ok())
        .any(|r| real.starts_with(r));
    if inside { Ok(real) } else { Err("outside the media folders".to_string()) }
}

/// Runs the OS handler for an already validated file (or Explorer's "select"
/// view) without blocking the UI thread.
fn run_launcher(path: PathBuf, reveal: bool) {
    let spawned = std::thread::Builder::new().name("open-file".into()).spawn(move || {
        if let Err(e) = launch(&path, reveal) {
            log::error!("could not open {}: {e}", path.display());
            let queued = slint::invoke_from_event_loop(move || {
                with_app(|a| a.show_banner(1, &format!("Could not open the file: {e}")));
            });
            if let Err(e) = queued {
                log::debug!("open error not delivered: {e}");
            }
        }
    });
    if let Err(e) = spawned {
        log::error!("could not start the file opener thread: {e}");
    }
}

#[cfg(windows)]
#[link(name = "ole32")]
unsafe extern "system" {
    fn CoInitializeEx(reserved: *const std::ffi::c_void, co_init: u32) -> i32;
    fn CoUninitialize();
}

#[cfg(windows)]
const COINIT_APARTMENTTHREADED: u32 = 0x2;
#[cfg(windows)]
const COINIT_DISABLE_OLE1DDE: u32 = 0x4;

#[cfg(windows)]
#[link(name = "shell32")]
unsafe extern "system" {
    fn ShellExecuteW(
        hwnd: isize,
        operation: *const u16,
        file: *const u16,
        parameters: *const u16,
        directory: *const u16,
        show: i32,
    ) -> isize;
}

/// `\\?\C:\x` -> `C:\x`; ShellExecute and Explorer handle the plain form better.
#[cfg(windows)]
fn strip_verbatim(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => p.to_path_buf(),
    }
}

#[cfg(windows)]
fn launch(path: &Path, reveal: bool) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::process::CommandExt;
    let path = strip_verbatim(path);
    if reveal {
        let mut cmd = std::process::Command::new("explorer");
        cmd.raw_arg(format!("/select,\"{}\"", path.display()));
        return cmd.spawn().map(drop);
    }
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let verb: Vec<u16> = "open".encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `verb` and `wide` are valid NUL-terminated UTF-16 strings that
    // outlive the call; the remaining pointer arguments are allowed to be null.
    let code = unsafe {
        ShellExecuteW(0, verb.as_ptr(), wide.as_ptr(), std::ptr::null(), std::ptr::null(), 1)
    };
    // Values above 32 mean success.
    if code > 32 {
        Ok(())
    } else {
        Err(std::io::Error::other(format!("ShellExecuteW failed with code {code}")))
    }
}

#[cfg(not(windows))]
fn launch(path: &Path, reveal: bool) -> std::io::Result<()> {
    let target = if reveal {
        path.parent().map_or_else(|| path.to_path_buf(), Path::to_path_buf)
    } else {
        path.to_path_buf()
    };
    std::process::Command::new("xdg-open").arg(target).spawn().map(drop)
}

/// Thumbnails are decoded inline (tiny); full images are decoded on the worker
/// pool and swapped in by [`App::on_decoded`].
fn images_for(
    cache: &mut HashMap<String, Option<Image>>,
    decoding: &mut HashSet<String>,
    pool: &DecodePool,
    m: &MessageRow,
) -> MsgImages {
    let full = match (m.kind, m.local_path.as_deref()) {
        (MessageKind::Image | MessageKind::Sticker, Some(p)) => {
            let key = format!("f:{p}");
            match cache.get(&key) {
                Some(v) => v.clone(),
                None => {
                    if decoding.insert(key.clone()) {
                        let job = DecodeJob::Full {
                            chat_jid: m.chat_jid.clone(),
                            key: key.clone(),
                            msg_id: m.id.clone(),
                            path: PathBuf::from(p),
                            max_dim: FULL_IMAGE_MAX_DIM,
                        };
                        if !pool.submit(job) {
                            decoding.remove(&key);
                        }
                    }
                    None
                }
            }
        }
        _ => None,
    };
    let thumb = match (&full, m.thumbnail.as_deref()) {
        (None, Some(bytes)) => cache
            .entry(format!("t:{}:{}", m.chat_jid, m.id))
            .or_insert_with(|| models::decode_image_bytes(bytes))
            .clone(),
        _ => None,
    };
    let link_thumb = m.link_thumb.as_deref().and_then(|bytes| {
        cache
            .entry(format!("l:{}:{}", m.chat_jid, m.id))
            .or_insert_with(|| models::decode_image_bytes(bytes))
            .clone()
    });
    MsgImages { thumb, full, link_thumb }
}

#[cfg(test)]
mod tests {
    #[test]
    fn recording_blocks_profile_switch() {
        assert!(super::switch_blocked_by_recording(true));
        assert!(!super::switch_blocked_by_recording(false));
    }

    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("waclient-launch-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    fn chat_row(jid: &str, unread: i64, archived: bool) -> ChatRow {
        ChatRow {
            jid: jid.to_string(),
            kind: ChatKind::Dm,
            display_name: jid.to_string(),
            last_ts: 0,
            unread,
            pinned: false,
            archived,
            muted_until: 0,
            last_preview: None,
            receipts_off: false,
            ephemeral_secs: 0,
            ephemeral_ts: 0,
        }
    }

    #[test]
    fn open_chat_is_found_in_the_full_list_even_when_archived() {
        let rows = [chat_row("a@s.whatsapp.net", 0, false), chat_row("b@s.whatsapp.net", 3, true)];
        assert_eq!(unread_chat(&rows, "b@s.whatsapp.net").map(|r| r.unread), Some(3));
        assert!(unread_chat(&rows, "a@s.whatsapp.net").is_none());
        assert!(unread_chat(&rows, "c@s.whatsapp.net").is_none());
        assert!(find_chat(&rows, "a@s.whatsapp.net").is_some());
    }

    #[test]
    fn launch_path_must_be_inside_the_media_roots() {
        let root = temp_dir("root");
        let outside = temp_dir("outside");
        let inside_file = root.join("a.pdf");
        let outside_file = outside.join("b.pdf");
        std::fs::write(&inside_file, b"x").expect("write");
        std::fs::write(&outside_file, b"x").expect("write");
        let roots = [root.clone()];

        assert!(validate_launch_path(&inside_file.to_string_lossy(), &roots).is_ok());
        assert!(validate_launch_path(&outside_file.to_string_lossy(), &roots).is_err());
        // `..` must not escape the root.
        let escape = root.join("..").join(outside.file_name().expect("name")).join("b.pdf");
        assert!(validate_launch_path(&escape.to_string_lossy(), &roots).is_err());
        assert!(validate_launch_path("relative/a.pdf", &roots).is_err());
        assert!(validate_launch_path(&root.join("missing.pdf").to_string_lossy(), &roots).is_err());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }
}
