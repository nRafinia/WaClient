//! In-chat search glue, pure and free of app state: the debounce state
//! machine, result navigation, and snippet / highlight building. The wiring
//! only has to feed it keystrokes and copy its output into Slint models.

use std::collections::BTreeSet;
use std::ops::Range;

use chrono::{DateTime, Datelike, NaiveDate, TimeZone};
use wa_store::{GlobalFilter, SearchKind, SenderFilter};

/// Quiet period after the last keystroke before the store is queried.
pub const DEBOUNCE_MS: u64 = 250;
/// Characters of context kept on each side of the match in a snippet.
#[allow(dead_code)] // only the optional result list (SearchResultList) needs this
pub const SNIPPET_RADIUS: usize = 30;
#[allow(dead_code)] // only the optional result list (SearchResultList) needs this
const NO_MATCH_PREVIEW_CHARS: usize = 60;

/// What the caller should do after `poll`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchAction {
    /// Nothing to do yet.
    Idle,
    /// Run the store query for this (trimmed) text.
    Query(String),
    /// The box was emptied: drop results and highlights.
    Clear,
}

/// Decides when a typed query is worth running. Times are caller-supplied
/// milliseconds from any monotonic clock, which keeps this testable.
#[derive(Debug, Default)]
pub struct Debouncer {
    pending: Option<(String, u64)>,
    last_issued: Option<String>,
}

impl Debouncer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the field's new text. A blank query clears immediately; any
    /// other waits `DEBOUNCE_MS` after the most recent keystroke.
    pub fn input(&mut self, text: &str, now_ms: u64) {
        let text = text.trim().to_string();
        let due = if text.is_empty() { now_ms } else { now_ms + DEBOUNCE_MS };
        self.pending = Some((text, due));
    }

    /// Time at which `poll` will next yield something, for timer scheduling.
    pub fn next_deadline(&self) -> Option<u64> {
        self.pending.as_ref().map(|(_, due)| *due)
    }

    pub fn poll(&mut self, now_ms: u64) -> SearchAction {
        let due = match &self.pending {
            Some((_, due)) => *due,
            None => return SearchAction::Idle,
        };
        if now_ms < due {
            return SearchAction::Idle;
        }
        let Some((text, _)) = self.pending.take() else {
            return SearchAction::Idle;
        };
        if self.last_issued.as_deref().unwrap_or("") == text {
            return SearchAction::Idle;
        }
        self.last_issued = Some(text.clone());
        if text.is_empty() {
            SearchAction::Clear
        } else {
            SearchAction::Query(text)
        }
    }

    /// Forget everything, e.g. when the bar closes or the chat changes, so the
    /// same query typed again is run again.
    pub fn reset(&mut self) {
        self.pending = None;
        self.last_issued = None;
    }
}

/// Position within a result list ordered newest first. `next` walks towards
/// older messages (what Enter and the down arrow do), `prev` towards newer
/// ones; both wrap around.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResultNav {
    total: usize,
    index: usize,
}

impl ResultNav {
    /// A fresh result set starts on the newest hit.
    pub fn new(total: usize) -> Self {
        Self { total, index: 0 }
    }

    pub fn total(&self) -> usize {
        self.total
    }

    /// Zero-based index of the current hit, `None` without results.
    #[allow(dead_code)] // only the optional result list (SearchResultList) needs this
    pub fn index(&self) -> Option<usize> {
        (self.total > 0).then_some(self.index)
    }

    pub fn next(&mut self) -> Option<usize> {
        if self.total == 0 {
            return None;
        }
        self.index = (self.index + 1) % self.total;
        Some(self.index)
    }

    pub fn prev(&mut self) -> Option<usize> {
        if self.total == 0 {
            return None;
        }
        self.index = (self.index + self.total - 1) % self.total;
        Some(self.index)
    }

    /// Jump to a hit picked from the result list; out of range is ignored.
    #[allow(dead_code)] // only the optional result list (SearchResultList) needs this
    pub fn select(&mut self, index: usize) -> Option<usize> {
        if index >= self.total {
            return None;
        }
        self.index = index;
        Some(index)
    }

    /// One-based position for the Slint `current` property (0 = none).
    pub fn current_label(&self) -> i32 {
        if self.total == 0 {
            0
        } else {
            (self.index + 1) as i32
        }
    }

    /// "3 of 17", or empty without results.
    #[allow(dead_code)] // only the optional result list (SearchResultList) needs this
    pub fn counter(&self) -> String {
        if self.total == 0 {
            String::new()
        } else {
            format!("{} of {}", self.index + 1, self.total)
        }
    }
}

/// Simple per-char case fold. Unlike `str::to_lowercase` it never changes the
/// number of chars, so match positions map straight back to the original text.
#[allow(dead_code)] // only the optional result list (SearchResultList) needs this
fn fold_char(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Byte ranges of every non-overlapping case-insensitive occurrence of
/// `query` in `text`. Ranges always sit on char boundaries of `text`.
#[allow(dead_code)] // only the optional result list (SearchResultList) needs this
pub fn find_matches(text: &str, query: &str) -> Vec<Range<usize>> {
    let needle: Vec<char> = query.trim().chars().map(fold_char).collect();
    if needle.is_empty() {
        return Vec::new();
    }
    let hay: Vec<(usize, char)> = text.char_indices().map(|(i, c)| (i, fold_char(c))).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        if hay[i..i + needle.len()].iter().map(|(_, c)| c).eq(needle.iter()) {
            let start = hay[i].0;
            let end_idx = i + needle.len();
            let end = hay.get(end_idx).map_or(text.len(), |(b, _)| *b);
            out.push(start..end);
            i = end_idx;
        } else {
            i += 1;
        }
    }
    out
}

/// A one-line excerpt with the matches inside it located.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[allow(dead_code)] // only the optional result list (SearchResultList) needs this
pub struct Snippet {
    pub text: String,
    /// Byte ranges into `text`.
    pub highlights: Vec<Range<usize>>,
}

impl Snippet {
    /// `(before, matched, after)` around the first highlight, which is how the
    /// result row draws it. Without a highlight everything is `before`.
    #[allow(dead_code)] // only the optional result list (SearchResultList) needs this
    pub fn split(&self) -> (&str, &str, &str) {
        match self.highlights.first() {
            Some(r) => (&self.text[..r.start], &self.text[r.clone()], &self.text[r.end..]),
            None => (&self.text, "", ""),
        }
    }
}

/// Excerpt of `text` with up to `SNIPPET_RADIUS` chars on each side of the
/// first match, "…" marking cut ends and newlines flattened to spaces. When
/// there is no match (e.g. the hit came from the file name) the start of the
/// text is returned unhighlighted.
#[allow(dead_code)] // only the optional result list (SearchResultList) needs this
pub fn build_snippet(text: &str, query: &str) -> Snippet {
    build_snippet_with_radius(text, query, SNIPPET_RADIUS)
}

#[allow(dead_code)] // only the optional result list (SearchResultList) needs this
pub fn build_snippet_with_radius(text: &str, query: &str, radius: usize) -> Snippet {
    const ELLIPSIS: &str = "\u{2026}";
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let chars: Vec<(usize, char)> = flat.char_indices().collect();
    let total = chars.len();
    let byte_at = |ci: usize| chars.get(ci).map_or(flat.len(), |(b, _)| *b);

    let Some(first) = find_matches(&flat, query).into_iter().next() else {
        let end = NO_MATCH_PREVIEW_CHARS.min(total);
        let mut text = flat[..byte_at(end)].to_string();
        if end < total {
            text.push_str(ELLIPSIS);
        }
        return Snippet { text, highlights: Vec::new() };
    };

    let first_char = chars.partition_point(|(b, _)| *b < first.start);
    let last_char = chars.partition_point(|(b, _)| *b < first.end);
    let start = first_char.saturating_sub(radius);
    let end = (last_char + radius).min(total);

    let mut body = String::new();
    if start > 0 {
        body.push_str(ELLIPSIS);
    }
    let prefix = body.len();
    let (from, to) = (byte_at(start), byte_at(end));
    body.push_str(&flat[from..to]);
    if end < total {
        body.push_str(ELLIPSIS);
    }

    let highlights = find_matches(&flat[from..to], query)
        .into_iter()
        .map(|r| r.start + prefix..r.end + prefix)
        .collect();
    Snippet { text: body, highlights }
}

// ---- filters, date ranges and the calendar ---------------------------------

/// The "date" filter of the global search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DateRange {
    #[default]
    Any,
    Today,
    /// Today and the six days before it.
    Last7,
    /// Today and the 29 days before it.
    Last30,
    /// Both days included, in either order.
    Custom { from: NaiveDate, to: NaiveDate },
}

impl DateRange {
    /// The preset behind a menu row (0 any time ... 3 last 30 days); 4 (custom)
    /// needs dates and is built by the caller.
    pub fn preset(index: i32) -> Option<DateRange> {
        match index {
            0 => Some(DateRange::Any),
            1 => Some(DateRange::Today),
            2 => Some(DateRange::Last7),
            3 => Some(DateRange::Last30),
            _ => None,
        }
    }

    /// Text of the date chip.
    pub fn label(&self) -> String {
        match self {
            DateRange::Any => "Any time".to_string(),
            DateRange::Today => "Today".to_string(),
            DateRange::Last7 => "Last 7 days".to_string(),
            DateRange::Last30 => "Last 30 days".to_string(),
            DateRange::Custom { from, to } => {
                let (a, b) = if from <= to { (from, to) } else { (to, from) };
                if a == b {
                    a.format("%d %b %Y").to_string()
                } else {
                    format!("{} - {}", a.format("%d %b %Y"), b.format("%d %b %Y"))
                }
            }
        }
    }
}

/// First second of `date` in `tz`. A midnight that does not exist (a daylight
/// saving jump) starts the day at the first hour that does.
pub fn day_start<Tz: TimeZone>(tz: &Tz, date: NaiveDate) -> i64 {
    for hour in 0..4 {
        let Some(naive) = date.and_hms_opt(hour, 0, 0) else { continue };
        if let Some(t) = tz.from_local_datetime(&naive).earliest() {
            return t.timestamp();
        }
    }
    date.and_hms_opt(0, 0, 0).map_or(0, |n| n.and_utc().timestamp())
}

/// The calendar day `ts` falls on in `tz`.
pub fn local_date<Tz: TimeZone>(tz: &Tz, ts: i64) -> Option<NaiveDate> {
    tz.timestamp_opt(ts, 0).single().map(|t| t.date_naive())
}

/// `[from, to)` in unix seconds for a date filter as of `now`; `None` is open.
pub fn range_bounds<Tz: TimeZone>(range: DateRange, now: &DateTime<Tz>) -> (Option<i64>, Option<i64>) {
    let tz = now.timezone();
    let today = now.date_naive();
    let tomorrow = today.succ_opt().unwrap_or(today);
    let days_back = |n: i64| today.checked_sub_signed(chrono::Duration::days(n)).unwrap_or(today);
    match range {
        DateRange::Any => (None, None),
        DateRange::Today => (Some(day_start(&tz, today)), Some(day_start(&tz, tomorrow))),
        DateRange::Last7 => (Some(day_start(&tz, days_back(6))), Some(day_start(&tz, tomorrow))),
        DateRange::Last30 => (Some(day_start(&tz, days_back(29))), Some(day_start(&tz, tomorrow))),
        DateRange::Custom { from, to } => {
            let (a, b) = if from <= to { (from, to) } else { (to, from) };
            let after = b.succ_opt().unwrap_or(b);
            (Some(day_start(&tz, a)), Some(day_start(&tz, after)))
        }
    }
}

/// The store kind behind a chip (0 all, 1 photos & videos, 2 links, 3 documents, 4 voice).
pub fn kind_from_index(index: i32) -> SearchKind {
    match index {
        1 => SearchKind::Media,
        2 => SearchKind::Links,
        3 => SearchKind::Documents,
        4 => SearchKind::Voice,
        _ => SearchKind::All,
    }
}

/// Everything the filter row and the search box say, as one store query.
pub fn build_filter<Tz: TimeZone>(
    text: &str,
    kind: i32,
    sender: Option<SenderFilter>,
    range: DateRange,
    now: &DateTime<Tz>,
) -> GlobalFilter {
    let (from_ts, to_ts) = range_bounds(range, now);
    GlobalFilter { text: text.trim().to_string(), kind: kind_from_index(kind), sender, from_ts, to_ts }
}

pub fn days_in_month(year: i32, month: u32) -> u32 {
    let first = NaiveDate::from_ymd_opt(year, month, 1);
    let next = if month == 12 { NaiveDate::from_ymd_opt(year + 1, 1, 1) } else { NaiveDate::from_ymd_opt(year, month + 1, 1) };
    match (first, next) {
        (Some(a), Some(b)) => u32::try_from(b.signed_duration_since(a).num_days()).unwrap_or(30),
        _ => 30,
    }
}

/// `(year, month)` moved by `delta` months.
pub fn shift_month(year: i32, month: u32, delta: i32) -> (i32, u32) {
    let index = year * 12 + month as i32 - 1 + delta;
    (index.div_euclid(12), index.rem_euclid(12) as u32 + 1)
}

/// "March 2026".
pub fn month_title(year: i32, month: u32) -> String {
    NaiveDate::from_ymd_opt(year, month, 1).map(|d| d.format("%B %Y").to_string()).unwrap_or_default()
}

/// `[from, to)` of a whole calendar month in `tz`.
pub fn month_bounds<Tz: TimeZone>(tz: &Tz, year: i32, month: u32) -> Option<(i64, i64)> {
    let first = NaiveDate::from_ymd_opt(year, month, 1)?;
    let (ny, nm) = shift_month(year, month, 1);
    let next = NaiveDate::from_ymd_opt(ny, nm, 1)?;
    Some((day_start(tz, first), day_start(tz, next)))
}

/// Day numbers of `year`-`month` that hold at least one of the timestamps.
pub fn days_with_messages<Tz: TimeZone>(tz: &Tz, year: i32, month: u32, times: &[i64]) -> BTreeSet<u32> {
    times
        .iter()
        .filter_map(|ts| local_date(tz, *ts))
        .filter(|d| d.year() == year && d.month() == month)
        .map(|d| d.day())
        .collect()
}

/// One square of the month grid; `day` 0 is a filler before the 1st or after the last day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CalCell {
    pub day: u32,
    pub enabled: bool,
}

/// The month as six Monday-first rows of seven squares; `enabled` says which
/// day numbers can be picked.
pub fn month_cells(year: i32, month: u32, enabled: impl Fn(u32) -> bool) -> Vec<CalCell> {
    let lead = NaiveDate::from_ymd_opt(year, month, 1).map_or(0, |d| d.weekday().num_days_from_monday() as usize);
    let days = days_in_month(year, month) as usize;
    (0usize..42)
        .map(|i| match i.checked_sub(lead).filter(|d| *d < days) {
            Some(d) => {
                let day = d as u32 + 1;
                CalCell { day, enabled: enabled(day) }
            }
            None => CalCell { day: 0, enabled: false },
        })
        .collect()
}

/// Keeps `(year, month)` between `lo` and `hi` (both inclusive, `lo <= hi`).
pub fn clamp_month(ym: (i32, u32), lo: (i32, u32), hi: (i32, u32)) -> (i32, u32) {
    ym.max(lo).min(hi)
}

#[cfg(test)]
mod tests {
    use chrono::FixedOffset;

    use super::*;

    fn at<Tz: TimeZone>(tz: &Tz, y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Tz> {
        tz.with_ymd_and_hms(y, m, d, h, min, 0).unwrap()
    }

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn today_is_the_local_calendar_day_not_the_utc_one() {
        // UTC+03:30: 01:00 local on the 10th is still the 9th in UTC.
        let tz = FixedOffset::east_opt(3 * 3600 + 1800).unwrap();
        let now = at(&tz, 2026, 3, 10, 1, 0);
        let (from, to) = range_bounds(DateRange::Today, &now);
        assert_eq!(from, Some(at(&tz, 2026, 3, 10, 0, 0).timestamp()));
        assert_eq!(to, Some(at(&tz, 2026, 3, 11, 0, 0).timestamp()));
        assert_eq!(to.unwrap() - from.unwrap(), 86_400);
        // A message at 23:59 local is inside, one a minute later is not.
        let late = at(&tz, 2026, 3, 10, 23, 59).timestamp();
        assert!(late >= from.unwrap() && late < to.unwrap());
        assert!(late + 60 >= to.unwrap());
        // West of UTC the local day starts later than the UTC one.
        let west = FixedOffset::west_opt(5 * 3600).unwrap();
        let now = at(&west, 2026, 3, 10, 22, 0);
        assert_eq!(range_bounds(DateRange::Today, &now).0, Some(at(&west, 2026, 3, 10, 0, 0).timestamp()));
    }

    #[test]
    fn last_days_count_today_and_cross_month_ends() {
        let tz = FixedOffset::east_opt(0).unwrap();
        let now = at(&tz, 2026, 3, 3, 12, 0);
        let (from, to) = range_bounds(DateRange::Last7, &now);
        assert_eq!(from, Some(at(&tz, 2026, 2, 25, 0, 0).timestamp()));
        assert_eq!(to, Some(at(&tz, 2026, 3, 4, 0, 0).timestamp()));
        let (from, _) = range_bounds(DateRange::Last30, &now);
        assert_eq!(from, Some(at(&tz, 2026, 2, 2, 0, 0).timestamp()));
        assert_eq!(range_bounds(DateRange::Any, &now), (None, None));
    }

    #[test]
    fn custom_range_includes_both_days_in_any_order() {
        let tz = FixedOffset::east_opt(2 * 3600).unwrap();
        let now = at(&tz, 2026, 3, 30, 12, 0);
        let want = (Some(at(&tz, 2026, 3, 5, 0, 0).timestamp()), Some(at(&tz, 2026, 3, 9, 0, 0).timestamp()));
        assert_eq!(range_bounds(DateRange::Custom { from: day(2026, 3, 5), to: day(2026, 3, 8) }, &now), want);
        assert_eq!(range_bounds(DateRange::Custom { from: day(2026, 3, 8), to: day(2026, 3, 5) }, &now), want);
        let single = range_bounds(DateRange::Custom { from: day(2026, 12, 31), to: day(2026, 12, 31) }, &now);
        assert_eq!(single.1.unwrap() - single.0.unwrap(), 86_400);
    }

    #[test]
    fn range_labels_and_presets() {
        assert_eq!(DateRange::preset(2), Some(DateRange::Last7));
        assert_eq!(DateRange::preset(4), None);
        assert_eq!(DateRange::Any.label(), "Any time");
        let r = DateRange::Custom { from: day(2026, 3, 20), to: day(2026, 3, 5) };
        assert_eq!(r.label(), "05 Mar 2026 - 20 Mar 2026");
        assert_eq!(DateRange::Custom { from: day(2026, 3, 5), to: day(2026, 3, 5) }.label(), "05 Mar 2026");
    }

    #[test]
    fn filter_mapping_carries_every_part() {
        let tz = FixedOffset::east_opt(0).unwrap();
        let now = at(&tz, 2026, 3, 10, 12, 0);
        let f = build_filter("  hi  ", 3, Some(SenderFilter::Me), DateRange::Today, &now);
        assert_eq!(f.text, "hi");
        assert_eq!(f.kind, SearchKind::Documents);
        assert_eq!(f.sender, Some(SenderFilter::Me));
        assert_eq!(f.from_ts, Some(at(&tz, 2026, 3, 10, 0, 0).timestamp()));
        assert!(!f.is_empty());
        assert!(build_filter("", 0, None, DateRange::Any, &now).is_empty());
        let kinds: Vec<SearchKind> = (0..6).map(kind_from_index).collect();
        assert_eq!(
            kinds,
            [
                SearchKind::All,
                SearchKind::Media,
                SearchKind::Links,
                SearchKind::Documents,
                SearchKind::Voice,
                SearchKind::All
            ]
        );
    }

    #[test]
    fn month_arithmetic() {
        assert_eq!(days_in_month(2026, 2), 28);
        assert_eq!(days_in_month(2028, 2), 29);
        assert_eq!(days_in_month(2026, 12), 31);
        assert_eq!(shift_month(2026, 1, -1), (2025, 12));
        assert_eq!(shift_month(2026, 12, 1), (2027, 1));
        assert_eq!(shift_month(2026, 3, -15), (2024, 12));
        assert_eq!(month_title(2026, 3), "March 2026");
        assert_eq!(clamp_month((2026, 5), (2026, 1), (2026, 3)), (2026, 3));
        assert_eq!(clamp_month((2024, 5), (2026, 1), (2026, 3)), (2026, 1));
        let tz = FixedOffset::east_opt(3600).unwrap();
        let (from, to) = month_bounds(&tz, 2026, 2).unwrap();
        assert_eq!(from, at(&tz, 2026, 2, 1, 0, 0).timestamp());
        assert_eq!(to, at(&tz, 2026, 3, 1, 0, 0).timestamp());
    }

    #[test]
    fn month_grid_is_monday_first_and_marks_enabled_days() {
        // 1 March 2026 is a Sunday: six filler squares first.
        let cells = month_cells(2026, 3, |d| d == 1 || d == 31);
        assert_eq!(cells.len(), 42);
        assert!(cells[..6].iter().all(|c| c.day == 0 && !c.enabled));
        assert_eq!(cells[6], CalCell { day: 1, enabled: true });
        assert_eq!(cells[7], CalCell { day: 2, enabled: false });
        assert_eq!(cells[36], CalCell { day: 31, enabled: true });
        assert!(cells[37..].iter().all(|c| c.day == 0));
        // 1 June 2026 is a Monday: no filler.
        assert_eq!(month_cells(2026, 6, |_| true)[0], CalCell { day: 1, enabled: true });
    }

    #[test]
    fn days_with_messages_use_the_local_day() {
        let tz = FixedOffset::east_opt(3 * 3600 + 1800).unwrap();
        let times = [
            at(&tz, 2026, 3, 1, 0, 10).timestamp(), // 20:40 UTC on 28 Feb
            at(&tz, 2026, 3, 1, 23, 50).timestamp(),
            at(&tz, 2026, 3, 15, 12, 0).timestamp(),
            at(&tz, 2026, 4, 2, 12, 0).timestamp(),
        ];
        let days = days_with_messages(&tz, 2026, 3, &times);
        assert_eq!(days.into_iter().collect::<Vec<_>>(), [1, 15]);
    }

    #[test]
    fn debounce_waits_for_quiet_period() {
        let mut d = Debouncer::new();
        d.input("he", 1000);
        assert_eq!(d.poll(1100), SearchAction::Idle);
        d.input("hel", 1200);
        assert_eq!(d.poll(1300), SearchAction::Idle);
        assert_eq!(d.next_deadline(), Some(1450));
        assert_eq!(d.poll(1450), SearchAction::Query("hel".into()));
        assert_eq!(d.poll(2000), SearchAction::Idle);
        assert_eq!(d.next_deadline(), None);
    }

    #[test]
    fn debounce_skips_repeated_query_and_trims() {
        let mut d = Debouncer::new();
        d.input(" hi ", 0);
        assert_eq!(d.poll(250), SearchAction::Query("hi".into()));
        d.input("hi", 300);
        assert_eq!(d.poll(600), SearchAction::Idle);
        d.reset();
        d.input("hi", 700);
        assert_eq!(d.poll(950), SearchAction::Query("hi".into()));
    }

    #[test]
    fn debounce_clears_immediately_on_blank() {
        let mut d = Debouncer::new();
        d.input("abc", 0);
        assert_eq!(d.poll(250), SearchAction::Query("abc".into()));
        d.input("", 300);
        assert_eq!(d.poll(300), SearchAction::Clear);
        d.input("  ", 400);
        assert_eq!(d.poll(400), SearchAction::Idle);
    }

    #[test]
    fn nav_wraps_both_ways() {
        let mut n = ResultNav::new(3);
        assert_eq!(n.counter(), "1 of 3");
        assert_eq!(n.next(), Some(1));
        assert_eq!(n.next(), Some(2));
        assert_eq!(n.next(), Some(0));
        assert_eq!(n.prev(), Some(2));
        assert_eq!(n.counter(), "3 of 3");
        assert_eq!(n.current_label(), 3);
        assert_eq!(n.select(1), Some(1));
        assert_eq!(n.select(3), None);
        assert_eq!(n.index(), Some(1));
    }

    #[test]
    fn nav_empty_and_single() {
        let mut e = ResultNav::new(0);
        assert_eq!((e.next(), e.prev(), e.index()), (None, None, None));
        assert_eq!((e.counter(), e.current_label()), (String::new(), 0));
        let mut one = ResultNav::new(1);
        assert_eq!((one.next(), one.prev()), (Some(0), Some(0)));
    }

    #[test]
    fn find_matches_is_case_insensitive_and_non_overlapping() {
        assert_eq!(find_matches("Hello hello", "HELLO"), vec![0..5, 6..11]);
        assert_eq!(find_matches("aaaa", "aa"), vec![0..2, 2..4]);
        assert!(find_matches("abc", "").is_empty());
        assert!(find_matches("abc", "x").is_empty());
    }

    #[test]
    fn find_matches_persian_and_unicode_boundaries() {
        let text = "سلام دوست من";
        let m = find_matches(text, "دوست");
        assert_eq!(m.len(), 1);
        assert_eq!(&text[m[0].clone()], "دوست");
        let cyr = "Привет МИР";
        let m = find_matches(cyr, "мир");
        assert_eq!(&cyr[m[0].clone()], "МИР");
    }

    #[test]
    fn snippet_short_text_has_no_ellipsis() {
        let s = build_snippet("hello world", "world");
        assert_eq!(s.text, "hello world");
        assert_eq!(&s.text[s.highlights[0].clone()], "world");
    }

    #[test]
    fn snippet_cuts_with_ellipsis_on_both_sides() {
        let text = format!("{}needle{}", "a".repeat(100), "b".repeat(100));
        let s = build_snippet(&text, "NEEDLE");
        assert!(s.text.starts_with('\u{2026}') && s.text.ends_with('\u{2026}'));
        assert_eq!(s.text.chars().count(), 1 + 30 + 6 + 30 + 1);
        assert_eq!(&s.text[s.highlights[0].clone()], "needle");
        let (before, matched, after) = s.split();
        assert_eq!((matched, before.chars().count(), after.chars().count()), ("needle", 31, 31));
    }

    #[test]
    fn snippet_persian_is_char_boundary_safe() {
        let text = format!("{} کلمه {}", "سلام ".repeat(30), "خداحافظ ".repeat(30));
        let s = build_snippet(&text, "کلمه");
        assert_eq!(&s.text[s.highlights[0].clone()], "کلمه");
        assert!(s.text.starts_with('\u{2026}') && s.text.ends_with('\u{2026}'));
    }

    #[test]
    fn snippet_match_at_edges_and_multiline() {
        let s = build_snippet("one\ntwo three", "two");
        assert_eq!(s.text, "one two three");
        assert_eq!(&s.text[s.highlights[0].clone()], "two");
        let s = build_snippet("start", "start");
        assert_eq!(s.highlights, vec![0..5]);
    }

    #[test]
    fn snippet_without_match_previews_start() {
        let text = "x".repeat(100);
        let s = build_snippet(&text, "zzz");
        assert!(s.highlights.is_empty());
        assert_eq!(s.text.chars().count(), 61);
        assert_eq!(s.split().1, "");
        assert_eq!(build_snippet("short", "zzz").text, "short");
    }

    #[test]
    fn snippet_highlights_repeat_matches_inside_window() {
        let s = build_snippet("ab ab ab", "ab");
        assert_eq!(s.highlights, vec![0..2, 3..5, 6..8]);
    }
}
