//! Composer extras: @mentions in groups and per-chat drafts.
//!
//! Mentions are plain "@<digits>" text (what WhatsApp clients render as the
//! member's name); the matching JIDs travel in `SendText.mentions`. Drafts are
//! the unsent text and reply target of every chat, kept per profile, restored
//! when the chat is opened again and persisted to `drafts.json`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use slint::{ComponentHandle, Model, ModelRc, Timer, TimerMode, VecModel};
use wa_engine::{Command, ConnState, ContactInfo, GroupInfo, GroupMember};
use wa_store::ChatKind;

use super::contact_info::format_phone;
use super::{App, ProfileState, TYPING_RESEND, with_app};
use crate::models::{initials, tint_for};
use crate::{ChatWindow, MainWindow, MentionItem, Mentions, MsgOps, TextDir};

#[path = "undo.rs"]
pub(super) mod undo;

/// Group members are reloaded when older than this.
const MEMBERS_FRESH: Duration = Duration::from_secs(10 * 60);
/// A member request that got no answer is repeated after this long.
const ASK_AGAIN: Duration = Duration::from_secs(15);
/// Suggestions shown at once.
const MAX_SUGGESTIONS: usize = 6;
/// Longest "@query" that still counts as a mention being typed.
const MAX_QUERY_CHARS: usize = 30;
/// The unsent text is written to disk this long after the last change.
const SAVE_DELAY: Duration = Duration::from_secs(1);
/// Drafts kept per profile; the oldest are dropped first.
const MAX_DRAFTS: usize = 200;
const DRAFTS_FILE: &str = "drafts.json";

// ---- mentions: pure parts ----------------------------------------------------------

/// Whitespace and invisible Unicode format characters (category Cf: bidi marks
/// and isolates, joiners, Arabic letter mark) may precede an '@' that starts a word.
fn is_mention_boundary(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '\u{200C}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{061C}'
        )
}

/// The "@query" being typed at `cursor`: byte offset of the '@' and the text after it.
/// The '@' must start a word, so e-mail addresses do not trigger it.
fn mention_token(text: &str, cursor: usize) -> Option<(usize, String)> {
    let mut cursor = cursor.min(text.len());
    while !text.is_char_boundary(cursor) {
        cursor -= 1;
    }
    let before = &text[..cursor];
    let at = before.rfind('@')?;
    if before[..at].chars().next_back().is_some_and(|c| !is_mention_boundary(c)) {
        return None;
    }
    let query = &before[at + 1..];
    if query.chars().any(char::is_whitespace) || query.chars().count() > MAX_QUERY_CHARS {
        return None;
    }
    Some((at, query.to_string()))
}

/// A group member that can be mentioned.
#[derive(Debug, Clone, PartialEq)]
struct Candidate {
    jid: String,
    name: String,
    /// What follows the '@' in the text: the phone number, else the address user.
    digits: String,
    phone: String,
}

fn jid_user(jid: &str) -> &str {
    let user = jid.split('@').next().unwrap_or(jid);
    user.split(':').next().unwrap_or(user)
}

fn candidates(members: &[GroupMember]) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = members
        .iter()
        .filter(|m| !m.is_me)
        .map(|m| {
            let phone_digits: String = m
                .phone
                .as_deref()
                .unwrap_or_default()
                .chars()
                .filter(char::is_ascii_digit)
                .collect();
            let digits = if phone_digits.is_empty() { jid_user(&m.jid).to_string() } else { phone_digits };
            let phone = format_phone(&digits);
            let name = m
                .name
                .as_deref()
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .map_or_else(|| phone.clone(), str::to_string);
            Candidate { jid: m.jid.clone(), name, digits, phone }
        })
        .collect();
    out.sort_by_cached_key(|c| c.name.to_lowercase());
    out
}

/// Members matching what was typed after the '@': names that start with it
/// first, then names or numbers that contain it.
fn filter_candidates(all: &[Candidate], query: &str, limit: usize) -> Vec<Candidate> {
    let q = query.to_lowercase();
    let mut starts = Vec::new();
    let mut contains = Vec::new();
    for c in all {
        let name = c.name.to_lowercase();
        if q.is_empty() || name.starts_with(&q) {
            starts.push(c);
        } else if name.contains(&q) || c.digits.contains(&q) {
            contains.push(c);
        }
    }
    starts.into_iter().chain(contains).take(limit).cloned().collect()
}

/// Replaces the "@query" (from `start` to `cursor`) by "@<digits> "; returns the
/// new text and the cursor behind the inserted mention.
fn insert_mention(text: &str, start: usize, cursor: usize, digits: &str) -> (String, usize) {
    let insert = format!("@{digits} ");
    let new = format!("{}{insert}{}", &text[..start], &text[cursor..]);
    (new, start + insert.len())
}

/// The JIDs of the "@<digits>" tokens in `text` that `lookup` knows, once each.
fn mention_jids(text: &str, lookup: impl Fn(&str) -> Option<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (at, _) in text.match_indices('@') {
        if text[..at].chars().next_back().is_some_and(|c| !c.is_whitespace()) {
            continue;
        }
        let rest = &text[at + 1..];
        let digits_len = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        if digits_len == 0 || rest[digits_len..].chars().next().is_some_and(char::is_alphanumeric) {
            continue;
        }
        if let Some(jid) = lookup(&rest[..digits_len]) {
            if !out.contains(&jid) {
                out.push(jid);
            }
        }
    }
    out
}

// ---- drafts: pure parts and persistence --------------------------------------------

/// The reply target as the controller keeps it: (message id, sender, preview).
pub(super) type ReplyRef = (String, String, String);

/// What a composer holds as a draft: the unsent text and the reply target.
/// `None` stands for "no draft" (empty text, no reply).
pub(super) type Content = (String, Option<ReplyRef>);

pub(super) fn draft_content(text: &str, reply: Option<&ReplyRef>) -> Option<Content> {
    (!(text.trim().is_empty() && reply.is_none())).then(|| (text.to_string(), reply.cloned()))
}

/// What a composer (the main window's or a pop-out's) should do with the
/// draft of its chat. All of them share one draft per (profile, chat); each
/// remembers what it last synced (`known`) so a window that was not edited
/// never overwrites what another window wrote since.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum DraftStep {
    /// The stored draft already is what the composer shows: nothing to write.
    InSync,
    /// Another window changed the draft and this composer was not edited since:
    /// keep the stored draft (the composer shows it the next time it is restored).
    Adopt,
    Write,
}

pub(super) fn draft_step(stored: Option<&Content>, known: Option<&Content>, current: Option<&Content>) -> DraftStep {
    if stored == current {
        DraftStep::InSync
    } else if stored != known && current == known {
        DraftStep::Adopt
    } else {
        DraftStep::Write
    }
}

/// Whether a "typing..." update is due: the first edit, then every
/// `TYPING_RESEND` while typing goes on. Shared by the main composer and the pop-outs.
pub(super) fn typing_due(since_last: Option<Duration>) -> bool {
    since_last.is_none_or(|d| d >= TYPING_RESEND)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Draft {
    text: String,
    #[serde(default)]
    reply: Option<ReplyRef>,
    /// Unix seconds of the last change; decides which drafts are dropped at the cap.
    ts: i64,
}

#[derive(Default, Serialize, Deserialize)]
struct DraftFile {
    #[serde(default)]
    chats: HashMap<String, Draft>,
}

/// Drops the oldest drafts until at most `max` remain.
fn cap_drafts(map: &mut HashMap<String, Draft>, max: usize) {
    if map.len() <= max {
        return;
    }
    let mut order: Vec<(i64, String)> = map.iter().map(|(k, d)| (d.ts, k.clone())).collect();
    order.sort();
    for (_, key) in order.into_iter().take(map.len() - max) {
        map.remove(&key);
    }
}

fn drafts_path(dir: &Path) -> PathBuf {
    dir.join(DRAFTS_FILE)
}

/// A missing file yields no drafts; a corrupt one is moved aside.
fn load_drafts(dir: &Path) -> HashMap<String, Draft> {
    let path = drafts_path(dir);
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                log::warn!("reading {}: {e}", path.display());
            }
            return HashMap::new();
        }
    };
    match serde_json::from_slice::<DraftFile>(&bytes) {
        Ok(f) => {
            let mut chats = f.chats;
            cap_drafts(&mut chats, MAX_DRAFTS);
            chats
        }
        Err(e) => {
            log::warn!("corrupt {}: {e}; dropping the drafts", path.display());
            let bad = dir.join(format!("{DRAFTS_FILE}.bad"));
            if let Err(e) = fs::rename(&path, &bad) {
                log::warn!("backing up {}: {e}", path.display());
            }
            HashMap::new()
        }
    }
}

/// Atomically writes `<dir>/drafts.json` (temp file + rename).
fn save_drafts(dir: &Path, chats: &HashMap<String, Draft>) -> anyhow::Result<()> {
    fs::create_dir_all(dir).context("creating the profile dir")?;
    let file = DraftFile { chats: chats.clone() };
    let json = serde_json::to_vec(&file).context("serializing drafts")?;
    let tmp = dir.join(format!("{DRAFTS_FILE}.tmp"));
    {
        let mut f = fs::File::create(&tmp).context("creating the temp drafts file")?;
        f.write_all(&json).context("writing the temp drafts file")?;
        f.sync_all().context("flushing the temp drafts file")?;
    }
    fs::rename(&tmp, drafts_path(dir)).context("replacing the drafts file")?;
    Ok(())
}

// ---- state ---------------------------------------------------------------------------

struct ProfileDrafts {
    /// `None` in demo mode: nothing is written.
    dir: Option<PathBuf>,
    map: HashMap<String, Draft>,
    dirty: bool,
}

struct Group {
    members: Vec<Candidate>,
    at: Instant,
}

/// The last composer edit of a group chat while a mention is being typed.
struct Typing {
    chat: String,
    text: String,
    cursor: usize,
}

#[derive(Default)]
struct ComposerState {
    /// By profile id.
    drafts: HashMap<String, ProfileDrafts>,
    /// By (profile id, group jid).
    groups: HashMap<(String, String), Group>,
    asked: HashMap<(String, String), Instant>,
    /// Mentions picked from the list: (profile, chat) -> digits -> jid.
    picked: HashMap<(String, String), HashMap<String, String>>,
    shown: Vec<Candidate>,
    typing: Option<Typing>,
    /// What the main composer last synced with the stored draft of its chat.
    main_known: Option<((String, String), Option<Content>)>,
    /// Chat list rows must be rebuilt (a draft appeared, changed or went away).
    list_dirty: bool,
    /// Send permission of groups, from complete replies: (profile id, group jid).
    gates: HashMap<(String, String), Gate>,
    /// Gate requests that got no answer yet.
    gate_asked: HashMap<(String, String), Instant>,
}

struct Gate {
    state: SendGate,
    at: Instant,
}

thread_local! {
    static ST: RefCell<ComposerState> = RefCell::default();
    static SAVE_TIMER: Timer = Timer::default();
    /// Undo/redo history of the composer text (see undo.rs).
    static UNDO: RefCell<undo::UndoStack> = RefCell::default();
}

/// Writes every changed draft file now.
pub(super) fn flush_drafts() {
    ST.with(|s| {
        for (id, d) in s.borrow_mut().drafts.iter_mut().filter(|(_, d)| d.dirty) {
            let Some(dir) = &d.dir else {
                d.dirty = false;
                continue;
            };
            // A removed profile's folder is gone or being deleted: never recreate it.
            if !dir.is_dir() {
                d.dirty = false;
                continue;
            }
            match save_drafts(dir, &d.map) {
                Ok(()) => d.dirty = false,
                Err(e) => log::warn!("could not save the drafts of profile {id}: {e:#}"),
            }
        }
    });
}

/// Forgets a removed profile's drafts so they are never written into its
/// (about to be deleted) folder.
pub(super) fn forget_profile(id: &str) {
    ST.with(|s| {
        s.borrow_mut().drafts.remove(id);
    });
}

/// Hooks the suggestion list of the composer up to the controller.
pub(super) fn wire(win: &MainWindow) {
    let m = win.global::<Mentions>();
    m.on_edited(|text, cursor| with_app(|a| a.mention_edited(text.as_str(), usize::try_from(cursor).unwrap_or(0))));
    m.on_pick(|i| with_app(|a| a.mention_pick(usize::try_from(i).unwrap_or(usize::MAX))));
    m.on_step(|d| with_app(|a| a.mention_step(d)));
    // WhatsApp formatting shortcuts / bar; the key handling lives in the
    // composer's `capture-key-pressed` in chat_view.slint (`apply-format`).
    let weak = win.as_weak();
    m.on_format(move |text, anchor, cursor, kind| {
        let Some(kind) = crate::format::Kind::from_code(kind) else { return text };
        let off = |v: i32| usize::try_from(v).unwrap_or(0);
        let edit = crate::format::apply(text.as_str(), off(anchor), off(cursor), kind);
        if let Some(win) = weak.upgrade() {
            let m = win.global::<Mentions>();
            m.set_fmt_start(i32::try_from(edit.anchor).unwrap_or(i32::MAX));
            m.set_fmt_end(i32::try_from(edit.cursor).unwrap_or(i32::MAX));
        }
        edit.text.into()
    });
    // Undo / redo (undo.rs). `note` is told about every change of the text.
    m.on_note(|text, cursor, atomic| {
        UNDO.with(|u| u.borrow_mut().note(text.as_str(), usize::try_from(cursor).unwrap_or(0), atomic));
    });
    m.on_reset_undo(|text, cursor| {
        UNDO.with(|u| u.borrow_mut().reset(text.as_str(), usize::try_from(cursor).unwrap_or(0)));
    });
    let weak = win.as_weak();
    m.on_history(move |text, cursor, redo| {
        let snap = UNDO.with(|u| {
            let mut u = u.borrow_mut();
            // Typed text not reported yet (should not happen) must not be lost.
            u.note(text.as_str(), usize::try_from(cursor).unwrap_or(0), false);
            if redo { u.redo() } else { u.undo() }
        });
        if let Some(win) = weak.upgrade() {
            let m = win.global::<Mentions>();
            m.set_undo_ok(snap.is_some());
            m.set_undo_cursor(snap.as_ref().map_or(0, |s| i32::try_from(s.cursor).unwrap_or(i32::MAX)));
        }
        snap.map_or(text, |s| s.text.into())
    });
    // Right-aligned text fields for right-to-left text (composer and search boxes).
    win.global::<TextDir>().on_is_rtl(|text| undo::paragraph_is_rtl(text.as_str()));
    // Home / End in right-to-left paragraphs (undo.rs `rtl_home_end`).
    let weak = win.as_weak();
    m.on_line_nav(move |text, anchor, cursor, home, shift| {
        let off = |v: i32| usize::try_from(v).unwrap_or(0);
        let Some((a, c)) = undo::rtl_home_end(text.as_str(), off(anchor), off(cursor), home, shift) else {
            return false;
        };
        let Some(win) = weak.upgrade() else { return false };
        let m = win.global::<Mentions>();
        m.set_nav_anchor(i32::try_from(a).unwrap_or(i32::MAX));
        m.set_nav_cursor(i32::try_from(c).unwrap_or(i32::MAX));
        true
    });
    // Left / Right arrows follow the screen in right-to-left paragraphs (textdir.rs).
    let weak = win.as_weak();
    m.on_arrow_nav(move |text, anchor, cursor, left, word, shift| {
        let off = |v: i32| usize::try_from(v).unwrap_or(0);
        let Some((a, c)) = undo::rtl_arrow(text.as_str(), off(anchor), off(cursor), left, word, shift) else {
            return false;
        };
        let Some(win) = weak.upgrade() else { return false };
        let m = win.global::<Mentions>();
        m.set_nav_anchor(i32::try_from(a).unwrap_or(i32::MAX));
        m.set_nav_cursor(i32::try_from(c).unwrap_or(i32::MAX));
        true
    });
}

/// Whether we may send to a chat, and if not why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SendGate {
    Open,
    AdminsOnly,
    NotParticipant,
}

impl SendGate {
    /// The notice that replaces the composer (empty: normal composer).
    pub(super) fn notice(self) -> &'static str {
        match self {
            SendGate::Open => "",
            SendGate::AdminsOnly => "Only admins can send messages",
            SendGate::NotParticipant => "You can't send messages to this group because you're no longer a participant.",
        }
    }
}

/// Why we may not send to a group. The quick local reply (`complete == false`)
/// assumes membership, so it never blocks.
pub(super) fn gate_state(info: &GroupInfo, complete: bool) -> SendGate {
    if !complete {
        SendGate::Open
    } else if !info.i_am_member {
        SendGate::NotParticipant
    } else if info.announce_only && !info.i_am_admin {
        SendGate::AdminsOnly
    } else {
        SendGate::Open
    }
}

/// Asks (once per `ASK_AGAIN`) for a group's members unless they are cached and fresh.
fn members_due(profile: &str, chat: &str, connected: bool) -> bool {
    let key = (profile.to_string(), chat.to_string());
    ST.with(|s| {
        let mut s = s.borrow_mut();
        let fresh = s.groups.get(&key).is_some_and(|g| g.at.elapsed() < MEMBERS_FRESH);
        let waiting = s.asked.get(&key).is_some_and(|t| t.elapsed() < ASK_AGAIN);
        if fresh || waiting || !connected {
            return false;
        }
        s.asked.insert(key, Instant::now());
        true
    })
}

/// The cached members of the typing's group that match its "@query".
fn matching(profile: &str, typing: &Typing) -> Option<Vec<Candidate>> {
    let (_, query) = mention_token(&typing.text, typing.cursor)?;
    ST.with(|s| {
        let s = s.borrow();
        let group = s.groups.get(&(profile.to_string(), typing.chat.clone()))?;
        Some(filter_candidates(&group.members, &query, MAX_SUGGESTIONS))
    })
}

fn mention_items(shown: &[Candidate]) -> Vec<MentionItem> {
    shown
        .iter()
        .map(|c| MentionItem {
            name: c.name.as_str().into(),
            phone: c.phone.as_str().into(),
            initials: initials(&c.name).into(),
            tint: tint_for(&c.jid),
        })
        .collect()
}

/// Mention suggestions of one pop-out window (the main window's live in `ST`).
#[derive(Default)]
pub(super) struct PopMentions {
    shown: Vec<Candidate>,
    typing: Option<Typing>,
}

impl PopMentions {
    /// The group the user is typing a mention for, if any.
    pub(super) fn typing_chat(&self) -> Option<&str> {
        self.typing.as_ref().map(|t| t.chat.as_str())
    }
}

/// Whether the suggestion list is open (Esc closes it first).
pub(super) fn mention_open() -> bool {
    ST.with(|s| !s.borrow().shown.is_empty())
}

impl App {
    // ---- mentions ------------------------------------------------------------------

    fn mention_edited(&self, text: &str, cursor: usize) {
        let p = self.p();
        let chat = p.st.borrow().selected.clone();
        let Some(chat) = chat.filter(|c| ChatKind::from_jid(c) == ChatKind::Group) else {
            self.close_mentions();
            return;
        };
        if mention_token(text, cursor).is_none() {
            self.close_mentions();
            return;
        }
        ST.with(|s| s.borrow_mut().typing = Some(Typing { chat: chat.clone(), text: text.to_string(), cursor }));
        self.ensure_members(&p.id, &chat, p.st.borrow().conn == ConnState::Connected);
        self.refresh_mentions();
    }

    /// Asks the engine for the group's members unless they are cached and fresh.
    fn ensure_members(&self, profile: &str, chat: &str, connected: bool) {
        if members_due(profile, chat, connected) {
            self.send(Command::FetchContactInfo { jid: chat.to_string() });
        }
    }

    /// Shows the members that match the mention being typed.
    fn refresh_mentions(&self) {
        let pid = self.p().id.clone();
        let shown = ST.with(|s| s.borrow().typing.as_ref().and_then(|t| matching(&pid, t)));
        let Some(shown) = shown.filter(|v| !v.is_empty()) else {
            self.close_mentions();
            return;
        };
        let items = mention_items(&shown);
        ST.with(|s| s.borrow_mut().shown = shown);
        if let Some(win) = self.win() {
            let m = win.global::<Mentions>();
            m.set_items(ModelRc::new(VecModel::from(items)));
            m.set_sel(0);
            m.set_open(true);
        }
    }

    pub(super) fn close_mentions(&self) {
        let was_open = ST.with(|s| {
            let mut s = s.borrow_mut();
            s.typing = None;
            !std::mem::take(&mut s.shown).is_empty()
        });
        if was_open {
            if let Some(win) = self.win() {
                let m = win.global::<Mentions>();
                m.set_open(false);
                m.set_items(ModelRc::default());
            }
        }
    }

    fn mention_step(&self, delta: i32) {
        let Some(win) = self.win() else { return };
        let m = win.global::<Mentions>();
        let n = i32::try_from(m.get_items().row_count()).unwrap_or(0);
        if n > 0 {
            m.set_sel((m.get_sel() + delta).rem_euclid(n));
        }
    }

    fn mention_pick(&self, index: usize) {
        let Some(win) = self.win() else { return };
        let Some(chosen) = ST.with(|s| s.borrow().shown.get(index).cloned()) else { return };
        let text = win.invoke_composer_text().to_string();
        let cursor = usize::try_from(win.invoke_composer_cursor()).unwrap_or(text.len());
        let cursor = cursor.min(text.len());
        let Some((start, _)) = mention_token(&text, cursor) else {
            self.close_mentions();
            return;
        };
        let (new, at) = insert_mention(&text, start, cursor.max(start), &chosen.digits);
        let key = (self.p().id.clone(), self.p().st.borrow().selected.clone().unwrap_or_default());
        ST.with(|s| {
            s.borrow_mut().picked.entry(key).or_default().insert(chosen.digits.clone(), chosen.jid.clone());
        });
        self.close_mentions();
        win.invoke_set_composer(new.as_str().into(), i32::try_from(at).unwrap_or(i32::MAX));
        win.invoke_focus_composer();
        self.on_edit(&new);
    }

    /// The engine answered a member request (for any profile): caches the group.
    pub(super) fn mentions_members(&self, profile: &str, info: &ContactInfo) {
        let Some(group) = &info.group else { return };
        let key = (profile.to_string(), info.jid.clone());
        ST.with(|s| {
            let mut s = s.borrow_mut();
            s.asked.remove(&key);
            s.groups.insert(key, Group { members: candidates(&group.participants), at: Instant::now() });
        });
        let relevant = ST.with(|s| s.borrow().typing.as_ref().is_some_and(|t| t.chat == info.jid));
        if relevant && self.p().id == profile {
            self.refresh_mentions();
        }
        self.popouts_members(profile, &info.jid);
    }

    // ---- mentions in pop-out windows -------------------------------------------------

    /// The composer of a pop-out changed: opens, updates or closes its suggestion list.
    pub(super) fn pop_mention_edited(
        &self,
        ms: &RefCell<PopMentions>,
        win: &ChatWindow,
        profile: &ProfileState,
        chat: &str,
        text: &str,
        cursor: usize,
    ) {
        if ChatKind::from_jid(chat) != ChatKind::Group || mention_token(text, cursor).is_none() {
            Self::pop_mention_close(ms, win);
            return;
        }
        ms.borrow_mut().typing = Some(Typing { chat: chat.to_string(), text: text.to_string(), cursor });
        if members_due(&profile.id, chat, profile.st.borrow().conn == ConnState::Connected) {
            (profile.send)(Command::FetchContactInfo { jid: chat.to_string() });
        }
        Self::pop_mention_refresh(ms, win, &profile.id);
    }

    pub(super) fn pop_mention_refresh(ms: &RefCell<PopMentions>, win: &ChatWindow, profile: &str) {
        let shown = ms.borrow().typing.as_ref().and_then(|t| matching(profile, t));
        let Some(shown) = shown.filter(|v| !v.is_empty()) else {
            Self::pop_mention_close(ms, win);
            return;
        };
        let items = mention_items(&shown);
        ms.borrow_mut().shown = shown;
        let m = win.global::<Mentions>();
        m.set_items(ModelRc::new(VecModel::from(items)));
        m.set_sel(0);
        m.set_open(true);
    }

    pub(super) fn pop_mention_close(ms: &RefCell<PopMentions>, win: &ChatWindow) {
        let was_open = {
            let mut m = ms.borrow_mut();
            m.typing = None;
            !std::mem::take(&mut m.shown).is_empty()
        };
        if was_open {
            let m = win.global::<Mentions>();
            m.set_open(false);
            m.set_items(ModelRc::default());
        }
    }

    pub(super) fn pop_mention_step(win: &ChatWindow, delta: i32) {
        let m = win.global::<Mentions>();
        let n = i32::try_from(m.get_items().row_count()).unwrap_or(0);
        if n > 0 {
            m.set_sel((m.get_sel() + delta).rem_euclid(n));
        }
    }

    /// Inserts the chosen member into the pop-out's composer; returns the new
    /// text so the caller can run its usual "composer edited" handling.
    pub(super) fn pop_mention_pick(
        ms: &RefCell<PopMentions>,
        win: &ChatWindow,
        profile: &str,
        chat: &str,
        index: usize,
    ) -> Option<String> {
        let chosen = ms.borrow().shown.get(index).cloned()?;
        let text = win.invoke_composer_text().to_string();
        let cursor = usize::try_from(win.invoke_composer_cursor()).unwrap_or(text.len()).min(text.len());
        let Some((start, _)) = mention_token(&text, cursor) else {
            Self::pop_mention_close(ms, win);
            return None;
        };
        let (new, at) = insert_mention(&text, start, cursor.max(start), &chosen.digits);
        ST.with(|s| {
            s.borrow_mut()
                .picked
                .entry((profile.to_string(), chat.to_string()))
                .or_default()
                .insert(chosen.digits.clone(), chosen.jid.clone());
        });
        Self::pop_mention_close(ms, win);
        win.invoke_set_composer(new.as_str().into(), i32::try_from(at).unwrap_or(i32::MAX));
        win.invoke_focus_composer();
        Some(new)
    }

    /// JIDs for `SendText.mentions`; forgets the picks of the chat.
    pub(super) fn mentions_for_send(&self, chat: &str, text: &str) -> Vec<String> {
        self.mentions_for_send_in(&self.p().id, chat, text)
    }

    /// [`Self::mentions_for_send`] for a chat of any profile (pop-out windows).
    pub(super) fn mentions_for_send_in(&self, profile: &str, chat: &str, text: &str) -> Vec<String> {
        if ChatKind::from_jid(chat) != ChatKind::Group {
            return Vec::new();
        }
        let key = (profile.to_string(), chat.to_string());
        ST.with(|s| {
            let mut s = s.borrow_mut();
            let picked = s.picked.remove(&key).unwrap_or_default();
            let group = s.groups.get(&key);
            mention_jids(text, |digits| {
                picked.get(digits).cloned().or_else(|| {
                    group.and_then(|g| g.members.iter().find(|c| c.digits == digits)).map(|c| c.jid.clone())
                })
            })
        })
    }

    // ---- drafts --------------------------------------------------------------------

    /// Loads the profile's drafts on first use.
    fn with_drafts<R>(&self, p: &super::ProfileState, f: impl FnOnce(&mut ProfileDrafts) -> R) -> R {
        ST.with(|s| {
            let mut s = s.borrow_mut();
            let d = s.drafts.entry(p.id.clone()).or_insert_with(|| {
                let dir = self.persist.then(|| self.profile_dir(p));
                let map = dir.as_deref().map(load_drafts).unwrap_or_default();
                ProfileDrafts { dir, map, dirty: false }
            });
            f(d)
        })
    }

    fn schedule_draft_save(&self) {
        SAVE_TIMER.with(|t| t.start(TimerMode::SingleShot, SAVE_DELAY, || with_app(App::drafts_tick)));
    }

    fn drafts_tick(&self) {
        flush_drafts();
        if ST.with(|s| std::mem::take(&mut s.borrow_mut().list_dirty)) {
            self.refresh_chats();
        }
    }

    /// The stored draft of a chat of `p`.
    pub(super) fn draft_get(&self, p: &ProfileState, chat: &str) -> Option<Content> {
        self.with_drafts(p, |d| d.map.get(chat).map(|d| (d.text.clone(), d.reply.clone())))
    }

    /// Stores a composer's text and reply target as the draft of `chat`, unless
    /// [`draft_step`] says another window's newer draft must stay. `known` is
    /// what this composer last synced and is updated.
    pub(super) fn draft_commit(
        &self,
        p: &ProfileState,
        chat: &str,
        text: &str,
        reply: Option<ReplyRef>,
        known: &mut Option<Content>,
    ) {
        let current = draft_content(text, reply.as_ref());
        let ts = chrono::Utc::now().timestamp();
        let changed = self.with_drafts(p, |d| {
            let stored = d.map.get(chat).map(|old| (old.text.clone(), old.reply.clone()));
            match draft_step(stored.as_ref(), known.as_ref(), current.as_ref()) {
                DraftStep::InSync => {
                    known.clone_from(&current);
                    false
                }
                DraftStep::Adopt => false,
                DraftStep::Write => {
                    match &current {
                        None => {
                            d.map.remove(chat);
                        }
                        Some((text, reply)) => {
                            d.map.insert(chat.to_string(), Draft { text: text.clone(), reply: reply.clone(), ts });
                            cap_drafts(&mut d.map, MAX_DRAFTS);
                        }
                    }
                    d.dirty = true;
                    known.clone_from(&current);
                    true
                }
            }
        });
        if changed {
            ST.with(|s| s.borrow_mut().list_dirty = true);
            self.schedule_draft_save();
        }
    }

    /// Stores the open chat's composer text and reply target as its draft.
    pub(super) fn draft_touch(&self) {
        let Some(win) = self.win() else { return };
        // The composer holds the message being edited, not a draft.
        if win.global::<MsgOps>().get_editing() {
            return;
        }
        let p = self.p();
        let (chat, reply) = {
            let st = p.st.borrow();
            let Some(chat) = st.selected.clone() else { return };
            (chat, st.reply.clone())
        };
        let text = win.invoke_composer_text().to_string();
        self.tr_edited(&text);
        let key = (p.id.clone(), chat.clone());
        let tracked = ST.with(|s| s.borrow().main_known.as_ref().filter(|(k, _)| *k == key).map(|(_, v)| v.clone()));
        // First touch of this chat: nothing was synced yet, so the stored draft is the baseline.
        let mut known = tracked.unwrap_or_else(|| self.draft_get(&p, &chat));
        self.draft_commit(&p, &chat, &text, reply, &mut known);
        ST.with(|s| s.borrow_mut().main_known = Some((key, known)));
    }

    /// The message was sent: its draft is gone.
    pub(super) fn draft_sent(&self, chat: &str) {
        self.tr_reset();
        let p = self.p();
        self.draft_sent_in(&p, chat);
        ST.with(|s| s.borrow_mut().main_known = Some(((p.id.clone(), chat.to_string()), None)));
    }

    /// Removes the stored draft of `chat` of `p` (a pop-out sent its message).
    pub(super) fn draft_sent_in(&self, p: &ProfileState, chat: &str) {
        let removed = self.with_drafts(p, |d| {
            let removed = d.map.remove(chat).is_some();
            d.dirty |= removed;
            removed
        });
        if removed {
            ST.with(|s| s.borrow_mut().list_dirty = true);
            self.schedule_draft_save();
        }
    }

    /// Puts the chat's draft (if any) into the composer, else empties it.
    /// Call after the chat is selected and its reply target reset.
    pub(super) fn draft_restore(&self, chat: &str) {
        let Some(win) = self.win() else { return };
        self.close_mentions();
        self.tr_reset();
        let p = self.p();
        let draft = self.with_drafts(&p, |d| d.map.get(chat).cloned());
        let known = draft.as_ref().and_then(|d| draft_content(&d.text, d.reply.as_ref()));
        match draft {
            Some(d) => {
                // The text goes first: syncing the reply re-saves the draft from the composer.
                win.invoke_set_composer(d.text.as_str().into(), i32::try_from(d.text.len()).unwrap_or(i32::MAX));
                p.st.borrow_mut().reply = d.reply;
                self.sync_reply();
                UNDO.with(|u| u.borrow_mut().reset(&d.text, d.text.len()));
            }
            None => win.invoke_clear_composer(),
        }
        ST.with(|s| s.borrow_mut().main_known = Some(((p.id.clone(), chat.to_string()), known)));
    }

    /// "Draft: <text>" for the chat list, when the chat has unsent text.
    pub fn draft_preview(&self, chat: &str) -> Option<String> {
        let p = self.p();
        let text = self.with_drafts(&p, |d| d.map.get(chat).map(|d| d.text.clone()))?;
        let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
        (!line.is_empty()).then(|| format!("Draft: {line}"))
    }
}

#[cfg(debug_assertions)]
impl App {
    /// `WACLIENT_DEMO_UI=seq:` steps for the composer: `group` opens the demo
    /// group with its members cached, `type:TEXT`, `down`, `pick`, `send`, `state`.
    pub(super) fn demo_composer(&self, what: &str) {
        let Some(win) = self.win() else { return };
        match what {
            "group" => {
                let jid = "999@g.us";
                self.select_chat(jid);
                let member = |n: &str, name: &str, me: bool| GroupMember {
                    jid: format!("{n}@s.whatsapp.net"),
                    name: (!name.is_empty()).then(|| name.to_string()),
                    phone: Some(n.to_string()),
                    is_me: me,
                    ..Default::default()
                };
                let info = ContactInfo {
                    jid: jid.to_string(),
                    group: Some(wa_engine::GroupInfo {
                        subject: "Family Group".into(),
                        participants: vec![
                            member("989123456789", "Alice Johnson", false),
                            member("447911123456", "Bob", false),
                            member("14155552671", "", false),
                            member("491511111111", "Alina", false),
                            member("100", "Me", true),
                        ],
                        ..Default::default()
                    }),
                    ..Default::default()
                };
                let id = self.p().id.clone();
                self.mentions_members(&id, &info);
            }
            "down" => self.mention_step(1),
            "pick" => {
                let i = win.global::<Mentions>().get_sel();
                self.mention_pick(usize::try_from(i).unwrap_or(0));
            }
            "send" => {
                let text = win.invoke_composer_text().to_string();
                self.send_text(&text);
            }
            "state" => log::debug!(
                "demo: composer_len={} mention_open={} has_draft_preview={}",
                win.invoke_composer_text().chars().count(),
                mention_open(),
                self.p().st.borrow().selected.as_deref().and_then(|c| self.draft_preview(c)).is_some(),
            ),
            t if t.starts_with("type:") => {
                let text = t.trim_start_matches("type:");
                win.invoke_set_composer(text.into(), i32::try_from(text.len()).unwrap_or(i32::MAX));
                self.on_edit(text);
                self.mention_edited(text, text.len());
            }
            other => log::warn!("demo: unknown compose step {other}"),
        }
    }
}

/// Group metadata older than this is requested again when the chat is opened.
const GATE_FRESH: Duration = Duration::from_secs(5 * 60);

/// Most gate lookups one opening of the forward picker starts.
const PICKER_GATE_ASKS: usize = 20;

impl App {
    /// The send gate of `jid` as last reported (`Open` while unknown).
    pub(super) fn gate_of(&self, profile: &str, jid: &str) -> SendGate {
        if ChatKind::from_jid(jid) != ChatKind::Group {
            return SendGate::Open;
        }
        ST.with(|s| s.borrow().gates.get(&(profile.to_string(), jid.to_string())).map_or(SendGate::Open, |g| g.state))
    }

    /// The notice that replaces the composer of `jid` (empty: normal composer).
    pub(super) fn gate_notice(&self, profile: &str, jid: &str) -> &'static str {
        self.gate_of(profile, jid).notice()
    }

    /// Whether a complete reply about this group's send permission was received this session.
    pub(super) fn gate_known(&self, profile: &str, jid: &str) -> bool {
        ST.with(|s| s.borrow().gates.contains_key(&(profile.to_string(), jid.to_string())))
    }

    /// Records a group's send permission from a reply and updates the open chat.
    pub(super) fn record_send_gate(&self, profile: &str, info: &ContactInfo) {
        let Some(group) = &info.group else { return };
        if !info.complete {
            return;
        }
        let key = (profile.to_string(), info.jid.clone());
        let state = gate_state(group, true);
        ST.with(|s| {
            let mut s = s.borrow_mut();
            s.gate_asked.remove(&key);
            s.gates.insert(key, Gate { state, at: Instant::now() });
        });
        self.refresh_picker();
        let open = self.p().st.borrow().selected.as_deref() == Some(info.jid.as_str());
        if open && self.p().id == profile {
            self.update_header();
        }
    }

    /// Opening a group: asks for its flags unless they are known and fresh.
    pub(super) fn ensure_send_gate(&self, jid: &str) -> bool {
        if ChatKind::from_jid(jid) != ChatKind::Group {
            return false;
        }
        let (id, connected) = {
            let p = self.p();
            (p.id.clone(), p.st.borrow().conn == ConnState::Connected)
        };
        let key = (id, jid.to_string());
        let ask = ST.with(|s| {
            let mut s = s.borrow_mut();
            let fresh = s.gates.get(&key).is_some_and(|g| g.at.elapsed() < GATE_FRESH);
            let waiting = s.gate_asked.get(&key).is_some_and(|t| t.elapsed() < ASK_AGAIN);
            if fresh || waiting || !connected {
                return false;
            }
            s.gate_asked.insert(key, Instant::now());
            true
        });
        if ask {
            self.send(Command::FetchContactInfo { jid: jid.to_string() });
        }
        ask
    }

    /// Forward picker: asks for the flags of listed groups whose gate is unknown,
    /// at most `PICKER_GATE_ASKS` per opening (the engine paces and caches them).
    pub(super) fn ensure_picker_gates(&self, jids: &[String]) {
        let mut asked = 0;
        for jid in jids {
            if asked >= PICKER_GATE_ASKS {
                break;
            }
            let p = self.p();
            if ChatKind::from_jid(jid) == ChatKind::Group && !self.gate_known(&p.id, jid) && self.ensure_send_gate(jid) {
                asked += 1;
            }
        }
    }

    /// Toasts and returns true when the open chat does not accept messages from us.
    pub(super) fn refuse_gated(&self) -> bool {
        let (id, sel) = {
            let p = self.p();
            (p.id.clone(), p.st.borrow().selected.clone())
        };
        let notice = sel.map_or("", |j| self.gate_notice(&id, &j));
        if !notice.is_empty() {
            self.show_toast(notice);
        }
        !notice.is_empty()
    }

    /// Demo: the open group says "only admins can send messages".
    #[cfg(debug_assertions)]
    pub(super) fn demo_group_announce(&self) {
        let jid = "999@g.us";
        self.select_chat(jid);
        let info = ContactInfo {
            jid: jid.to_string(),
            complete: true,
            group: Some(GroupInfo { announce_only: true, i_am_member: true, ..Default::default() }),
            ..Default::default()
        };
        let id = self.p().id.clone();
        self.record_send_gate(&id, &info);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(announce_only: bool, i_am_admin: bool, i_am_member: bool) -> GroupInfo {
        GroupInfo { announce_only, i_am_admin, i_am_member, ..Default::default() }
    }

    #[test]
    fn gate_blocks_non_admins_of_announce_groups() {
        assert_eq!(gate_state(&flags(true, false, true), true), SendGate::AdminsOnly);
        assert_eq!(gate_state(&flags(true, true, true), true), SendGate::Open);
        assert_eq!(gate_state(&flags(false, false, true), true), SendGate::Open);
    }

    #[test]
    fn gate_blocks_former_members_first() {
        assert_eq!(gate_state(&flags(true, false, false), true), SendGate::NotParticipant);
        assert_eq!(gate_state(&flags(false, true, false), true), SendGate::NotParticipant);
    }

    #[test]
    fn gate_ignores_incomplete_info() {
        assert_eq!(gate_state(&flags(true, false, false), false), SendGate::Open);
    }

    #[test]
    fn gate_notice_strings_are_unchanged() {
        assert_eq!(SendGate::Open.notice(), "");
        assert_eq!(SendGate::AdminsOnly.notice(), "Only admins can send messages");
        assert!(SendGate::NotParticipant.notice().contains("no longer a participant"));
    }

    fn member(jid: &str, name: Option<&str>, phone: Option<&str>) -> GroupMember {
        GroupMember {
            jid: jid.into(),
            name: name.map(String::from),
            phone: phone.map(String::from),
            ..Default::default()
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("waclient-drafts-test-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).expect("create temp dir");
        p
    }

    #[test]
    fn token_needs_a_word_start() {
        assert_eq!(mention_token("hi @al", 6), Some((3, "al".into())));
        assert_eq!(mention_token("@", 1), Some((0, String::new())));
        assert_eq!(mention_token("mail me at a@b", 14), None);
        assert_eq!(mention_token("@al done", 8), None);
        assert_eq!(mention_token("no at sign", 5), None);
        // The cursor decides: text after it is not part of the query.
        assert_eq!(mention_token("@alice bob", 3), Some((0, "al".into())));
        assert_eq!(mention_token("é @x", 5), Some((3, "x".into())));
    }

    #[test]
    fn token_accepts_format_characters_before_the_at_sign() {
        // Persian text followed by a right-to-left mark, then "@".
        let text = "سلام\u{200F}@al";
        assert_eq!(mention_token(text, text.len()), Some((text.find('@').expect("at"), "al".into())));
        for c in ['\u{200E}', '\u{200C}', '\u{200D}', '\u{2066}', '\u{2069}', '\u{061C}'] {
            let text = format!("x{c}@b");
            assert_eq!(mention_token(&text, text.len()), Some((text.find('@').expect("at"), "b".into())));
        }
        assert_eq!(mention_token("سلام@al", "سلام@al".len()), None);
    }

    #[test]
    fn flush_skips_removed_profiles_and_forget_drops_them() {
        let gone = temp_dir("gone");
        let alive = temp_dir("alive");
        let mut map = HashMap::new();
        map.insert("c".to_string(), Draft { text: "t".into(), reply: None, ts: 1 });
        ST.with(|s| {
            let mut s = s.borrow_mut();
            s.drafts.insert("gone".into(), ProfileDrafts { dir: Some(gone.clone()), map: map.clone(), dirty: true });
            s.drafts.insert("alive".into(), ProfileDrafts { dir: Some(alive.clone()), map: map.clone(), dirty: true });
            s.drafts.insert("forgotten".into(), ProfileDrafts { dir: Some(gone.clone()), map, dirty: true });
        });
        fs::remove_dir_all(&gone).expect("remove");
        forget_profile("forgotten");
        flush_drafts();
        assert!(!gone.exists(), "a removed profile folder must not be recreated");
        assert!(alive.join(DRAFTS_FILE).exists());
        ST.with(|s| {
            let s = s.borrow();
            assert!(!s.drafts.contains_key("forgotten"));
            assert!(!s.drafts["gone"].dirty);
        });
        ST.with(|s| s.borrow_mut().drafts.clear());
        let _ = fs::remove_dir_all(&alive);
    }

    #[test]
    fn candidates_skip_me_and_fall_back_to_phone() {
        let mut me = member("1@s.whatsapp.net", Some("Me"), Some("1"));
        me.is_me = true;
        let list = candidates(&[
            member("989123456789@s.whatsapp.net", Some("Zed"), Some("989123456789")),
            me,
            member("4915123456789@s.whatsapp.net", None, Some("4915123456789")),
            member("77@lid", Some("Amy"), None),
        ]);
        let names: Vec<&str> = list.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["+49 151 2345 6789", "Amy", "Zed"]);
        assert_eq!(list[1].digits, "77");
        assert_eq!(list[2].digits, "989123456789");
    }

    #[test]
    fn filter_prefers_prefix_then_substring_and_digits() {
        let list = candidates(&[
            member("1@s.whatsapp.net", Some("Maria"), Some("491511111")),
            member("2@s.whatsapp.net", Some("Omar"), Some("98912")),
            member("3@s.whatsapp.net", Some("Marco"), Some("3933")),
        ]);
        let names = |q: &str| -> Vec<String> { filter_candidates(&list, q, 6).into_iter().map(|c| c.name).collect() };
        assert_eq!(names("mar"), ["Marco", "Maria", "Omar"]);
        assert_eq!(names("MARI"), ["Maria"]);
        assert_eq!(names("989"), ["Omar"]);
        assert_eq!(names("").len(), 3);
        assert_eq!(filter_candidates(&list, "", 2).len(), 2);
        assert!(names("zzz").is_empty());
    }

    #[test]
    fn picking_inserts_digits_and_moves_the_cursor() {
        let (text, cursor) = insert_mention("hi @al, ok", 3, 6, "989123456789");
        assert_eq!(text, "hi @989123456789 , ok");
        assert_eq!(cursor, 3 + "@989123456789 ".len());
        let (text, cursor) = insert_mention("@", 0, 1, "55");
        assert_eq!((text.as_str(), cursor), ("@55 ", 4));
    }

    #[test]
    fn mention_jids_follow_the_tokens_in_the_text() {
        let lookup = |d: &str| match d {
            "111" => Some("111@s.whatsapp.net".to_string()),
            "222" => Some("222@lid".to_string()),
            _ => None,
        };
        let jids = mention_jids("@111 and @222, @111 again @999 mail a@111 @111x @", lookup);
        assert_eq!(jids, ["111@s.whatsapp.net", "222@lid"]);
        assert!(mention_jids("no mentions", lookup).is_empty());
        // A mention whose text was deleted is not sent.
        assert!(mention_jids("only @", lookup).is_empty());
    }

    #[test]
    fn cap_drops_the_oldest() {
        let mut map: HashMap<String, Draft> = (0..5)
            .map(|i| (format!("c{i}"), Draft { text: "x".into(), reply: None, ts: i }))
            .collect();
        cap_drafts(&mut map, 3);
        let mut keys: Vec<_> = map.keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["c2", "c3", "c4"]);
    }

    #[test]
    fn drafts_survive_a_restart() {
        let dir = temp_dir("roundtrip");
        assert!(load_drafts(&dir).is_empty());
        let mut map = HashMap::new();
        map.insert(
            "a@g.us".to_string(),
            Draft { text: "héllo\nworld".into(), reply: Some(("id1".into(), "Ann".into(), "hey".into())), ts: 5 },
        );
        map.insert("b@s.whatsapp.net".to_string(), Draft { text: "x".into(), reply: None, ts: 6 });
        save_drafts(&dir, &map).expect("save");
        assert!(!dir.join("drafts.json.tmp").exists());
        assert_eq!(load_drafts(&dir), map);
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn corrupt_drafts_are_moved_aside() {
        let dir = temp_dir("corrupt");
        fs::write(drafts_path(&dir), b"{not json").expect("write");
        assert!(load_drafts(&dir).is_empty());
        assert!(dir.join("drafts.json.bad").exists());
        assert!(!drafts_path(&dir).exists());
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn saved_file_is_capped_on_load() {
        let dir = temp_dir("cap");
        let map: HashMap<String, Draft> = (0..(MAX_DRAFTS as i64 + 20))
            .map(|i| (format!("c{i}"), Draft { text: "x".into(), reply: None, ts: i }))
            .collect();
        save_drafts(&dir, &map).expect("save");
        let loaded = load_drafts(&dir);
        assert_eq!(loaded.len(), MAX_DRAFTS);
        assert!(!loaded.contains_key("c0"));
        assert!(loaded.contains_key(&format!("c{}", MAX_DRAFTS + 19)));
        fs::remove_dir_all(&dir).expect("cleanup");
    }
}
