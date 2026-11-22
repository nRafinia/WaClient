//! In-chat search: debounced queries on a worker thread, next/previous hit
//! navigation, and jumping to a hit (loading a window of messages around it
//! when it is older than what the chat view holds), plus the "jump to date"
//! calendar of the search bar.

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use chrono::{Datelike, Local, NaiveDate};
use slint::{ComponentHandle, Model, ModelRc, Timer, TimerMode, VecModel};

use super::chat_search::{
    Debouncer, ResultNav, SearchAction, clamp_month, day_start, days_in_month, days_with_messages, local_date,
    month_bounds, month_cells, month_title, shift_month,
};
use super::{App, LayoutFix, PAGE, with_app};
use crate::{CalendarCell, CalendarState, MainWindow};

/// Most hits one query returns.
const SEARCH_LIMIT: u32 = 500;
/// Messages loaded on each side of a hit that is not in the chat view yet.
const WINDOW_SIDE: u32 = 30;
/// The new rows need a layout pass before a scroll position can be computed.
const JUMP_DELAY: Duration = Duration::from_millis(80);
/// Longer than the settling time of a jump, so "load older" cannot move the view.
const JUMP_GUARD: Duration = Duration::from_millis(1200);

pub(super) struct SearchState {
    epoch: Instant,
    debounce: RefCell<Debouncer>,
    timer: Timer,
    jump_timer: Timer,
    nav: Cell<ResultNav>,
    /// Ids of the hits, newest first.
    hits: RefCell<Vec<String>>,
    /// Bumped by every query and reset, so an answer that arrives late is dropped.
    generation: Cell<u64>,
    /// The chat view holds a window around a hit instead of the newest page.
    windowed: Cell<bool>,
}

impl SearchState {
    pub(super) fn new() -> Self {
        SearchState {
            epoch: Instant::now(),
            debounce: RefCell::new(Debouncer::new()),
            timer: Timer::default(),
            jump_timer: Timer::default(),
            nav: Cell::new(ResultNav::new(0)),
            hits: RefCell::new(Vec::new()),
            generation: Cell::new(0),
            windowed: Cell::new(false),
        }
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    pub(super) fn leave_window(&self) {
        self.windowed.set(false);
    }
}

impl App {
    /// A keystroke in the search field.
    pub(super) fn search_input(&self, text: &str) {
        let now = self.search.now_ms();
        self.search.debounce.borrow_mut().input(text, now);
        self.search_arm();
    }

    fn search_arm(&self) {
        let Some(deadline) = self.search.debounce.borrow().next_deadline() else {
            self.search.timer.stop();
            return;
        };
        let wait = deadline.saturating_sub(self.search.now_ms());
        self.search
            .timer
            .start(TimerMode::SingleShot, Duration::from_millis(wait), || with_app(App::search_tick));
    }

    fn search_tick(&self) {
        let action = self.search.debounce.borrow_mut().poll(self.search.now_ms());
        match action {
            SearchAction::Idle => self.search_arm(),
            SearchAction::Query(text) => self.search_start(text),
            SearchAction::Clear => self.search_drop_hits(),
        }
    }

    fn search_start(&self, query: String) {
        let p = self.p();
        let Some(jid) = p.st.borrow().selected.clone() else { return };
        let generation = self.search.generation.get() + 1;
        self.search.generation.set(generation);
        if let Some(win) = self.win() {
            win.set_search_searching(true);
        }
        let store = p.store.clone();
        let profile = p.id.clone();
        let spawned = std::thread::Builder::new().name("chat-search".into()).spawn(move || {
            let result = store.search_messages(&jid, &query, SEARCH_LIMIT).map_err(|e| e.to_string());
            let queued = slint::invoke_from_event_loop(move || {
                with_app(|a| a.search_done(generation, &profile, &jid, result));
            });
            if let Err(e) = queued {
                log::debug!("search result not delivered: {e}");
            }
        });
        if let Err(e) = spawned {
            log::error!("could not start the search thread: {e}");
            self.show_banner(1, &format!("Could not search: {e}"));
        }
    }

    fn search_done(
        &self,
        generation: u64,
        profile: &str,
        jid: &str,
        result: Result<Vec<wa_store::MessageRow>, String>,
    ) {
        if generation != self.search.generation.get() {
            return;
        }
        let still_here = {
            let p = self.p();
            p.id == profile && p.st.borrow().selected.as_deref() == Some(jid)
        };
        if !still_here {
            return;
        }
        let rows = match result {
            Ok(rows) => rows,
            Err(e) => {
                log::error!("search failed: {e}");
                self.show_banner(1, &format!("Search failed: {e}"));
                Vec::new()
            }
        };
        let ids: Vec<String> = rows.into_iter().map(|m| m.id).collect();
        let first = ids.first().cloned();
        self.search.nav.set(ResultNav::new(ids.len()));
        *self.search.hits.borrow_mut() = ids;
        self.search_publish(false);
        match first {
            Some(id) => self.search_jump(&id),
            None => self.search_highlight(""),
        }
    }

    /// Pushes the counters (and the end of the "searching" state) to the window.
    fn search_publish(&self, searching: bool) {
        let Some(win) = self.win() else { return };
        let nav = self.search.nav.get();
        win.set_search_total(i32::try_from(nav.total()).unwrap_or(i32::MAX));
        win.set_search_current(nav.current_label());
        win.set_search_searching(searching);
    }

    fn search_highlight(&self, id: &str) {
        if let Some(win) = self.win() {
            win.set_highlight_id(id.into());
        }
    }

    /// The box was emptied: no hits, no highlight, bar stays open.
    fn search_drop_hits(&self) {
        self.search.generation.set(self.search.generation.get() + 1);
        self.search.hits.borrow_mut().clear();
        self.search.nav.set(ResultNav::new(0));
        self.search_publish(false);
        self.search_highlight("");
    }

    pub(super) fn search_next(&self) {
        let mut nav = self.search.nav.get();
        let idx = nav.next();
        self.search_go(nav, idx);
    }

    pub(super) fn search_prev(&self) {
        let mut nav = self.search.nav.get();
        let idx = nav.prev();
        self.search_go(nav, idx);
    }

    fn search_go(&self, nav: ResultNav, idx: Option<usize>) {
        let Some(idx) = idx else { return };
        self.search.nav.set(nav);
        self.search_publish(false);
        let id = self.search.hits.borrow().get(idx).cloned();
        if let Some(id) = id {
            self.search_jump(&id);
        }
    }

    /// Forgets everything about the current search and closes the bar. Used
    /// when the chat or profile changes, and by [`App::search_close`].
    pub(super) fn search_reset(&self) {
        self.search.debounce.borrow_mut().reset();
        self.search.timer.stop();
        self.search.jump_timer.stop();
        self.search.windowed.set(false);
        self.search_drop_hits();
        if let Some(win) = self.win() {
            win.set_search_open(false);
            win.set_search_query(Default::default());
        }
    }

    /// The bar's close button or Esc: clear the search and, if the view was
    /// moved to an old window of the chat, return to the newest messages.
    pub(super) fn search_close(&self) {
        let was_windowed = self.search.windowed.get();
        self.search_reset();
        if !was_windowed {
            return;
        }
        let p = self.p();
        let Some(jid) = p.st.borrow().selected.clone() else { return };
        let Some(page) = self.ok("load messages", p.store.messages_page(&jid, None, PAGE as u32)) else {
            return;
        };
        {
            let mut st = p.st.borrow_mut();
            st.exhausted = page.len() < PAGE;
            st.limit = PAGE;
            st.msgs = page;
            st.polls.clear();
            st.force_bottom = false;
        }
        self.rebuild_rows();
        self.schedule_fix(LayoutFix::Bottom);
    }

    /// A live refresh while the view holds a search window: reloads the same
    /// span in place (anchored on its first message) so the user stays on the
    /// hit. Returns false when the view is not windowed or the window cannot
    /// be reloaded, and the caller falls back to the newest page.
    pub(super) fn search_refresh_window(&self, jid: &str) -> bool {
        if !self.search.windowed.get() {
            return false;
        }
        let p = self.p();
        let (anchor, len) = {
            let st = p.st.borrow();
            match st.msgs.first() {
                Some(m) => (m.id.clone(), st.msgs.len()),
                None => return false,
            }
        };
        let after = u32::try_from(len).unwrap_or(u32::MAX);
        let Some(rows) = self.ok("refresh search window", p.store.message_window_around(jid, &anchor, 0, after))
        else {
            return true;
        };
        if rows.is_empty() {
            return false;
        }
        {
            let mut st = p.st.borrow_mut();
            st.msgs = rows;
            st.polls.clear();
            st.force_bottom = false;
        }
        self.rebuild_rows();
        true
    }

    /// Highlights message `id` and scrolls to it, loading a window of messages
    /// around it first when the chat view does not hold it.
    pub(super) fn search_jump(&self, id: &str) {
        let p = self.p();
        let (jid, loaded) = {
            let st = p.st.borrow();
            let Some(jid) = st.selected.clone() else { return };
            (jid, st.msgs.iter().any(|m| m.id == id))
        };
        self.search_highlight(id);
        if loaded {
            self.search_scroll(id);
            return;
        }
        let Some(rows) = self.ok("load search result", p.store.message_window_around(&jid, id, WINDOW_SIDE, WINDOW_SIDE))
        else {
            return;
        };
        if rows.is_empty() {
            self.show_toast("That message is no longer available");
            return;
        }
        {
            let mut st = p.st.borrow_mut();
            // Older messages may exist before the window; "load older" finds out.
            st.exhausted = false;
            st.limit = PAGE;
            st.msgs = rows;
            st.polls.clear();
            st.force_bottom = false;
        }
        self.search.windowed.set(true);
        self.fix_timer.stop();
        *self.fix_run.borrow_mut() = None;
        self.rebuild_rows();
        p.st.borrow_mut().guard_until = Instant::now() + JUMP_GUARD;
        let id = id.to_string();
        self.search.jump_timer.start(TimerMode::SingleShot, JUMP_DELAY, move || {
            with_app(|a| a.search_scroll(&id));
        });
    }

    fn search_scroll(&self, id: &str) {
        let Some(win) = self.win() else { return };
        let count = self.msgs_model.row_count();
        let Some(idx) = self.msgs_model.iter().position(|m| !m.is_sep && m.id.as_str() == id) else {
            return;
        };
        self.p().st.borrow_mut().guard_until = Instant::now() + JUMP_GUARD;
        win.invoke_jump_to((idx as f32 + 0.5) / count.max(1) as f32);
    }
}

// ---- jump to date ---------------------------------------------------------------------

/// How long the first message of a picked day stays highlighted.
const DATE_HIGHLIGHT: Duration = Duration::from_millis(2500);

/// What a pick in the open calendar does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CalPurpose {
    /// Scroll the open chat to the first message of the day.
    Jump,
    /// First day of a custom range for the global search.
    RangeFrom,
    /// Last day of the custom range that starts on this day.
    RangeTo(NaiveDate),
}

struct CalState {
    purpose: CalPurpose,
    chat: String,
    /// Month on show and the first and last month that can be reached.
    shown: (i32, u32),
    first: (i32, u32),
    last: (i32, u32),
}

thread_local! {
    static CAL: RefCell<Option<CalState>> = const { RefCell::new(None) };
    static DATE_HL: Timer = Timer::default();
}

fn month_of(ts: i64) -> Option<(i32, u32)> {
    local_date(&Local, ts).map(|d| (d.year(), d.month()))
}

pub(super) fn wire(win: &MainWindow) {
    let g = win.global::<CalendarState>();
    g.on_open_jump(|| with_app(App::calendar_open_jump));
    g.on_prev(|| with_app(|a| a.calendar_move(-1)));
    g.on_next(|| with_app(|a| a.calendar_move(1)));
    g.on_pick(|day| with_app(|a| a.calendar_pick(u32::try_from(day).unwrap_or(0))));
    g.on_close(|| with_app(App::calendar_close));
}

impl App {
    /// The calendar button of the chat search: days with messages are enabled.
    pub(super) fn calendar_open_jump(&self) {
        let p = self.p();
        let Some(chat) = p.st.borrow().selected.clone() else { return };
        let span = self.ok("read chat dates", p.store.chat_time_span(&chat)).flatten();
        let Some((first, last)) = span.and_then(|(a, b)| Some((month_of(a)?, month_of(b)?))) else {
            self.show_toast("No messages to jump to");
            return;
        };
        self.calendar_show(CalState { purpose: CalPurpose::Jump, chat, shown: last, first, last });
    }

    /// The custom date range of the global search: any day up to today.
    pub(super) fn calendar_open_range(&self, purpose: CalPurpose) {
        let p = self.p();
        let now = Local::now();
        let today = (now.year(), now.month());
        let first = self
            .ok("read message dates", p.store.message_time_span())
            .flatten()
            .and_then(|(a, _)| month_of(a))
            .map_or(today, |m| m.min(today));
        self.calendar_show(CalState { purpose, chat: String::new(), shown: today, first, last: today });
    }

    fn calendar_show(&self, state: CalState) {
        CAL.with(|c| *c.borrow_mut() = Some(state));
        self.calendar_render();
        if let Some(win) = self.win() {
            win.global::<CalendarState>().set_open(true);
        }
    }

    pub(super) fn calendar_close(&self) {
        CAL.with(|c| *c.borrow_mut() = None);
        if let Some(win) = self.win() {
            win.global::<CalendarState>().set_open(false);
        }
    }

    pub(super) fn calendar_is_open(&self) -> bool {
        CAL.with(|c| c.borrow().is_some())
    }

    fn calendar_move(&self, delta: i32) {
        CAL.with(|c| {
            if let Some(s) = c.borrow_mut().as_mut() {
                let (y, m) = shift_month(s.shown.0, s.shown.1, delta);
                s.shown = clamp_month((y, m), s.first, s.last);
            }
        });
        self.calendar_render();
    }

    fn calendar_render(&self) {
        let Some(win) = self.win() else { return };
        let Some((purpose, chat, shown, first, last)) =
            CAL.with(|c| c.borrow().as_ref().map(|s| (s.purpose, s.chat.clone(), s.shown, s.first, s.last)))
        else {
            return;
        };
        let (year, month) = shown;
        let enabled_days: BTreeSet<u32> = match purpose {
            CalPurpose::Jump => {
                let times = month_bounds(&Local, year, month)
                    .and_then(|(from, to)| self.ok("read chat dates", self.p().store.chat_message_times(&chat, from, to)))
                    .unwrap_or_default();
                days_with_messages(&Local, year, month, &times)
            }
            CalPurpose::RangeFrom | CalPurpose::RangeTo(_) => {
                let today = Local::now().date_naive();
                let not_before = match purpose {
                    CalPurpose::RangeTo(from) => Some(from),
                    _ => None,
                };
                (1..=days_in_month(year, month))
                    .filter(|d| {
                        NaiveDate::from_ymd_opt(year, month, *d)
                            .is_some_and(|date| date <= today && not_before.is_none_or(|f| date >= f))
                    })
                    .collect()
            }
        };
        let picked = match purpose {
            CalPurpose::RangeTo(from) if (from.year(), from.month()) == shown => Some(from.day()),
            _ => None,
        };
        let cells: Vec<CalendarCell> = month_cells(year, month, |d| enabled_days.contains(&d))
            .into_iter()
            .map(|c| CalendarCell {
                day: i32::try_from(c.day).unwrap_or(0),
                enabled: c.enabled,
                selected: c.day != 0 && Some(c.day) == picked,
            })
            .collect();
        let g = win.global::<CalendarState>();
        g.set_heading(
            match purpose {
                CalPurpose::Jump => "Jump to date",
                CalPurpose::RangeFrom => "Select the first day",
                CalPurpose::RangeTo(_) => "Select the last day",
            }
            .into(),
        );
        g.set_title(month_title(year, month).into());
        g.set_cells(ModelRc::new(VecModel::from(cells)));
        g.set_can_prev(shown > first);
        g.set_can_next(shown < last);
    }

    fn calendar_pick(&self, day: u32) {
        let Some((purpose, chat, (year, month))) =
            CAL.with(|c| c.borrow().as_ref().map(|s| (s.purpose, s.chat.clone(), s.shown)))
        else {
            return;
        };
        let Some(date) = NaiveDate::from_ymd_opt(year, month, day) else { return };
        match purpose {
            CalPurpose::Jump => self.calendar_jump(&chat, date),
            CalPurpose::RangeFrom => {
                CAL.with(|c| {
                    if let Some(s) = c.borrow_mut().as_mut() {
                        s.purpose = CalPurpose::RangeTo(date);
                    }
                });
                self.calendar_render();
            }
            CalPurpose::RangeTo(from) => {
                self.calendar_close();
                self.gsearch_range_picked(from, date);
            }
        }
    }

    /// Scrolls `chat` to the first message of `date`, loading older history around it when needed.
    fn calendar_jump(&self, chat: &str, date: NaiveDate) {
        let next = date.succ_opt().unwrap_or(date);
        let (from, to) = (day_start(&Local, date), day_start(&Local, next));
        let p = self.p();
        if p.st.borrow().selected.as_deref() != Some(chat) {
            self.calendar_close();
            return;
        }
        let Some(first) = self.ok("find the first message of the day", p.store.first_message_between(chat, from, to))
        else {
            return;
        };
        self.calendar_close();
        let Some(id) = first else {
            self.show_toast("No messages on that day");
            return;
        };
        self.search_jump(&id);
        DATE_HL.with(|t| {
            t.start(TimerMode::SingleShot, DATE_HIGHLIGHT, || {
                with_app(|a| a.search_highlight(""));
            });
        });
    }

    /// `WACLIENT_DEMO_UI=jump-date`: the open chat's search bar with the calendar on top.
    pub(super) fn demo_jump_date(&self) {
        self.select_chat("111@s.whatsapp.net");
        if let Some(win) = self.win() {
            win.set_search_open(true);
        }
        self.calendar_open_jump();
    }

    /// `WACLIENT_DEMO_UI=seq:` step `jump-pick`: picks the newest enabled day.
    pub(super) fn demo_jump_pick(&self) {
        let day = self
            .win()
            .map(|w| w.global::<CalendarState>().get_cells())
            .and_then(|cells| cells.iter().filter(|c| c.enabled).map(|c| c.day).last());
        if let Some(day) = day {
            self.calendar_pick(u32::try_from(day).unwrap_or(0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn month_of_follows_the_local_calendar() {
        let ts = Local.with_ymd_and_hms(2026, 3, 31, 23, 30, 0).unwrap().timestamp();
        assert_eq!(month_of(ts), Some((2026, 3)));
        let ts = Local.with_ymd_and_hms(2026, 4, 1, 0, 30, 0).unwrap().timestamp();
        assert_eq!(month_of(ts), Some((2026, 4)));
    }
}
