//! Composer undo/redo and RTL Home/End, as pure logic.
//!
//! Slint's TextInput has its own undo stack, but it is wiped every time the
//! application assigns `text` (formatting, @mention pick, paste of a file name
//! or image caption, emoji, translation), so it cannot give one reliable
//! Ctrl+Z. The composer therefore keeps its own snapshot stack: every change
//! (typed or programmatic) is reported to `UndoStack::note` and Ctrl+Z / Ctrl+Y
//! walk the snapshots.
//!
//! Offsets are UTF-8 byte offsets, like Slint's cursor positions.

/// Oldest steps are dropped beyond this many.
const CAP: usize = 100;

/// The composer text and the cursor after an edit.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Snap {
    pub text: String,
    pub cursor: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Last {
    /// Nothing that can be extended (start, programmatic edit, undo/redo, bulk edit).
    Other,
    /// A run of single typed characters; `ws` says the last one was whitespace,
    /// which closes the word-sized step.
    Insert { ws: bool },
    /// A run of single deleted characters.
    Delete,
}

#[derive(Debug)]
pub struct UndoStack {
    undo: Vec<Snap>,
    redo: Vec<Snap>,
    cur: Snap,
    last: Last,
}

impl Default for UndoStack {
    fn default() -> Self {
        Self { undo: Vec::new(), redo: Vec::new(), cur: Snap::default(), last: Last::Other }
    }
}

/// What changed between two texts: the byte range of `old` that was replaced
/// and the text that replaced it.
struct Change {
    start: usize,
    removed_end: usize,
    inserted: String,
}

fn diff(old: &str, new: &str) -> Change {
    let mut start = 0;
    for ((i, a), b) in old.char_indices().zip(new.chars()) {
        if a != b {
            break;
        }
        start = i + a.len_utf8();
    }
    let (o, n) = (&old[start..], &new[start..]);
    let mut tail = 0;
    for (a, b) in o.chars().rev().zip(n.chars().rev()) {
        if a != b {
            break;
        }
        tail += a.len_utf8();
    }
    Change { start, removed_end: old.len() - tail, inserted: new[start..new.len() - tail].to_string() }
}

impl UndoStack {
    /// Forgets the history; `text` is the new starting point (chat switch, send).
    pub fn reset(&mut self, text: &str, cursor: usize) {
        self.undo.clear();
        self.redo.clear();
        self.cur = Snap { text: text.to_string(), cursor: cursor.min(text.len()) };
        self.last = Last::Other;
    }

    #[cfg(test)]
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    #[cfg(test)]
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// The composer now holds `text` with the cursor at `cursor`. `atomic`
    /// edits (formatting, mention, paste, emoji, translation) are always one
    /// step; typed edits extend the previous step while they continue the same
    /// word (typing) or run (deleting).
    pub fn note(&mut self, text: &str, cursor: usize, atomic: bool) {
        let cursor = cursor.min(text.len());
        if text == self.cur.text {
            self.cur.cursor = cursor;
            return;
        }
        let ch = diff(&self.cur.text, text);
        let single = ch.inserted.chars().count() == 1;
        let kind = if atomic {
            Last::Other
        } else if ch.start == ch.removed_end && single {
            Last::Insert { ws: ch.inserted.chars().all(char::is_whitespace) }
        } else if ch.inserted.is_empty() && self.cur.text[ch.start..ch.removed_end].chars().count() == 1 {
            Last::Delete
        } else {
            Last::Other
        };
        let extends = match (self.last, kind) {
            // Typing continues the step at the old cursor, until a space ends the word.
            (Last::Insert { ws: false }, Last::Insert { .. }) => ch.start == self.cur.cursor,
            // Backspace or Delete keep eating at the same place.
            (Last::Delete, Last::Delete) => ch.start == self.cur.cursor || ch.removed_end == self.cur.cursor,
            _ => false,
        };
        if !extends {
            self.undo.push(self.cur.clone());
            if self.undo.len() > CAP {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
        self.cur = Snap { text: text.to_string(), cursor };
        self.last = kind;
    }

    /// Ctrl+Z: the snapshot to restore, if there is one.
    pub fn undo(&mut self) -> Option<Snap> {
        let prev = self.undo.pop()?;
        self.redo.push(std::mem::replace(&mut self.cur, prev.clone()));
        self.last = Last::Other;
        Some(prev)
    }

    /// Ctrl+Y / Ctrl+Shift+Z.
    pub fn redo(&mut self) -> Option<Snap> {
        let next = self.redo.pop()?;
        self.undo.push(std::mem::replace(&mut self.cur, next.clone()));
        self.last = Last::Other;
        Some(next)
    }
}

// ---- RTL Home / End ----------------------------------------------------------------

/// Whether the paragraph starts right to left (see [`crate::textdir`]).
pub(crate) fn paragraph_is_rtl(par: &str) -> bool {
    crate::textdir::text_is_rtl(par)
}

fn floor_boundary(text: &str, mut at: usize) -> usize {
    at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// Left / Right arrows (Ctrl: by word) in a right-to-left paragraph, visually
/// (`textdir::arrow_nav`); `None` when Slint's own movement is right.
pub fn rtl_arrow(text: &str, anchor: usize, cursor: usize, left: bool, word: bool, shift: bool) -> Option<(usize, usize)> {
    crate::textdir::arrow_nav(text, anchor, cursor, left, word, shift)
}

/// Home / End inside a right-to-left paragraph. Slint moves to the visual left
/// or right edge, which in RTL text is the logical end or start. Returns the new
/// `(anchor, cursor)` going to the paragraph's logical start (`home`) or end, or
/// `None` when the paragraph is left to right (Slint's own movement is right).
/// With `shift` the anchor stays so the selection extends.
pub fn rtl_home_end(text: &str, anchor: usize, cursor: usize, home: bool, shift: bool) -> Option<(usize, usize)> {
    let cursor = floor_boundary(text, cursor);
    let anchor = floor_boundary(text, anchor);
    let start = text[..cursor].rfind('\n').map_or(0, |i| i + 1);
    let end = text[cursor..].find('\n').map_or(text.len(), |i| cursor + i);
    if !paragraph_is_rtl(&text[start..end]) {
        return None;
    }
    let target = if home { start } else { end };
    Some((if shift { anchor } else { target }, target))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(s: &mut UndoStack, text: &str) {
        s.note(text, text.len(), false);
    }

    #[test]
    fn typing_coalesces_into_words() {
        let mut s = UndoStack::default();
        s.reset("", 0);
        for t in ["h", "he", "hel", "hell", "hello", "hello ", "hello w", "hello wo"] {
            typed(&mut s, t);
        }
        // "hello " closes the first step; "wo" is the second.
        assert_eq!(s.undo().unwrap().text, "hello ");
        assert_eq!(s.undo().unwrap().text, "");
        assert!(s.undo().is_none());
        assert_eq!(s.redo().unwrap().text, "hello ");
        assert_eq!(s.redo().unwrap().text, "hello wo");
        assert!(s.redo().is_none());
    }

    #[test]
    fn deleting_run_is_one_step_and_cut_is_own_step() {
        let mut s = UndoStack::default();
        s.reset("abcdef", 6);
        typed(&mut s, "abcde");
        typed(&mut s, "abcd");
        typed(&mut s, "abc");
        assert_eq!(s.undo().unwrap().text, "abcdef");
        s.reset("abcdef", 6);
        s.note("ab", 2, false); // a selection cut: several chars at once
        s.note("a", 1, false);
        assert_eq!(s.undo().unwrap().text, "ab");
        assert_eq!(s.undo().unwrap().text, "abcdef");
    }

    #[test]
    fn atomic_edit_is_a_single_step_and_never_merges() {
        let mut s = UndoStack::default();
        s.reset("hi", 2);
        typed(&mut s, "hi!");
        s.note("hi *!*", 5, true);
        s.note("hi *!*x", 7, false);
        assert_eq!(s.undo().unwrap().text, "hi *!*");
        let back = s.undo().unwrap();
        assert_eq!(back.text, "hi!");
        assert_eq!(back.cursor, 3);
        assert_eq!(s.undo().unwrap().text, "hi");
    }

    #[test]
    fn new_edit_clears_redo_and_undo_redo_roundtrip() {
        let mut s = UndoStack::default();
        s.reset("", 0);
        s.note("one", 3, true);
        s.note("two", 3, true);
        assert_eq!(s.undo().unwrap().text, "one");
        assert!(s.can_redo());
        s.note("three", 5, true);
        assert!(!s.can_redo());
        assert_eq!(s.undo().unwrap().text, "one");
        assert_eq!(s.redo().unwrap().text, "three");
    }

    #[test]
    fn cursor_only_change_is_not_a_step() {
        let mut s = UndoStack::default();
        s.reset("abc", 3);
        s.note("abc", 0, false);
        assert!(!s.can_undo());
    }

    #[test]
    fn history_is_capped() {
        let mut s = UndoStack::default();
        s.reset("", 0);
        for i in 1..=250 {
            s.note(&"x".repeat(i), i, true);
        }
        let mut n = 0;
        while s.undo().is_some() {
            n += 1;
        }
        assert_eq!(n, CAP);
    }

    #[test]
    fn reset_forgets_everything() {
        let mut s = UndoStack::default();
        s.note("abc", 3, true);
        s.reset("", 0);
        assert!(!s.can_undo() && !s.can_redo());
    }

    #[test]
    fn persian_and_emoji_diff_uses_byte_offsets() {
        let mut s = UndoStack::default();
        s.reset("سلام 😀", "سلام 😀".len());
        let t = "سلام 😀x";
        s.note(t, t.len(), false);
        let back = s.undo().unwrap();
        assert_eq!(back.text, "سلام 😀");
        assert_eq!(back.cursor, "سلام 😀".len());
        // Inserting in the middle of the text.
        s.reset("درود 😀 دنیا", 0);
        s.note("درود 😀 خوب دنیا", 0, true);
        assert_eq!(s.undo().unwrap().text, "درود 😀 دنیا");
    }

    #[test]
    fn rtl_home_end_goes_to_logical_edges() {
        let t = "سلام دنیا";
        let mid = "سلام ".len();
        assert_eq!(rtl_home_end(t, mid, mid, true, false), Some((0, 0)));
        assert_eq!(rtl_home_end(t, mid, mid, false, false), Some((t.len(), t.len())));
    }

    #[test]
    fn rtl_home_end_shift_extends_selection() {
        let t = "سلام دنیا";
        let mid = "سلام ".len();
        assert_eq!(rtl_home_end(t, mid, mid, true, true), Some((mid, 0)));
        assert_eq!(rtl_home_end(t, 2, mid, false, true), Some((2, t.len())));
    }

    #[test]
    fn ltr_and_neutral_lines_are_left_to_slint() {
        assert_eq!(rtl_home_end("hello سلام", 3, 3, true, false), None);
        assert_eq!(rtl_home_end("", 0, 0, true, false), None);
        assert_eq!(rtl_home_end("123 😀", 2, 2, false, false), None);
    }

    #[test]
    fn direction_is_the_first_strong_character() {
        // Leading digits, punctuation and emoji are neutral.
        let t = "1) 😀 سلام hello";
        assert_eq!(rtl_home_end(t, 4, 4, false, false), Some((t.len(), t.len())));
    }

    #[test]
    fn both_direction_paths_agree() {
        let samples = ["سلام", "שלום", "hello", "hello سلام", "سلام hello", "123 😀 سلام", "1) 😀 hello",
            "... @98912 سلام", "اردو زبان", "ܐܒܓ", "ހަނދު", "ߊߋ", "ﭐﻻ", "😀", "123 !?", "x"];
        for s in samples {
            let paras = crate::format::paragraphs(s);
            assert_eq!(paras.len(), 1, "{s}");
            let expected = crate::textdir::strong_direction(s).unwrap_or(false);
            assert_eq!(paragraph_is_rtl(s), expected, "{s}");
            assert_eq!(paras[0].rtl, expected, "{s}");
        }
    }

    #[test]
    fn paragraphs_are_independent_and_offsets_survive_emoji() {
        let t = "hello\nسلام 😀 دنیا\nbye";
        let line2 = "hello\n".len();
        let emoji_end = line2 + "سلام 😀".len();
        let end2 = line2 + "سلام 😀 دنیا".len();
        assert_eq!(rtl_home_end(t, emoji_end, emoji_end, true, false), Some((line2, line2)));
        assert_eq!(rtl_home_end(t, emoji_end, emoji_end, false, false), Some((end2, end2)));
        // The LTR paragraphs around it keep Slint's behaviour.
        assert_eq!(rtl_home_end(t, 2, 2, false, false), None);
        assert_eq!(rtl_home_end(t, t.len(), t.len(), true, false), None);
        // A cursor in the middle of an emoji is clamped to a char boundary.
        let inside = line2 + "سلام ".len() + 1;
        assert_eq!(rtl_home_end(t, inside, inside, true, false), Some((line2, line2)));
    }
}
