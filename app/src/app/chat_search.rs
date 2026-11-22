//! In-chat search glue, pure and free of app state: the debounce state
//! machine, result navigation, and snippet / highlight building. The wiring
//! only has to feed it keystrokes and copy its output into Slint models.

use std::ops::Range;

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

#[cfg(test)]
mod tests {
    use super::*;

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
