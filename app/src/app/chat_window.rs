//! "Open in new window": a chat popped out into its own top-level window with
//! the messages and the composer. The window belongs to the profile it was
//! opened from (a profile switch in the main window does not touch it), shows
//! the same rows as the main view (`models::msg_item`) from its own model and
//! sends through that profile's engine.
//!
//! Limits: at most [`MAX_POPOUTS`] windows, one per chat; text, reply,
//! @mentions, stickers, drafts (shared with the main window), the typing
//! indicator, the chat's wallpaper, reactions (view and toggle), media
//! open/download, full-size inline images for the newest picture rows, mark as
//! read. Everything else (message menu beyond Reply/Copy, polls voting, info
//! panel, search, translation, attachments, voice notes) stays in the main
//! window. The windows are closed when the app locks so no content stays
//! visible, and come back after the next successful unlock.

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use chrono::Local;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::{
    CloseRequestResponse, ComponentHandle, Image, LogicalSize, Model, ModelRc, PhysicalPosition, SharedString, Timer, TimerMode,
    VecModel,
};
use wa_engine::{Command, ConnState, UiEvent};
use wa_store::{ChatKind, ChatRow, MessageKind, MessageRow};

use super::composer::undo::UndoStack;
use super::paging::{self, Pager};
use super::composer::{Content, DraftStep, PopMentions, draft_content, draft_step, typing_due};
use super::sticker_ui::{self, StickerUi};
use super::{
    App, FIX_EPS, FIX_MAX, FIX_STABLE_CHECKS, FIX_TICK, FULL_IMAGE_MAX_DIM, LAYOUT_GUARD, PAGE, ProfileState, TYPING_PAUSE,
    TYPING_TTL, conn_text, emoji_ui, find_chat, privacy, with_app,
};
use crate::decode::{DecodeDone, DecodeJob, DecodePool};
use crate::emoji::{QUICK_REACTIONS, ReactionChip};
use crate::models::{self, MsgImages, RowUi};
use crate::settings::WallpaperPref;
use crate::status_view::{PollViewModel, poll_view_model};
use crate::{AppearanceState, ChatWindow, EmojiRow, Mentions, MsgItem, ReactionConfig, StickerState, TextDir, Theme, drop_paste};

/// Pop-out windows open at the same time.
pub(super) const MAX_POPOUTS: usize = 4;
/// How often headers, theme and the unread state of the windows are refreshed.
const POPOUT_TICK: Duration = Duration::from_secs(1);
/// A chat is marked read at most this often by a focused window.
const MARK_READ_EVERY: Duration = Duration::from_secs(3);
/// Newest picture rows of a window shown full size (older ones keep their thumbnail).
const FULL_ROWS: usize = 6;
/// Decoded full-size images kept in memory for all windows together.
const FULL_CACHE_BYTES: usize = 64 * 1024 * 1024;

/// A chat of a profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Key {
    pub profile: String,
    pub chat: String,
}

/// What opening a pop-out for a chat should do.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Plan {
    /// The app is locked: nothing opens.
    Locked,
    /// The chat already has a window: bring it to the front.
    Refocus,
    /// [`MAX_POPOUTS`] windows are open.
    Full,
    Create,
}

/// The open pop-outs. Generic so the rules can be tested without windows.
pub(super) struct Registry<T> {
    items: Vec<(Key, T)>,
}

impl<T> Registry<T> {
    pub(super) fn new() -> Self {
        Registry { items: Vec::new() }
    }

    pub(super) fn plan(&self, key: &Key, locked: bool) -> Plan {
        if locked {
            Plan::Locked
        } else if self.get(key).is_some() {
            Plan::Refocus
        } else if self.items.len() >= MAX_POPOUTS {
            Plan::Full
        } else {
            Plan::Create
        }
    }

    pub(super) fn get(&self, key: &Key) -> Option<&T> {
        self.items.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub(super) fn insert(&mut self, key: Key, value: T) {
        self.items.retain(|(k, _)| *k != key);
        self.items.push((key, value));
    }

    pub(super) fn remove(&mut self, key: &Key) -> Option<T> {
        let at = self.items.iter().position(|(k, _)| k == key)?;
        Some(self.items.remove(at).1)
    }

    /// Takes every entry (the app locked or is quitting).
    pub(super) fn drain_all(&mut self) -> Vec<T> {
        self.items.drain(..).map(|(_, v)| v).collect()
    }

    /// Takes the entries of one profile (it was removed).
    pub(super) fn drain_profile(&mut self, profile: &str) -> Vec<T> {
        let (gone, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.items).into_iter().partition(|(k, _)| k.profile == profile);
        self.items = kept;
        gone.into_iter().map(|(_, v)| v).collect()
    }

    pub(super) fn values(&self) -> Vec<&T> {
        self.items.iter().map(|(_, v)| v).collect()
    }

    pub(super) fn len(&self) -> usize {
        self.items.len()
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = (&Key, &T)> {
        self.items.iter().map(|(k, v)| (k, v))
    }
}

/// Where a pop-out was on screen.
#[derive(Clone, Debug, PartialEq)]
struct Geometry {
    size: LogicalSize,
    pos: Option<PhysicalPosition>,
}

/// A minimized window reports a position far off screen; that is not a place to go back to.
fn usable_position(p: PhysicalPosition) -> Option<PhysicalPosition> {
    (p.x > -10_000 && p.y > -10_000).then_some(p)
}

/// A pop-out that was open when the app locked.
#[derive(Clone, Debug, PartialEq)]
struct Saved {
    key: Key,
    geom: Option<Geometry>,
}

/// The pop-outs to bring back after the lock screen. Kept only in memory; an
/// unlock that has not happened yet never hands anything out.
#[derive(Default)]
struct ReopenList {
    items: Vec<Saved>,
}

impl ReopenList {
    fn remember(&mut self, items: Vec<Saved>) {
        self.items = items;
    }

    /// The windows to reopen, once; nothing while the app is still locked.
    fn take(&mut self, locked: bool) -> Vec<Saved> {
        if locked {
            return Vec::new();
        }
        let mut items = std::mem::take(&mut self.items);
        items.truncate(MAX_POPOUTS);
        items
    }
}

/// The typing indicator of one pop-out (the main window keeps the same
/// state in `State::last_typing_sent` / `composing_in`).
#[derive(Default)]
struct TypingState {
    last_sent: Option<Instant>,
    composing: bool,
}

impl TypingState {
    /// An edit of the composer: `Some(true)` when "typing..." is to be sent
    /// now, `Some(false)` when the composer was emptied after we said we were typing.
    fn edit(&mut self, empty: bool, now: Instant) -> Option<bool> {
        if empty {
            return self.stop();
        }
        if typing_due(self.last_sent.map(|t| now.saturating_duration_since(t))) {
            self.last_sent = Some(now);
            self.composing = true;
            Some(true)
        } else {
            None
        }
    }

    /// Typing ended (pause, send, close): `Some(false)` when the peer was told we were typing.
    fn stop(&mut self) -> Option<bool> {
        self.last_sent = None;
        std::mem::take(&mut self.composing).then_some(false)
    }
}

/// Decoded pictures with a memory budget; the oldest are dropped first.
struct ByteCache<V> {
    map: HashMap<String, (V, usize)>,
    order: VecDeque<String>,
    bytes: usize,
    cap: usize,
}

impl<V: Clone> ByteCache<V> {
    fn new(cap: usize) -> Self {
        ByteCache { map: HashMap::new(), order: VecDeque::new(), bytes: 0, cap }
    }

    fn get(&self, key: &str) -> Option<V> {
        self.map.get(key).map(|(v, _)| v.clone())
    }

    fn insert(&mut self, key: String, value: V, size: usize) {
        if let Some((_, old)) = self.map.remove(&key) {
            self.bytes -= old;
            self.order.retain(|k| *k != key);
        }
        self.bytes += size;
        self.order.push_back(key.clone());
        self.map.insert(key, (value, size));
        // The entry just added always stays, even when it alone is over budget.
        while self.bytes > self.cap && self.order.len() > 1 {
            if let Some(old) = self.order.pop_front() {
                if let Some((_, size)) = self.map.remove(&old) {
                    self.bytes -= size;
                }
            }
        }
    }

    fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
        self.bytes = 0;
    }
}

/// The `n` newest of the rows that show a picture: only those are decoded full size.
fn full_image_ids(msgs: &[MessageRow], n: usize) -> HashSet<String> {
    msgs.iter()
        .rev()
        .filter(|m| matches!(m.kind, MessageKind::Image | MessageKind::Sticker) && m.local_path.is_some())
        .take(n)
        .map(|m| m.id.clone())
        .collect()
}

/// What an engine event means for the pop-outs.
pub(super) enum Note {
    /// Rows of this chat changed (new message, status, reaction, edit...).
    Messages(String),
    MediaReady { chat: String, id: String, path: PathBuf },
    MediaFailed { chat: String, id: String },
    /// Something the header or the composer gate shows changed.
    Header,
    /// The chat list changed (names, unread counters).
    Chats,
}

pub(super) fn note_for(ev: &UiEvent) -> Option<Note> {
    Some(match ev {
        UiEvent::MessagesChanged { chat_jid } | UiEvent::MessageStatus { chat_jid, .. } => Note::Messages(chat_jid.clone()),
        UiEvent::MediaReady { chat_jid, id, path } => {
            Note::MediaReady { chat: chat_jid.clone(), id: id.clone(), path: path.clone() }
        }
        UiEvent::MediaFailed { chat_jid, id, .. } => Note::MediaFailed { chat: chat_jid.clone(), id: id.clone() },
        UiEvent::Connection(_)
        | UiEvent::Typing { .. }
        | UiEvent::Presence { .. }
        | UiEvent::AvatarReady { .. }
        | UiEvent::ContactInfo(_)
        | UiEvent::Blocklist { .. } => Note::Header,
        UiEvent::ChatsChanged => Note::Chats,
        _ => return None,
    })
}

/// A focused window marks its chat read when there is something unread and it
/// has not just done so.
fn should_mark_read(foreground: bool, unread: i64, since_last: Option<Duration>) -> bool {
    foreground && unread > 0 && since_last.is_none_or(|d| d >= MARK_READ_EVERY)
}

#[derive(Clone, Copy)]
enum Fix {
    Bottom,
}

/// A running scroll correction (the pop-out twin of `App::schedule_fix`).
struct FixRun {
    fix: Fix,
    started: Instant,
    last_h: Option<f32>,
    stable: u8,
}

impl FixRun {
    fn new(fix: Fix) -> Self {
        FixRun { fix, started: Instant::now(), last_h: None, stable: 0 }
    }

    /// Feeds the current content height; true once the layout has settled.
    fn settled(&mut self, h: f32) -> bool {
        let unchanged = self.last_h.is_some_and(|l| (h - l).abs() <= FIX_EPS);
        self.stable = if unchanged { self.stable + 1 } else { 0 };
        self.last_h = Some(h);
        self.stable >= FIX_STABLE_CHECKS || self.started.elapsed() >= FIX_MAX
    }
}

struct PState {
    row: Option<ChatRow>,
    msgs: Vec<MessageRow>,
    limit: usize,
    exhausted: bool,
    pager: Pager,
    reply: Option<(String, String, String)>,
    downloading: HashSet<String>,
    failed: HashSet<String>,
    after_download: HashSet<String>,
    media: HashMap<String, Option<Image>>,
    polls: HashMap<String, PollViewModel>,
    reactions: HashMap<String, Vec<ReactionChip>>,
    seen_paths: HashSet<String>,
    guard_until: Instant,
    force_bottom: bool,
    last_mark: Option<Instant>,
    /// What this composer last synced with the shared draft of its chat.
    draft_known: Option<Content>,
    typing: TypingState,
    /// Wallpaper preference and mode the window shows now.
    wallpaper: Option<(WallpaperPref, bool)>,
}

pub(super) struct Popout {
    key: Key,
    win: ChatWindow,
    model: Rc<VecModel<MsgItem>>,
    emoji_rows: Rc<VecModel<EmojiRow>>,
    st: RefCell<PState>,
    fix_timer: Timer,
    fix: RefCell<Option<FixRun>>,
    mentions: RefCell<PopMentions>,
    stickers: StickerUi,
    /// Ends "typing..." a moment after the last keystroke.
    typing_timer: Timer,
}

/// The pop-out windows of the app (a field of [`App`]).
pub(super) struct Popouts {
    reg: RefCell<Registry<Rc<Popout>>>,
    tick: Timer,
    /// Size of the window closed last; new windows start with it.
    size: Cell<Option<LogicalSize>>,
    /// Windows that were open when the app locked.
    reopen: RefCell<ReopenList>,
    /// Full-size pictures of the windows (by file path), decoded on `pool`.
    images: RefCell<ByteCache<Option<Image>>>,
    decoding: RefCell<HashSet<String>>,
    /// Started with the first window: most sessions never open one.
    pool: OnceCell<DecodePool>,
}

impl Popouts {
    pub(super) fn new() -> Self {
        Popouts {
            reg: RefCell::new(Registry::new()),
            tick: Timer::default(),
            size: Cell::new(None),
            reopen: RefCell::new(ReopenList::default()),
            images: RefCell::new(ByteCache::new(FULL_CACHE_BYTES)),
            decoding: RefCell::new(HashSet::new()),
            pool: OnceCell::new(),
        }
    }
}

#[cfg(windows)]
#[link(name = "user32")]
unsafe extern "system" {
    fn GetForegroundWindow() -> isize;
}

/// True when the window is shown, not minimized and the foreground window.
fn is_foreground(win: &ChatWindow) -> bool {
    let w = win.window();
    if !w.is_visible() || w.is_minimized() {
        return false;
    }
    #[cfg(windows)]
    {
        let handle = w.window_handle();
        let Ok(h) = handle.window_handle() else { return false };
        match h.as_raw() {
            // SAFETY: GetForegroundWindow has no preconditions.
            RawWindowHandle::Win32(h) => h.hwnd.get() == unsafe { GetForegroundWindow() },
            _ => false,
        }
    }
    #[cfg(not(windows))]
    {
        true
    }
}

/// Rows with a full-size picture show it instead of the thumbnail.
fn images_for(cache: &mut HashMap<String, Option<Image>>, m: &MessageRow, full: Option<Image>) -> MsgImages {
    let thumb = m.thumbnail.as_deref().filter(|_| full.is_none()).and_then(|bytes| {
        cache.entry(format!("t:{}:{}", m.chat_jid, m.id)).or_insert_with(|| models::decode_image_bytes(bytes)).clone()
    });
    let link_thumb = m.link_thumb.as_deref().and_then(|bytes| {
        cache.entry(format!("l:{}:{}", m.chat_jid, m.id)).or_insert_with(|| models::decode_image_bytes(bytes)).clone()
    });
    MsgImages { thumb, full, link_thumb }
}

impl App {
    /// "Open in new window" for a chat of the active profile.
    pub(super) fn popout_open(&self, jid: &str) {
        let profile = self.p();
        self.popout_open_in(&profile, jid);
    }

    fn popout_open_in(&self, profile: &Rc<ProfileState>, jid: &str) {
        self.popout_open_at(profile, jid, None);
    }

    fn popout_open_at(&self, profile: &Rc<ProfileState>, jid: &str, geom: Option<Geometry>) {
        let key = Key { profile: profile.id.clone(), chat: jid.to_string() };
        let plan = self.popouts.reg.borrow().plan(&key, self.is_locked());
        match plan {
            Plan::Locked => {}
            Plan::Refocus => {
                let existing = self.popouts.reg.borrow().get(&key).cloned();
                if let Some(p) = existing {
                    bring_to_front(&p.win);
                }
            }
            Plan::Full => self.show_toast(&format!("At most {MAX_POPOUTS} chats can be open in their own windows")),
            Plan::Create => self.popout_create(profile, key, geom),
        }
    }

    fn popout_create(&self, profile: &Rc<ProfileState>, key: Key, geom: Option<Geometry>) {
        let Some(page) = self.ok("load messages", profile.store.messages_page(&key.chat, None, PAGE as u32)) else {
            return;
        };
        let row = {
            let st = profile.st.borrow();
            find_chat(&st.chats, &key.chat).cloned()
        }
        .or_else(|| profile.store.chat(&key.chat).ok().flatten());
        let Some(row) = row else {
            self.show_banner(1, "This chat cannot be opened in a new window yet");
            return;
        };
        let win = match ChatWindow::new() {
            Ok(w) => w,
            Err(e) => {
                log::error!("could not create the chat window: {e}");
                self.show_banner(1, &format!("Could not open a new window: {e}"));
                return;
            }
        };
        let model: Rc<VecModel<MsgItem>> = Rc::new(VecModel::default());
        let emoji_rows = Rc::new(VecModel::from(emoji_ui::category_rows().0));
        win.set_messages(ModelRc::from(model.clone()));
        win.set_emoji_rows(ModelRc::from(emoji_rows.clone()));
        win.set_emoji_tabs(ModelRc::new(VecModel::from(emoji_ui::category_rows().1)));
        win.set_emoji_recents(ModelRc::new(VecModel::<SharedString>::default()));
        let quick: Vec<SharedString> = QUICK_REACTIONS.iter().map(|e| SharedString::from(*e)).collect();
        win.global::<ReactionConfig>().set_quick(ModelRc::new(VecModel::from(quick)));
        win.global::<TextDir>().on_is_rtl(|text| super::composer::undo::paragraph_is_rtl(text.as_str()));
        win.set_enter_sends(profile.settings.borrow().enter_sends);
        if let Some(g) = &geom {
            win.window().set_size(g.size);
            if let Some(pos) = g.pos {
                win.window().set_position(pos);
            }
        } else if let Some(size) = self.popouts.size.get() {
            win.window().set_size(size);
        }
        let exhausted = page.len() < PAGE;
        let unread = row.unread;
        let draft = self.draft_get(profile, &key.chat);
        let p = Rc::new(Popout {
            key: key.clone(),
            win,
            model,
            emoji_rows,
            st: RefCell::new(PState {
                row: Some(row),
                msgs: page,
                limit: PAGE,
                exhausted,
                pager: Pager::default(),
                reply: draft.as_ref().and_then(|d| d.1.clone()),
                downloading: HashSet::new(),
                failed: HashSet::new(),
                after_download: HashSet::new(),
                media: HashMap::new(),
                polls: HashMap::new(),
                reactions: HashMap::new(),
                seen_paths: HashSet::new(),
                guard_until: Instant::now(),
                force_bottom: false,
                last_mark: None,
                draft_known: draft.clone(),
                typing: TypingState::default(),
                wallpaper: None,
            }),
            fix_timer: Timer::default(),
            fix: RefCell::new(None),
            mentions: RefCell::default(),
            stickers: StickerUi::new(),
            typing_timer: Timer::default(),
        });
        self.popout_wire(&p);
        self.popout_sync_theme(&p);
        self.popout_wallpaper(&p, true);
        // The draft of the chat (shared with the main window) goes into the composer.
        if let Some((text, _)) = &draft {
            p.win.invoke_set_composer(text.as_str().into(), i32::try_from(text.len()).unwrap_or(i32::MAX));
        }
        self.popout_sync_reply(&p);
        self.popout_rebuild(&p);
        self.popout_header(&p);
        self.popouts.reg.borrow_mut().insert(key.clone(), p.clone());
        if !self.popouts.tick.running() {
            self.popouts.tick.start(TimerMode::Repeated, POPOUT_TICK, || with_app(App::popouts_tick));
        }
        if let Err(e) = p.win.show() {
            log::error!("could not show the chat window: {e}");
            self.popout_close(&key);
            self.show_banner(1, &format!("Could not open a new window: {e}"));
            return;
        }
        p.win.invoke_focus_composer();
        self.popout_fix(&p, Fix::Bottom);
        self.popout_subscribe(profile, &key.chat);
        if unread > 0 && is_foreground(&p.win) {
            self.popout_mark_read(&p);
        }
    }

    /// Presence for a direct chat and the send permission of a group.
    fn popout_subscribe(&self, profile: &ProfileState, jid: &str) {
        if profile.st.borrow().conn != ConnState::Connected {
            return;
        }
        match ChatKind::from_jid(jid) {
            ChatKind::Dm => (profile.send)(Command::SubscribePresence { jid: jid.to_string() }),
            ChatKind::Group if !self.gate_known(&profile.id, jid) => {
                (profile.send)(Command::FetchContactInfo { jid: jid.to_string() });
            }
            _ => {}
        }
    }

    fn popout_wire(&self, p: &Rc<Popout>) {
        let w = &p.win;
        let on = |f: fn(&App, &Rc<Popout>)| {
            let weak = Rc::downgrade(p);
            move || {
                if let Some(p) = weak.upgrade() {
                    with_app(|a| f(a, &p));
                }
            }
        };
        let on_s = |f: fn(&App, &Rc<Popout>, &str)| {
            let weak = Rc::downgrade(p);
            move |s: SharedString| {
                if let Some(p) = weak.upgrade() {
                    with_app(|a| f(a, &p, s.as_str()));
                }
            }
        };
        w.on_send_message(on_s(App::popout_send));
        w.on_composing(on_s(App::popout_edited));
        w.on_reply_to(on_s(App::popout_reply));
        w.on_cancel_reply(on(|a, p| {
            p.st.borrow_mut().reply = None;
            a.popout_sync_reply(p);
        }));
        {
            let weak = Rc::downgrade(p);
            w.on_react(move |id, emoji| {
                if let Some(p) = weak.upgrade() {
                    with_app(|a| a.popout_react(&p, id.as_str(), emoji.as_str()));
                }
            });
        }
        w.on_download(on_s(App::popout_download));
        w.on_open(on_s(App::popout_open_media));
        w.on_toggle_play(on_s(App::popout_open_media));
        w.on_show_in_folder(on_s(App::popout_reveal));
        w.on_copy_text(on_s(App::popout_copy));
        w.on_open_link(on_s(App::popout_link));
        w.on_load_older(on(|a, p| a.popout_load_older(p, false)));
        w.on_jump_latest(on(App::popout_jump_latest));
        w.on_unblock(on(App::popout_unblock));
        w.on_emoji_search(on_s(|_, p, q| p.emoji_rows.set_vec(emoji_ui::picker_rows(q))));
        w.on_emoji_picked(on_s(|a, _, e| a.emoji_picked(e)));
        w.on_emoji_insert(on_s(App::popout_emoji_insert));

        // The Stickers tab of the picker (`StickerState` is per window).
        p.stickers.attach_to(&w.global::<StickerState>());
        let g = w.global::<StickerState>();
        {
            let weak = Rc::downgrade(p);
            g.on_refresh(move || {
                if let Some(p) = weak.upgrade() {
                    with_app(|a| a.popout_stickers_refresh(&p));
                }
            });
        }
        {
            let weak = Rc::downgrade(p);
            g.on_send(move |favs, i| {
                if let Some(p) = weak.upgrade() {
                    with_app(|a| a.popout_stickers_send(&p, favs, i));
                }
            });
        }
        {
            let weak = Rc::downgrade(p);
            g.on_toggle_fav(move |favs, i| {
                if let Some(p) = weak.upgrade() {
                    with_app(|a| a.popout_stickers_toggle_fav(&p, favs, i));
                }
            });
        }

        // Composer helpers: mention suggestions, formatting shortcuts,
        // undo/redo and RTL Home/End, with this window's own history.
        let undo = Rc::new(RefCell::new(UndoStack::default()));
        let m = w.global::<Mentions>();
        {
            let weak = Rc::downgrade(p);
            m.on_edited(move |text, cursor| {
                if let Some(p) = weak.upgrade() {
                    with_app(|a| a.popout_mention_edited(&p, text.as_str(), usize::try_from(cursor).unwrap_or(0)));
                }
            });
        }
        {
            let weak = Rc::downgrade(p);
            m.on_pick(move |i| {
                if let Some(p) = weak.upgrade() {
                    with_app(|a| a.popout_mention_pick(&p, usize::try_from(i).unwrap_or(usize::MAX)));
                }
            });
        }
        {
            let weak = w.as_weak();
            m.on_step(move |d| {
                if let Some(win) = weak.upgrade() {
                    App::pop_mention_step(&win, d);
                }
            });
        }
        m.on_format({
            let weak = w.as_weak();
            move |text, anchor, cursor, kind| {
                let Some(kind) = crate::format::Kind::from_code(kind) else { return text };
                let off = |v: i32| usize::try_from(v).unwrap_or(0);
                let edit = crate::format::apply(text.as_str(), off(anchor), off(cursor), kind);
                if let Some(win) = weak.upgrade() {
                    let m = win.global::<Mentions>();
                    m.set_fmt_start(i32::try_from(edit.anchor).unwrap_or(i32::MAX));
                    m.set_fmt_end(i32::try_from(edit.cursor).unwrap_or(i32::MAX));
                }
                edit.text.into()
            }
        });
        m.on_note({
            let undo = undo.clone();
            move |text, cursor, atomic| {
                undo.borrow_mut().note(text.as_str(), usize::try_from(cursor).unwrap_or(0), atomic);
            }
        });
        m.on_reset_undo({
            let undo = undo.clone();
            move |text, cursor| undo.borrow_mut().reset(text.as_str(), usize::try_from(cursor).unwrap_or(0))
        });
        m.on_history({
            let undo = undo.clone();
            let weak = w.as_weak();
            move |text, cursor, redo| {
                let snap = {
                    let mut u = undo.borrow_mut();
                    u.note(text.as_str(), usize::try_from(cursor).unwrap_or(0), false);
                    if redo { u.redo() } else { u.undo() }
                };
                if let Some(win) = weak.upgrade() {
                    let m = win.global::<Mentions>();
                    m.set_undo_ok(snap.is_some());
                    m.set_undo_cursor(snap.as_ref().map_or(0, |s| i32::try_from(s.cursor).unwrap_or(i32::MAX)));
                }
                snap.map_or(text, |s| s.text.into())
            }
        });
        m.on_line_nav({
            let weak = w.as_weak();
            move |text, anchor, cursor, home, shift| {
                let off = |v: i32| usize::try_from(v).unwrap_or(0);
                let Some((a, c)) = super::composer::undo::rtl_home_end(text.as_str(), off(anchor), off(cursor), home, shift)
                else {
                    return false;
                };
                let Some(win) = weak.upgrade() else { return false };
                let m = win.global::<Mentions>();
                m.set_nav_anchor(i32::try_from(a).unwrap_or(i32::MAX));
                m.set_nav_cursor(i32::try_from(c).unwrap_or(i32::MAX));
                true
            }
        });

        m.on_arrow_nav({
            let weak = w.as_weak();
            move |text, anchor, cursor, left, word, shift| {
                let off = |v: i32| usize::try_from(v).unwrap_or(0);
                let Some((a, c)) =
                    super::composer::undo::rtl_arrow(text.as_str(), off(anchor), off(cursor), left, word, shift)
                else {
                    return false;
                };
                let Some(win) = weak.upgrade() else { return false };
                let m = win.global::<Mentions>();
                m.set_nav_anchor(i32::try_from(a).unwrap_or(i32::MAX));
                m.set_nav_cursor(i32::try_from(c).unwrap_or(i32::MAX));
                true
            }
        });

        // Ctrl shortcuts on non-Latin layouts; nothing is dropped into pop-outs.
        drop_paste::enable_file_drop(w.window(), |_| {});

        let key = p.key.clone();
        let weak_win = w.as_weak();
        w.window().on_close_requested(move || {
            // Dropping the window inside its own close request is not safe: finish afterwards.
            let key = key.clone();
            let size = weak_win.upgrade().map(|w| w.window().size().to_logical(w.window().scale_factor()));
            Timer::single_shot(Duration::ZERO, move || {
                with_app(|a| {
                    if let Some(size) = size {
                        a.popouts.size.set(Some(size));
                    }
                    a.popout_close(&key);
                });
            });
            CloseRequestResponse::HideWindow
        });
    }

    fn popout_close(&self, key: &Key) {
        let removed = {
            let mut reg = self.popouts.reg.borrow_mut();
            let removed = reg.remove(key);
            if reg.len() == 0 {
                self.popouts.tick.stop();
            }
            removed
        };
        if let Some(p) = removed {
            self.popout_stop_typing(&p);
            Self::popout_dispose(&p);
        }
        if self.popouts.reg.borrow().len() == 0 {
            self.popouts_forget_images();
        }
    }

    /// No window is left: the decoded pictures go with them.
    fn popouts_forget_images(&self) {
        self.popouts.images.borrow_mut().clear();
        self.popouts.decoding.borrow_mut().clear();
    }

    fn popout_dispose(p: &Popout) {
        p.fix_timer.stop();
        if let Err(e) = p.win.hide() {
            log::warn!("could not hide the chat window: {e}");
        }
    }

    /// Closes every pop-out (the app quits).
    pub(super) fn popouts_close_all(&self) {
        let all = self.popouts.reg.borrow_mut().drain_all();
        self.popouts.tick.stop();
        for p in &all {
            self.popout_stop_typing(p);
            Self::popout_dispose(p);
        }
        self.popouts_forget_images();
    }

    /// The app locks: remembers which chats were open (and where), then closes
    /// every window so no content stays visible.
    pub(super) fn popouts_lock(&self) {
        let saved: Vec<Saved> = self
            .popouts
            .reg
            .borrow()
            .iter()
            .map(|(k, p)| Saved { key: k.clone(), geom: geometry_of(&p.win) })
            .collect();
        self.popouts.reopen.borrow_mut().remember(saved);
        self.popouts_close_all();
    }

    /// The passcode was accepted: brings the windows of before the lock back.
    pub(super) fn popouts_reopen(&self) {
        let saved = self.popouts.reopen.borrow_mut().take(self.is_locked());
        for s in saved {
            let Some(profile) = self.find(&s.key.profile) else { continue };
            self.popout_open_at(&profile, &s.key.chat, s.geom);
        }
    }

    /// True when a pop-out is the foreground window (the user is working in it).
    pub(super) fn popout_in_front(&self) -> bool {
        self.popout_list().iter().any(|p| is_foreground(&p.win))
    }

    /// True when a pop-out is shown and not minimized.
    pub(super) fn popout_shown(&self) -> bool {
        self.popout_list().iter().any(|p| p.win.window().is_visible() && !p.win.window().is_minimized())
    }

    fn popout_list(&self) -> Vec<Rc<Popout>> {
        self.popouts.reg.borrow().values().into_iter().cloned().collect()
    }

    /// Once a second: closes windows of removed profiles, follows the main
    /// window's theme, refreshes the header (typing and "last seen" expire on
    /// their own) and marks the chat read while the window is focused.
    fn popouts_tick(&self) {
        if self.is_locked() {
            self.popouts_lock();
            return;
        }
        let gone: Vec<Rc<Popout>> = {
            let profs = self.profs.borrow();
            let mut reg = self.popouts.reg.borrow_mut();
            let removed: Vec<String> = reg
                .values()
                .iter()
                .map(|p| p.key.profile.clone())
                .filter(|id| !profs.iter().any(|p| p.id == *id))
                .collect();
            removed.iter().flat_map(|id| reg.drain_profile(id)).collect()
        };
        for p in &gone {
            Self::popout_dispose(p);
        }
        for p in self.popout_list() {
            self.popout_sync_theme(&p);
            self.popout_wallpaper(&p, false);
            self.popout_header(&p);
            let unread = p.st.borrow().row.as_ref().map_or(0, |r| r.unread);
            let since = p.st.borrow().last_mark.map(|t| t.elapsed());
            let foreground = is_foreground(&p.win);
            if should_mark_read(foreground, unread, since) {
                self.popout_mark_read(&p);
            }
            // The main window may have changed the draft while this one was not in use.
            if !foreground {
                self.popout_draft_pull(&p);
            }
        }
        if self.popouts.reg.borrow().len() == 0 {
            self.popouts.tick.stop();
        }
    }

    pub(super) fn popouts_note(&self, profile_id: &str, note: Note) {
        let mine: Vec<Rc<Popout>> = self.popout_list().into_iter().filter(|p| p.key.profile == profile_id).collect();
        if mine.is_empty() {
            return;
        }
        for p in mine {
            match &note {
                Note::Messages(chat) if *chat == p.key.chat => self.popout_reload(&p),
                Note::MediaReady { chat, id, path } if *chat == p.key.chat => self.popout_media_ready(&p, id, path),
                Note::MediaFailed { chat, id } if *chat == p.key.chat => {
                    {
                        let mut st = p.st.borrow_mut();
                        st.downloading.remove(id);
                        st.after_download.remove(id);
                        st.failed.insert(id.clone());
                    }
                    self.popout_rebuild(&p);
                }
                Note::Header => self.popout_header(&p),
                Note::Chats => {
                    self.popout_refresh_row(&p);
                    self.popout_header(&p);
                }
                _ => {}
            }
        }
    }

    /// The members of a group arrived: windows typing a mention there update their list.
    pub(super) fn popouts_members(&self, profile: &str, jid: &str) {
        for p in self.popout_list() {
            if p.key.profile == profile && p.mentions.borrow().typing_chat() == Some(jid) {
                App::pop_mention_refresh(&p.mentions, &p.win, profile);
            }
        }
    }

    /// A wallpaper file of the profile changed on disk (the blurred copy is ready).
    pub(super) fn popouts_wallpaper_changed(&self, profile: &str) {
        for p in self.popout_list().iter().filter(|p| p.key.profile == profile) {
            self.popout_wallpaper(p, true);
        }
    }

    /// Shows the chat's wallpaper (its own or the profile's) behind the messages;
    /// unless `force`, only when the preference or the light/dark mode changed.
    fn popout_wallpaper(&self, p: &Popout, force: bool) {
        let Some(profile) = self.find(&p.key.profile) else { return };
        let dark = p.win.global::<Theme>().get_dark();
        let pref = Self::wallpaper_pref_of(&profile, &p.key.chat);
        let now = Some((pref, dark));
        if !force && p.st.borrow().wallpaper == now {
            return;
        }
        self.wallpaper_look(&profile, Some(&p.key.chat), dark).apply(&p.win.global::<AppearanceState>());
        p.st.borrow_mut().wallpaper = now;
    }

    /// A full-size picture finished decoding on the pop-out pool.
    pub(super) fn popouts_decoded(&self, done: DecodeDone) {
        let DecodeDone::Media { path, buf } = done else { return };
        // Nothing asked for it any more (the windows closed or the app locked meanwhile).
        if !self.popouts.decoding.borrow_mut().remove(&path) {
            return;
        }
        let size = buf.as_ref().map_or(0, |b| {
            usize::try_from(b.width()).unwrap_or(0).saturating_mul(usize::try_from(b.height()).unwrap_or(0)).saturating_mul(4)
        });
        self.popouts.images.borrow_mut().insert(path.clone(), buf.map(Image::from_rgba8), size);
        for p in self.popout_list() {
            let shown = p.st.borrow().msgs.iter().any(|m| m.local_path.as_deref() == Some(path.as_str()));
            if shown {
                self.popout_rebuild(&p);
            }
        }
    }

    fn popout_pool(&self) -> &DecodePool {
        self.popouts.pool.get_or_init(|| {
            DecodePool::new(1, |done| {
                let queued = slint::invoke_from_event_loop(move || with_app(|a| a.popouts_decoded(done)));
                if let Err(e) = queued {
                    log::debug!("decoded pop-out image not delivered: {e}");
                }
            })
        })
    }

    /// The full-size picture of a row: from the pop-out cache, else from the
    /// main window's decode cache when it has it, else queued for decoding.
    fn popout_full(&self, profile: &ProfileState, m: &MessageRow) -> Option<Image> {
        let path = m.local_path.as_deref()?;
        if let Some(v) = self.popouts.images.borrow().get(path) {
            return v;
        }
        if let Ok(st) = profile.st.try_borrow() {
            if let Some(v) = st.media.get(&format!("f:{path}")) {
                return v.clone();
            }
        }
        if self.popouts.decoding.borrow_mut().insert(path.to_string()) {
            let job = DecodeJob::Media { path: PathBuf::from(path), max_dim: FULL_IMAGE_MAX_DIM };
            if !self.popout_pool().submit(job) {
                self.popouts.decoding.borrow_mut().remove(path);
            }
        }
        None
    }

    fn popout_refresh_row(&self, p: &Popout) {
        let Some(profile) = self.find(&p.key.profile) else { return };
        match profile.store.chat(&p.key.chat) {
            Ok(Some(row)) => p.st.borrow_mut().row = Some(row),
            Ok(None) => {}
            Err(e) => log::warn!("chat lookup failed: {e}"),
        }
    }

    fn popout_sync_theme(&self, p: &Popout) {
        let Some(main) = self.win() else { return };
        let (src, dst) = (main.global::<Theme>(), p.win.global::<Theme>());
        dst.set_dark(src.get_dark());
        dst.set_accent(src.get_accent());
        dst.set_badge(src.get_badge());
        dst.set_my_bubble(src.get_my_bubble());
        dst.set_my_quote(src.get_my_quote());
        dst.set_font_scale(src.get_font_scale());
    }

    fn popout_header(&self, p: &Popout) {
        let Some(profile) = self.find(&p.key.profile) else { return };
        let Some(row) = p.st.borrow().row.clone() else { return };
        let (avatar, subtitle) = {
            let mut st = profile.st.borrow_mut();
            let avatar = self.avatar(&profile, &mut st, &row.jid);
            let typing = st.typing.get(&row.jid).is_some_and(|t| t.elapsed() < TYPING_TTL);
            let subtitle = if typing {
                "typing…".to_string()
            } else if st.conn == ConnState::Connected {
                privacy::presence_subtitle(st.presence.get(&row.jid), Local::now())
            } else {
                conn_text(&st.conn)
            };
            (avatar, subtitle)
        };
        let name = self.header_title(&profile, &row);
        let w = &p.win;
        w.set_window_title(format!("{name} — WaClient").into());
        w.set_header_name(name.into());
        w.set_header_initials(models::initials(&row.display_name).into());
        w.set_header_tint(models::tint_for(&row.jid));
        w.set_header_has_avatar(avatar.is_some());
        w.set_header_avatar(avatar.unwrap_or_default());
        w.set_header_subtitle(subtitle.into());
        let blocked = ChatKind::from_jid(&row.jid) == ChatKind::Dm && self.blocked_matcher(&profile)(&row.jid);
        w.set_blocked(blocked);
        w.set_composer_blocked_text(self.gate_notice(&profile.id, &row.jid).into());
    }

    /// Forgets files deleted from disk, and looks up poll tallies and
    /// reactions of the loaded rows (see `App::prepare_rows`).
    fn popout_prepare(&self, profile: &ProfileState, p: &Popout) {
        let mut st = p.st.borrow_mut();
        let st = &mut *st;
        for m in &mut st.msgs {
            let Some(f) = m.local_path.as_deref() else { continue };
            if st.seen_paths.contains(f) {
                continue;
            }
            if std::path::Path::new(f).exists() {
                st.seen_paths.insert(f.to_string());
            } else {
                m.local_path = None;
            }
        }
        let missing: Vec<String> =
            st.msgs.iter().filter(|m| models::is_poll_row(m) && !st.polls.contains_key(&m.id)).map(|m| m.id.clone()).collect();
        for id in missing {
            match profile.store.poll_for(&p.key.chat, &id) {
                Ok(Some(view)) => {
                    st.polls.insert(id, poll_view_model(&view));
                }
                Ok(None) => {}
                Err(e) => log::warn!("poll lookup for {id} failed: {e}"),
            }
        }
        let ids: Vec<String> = st.msgs.iter().map(|m| m.id.clone()).collect();
        st.reactions = match profile.store.reactions_for(&p.key.chat, &ids) {
            Ok(map) => map.into_iter().map(|(id, list)| (id, crate::emoji::aggregate_label(&list))).collect(),
            Err(e) => {
                log::warn!("reaction lookup failed: {e}");
                HashMap::new()
            }
        };
    }

    fn popout_rebuild(&self, p: &Popout) {
        let Some(profile) = self.find(&p.key.profile) else { return };
        self.popout_prepare(&profile, p);
        let now = Local::now();
        let mut items = Vec::new();
        {
            let mut st = p.st.borrow_mut();
            let st = &mut *st;
            let is_group = ChatKind::from_jid(&p.key.chat) == ChatKind::Group;
            let full_ids = full_image_ids(&st.msgs, FULL_ROWS);
            let mut prev_ts = None;
            for m in &st.msgs {
                if models::new_day(prev_ts, m.ts) {
                    items.push(models::separator_item(m.ts, now));
                }
                prev_ts = Some(m.ts);
                let full = if full_ids.contains(&m.id) { self.popout_full(&profile, m) } else { None };
                let images = images_for(&mut st.media, m, full);
                let ui = RowUi {
                    downloading: st.downloading.contains(&m.id),
                    failed: st.failed.contains(&m.id),
                    play: None,
                    poll: st.polls.get(&m.id),
                    reactions: st.reactions.get(&m.id).map_or(&[], Vec::as_slice),
                };
                items.push(models::msg_item(m, is_group, images, ui));
            }
            st.guard_until = Instant::now() + LAYOUT_GUARD;
        }
        p.model.set_vec(items);
        p.win.set_new_below(false);
    }

    /// A live change: reloads the newest page, keeping the scroll position
    /// unless the user was at the bottom.
    fn popout_reload(&self, p: &Rc<Popout>) {
        let Some(profile) = self.find(&p.key.profile) else { return };
        let stick = p.win.invoke_at_bottom() || p.st.borrow().force_bottom;
        let limit = p.st.borrow().limit.max(PAGE);
        let Some(page) = self.ok("load messages", profile.store.messages_page(&p.key.chat, None, limit as u32)) else {
            return;
        };
        let last_incoming = page.last().is_some_and(|m| !m.from_me);
        {
            let mut st = p.st.borrow_mut();
            st.exhausted = page.len() < limit;
            st.msgs = page;
            st.force_bottom = false;
            st.polls.clear();
        }
        self.popout_rebuild(p);
        if stick {
            self.popout_fix(p, Fix::Bottom);
        }
        if last_incoming && is_foreground(&p.win) {
            self.popout_mark_read(p);
        }
    }

    fn popout_jump_latest(&self, p: &Rc<Popout>) {
        self.popout_reload(p);
        self.popout_fix(p, Fix::Bottom);
    }

    fn popout_load_older(&self, p: &Rc<Popout>, fill: bool) {
        let Some(profile) = self.find(&p.key.profile) else { return };
        let before = {
            let mut st = p.st.borrow_mut();
            if st.exhausted || (!fill && Instant::now() < st.guard_until) {
                return;
            }
            let Some(first) = st.msgs.first() else { return };
            let before = (first.ts, first.id.clone());
            // One scroll load at a time (see `paging`).
            if !fill && !st.pager.request(false) {
                return;
            }
            before
        };
        let page = match profile.store.messages_page_before(&p.key.chat, Some(before), PAGE as u32) {
            Ok(page) => page,
            Err(e) => {
                log::error!("load older messages: {e}");
                let mut st = p.st.borrow_mut();
                st.guard_until = Instant::now() + Duration::from_secs(5);
                st.pager.cancel();
                return;
            }
        };
        let off = p.win.invoke_scroll_y();
        let rows_before = p.model.row_count();
        let seam_continues_day;
        {
            let mut st = p.st.borrow_mut();
            if page.len() < PAGE {
                st.exhausted = true;
            }
            if page.is_empty() {
                st.pager.cancel();
                return;
            }
            st.limit += page.len();
            seam_continues_day = match (page.last(), st.msgs.first()) {
                (Some(newest), Some(old_first)) => !models::new_day(Some(newest.ts), old_first.ts),
                _ => false,
            };
            let mut page = page;
            page.append(&mut st.msgs);
            st.msgs = page;
        }
        self.popout_rebuild(p);
        if fill {
            p.st.borrow_mut().pager.cancel();
            self.popout_fix(p, Fix::Bottom);
            return;
        }
        // Pin the row that was first before to where it was on screen (see `paging`).
        let rows_after = p.model.row_count();
        let idx = paging::anchor_index(rows_before, rows_after, seam_continues_day);
        p.win.invoke_anchor_begin(idx as i32, rows_after as i32, off);
        let weak: Weak<Popout> = Rc::downgrade(p);
        Timer::single_shot(paging::SETTLE, move || {
            if let Some(p) = weak.upgrade() {
                with_app(|a| a.popout_older_settled(&p));
            }
        });
    }

    /// The anchored row has settled: release the view and carry on with the next
    /// page when it is still near the top.
    fn popout_older_settled(&self, p: &Rc<Popout>) {
        p.win.invoke_anchor_end();
        let again = {
            let mut st = p.st.borrow_mut();
            let exhausted = st.exhausted;
            let again = st.pager.settled(p.win.invoke_scroll_y(), exhausted);
            if again {
                st.pager.cancel();
            }
            again
        };
        if again {
            self.popout_load_older(p, false);
        }
    }

    /// The list only knows its real height after layout: re-applies the
    /// scroll correction until the height is stable (see `App::schedule_fix`).
    fn popout_fix(&self, p: &Rc<Popout>, fix: Fix) {
        *p.fix.borrow_mut() = Some(FixRun::new(fix));
        let weak: Weak<Popout> = Rc::downgrade(p);
        p.fix_timer.start(TimerMode::Repeated, FIX_TICK, move || {
            if let Some(p) = weak.upgrade() {
                with_app(|a| a.popout_fix_tick(&p));
            }
        });
    }

    fn popout_fix_tick(&self, p: &Rc<Popout>) {
        let h = p.win.invoke_content_h();
        let done = {
            let mut guard = p.fix.borrow_mut();
            let Some(run) = guard.as_mut() else {
                p.fix_timer.stop();
                return;
            };
            match run.fix {
                Fix::Bottom => p.win.invoke_scroll_to_bottom(),
            }
            run.settled(h)
        };
        if done {
            p.fix_timer.stop();
            *p.fix.borrow_mut() = None;
        }
    }

    fn popout_mark_read(&self, p: &Popout) {
        let Some(profile) = self.find(&p.key.profile) else { return };
        p.st.borrow_mut().last_mark = Some(Instant::now());
        (profile.send)(Command::MarkChatRead { chat_jid: p.key.chat.clone() });
    }

    // ---- composer ----------------------------------------------------------

    /// Whether the composer of the chat accepts a message from us now.
    fn popout_may_send(&self, profile: &ProfileState, chat: &str) -> bool {
        !(ChatKind::from_jid(chat) == ChatKind::Dm && self.blocked_matcher(profile)(chat))
            && self.gate_notice(&profile.id, chat).is_empty()
    }

    fn popout_send(&self, p: &Rc<Popout>, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let Some(profile) = self.find(&p.key.profile) else { return };
        if !self.popout_may_send(&profile, &p.key.chat) {
            return;
        }
        let quoted = {
            let mut st = p.st.borrow_mut();
            st.force_bottom = true;
            st.reply.take().map(|r| r.0)
        };
        let mentions = self.mentions_for_send_in(&profile.id, &p.key.chat, text);
        (profile.send)(Command::SendText { chat_jid: p.key.chat.clone(), text: text.to_string(), quoted_id: quoted, mentions });
        self.popout_stop_typing(p);
        App::pop_mention_close(&p.mentions, &p.win);
        // The view empties the composer once this returns; undo history and the draft go with it.
        p.win.invoke_clear_composer();
        self.draft_sent_in(&profile, &p.key.chat);
        p.st.borrow_mut().draft_known = None;
        self.popout_sync_reply(p);
    }

    /// The composer text changed: saves the shared draft and tells the peer we are typing.
    fn popout_edited(&self, p: &Rc<Popout>, text: &str) {
        self.popout_draft_touch(p);
        let Some(profile) = self.find(&p.key.profile) else { return };
        let act = p.st.borrow_mut().typing.edit(text.is_empty(), Instant::now());
        if let Some(composing) = act {
            (profile.send)(Command::SetTyping { chat_jid: p.key.chat.clone(), composing });
        }
        if text.is_empty() {
            p.typing_timer.stop();
        } else {
            let weak = Rc::downgrade(p);
            p.typing_timer.start(TimerMode::SingleShot, TYPING_PAUSE, move || {
                if let Some(p) = weak.upgrade() {
                    with_app(|a| a.popout_stop_typing(&p));
                }
            });
        }
    }

    /// Ends "typing..." (pause, send, close); only says so when it had been announced.
    fn popout_stop_typing(&self, p: &Popout) {
        p.typing_timer.stop();
        let announced = p.st.borrow_mut().typing.stop();
        if let (Some(composing), Some(profile)) = (announced, self.find(&p.key.profile)) {
            (profile.send)(Command::SetTyping { chat_jid: p.key.chat.clone(), composing });
        }
    }

    /// Saves the composer as the chat's draft (shared with the main window).
    fn popout_draft_touch(&self, p: &Popout) {
        let Some(profile) = self.find(&p.key.profile) else { return };
        let text = p.win.invoke_composer_text().to_string();
        let (reply, mut known) = {
            let st = p.st.borrow();
            (st.reply.clone(), st.draft_known.clone())
        };
        self.draft_commit(&profile, &p.key.chat, &text, reply, &mut known);
        p.st.borrow_mut().draft_known = known;
    }

    /// When the main window changed the draft and this composer was not edited
    /// since, the composer shows the new draft.
    fn popout_draft_pull(&self, p: &Popout) {
        let Some(profile) = self.find(&p.key.profile) else { return };
        let stored = self.draft_get(&profile, &p.key.chat);
        let text = p.win.invoke_composer_text().to_string();
        let (current, known) = {
            let st = p.st.borrow();
            (draft_content(&text, st.reply.as_ref()), st.draft_known.clone())
        };
        if draft_step(stored.as_ref(), known.as_ref(), current.as_ref()) != DraftStep::Adopt {
            return;
        }
        {
            let mut st = p.st.borrow_mut();
            st.reply = stored.as_ref().and_then(|d| d.1.clone());
            st.draft_known.clone_from(&stored);
        }
        match &stored {
            Some((text, _)) => p.win.invoke_set_composer(text.as_str().into(), i32::try_from(text.len()).unwrap_or(i32::MAX)),
            None => p.win.invoke_clear_composer(),
        }
        self.popout_sync_reply(p);
    }

    fn popout_mention_edited(&self, p: &Rc<Popout>, text: &str, cursor: usize) {
        let Some(profile) = self.find(&p.key.profile) else { return };
        self.pop_mention_edited(&p.mentions, &p.win, &profile, &p.key.chat, text, cursor);
    }

    fn popout_mention_pick(&self, p: &Rc<Popout>, index: usize) {
        if let Some(new) = App::pop_mention_pick(&p.mentions, &p.win, &p.key.profile, &p.key.chat, index) {
            self.popout_edited(p, &new);
        }
    }

    // ---- stickers ------------------------------------------------------------

    fn popout_stickers_refresh(&self, p: &Rc<Popout>) {
        let Some(profile) = self.find(&p.key.profile) else { return };
        let generation = p.stickers.next_generation();
        let g = p.win.global::<StickerState>();
        g.set_loading(true);
        let key = p.key.clone();
        let started = sticker_ui::load_async(profile.store.clone(), move |loaded| {
            let queued = slint::invoke_from_event_loop(move || {
                with_app(|a| a.popout_stickers_loaded(&key, generation, loaded));
            });
            if let Err(e) = queued {
                log::error!("could not deliver the stickers to the UI: {e}");
            }
        });
        if !started {
            g.set_loading(false);
        }
    }

    fn popout_stickers_loaded(&self, key: &Key, generation: u64, loaded: Result<sticker_ui::Loaded, String>) {
        let Some(p) = self.popouts.reg.borrow().get(key).cloned() else { return };
        if !p.stickers.is_current(generation) {
            return;
        }
        p.win.global::<StickerState>().set_loading(false);
        p.stickers.show_loaded(loaded);
    }

    /// Sends a grid sticker to the window's chat (as a reply while the reply bar is up).
    fn popout_stickers_send(&self, p: &Rc<Popout>, from_favs: bool, index: i32) {
        let Some(profile) = self.find(&p.key.profile) else { return };
        if !self.popout_may_send(&profile, &p.key.chat) {
            return;
        }
        let quoted = p.st.borrow().reply.as_ref().map(|r| r.0.clone());
        let Some(cmd) = p.stickers.send_command(from_favs, index, &p.key.chat, quoted) else { return };
        {
            let mut st = p.st.borrow_mut();
            st.force_bottom = true;
            st.reply = None;
        }
        (profile.send)(cmd);
        self.popout_sync_reply(p);
    }

    fn popout_stickers_toggle_fav(&self, p: &Rc<Popout>, from_favs: bool, index: i32) {
        let Some(profile) = self.find(&p.key.profile) else { return };
        if p.stickers.toggle_fav(&profile.store, from_favs, index) {
            self.popout_stickers_refresh(p);
        }
    }

    fn popout_reply(&self, p: &Rc<Popout>, id: &str) {
        {
            let mut st = p.st.borrow_mut();
            let Some(m) = st.msgs.iter().find(|m| m.id == id && !m.revoked) else { return };
            let sender = if m.from_me {
                "You".to_string()
            } else {
                m.sender_name.clone().unwrap_or_else(|| m.sender_jid.split('@').next().unwrap_or("").to_string())
            };
            let reply = (m.id.clone(), sender, models::reply_preview(m));
            st.reply = Some(reply);
        }
        self.popout_sync_reply(p);
        p.win.invoke_focus_composer();
    }

    fn popout_sync_reply(&self, p: &Popout) {
        {
            let st = p.st.borrow();
            match &st.reply {
                Some((_, sender, text)) => {
                    p.win.set_has_reply(true);
                    p.win.set_reply_sender(sender.as_str().into());
                    p.win.set_reply_text(text.as_str().into());
                }
                None => {
                    p.win.set_has_reply(false);
                    p.win.set_reply_sender("".into());
                    p.win.set_reply_text("".into());
                }
            }
        }
        self.popout_draft_touch(p);
    }

    /// Inserts at the composer's cursor; the picker stays open.
    fn popout_emoji_insert(&self, p: &Rc<Popout>, e: &str) {
        let text = p.win.invoke_composer_text().to_string();
        let mut at = usize::try_from(p.win.invoke_composer_cursor()).unwrap_or(text.len()).min(text.len());
        while !text.is_char_boundary(at) {
            at -= 1;
        }
        let new = format!("{}{e}{}", &text[..at], &text[at..]);
        let cursor = i32::try_from(at + e.len()).unwrap_or(i32::MAX);
        p.win.invoke_set_composer(new.as_str().into(), cursor);
    }

    fn popout_unblock(&self, p: &Rc<Popout>) {
        let Some(profile) = self.find(&p.key.profile) else { return };
        // The demo has no engine, so there is nothing to unblock there.
        if self.persist {
            (profile.send)(Command::SetBlocked { jid: p.key.chat.clone(), blocked: false });
        }
    }

    // ---- message actions -----------------------------------------------------

    fn popout_react(&self, p: &Rc<Popout>, id: &str, emoji: &str) {
        if emoji.is_empty() {
            return;
        }
        let Some(profile) = self.find(&p.key.profile) else { return };
        let remove = {
            let st = p.st.borrow();
            if !st.msgs.iter().any(|m| m.id == id && !m.revoked) {
                return;
            }
            st.reactions.get(id).is_some_and(|c| c.iter().any(|c| c.mine && c.emoji == emoji))
        };
        let wire = if remove { String::new() } else { emoji.to_string() };
        (profile.send)(Command::SendReaction { chat_jid: p.key.chat.clone(), id: id.to_string(), emoji: wire });
    }

    fn popout_download(&self, p: &Rc<Popout>, id: &str) {
        let Some(profile) = self.find(&p.key.profile) else { return };
        {
            let mut st = p.st.borrow_mut();
            if !st.downloading.insert(id.to_string()) {
                return;
            }
            st.failed.remove(id);
        }
        (profile.send)(Command::DownloadMedia { chat_jid: p.key.chat.clone(), id: id.to_string() });
        self.popout_rebuild(p);
    }

    /// Click on media: opens it with the system app, downloading first when needed.
    fn popout_open_media(&self, p: &Rc<Popout>, id: &str) {
        let local = match p.st.borrow().msgs.iter().find(|m| m.id == id) {
            Some(m) => m.local_path.clone(),
            None => return,
        };
        match local {
            Some(path) => self.open_path(&path),
            None => {
                p.st.borrow_mut().after_download.insert(id.to_string());
                self.popout_download(p, id);
            }
        }
    }

    fn popout_media_ready(&self, p: &Rc<Popout>, id: &str, path: &std::path::Path) {
        let (follow, local) = {
            let mut st = p.st.borrow_mut();
            st.downloading.remove(id);
            st.failed.remove(id);
            let follow = st.after_download.remove(id);
            let mut local = None;
            let mut known = None;
            if let Some(m) = st.msgs.iter_mut().find(|m| m.id == id) {
                if m.local_path.is_none() {
                    m.local_path = Some(path.to_string_lossy().into_owned());
                }
                local = m.local_path.clone();
                known.clone_from(&local);
            }
            if let Some(f) = known {
                st.seen_paths.insert(f);
            }
            (follow, local)
        };
        self.popout_rebuild(p);
        if let (true, Some(local)) = (follow, local) {
            self.open_path(&local);
        }
    }

    fn popout_reveal(&self, p: &Rc<Popout>, id: &str) {
        let local = p.st.borrow().msgs.iter().find(|m| m.id == id).and_then(|m| m.local_path.clone());
        if let Some(path) = local {
            self.launch_checked(&path, true);
        }
    }

    fn popout_copy(&self, p: &Rc<Popout>, id: &str) {
        let text = {
            let st = p.st.borrow();
            st.msgs.iter().find(|m| m.id == id && !m.revoked && m.kind != MessageKind::Other).and_then(|m| m.text.clone())
        };
        if let Some(text) = text.filter(|t| !t.is_empty()) {
            self.copy_text(&text);
        }
    }

    fn popout_link(&self, _p: &Rc<Popout>, url: &str) {
        // Group invite links open a dialog in the main window of the active profile.
        if super::group_new_ui::is_invite_link(url) {
            log::debug!("invite links are not opened from a pop-out window");
            return;
        }
        self.open_link(url);
    }
}

#[cfg(debug_assertions)]
impl App {
    /// Demo (`WACLIENT_DEMO_UI=popout|popout-send|popout-lock|popout-reopen|
    /// popout-wallpaper|popout-mention|popout-stickers|popout-draft`): opens
    /// Alice's chat (the group for `-mention`) in a pop-out; `-send` then
    /// replies and sends through the same code as the window's callbacks,
    /// `-lock` locks the app (the pop-out closes), `-reopen` also unlocks it
    /// (the pop-out comes back), `-wallpaper` / `-mention` / `-stickers` show
    /// the wallpaper, the mention list and the Stickers tab, `-draft` types in
    /// the pop-out and reads the draft back in the main window.
    pub(super) fn demo_popout(&self, what: &str) {
        const ALICE: &str = "111@s.whatsapp.net";
        const GROUP: &str = "999@g.us";
        if matches!(what, "popout-wallpaper" | "popout-mention" | "popout-stickers") {
            self.demo_appearance("wallpaper-blur");
        }
        let chat = if what == "popout-mention" {
            self.demo_composer("group");
            GROUP
        } else {
            ALICE
        };
        self.popout_open(chat);
        let what = what.to_string();
        Timer::single_shot(Duration::from_millis(1500), move || {
            with_app(|a| {
                let key = Key { profile: a.p().id.clone(), chat: chat.to_string() };
                let Some(p) = a.popouts.reg.borrow().get(&key).cloned() else { return };
                match what.as_str() {
                    "popout-send" => {
                        let target = p.st.borrow().msgs.iter().rev().find(|m| !m.from_me).map(|m| m.id.clone());
                        if let Some(id) = target {
                            a.popout_reply(&p, &id);
                        }
                        a.popout_send(&p, "Sent from the pop-out window");
                    }
                    "popout-lock" => a.demo_lock(),
                    "popout-reopen" => {
                        a.demo_lock();
                        Timer::single_shot(Duration::from_millis(2000), || with_app(App::demo_unlock));
                    }
                    "popout-mention" => a.demo_popout_type(&p, "Hi @Al"),
                    "popout-stickers" => {
                        p.win.global::<StickerState>().set_tab(1);
                        a.popout_stickers_refresh(&p);
                        Timer::single_shot(Duration::from_millis(1500), move || p.win.invoke_show_emoji_picker());
                    }
                    "popout-draft" => {
                        a.demo_popout_type(&p, "Typed in the pop-out");
                        a.demo_draft_probe(ALICE);
                    }
                    _ => {}
                }
            });
        });
    }

    /// Types `text` into a pop-out the way the composer would (no focus needed).
    fn demo_popout_type(&self, p: &Rc<Popout>, text: &str) {
        p.win.invoke_set_composer(text.into(), i32::try_from(text.len()).unwrap_or(i32::MAX));
        self.popout_edited(p, text);
        self.popout_mention_edited(p, text, text.len());
    }

    /// Switches the main window away from `chat` and back, then logs what its composer holds.
    fn demo_draft_probe(&self, chat: &str) {
        self.select_chat("222@s.whatsapp.net");
        self.select_chat(chat);
        if let Some(win) = self.win() {
            log::warn!("demo: main composer after switching back: {:?}", win.invoke_composer_text().as_str());
        }
    }
}

/// Size and place of a window to put it back later; none while minimized.
fn geometry_of(win: &ChatWindow) -> Option<Geometry> {
    let w = win.window();
    if w.is_minimized() {
        return None;
    }
    Some(Geometry { size: w.size().to_logical(w.scale_factor()), pos: usable_position(w.position()) })
}

fn bring_to_front(win: &ChatWindow) {
    if let Err(e) = win.show() {
        log::error!("could not show the chat window: {e}");
        return;
    }
    win.window().set_minimized(false);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(profile: &str, chat: &str) -> Key {
        Key { profile: profile.into(), chat: chat.into() }
    }

    #[test]
    fn a_chat_gets_one_window_and_refocuses() {
        let mut reg: Registry<u32> = Registry::new();
        let k = key("p1", "111@s.whatsapp.net");
        assert_eq!(reg.plan(&k, false), Plan::Create);
        reg.insert(k.clone(), 1);
        assert_eq!(reg.plan(&k, false), Plan::Refocus);
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn the_same_chat_of_another_profile_is_a_different_window() {
        let mut reg: Registry<u32> = Registry::new();
        reg.insert(key("p1", "111@s.whatsapp.net"), 1);
        assert_eq!(reg.plan(&key("p2", "111@s.whatsapp.net"), false), Plan::Create);
    }

    #[test]
    fn the_cap_refuses_a_fifth_window_but_still_refocuses() {
        let mut reg: Registry<usize> = Registry::new();
        for i in 0..MAX_POPOUTS {
            reg.insert(key("p", &format!("{i}@s.whatsapp.net")), i);
        }
        assert_eq!(reg.plan(&key("p", "99@s.whatsapp.net"), false), Plan::Full);
        assert_eq!(reg.plan(&key("p", "2@s.whatsapp.net"), false), Plan::Refocus);
    }

    #[test]
    fn closing_frees_a_slot() {
        let mut reg: Registry<usize> = Registry::new();
        for i in 0..MAX_POPOUTS {
            reg.insert(key("p", &format!("{i}@s.whatsapp.net")), i);
        }
        assert_eq!(reg.remove(&key("p", "1@s.whatsapp.net")), Some(1));
        assert_eq!(reg.remove(&key("p", "1@s.whatsapp.net")), None);
        assert_eq!(reg.plan(&key("p", "99@s.whatsapp.net"), false), Plan::Create);
    }

    #[test]
    fn a_locked_app_opens_nothing_not_even_a_refocus() {
        let mut reg: Registry<u32> = Registry::new();
        let k = key("p1", "111@s.whatsapp.net");
        assert_eq!(reg.plan(&k, true), Plan::Locked);
        reg.insert(k.clone(), 1);
        assert_eq!(reg.plan(&k, true), Plan::Locked);
    }

    #[test]
    fn locking_takes_every_window() {
        let mut reg: Registry<usize> = Registry::new();
        for i in 0..3 {
            reg.insert(key("p", &format!("{i}@s.whatsapp.net")), i);
        }
        let mut all = reg.drain_all();
        all.sort_unstable();
        assert_eq!(all, vec![0, 1, 2]);
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn removing_a_profile_takes_only_its_windows() {
        let mut reg: Registry<u32> = Registry::new();
        reg.insert(key("a", "1@s.whatsapp.net"), 1);
        reg.insert(key("b", "1@s.whatsapp.net"), 2);
        reg.insert(key("a", "2@s.whatsapp.net"), 3);
        let mut gone = reg.drain_profile("a");
        gone.sort_unstable();
        assert_eq!(gone, vec![1, 3]);
        assert_eq!(reg.len(), 1);
        assert!(reg.get(&key("b", "1@s.whatsapp.net")).is_some());
    }

    #[test]
    fn events_map_to_notes() {
        let chat = "111@s.whatsapp.net".to_string();
        assert!(matches!(note_for(&UiEvent::MessagesChanged { chat_jid: chat.clone() }), Some(Note::Messages(c)) if c == chat));
        assert!(matches!(note_for(&UiEvent::Connection(ConnState::Connected)), Some(Note::Header)));
        assert!(matches!(note_for(&UiEvent::ChatsChanged), Some(Note::Chats)));
        assert!(note_for(&UiEvent::StatusesChanged).is_none());
    }

    #[test]
    fn a_focused_window_marks_read_at_most_every_few_seconds() {
        assert!(should_mark_read(true, 2, None));
        assert!(!should_mark_read(false, 2, None));
        assert!(!should_mark_read(true, 0, None));
        assert!(!should_mark_read(true, 2, Some(Duration::from_secs(1))));
        assert!(should_mark_read(true, 2, Some(MARK_READ_EVERY)));
    }

    fn saved(chat: &str) -> Saved {
        Saved { key: key("p", chat), geom: None }
    }

    #[test]
    fn windows_come_back_only_after_a_successful_unlock() {
        let mut list = ReopenList::default();
        list.remember(vec![saved("1@s.whatsapp.net"), saved("2@g.us")]);
        // Still locked (a wrong passcode, a second look): nothing is handed out and nothing is lost.
        assert!(list.take(true).is_empty());
        assert!(list.take(true).is_empty());
        let back = list.take(false);
        assert_eq!(back, vec![saved("1@s.whatsapp.net"), saved("2@g.us")]);
        // Handed out once: a later lock starts from its own list.
        assert!(list.take(false).is_empty());
    }

    #[test]
    fn locking_again_replaces_the_remembered_windows() {
        let mut list = ReopenList::default();
        list.remember(vec![saved("1@s.whatsapp.net")]);
        list.remember(vec![saved("2@s.whatsapp.net")]);
        assert_eq!(list.take(false), vec![saved("2@s.whatsapp.net")]);
        list.remember(Vec::new());
        assert!(list.take(false).is_empty());
    }

    #[test]
    fn at_most_the_window_cap_comes_back() {
        let mut list = ReopenList::default();
        list.remember((0..MAX_POPOUTS + 3).map(|i| saved(&format!("{i}@s.whatsapp.net"))).collect());
        let back = list.take(false);
        assert_eq!(back.len(), MAX_POPOUTS);
        assert_eq!(back[0], saved("0@s.whatsapp.net"));
    }

    #[test]
    fn the_registry_lists_windows_in_opening_order() {
        let mut reg: Registry<u32> = Registry::new();
        reg.insert(key("p", "b@s.whatsapp.net"), 1);
        reg.insert(key("p", "a@s.whatsapp.net"), 2);
        let chats: Vec<&str> = reg.iter().map(|(k, _)| k.chat.as_str()).collect();
        assert_eq!(chats, ["b@s.whatsapp.net", "a@s.whatsapp.net"]);
    }

    #[test]
    fn a_minimized_windows_position_is_not_remembered() {
        assert_eq!(usable_position(PhysicalPosition::new(120, 80)), Some(PhysicalPosition::new(120, 80)));
        assert_eq!(usable_position(PhysicalPosition::new(-32000, -32000)), None);
        // A window on a monitor to the left of the main one has a negative x.
        assert!(usable_position(PhysicalPosition::new(-1500, 40)).is_some());
    }

    #[test]
    fn typing_is_announced_once_then_every_resend_interval() {
        let t0 = Instant::now();
        let mut t = TypingState::default();
        assert_eq!(t.edit(false, t0), Some(true));
        assert_eq!(t.edit(false, t0 + Duration::from_secs(1)), None);
        assert_eq!(t.edit(false, t0 + super::super::TYPING_RESEND), Some(true));
    }

    #[test]
    fn emptying_the_composer_or_pausing_ends_typing_only_if_announced() {
        let t0 = Instant::now();
        let mut t = TypingState::default();
        // Nothing was announced: nothing to retract.
        assert_eq!(t.edit(true, t0), None);
        assert_eq!(t.stop(), None);
        assert_eq!(t.edit(false, t0), Some(true));
        assert_eq!(t.edit(true, t0), Some(false));
        assert_eq!(t.edit(false, t0), Some(true));
        assert_eq!(t.stop(), Some(false));
        // After a stop the next keystroke announces again right away.
        assert_eq!(t.edit(false, t0), Some(true));
    }

    #[test]
    fn the_cache_drops_the_oldest_pictures_over_budget() {
        let mut c: ByteCache<u8> = ByteCache::new(100);
        c.insert("a".into(), 1, 40);
        c.insert("b".into(), 2, 40);
        c.insert("c".into(), 3, 40);
        assert_eq!(c.get("a"), None);
        assert_eq!(c.get("b"), Some(2));
        assert_eq!(c.get("c"), Some(3));
        assert_eq!(c.bytes, 80);
        // Replacing an entry does not count it twice.
        c.insert("b".into(), 9, 40);
        assert_eq!((c.get("b"), c.bytes), (Some(9), 80));
        // One picture over budget on its own still stays.
        c.insert("huge".into(), 7, 500);
        assert_eq!(c.get("huge"), Some(7));
        assert_eq!(c.get("b"), None);
        c.clear();
        assert_eq!((c.get("huge"), c.bytes), (None, 0));
    }

    fn row(id: &str, kind: MessageKind, path: Option<&str>) -> MessageRow {
        MessageRow {
            chat_jid: "c@s.whatsapp.net".into(),
            id: id.into(),
            kind,
            local_path: path.map(String::from),
            ..Default::default()
        }
    }

    #[test]
    fn only_the_newest_picture_rows_are_decoded_full_size() {
        let msgs = vec![
            row("old", MessageKind::Image, Some("/a")),
            row("text", MessageKind::Text, None),
            row("nofile", MessageKind::Image, None),
            row("sticker", MessageKind::Sticker, Some("/b")),
            row("new", MessageKind::Image, Some("/c")),
        ];
        let ids = full_image_ids(&msgs, 2);
        assert_eq!(ids.len(), 2);
        assert!(ids.contains("new") && ids.contains("sticker"));
        assert!(full_image_ids(&msgs, 10).contains("old"));
    }

    #[test]
    fn the_same_draft_is_not_saved_twice() {
        let mine: Option<Content> = Some(("hi".into(), None));
        // Typing and the stored draft agree: nothing to write.
        assert_eq!(draft_step(mine.as_ref(), mine.as_ref(), mine.as_ref()), DraftStep::InSync);
        assert_eq!(draft_step(None, None, None), DraftStep::InSync);
    }

    #[test]
    fn an_untouched_composer_never_overwrites_the_other_windows_draft() {
        let old: Option<Content> = Some(("old".into(), None));
        let newer: Option<Content> = Some(("typed elsewhere".into(), None));
        // Another window changed the draft; this composer still shows what it last synced.
        assert_eq!(draft_step(newer.as_ref(), old.as_ref(), old.as_ref()), DraftStep::Adopt);
        // The other window sent its message (draft gone): this composer must not bring it back.
        assert_eq!(draft_step(None, old.as_ref(), old.as_ref()), DraftStep::Adopt);
        // Once this composer is edited it wins.
        let edited: Option<Content> = Some(("old and more".into(), None));
        assert_eq!(draft_step(newer.as_ref(), old.as_ref(), edited.as_ref()), DraftStep::Write);
        // Emptying a composer that held the draft clears it.
        assert_eq!(draft_step(old.as_ref(), old.as_ref(), None), DraftStep::Write);
    }

    #[test]
    fn a_reply_target_is_part_of_the_shared_draft() {
        let reply = ("id1".to_string(), "Ann".to_string(), "hey".to_string());
        assert_eq!(draft_content("", None), None);
        assert_eq!(draft_content("  \n", None), None);
        assert_eq!(draft_content("", Some(&reply)), Some((String::new(), Some(reply.clone()))));
        let stored = draft_content("x", None);
        let with_reply = draft_content("x", Some(&reply));
        assert_eq!(draft_step(stored.as_ref(), stored.as_ref(), with_reply.as_ref()), DraftStep::Write);
    }

    #[test]
    fn the_scroll_fix_settles_after_the_height_stops_changing() {
        let mut run = FixRun::new(Fix::Bottom);
        assert!(!run.settled(100.0));
        assert!(!run.settled(300.0));
        let mut done = false;
        for _ in 0..FIX_STABLE_CHECKS {
            done = run.settled(300.0);
        }
        assert!(done);
    }
}
