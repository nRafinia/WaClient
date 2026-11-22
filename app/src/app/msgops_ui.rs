//! Message operations of the chat view: edit, multi-select and forward, pin,
//! star (with the starred list) and the message-info panel. The Slint side is
//! the `MsgOps` global (ui/msgops.slint); this file owns the state behind it.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use chrono::{DateTime, Local, TimeZone};
use slint::{ComponentHandle, Model, ModelRc, Timer, TimerMode, VecModel};
use wa_engine::{Command, ReceiptEntry, STATUS_JID};
use wa_store::{ChatKind, ChatRow, ContactRow, MessageRow, MessageStatus};

use super::composer::SendGate;
use super::{App, with_app};
use crate::{MainWindow, MsgItem, MsgOps, PickItem, ReceiptRow, StarredRow, models};

/// Most messages one forward carries.
pub const MAX_FORWARD_MESSAGES: usize = 20;
/// Most chats one forward goes to.
pub const MAX_FORWARD_CHATS: usize = 5;
/// WhatsApp's pin lengths: 24 hours, 7 days, 30 days.
const PIN_SECS: [u32; 3] = [86_400, 604_800, 2_592_000];
/// Rows one load of the starred list asks the store for.
const STARRED_WANT: usize = 40;
/// Starred-list reloads wait this long for further changes.
const STARRED_DEBOUNCE: Duration = Duration::from_millis(200);
/// A message-info request that gets no answer stops showing "Loading…".
const INFO_TIMEOUT: Duration = Duration::from_secs(8);
/// How long the jump target keeps its highlight.
const JUMP_HIGHLIGHT: Duration = Duration::from_millis(2500);
/// A chat opened by a jump needs a layout pass before it can be scrolled.
const JUMP_AFTER_OPEN: Duration = Duration::from_millis(150);
/// Longest one-line preview in the edit and pin strips.
const PREVIEW_CHARS: usize = 120;

thread_local! {
    static OPS: RefCell<Ops> = RefCell::new(Ops::default());
    static HIGHLIGHT: Timer = Timer::default();
    static INFO_WAIT: Timer = Timer::default();
    static JUMP: Timer = Timer::default();
    static RESTORE: Timer = Timer::default();
    static STAR_RELOAD: Timer = Timer::default();
}

#[derive(Default)]
struct Ops {
    /// Ids picked in selection mode, in the order they were picked.
    selected: Vec<String>,
    /// Ids the "Delete N messages for me?" dialog is about, fixed when it opened.
    delete_ids: Vec<String>,
    edit: Option<EditTarget>,
    forward: Option<ForwardDraft>,
    forward_model: Option<Rc<VecModel<PickItem>>>,
    /// The message whose info panel is open: (chat, id).
    info: Option<(String, String)>,
    starred: Option<StarredView>,
    /// Id of the open chat's pinned message, as of the last rebuild.
    pinned: Option<String>,
}

#[derive(Clone)]
struct EditTarget {
    chat: String,
    id: String,
    /// What the composer held before the edit started; put back afterwards.
    prev_text: String,
    /// The quoted message that was pending before the edit; put back afterwards.
    prev_reply: Option<(String, String, String)>,
}

struct ForwardDraft {
    from: String,
    ids: Vec<String>,
    query: String,
    picked: Vec<String>,
}

struct StarredView {
    chat: Option<String>,
    /// `(ts, chat_jid, id)` of the last loaded row: the store's keyset cursor.
    cursor: Option<(i64, String, String)>,
    rows: Vec<MessageRow>,
    more: bool,
}

// ---- pure helpers -----------------------------------------------------------------

/// Seconds a pin lasts for the dialog's choice (0 24 h, 1 7 days, 2 30 days).
pub fn pin_duration_secs(choice: i32) -> Option<u32> {
    usize::try_from(choice).ok().and_then(|i| PIN_SECS.get(i)).copied()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toggle {
    Added,
    Removed,
    /// Not added: the list already holds `max` entries.
    Full,
}

/// Adds `id` to `list`, or removes it when present; never beyond `max` entries.
pub fn toggle_limited(list: &mut Vec<String>, id: &str, max: usize) -> Toggle {
    if let Some(pos) = list.iter().position(|s| s == id) {
        list.remove(pos);
        return Toggle::Removed;
    }
    if list.len() >= max {
        return Toggle::Full;
    }
    list.push(id.to_string());
    Toggle::Added
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BarState {
    pub count: usize,
    /// Every picked message is starred, so the bar offers "Unstar".
    pub all_starred: bool,
}

/// What the selection bar shows for the picked ids.
pub fn bar_state(selected: &[String], msgs: &[MessageRow]) -> BarState {
    let all_starred = !selected.is_empty()
        && selected.iter().all(|id| msgs.iter().find(|m| &m.id == id).is_some_and(|m| m.starred));
    BarState { count: selected.len(), all_starred }
}

/// The picked ids in the order of the chat (oldest first); ids that are not
/// loaded are dropped.
pub fn ordered_ids(msgs: &[MessageRow], selected: &[String]) -> Vec<String> {
    msgs.iter().filter(|m| selected.contains(&m.id)).map(|m| m.id.clone()).collect()
}

/// The picked ids in chat order that can be forwarded or starred (no polls,
/// system rows or revoked messages); ids that are not loaded are dropped.
pub fn shareable_ids(msgs: &[MessageRow], selected: &[String]) -> Vec<String> {
    msgs.iter().filter(|m| selected.contains(&m.id) && models::can_share(m)).map(|m| m.id.clone()).collect()
}

/// Short reason shown under a chat in the forward picker when its send gate
/// forbids sending.
pub fn gate_label(gate: SendGate) -> &'static str {
    match gate {
        SendGate::Open => "",
        SendGate::AdminsOnly => "Only admins can send",
        SendGate::NotParticipant => "You're no longer a participant",
    }
}

/// Splits the picked chats into those that accept messages and how many do not.
pub fn split_gated(picked: Vec<String>, gated: impl Fn(&str) -> bool) -> (Vec<String>, usize) {
    let total = picked.len();
    let open: Vec<String> = picked.into_iter().filter(|j| !gated(j)).collect();
    let dropped = total - open.len();
    (open, dropped)
}

/// Toast for picked messages that were left out of a forward or star.
fn left_out_text(picked: usize, kept: usize) -> Option<String> {
    let dropped = picked.checked_sub(kept).filter(|d| *d > 0)?;
    Some(if dropped == 1 {
        "1 selected message can't be used here and was left out".to_string()
    } else {
        format!("{dropped} selected messages can't be used here and were left out")
    })
}

/// Why a forward of `messages` to `chats` cannot go ahead.
pub fn check_forward(messages: usize, chats: usize) -> Result<(), &'static str> {
    if messages == 0 {
        Err("Select at least one message")
    } else if messages > MAX_FORWARD_MESSAGES {
        Err("You can forward up to 20 messages at once")
    } else if chats == 0 {
        Err("Pick at least one chat")
    } else if chats > MAX_FORWARD_CHATS {
        Err("You can forward to up to 5 chats at once")
    } else {
        Ok(())
    }
}

/// Texts of the picked messages for the clipboard, oldest first, one per line.
pub fn copy_text_of(msgs: &[MessageRow], selected: &[String]) -> String {
    msgs.iter()
        .filter(|m| selected.contains(&m.id) && !m.revoked)
        .filter_map(|m| m.text.as_deref().filter(|t| !t.trim().is_empty()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// "Today, 14:05" / "Yesterday, 09:10" / "03/02/2026, 14:05".
pub fn when_text(ts: i64, now: DateTime<Local>) -> String {
    let Some(t) = Local.timestamp_opt(ts, 0).single() else { return String::new() };
    let clock = t.format("%H:%M");
    let days = now.date_naive().signed_duration_since(t.date_naive()).num_days();
    match days {
        0 => format!("Today, {clock}"),
        1 => format!("Yesterday, {clock}"),
        _ => format!("{}, {clock}", t.format("%d/%m/%Y")),
    }
}

/// How a contact is named in the info panel: saved name, push name, number.
pub fn contact_label(c: Option<&ContactRow>, jid: &str) -> String {
    let non_empty = |s: &Option<String>| s.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    if let Some(c) = c {
        if let Some(n) = non_empty(&c.full_name).or_else(|| non_empty(&c.push_name)) {
            return n;
        }
        if let Some(p) = non_empty(&c.phone) {
            return if p.starts_with('+') { p } else { format!("+{p}") };
        }
    }
    let user = jid.split('@').next().unwrap_or(jid);
    if user.chars().all(|c| c.is_ascii_digit()) && !user.is_empty() { format!("+{user}") } else { user.to_string() }
}

/// Recipients who read it first (latest first), then who only got it, then the rest.
pub fn sort_receipts(list: &mut [ReceiptEntry]) {
    let rank = |r: &ReceiptEntry| -> (u8, i64) {
        if let Some(t) = r.read_at {
            (0, -t)
        } else if let Some(t) = r.delivered_at {
            (1, -t)
        } else {
            (2, 0)
        }
    };
    list.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.jid.cmp(&b.jid)));
}

/// What is known of one delivery stage of a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seen {
    No,
    /// Reached, but no time is stored (older message, or the receipt rows
    /// are missing): known from the message's own status only.
    Yes,
    At(i64),
}

/// Read, delivered and played stage of a direct-chat message, from its
/// receipt (if any) and its status. A later stage implies the earlier ones.
pub fn dm_seen(entry: Option<&ReceiptEntry>, status: MessageStatus) -> (Seen, Seen, Seen) {
    let stage = |at: Option<i64>, rank: i64| match at {
        Some(t) => Seen::At(t),
        None if status.to_i64() >= rank => Seen::Yes,
        None => Seen::No,
    };
    let played = stage(entry.and_then(|e| e.played_at), MessageStatus::Played.to_i64());
    let mut read = stage(entry.and_then(|e| e.read_at), MessageStatus::Read.to_i64());
    let mut delivered = stage(entry.and_then(|e| e.delivered_at), MessageStatus::Delivered.to_i64());
    if read == Seen::No && played != Seen::No {
        read = Seen::Yes;
    }
    if delivered == Seen::No && read != Seen::No {
        delivered = Seen::Yes;
    }
    (read, delivered, played)
}

/// Send time and state of the message under its preview.
fn info_time_text(ts: i64, status: MessageStatus, now: DateTime<Local>) -> String {
    format!("{} · {}", when_text(ts, now), status_label(status))
}

/// The message's own state, shown next to its time in the info panel.
pub fn status_label(status: MessageStatus) -> &'static str {
    match status {
        MessageStatus::Pending => "Sending",
        MessageStatus::Sent => "Sent",
        MessageStatus::Delivered => "Delivered",
        MessageStatus::Read => "Read",
        MessageStatus::Played => "Played",
        MessageStatus::Failed => "Not sent",
    }
}

fn one_line(s: &str) -> String {
    let line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= PREVIEW_CHARS {
        return line;
    }
    let mut cut: String = line.chars().take(PREVIEW_CHARS).collect();
    cut.push('…');
    cut
}

// ---- wiring -----------------------------------------------------------------------

pub(super) fn wire(win: &MainWindow) {
    let g = win.global::<MsgOps>();
    g.on_begin_select(|id| with_app(|a| a.begin_select(id.as_str())));
    g.on_toggle_select(|id| with_app(|a| a.toggle_select(id.as_str())));
    g.on_select_forward(|| with_app(App::select_forward));
    g.on_select_star(|| with_app(App::select_star));
    g.on_select_copy(|| with_app(App::select_copy));
    g.on_select_delete(|| with_app(App::select_delete));
    g.on_select_cancel(|| with_app(App::select_cancel));
    g.on_forward_one(|id| with_app(|a| a.forward_one(id.as_str())));
    g.on_begin_edit(|id| with_app(|a| a.begin_edit_message(id.as_str())));
    g.on_edit_cancel(|| with_app(App::edit_cancel));
    g.on_pin_chosen(|id, choice| with_app(|a| a.pin_chosen(id.as_str(), choice)));
    g.on_unpin(|id| with_app(|a| a.unpin(id.as_str())));
    g.on_pin_unpin(|| with_app(App::pin_unpin));
    g.on_pin_jump(|| with_app(App::pin_jump));
    g.on_toggle_star(|id| with_app(|a| a.toggle_star(id.as_str())));
    g.on_show_info(|id| with_app(|a| a.show_info(id.as_str())));
    g.on_forward_search(|q| with_app(|a| a.forward_search(q.as_str())));
    g.on_forward_toggle(|jid| with_app(|a| a.forward_toggle(jid.as_str())));
    g.on_forward_send(|| with_app(App::forward_send));
    g.on_forward_cancel(|| with_app(App::forward_cancel));
    g.on_panel_close(|| with_app(App::ops_panel_close));
    g.on_starred_open_chat(|| {
        with_app(|a| {
            let chat = a.p().st.borrow().selected.clone();
            a.open_starred(chat);
        });
    });
    g.on_starred_jump(|chat, id| with_app(|a| a.jump_to_message(chat.as_str(), id.as_str())));
    g.on_starred_unstar(|chat, id| with_app(|a| a.starred_unstar(chat.as_str(), id.as_str())));
    g.on_starred_load_more(|| with_app(App::starred_more));
}

impl App {
    /// A loaded message of the open chat.
    fn loaded_msg(&self, id: &str) -> Option<MessageRow> {
        let p = self.p();
        let st = p.st.borrow();
        st.msgs.iter().find(|m| m.id == id).cloned()
    }

    /// A message of the open chat, from the loaded page or else the store.
    fn msg_of_open_chat(&self, id: &str) -> Option<MessageRow> {
        if let Some(m) = self.loaded_msg(id) {
            return Some(m);
        }
        let p = self.p();
        let chat = p.st.borrow().selected.clone()?;
        match p.store.message(&chat, id) {
            Ok(m) => m,
            Err(e) => {
                log::warn!("message lookup failed: {e}");
                None
            }
        }
    }

    // ---- row decoration (called by the list builders in app.rs) -----------------------

    /// Marks the selected and pinned rows and refreshes the pinned strip and the
    /// selection bar. Runs after every rebuild of the message list.
    pub(super) fn ops_decorate(&self, items: &mut [MsgItem]) {
        self.refresh_pin();
        OPS.with(|o| {
            let o = o.borrow();
            for it in items.iter_mut().filter(|i| !i.is_sep) {
                it.selected = o.selected.iter().any(|s| s.as_str() == it.id.as_str());
                it.pinned = o.pinned.as_deref() == Some(it.id.as_str());
            }
        });
        self.publish_bar();
    }

    /// Same for one re-rendered row.
    pub(super) fn ops_decorate_one(&self, item: &mut MsgItem) {
        OPS.with(|o| {
            let o = o.borrow();
            item.selected = o.selected.iter().any(|s| s.as_str() == item.id.as_str());
            item.pinned = o.pinned.as_deref() == Some(item.id.as_str());
        });
    }

    // ---- selection ---------------------------------------------------------------------

    fn begin_select(&self, id: &str) {
        let Some(win) = self.win() else { return };
        self.edit_cancel();
        OPS.with(|o| o.borrow_mut().selected = vec![id.to_string()]);
        win.global::<MsgOps>().set_selecting(true);
        self.sync_selection();
    }

    fn toggle_select(&self, id: &str) {
        let outcome = OPS.with(|o| toggle_limited(&mut o.borrow_mut().selected, id, MAX_FORWARD_MESSAGES));
        if outcome == Toggle::Full {
            self.show_toast("You can select up to 20 messages");
            return;
        }
        self.sync_selection();
    }

    /// Pushes the picked ids to the rows and the bar.
    fn sync_selection(&self) {
        let picked = OPS.with(|o| o.borrow().selected.clone());
        for i in 0..self.msgs_model.row_count() {
            let Some(mut row) = self.msgs_model.row_data(i) else { continue };
            if row.is_sep {
                continue;
            }
            let want = picked.iter().any(|s| s.as_str() == row.id.as_str());
            if row.selected != want {
                row.selected = want;
                self.msgs_model.set_row_data(i, row);
            }
        }
        self.publish_bar();
    }

    fn publish_bar(&self) {
        let Some(win) = self.win() else { return };
        let state = {
            let p = self.p();
            let st = p.st.borrow();
            OPS.with(|o| bar_state(&o.borrow().selected, &st.msgs))
        };
        let g = win.global::<MsgOps>();
        g.set_selected_count(i32::try_from(state.count).unwrap_or(i32::MAX));
        g.set_all_starred(state.all_starred);
    }

    pub(super) fn select_cancel(&self) {
        let Some(win) = self.win() else { return };
        let was = win.global::<MsgOps>().get_selecting();
        OPS.with(|o| o.borrow_mut().selected.clear());
        win.global::<MsgOps>().set_selecting(false);
        self.cancel_select_delete();
        self.sync_selection();
        if was {
            win.invoke_focus_composer();
        }
    }

    /// The picked ids in chat order.
    fn picked_in_order(&self) -> Vec<String> {
        let p = self.p();
        let st = p.st.borrow();
        OPS.with(|o| ordered_ids(&st.msgs, &o.borrow().selected))
    }

    fn select_forward(&self) {
        let picked = OPS.with(|o| o.borrow().selected.len());
        let ids = {
            let p = self.p();
            let st = p.st.borrow();
            OPS.with(|o| shareable_ids(&st.msgs, &o.borrow().selected))
        };
        if ids.is_empty() {
            self.show_toast("These messages can't be forwarded");
            return;
        }
        self.open_forward(ids.clone());
        if let Some(note) = left_out_text(picked, ids.len()) {
            self.show_toast(&note);
        }
    }

    fn select_star(&self) {
        let (chat, rows, all_starred) = {
            let p = self.p();
            let st = p.st.borrow();
            let Some(chat) = st.selected.clone() else { return };
            let ids = OPS.with(|o| o.borrow().selected.clone());
            let all = bar_state(&ids, &st.msgs).all_starred;
            let rows: Vec<MessageRow> = st.msgs.iter().filter(|m| ids.contains(&m.id) && models::can_share(m)).cloned().collect();
            (chat, rows, all)
        };
        let picked = OPS.with(|o| o.borrow().selected.len());
        if let Some(note) = left_out_text(picked, rows.len()) {
            self.show_toast(&note);
        }
        for m in rows.iter().filter(|m| m.starred == all_starred) {
            self.send(Command::StarMessage { chat_jid: chat.clone(), id: m.id.clone(), starred: !all_starred });
        }
        self.select_cancel();
    }

    fn select_copy(&self) {
        let text = {
            let p = self.p();
            let st = p.st.borrow();
            OPS.with(|o| copy_text_of(&st.msgs, &o.borrow().selected))
        };
        if text.is_empty() {
            self.show_toast("Nothing to copy");
        } else {
            self.copy_text(&text);
        }
        self.select_cancel();
    }

    /// "Delete for me" of the selection bar: asks first, naming the count.
    fn select_delete(&self) {
        let Some(win) = self.win() else { return };
        let ids = self.picked_in_order();
        let count = ids.len();
        OPS.with(|o| o.borrow_mut().delete_ids = ids);
        win.set_delete_selected_count(i32::try_from(count).unwrap_or(i32::MAX));
    }

    pub(super) fn cancel_select_delete(&self) {
        OPS.with(|o| o.borrow_mut().delete_ids.clear());
        if let Some(win) = self.win() {
            win.set_delete_selected_count(0);
        }
    }

    /// Deletes exactly the messages the dialog named, whatever the selection is now.
    pub(super) fn confirm_select_delete(&self) {
        let ids = OPS.with(|o| std::mem::take(&mut o.borrow_mut().delete_ids));
        self.cancel_select_delete();
        for id in ids {
            self.delete_message(&id, false);
        }
        self.select_cancel();
    }

    // ---- forward -----------------------------------------------------------------------

    fn forward_one(&self, id: &str) {
        self.open_forward(vec![id.to_string()]);
    }

    fn open_forward(&self, ids: Vec<String>) {
        let Some(win) = self.win() else { return };
        if let Err(why) = check_forward(ids.len(), 1) {
            self.show_toast(why);
            return;
        }
        let Some(from) = self.p().st.borrow().selected.clone() else { return };
        let msgs = i32::try_from(ids.len()).unwrap_or(i32::MAX);
        let model = Rc::new(VecModel::<PickItem>::default());
        OPS.with(|o| {
            let mut o = o.borrow_mut();
            o.forward = Some(ForwardDraft { from, ids, query: String::new(), picked: Vec::new() });
            o.forward_model = Some(model.clone());
        });
        let g = win.global::<MsgOps>();
        g.set_forward_chats(ModelRc::from(model));
        g.set_forward_msgs(msgs);
        g.set_forward_open(true);
        self.refresh_picker();
        let groups: Vec<String> = pickable_chats(&self.p().st.borrow().chats, "")
            .into_iter()
            .filter(|r| r.kind == ChatKind::Group)
            .map(|r| r.jid)
            .collect();
        self.ensure_picker_gates(&groups);
    }

    /// Rebuilds the picker rows for the current search text, most recent chat first.
    pub(super) fn refresh_picker(&self) {
        let Some(win) = self.win() else { return };
        let (query, picked, model) = OPS.with(|o| {
            let o = o.borrow();
            let Some(f) = &o.forward else { return (String::new(), Vec::new(), None) };
            (f.query.clone(), f.picked.clone(), o.forward_model.clone())
        });
        let Some(model) = model else { return };
        let p = self.p();
        let blocked = self.blocked_matcher(&p);
        let profile = p.id.clone();
        let rows: Vec<ChatRow> = {
            let st = p.st.borrow();
            // Blocked contacts cannot be written to, so they are not offered.
            pickable_chats(&st.chats, &query).into_iter().filter(|r| !(r.kind == ChatKind::Dm && blocked(&r.jid))).collect()
        };
        let mut items = Vec::with_capacity(rows.len());
        {
            let mut st = p.st.borrow_mut();
            for r in &rows {
                let avatar = self.avatar(&p, &mut st, &r.jid);
                items.push(PickItem {
                    jid: r.jid.as_str().into(),
                    name: r.display_name.as_str().into(),
                    initials: models::initials(&r.display_name).into(),
                    tint: models::tint_for(&r.jid),
                    has_avatar: avatar.is_some(),
                    avatar: avatar.unwrap_or_default(),
                    selected: picked.contains(&r.jid),
                    note: gate_label(self.gate_of(&profile, &r.jid)).into(),
                });
            }
        }
        model.set_vec(items);
        self.publish_picked(&win);
    }

    fn publish_picked(&self, win: &MainWindow) {
        let picked = OPS.with(|o| o.borrow().forward.as_ref().map(|f| f.picked.clone()).unwrap_or_default());
        let names: Vec<String> = {
            let p = self.p();
            let st = p.st.borrow();
            picked
                .iter()
                .map(|j| super::find_chat(&st.chats, j).map_or_else(|| j.clone(), |c| c.display_name.clone()))
                .collect()
        };
        let g = win.global::<MsgOps>();
        g.set_forward_picked(i32::try_from(picked.len()).unwrap_or(i32::MAX));
        g.set_forward_note(if names.is_empty() { "Pick up to 5 chats".into() } else { names.join(", ").into() });
    }

    fn forward_search(&self, q: &str) {
        OPS.with(|o| {
            if let Some(f) = o.borrow_mut().forward.as_mut() {
                f.query = q.trim().to_string();
            }
        });
        self.refresh_picker();
    }

    fn forward_toggle(&self, jid: &str) {
        let Some(win) = self.win() else { return };
        let gated = self.gate_of(&self.p().id, jid);
        if gated != SendGate::Open {
            self.show_toast(gate_label(gated));
            return;
        }
        let outcome = OPS.with(|o| {
            o.borrow_mut().forward.as_mut().map(|f| toggle_limited(&mut f.picked, jid, MAX_FORWARD_CHATS))
        });
        let Some(outcome) = outcome else { return };
        let model = OPS.with(|o| o.borrow().forward_model.clone());
        if outcome != Toggle::Full {
            if let Some(model) = model {
                for i in 0..model.row_count() {
                    let Some(mut row) = model.row_data(i) else { continue };
                    if row.jid.as_str() == jid {
                        row.selected = outcome == Toggle::Added;
                        model.set_row_data(i, row);
                        break;
                    }
                }
            }
        }
        self.publish_picked(&win);
        if outcome == Toggle::Full {
            win.global::<MsgOps>().set_forward_note("You can forward to up to 5 chats".into());
        }
    }

    fn forward_send(&self) {
        let draft = OPS.with(|o| o.borrow_mut().forward.take());
        let Some(d) = draft else { return };
        if let Err(why) = check_forward(d.ids.len(), d.picked.len()) {
            self.show_toast(why);
            OPS.with(|o| o.borrow_mut().forward = Some(d));
            return;
        }
        // A group's send gate can become known after it was picked.
        let profile = self.p().id.clone();
        let (open, dropped) = split_gated(d.picked.clone(), |j| !self.gate_notice(&profile, j).is_empty());
        if open.is_empty() {
            self.show_toast("None of the picked chats accepts messages from you");
            OPS.with(|o| o.borrow_mut().forward = Some(d));
            return;
        }
        let count = open.len();
        self.close_forward_ui();
        self.send(Command::ForwardMessages { from_chat: d.from, ids: d.ids, to_chats: open });
        self.select_cancel();
        self.show_toast(&if dropped > 0 {
            format!("Forwarding to {count} chat(s); {dropped} skipped (you can't send there)")
        } else if count == 1 {
            "Forwarding…".to_string()
        } else {
            format!("Forwarding to {count} chats…")
        });
    }

    fn forward_cancel(&self) {
        OPS.with(|o| o.borrow_mut().forward = None);
        self.close_forward_ui();
        if let Some(win) = self.win() {
            win.invoke_focus_composer();
        }
    }

    fn close_forward_ui(&self) {
        OPS.with(|o| {
            let mut o = o.borrow_mut();
            o.forward = None;
            o.forward_model = None;
        });
        if let Some(win) = self.win() {
            let g = win.global::<MsgOps>();
            g.set_forward_open(false);
            g.set_forward_chats(ModelRc::default());
            g.set_forward_picked(0);
            g.set_forward_note(Default::default());
        }
    }

    // ---- edit --------------------------------------------------------------------------

    pub(super) fn begin_edit_message(&self, id: &str) {
        let Some(chat) = self.p().st.borrow().selected.clone() else { return };
        let now = chrono::Utc::now().timestamp();
        match self.msg_of_open_chat(id) {
            Some(m) if models::can_edit(&m, now) => {
                let text = m.text.clone().unwrap_or_default();
                self.begin_edit(&chat, id, &text);
            }
            _ => self.show_toast("This message can no longer be edited"),
        }
    }

    /// Switches the composer to edit mode for message `id` of `chat`: the
    /// original text is put in, an "Editing message" strip shows above it, and
    /// the next send becomes an `EditMessage`. Esc or the strip's cross cancels.
    pub fn begin_edit(&self, chat: &str, id: &str, text: &str) {
        let Some(win) = self.win() else { return };
        if self.p().st.borrow().selected.as_deref() != Some(chat) {
            return;
        }
        let already = OPS.with(|o| o.borrow().edit.as_ref().map(|e| (e.prev_text.clone(), e.prev_reply.clone())));
        let (prev_text, prev_reply) = match already {
            Some(prev) => prev,
            None => (win.invoke_composer_text().to_string(), self.p().st.borrow().reply.clone()),
        };
        OPS.with(|o| {
            o.borrow_mut().edit =
                Some(EditTarget { chat: chat.to_string(), id: id.to_string(), prev_text, prev_reply });
        });
        let g = win.global::<MsgOps>();
        g.set_edit_text(one_line(text).into());
        // Editing first: dropping the quote below must not overwrite the saved draft.
        g.set_editing(true);
        self.cancel_reply();
        win.invoke_set_composer(text.into(), i32::try_from(text.len()).unwrap_or(i32::MAX));
        win.invoke_focus_composer();
    }

    pub(super) fn edit_cancel(&self) {
        let Some(target) = OPS.with(|o| o.borrow_mut().edit.take()) else { return };
        if let Some(win) = self.win() {
            win.global::<MsgOps>().set_editing(false);
            win.invoke_focus_composer();
        }
        self.restore_composer(&target.prev_text);
        self.restore_reply(&target.chat, target.prev_reply);
    }

    /// Puts the quote from before an edit back, if its chat is still open.
    fn restore_reply(&self, chat: &str, reply: Option<(String, String, String)>) {
        if reply.is_none() {
            return;
        }
        {
            let p = self.p();
            let mut st = p.st.borrow_mut();
            if st.selected.as_deref() != Some(chat) {
                return;
            }
            st.reply = reply;
        }
        self.sync_reply();
    }

    fn restore_composer(&self, text: &str) {
        let Some(win) = self.win() else { return };
        if text.is_empty() {
            win.invoke_clear_composer();
        } else {
            win.invoke_set_composer(text.into(), i32::try_from(text.len()).unwrap_or(i32::MAX));
        }
        self.draft_touch();
    }

    /// The composer's send while in edit mode. True when the send was an edit
    /// (or was refused as one) and must not go out as a new message.
    pub(super) fn ops_edit_send(&self, text: &str) -> bool {
        let Some(target) = OPS.with(|o| o.borrow().edit.clone()) else { return false };
        let chat = self.p().st.borrow().selected.clone();
        if chat.as_deref() != Some(target.chat.as_str()) {
            OPS.with(|o| o.borrow_mut().edit = None);
            if let Some(win) = self.win() {
                win.global::<MsgOps>().set_editing(false);
            }
            // The text belongs to the edit, never to a new message in this chat.
            return true;
        }
        let now = chrono::Utc::now().timestamp();
        match self.msg_of_open_chat(&target.id) {
            Some(m) if models::can_edit(&m, now) => {
                if m.text.as_deref() != Some(text) {
                    self.send(Command::EditMessage { chat_jid: target.chat.clone(), id: target.id.clone(), text: text.to_string() });
                }
            }
            _ => self.show_banner(1, "This message can no longer be edited"),
        }
        self.stop_composing();
        self.close_mentions();
        OPS.with(|o| o.borrow_mut().edit = None);
        if let Some(win) = self.win() {
            win.global::<MsgOps>().set_editing(false);
        }
        // The composer view empties itself once the send callback returns; the
        // text from before the edit goes back right after that.
        let (prev, prev_reply, chat) = (target.prev_text, target.prev_reply, target.chat);
        RESTORE.with(|t| {
            t.start(TimerMode::SingleShot, Duration::from_millis(0), move || {
                with_app(|a| {
                    a.restore_composer(&prev);
                    a.restore_reply(&chat, prev_reply.clone());
                });
            });
        });
        true
    }

    // ---- pin ---------------------------------------------------------------------------

    fn pin_chosen(&self, id: &str, choice: i32) {
        let Some(secs) = pin_duration_secs(choice) else { return };
        let Some(chat) = self.p().st.borrow().selected.clone() else { return };
        self.send(Command::PinMessage { chat_jid: chat, id: id.to_string(), pin: true, duration_secs: secs });
    }

    fn unpin(&self, id: &str) {
        let Some(chat) = self.p().st.borrow().selected.clone() else { return };
        self.send(Command::PinMessage { chat_jid: chat, id: id.to_string(), pin: false, duration_secs: 0 });
    }

    fn pin_unpin(&self) {
        if let Some(id) = OPS.with(|o| o.borrow().pinned.clone()) {
            self.unpin(&id);
        }
    }

    fn pin_jump(&self) {
        let Some(id) = OPS.with(|o| o.borrow().pinned.clone()) else { return };
        let Some(chat) = self.p().st.borrow().selected.clone() else { return };
        self.jump_to_message(&chat, &id);
    }

    /// Looks the pinned message of the open chat up and updates the strip.
    fn refresh_pin(&self) {
        let Some(win) = self.win() else { return };
        let p = self.p();
        let chat = p.st.borrow().selected.clone();
        let pin = match &chat {
            Some(c) => match p.store.pinned_message(c) {
                Ok(v) => v,
                Err(e) => {
                    log::warn!("pinned message lookup failed: {e}");
                    None
                }
            },
            None => None,
        };
        let text = match (&pin, &chat) {
            (Some((id, _)), Some(c)) => {
                let row = p.st.borrow().msgs.iter().find(|m| &m.id == id).cloned().or_else(|| p.store.message(c, id).ok().flatten());
                row.map_or_else(|| "Pinned message".to_string(), |m| one_line(&models::reply_preview(&m)))
            }
            _ => String::new(),
        };
        OPS.with(|o| o.borrow_mut().pinned = pin.map(|(id, _)| id));
        let g = win.global::<MsgOps>();
        g.set_pin_visible(!text.is_empty());
        g.set_pin_text(text.into());
    }

    // ---- star --------------------------------------------------------------------------

    fn toggle_star(&self, id: &str) {
        let Some(chat) = self.p().st.borrow().selected.clone() else { return };
        let Some(m) = self.msg_of_open_chat(id) else { return };
        self.send(Command::StarMessage { chat_jid: chat, id: id.to_string(), starred: !m.starred });
    }

    /// Opens the starred-messages panel for `chat`, or for every chat when `None`.
    pub fn open_starred(&self, chat: Option<String>) {
        let Some(win) = self.win() else { return };
        self.info_close();
        self.close_info_panel_state();
        let title = match &chat {
            Some(c) => {
                let name = {
                    let p = self.p();
                    let st = p.st.borrow();
                    super::find_chat(&st.chats, c).map(|r| r.display_name.clone())
                };
                match name {
                    Some(n) => format!("Starred · {n}"),
                    None => "Starred messages".to_string(),
                }
            }
            None => "Starred messages".to_string(),
        };
        OPS.with(|o| o.borrow_mut().starred = Some(StarredView { chat, cursor: None, rows: Vec::new(), more: false }));
        let g = win.global::<MsgOps>();
        g.set_starred_title(title.into());
        g.set_starred_rows(ModelRc::default());
        g.set_panel(2);
        self.starred_load(true, STARRED_WANT);
    }

    /// Loads the next page (or, with `fresh`, the list again from the top, at
    /// least `want` rows deep) of the starred list.
    fn starred_load(&self, fresh: bool, want: usize) {
        let p = self.p();
        let (chat, cursor, before) = OPS.with(|o| {
            let o = o.borrow();
            match o.starred.as_ref() {
                Some(s) => (s.chat.clone(), if fresh { None } else { s.cursor.clone() }, s.rows.clone()),
                None => (None, None, Vec::new()),
            }
        });
        let key = cursor.as_ref().map(|(ts, c, i)| (*ts, c.as_str(), i.as_str()));
        let rows = match p.store.starred_messages(want as u32, chat.as_deref(), key) {
            Ok(rows) => rows,
            Err(e) => {
                log::warn!("starred messages failed: {e}");
                Vec::new()
            }
        };
        let more = rows.len() >= want;
        let cursor = rows.last().map(|m| (m.ts, m.chat_jid.clone(), m.id.clone())).or(cursor);
        // Whole rows are compared, so an edit or a revoke of a shown message republishes.
        let unchanged = fresh
            && rows == before
            && OPS.with(|o| o.borrow().starred.as_ref().is_some_and(|s| s.more == more));
        OPS.with(|o| {
            if let Some(s) = o.borrow_mut().starred.as_mut() {
                if fresh {
                    s.rows.clear();
                }
                s.rows.extend(rows);
                s.cursor = cursor;
                s.more = more;
            }
        });
        if !unchanged {
            self.publish_starred();
        }
    }

    /// A message of `chat` changed (a star toggled here or on the phone, a new
    /// message...) while the starred panel is open: reload what it shows, as
    /// deep as the user has already paged.
    pub(super) fn starred_changed(&self, chat: &str) {
        let Some(win) = self.win() else { return };
        if win.global::<MsgOps>().get_panel() != 2 {
            return;
        }
        let depth = OPS.with(|o| {
            o.borrow().starred.as_ref().filter(|s| s.chat.as_deref().is_none_or(|c| c == chat)).map(|s| s.rows.len())
        });
        if depth.is_some() {
            // Bursts of changes (a forward, a sync) reload once.
            STAR_RELOAD.with(|t| {
                t.start(TimerMode::SingleShot, STARRED_DEBOUNCE, || with_app(App::starred_reload));
            });
        }
    }

    fn starred_reload(&self) {
        let depth = OPS.with(|o| o.borrow().starred.as_ref().map(|s| s.rows.len()));
        if let Some(depth) = depth {
            self.starred_load(true, depth.max(STARRED_WANT));
        }
    }

    fn starred_more(&self) {
        self.starred_load(false, STARRED_WANT);
    }

    fn publish_starred(&self) {
        let Some(win) = self.win() else { return };
        let now = Local::now();
        let (chat_specific, rows, more) = OPS.with(|o| {
            let o = o.borrow();
            match &o.starred {
                Some(s) => (s.chat.is_some(), s.rows.clone(), s.more),
                None => (false, Vec::new(), false),
            }
        });
        let p = self.p();
        let items: Vec<StarredRow> = rows
            .iter()
            .map(|m| {
                let chat_name = if chat_specific {
                    String::new()
                } else {
                    let st = p.st.borrow();
                    super::find_chat(&st.chats, &m.chat_jid)
                        .map(|c| c.display_name.clone())
                        .or_else(|| p.store.chat(&m.chat_jid).ok().flatten().map(|c| c.display_name))
                        .unwrap_or_default()
                };
                StarredRow {
                    id: m.id.as_str().into(),
                    chat_jid: m.chat_jid.as_str().into(),
                    chat_name: chat_name.into(),
                    // In a direct chat the chat name already says who wrote it.
                    sender: if !chat_specific && !m.from_me && ChatKind::from_jid(&m.chat_jid) == ChatKind::Dm {
                        String::new()
                    } else {
                        self.sender_label(&p, m)
                    }
                    .into(),
                    text: models::reply_preview(m).into(),
                    time: when_text(m.ts, now).into(),
                }
            })
            .collect();
        let g = win.global::<MsgOps>();
        g.set_starred_rows(ModelRc::new(VecModel::from(items)));
        g.set_starred_more(more);
        g.set_starred_loading(false);
    }

    fn sender_label(&self, p: &super::ProfileState, m: &MessageRow) -> String {
        if m.from_me {
            return "You".to_string();
        }
        if let Some(n) = m.sender_name.as_deref().filter(|n| !n.trim().is_empty()) {
            return n.to_string();
        }
        let contact = p.store.contact(&m.sender_jid).ok().flatten();
        contact_label(contact.as_ref(), &m.sender_jid)
    }

    fn starred_unstar(&self, chat: &str, id: &str) {
        self.send(Command::StarMessage { chat_jid: chat.to_string(), id: id.to_string(), starred: false });
        OPS.with(|o| {
            if let Some(s) = o.borrow_mut().starred.as_mut() {
                s.rows.retain(|m| !(m.chat_jid == chat && m.id == id));
            }
        });
        self.publish_starred();
    }

    /// Opens `chat` if needed and scrolls to message `id`, highlighting it briefly.
    pub(super) fn jump_to_message(&self, chat: &str, id: &str) {
        let same = self.p().st.borrow().selected.as_deref() == Some(chat);
        if !same {
            self.select_chat(chat);
        }
        // select_chat schedules a scroll to the bottom; the jump replaces it.
        self.fix_timer.stop();
        *self.fix_run.borrow_mut() = None;
        let id = id.to_string();
        let go = move || {
            with_app(|a| {
                a.search_jump(&id);
                HIGHLIGHT.with(|t| {
                    t.start(TimerMode::SingleShot, JUMP_HIGHLIGHT, || {
                        with_app(|a| {
                            if let Some(w) = a.win().filter(|w| !w.get_search_open()) {
                                w.set_highlight_id(Default::default());
                            }
                        });
                    });
                });
            });
        };
        if same {
            go();
        } else {
            JUMP.with(|t| t.start(TimerMode::SingleShot, JUMP_AFTER_OPEN, go));
        }
    }

    // ---- message info ------------------------------------------------------------------

    fn show_info(&self, id: &str) {
        let Some(win) = self.win() else { return };
        let Some(chat) = self.p().st.borrow().selected.clone() else { return };
        let Some(m) = self.msg_of_open_chat(id).filter(|m| m.from_me && !m.revoked) else { return };
        self.info_close();
        self.close_info_panel_state();
        OPS.with(|o| o.borrow_mut().info = Some((chat.clone(), id.to_string())));
        let g = win.global::<MsgOps>();
        g.set_info_preview(models::reply_preview(&m).into());
        g.set_info_time(info_time_text(m.ts, m.status, Local::now()).into());
        g.set_info_compact(ChatKind::from_jid(&chat) != ChatKind::Group);
        g.set_info_rows(ModelRc::default());
        g.set_info_loading(true);
        g.set_panel(1);
        self.send(Command::FetchMessageInfo { chat_jid: chat, id: id.to_string() });
        INFO_WAIT.with(|t| {
            t.start(TimerMode::SingleShot, INFO_TIMEOUT, || {
                // No answer: show what the message row alone tells.
                with_app(|a| {
                    if let Some((chat, id)) = OPS.with(|o| o.borrow().info.clone()) {
                        a.on_message_info(true, &chat, &id, Vec::new());
                    }
                });
            });
        });
    }

    /// Engine answer to `FetchMessageInfo`.
    pub(super) fn on_message_info(&self, active: bool, chat: &str, id: &str, receipts: Vec<ReceiptEntry>) {
        if !active {
            return;
        }
        let open = OPS.with(|o| o.borrow().info.clone());
        if open.as_ref().is_none_or(|(c, i)| c != chat || i != id) {
            return;
        }
        let Some(win) = self.win() else { return };
        INFO_WAIT.with(Timer::stop);
        let mut list = receipts;
        sort_receipts(&mut list);
        let now = Local::now();
        let p = self.p();
        let seen = |s: Seen| match s {
            Seen::No => String::new(),
            Seen::Yes => "Yes".to_string(),
            Seen::At(t) => when_text(t, now),
        };
        let when = |t: Option<i64>| t.map(|t| when_text(t, now)).unwrap_or_default();
        let message = match p.store.message(chat, id) {
            Ok(m) => m,
            Err(e) => {
                log::warn!("message lookup failed: {e}");
                None
            }
        };
        let status = message.as_ref().map(|m| m.status);
        let rows: Vec<ReceiptRow> = if ChatKind::from_jid(chat) == ChatKind::Group {
            list.iter()
                .map(|r| {
                    let contact = p.store.contact(&r.jid).ok().flatten();
                    let name = contact_label(contact.as_ref(), &r.jid);
                    ReceiptRow {
                        initials: models::initials(&name).into(),
                        tint: models::tint_for(&r.jid),
                        name: name.into(),
                        read: when(r.read_at).into(),
                        delivered: when(r.delivered_at).into(),
                        played: when(r.played_at).into(),
                    }
                })
                .collect()
        } else {
            // A direct chat always shows its one block; without receipt rows
            // (older messages) it comes from the message's own status.
            let (read, delivered, played) = dm_seen(list.first(), status.unwrap_or_default());
            let name = contact_label(p.store.contact(chat).ok().flatten().as_ref(), chat);
            vec![ReceiptRow {
                initials: models::initials(&name).into(),
                tint: models::tint_for(chat),
                name: name.into(),
                read: seen(read).into(),
                delivered: seen(delivered).into(),
                played: seen(played).into(),
            }]
        };
        let g = win.global::<MsgOps>();
        if let Some(m) = &message {
            g.set_info_time(info_time_text(m.ts, m.status, now).into());
        }
        g.set_info_rows(ModelRc::new(VecModel::from(rows)));
        g.set_info_loading(false);
    }

    // ---- panels and resets -------------------------------------------------------------

    fn close_info_panel_state(&self) {
        INFO_WAIT.with(Timer::stop);
        OPS.with(|o| {
            let mut o = o.borrow_mut();
            o.info = None;
            o.starred = None;
        });
    }

    pub(super) fn ops_panel_close(&self) {
        self.close_info_panel_state();
        if let Some(win) = self.win() {
            let g = win.global::<MsgOps>();
            g.set_panel(0);
            g.set_starred_rows(ModelRc::default());
            g.set_info_rows(ModelRc::default());
        }
    }

    /// The open chat is about to change or close. An edit is undone, the
    /// selection, picker and pin dialog end, and the info panel closes; the
    /// starred list survives a switch of chat (it is how one jumps) but not a
    /// close.
    pub(super) fn ops_chat_leaving(&self, keep_starred: bool) {
        self.edit_cancel();
        self.select_cancel();
        self.close_forward_ui();
        let Some(win) = self.win() else { return };
        let g = win.global::<MsgOps>();
        g.set_pin_ask_id(Default::default());
        let panel = g.get_panel();
        if panel == 1 || (panel == 2 && !keep_starred) {
            self.ops_panel_close();
        }
    }

    /// The profile in the window changed: all of it belongs to the old one.
    pub(super) fn ops_profile_changed(&self) {
        OPS.with(|o| *o.borrow_mut() = Ops::default());
        INFO_WAIT.with(Timer::stop);
        let Some(win) = self.win() else { return };
        let g = win.global::<MsgOps>();
        g.set_selecting(false);
        g.set_editing(false);
        g.set_forward_open(false);
        g.set_forward_chats(ModelRc::default());
        g.set_pin_ask_id(Default::default());
        g.set_pin_visible(false);
        g.set_panel(0);
        g.set_starred_rows(ModelRc::default());
        g.set_info_rows(ModelRc::default());
        g.set_selected_count(0);
    }

    /// Closes the topmost modal of these features (Esc).
    pub(super) fn ops_dialog_close(&self) {
        let Some(win) = self.win() else { return };
        if win.global::<MsgOps>().get_forward_open() {
            self.forward_cancel();
        } else {
            win.global::<MsgOps>().set_pin_ask_id(Default::default());
        }
    }
}

#[cfg(debug_assertions)]
impl App {
    /// `WACLIENT_DEMO_UI=seq:` steps (`ops:` prefix) that drive the same
    /// callbacks the window uses: `select:ID`, `toggle:ID`, `forward:ID`,
    /// `pick:JID` (forward picker), `send-forward`, `edit:ID`, `type:TEXT`,
    /// `send`, `pinask:ID`, `pin:ID:CHOICE`, `unpin:ID`, `star:ID`, `info:ID`,
    /// `starred`, `starred-chat`, `jump:CHAT:ID`, `state`.
    pub(super) fn demo_msgops(&self, what: &str) {
        let Some(win) = self.win() else { return };
        let g = win.global::<MsgOps>();
        let (verb, rest) = what.split_once(':').unwrap_or((what, ""));
        match verb {
            "select" => g.invoke_begin_select(rest.into()),
            "toggle" => g.invoke_toggle_select(rest.into()),
            "forward" => g.invoke_forward_one(rest.into()),
            "sel-forward" => g.invoke_select_forward(),
            "sel-star" => g.invoke_select_star(),
            "sel-copy" => g.invoke_select_copy(),
            "sel-delete" => g.invoke_select_delete(),
            "pick" => g.invoke_forward_toggle(rest.into()),
            "fsearch" => g.invoke_forward_search(rest.into()),
            "send-forward" => g.invoke_forward_send(),
            "edit" => g.invoke_begin_edit(rest.into()),
            "type" => win.invoke_set_composer(rest.into(), i32::try_from(rest.len()).unwrap_or(0)),
            "send" => {
                let text = win.invoke_composer_text();
                win.invoke_send_message(text);
            }
            "pinask" => g.set_pin_ask_id(rest.into()),
            "pin" => {
                let (id, choice) = rest.split_once(':').unwrap_or((rest, "0"));
                g.invoke_pin_chosen(id.into(), choice.parse().unwrap_or(0));
            }
            "unpin" => g.invoke_unpin(rest.into()),
            "star" => g.invoke_toggle_star(rest.into()),
            "info" => g.invoke_show_info(rest.into()),
            "starred" => self.open_starred(None),
            "starred-chat" => g.invoke_starred_open_chat(),
            "jump" => {
                let (chat, id) = rest.split_once(':').unwrap_or((rest, ""));
                g.invoke_starred_jump(chat.into(), id.into());
            }
            "state" => log::debug!(
                "demo: selecting={} selected={} editing={} forward_open={} picked={} panel={} pin_visible={} rows={} info_rows={} starred_rows={} composer_len={}",
                g.get_selecting(),
                g.get_selected_count(),
                g.get_editing(),
                g.get_forward_open(),
                g.get_forward_picked(),
                g.get_panel(),
                g.get_pin_visible(),
                self.msgs_model.row_count(),
                g.get_info_rows().row_count(),
                g.get_starred_rows().row_count(),
                win.invoke_composer_text().chars().count(),
            ),
            other => log::warn!("demo: unknown ops action {other}"),
        }
    }
}

/// Chats a message can be forwarded to, most recent first, narrowed by `query`.
fn pickable_chats(all: &[ChatRow], query: &str) -> Vec<ChatRow> {
    let q = query.to_lowercase();
    let mut rows: Vec<ChatRow> = all
        .iter()
        .filter(|r| r.kind != ChatKind::Other && r.jid != STATUS_JID)
        .filter(|r| q.is_empty() || r.display_name.to_lowercase().contains(&q))
        .cloned()
        .collect();
    rows.sort_by(|a, b| b.last_ts.cmp(&a.last_ts));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: &str, chat: &str, ts: i64, starred: bool) -> MessageRow {
        MessageRow { id: id.into(), chat_jid: chat.into(), ts, starred, text: Some(format!("text {id}")), ..Default::default() }
    }

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn pin_choices_map_to_whatsapp_durations() {
        assert_eq!(pin_duration_secs(0), Some(86_400));
        assert_eq!(pin_duration_secs(1), Some(604_800));
        assert_eq!(pin_duration_secs(2), Some(2_592_000));
        assert_eq!(pin_duration_secs(3), None);
        assert_eq!(pin_duration_secs(-1), None);
    }

    #[test]
    fn selection_toggles_and_stops_at_the_limit() {
        let mut sel = Vec::new();
        assert_eq!(toggle_limited(&mut sel, "a", 2), Toggle::Added);
        assert_eq!(toggle_limited(&mut sel, "b", 2), Toggle::Added);
        assert_eq!(toggle_limited(&mut sel, "c", 2), Toggle::Full);
        assert_eq!(sel, ids(&["a", "b"]));
        assert_eq!(toggle_limited(&mut sel, "a", 2), Toggle::Removed);
        assert_eq!(toggle_limited(&mut sel, "c", 2), Toggle::Added);
        assert_eq!(sel, ids(&["b", "c"]));
    }

    #[test]
    fn bar_offers_unstar_only_when_everything_picked_is_starred() {
        let msgs = vec![msg("a", "c", 1, true), msg("b", "c", 2, false), msg("c", "c", 3, true)];
        assert_eq!(bar_state(&[], &msgs), BarState { count: 0, all_starred: false });
        assert_eq!(bar_state(&ids(&["a"]), &msgs), BarState { count: 1, all_starred: true });
        assert_eq!(bar_state(&ids(&["a", "c"]), &msgs), BarState { count: 2, all_starred: true });
        assert_eq!(bar_state(&ids(&["a", "b"]), &msgs), BarState { count: 2, all_starred: false });
        // A picked id that is no longer loaded is not starred as far as we know.
        assert!(!bar_state(&ids(&["gone"]), &msgs).all_starred);
    }

    #[test]
    fn picked_messages_forward_in_chat_order() {
        let msgs = vec![msg("a", "c", 1, false), msg("b", "c", 2, false), msg("c", "c", 3, false)];
        assert_eq!(ordered_ids(&msgs, &ids(&["c", "a", "zzz"])), ids(&["a", "c"]));
    }

    #[test]
    fn forward_limits() {
        assert!(check_forward(1, 1).is_ok());
        assert!(check_forward(20, 5).is_ok());
        assert!(check_forward(0, 1).is_err());
        assert!(check_forward(21, 1).is_err());
        assert!(check_forward(1, 0).is_err());
        assert!(check_forward(1, 6).is_err());
    }

    #[test]
    fn copy_joins_live_texts_oldest_first() {
        let mut gone = msg("b", "c", 2, false);
        gone.revoked = true;
        let mut blank = msg("d", "c", 4, false);
        blank.text = Some("  ".into());
        let msgs = vec![msg("a", "c", 1, false), gone, msg("c", "c", 3, false), blank];
        assert_eq!(copy_text_of(&msgs, &ids(&["c", "a", "b", "d"])), "text a\ntext c");
    }

    #[test]
    fn when_text_names_today_and_yesterday() {
        let now = Local.with_ymd_and_hms(2026, 3, 10, 15, 0, 0).unwrap();
        let at = |d, h, m| Local.with_ymd_and_hms(2026, 3, d, h, m, 0).unwrap().timestamp();
        assert_eq!(when_text(at(10, 9, 5), now), "Today, 09:05");
        assert_eq!(when_text(at(9, 23, 59), now), "Yesterday, 23:59");
        assert_eq!(when_text(at(1, 8, 0), now), "01/03/2026, 08:00");
    }

    fn contact(full: Option<&str>, push: Option<&str>, phone: Option<&str>) -> ContactRow {
        ContactRow {
            jid: "49@s.whatsapp.net".into(),
            full_name: full.map(Into::into),
            push_name: push.map(Into::into),
            phone: phone.map(Into::into),
            avatar_path: None,
            avatar_id: None,
            avatar_checked_at: 0,
            about: None,
            about_set_at: None,
        }
    }

    #[test]
    fn contacts_are_named_by_saved_name_then_push_name_then_number() {
        let jid = "49@s.whatsapp.net";
        assert_eq!(contact_label(Some(&contact(None, None, None)), jid), "+49");
        assert_eq!(contact_label(Some(&contact(None, None, Some("4915112345678"))), jid), "+4915112345678");
        assert_eq!(contact_label(Some(&contact(None, Some("Pushy"), Some("1"))), jid), "Pushy");
        assert_eq!(contact_label(Some(&contact(Some("Saved Name"), Some("Pushy"), None)), jid), "Saved Name");
        assert_eq!(contact_label(Some(&contact(Some("  "), Some("Pushy"), None)), jid), "Pushy");
        assert_eq!(contact_label(None, "lid-abc@lid"), "lid-abc");
    }

    #[test]
    fn receipts_sort_readers_first_then_delivered() {
        let r = |jid: &str, d: Option<i64>, rd: Option<i64>| ReceiptEntry { jid: jid.into(), delivered_at: d, read_at: rd, played_at: None };
        let mut list = vec![r("none", None, None), r("deliv", Some(5), None), r("read-old", Some(1), Some(10)), r("read-new", Some(1), Some(20))];
        sort_receipts(&mut list);
        let order: Vec<&str> = list.iter().map(|e| e.jid.as_str()).collect();
        assert_eq!(order, ["read-new", "read-old", "deliv", "none"]);
    }

    #[test]
    fn direct_info_falls_back_to_the_message_status_without_receipt_rows() {
        use MessageStatus::*;
        assert_eq!(dm_seen(None, Pending), (Seen::No, Seen::No, Seen::No));
        assert_eq!(dm_seen(None, Sent), (Seen::No, Seen::No, Seen::No));
        assert_eq!(dm_seen(None, Delivered), (Seen::No, Seen::Yes, Seen::No));
        assert_eq!(dm_seen(None, Read), (Seen::Yes, Seen::Yes, Seen::No));
        assert_eq!(dm_seen(None, Played), (Seen::Yes, Seen::Yes, Seen::Yes));
        assert_eq!(dm_seen(None, Failed), (Seen::No, Seen::No, Seen::No));
    }

    #[test]
    fn direct_info_keeps_known_times_and_implies_earlier_stages() {
        let e = ReceiptEntry { jid: "1@s.whatsapp.net".into(), delivered_at: None, read_at: Some(50), played_at: None };
        assert_eq!(dm_seen(Some(&e), MessageStatus::Sent), (Seen::At(50), Seen::Yes, Seen::No));
        let e = ReceiptEntry { jid: "1@s.whatsapp.net".into(), delivered_at: Some(40), read_at: None, played_at: None };
        assert_eq!(dm_seen(Some(&e), MessageStatus::Read), (Seen::Yes, Seen::At(40), Seen::No));
    }

    #[test]
    fn info_labels_name_every_status() {
        assert_eq!(status_label(MessageStatus::Sent), "Sent");
        assert_eq!(status_label(MessageStatus::Delivered), "Delivered");
        assert_eq!(status_label(MessageStatus::Read), "Read");
    }

    #[test]
    fn shareable_ids_drop_revoked_and_system_rows() {
        let mut revoked = msg("r", "x", 1, false);
        revoked.revoked = true;
        let mut other = msg("o", "x", 2, false);
        other.kind = wa_store::MessageKind::Other;
        let msgs = vec![revoked, other, msg("t", "x", 3, false)];
        let picked = vec!["t".to_string(), "r".to_string(), "o".to_string()];
        assert_eq!(shareable_ids(&msgs, &picked), ["t"]);
        assert_eq!(left_out_text(3, 1).as_deref(), Some("2 selected messages can't be used here and were left out"));
        assert_eq!(left_out_text(2, 2), None);
    }

    #[test]
    fn picker_lists_recent_chats_first_and_skips_status() {
        let chat = |jid: &str, name: &str, ts: i64, kind: ChatKind| ChatRow {
            jid: jid.into(),
            kind,
            display_name: name.into(),
            last_ts: ts,
            unread: 0,
            pinned: false,
            archived: false,
            muted_until: 0,
            last_preview: None,
            receipts_off: false,
            ephemeral_secs: 0,
            ephemeral_ts: 0,
        };
        let all = vec![
            chat("1@s.whatsapp.net", "Alice", 10, ChatKind::Dm),
            chat("2@s.whatsapp.net", "Bob", 30, ChatKind::Dm),
            chat(STATUS_JID, "Status", 50, ChatKind::Other),
            chat("3@g.us", "Alicia Group", 20, ChatKind::Group),
        ];
        let names = |q: &str| pickable_chats(&all, q).into_iter().map(|c| c.display_name).collect::<Vec<_>>();
        assert_eq!(names(""), ["Bob", "Alicia Group", "Alice"]);
        assert_eq!(names("ali"), ["Alicia Group", "Alice"]);
        assert!(names("zzz").is_empty());
    }

    #[test]
    fn gate_labels_name_the_reason() {
        assert_eq!(gate_label(SendGate::Open), "");
        assert_eq!(gate_label(SendGate::AdminsOnly), "Only admins can send");
        assert_eq!(gate_label(SendGate::NotParticipant), "You're no longer a participant");
    }

    #[test]
    fn gated_chats_are_skipped_on_forward() {
        let (open, dropped) = split_gated(ids(&["a@g.us", "b@s.whatsapp.net", "c@g.us"]), |j| j == "a@g.us");
        assert_eq!(open, ids(&["b@s.whatsapp.net", "c@g.us"]));
        assert_eq!(dropped, 1);
        let (open, dropped) = split_gated(ids(&["a@g.us"]), |_| true);
        assert!(open.is_empty());
        assert_eq!(dropped, 1);
    }
}
