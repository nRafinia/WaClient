//! In-chat search: debounced queries on a worker thread, next/previous hit
//! navigation, and jumping to a hit (loading a window of messages around it
//! when it is older than what the chat view holds).

use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

use slint::{Model, Timer, TimerMode};

use super::chat_search::{Debouncer, ResultNav, SearchAction};
use super::{App, LayoutFix, PAGE, with_app};

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
