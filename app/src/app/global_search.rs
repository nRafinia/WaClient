//! The chat-list search with filters: message hits from every chat, narrowed
//! by kind (photos & videos, links, documents, voice), sender and date range.
//! The Slint side is the `SearchFilters` global (ui/search_filters.slint); the
//! pure parts (filter -> query, date bounds) live in `chat_search.rs`.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use chrono::{Local, NaiveDate};
use slint::{ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, Timer, TimerMode, VecModel};
use wa_store::{GlobalFilter, MessageKind, MessageRow, SenderFilter};

use super::chat_search::{DateRange, DEBOUNCE_MS, build_filter, build_snippet_with_radius};
use super::msgops_ui::{contact_label, when_text};
use super::search_ui::CalPurpose;
use super::{App, with_app};
use crate::{MainWindow, SearchFilters, SearchHitItem, SenderItem, models};

/// Hits fetched per page.
const PAGE_HITS: u32 = 40;
/// Most people the "From" menu lists at once.
const SENDER_LIMIT: usize = 60;
/// Characters of context kept on each side of the match in a result line.
const SNIPPET_RADIUS: usize = 12;
/// Quiet period before shown results are re-queried after new messages arrive.
const REFRESH_AFTER: Duration = Duration::from_secs(1);
/// `SearchFilters.range-index` of the custom range.
const RANGE_CUSTOM: i32 = 4;
/// Sentinel `jid` of the own account in the sender picker.
const ME: &str = "@me";

struct Gs {
    text: String,
    kind: i32,
    sender: Option<(SenderFilter, String)>,
    range: DateRange,
    /// `(ts, chat_jid, id)` of the last hit shown.
    cursor: Option<(i64, String, String)>,
    /// Bumped by every new query so a late answer is dropped.
    generation: u64,
    /// The running query is a refresh: the old rows stay until its answer replaces them.
    replace_on_done: bool,
    /// New messages arrived while a query was running; refresh once it is done.
    refresh_pending: bool,
    model: Rc<VecModel<SearchHitItem>>,
}

thread_local! {
    static GS: RefCell<Gs> = RefCell::new(Gs {
        text: String::new(),
        kind: 0,
        sender: None,
        range: DateRange::Any,
        cursor: None,
        generation: 0,
        replace_on_done: false,
        refresh_pending: false,
        model: Rc::new(VecModel::default()),
    });
    static DEBOUNCE: Timer = Timer::default();
    static REFRESH: Timer = Timer::default();
}

/// One hit with everything the worker thread could resolve.
struct HitData {
    row: MessageRow,
    chat_name: String,
    thumb: Option<SharedPixelBuffer<Rgba8Pixel>>,
}

struct Page {
    hits: Vec<HitData>,
    /// The page was full, so there may be more.
    more: bool,
}

pub(super) fn wire(win: &MainWindow) {
    let g = win.global::<SearchFilters>();
    GS.with(|s| g.set_hits(ModelRc::from(s.borrow().model.clone())));
    g.on_kind_selected(|i| with_app(|a| a.gsearch_kind(i)));
    g.on_range_selected(|i| with_app(|a| a.gsearch_range(i)));
    g.on_sender_query(|q| with_app(|a| a.gsearch_senders(q.as_str())));
    g.on_sender_picked(|jid, label| with_app(|a| a.gsearch_sender(jid.as_str(), label.as_str())));
    g.on_hit_clicked(|chat, id| with_app(|a| a.jump_to_message(chat.as_str(), id.as_str())));
    g.on_load_more(|| with_app(|a| a.gsearch_run(false)));
    g.on_menu_opened(|m| with_app(|a| a.gsearch_menu(m)));
    g.on_bar_closed(|| with_app(App::gsearch_close_bar));
}

/// `SearchHitItem.kind` of a message: 0 text, 1 photo, 2 video, 3 link, 4 document, 5 voice.
fn hit_kind(m: &MessageRow) -> i32 {
    match m.kind {
        MessageKind::Image => 1,
        MessageKind::Video => 2,
        MessageKind::Document => 4,
        MessageKind::Voice | MessageKind::Audio => 5,
        _ => {
            let has_link = m.link_url.is_some()
                || m.text.as_deref().is_some_and(|t| t.contains("http://") || t.contains("https://") || t.contains("www."));
            if has_link { 3 } else { 0 }
        }
    }
}

/// The line under the chat name: caption or text, else the file name or kind.
fn hit_body(m: &MessageRow) -> String {
    let text = m.text.as_deref().map(str::trim).filter(|t| !t.is_empty());
    match text {
        Some(t) => crate::format::strip(t),
        None => match m.kind {
            MessageKind::Document => m.media_name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| "Document".into()),
            _ => models::reply_preview(m),
        },
    }
}

/// `SearchFilters.mode`: 0 plain chat list, 1 chats then messages, 2 messages only.
fn list_mode(filter: &GlobalFilter) -> i32 {
    if filter.is_empty() {
        0
    } else if filter.kind == wa_store::SearchKind::All && filter.sender.is_none() && filter.from_ts.is_none() {
        1
    } else {
        2
    }
}

/// Whether new messages should re-run the search: only while results are shown
/// (`mode` > 0) and no query is in flight.
fn refresh_wanted(mode: i32, loading: bool) -> bool {
    mode > 0 && !loading
}

/// What new messages mean for the search.
#[derive(Debug, PartialEq, Eq)]
enum RefreshStep {
    Ignore,
    Schedule,
    /// A query is in flight: re-arm the refresh when it completes.
    Defer,
}

fn refresh_step(mode: i32, loading: bool) -> RefreshStep {
    if mode == 0 {
        RefreshStep::Ignore
    } else if loading {
        RefreshStep::Defer
    } else {
        RefreshStep::Schedule
    }
}

/// A refresh keeps every row already loaded, so "Load more" pages do not vanish.
fn refresh_limit(rows: usize) -> u32 {
    u32::try_from(rows).unwrap_or(u32::MAX).max(PAGE_HITS)
}

impl App {
    /// New chats or messages arrived: refreshes shown results after a quiet second.
    pub(super) fn gsearch_changed(&self) {
        let Some(win) = self.win() else { return };
        let g = win.global::<SearchFilters>();
        match refresh_step(list_mode(&self.gsearch_filter()), g.get_loading()) {
            RefreshStep::Ignore => {}
            RefreshStep::Defer => GS.with(|s| s.borrow_mut().refresh_pending = true),
            RefreshStep::Schedule => Self::gsearch_arm_refresh(),
        }
    }

    fn gsearch_arm_refresh() {
        REFRESH.with(|t| {
            if !t.running() {
                t.start(TimerMode::SingleShot, REFRESH_AFTER, || with_app(App::gsearch_refresh));
            }
        });
    }

    fn gsearch_refresh(&self) {
        let Some(win) = self.win() else { return };
        if !refresh_wanted(list_mode(&self.gsearch_filter()), win.global::<SearchFilters>().get_loading()) {
            return;
        }
        self.gsearch_start(true, true);
    }

    /// A keystroke in the chat-list search box.
    pub(super) fn gsearch_text_changed(&self, text: &str) {
        GS.with(|s| s.borrow_mut().text = text.trim().to_string());
        DEBOUNCE.with(|t| {
            t.start(TimerMode::SingleShot, Duration::from_millis(DEBOUNCE_MS), || with_app(|a| a.gsearch_run(true)));
        });
        // Empty box and no filter: leave message mode at once instead of after the quiet period.
        if self.gsearch_filter().is_empty() {
            DEBOUNCE.with(Timer::stop);
            self.gsearch_run(true);
        }
    }

    fn gsearch_filter(&self) -> GlobalFilter {
        GS.with(|s| {
            let s = s.borrow();
            build_filter(&s.text, s.kind, s.sender.as_ref().map(|(f, _)| f.clone()), s.range, &Local::now())
        })
    }

    fn gsearch_kind(&self, kind: i32) {
        GS.with(|s| s.borrow_mut().kind = kind);
        if let Some(win) = self.win() {
            let g = win.global::<SearchFilters>();
            g.set_kind(kind);
            g.set_menu(0);
        }
        self.gsearch_run(true);
    }

    fn gsearch_range(&self, index: i32) {
        if let Some(win) = self.win() {
            win.global::<SearchFilters>().set_menu(0);
        }
        match DateRange::preset(index) {
            Some(range) => {
                GS.with(|s| s.borrow_mut().range = range);
                self.gsearch_run(true);
            }
            None => self.calendar_open_range(CalPurpose::RangeFrom),
        }
    }

    /// The calendar finished a custom range.
    pub(super) fn gsearch_range_picked(&self, from: NaiveDate, to: NaiveDate) {
        GS.with(|s| s.borrow_mut().range = DateRange::Custom { from, to });
        self.gsearch_run(true);
    }

    fn gsearch_menu(&self, menu: i32) {
        let Some(win) = self.win() else { return };
        win.global::<SearchFilters>().set_menu(menu);
        if menu == 2 {
            self.gsearch_senders("");
        }
    }

    fn gsearch_senders(&self, query: &str) {
        let Some(win) = self.win() else { return };
        let p = self.p();
        let Some(people) = self.ok("list senders", p.store.known_senders(query, SENDER_LIMIT)) else { return };
        let mut items = Vec::with_capacity(people.len() + 2);
        items.push(SenderItem { jid: "".into(), label: "Anyone".into() });
        if "you".contains(&query.trim().to_lowercase()) {
            items.push(SenderItem { jid: ME.into(), label: "You".into() });
        }
        items.extend(people.iter().map(|r| {
            let contact = p.store.contact(&r.jid).ok().flatten();
            SenderItem { jid: r.jid.as_str().into(), label: contact_label(contact.as_ref(), &r.jid).into() }
        }));
        win.global::<SearchFilters>().set_senders(ModelRc::new(VecModel::from(items)));
    }

    fn gsearch_sender(&self, jid: &str, label: &str) {
        let sender = match jid {
            "" => None,
            ME => Some((SenderFilter::Me, label.to_string())),
            other => Some((SenderFilter::Jid(other.to_string()), label.to_string())),
        };
        GS.with(|s| s.borrow_mut().sender = sender);
        if let Some(win) = self.win() {
            win.global::<SearchFilters>().set_menu(0);
        }
        self.gsearch_run(true);
    }

    /// The "x" of the box: back to the plain chat list.
    fn gsearch_close_bar(&self) {
        self.gsearch_reset();
        self.search("");
    }

    /// Filters, hits and the chip rows are dropped.
    pub(super) fn gsearch_reset(&self) {
        DEBOUNCE.with(Timer::stop);
        GS.with(|s| {
            let mut s = s.borrow_mut();
            s.text.clear();
            s.kind = 0;
            s.sender = None;
            s.range = DateRange::Any;
            s.cursor = None;
            s.generation += 1;
            s.refresh_pending = false;
            s.model.set_vec(Vec::new());
        });
        if let Some(win) = self.win() {
            let g = win.global::<SearchFilters>();
            g.set_open(false);
            g.set_menu(0);
            self.gsearch_publish(&win, false, false);
        }
    }

    /// Runs the query for the current filters: from the top (`fresh`) or the next page.
    fn gsearch_run(&self, fresh: bool) {
        self.gsearch_start(fresh, false);
    }

    /// `quiet`: a refresh of the shown results, which keeps them (and the
    /// scroll position) until the answer arrives.
    fn gsearch_start(&self, fresh: bool, quiet: bool) {
        let Some(win) = self.win() else { return };
        let filter = self.gsearch_filter();
        let (generation, cursor, limit) = GS.with(|s| {
            let mut s = s.borrow_mut();
            s.replace_on_done = quiet;
            if quiet {
                // This refresh sees everything stored so far.
                s.refresh_pending = false;
            }
            let limit = if quiet { refresh_limit(s.model.row_count()) } else { PAGE_HITS };
            // Every start supersedes an answer still on its way (a refresh must
            // not land on top of a "Load more" page).
            s.generation += 1;
            if fresh {
                s.cursor = None;
                if filter.is_empty() {
                    s.model.set_vec(Vec::new());
                }
            }
            (s.generation, s.cursor.clone(), limit)
        });
        if filter.is_empty() {
            self.gsearch_publish(&win, false, false);
            return;
        }
        if fresh && !quiet {
            GS.with(|s| s.borrow().model.set_vec(Vec::new()));
        }
        if !quiet {
            self.gsearch_publish(&win, true, false);
        }
        let p = self.p();
        let store = p.store.clone();
        let profile = p.id.clone();
        let spawned = std::thread::Builder::new().name("global-search".into()).spawn(move || {
            let key = cursor.as_ref().map(|(t, c, i)| (*t, c.as_str(), i.as_str()));
            let result = store.search_everywhere(&filter, key, limit).map_err(|e| e.to_string()).map(|rows| {
                let more = rows.len() >= limit as usize;
                let hits = rows
                    .into_iter()
                    .map(|row| {
                        let chat_name = store.chat(&row.chat_jid).ok().flatten().map(|c| c.display_name).unwrap_or_default();
                        let bytes = row.thumbnail.as_deref().or(row.link_thumb.as_deref());
                        let thumb = bytes.and_then(|b| crate::thumbs::decode_thumbnail_cached(&row.id, b));
                        HitData { row, chat_name, thumb }
                    })
                    .collect();
                Page { hits, more }
            });
            let queued = slint::invoke_from_event_loop(move || {
                with_app(|a| a.gsearch_done(generation, &profile, result));
            });
            if let Err(e) = queued {
                log::debug!("global search result not delivered: {e}");
            }
        });
        if let Err(e) = spawned {
            log::error!("could not start the global search thread: {e}");
            self.show_banner(1, &format!("Could not search: {e}"));
            self.gsearch_publish(&win, false, false);
        }
    }

    fn gsearch_done(&self, generation: u64, profile: &str, result: Result<Page, String>) {
        let current = GS.with(|s| s.borrow().generation);
        if generation != current || self.p().id != profile {
            return;
        }
        let Some(win) = self.win() else { return };
        let page = match result {
            Ok(page) => page,
            Err(e) => {
                log::error!("global search failed: {e}");
                self.show_banner(1, &format!("Search failed: {e}"));
                self.gsearch_publish(&win, false, false);
                return;
            }
        };
        let (query, now) = (GS.with(|s| s.borrow().text.clone()), Local::now());
        let p = self.p();
        let items: Vec<SearchHitItem> = page
            .hits
            .iter()
            .map(|h| {
                let m = &h.row;
                let snippet = build_snippet_with_radius(&hit_body(m), &query, SNIPPET_RADIUS);
                let (before, matched, after) = snippet.split();
                let dm = wa_store::ChatKind::from_jid(&m.chat_jid) == wa_store::ChatKind::Dm;
                let sender = if m.from_me {
                    "You".to_string()
                } else if dm {
                    String::new()
                } else {
                    match m.sender_name.as_deref().filter(|n| !n.trim().is_empty()) {
                        Some(n) => n.to_string(),
                        None => contact_label(p.store.contact(&m.sender_jid).ok().flatten().as_ref(), &m.sender_jid),
                    }
                };
                SearchHitItem {
                    chat_jid: m.chat_jid.as_str().into(),
                    id: m.id.as_str().into(),
                    chat_name: h.chat_name.as_str().into(),
                    sender: sender.into(),
                    time: when_text(m.ts, now).into(),
                    before: before.into(),
                    matched: matched.into(),
                    after: after.into(),
                    kind: hit_kind(m),
                    has_thumb: h.thumb.is_some(),
                    thumb: h.thumb.clone().map(Image::from_rgba8).unwrap_or_default(),
                }
            })
            .collect();
        GS.with(|s| {
            let mut s = s.borrow_mut();
            if std::mem::take(&mut s.replace_on_done) {
                s.model.set_vec(Vec::new());
            }
            if let Some(last) = page.hits.last() {
                s.cursor = Some((last.row.ts, last.row.chat_jid.clone(), last.row.id.clone()));
            }
            for item in items {
                s.model.push(item);
            }
        });
        self.gsearch_publish(&win, false, page.more);
        if GS.with(|s| std::mem::take(&mut s.borrow_mut().refresh_pending)) {
            self.gsearch_changed();
        }
    }

    /// Pushes the chip labels, the list mode and the loading state to the window.
    fn gsearch_publish(&self, win: &MainWindow, loading: bool, more: bool) {
        let filter = self.gsearch_filter();
        let (kind, sender, range, hits) = GS.with(|s| {
            let s = s.borrow();
            (s.kind, s.sender.as_ref().map(|(_, l)| l.clone()), s.range, s.model.row_count())
        });
        let g = win.global::<SearchFilters>();
        g.set_kind(kind);
        g.set_from_label(sender.unwrap_or_default().into());
        g.set_range_label(range.label().into());
        g.set_range_set(range != DateRange::Any);
        g.set_range_index(match range {
            DateRange::Any => 0,
            DateRange::Today => 1,
            DateRange::Last7 => 2,
            DateRange::Last30 => 3,
            DateRange::Custom { .. } => RANGE_CUSTOM,
        });
        g.set_mode(list_mode(&filter));
        g.set_loading(loading);
        g.set_more(more);
        g.set_empty_text(if hits == 0 && !loading { "No messages found".into() } else { Default::default() });
    }

    /// `WACLIENT_DEMO_UI=search-filters[-docs|-links|-voice|-text|-from|-date]`.
    pub(super) fn demo_search_filters(&self, what: &str) {
        let Some(win) = self.win() else { return };
        let g = win.global::<SearchFilters>();
        g.set_open(true);
        let variant = what.trim_start_matches("search-filters").trim_start_matches('-');
        match variant {
            "docs" => self.gsearch_kind(3),
            "links" => self.gsearch_kind(2),
            "voice" => self.gsearch_kind(4),
            "text" => self.search("Message number"),
            "from" => {
                self.gsearch_kind(0);
                self.gsearch_menu(2);
            }
            "date" => {
                self.gsearch_kind(1);
                GS.with(|s| s.borrow_mut().range = DateRange::Last30);
                self.gsearch_run(true);
                self.gsearch_menu(1);
            }
            _ => self.gsearch_kind(1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wa_store::SearchKind;

    fn row(kind: MessageKind, text: Option<&str>) -> MessageRow {
        MessageRow { kind, text: text.map(Into::into), ..Default::default() }
    }

    #[test]
    fn hit_kinds_cover_media_links_and_text() {
        assert_eq!(hit_kind(&row(MessageKind::Image, None)), 1);
        assert_eq!(hit_kind(&row(MessageKind::Video, None)), 2);
        assert_eq!(hit_kind(&row(MessageKind::Document, None)), 4);
        assert_eq!(hit_kind(&row(MessageKind::Voice, None)), 5);
        assert_eq!(hit_kind(&row(MessageKind::Audio, None)), 5);
        assert_eq!(hit_kind(&row(MessageKind::Text, Some("see https://a.b"))), 3);
        assert_eq!(hit_kind(&row(MessageKind::Text, Some("see www.a.b"))), 3);
        assert_eq!(hit_kind(&row(MessageKind::Text, Some("hello"))), 0);
        let mut card = row(MessageKind::Text, Some("look"));
        card.link_url = Some("https://a.b".into());
        assert_eq!(hit_kind(&card), 3);
    }

    #[test]
    fn body_prefers_text_then_file_name_then_kind() {
        assert_eq!(hit_body(&row(MessageKind::Image, Some("  holiday "))), "holiday");
        let mut doc = row(MessageKind::Document, None);
        doc.media_name = Some("Invoice.pdf".into());
        assert_eq!(hit_body(&doc), "Invoice.pdf");
        assert_eq!(hit_body(&row(MessageKind::Document, None)), "Document");
        assert!(hit_body(&row(MessageKind::Voice, None)).contains("Voice"));
    }

    #[test]
    fn refresh_only_while_results_are_shown_and_idle() {
        assert!(!refresh_wanted(0, false));
        assert!(refresh_wanted(1, false));
        assert!(refresh_wanted(2, false));
        assert!(!refresh_wanted(2, true));
    }

    #[test]
    fn refresh_during_a_query_is_deferred_not_dropped() {
        assert_eq!(refresh_step(0, false), RefreshStep::Ignore);
        assert_eq!(refresh_step(0, true), RefreshStep::Ignore);
        assert_eq!(refresh_step(1, false), RefreshStep::Schedule);
        assert_eq!(refresh_step(2, true), RefreshStep::Defer);
    }

    #[test]
    fn refresh_keeps_the_loaded_pages() {
        assert_eq!(refresh_limit(0), PAGE_HITS);
        assert_eq!(refresh_limit(10), PAGE_HITS);
        assert_eq!(refresh_limit(120), 120);
    }

    #[test]
    fn list_mode_keeps_chat_matches_only_for_plain_text() {
        let f = |text: &str, kind, sender: Option<SenderFilter>, from_ts| GlobalFilter {
            text: text.into(),
            kind,
            sender,
            from_ts,
            to_ts: None,
        };
        assert_eq!(list_mode(&f("", SearchKind::All, None, None)), 0);
        assert_eq!(list_mode(&f("hi", SearchKind::All, None, None)), 1);
        assert_eq!(list_mode(&f("hi", SearchKind::Media, None, None)), 2);
        assert_eq!(list_mode(&f("", SearchKind::All, Some(SenderFilter::Me), None)), 2);
        assert_eq!(list_mode(&f("hi", SearchKind::All, None, Some(5))), 2);
    }
}
