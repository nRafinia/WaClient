//! "Send later": text messages scheduled for a time, kept per profile in
//! `scheduled.json` and sent by this app while it runs (WhatsApp has no
//! server-side scheduling).
//!
//! Safety rules:
//! - An item is removed from the file BEFORE its command is sent, so a crash
//!   in between loses the message but can never send it twice. If the file
//!   cannot be written, nothing is sent.
//! - Items that were due while the app was closed (or asleep for more than
//!   [`LATE_GRACE_SECS`]) are never sent silently: they are held as "overdue"
//!   and the user decides in the banner / list.
//! - The composer gate (admins-only groups, left groups) and blocked contacts
//!   are checked when the item fires, not only when it was scheduled.
//! - Message text is never logged.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use chrono::{Datelike, Local, LocalResult, NaiveDate, NaiveDateTime, NaiveTime, TimeDelta, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use slint::{ComponentHandle, ModelRc, Timer, TimerMode, VecModel};
use wa_engine::{Command, ConnState};
use wa_store::ChatKind;

use super::{App, ProfileState, Removal, with_app};
use crate::{MainWindow, MsgOps, QuickPick, SchedRow, Schedule};

const FILE: &str = "scheduled.json";
/// How often due items are looked for.
const TICK_EVERY: Duration = Duration::from_secs(15);
/// An item that is found due later than this after its time (the PC slept,
/// the app was busy) is held instead of sent.
const LATE_GRACE_SECS: i64 = 300;
/// Furthest time ahead an item may be scheduled.
const MAX_AHEAD_SECS: i64 = 365 * 24 * 3600;
const MAX_ITEMS: usize = 200;
const PREVIEW_CHARS: usize = 140;

// ---- pure parts ------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Item {
    id: String,
    chat_jid: String,
    text: String,
    #[serde(default)]
    mentions: Vec<String>,
    #[serde(default)]
    quoted_id: Option<String>,
    /// Unix seconds.
    due: i64,
    created: i64,
}

#[derive(Default, Serialize, Deserialize)]
struct SchedFile {
    #[serde(default)]
    items: Vec<Item>,
}

/// Why an item is not sent by the timer.
#[derive(Debug, Clone, PartialEq)]
enum Held {
    /// Due while the app was not running.
    Overdue,
    /// Firing was refused; the text says why.
    Failed(String),
    /// The profile's engine was not connected when the item came due; the
    /// timer tries again on every tick (and moves it to `Overdue` if that
    /// takes longer than [`LATE_GRACE_SECS`]).
    Waiting,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fate {
    Wait,
    Send,
    Overdue,
}

/// What to do with an item due at `due` when looked at `now`.
fn fate(due: i64, now: i64, grace: i64) -> Fate {
    if due > now {
        Fate::Wait
    } else if now - due <= grace {
        Fate::Send
    } else {
        Fate::Overdue
    }
}

#[derive(Debug, Default, PartialEq)]
struct DueSet {
    /// Ids to send now, oldest first.
    send: Vec<String>,
    /// Ids found too late to send silently.
    overdue: Vec<String>,
}

/// The items the timer must act on; held items are left alone.
fn select_due(items: &[Item], held: &HashMap<String, Held>, now: i64, grace: i64) -> DueSet {
    let mut sorted: Vec<&Item> =
        items.iter().filter(|i| !matches!(held.get(&i.id), Some(h) if *h != Held::Waiting)).collect();
    sorted.sort_by(|a, b| (a.due, a.created, &a.id).cmp(&(b.due, b.created, &b.id)));
    let mut out = DueSet::default();
    for i in sorted {
        match fate(i.due, now, grace) {
            Fate::Wait => {}
            Fate::Send => out.send.push(i.id.clone()),
            Fate::Overdue => out.overdue.push(i.id.clone()),
        }
    }
    out
}

/// What `fire` does before sending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Readiness {
    /// The engine is not live: keep the item.
    WaitConnection,
    /// A group whose send permission is unknown: ask for it, fire on the next tick.
    AskGroupInfo,
    Go,
}

/// The timer asks for an unknown group's flags once and defers that item by
/// one tick; after that it fires anyway and the engine rejects what it must.
/// A user-initiated send never waits for it.
fn readiness(connected: bool, gate_unknown: bool, deferred_before: bool, from_timer: bool) -> Readiness {
    if !connected {
        Readiness::WaitConnection
    } else if from_timer && gate_unknown && !deferred_before {
        Readiness::AskGroupInfo
    } else {
        Readiness::Go
    }
}

/// At start every item whose time has passed is overdue, however recently.
fn startup_overdue(items: &[Item], now: i64) -> Vec<String> {
    let mut due: Vec<&Item> = items.iter().filter(|i| i.due <= now).collect();
    due.sort_by(|a, b| (a.due, a.created, &a.id).cmp(&(b.due, b.created, &b.id)));
    due.into_iter().map(|i| i.id.clone()).collect()
}

type Resolve<'a> = &'a dyn Fn(NaiveDateTime) -> LocalResult<i64>;

fn local_resolve(n: NaiveDateTime) -> LocalResult<i64> {
    Local.from_local_datetime(&n).map(|d| d.timestamp())
}

fn local_naive(ts: i64) -> Option<NaiveDateTime> {
    Local.timestamp_opt(ts, 0).single().map(|d| d.naive_local())
}

/// Unix time of a wall-clock time. An ambiguous time (clocks go back) takes
/// its first occurrence; a time that does not exist (clocks go forward) moves
/// ahead to the first valid hour.
fn resolve_wall(resolve: Resolve, wall: NaiveDateTime) -> Option<i64> {
    match resolve(wall) {
        LocalResult::Single(t) => Some(t),
        LocalResult::Ambiguous(a, b) => Some(a.min(b)),
        LocalResult::None => (1..=3).find_map(|h| match resolve(wall + TimeDelta::hours(h)) {
            LocalResult::Single(t) => Some(t),
            LocalResult::Ambiguous(a, b) => Some(a.min(b)),
            LocalResult::None => None,
        }),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuickKind {
    InOneHour,
    Tonight,
    Tomorrow,
}

#[derive(Debug, Clone, PartialEq)]
struct Quick {
    kind: QuickKind,
    label: String,
    due: i64,
}

fn at(date: NaiveDate, h: u32, m: u32) -> Option<NaiveDateTime> {
    Some(date.and_time(NaiveTime::from_hms_opt(h, m, 0)?))
}

/// "In 1 hour", "Tonight 21:00" (only while that is still ahead) and
/// "Tomorrow 09:00", computed on the local calendar.
fn quick_picks(now: i64, now_local: NaiveDateTime, resolve: Resolve) -> Vec<Quick> {
    let mut out = vec![Quick { kind: QuickKind::InOneHour, label: "In 1 hour".into(), due: now + 3600 }];
    let today = now_local.date();
    let tonight = at(today, 21, 0).and_then(|w| resolve_wall(resolve, w)).filter(|d| *d > now + 60);
    if let Some(due) = tonight {
        out.push(Quick { kind: QuickKind::Tonight, label: "Tonight 21:00".into(), due });
    }
    let tomorrow = today.succ_opt().and_then(|d| at(d, 9, 0)).and_then(|w| resolve_wall(resolve, w));
    if let Some(due) = tomorrow {
        out.push(Quick { kind: QuickKind::Tomorrow, label: "Tomorrow 09:00".into(), due });
    }
    out
}

/// The time of a quick pick at the moment it is confirmed: "In 1 hour" means
/// one hour from the press, not from when the dialog opened.
fn resolve_quick(kind: QuickKind, now: i64, now_local: NaiveDateTime, resolve: Resolve) -> Result<i64, WhenError> {
    let due = quick_picks(now, now_local, resolve).into_iter().find(|q| q.kind == kind).map(|q| q.due);
    due.ok_or(WhenError::Past).and_then(|d| check_future(d, now))
}

/// Every path to a scheduled time ends here: it must still lie ahead.
fn check_future(due: i64, now: i64) -> Result<i64, WhenError> {
    if due > now { Ok(due) } else { Err(WhenError::Past) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WhenError {
    BadDate,
    BadTime,
    Past,
    TooFar,
}

impl WhenError {
    fn message(self) -> &'static str {
        match self {
            WhenError::BadDate => "Enter the date as YYYY-MM-DD, for example 2026-12-24.",
            WhenError::BadTime => "Enter the time as HH:MM (24-hour), for example 21:30.",
            WhenError::Past => "Pick a time in the future.",
            WhenError::TooFar => "Pick a time within the next year.",
        }
    }
}

/// The unix time of a typed local date and time; it must lie ahead, at most a year.
fn parse_when(date: &str, time: &str, now: i64, resolve: Resolve) -> Result<i64, WhenError> {
    let date = NaiveDate::parse_from_str(date.trim(), "%Y-%m-%d").map_err(|_| WhenError::BadDate)?;
    let time = NaiveTime::parse_from_str(time.trim(), "%H:%M").map_err(|_| WhenError::BadTime)?;
    let due = resolve_wall(resolve, date.and_time(time)).ok_or(WhenError::BadTime)?;
    if due <= now {
        Err(WhenError::Past)
    } else if due > now + MAX_AHEAD_SECS {
        Err(WhenError::TooFar)
    } else {
        Ok(due)
    }
}

/// "Today 21:00", "Tomorrow 09:00", "Sat 28 Nov, 09:00" (with the year when it differs).
fn format_when(due: i64, now_local: NaiveDateTime, to_local: &dyn Fn(i64) -> Option<NaiveDateTime>) -> String {
    let Some(l) = to_local(due) else { return "?".into() };
    let today = now_local.date();
    let clock = l.format("%H:%M");
    if l.date() == today {
        format!("Today {clock}")
    } else if today.succ_opt() == Some(l.date()) {
        format!("Tomorrow {clock}")
    } else if l.year() == today.year() {
        format!("{}, {clock}", l.format("%a %-d %b"))
    } else {
        format!("{}, {clock}", l.format("%a %-d %b %Y"))
    }
}

/// One line of the text for the list: whitespace collapsed, cut at [`PREVIEW_CHARS`].
fn preview(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > PREVIEW_CHARS {
        let cut: String = line.chars().take(PREVIEW_CHARS).collect();
        format!("{cut}…")
    } else {
        line
    }
}

/// "Couldn't send: only admins can send messages" from the composer notice.
fn couldnt_send(notice: &str) -> String {
    let notice = notice.trim().trim_end_matches('.');
    let mut chars = notice.chars();
    let lowered = chars.next().map(|c| c.to_lowercase().collect::<String>() + chars.as_str()).unwrap_or_default();
    format!("Couldn't send: {lowered}")
}

fn overdue_text(n: usize) -> String {
    if n == 1 { "1 scheduled message is overdue".into() } else { format!("{n} scheduled messages are overdue") }
}

/// Banner text over every profile: the total, and the other profiles' share.
fn overdue_summary(active: usize, others: &[(String, usize)]) -> String {
    let others: Vec<&(String, usize)> = others.iter().filter(|(_, n)| *n > 0).collect();
    let total = active + others.iter().map(|(_, n)| n).sum::<usize>();
    if total == 0 {
        return String::new();
    }
    let base = overdue_text(total);
    if others.is_empty() {
        return base;
    }
    let list: Vec<String> = others.iter().map(|(name, n)| format!("{name}: {n}")).collect();
    format!("{base} ({})", list.join(", "))
}

fn items_path(dir: &Path) -> PathBuf {
    dir.join(FILE)
}

/// A missing file yields no items; a corrupt one is moved aside as `.bad`.
fn load_items(dir: &Path) -> Vec<Item> {
    let path = items_path(dir);
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                log::warn!("reading {}: {e}", path.display());
            }
            return Vec::new();
        }
    };
    match serde_json::from_slice::<SchedFile>(&bytes) {
        Ok(f) => {
            let mut seen = std::collections::HashSet::new();
            f.items.into_iter().filter(|i| seen.insert(i.id.clone())).take(MAX_ITEMS).collect()
        }
        Err(e) => {
            log::warn!("corrupt {}: {e}; setting it aside", path.display());
            let bad = dir.join(format!("{FILE}.bad"));
            if let Err(e) = fs::rename(&path, &bad) {
                log::warn!("backing up {}: {e}", path.display());
            }
            Vec::new()
        }
    }
}

/// Atomically writes `<dir>/scheduled.json` (temp file + rename).
fn save_items(dir: &Path, items: &[Item]) -> anyhow::Result<()> {
    fs::create_dir_all(dir).context("creating the profile dir")?;
    let json = serde_json::to_vec(&SchedFile { items: items.to_vec() }).context("serializing scheduled messages")?;
    let tmp = dir.join(format!("{FILE}.tmp"));
    {
        let mut f = fs::File::create(&tmp).context("creating the temp schedule file")?;
        f.write_all(&json).context("writing the temp schedule file")?;
        f.sync_all().context("flushing the temp schedule file")?;
    }
    fs::rename(&tmp, items_path(dir)).context("replacing the schedule file")?;
    Ok(())
}

// ---- state -----------------------------------------------------------------------------

struct ProfileSched {
    /// `None`: demo mode or a profile that cannot be opened; nothing is written.
    dir: Option<PathBuf>,
    items: Vec<Item>,
    held: HashMap<String, Held>,
}

impl ProfileSched {
    /// Writes the file; a profile folder that is gone (being removed) is never recreated.
    fn persist(&self) -> anyhow::Result<()> {
        match &self.dir {
            Some(dir) if dir.is_dir() => save_items(dir, &self.items),
            _ => Ok(()),
        }
    }
}

/// What the open dialog does when confirmed.
enum Target {
    /// Schedule this composer text for the open chat.
    New { profile: String, chat: String, text: String },
    /// Change the time of an existing item.
    Edit { profile: String, id: String },
}

#[derive(Default)]
struct Sched {
    /// By profile id.
    profiles: HashMap<String, ProfileSched>,
    target: Option<Target>,
    /// Quick picks of the open dialog; their times are recomputed on confirm.
    quick: Vec<QuickKind>,
    /// Items (profile id, item id) held back one tick to learn their group's flags.
    deferred: HashSet<(String, String)>,
    /// `Some(chat)`: the list shows that chat only; `None`: the whole profile.
    list_chat: Option<String>,
    seq: u32,
}

thread_local! {
    static ST: RefCell<Sched> = RefCell::default();
    static TICK: Timer = Timer::default();
}

enum Fired {
    Sent,
    Refused(String),
    /// The profile's engine is not connected; the item stays.
    Waiting,
    /// Group flags were requested; the timer retries next tick.
    Deferred,
    Missing,
}

pub(super) fn wire(win: &MainWindow) {
    let g = win.global::<Schedule>();
    g.on_open_dialog(|text| with_app(|a| a.schedule_open(text.as_str())));
    g.on_pick_quick(|i| with_app(|a| a.schedule_quick(i)));
    g.on_apply_custom(|d, t| with_app(|a| a.schedule_custom(d.as_str(), t.as_str())));
    g.on_close_dialog(|| with_app(App::schedule_close_dialog));
    g.on_open_chat_list(|| with_app(|a| a.schedule_open_list(true)));
    g.on_open_all_list(|| with_app(|a| a.schedule_open_list(false)));
    g.on_close_list(|| with_app(App::schedule_close_list));
    g.on_edit_time(|id| with_app(|a| a.schedule_edit_time(id.as_str())));
    g.on_send_now(|id| with_app(|a| a.schedule_send_now(id.as_str())));
    g.on_delete(|id| with_app(|a| a.schedule_delete(id.as_str())));
    g.on_overdue_send_all(|| with_app(App::schedule_overdue_send_all));
    g.on_overdue_review(|| with_app(App::schedule_overdue_review));
}

/// Whether a dialog of this feature is open (Esc closes it first).
pub(super) fn dialog_open(win: &MainWindow) -> bool {
    win.global::<Schedule>().get_dialog() != 0
}

pub(super) fn list_open(win: &MainWindow) -> bool {
    win.global::<Schedule>().get_list_open()
}

impl App {
    /// Loads every profile's schedule, holds what came due while the app was
    /// closed and starts the timer.
    pub(super) fn schedule_start(&self) {
        let profs: Vec<_> = self.profs.borrow().clone();
        for p in &profs {
            self.with_sched(p, |_| ());
        }
        TICK.with(|t| t.start(TimerMode::Repeated, TICK_EVERY, || with_app(App::schedule_tick)));
        self.sched_refresh_ui();
    }

    /// The window moved to another profile: its dialogs are void.
    pub(super) fn schedule_profile_changed(&self) {
        ST.with(|s| {
            let mut s = s.borrow_mut();
            s.target = None;
            s.list_chat = None;
        });
        if let Some(win) = self.win() {
            let g = win.global::<Schedule>();
            g.set_dialog(0);
            g.set_list_open(false);
        }
        self.sched_refresh_ui();
    }

    fn with_sched<R>(&self, p: &ProfileState, f: impl FnOnce(&mut ProfileSched) -> R) -> R {
        ST.with(|s| {
            let mut s = s.borrow_mut();
            let ps = s.profiles.entry(p.id.clone()).or_insert_with(|| self.load_profile(p));
            f(ps)
        })
    }

    fn load_profile(&self, p: &ProfileState) -> ProfileSched {
        let dir = (self.persist && p.meta.borrow().is_ready()).then(|| self.profile_dir(p));
        let items = dir.as_deref().map(load_items).unwrap_or_default();
        let held = startup_overdue(&items, Utc::now().timestamp()).into_iter().map(|id| (id, Held::Overdue)).collect();
        ProfileSched { dir, items, held }
    }

    /// Persists after an edit; the in-memory change stays when the write fails.
    fn sched_save(&self, p: &ProfileState) {
        let saved = self.with_sched(p, |ps| ps.persist());
        if let Err(e) = saved {
            log::error!("could not save the scheduled messages of profile {}: {e:#}", p.id);
            self.show_toast("Couldn't save scheduled messages to disk");
        }
    }

    fn schedule_tick(&self) {
        let now = Utc::now().timestamp();
        let ids: Vec<String> = ST.with(|s| s.borrow().profiles.keys().cloned().collect());
        let mut changed = false;
        for id in ids {
            let Some(p) = self.find(&id).filter(|p| p.removal.get() == Removal::No) else {
                ST.with(|s| s.borrow_mut().profiles.remove(&id));
                continue;
            };
            let due = self.with_sched(&p, |ps| select_due(&ps.items, &ps.held, now, LATE_GRACE_SECS));
            if !due.overdue.is_empty() {
                self.with_sched(&p, |ps| {
                    for id in &due.overdue {
                        ps.held.insert(id.clone(), Held::Overdue);
                    }
                });
                changed = true;
            }
            for item in &due.send {
                changed = true;
                self.fire(&p, item, true);
            }
        }
        if changed {
            self.sched_refresh_ui();
        }
    }

    /// Why a message to `jid` of profile `p` must not go out now.
    fn send_refusal(&self, p: &ProfileState, jid: &str) -> Option<String> {
        let notice = self.gate_notice(&p.id, jid);
        if !notice.is_empty() {
            return Some(couldnt_send(notice));
        }
        if ChatKind::from_jid(jid) == ChatKind::Dm && self.blocked_matcher(p)(jid) {
            return Some(couldnt_send("The contact is blocked"));
        }
        None
    }

    /// Sends one item through its own profile's engine. The item leaves the
    /// file first, so it can never be sent twice.
    ///
    /// `from_timer`: the timer fires it (a waiting item is marked, an unknown
    /// group is asked for its flags first); otherwise the user asked for it now.
    fn fire(&self, p: &ProfileState, id: &str, from_timer: bool) -> Fired {
        let Some(item) = self.with_sched(p, |ps| ps.items.iter().find(|i| i.id == id).cloned()) else {
            return Fired::Missing;
        };
        // A stopping engine ignores commands silently, so only a live one counts.
        let connected = p.removal.get() == Removal::No && p.st.borrow().conn == ConnState::Connected;
        let gate_unknown =
            ChatKind::from_jid(&item.chat_jid) == ChatKind::Group && !self.gate_known(&p.id, &item.chat_jid);
        let key = (p.id.clone(), id.to_string());
        let deferred = ST.with(|s| s.borrow().deferred.contains(&key));
        match readiness(connected, gate_unknown, deferred, from_timer) {
            Readiness::WaitConnection => {
                if from_timer {
                    self.with_sched(p, |ps| ps.held.insert(id.to_string(), Held::Waiting));
                }
                return Fired::Waiting;
            }
            Readiness::AskGroupInfo => {
                ST.with(|s| s.borrow_mut().deferred.insert(key));
                (p.send)(Command::FetchContactInfo { jid: item.chat_jid });
                return Fired::Deferred;
            }
            Readiness::Go => {}
        }
        if let Some(reason) = self.send_refusal(p, &item.chat_jid) {
            self.with_sched(p, |ps| ps.held.insert(id.to_string(), Held::Failed(reason.clone())));
            return Fired::Refused(reason);
        }
        let removed = self.with_sched(p, |ps| {
            let pos = ps.items.iter().position(|i| i.id == id)?;
            let taken = ps.items.remove(pos);
            let saved = ps.persist();
            match saved {
                Ok(()) => {
                    ps.held.remove(id);
                    Some(Ok(taken))
                }
                Err(e) => {
                    ps.items.insert(pos, taken);
                    ps.held.insert(id.to_string(), Held::Failed("Couldn't send: the schedule file is not writable".into()));
                    Some(Err(e))
                }
            }
        });
        match removed {
            None => Fired::Missing,
            Some(Err(e)) => {
                log::error!("not sending a scheduled message of profile {}: {e:#}", p.id);
                Fired::Refused("Couldn't send: the schedule file is not writable".into())
            }
            Some(Ok(item)) => {
                ST.with(|s| s.borrow_mut().deferred.remove(&key));
                log::info!("sending a scheduled message (profile {}, chat {})", p.id, crate::models::redact_jid(&item.chat_jid));
                (p.send)(Command::SendText {
                    chat_jid: item.chat_jid,
                    text: item.text,
                    quoted_id: item.quoted_id,
                    mentions: item.mentions,
                });
                Fired::Sent
            }
        }
    }

    // ---- dialog ------------------------------------------------------------------------

    /// "Send later…" on the composer: asks when to send `text` to the open chat.
    fn schedule_open(&self, text: &str) {
        let text = text.trim();
        let Some(win) = self.win() else { return };
        if text.is_empty() || win.global::<MsgOps>().get_editing() {
            return;
        }
        if self.chat_is_blocked_dm() {
            self.show_toast("Unblock this contact to send messages");
            return;
        }
        if self.refuse_gated() {
            return;
        }
        let p = self.p();
        let Some(chat) = p.st.borrow().selected.clone() else { return };
        self.schedule_begin(&p, chat, text.to_string());
    }

    fn schedule_begin(&self, p: &ProfileState, chat: String, text: String) {
        if self.with_sched(p, |ps| ps.items.len()) >= MAX_ITEMS {
            self.show_toast("Too many scheduled messages: send or delete some first");
            return;
        }
        let shown = preview(&text);
        ST.with(|s| s.borrow_mut().target = Some(Target::New { profile: p.id.clone(), chat, text }));
        self.sched_open_dialog(1, &shown, Utc::now().timestamp() + 3600);
    }

    /// Shows the dialog with the quick picks and a date/time prefilled with `default_due`.
    fn sched_open_dialog(&self, mode: i32, shown: &str, default_due: i64) {
        let Some(win) = self.win() else { return };
        let now = Utc::now().timestamp();
        let now_local = local_naive(now).unwrap_or_default();
        let picks = quick_picks(now, now_local, &local_resolve);
        let labels: Vec<QuickPick> = picks.iter().map(|q| QuickPick { label: q.label.as_str().into() }).collect();
        ST.with(|s| s.borrow_mut().quick = picks.iter().map(|q| q.kind).collect());
        let when = local_naive(default_due).unwrap_or(now_local);
        let g = win.global::<Schedule>();
        g.set_quick(ModelRc::new(VecModel::from(labels)));
        g.set_preview(shown.into());
        g.set_date_text(when.format("%Y-%m-%d").to_string().into());
        g.set_time_text(when.format("%H:%M").to_string().into());
        g.set_error("".into());
        g.set_dialog(mode);
    }

    pub(super) fn schedule_close_dialog(&self) {
        ST.with(|s| s.borrow_mut().target = None);
        let Some(win) = self.win() else { return };
        let g = win.global::<Schedule>();
        g.set_dialog(0);
        g.set_error("".into());
        if !g.get_list_open() {
            win.invoke_focus_composer();
        }
    }

    fn schedule_quick(&self, i: i32) {
        let kind = usize::try_from(i).ok().and_then(|i| ST.with(|s| s.borrow().quick.get(i).copied()));
        let (Some(kind), Some(win)) = (kind, self.win()) else { return };
        let now = Utc::now().timestamp();
        match resolve_quick(kind, now, local_naive(now).unwrap_or_default(), &local_resolve) {
            Ok(due) => self.schedule_apply(due),
            Err(e) => win.global::<Schedule>().set_error(e.message().into()),
        }
    }

    fn schedule_custom(&self, date: &str, time: &str) {
        let Some(win) = self.win() else { return };
        match parse_when(date, time, Utc::now().timestamp(), &local_resolve) {
            Ok(due) => self.schedule_apply(due),
            Err(e) => win.global::<Schedule>().set_error(e.message().into()),
        }
    }

    /// The dialog was confirmed with a valid time.
    fn schedule_apply(&self, due: i64) {
        let now = Utc::now().timestamp();
        if let Err(e) = check_future(due, now) {
            if let Some(win) = self.win() {
                win.global::<Schedule>().set_error(e.message().into());
            }
            return;
        }
        let Some(target) = ST.with(|s| s.borrow_mut().target.take()) else { return };
        let p = self.p();
        let when = format_when(due, local_naive(now).unwrap_or_default(), &local_naive);
        match target {
            Target::New { profile, chat, text } => {
                let still_here = p.id == profile && p.st.borrow().selected.as_deref() == Some(chat.as_str());
                self.schedule_close_dialog();
                if !still_here {
                    self.show_toast("Not scheduled: the chat was changed meanwhile");
                    return;
                }
                let quoted_id = p.st.borrow_mut().reply.take().map(|r| r.0);
                let mentions = self.mentions_for_send(&chat, &text);
                let id = ST.with(|s| {
                    let mut s = s.borrow_mut();
                    s.seq = s.seq.wrapping_add(1);
                    format!("{:x}-{:x}", Utc::now().timestamp_micros(), s.seq)
                });
                let item = Item { id, chat_jid: chat.clone(), text, mentions, quoted_id, due, created: now };
                self.with_sched(&p, |ps| ps.items.push(item));
                self.sched_save(&p);
                // Same tail as a normal send: the composer and its draft are done with.
                self.stop_composing();
                self.close_mentions();
                if let Some(win) = self.win() {
                    win.invoke_clear_composer();
                }
                self.draft_sent(&chat);
                self.sync_reply();
                self.show_toast(&format!("Scheduled for {when}. WaClient must be running to send it."));
            }
            Target::Edit { profile, id } => {
                self.schedule_close_dialog();
                let Some(owner) = self.find(&profile) else { return };
                self.with_sched(&owner, |ps| {
                    if let Some(i) = ps.items.iter_mut().find(|i| i.id == id) {
                        i.due = due;
                        ps.held.remove(&id);
                    }
                });
                self.sched_save(&owner);
                self.show_toast(&format!("Rescheduled for {when}"));
            }
        }
        self.sched_refresh_ui();
    }

    // ---- list and banner ---------------------------------------------------------------

    fn schedule_open_list(&self, chat_only: bool) {
        let Some(win) = self.win() else { return };
        let p = self.p();
        let chat = if chat_only { p.st.borrow().selected.clone() } else { None };
        ST.with(|s| s.borrow_mut().list_chat = chat);
        win.global::<Schedule>().set_list_open(true);
        self.sched_refresh_ui();
    }

    pub(super) fn schedule_close_list(&self) {
        let Some(win) = self.win() else { return };
        win.global::<Schedule>().set_list_open(false);
        win.invoke_focus_composer();
    }

    fn schedule_edit_time(&self, id: &str) {
        let p = self.p();
        let Some(due) = self.with_sched(&p, |ps| ps.items.iter().find(|i| i.id == id).map(|i| i.due)) else { return };
        let now = Utc::now().timestamp();
        ST.with(|s| s.borrow_mut().target = Some(Target::Edit { profile: p.id.clone(), id: id.to_string() }));
        self.sched_open_dialog(2, "", if due > now { due } else { now + 3600 });
    }

    fn schedule_send_now(&self, id: &str) {
        let p = self.p();
        match self.fire(&p, id, false) {
            Fired::Sent => self.show_toast("Scheduled message sent"),
            Fired::Refused(reason) => self.show_toast(&reason),
            Fired::Waiting => self.show_toast("Not connected: try again once WaClient is connected"),
            Fired::Deferred | Fired::Missing => {}
        }
        self.sched_refresh_ui();
    }

    fn schedule_delete(&self, id: &str) {
        let p = self.p();
        self.with_sched(&p, |ps| {
            ps.items.retain(|i| i.id != id);
            ps.held.remove(id);
        });
        self.sched_save(&p);
        self.sched_refresh_ui();
    }

    /// Banner "Send now": every overdue item of the profile goes out, oldest first.
    fn schedule_overdue_send_all(&self) {
        let p = self.p();
        if self.overdue_count(&p) == 0 {
            // The overdue items belong to another profile: show them there instead.
            self.schedule_overdue_review();
            return;
        }
        let ids = self.with_sched(&p, |ps| {
            let overdue: Vec<Item> =
                ps.items.iter().filter(|i| ps.held.get(&i.id) == Some(&Held::Overdue)).cloned().collect();
            let mut ids: Vec<(i64, i64, String)> = overdue.into_iter().map(|i| (i.due, i.created, i.id)).collect();
            ids.sort();
            ids
        });
        let mut refused = 0;
        for (_, _, id) in ids {
            if matches!(self.fire(&p, &id, false), Fired::Refused(_) | Fired::Waiting) {
                refused += 1;
            }
        }
        if refused > 0 {
            self.show_toast(&format!("{refused} could not be sent: see Scheduled messages"));
        }
        self.sched_refresh_ui();
    }

    fn overdue_count(&self, p: &ProfileState) -> usize {
        self.with_sched(p, |ps| ps.held.values().filter(|h| **h == Held::Overdue).count())
    }

    /// Banner "Review": the list of the profile that has overdue items (the active one first).
    fn schedule_overdue_review(&self) {
        if self.overdue_count(&self.p()) == 0 {
            let profs: Vec<_> = self.profs.borrow().clone();
            if let Some(other) = profs.iter().find(|q| self.overdue_count(q) > 0) {
                self.activate(&other.id);
            }
        }
        self.schedule_open_list(false);
    }

    fn chat_name(&self, p: &ProfileState, jid: &str) -> String {
        let name = p.st.borrow().chats.iter().find(|c| c.jid == jid).map(|c| c.display_name.clone());
        name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| jid.split('@').next().unwrap_or(jid).to_string())
    }

    /// Pushes the active profile's schedule into the window: header clock,
    /// overdue banner and (while open) the list.
    pub(super) fn sched_refresh_ui(&self) {
        let Some(win) = self.win() else { return };
        let p = self.p();
        let selected = p.st.borrow().selected.clone();
        let list_chat = ST.with(|s| s.borrow().list_chat.clone());
        let g = win.global::<Schedule>();
        let (mut items, held) = self.with_sched(&p, |ps| (ps.items.clone(), ps.held.clone()));
        items.sort_by(|a, b| (a.due, a.created, &a.id).cmp(&(b.due, b.created, &b.id)));
        let in_chat = selected.as_deref().map_or(0, |c| items.iter().filter(|i| i.chat_jid == c).count());
        let active_overdue = held.values().filter(|h| **h == Held::Overdue).count();
        let profs: Vec<_> = self.profs.borrow().clone();
        let others: Vec<(String, usize)> =
            profs.iter().filter(|q| q.id != p.id).map(|q| (q.name(), self.overdue_count(q))).collect();
        let overdue = active_overdue + others.iter().map(|(_, n)| n).sum::<usize>();
        g.set_chat_count(i32::try_from(in_chat).unwrap_or(i32::MAX));
        g.set_overdue_count(i32::try_from(overdue).unwrap_or(i32::MAX));
        g.set_overdue_text(overdue_summary(active_overdue, &others).into());
        if !g.get_list_open() {
            return;
        }
        let now_local = local_naive(Utc::now().timestamp()).unwrap_or_default();
        let rows: Vec<SchedRow> = items
            .iter()
            .filter(|i| list_chat.as_deref().is_none_or(|c| i.chat_jid == c))
            .map(|i| SchedRow {
                id: i.id.as_str().into(),
                chat: self.chat_name(&p, &i.chat_jid).into(),
                when: format_when(i.due, now_local, &local_naive).into(),
                text: preview(&i.text).into(),
                status: match held.get(&i.id) {
                    Some(Held::Overdue) => "Overdue: WaClient was not running at that time".into(),
                    Some(Held::Failed(why)) => why.as_str().into(),
                    Some(Held::Waiting) => "Waiting for connection".into(),
                    None => "".into(),
                },
            })
            .collect();
        let title = match &list_chat {
            Some(c) => format!("Scheduled messages · {}", self.chat_name(&p, c)),
            None => "Scheduled messages".to_string(),
        };
        g.set_list_all(list_chat.is_none());
        g.set_list_title(title.into());
        g.set_rows(ModelRc::new(VecModel::from(rows)));
    }
}

#[cfg(debug_assertions)]
impl App {
    /// Debug aid for the demo (`WACLIENT_DEMO_UI=schedule-dialog|scheduled-list|
    /// scheduled-overdue|scheduled-header`, also usable as `seq:` steps). Demo data lives in memory only.
    pub(super) fn demo_schedule(&self, what: &str) {
        let p = self.p();
        let alice = "111@s.whatsapp.net";
        let now = Utc::now().timestamp();
        match what {
            "schedule-dialog" => {
                self.select_chat(alice);
                self.schedule_begin(&p, alice.to_string(), "Happy birthday! Dinner at 7 at the usual place?".into());
            }
            "scheduled-header" => {
                self.select_chat(alice);
                let text = "See you at 6!";
                self.with_sched(&p, |ps| {
                    ps.held.clear();
                    ps.items = vec![Item {
                        id: "demo-h".into(),
                        chat_jid: alice.to_string(),
                        text: text.into(),
                        mentions: Vec::new(),
                        quoted_id: None,
                        due: now + 3 * 3600,
                        created: now,
                    }];
                });
                if let Some(win) = self.win() {
                    win.invoke_set_composer("Running late, start without me".into(), 29);
                }
                self.sched_refresh_ui();
            }
            "scheduled-list" | "scheduled-overdue" => {
                self.select_chat(alice);
                let mk = |n: u32, chat: &str, text: &str, due: i64| Item {
                    id: format!("demo-{n}"),
                    chat_jid: chat.to_string(),
                    text: text.to_string(),
                    mentions: Vec::new(),
                    quoted_id: None,
                    due,
                    created: now,
                };
                self.with_sched(&p, |ps| {
                    ps.items = vec![
                        mk(1, alice, "Happy birthday! Dinner at 7 at the usual place?", now + 3 * 3600),
                        mk(2, "999@g.us", "Reminder: team meeting moved to 10:00 tomorrow.", now + 20 * 3600),
                        mk(3, alice, "Don't forget to bring the documents.", now + 3 * 86_400),
                        mk(4, alice, "Good morning!", now - 7200),
                        mk(5, "999@g.us", "Weekly report is due today.", now - 3600),
                    ];
                    ps.held.clear();
                    ps.held.insert("demo-4".into(), Held::Overdue);
                    ps.held.insert("demo-5".into(), Held::Failed(couldnt_send("Only admins can send messages")));
                });
                if what == "scheduled-list" {
                    self.schedule_open_list(false);
                } else {
                    self.sched_refresh_ui();
                }
            }
            other => log::warn!("unknown schedule demo: {other}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, due: i64) -> Item {
        Item {
            id: id.into(),
            chat_jid: "1@s.whatsapp.net".into(),
            text: format!("text {id}"),
            mentions: vec!["2@s.whatsapp.net".into()],
            quoted_id: Some("Q1".into()),
            due,
            created: 10,
        }
    }

    fn utc(n: NaiveDateTime) -> LocalResult<i64> {
        LocalResult::Single(n.and_utc().timestamp())
    }

    fn wall(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, mo, d).and_then(|d| d.and_hms_opt(h, mi, 0)).expect("valid date")
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("waclient-schedule-test-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).expect("create temp dir");
        p
    }

    #[test]
    fn fate_waits_sends_or_calls_overdue() {
        assert_eq!(fate(101, 100, 300), Fate::Wait);
        assert_eq!(fate(100, 100, 300), Fate::Send);
        assert_eq!(fate(100 - 300, 100, 300), Fate::Send);
        assert_eq!(fate(100 - 301, 100, 300), Fate::Overdue);
    }

    #[test]
    fn due_selection_is_ordered_and_skips_held_items() {
        let items = [item("c", 90), item("a", 50), item("late", 1), item("future", 500), item("held", 80)];
        let mut held = HashMap::new();
        held.insert("held".to_string(), Held::Failed("x".into()));
        let due = select_due(&items, &held, 100, 300);
        assert_eq!(due.send, ["late", "a", "c"]);
        assert!(due.overdue.is_empty());
        let due = select_due(&items, &held, 1000, 300);
        assert_eq!(due.send, Vec::<String>::new());
        assert_eq!(due.overdue, ["late", "a", "c", "future"]);
    }

    #[test]
    fn everything_past_is_overdue_at_start() {
        let items = [item("b", 60), item("a", 40), item("future", 101), item("now", 100)];
        assert_eq!(startup_overdue(&items, 100), ["a", "b", "now"]);
        assert!(startup_overdue(&[], 100).is_empty());
    }

    #[test]
    fn quick_picks_follow_the_local_calendar() {
        let now_local = wall(2026, 11, 22, 14, 0);
        let now = now_local.and_utc().timestamp();
        let picks = quick_picks(now, now_local, &utc);
        let labels: Vec<&str> = picks.iter().map(|q| q.label.as_str()).collect();
        assert_eq!(labels, ["In 1 hour", "Tonight 21:00", "Tomorrow 09:00"]);
        assert_eq!(picks[0].due, now + 3600);
        assert_eq!(picks[1].due, wall(2026, 11, 22, 21, 0).and_utc().timestamp());
        assert_eq!(picks[2].due, wall(2026, 11, 23, 9, 0).and_utc().timestamp());
    }

    #[test]
    fn tonight_is_dropped_once_it_has_passed() {
        let now_local = wall(2026, 12, 31, 21, 30);
        let picks = quick_picks(now_local.and_utc().timestamp(), now_local, &utc);
        let labels: Vec<&str> = picks.iter().map(|q| q.label.as_str()).collect();
        assert_eq!(labels, ["In 1 hour", "Tomorrow 09:00"]);
        // Tomorrow crosses the year.
        assert_eq!(picks[1].due, wall(2027, 1, 1, 9, 0).and_utc().timestamp());
    }

    #[test]
    fn nonexistent_wall_times_move_forward() {
        // Clocks jump from 02:00 to 03:00 on this day (UTC+1 -> UTC+2).
        let gap = wall(2026, 3, 29, 2, 0)..wall(2026, 3, 29, 3, 0);
        let resolve = |n: NaiveDateTime| {
            if gap.contains(&n) { LocalResult::None } else { LocalResult::Single(n.and_utc().timestamp() - 3600) }
        };
        let got = resolve_wall(&resolve, wall(2026, 3, 29, 2, 30));
        assert_eq!(got, Some(wall(2026, 3, 29, 3, 30).and_utc().timestamp() - 3600));
        assert_eq!(resolve_wall(&resolve, wall(2026, 3, 29, 9, 0)), Some(wall(2026, 3, 29, 9, 0).and_utc().timestamp() - 3600));
    }

    #[test]
    fn ambiguous_wall_times_take_the_first_occurrence() {
        let resolve = |n: NaiveDateTime| {
            let t = n.and_utc().timestamp();
            LocalResult::Ambiguous(t - 7200, t - 3600)
        };
        let t = wall(2026, 10, 25, 2, 30).and_utc().timestamp();
        assert_eq!(resolve_wall(&resolve, wall(2026, 10, 25, 2, 30)), Some(t - 7200));
    }

    #[test]
    fn quick_picks_are_recomputed_when_confirmed() {
        let opened = wall(2026, 11, 22, 14, 0);
        let later = wall(2026, 11, 22, 14, 40);
        let (t0, t1) = (opened.and_utc().timestamp(), later.and_utc().timestamp());
        assert_eq!(resolve_quick(QuickKind::InOneHour, t0, opened, &utc), Ok(t0 + 3600));
        assert_eq!(resolve_quick(QuickKind::InOneHour, t1, later, &utc), Ok(t1 + 3600));
        let night = wall(2026, 11, 22, 21, 30);
        let tn = night.and_utc().timestamp();
        assert_eq!(resolve_quick(QuickKind::Tonight, tn, night, &utc), Err(WhenError::Past));
        assert_eq!(resolve_quick(QuickKind::Tomorrow, tn, night, &utc), Ok(wall(2026, 11, 23, 9, 0).and_utc().timestamp()));
    }

    #[test]
    fn a_time_must_be_ahead_at_confirm() {
        assert_eq!(check_future(101, 100), Ok(101));
        assert_eq!(check_future(100, 100), Err(WhenError::Past));
        assert_eq!(check_future(50, 100), Err(WhenError::Past));
    }

    #[test]
    fn firing_waits_for_the_connection_and_defers_unknown_groups_once() {
        assert_eq!(readiness(false, false, false, true), Readiness::WaitConnection);
        assert_eq!(readiness(false, true, false, false), Readiness::WaitConnection);
        assert_eq!(readiness(true, true, false, true), Readiness::AskGroupInfo);
        assert_eq!(readiness(true, true, true, true), Readiness::Go);
        assert_eq!(readiness(true, false, false, true), Readiness::Go);
        assert_eq!(readiness(true, true, false, false), Readiness::Go);
    }

    #[test]
    fn waiting_items_are_retried_but_failed_ones_are_not() {
        let items = [item("w", 90), item("f", 91)];
        let mut held = HashMap::new();
        held.insert("w".to_string(), Held::Waiting);
        held.insert("f".to_string(), Held::Failed("x".into()));
        assert_eq!(select_due(&items, &held, 100, 300).send, ["w"]);
        assert_eq!(select_due(&items, &held, 1000, 300).overdue, ["w"]);
    }

    #[test]
    fn overdue_summary_spans_profiles() {
        let work = |n| vec![("Work".to_string(), n)];
        assert_eq!(overdue_summary(0, &work(0)), "");
        assert_eq!(overdue_summary(1, &work(0)), "1 scheduled message is overdue");
        assert_eq!(overdue_summary(1, &work(2)), "3 scheduled messages are overdue (Work: 2)");
        assert_eq!(overdue_summary(0, &work(1)), "1 scheduled message is overdue (Work: 1)");
        let two = vec![("Work".to_string(), 2), ("Home".to_string(), 1)];
        assert_eq!(overdue_summary(0, &two), "3 scheduled messages are overdue (Work: 2, Home: 1)");
    }

    #[test]
    fn typed_times_are_validated() {
        let now = wall(2026, 11, 22, 14, 0).and_utc().timestamp();
        let ok = parse_when(" 2026-11-22 ", "14:01", now, &utc);
        assert_eq!(ok, Ok(wall(2026, 11, 22, 14, 1).and_utc().timestamp()));
        assert_eq!(parse_when("2026-11-22", "14:00", now, &utc), Err(WhenError::Past));
        assert_eq!(parse_when("2026-11-21", "23:00", now, &utc), Err(WhenError::Past));
        assert_eq!(parse_when("2027-11-22", "14:00", now, &utc), Ok(now + MAX_AHEAD_SECS));
        assert_eq!(parse_when("2027-11-22", "14:01", now, &utc), Err(WhenError::TooFar));
        assert_eq!(parse_when("22/11/2026", "15:00", now, &utc), Err(WhenError::BadDate));
        assert_eq!(parse_when("2026-02-30", "15:00", now, &utc), Err(WhenError::BadDate));
        assert_eq!(parse_when("2026-11-23", "25:00", now, &utc), Err(WhenError::BadTime));
        assert_eq!(parse_when("2026-11-23", "", now, &utc), Err(WhenError::BadTime));
    }

    #[test]
    fn times_are_worded_relative_to_today() {
        let now_local = wall(2026, 11, 22, 14, 0);
        let to_local = |ts: i64| chrono::DateTime::from_timestamp(ts, 0).map(|d| d.naive_utc());
        let f = |n: NaiveDateTime| format_when(n.and_utc().timestamp(), now_local, &to_local);
        assert_eq!(f(wall(2026, 11, 22, 21, 0)), "Today 21:00");
        assert_eq!(f(wall(2026, 11, 23, 9, 5)), "Tomorrow 09:05");
        assert_eq!(f(wall(2026, 11, 28, 9, 0)), "Sat 28 Nov, 09:00");
        assert_eq!(f(wall(2027, 1, 4, 18, 30)), "Mon 4 Jan 2027, 18:30");
    }

    #[test]
    fn previews_are_one_short_line() {
        assert_eq!(preview("a\n\n b   c"), "a b c");
        let long = "x".repeat(300);
        let p = preview(&long);
        assert_eq!(p.chars().count(), PREVIEW_CHARS + 1);
        assert!(p.ends_with('…'));
    }

    #[test]
    fn refusal_reads_as_a_sentence() {
        assert_eq!(couldnt_send("Only admins can send messages"), "Couldn't send: only admins can send messages");
        assert_eq!(couldnt_send("You can't send here."), "Couldn't send: you can't send here");
        assert_eq!(overdue_text(1), "1 scheduled message is overdue");
        assert_eq!(overdue_text(3), "3 scheduled messages are overdue");
    }

    #[test]
    fn items_survive_a_round_trip() {
        let dir = temp_dir("roundtrip");
        let items = vec![item("a", 100), item("b", 200)];
        save_items(&dir, &items).expect("save");
        assert_eq!(load_items(&dir), items);
        assert!(!dir.join(format!("{FILE}.tmp")).exists(), "the temp file is renamed away");
        // Removing one and saving again keeps only the other.
        save_items(&dir, &items[1..]).expect("save again");
        assert_eq!(load_items(&dir), items[1..]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_is_empty_and_a_corrupt_one_is_set_aside() {
        let dir = temp_dir("corrupt");
        assert!(load_items(&dir).is_empty());
        fs::write(items_path(&dir), b"{ not json").expect("write");
        assert!(load_items(&dir).is_empty());
        assert!(!items_path(&dir).exists());
        assert!(dir.join(format!("{FILE}.bad")).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn duplicate_ids_and_old_files_without_optional_fields_load() {
        let dir = temp_dir("lenient");
        let json = r#"{"items":[{"id":"a","chat_jid":"1@s.whatsapp.net","text":"x","due":5,"created":1},
            {"id":"a","chat_jid":"2@s.whatsapp.net","text":"dup","due":6,"created":1}]}"#;
        fs::write(items_path(&dir), json).expect("write");
        let items = load_items(&dir);
        assert_eq!(items.len(), 1);
        assert!(items[0].mentions.is_empty() && items[0].quoted_id.is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_removed_profile_folder_is_not_recreated() {
        let dir = temp_dir("gone");
        let ps = ProfileSched { dir: Some(dir.clone()), items: vec![item("a", 1)], held: HashMap::new() };
        fs::remove_dir_all(&dir).expect("remove");
        ps.persist().expect("skipped");
        assert!(!dir.exists());
    }
}
