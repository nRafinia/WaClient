//! View-model for the status (stories) viewer and for poll bubbles.
//!
//! Everything here is pure: no timers, no UI handles. The integration layer
//! feeds wall-clock deltas into [`StatusPlayer::advance_time`] and copies the
//! results into the generated Slint structs.

use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use slint::Color;
use wa_store::{MessageKind, MessageRow, PollView, StatusGroup};

use crate::models::{format_chat_time, initials, kind_label, tint_for};

/// Auto-advance time for text and image slides.
pub const DEFAULT_SLIDE_MS: u32 = 5_000;
/// Auto-advance time for videos with an unknown length, and the cap for long ones.
pub const VIDEO_CAP_MS: u32 = 15_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlideKind {
    Text,
    Image,
    Video,
    Other,
}

impl SlideKind {
    pub fn from_message(kind: MessageKind) -> Self {
        match kind {
            MessageKind::Text => SlideKind::Text,
            MessageKind::Image => SlideKind::Image,
            MessageKind::Video => SlideKind::Video,
            _ => SlideKind::Other,
        }
    }

    /// Matches `StatusSlideItem.kind` in status_view.slint.
    pub fn as_int(self) -> i32 {
        match self {
            SlideKind::Text => 0,
            SlideKind::Image => 1,
            SlideKind::Video => 2,
            SlideKind::Other => 3,
        }
    }
}

#[derive(Debug, Clone)]
pub struct StatusListItem {
    pub name: String,
    pub initials: String,
    pub color: Color,
    pub count: usize,
    pub all_viewed: bool,
    pub last_time_label: String,
    pub preview: String,
}

/// The list as shown: own entry (if any) on top, then unviewed, then viewed.
#[derive(Debug, Clone, Default)]
pub struct StatusList {
    pub mine: Option<StatusListItem>,
    pub others: Vec<StatusListItem>,
}

#[derive(Debug, Clone)]
pub struct StatusSlide {
    pub id: String,
    pub kind: SlideKind,
    pub text: String,
    pub thumbnail: Option<Vec<u8>>,
    pub local_path: Option<String>,
    pub time_label: String,
    pub duration_ms: u32,
    /// Background of text slides.
    pub color: Color,
    /// Font family of text slides; empty is the default font.
    pub font: &'static str,
}

/// Background and font a text status was posted with (`media_mime` of our own
/// text statuses: `text/status;bg=AARRGGBB;font=N`). Received ones carry none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TextStyle {
    pub bg_argb: Option<u32>,
    pub font: Option<u8>,
}

pub fn parse_text_style(mime: Option<&str>) -> TextStyle {
    let mut style = TextStyle::default();
    let mut parts = mime.unwrap_or("").split(';');
    if parts.next().map(str::trim) != Some("text/status") {
        return style;
    }
    for part in parts {
        match part.trim().split_once('=') {
            Some(("bg", v)) if v.len() == 8 => style.bg_argb = u32::from_str_radix(v, 16).ok(),
            Some(("font", v)) => style.font = v.parse().ok(),
            _ => {}
        }
    }
    style
}

/// Family drawn for an engine font code; empty (the default font) for unknown ones.
pub fn font_family(code: Option<u8>) -> &'static str {
    match code {
        Some(0) => "Segoe UI",
        Some(1) => "Georgia",
        Some(2) => "Segoe Script",
        Some(6) => "Ink Free",
        Some(7) => "Bahnschrift",
        _ => "",
    }
}

fn last_ts(g: &StatusGroup) -> i64 {
    g.items.iter().map(|m| m.ts).max().unwrap_or(0)
}

fn display_name(g: &StatusGroup) -> String {
    g.sender_name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| g.sender_jid.split('@').next().unwrap_or("").to_owned())
}

fn preview_of(m: &MessageRow) -> String {
    let text = m.text.as_deref().unwrap_or("");
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if !one_line.is_empty() {
        return one_line;
    }
    match m.kind {
        MessageKind::Image => "📷 Photo".into(),
        MessageKind::Video => "🎥 Video".into(),
        _ => kind_label(m),
    }
}

fn list_item(g: &StatusGroup, name: String, now: DateTime<Local>) -> StatusListItem {
    let latest = g.items.iter().max_by_key(|m| m.ts);
    StatusListItem {
        initials: initials(&name),
        color: tint_for(&g.sender_jid),
        name,
        count: g.items.len(),
        all_viewed: g.all_viewed,
        last_time_label: latest.map(|m| format_chat_time(m.ts, now)).unwrap_or_default(),
        preview: latest.map(preview_of).unwrap_or_default(),
    }
}

/// Groups without items are dropped. Unviewed first, newest first within each half.
pub fn build_status_list(groups: &[StatusGroup], own_jid: &str, now: DateTime<Local>) -> StatusList {
    let mut mine = None;
    let mut others: Vec<&StatusGroup> = Vec::new();
    for g in groups.iter().filter(|g| !g.items.is_empty()) {
        if g.sender_jid == own_jid {
            mine = Some(list_item(g, "My status".to_owned(), now));
        } else {
            others.push(g);
        }
    }
    others.sort_by(|a, b| {
        a.all_viewed
            .cmp(&b.all_viewed)
            .then_with(|| last_ts(b).cmp(&last_ts(a)))
    });
    StatusList {
        mine,
        others: others
            .into_iter()
            .map(|g| list_item(g, display_name(g), now))
            .collect(),
    }
}

/// Display order matching [`build_status_list`] (own group excluded), so that
/// list index `i` opens `ordered_groups(...)[i]`.
pub fn ordered_groups<'a>(groups: &'a [StatusGroup], own_jid: &str) -> Vec<&'a StatusGroup> {
    let mut v: Vec<&StatusGroup> = groups
        .iter()
        .filter(|g| !g.items.is_empty() && g.sender_jid != own_jid)
        .collect();
    v.sort_by(|a, b| {
        a.all_viewed
            .cmp(&b.all_viewed)
            .then_with(|| last_ts(b).cmp(&last_ts(a)))
    });
    v
}

pub fn slide_duration_ms(kind: SlideKind, media_duration_secs: Option<i64>) -> u32 {
    match kind {
        SlideKind::Video => match media_duration_secs {
            Some(s) if s > 0 => (s.saturating_mul(1000)).min(VIDEO_CAP_MS as i64) as u32,
            _ => VIDEO_CAP_MS,
        },
        _ => DEFAULT_SLIDE_MS,
    }
}

pub fn status_slide(m: &MessageRow, now: DateTime<Local>) -> StatusSlide {
    let kind = SlideKind::from_message(m.kind);
    let style = if kind == SlideKind::Text { parse_text_style(m.media_mime.as_deref()) } else { TextStyle::default() };
    StatusSlide {
        id: m.id.clone(),
        kind,
        text: m.text.clone().unwrap_or_default(),
        thumbnail: m.thumbnail.clone(),
        local_path: m.local_path.clone(),
        time_label: format_chat_time(m.ts, now),
        duration_ms: slide_duration_ms(kind, m.media_duration),
        color: style.bg_argb.map_or_else(|| tint_for(&m.id), Color::from_argb_encoded),
        font: font_family(style.font),
    }
}

/// Slides of one group, oldest first.
pub fn build_slides(g: &StatusGroup, now: DateTime<Local>) -> Vec<StatusSlide> {
    let mut items: Vec<&MessageRow> = g.items.iter().collect();
    items.sort_by_key(|m| m.ts);
    items.into_iter().map(|m| status_slide(m, now)).collect()
}

/// State of a slide's full-size image (decoded off the UI thread).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FullState {
    Pending,
    Decoded,
    Failed,
}

/// What the viewer shows for a slide, and whether its timer may run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlidePhase {
    /// The content is on screen (text, a decoded photo, a video with Play).
    Shown,
    /// Downloading or decoding: thumbnail and spinner, timer waits.
    Loading,
    /// "Couldn't load" with a Retry button, timer waits.
    Failed,
}

pub fn slide_phase(kind: SlideKind, has_local: bool, full: FullState, in_flight: bool, failed: bool) -> SlidePhase {
    let missing = || {
        if failed && !in_flight {
            SlidePhase::Failed
        } else {
            SlidePhase::Loading
        }
    };
    match kind {
        SlideKind::Text | SlideKind::Other => SlidePhase::Shown,
        SlideKind::Video if has_local => SlidePhase::Shown,
        SlideKind::Image if has_local => match full {
            FullState::Decoded => SlidePhase::Shown,
            FullState::Pending => SlidePhase::Loading,
            FullState::Failed => SlidePhase::Failed,
        },
        SlideKind::Image | SlideKind::Video => missing(),
    }
}

/// Whether entering a slide should ask the engine for its media: once per id
/// (`requested`), never when the file is already on disk.
pub fn should_request(kind: SlideKind, has_local: bool, requested: bool) -> bool {
    matches!(kind, SlideKind::Image | SlideKind::Video) && !has_local && !requested
}

/// A slide must be shown this long before it counts as viewed: receipts
/// cannot be undone, so slides skipped through must not send one.
pub const DWELL_MS: u64 = 800;
/// A slide must be current this long before its media is requested, so
/// skipping through slides does not start a download per slide.
pub const REQUEST_DELAY_MS: u64 = 300;
/// Most downloads running at once.
pub const MAX_IN_FLIGHT: usize = 3;

/// True once the content has been on screen without interruption for
/// [`DWELL_MS`]. `None` means it is not (or no longer) on screen.
pub fn dwell_met(shown_since: Option<Instant>, now: Instant) -> bool {
    shown_since.is_some_and(|t| now.saturating_duration_since(t) >= Duration::from_millis(DWELL_MS))
}

/// Whether the current slide may be requested now: it has been current long
/// enough and a download slot is free.
pub fn request_due(current_since: Instant, now: Instant, in_flight: usize) -> bool {
    in_flight < MAX_IN_FLIGHT
        && now.saturating_duration_since(current_since) >= Duration::from_millis(REQUEST_DELAY_MS)
}

// ---------------------------------------------------------------- player

#[derive(Debug, Clone)]
pub struct PlayerGroup {
    pub sender_jid: String,
    /// `(message id, auto-advance ms)` per slide.
    pub slides: Vec<(String, u32)>,
    /// False for our own statuses: nothing to report as viewed.
    pub trackable: bool,
}

impl PlayerGroup {
    pub fn from_slides(sender_jid: &str, slides: &[StatusSlide], trackable: bool) -> Self {
        PlayerGroup {
            sender_jid: sender_jid.to_owned(),
            slides: slides.iter().map(|s| (s.id.clone(), s.duration_ms)).collect(),
            trackable,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    /// The visible slide changed; call `on_enter_slide`.
    Moved,
    /// Nothing changed (already at the very start).
    Stayed,
    /// Ran past the last slide of the last group: close the viewer.
    Finished,
}

#[derive(Debug, Clone)]
pub struct StatusPlayer {
    groups: Vec<PlayerGroup>,
    group: usize,
    slide: usize,
    elapsed_ms: u32,
    viewed: std::collections::HashSet<String>,
}

impl StatusPlayer {
    /// Empty groups are dropped. `start_group` indexes the groups as given
    /// and is clamped.
    pub fn new(groups: Vec<PlayerGroup>, start_group: usize) -> Self {
        let start_id = groups.get(start_group).map(|g| g.sender_jid.clone());
        let groups: Vec<PlayerGroup> = groups.into_iter().filter(|g| !g.slides.is_empty()).collect();
        let group = start_id
            .and_then(|id| groups.iter().position(|g| g.sender_jid == id))
            .unwrap_or(0);
        StatusPlayer {
            groups,
            group,
            slide: 0,
            elapsed_ms: 0,
            viewed: Default::default(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }
    pub fn group_index(&self) -> usize {
        self.group
    }
    pub fn slide_index(&self) -> usize {
        self.slide
    }
    pub fn slide_count(&self) -> usize {
        self.groups.get(self.group).map_or(0, |g| g.slides.len())
    }
    pub fn current_sender(&self) -> Option<&str> {
        self.groups.get(self.group).map(|g| g.sender_jid.as_str())
    }
    pub fn current_id(&self) -> Option<&str> {
        self.groups
            .get(self.group)
            .and_then(|g| g.slides.get(self.slide))
            .map(|(id, _)| id.as_str())
    }
    pub fn current_duration_ms(&self) -> u32 {
        self.groups
            .get(self.group)
            .and_then(|g| g.slides.get(self.slide))
            .map_or(DEFAULT_SLIDE_MS, |(_, d)| (*d).max(1))
    }

    /// Fill of the current slide's progress bar, 0..=1.
    pub fn progress(&self) -> f32 {
        (self.elapsed_ms as f32 / self.current_duration_ms() as f32).clamp(0.0, 1.0)
    }

    fn reset_slide(&mut self, group: usize, slide: usize) {
        self.group = group;
        self.slide = slide;
        self.elapsed_ms = 0;
    }

    pub fn next(&mut self) -> Nav {
        if self.is_empty() {
            return Nav::Finished;
        }
        if self.slide + 1 < self.slide_count() {
            self.reset_slide(self.group, self.slide + 1);
            Nav::Moved
        } else {
            self.skip_group()
        }
    }

    /// Previous slide; at a group's first slide, the last slide of the
    /// previous group; at the very start, the current slide restarts.
    pub fn prev(&mut self) -> Nav {
        if self.is_empty() {
            return Nav::Stayed;
        }
        if self.slide > 0 {
            self.reset_slide(self.group, self.slide - 1);
            Nav::Moved
        } else if self.group > 0 {
            let g = self.group - 1;
            let last = self.groups[g].slides.len() - 1;
            self.reset_slide(g, last);
            Nav::Moved
        } else {
            let restarted = self.elapsed_ms > 0;
            self.elapsed_ms = 0;
            if restarted { Nav::Moved } else { Nav::Stayed }
        }
    }

    pub fn skip_group(&mut self) -> Nav {
        if self.is_empty() || self.group + 1 >= self.groups.len() {
            return Nav::Finished;
        }
        self.reset_slide(self.group + 1, 0);
        Nav::Moved
    }

    /// Feed elapsed wall time while playing (not while paused). Auto-advances.
    pub fn advance_time(&mut self, dt_ms: u32) -> Option<Nav> {
        if self.is_empty() {
            return None;
        }
        self.elapsed_ms = self.elapsed_ms.saturating_add(dt_ms);
        if self.elapsed_ms >= self.current_duration_ms() {
            Some(self.next())
        } else {
            None
        }
    }

    /// Call whenever a slide becomes visible. Returns the message id to send
    /// `MarkStatusViewed` for, at most once per id per session, and never for
    /// untrackable (own) groups. The sender is [`Self::current_sender`].
    pub fn on_enter_slide(&mut self) -> Option<String> {
        let group = self.groups.get(self.group)?;
        if !group.trackable {
            return None;
        }
        let id = group.slides.get(self.slide)?.0.clone();
        self.viewed.insert(id.clone()).then_some(id)
    }
}

// ----------------------------------------------------------------- polls

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PollOptionView {
    pub name: String,
    pub votes: u32,
    /// 0..=100, share of all votes cast.
    pub percent: u32,
    pub mine: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PollViewModel {
    pub question: String,
    pub options: Vec<PollOptionView>,
    pub total_votes: u32,
    pub multi: bool,
    /// Index of a quiz's correct option; `None` for ordinary polls.
    pub correct: Option<usize>,
}

/// How a quiz option is marked once the answer is revealed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuizMark {
    None,
    Correct,
    /// The viewer's own wrong choice.
    Wrong,
}

/// What a quiz shows after the viewer answered (or for their own quiz): a
/// mark per option and the correct option's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuizReveal {
    pub marks: Vec<QuizMark>,
    pub correct_name: String,
}

/// The revealed state of a quiz: `None` for ordinary polls and while the
/// viewer has not voted on somebody else's quiz.
pub fn quiz_reveal(p: &PollViewModel, own_poll: bool) -> Option<QuizReveal> {
    let correct = p.correct?;
    let correct_name = p.options.get(correct)?.name.clone();
    if !own_poll && !p.options.iter().any(|o| o.mine) {
        return None;
    }
    let marks = p
        .options
        .iter()
        .enumerate()
        .map(|(i, o)| match (i == correct, o.mine) {
            (true, _) => QuizMark::Correct,
            (false, true) => QuizMark::Wrong,
            (false, false) => QuizMark::None,
        })
        .collect();
    Some(QuizReveal { marks, correct_name })
}

pub fn poll_view_model(p: &PollView) -> PollViewModel {
    let total: u64 = p.options.iter().map(|(_, v, _)| u64::from(*v)).sum();
    let options = p
        .options
        .iter()
        .map(|(name, votes, mine)| PollOptionView {
            name: name.clone(),
            votes: *votes,
            percent: if total == 0 {
                0
            } else {
                ((u64::from(*votes) * 100 + total / 2) / total).min(100) as u32
            },
            mine: *mine,
        })
        .collect();
    PollViewModel {
        question: p.question.clone(),
        options,
        total_votes: total.min(u64::from(u32::MAX)) as u32,
        // 0 means "no limit" in the protocol; quizzes take one answer.
        multi: p.selectable_count != 1 && p.correct.is_none(),
        correct: p.correct,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn text_style_is_read_from_the_mime() {
        use super::*;
        let s = parse_text_style(Some("text/status;bg=FF1E6E4F;font=7"));
        assert_eq!(s, TextStyle { bg_argb: Some(0xFF1E6E4F), font: Some(7) });
        assert_eq!(font_family(s.font), "Bahnschrift");
        assert_eq!(parse_text_style(Some("text/status;font=3")).font, Some(3));
        assert_eq!(font_family(Some(3)), "");
        assert_eq!(parse_text_style(Some("text/status;bg=zz1E6E4F")), TextStyle::default());
        assert_eq!(parse_text_style(Some("text/status;bg=1E6E4F")), TextStyle::default());
        assert_eq!(parse_text_style(Some("image/jpeg;bg=FF1E6E4F")), TextStyle::default());
        assert_eq!(parse_text_style(Some("")), TextStyle::default());
    }

    use super::*;
    use chrono::TimeZone;
    use std::future::Future;
    use std::task::{Context, Poll, Wake, Waker};
    use std::sync::Arc;
    use wa_store::MessageStatus;

    fn now() -> DateTime<Local> {
        Local.timestamp_opt(1_700_100_000, 0).unwrap()
    }

    fn row(id: &str, ts: i64, kind: MessageKind) -> MessageRow {
        MessageRow {
            chat_jid: "status@broadcast".into(),
            id: id.into(),
            sender_jid: "x@s.whatsapp.net".into(),
            sender_name: None,
            from_me: false,
            ts,
            kind,
            text: None,
            quoted_id: None,
            quoted_preview: None,
            media_mime: None,
            media_name: None,
            media_size: None,
            media_duration: None,
            thumbnail: None,
            local_path: None,
            status: MessageStatus::from_i64(0),
            revoked: false,
            edited: false,
            ..Default::default()
        }
    }

    fn group(jid: &str, name: Option<&str>, ts: &[i64], viewed: bool) -> StatusGroup {
        StatusGroup {
            sender_jid: jid.into(),
            sender_name: name.map(Into::into),
            items: ts
                .iter()
                .enumerate()
                .map(|(i, t)| row(&format!("{jid}-{i}"), *t, MessageKind::Text))
                .collect(),
            all_viewed: viewed,
        }
    }

    #[test]
    fn list_orders_unviewed_first_newest_first_and_splits_mine() {
        let groups = vec![
            group("a@s", Some("Alice"), &[100], true),
            group("me@s", Some("Me"), &[50], true),
            group("b@s", Some("Bob"), &[200], false),
            group("c@s", Some("Carol"), &[300], false),
            group("d@s", Some("Dan"), &[400], true),
            group("e@s", Some("Empty"), &[], false),
        ];
        let l = build_status_list(&groups, "me@s", now());
        let names: Vec<_> = l.others.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["Carol", "Bob", "Dan", "Alice"]);
        let mine = l.mine.unwrap();
        assert_eq!(mine.name, "My status");
        assert_eq!(mine.count, 1);
        let order: Vec<_> = ordered_groups(&groups, "me@s")
            .iter()
            .map(|g| g.sender_jid.as_str())
            .collect();
        assert_eq!(order, ["c@s", "b@s", "d@s", "a@s"]);
    }

    #[test]
    fn list_item_fields() {
        let mut g = group("a@s", None, &[100, 300], false);
        g.items[1].text = Some("hello\n  world".into());
        let l = build_status_list(&[g], "me@s", now());
        let i = &l.others[0];
        assert_eq!(i.name, "a");
        assert_eq!(i.count, 2);
        assert_eq!(i.preview, "hello world");
        assert!(l.mine.is_none());
    }

    #[test]
    fn slide_kinds_and_durations() {
        assert_eq!(slide_duration_ms(SlideKind::Text, None), 5_000);
        assert_eq!(slide_duration_ms(SlideKind::Image, Some(99)), 5_000);
        assert_eq!(slide_duration_ms(SlideKind::Video, None), 15_000);
        assert_eq!(slide_duration_ms(SlideKind::Video, Some(0)), 15_000);
        assert_eq!(slide_duration_ms(SlideKind::Video, Some(7)), 7_000);
        assert_eq!(slide_duration_ms(SlideKind::Video, Some(120)), 15_000);
        let mut v = row("v", 10, MessageKind::Video);
        v.media_duration = Some(4);
        let s = status_slide(&v, now());
        assert_eq!(s.kind, SlideKind::Video);
        assert_eq!(s.duration_ms, 4_000);
        assert_eq!(status_slide(&row("o", 1, MessageKind::Audio), now()).kind, SlideKind::Other);
    }

    #[test]
    fn slides_are_oldest_first() {
        let mut g = group("a@s", None, &[], false);
        g.items = vec![row("late", 20, MessageKind::Text), row("early", 10, MessageKind::Text)];
        let ids: Vec<_> = build_slides(&g, now()).into_iter().map(|s| s.id).collect();
        assert_eq!(ids, ["early", "late"]);
    }

    #[test]
    fn phases_wait_for_media_and_report_failures() {
        use FullState::{Decoded, Pending};
        use SlideKind::{Image, Other, Text, Video};
        use SlidePhase::{Loading, Shown};
        let failed = SlidePhase::Failed;
        assert_eq!(slide_phase(Text, false, Pending, false, false), Shown);
        assert_eq!(slide_phase(Other, false, Pending, false, false), Shown);
        assert_eq!(slide_phase(Image, false, Pending, true, false), Loading);
        assert_eq!(slide_phase(Image, false, Pending, false, false), Loading);
        assert_eq!(slide_phase(Image, false, Pending, false, true), failed);
        // A retry in flight is loading again, not failed.
        assert_eq!(slide_phase(Image, false, Pending, true, true), Loading);
        assert_eq!(slide_phase(Image, true, Pending, false, false), Loading);
        assert_eq!(slide_phase(Image, true, Decoded, false, false), Shown);
        assert_eq!(slide_phase(Image, true, FullState::Failed, false, false), failed);
        assert_eq!(slide_phase(Video, false, Pending, true, false), Loading);
        assert_eq!(slide_phase(Video, false, Pending, false, true), failed);
        assert_eq!(slide_phase(Video, true, Pending, false, false), Shown);
    }

    #[test]
    fn media_is_requested_once_and_only_when_missing() {
        assert!(should_request(SlideKind::Image, false, false));
        assert!(should_request(SlideKind::Video, false, false));
        assert!(!should_request(SlideKind::Image, false, true));
        assert!(!should_request(SlideKind::Image, true, false));
        assert!(!should_request(SlideKind::Text, false, false));
        assert!(!should_request(SlideKind::Other, false, false));
    }

    #[test]
    fn dwell_needs_continuous_time_on_screen() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        assert!(!dwell_met(None, t0 + ms(5_000)));
        assert!(!dwell_met(Some(t0), t0));
        assert!(!dwell_met(Some(t0), t0 + ms(DWELL_MS - 1)));
        assert!(dwell_met(Some(t0), t0 + ms(DWELL_MS)));
        // A clock that is behind the start never counts.
        assert!(!dwell_met(Some(t0 + ms(10)), t0));
    }

    #[test]
    fn requests_wait_for_delay_and_a_free_slot() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        assert!(!request_due(t0, t0 + ms(REQUEST_DELAY_MS - 1), 0));
        assert!(request_due(t0, t0 + ms(REQUEST_DELAY_MS), 0));
        assert!(request_due(t0, t0 + ms(REQUEST_DELAY_MS), MAX_IN_FLIGHT - 1));
        assert!(!request_due(t0, t0 + ms(10_000), MAX_IN_FLIGHT));
    }

    fn pg(jid: &str, n: usize, trackable: bool) -> PlayerGroup {
        PlayerGroup {
            sender_jid: jid.into(),
            slides: (0..n).map(|i| (format!("{jid}{i}"), 5_000)).collect(),
            trackable,
        }
    }

    #[test]
    fn player_navigation_and_edges() {
        let mut p = StatusPlayer::new(vec![pg("a", 2, true), pg("b", 1, true)], 0);
        assert_eq!(p.prev(), Nav::Stayed);
        assert_eq!(p.next(), Nav::Moved);
        assert_eq!((p.group_index(), p.slide_index()), (0, 1));
        assert_eq!(p.next(), Nav::Moved);
        assert_eq!((p.group_index(), p.slide_index()), (1, 0));
        assert_eq!(p.prev(), Nav::Moved);
        assert_eq!((p.group_index(), p.slide_index()), (0, 1));
        assert_eq!(p.skip_group(), Nav::Moved);
        assert_eq!(p.current_id(), Some("b0"));
        assert_eq!(p.next(), Nav::Finished);
        assert_eq!(p.skip_group(), Nav::Finished);
    }

    #[test]
    fn player_start_group_and_empty() {
        let p = StatusPlayer::new(vec![pg("a", 1, true), pg("b", 1, true)], 1);
        assert_eq!(p.current_sender(), Some("b"));
        let p = StatusPlayer::new(vec![pg("a", 0, true), pg("b", 1, true)], 0);
        assert_eq!(p.current_sender(), Some("b"));
        let p = StatusPlayer::new(vec![pg("a", 1, true)], 9);
        assert_eq!(p.current_sender(), Some("a"));
        let mut e = StatusPlayer::new(vec![], 0);
        assert!(e.is_empty());
        assert_eq!(e.next(), Nav::Finished);
        assert_eq!(e.prev(), Nav::Stayed);
        assert_eq!(e.advance_time(10), None);
        assert_eq!(e.on_enter_slide(), None);
    }

    #[test]
    fn player_auto_advance_and_progress() {
        let mut p = StatusPlayer::new(vec![pg("a", 2, true)], 0);
        assert_eq!(p.advance_time(2_500), None);
        assert!((p.progress() - 0.5).abs() < 1e-6);
        assert_eq!(p.advance_time(2_500), Some(Nav::Moved));
        assert_eq!(p.progress(), 0.0);
        assert_eq!(p.slide_index(), 1);
        assert_eq!(p.advance_time(9_999), Some(Nav::Finished));
    }

    #[test]
    fn player_prev_restarts_first_slide() {
        let mut p = StatusPlayer::new(vec![pg("a", 1, true)], 0);
        p.advance_time(1_000);
        assert_eq!(p.prev(), Nav::Moved);
        assert_eq!(p.progress(), 0.0);
    }

    #[test]
    fn mark_viewed_once_per_id_and_not_for_own() {
        let mut p = StatusPlayer::new(vec![pg("a", 2, true), pg("me", 1, false)], 0);
        assert_eq!(p.on_enter_slide().as_deref(), Some("a0"));
        assert_eq!(p.on_enter_slide(), None);
        p.next();
        assert_eq!(p.on_enter_slide().as_deref(), Some("a1"));
        p.prev();
        assert_eq!(p.on_enter_slide(), None);
        p.skip_group();
        p.skip_group();
        assert_eq!(p.current_sender(), Some("me"));
        assert_eq!(p.on_enter_slide(), None);
    }

    fn poll(opts: &[(&str, u32, bool)], sel: u32) -> PollView {
        PollView {
            question: "Q?".into(),
            options: opts.iter().map(|(n, v, m)| (n.to_string(), *v, *m)).collect(),
            selectable_count: sel,
            correct: None,
        }
    }

    #[test]
    fn quiz_is_single_answer_and_reveals_after_voting() {
        let mut view = poll(&[("a", 1, false), ("b", 2, false), ("c", 0, false)], 0);
        view.correct = Some(1);
        let vm = poll_view_model(&view);
        assert!(!vm.multi, "a quiz never allows several answers");
        assert_eq!(quiz_reveal(&vm, false), None, "hidden until the viewer votes");
        let own = quiz_reveal(&vm, true).expect("own quiz");
        assert_eq!(own.marks, [QuizMark::None, QuizMark::Correct, QuizMark::None]);
        assert_eq!(own.correct_name, "b");

        view.options[0].2 = true;
        let wrong = quiz_reveal(&poll_view_model(&view), false).expect("voted");
        assert_eq!(wrong.marks, [QuizMark::Wrong, QuizMark::Correct, QuizMark::None]);
        view.options[0].2 = false;
        view.options[1].2 = true;
        let right = quiz_reveal(&poll_view_model(&view), false).expect("voted");
        assert_eq!(right.marks, [QuizMark::None, QuizMark::Correct, QuizMark::None]);
    }

    #[test]
    fn ordinary_poll_and_stale_correct_index_have_no_reveal() {
        let vm = poll_view_model(&poll(&[("a", 1, true), ("b", 0, false)], 1));
        assert_eq!(quiz_reveal(&vm, true), None);
        let mut view = poll(&[("a", 1, true)], 1);
        view.correct = Some(5);
        assert_eq!(quiz_reveal(&poll_view_model(&view), true), None);
    }

    #[test]
    fn poll_zero_votes() {
        let vm = poll_view_model(&poll(&[("a", 0, false), ("b", 0, false)], 1));
        assert_eq!(vm.total_votes, 0);
        assert!(vm.options.iter().all(|o| o.percent == 0));
        assert!(!vm.multi);
    }

    #[test]
    fn poll_ties_and_rounding() {
        let vm = poll_view_model(&poll(&[("a", 1, true), ("b", 1, false), ("c", 1, false)], 0));
        assert_eq!(vm.total_votes, 3);
        assert!(vm.multi);
        assert!(vm.options.iter().all(|o| o.percent == 33));
        assert!(vm.options[0].mine && !vm.options[1].mine);
        let vm = poll_view_model(&poll(&[("a", 2, false), ("b", 1, false)], 1));
        assert_eq!((vm.options[0].percent, vm.options[1].percent), (67, 33));
        let vm = poll_view_model(&poll(&[("a", 5, false)], 2));
        assert_eq!(vm.options[0].percent, 100);
        assert!(vm.multi);
        let vm = poll_view_model(&poll(&[("a", 1, false), ("b", 1, false)], 1));
        assert_eq!((vm.options[0].percent, vm.options[1].percent), (50, 50));
    }

    // ------------------------------------------------ slint compile checks

    struct Noop;
    impl Wake for Noop {
        fn wake(self: Arc<Self>) {}
    }

    fn block_on<F: Future>(f: F) -> F::Output {
        let waker = Waker::from(Arc::new(Noop));
        let mut cx = Context::from_waker(&waker);
        let mut f = Box::pin(f);
        loop {
            if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
                return v;
            }
        }
    }

    fn assert_compiles(file: &str) {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("ui").join(file);
        let compiler = slint_interpreter::Compiler::default();
        let result = block_on(compiler.build_from_path(&path));
        let errors: Vec<String> = result
            .diagnostics()
            .filter(|d| d.level() == slint_interpreter::DiagnosticLevel::Error)
            .map(|d| d.to_string())
            .collect();
        assert!(errors.is_empty(), "{file}: {errors:#?}");
        assert!(result.component_names().next().is_some(), "{file}: no exported components");
    }

    #[test]
    fn status_view_slint_compiles() {
        assert_compiles("status_view.slint");
    }

    #[test]
    fn poll_bubble_slint_compiles() {
        assert_compiles("poll_bubble.slint");
    }

    #[test]
    fn poll_create_slint_compiles() {
        assert_compiles("poll_create.slint");
    }
}
