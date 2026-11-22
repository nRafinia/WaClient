//! Polls: voting from a poll bubble and the "Create poll" dialog.
//!
//! Questions and options are message text: nothing here logs them.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use slint::{ComponentHandle, Model, ModelRc, VecModel};
use wa_engine::Command;

use super::{App, with_app};
use crate::status_view::{PollOptionView, PollViewModel};
use crate::{AttachActions, MainWindow, PollActions, PollField, PollState};

/// Most options a poll can have (WhatsApp's limit), and the fewest.
const MAX_OPTIONS: usize = 12;
const MIN_OPTIONS: usize = 2;
/// Longest question that is sent, in characters.
const QUESTION_MAX_CHARS: usize = 255;

/// Why a poll cannot be sent as typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollIssue {
    QuestionEmpty,
    QuestionTooLong,
    TooFewOptions,
    TooManyOptions,
    DuplicateOption,
}

impl PollIssue {
    /// Shown under the options when the poll is clearly wrong; "not finished
    /// yet" states have no message.
    fn hint(self) -> &'static str {
        match self {
            PollIssue::QuestionTooLong => "The question is too long (255 characters at most)",
            PollIssue::DuplicateOption => "Options must be different from each other",
            PollIssue::TooManyOptions => "A poll can have at most 12 options",
            PollIssue::QuestionEmpty | PollIssue::TooFewOptions => "",
        }
    }
}

/// A poll that passed validation: trimmed question, trimmed non-empty options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PollDraft {
    pub question: String,
    pub options: Vec<String>,
}

/// Checks what the dialog holds. Blank option fields are ignored (there is
/// always an empty one at the end); options must differ ignoring case.
pub fn validate_poll(question: &str, options: &[String]) -> Result<PollDraft, PollIssue> {
    let question = question.trim();
    if question.is_empty() {
        return Err(PollIssue::QuestionEmpty);
    }
    if question.chars().count() > QUESTION_MAX_CHARS {
        return Err(PollIssue::QuestionTooLong);
    }
    let options: Vec<String> = options.iter().map(|o| o.trim()).filter(|o| !o.is_empty()).map(str::to_string).collect();
    let mut seen = HashSet::new();
    if !options.iter().all(|o| seen.insert(o.to_lowercase())) {
        return Err(PollIssue::DuplicateOption);
    }
    if options.len() < MIN_OPTIONS {
        return Err(PollIssue::TooFewOptions);
    }
    if options.len() > MAX_OPTIONS {
        return Err(PollIssue::TooManyOptions);
    }
    Ok(PollDraft { question: question.to_string(), options })
}

/// Keeps the option fields tidy: at least two, one empty field after the last
/// filled one (until the maximum), never two empty ones at the end.
pub fn normalize_fields(mut fields: Vec<String>) -> Vec<String> {
    while fields.len() < MIN_OPTIONS {
        fields.push(String::new());
    }
    while fields.len() > MIN_OPTIONS
        && fields.iter().rev().take(2).all(|f| f.trim().is_empty())
    {
        fields.pop();
    }
    if fields.len() < MAX_OPTIONS && fields.last().is_some_and(|f| !f.trim().is_empty()) {
        fields.push(String::new());
    }
    fields
}

/// The option names to vote for after a click on option `clicked`, given the
/// poll as shown. Single-answer polls: the clicked option, or none when it is
/// already the choice (withdraw). Multi-answer polls: the clicked option is
/// toggled in the current set. `None` when `clicked` is out of range.
pub fn next_selection(multi: bool, options: &[PollOptionView], clicked: usize) -> Option<Vec<String>> {
    let target = options.get(clicked)?;
    if !multi {
        return Some(if target.mine { Vec::new() } else { vec![target.name.clone()] });
    }
    Some(
        options
            .iter()
            .enumerate()
            .filter(|(i, o)| if *i == clicked { !o.mine } else { o.mine })
            .map(|(_, o)| o.name.clone())
            .collect(),
    )
}

/// Shows `selection` as the viewer's own vote in the cached poll: my flags,
/// vote counts, total and percentages. The engine's refresh replaces it with
/// the real tally; until then the next click is computed from this state.
pub fn apply_selection(poll: &mut PollViewModel, selection: &[String]) {
    for o in &mut poll.options {
        let now = selection.iter().any(|s| *s == o.name);
        match (now, o.mine) {
            (true, false) => o.votes = o.votes.saturating_add(1),
            (false, true) => o.votes = o.votes.saturating_sub(1),
            _ => {}
        }
        o.mine = now;
    }
    let total: u64 = poll.options.iter().map(|o| u64::from(o.votes)).sum();
    for o in &mut poll.options {
        o.percent = if total == 0 { 0 } else { u32::try_from((u64::from(o.votes) * 100 + total / 2) / total).map_or(100, |p| p.min(100)) };
    }
    poll.total_votes = u32::try_from(total).unwrap_or(u32::MAX);
}

#[derive(Default)]
struct Draft {
    /// Chat the poll goes to (the chat that was open when the dialog opened).
    chat: Option<String>,
    /// The profile the dialog was opened for; a send from another one is refused.
    profile: String,
}

thread_local! {
    static DRAFT: RefCell<Draft> = RefCell::new(Draft::default());
    static FIELDS: Rc<VecModel<PollField>> = Rc::new(VecModel::default());
}

fn field_texts() -> Vec<String> {
    FIELDS.with(|m| m.iter().map(|f| f.text.to_string()).collect())
}

/// Replaces the fields in place where the count is unchanged, so rows that are
/// being typed in are not rebuilt.
fn set_fields(texts: Vec<String>) {
    FIELDS.with(|m| {
        if m.row_count() == texts.len() {
            for (i, t) in texts.into_iter().enumerate() {
                if m.row_data(i).is_none_or(|f| f.text != t.as_str()) {
                    m.set_row_data(i, PollField { text: t.into() });
                }
            }
        } else {
            m.set_vec(texts.into_iter().map(|t| PollField { text: t.into() }).collect::<Vec<_>>());
        }
    });
}

pub(super) fn wire(win: &MainWindow) {
    FIELDS.with(|m| win.global::<PollState>().set_fields(ModelRc::from(m.clone())));
    win.global::<PollActions>().on_vote(|id, index| with_app(|a| a.poll_vote(id.as_str(), index)));
    win.global::<AttachActions>().on_poll(|| with_app(App::open_create_poll));
    let s = win.global::<PollState>();
    s.on_question_edited(|_| with_app(App::poll_publish));
    s.on_option_edited(|i, t| with_app(|a| a.poll_option_edited(i, t.as_str())));
    s.on_remove_option(|i| with_app(|a| a.poll_remove_option(i)));
    s.on_send(|| with_app(App::poll_send));
    s.on_cancel(|| with_app(App::poll_close));
}

impl App {
    /// A click on an option of a poll bubble.
    fn poll_vote(&self, poll_id: &str, index: i32) {
        let Ok(index) = usize::try_from(index) else { return };
        let (chat, selection) = {
            let p = self.p();
            let st = p.st.borrow();
            let Some(chat) = st.selected.clone() else { return };
            let Some(poll) = st.polls.get(poll_id) else { return };
            let Some(selection) = next_selection(poll.multi, &poll.options, index) else { return };
            (chat, selection)
        };
        if self.refuse_blocked(&chat) {
            return;
        }
        if let Some(poll) = self.p().st.borrow_mut().polls.get_mut(poll_id) {
            apply_selection(poll, &selection);
        }
        log::info!("poll: vote with {} option(s)", selection.len());
        self.send(Command::VotePoll { chat_jid: chat, poll_id: poll_id.to_string(), options: selection });
    }

    /// Opens the dialog for the open chat. The attach menu calls this.
    pub(super) fn open_create_poll(&self) {
        let Some(win) = self.win() else { return };
        let Some(chat) = self.p().st.borrow().selected.clone() else { return };
        if self.refuse_blocked(&chat) || self.refuse_gated() {
            return;
        }
        let profile = self.p().id.clone();
        DRAFT.with(|d| *d.borrow_mut() = Draft { chat: Some(chat), profile });
        let s = win.global::<PollState>();
        s.set_question(Default::default());
        s.set_multi(true);
        set_fields(normalize_fields(Vec::new()));
        self.poll_publish();
        s.set_open(true);
    }

    pub(super) fn poll_close(&self) {
        DRAFT.with(|d| *d.borrow_mut() = Draft::default());
        if let Some(win) = self.win() {
            win.global::<PollState>().set_open(false);
            win.invoke_focus_composer();
        }
    }

    /// The open chat changed (or the profile): a draft for the old chat goes.
    pub(super) fn poll_chat_changed(&self) {
        if DRAFT.with(|d| d.borrow().chat.is_some()) {
            self.poll_close();
        }
    }

    /// Recomputes whether Send is allowed and the hint under the options.
    fn poll_publish(&self) {
        let Some(win) = self.win() else { return };
        let s = win.global::<PollState>();
        let checked = validate_poll(s.get_question().as_str(), &field_texts());
        s.set_valid(checked.is_ok());
        s.set_hint(checked.err().map_or("", PollIssue::hint).into());
    }

    fn poll_option_edited(&self, index: i32, text: &str) {
        let Ok(index) = usize::try_from(index) else { return };
        let mut texts = field_texts();
        let Some(slot) = texts.get_mut(index) else { return };
        *slot = text.to_string();
        set_fields(normalize_fields(texts));
        self.poll_publish();
    }

    fn poll_remove_option(&self, index: i32) {
        let Ok(index) = usize::try_from(index) else { return };
        let mut texts = field_texts();
        if index >= texts.len() || texts.len() <= MIN_OPTIONS {
            return;
        }
        texts.remove(index);
        set_fields(normalize_fields(texts));
        self.poll_publish();
    }

    fn poll_send(&self) {
        let Some(win) = self.win() else { return };
        let Some((chat, profile)) = DRAFT.with(|d| {
            let d = d.borrow();
            d.chat.clone().map(|c| (c, d.profile.clone()))
        }) else {
            return;
        };
        let s = win.global::<PollState>();
        let Ok(draft) = validate_poll(s.get_question().as_str(), &field_texts()) else { return };
        let still_here = {
            let p = self.p();
            p.id == profile && p.st.borrow().selected.as_deref() == Some(chat.as_str())
        };
        if !still_here {
            self.poll_close();
            return;
        }
        if self.refuse_blocked(&chat) {
            return;
        }
        log::info!("poll: creating a poll with {} options", draft.options.len());
        let multi = s.get_multi();
        self.poll_close();
        self.p().st.borrow_mut().force_bottom = true;
        self.send(Command::CreatePoll { chat_jid: chat, question: draft.question, options: draft.options, multi });
    }

    /// `WACLIENT_DEMO_UI=poll-create`: opens the dialog in Alice's chat with a
    /// question and two options filled in.
    #[cfg(debug_assertions)]
    pub fn demo_poll_create(&self) {
        self.select_chat("111@s.whatsapp.net");
        self.open_create_poll();
        let Some(win) = self.win() else { return };
        win.global::<PollState>().set_question("Movie night?".into());
        set_fields(normalize_fields(vec!["Friday".into(), "Saturday".into()]));
        self.poll_publish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opt(name: &str, mine: bool) -> PollOptionView {
        PollOptionView { name: name.into(), votes: 0, percent: 0, mine }
    }

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn poll(multi: bool, opts: &[(&str, u32, bool)]) -> PollViewModel {
        PollViewModel {
            question: "q".into(),
            options: opts.iter().map(|(n, v, m)| PollOptionView { name: (*n).into(), votes: *v, percent: 0, mine: *m }).collect(),
            total_votes: 0,
            multi,
        }
    }

    #[test]
    fn optimistic_vote_updates_flags_counts_and_percent() {
        let mut p = poll(false, &[("a", 1, true), ("b", 1, false)]);
        apply_selection(&mut p, &strs(&["b"]));
        assert_eq!(p.options.iter().map(|o| (o.votes, o.mine)).collect::<Vec<_>>(), [(0, false), (2, true)]);
        assert_eq!((p.options[0].percent, p.options[1].percent, p.total_votes), (0, 100, 2));
        // A second click now withdraws, because it starts from the updated state.
        assert_eq!(next_selection(p.multi, &p.options, 1), Some(Vec::new()));
        apply_selection(&mut p, &[]);
        // Only the other voter's vote is left.
        assert_eq!((p.total_votes, p.options[1].votes, p.options[1].percent), (1, 1, 100));
        assert!(p.options.iter().all(|o| !o.mine));
    }

    #[test]
    fn optimistic_multi_vote_never_underflows() {
        let mut p = poll(true, &[("a", 0, true), ("b", 3, false)]);
        apply_selection(&mut p, &strs(&["b"]));
        assert_eq!((p.options[0].votes, p.options[1].votes), (0, 4));
        apply_selection(&mut p, &strs(&["a", "b"]));
        assert_eq!((p.options[0].votes, p.options[1].votes, p.total_votes), (1, 4, 5));
    }

    #[test]
    fn single_vote_picks_and_withdraws() {
        let o = [opt("a", false), opt("b", true), opt("c", false)];
        assert_eq!(next_selection(false, &o, 0), Some(strs(&["a"])));
        assert_eq!(next_selection(false, &o, 1), Some(Vec::new()));
        assert_eq!(next_selection(false, &o, 9), None);
    }

    #[test]
    fn multi_vote_toggles_within_the_current_set() {
        let o = [opt("a", true), opt("b", false), opt("c", true)];
        assert_eq!(next_selection(true, &o, 1), Some(strs(&["a", "b", "c"])));
        assert_eq!(next_selection(true, &o, 0), Some(strs(&["c"])));
        let one = [opt("a", true), opt("b", false)];
        assert_eq!(next_selection(true, &one, 0), Some(Vec::new()));
        assert_eq!(next_selection(true, &one, 2), None);
    }

    #[test]
    fn valid_poll_is_trimmed_and_blank_fields_ignored() {
        let d = validate_poll("  Lunch? ", &strs(&[" Pizza ", "Sushi", "", "  "])).unwrap();
        assert_eq!(d.question, "Lunch?");
        assert_eq!(d.options, strs(&["Pizza", "Sushi"]));
    }

    #[test]
    fn question_rules() {
        assert_eq!(validate_poll("  ", &strs(&["a", "b"])), Err(PollIssue::QuestionEmpty));
        let long = "x".repeat(256);
        assert_eq!(validate_poll(&long, &strs(&["a", "b"])), Err(PollIssue::QuestionTooLong));
        assert!(validate_poll(&"x".repeat(255), &strs(&["a", "b"])).is_ok());
        // Characters, not bytes.
        assert!(validate_poll(&"é".repeat(255), &strs(&["a", "b"])).is_ok());
    }

    #[test]
    fn option_rules() {
        assert_eq!(validate_poll("q", &strs(&["a", ""])), Err(PollIssue::TooFewOptions));
        assert_eq!(validate_poll("q", &strs(&["a", "A"])), Err(PollIssue::DuplicateOption));
        assert_eq!(validate_poll("q", &strs(&["a", " a "])), Err(PollIssue::DuplicateOption));
        let twelve: Vec<String> = (0..12).map(|i| format!("o{i}")).collect();
        assert!(validate_poll("q", &twelve).is_ok());
        let thirteen: Vec<String> = (0..13).map(|i| format!("o{i}")).collect();
        assert_eq!(validate_poll("q", &thirteen), Err(PollIssue::TooManyOptions));
    }

    #[test]
    fn fields_start_with_two_and_grow_with_the_last_one() {
        assert_eq!(normalize_fields(Vec::new()), strs(&["", ""]));
        assert_eq!(normalize_fields(strs(&["a", ""])), strs(&["a", ""]));
        assert_eq!(normalize_fields(strs(&["a", "b"])), strs(&["a", "b", ""]));
        assert_eq!(normalize_fields(strs(&["a", "b", "c", ""])), strs(&["a", "b", "c", ""]));
    }

    #[test]
    fn fields_shrink_trailing_blanks_and_cap_at_twelve() {
        assert_eq!(normalize_fields(strs(&["a", "b", "", ""])), strs(&["a", "b", ""]));
        assert_eq!(normalize_fields(strs(&["a", "", "", ""])), strs(&["a", ""]));
        let full: Vec<String> = (0..12).map(|i| format!("o{i}")).collect();
        assert_eq!(normalize_fields(full.clone()), full);
    }
}
