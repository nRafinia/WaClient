//! "Open in new window": a chat popped out into its own top-level window with
//! the messages and the composer. The window belongs to the profile it was
//! opened from (a profile switch in the main window does not touch it), shows
//! the same rows as the main view (`models::msg_item`) from its own model and
//! sends through that profile's engine.
//!
//! v1 limits: at most [`MAX_POPOUTS`] windows, one per chat; text, reply,
//! reactions (view and toggle), media open/download, mark as read. Everything
//! else (message menu beyond Reply/Copy, polls voting, info panel, search,
//! translation, stickers, attachments, voice notes, mentions suggestions,
//! full-size inline images) stays in the main window. The windows are closed
//! when the app locks so no content stays visible.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use chrono::Local;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::{CloseRequestResponse, ComponentHandle, Image, LogicalSize, ModelRc, SharedString, Timer, TimerMode, VecModel};
use wa_engine::{Command, ConnState, UiEvent};
use wa_store::{ChatKind, ChatRow, MessageKind, MessageRow};

use super::composer::undo::UndoStack;
use super::{
    App, FIX_EPS, FIX_MAX, FIX_STABLE_CHECKS, FIX_TICK, LAYOUT_GUARD, PAGE, ProfileState, TYPING_TTL, conn_text,
    emoji_ui, find_chat, privacy, with_app,
};
use crate::emoji::{QUICK_REACTIONS, ReactionChip};
use crate::models::{self, MsgImages, RowUi};
use crate::status_view::{PollViewModel, poll_view_model};
use crate::{ChatWindow, EmojiRow, Mentions, MsgItem, ReactionConfig, TextDir, Theme, drop_paste};

/// Pop-out windows open at the same time.
pub(super) const MAX_POPOUTS: usize = 4;
/// How often headers, theme and the unread state of the windows are refreshed.
const POPOUT_TICK: Duration = Duration::from_secs(1);
/// A chat is marked read at most this often by a focused window.
const MARK_READ_EVERY: Duration = Duration::from_secs(3);

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
    Restore { y: f32, h: f32 },
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
}

pub(super) struct Popout {
    key: Key,
    win: ChatWindow,
    model: Rc<VecModel<MsgItem>>,
    emoji_rows: Rc<VecModel<EmojiRow>>,
    st: RefCell<PState>,
    fix_timer: Timer,
    fix: RefCell<Option<FixRun>>,
}

/// The pop-out windows of the app (a field of [`App`]).
pub(super) struct Popouts {
    reg: RefCell<Registry<Rc<Popout>>>,
    tick: Timer,
    /// Size of the window closed last; new windows start with it.
    size: Cell<Option<LogicalSize>>,
}

impl Popouts {
    pub(super) fn new() -> Self {
        Popouts { reg: RefCell::new(Registry::new()), tick: Timer::default(), size: Cell::new(None) }
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

fn images_for(cache: &mut HashMap<String, Option<Image>>, m: &MessageRow) -> MsgImages {
    let thumb = m.thumbnail.as_deref().and_then(|bytes| {
        cache.entry(format!("t:{}:{}", m.chat_jid, m.id)).or_insert_with(|| models::decode_image_bytes(bytes)).clone()
    });
    let link_thumb = m.link_thumb.as_deref().and_then(|bytes| {
        cache.entry(format!("l:{}:{}", m.chat_jid, m.id)).or_insert_with(|| models::decode_image_bytes(bytes)).clone()
    });
    MsgImages { thumb, full: None, link_thumb }
}

impl App {
    /// "Open in new window" for a chat of the active profile.
    pub(super) fn popout_open(&self, jid: &str) {
        let profile = self.p();
        self.popout_open_in(&profile, jid);
    }

    fn popout_open_in(&self, profile: &Rc<ProfileState>, jid: &str) {
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
            Plan::Create => self.popout_create(profile, key),
        }
    }

    fn popout_create(&self, profile: &Rc<ProfileState>, key: Key) {
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
        if let Some(size) = self.popouts.size.get() {
            win.window().set_size(size);
        }
        let exhausted = page.len() < PAGE;
        let unread = row.unread;
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
                reply: None,
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
            }),
            fix_timer: Timer::default(),
            fix: RefCell::new(None),
        });
        self.popout_wire(&p);
        self.popout_sync_theme(&p);
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

        // Composer helpers: formatting shortcuts, undo/redo and RTL Home/End,
        // with this window's own history.
        let undo = Rc::new(RefCell::new(UndoStack::default()));
        let m = w.global::<Mentions>();
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
            Self::popout_dispose(&p);
        }
    }

    fn popout_dispose(p: &Popout) {
        p.fix_timer.stop();
        if let Err(e) = p.win.hide() {
            log::warn!("could not hide the chat window: {e}");
        }
    }

    /// Closes every pop-out (the app locks or quits).
    pub(super) fn popouts_close_all(&self) {
        let all = self.popouts.reg.borrow_mut().drain_all();
        self.popouts.tick.stop();
        for p in &all {
            Self::popout_dispose(p);
        }
    }

    fn popout_list(&self) -> Vec<Rc<Popout>> {
        self.popouts.reg.borrow().values().into_iter().cloned().collect()
    }

    /// Once a second: closes windows of removed profiles, follows the main
    /// window's theme, refreshes the header (typing and "last seen" expire on
    /// their own) and marks the chat read while the window is focused.
    fn popouts_tick(&self) {
        if self.is_locked() {
            self.popouts_close_all();
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
            self.popout_header(&p);
            let unread = p.st.borrow().row.as_ref().map_or(0, |r| r.unread);
            let since = p.st.borrow().last_mark.map(|t| t.elapsed());
            if should_mark_read(is_foreground(&p.win), unread, since) {
                self.popout_mark_read(&p);
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
            let mut prev_ts = None;
            for m in &st.msgs {
                if models::new_day(prev_ts, m.ts) {
                    items.push(models::separator_item(m.ts, now));
                }
                prev_ts = Some(m.ts);
                let images = images_for(&mut st.media, m);
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
            let st = p.st.borrow();
            if st.exhausted || (!fill && Instant::now() < st.guard_until) {
                return;
            }
            let Some(first) = st.msgs.first() else { return };
            (first.ts, first.id.clone())
        };
        let page = match profile.store.messages_page_before(&p.key.chat, Some(before), PAGE as u32) {
            Ok(page) => page,
            Err(e) => {
                log::error!("load older messages: {e}");
                p.st.borrow_mut().guard_until = Instant::now() + Duration::from_secs(5);
                return;
            }
        };
        let (y, h) = (p.win.invoke_scroll_y(), p.win.invoke_content_h());
        {
            let mut st = p.st.borrow_mut();
            if page.len() < PAGE {
                st.exhausted = true;
            }
            if page.is_empty() {
                return;
            }
            st.limit += page.len();
            let mut page = page;
            page.append(&mut st.msgs);
            st.msgs = page;
        }
        self.popout_rebuild(p);
        self.popout_fix(p, if fill { Fix::Bottom } else { Fix::Restore { y, h } });
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
                Fix::Restore { y, h } => p.win.invoke_restore_scroll(y, h),
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

    fn popout_send(&self, p: &Rc<Popout>, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let Some(profile) = self.find(&p.key.profile) else { return };
        if (ChatKind::from_jid(&p.key.chat) == ChatKind::Dm && self.blocked_matcher(&profile)(&p.key.chat))
            || !self.gate_notice(&profile.id, &p.key.chat).is_empty()
        {
            return;
        }
        let quoted = {
            let mut st = p.st.borrow_mut();
            st.force_bottom = true;
            st.reply.take().map(|r| r.0)
        };
        (profile.send)(Command::SendText {
            chat_jid: p.key.chat.clone(),
            text: text.to_string(),
            quoted_id: quoted,
            mentions: Vec::new(),
        });
        // The view empties the composer once this returns; undo history goes with it.
        p.win.invoke_clear_composer();
        self.popout_sync_reply(p);
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
    /// Demo (`WACLIENT_DEMO_UI=popout|popout-send|popout-lock`): opens Alice's
    /// chat in a pop-out; `-send` then replies and sends through the same code
    /// as the window's callbacks, `-lock` locks the app (the pop-out closes).
    pub(super) fn demo_popout(&self, what: &str) {
        const ALICE: &str = "111@s.whatsapp.net";
        self.popout_open(ALICE);
        let what = what.to_string();
        Timer::single_shot(Duration::from_millis(1500), move || {
            with_app(|a| {
                let key = Key { profile: a.p().id.clone(), chat: ALICE.to_string() };
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
                    _ => {}
                }
            });
        });
    }
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
