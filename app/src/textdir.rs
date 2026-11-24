//! The one right-to-left rule of the app: Slint's `TextDir.is-rtl`, RTL Home/End
//! (`app::composer::undo`) and per-paragraph bubble direction (`format::paragraphs`)
//! all decide through this module so they cannot drift apart.

/// A character of a right-to-left script (Hebrew, Arabic incl. Persian/Urdu, Syriac,
/// Thaana, N'Ko and their presentation forms).
pub(crate) fn is_rtl_char(c: char) -> bool {
    matches!(c as u32,
        0x0590..=0x08FF | 0xFB1D..=0xFDFF | 0xFE70..=0xFEFF | 0x10800..=0x10FFF | 0x1E800..=0x1EFFF)
        && c.is_alphabetic()
}

/// Direction of the first strong (alphabetic) character; `None` when there is none.
/// Digits, punctuation, emoji and spaces are neutral.
pub(crate) fn strong_direction(s: &str) -> Option<bool> {
    s.chars().find(|c| c.is_alphabetic()).map(is_rtl_char)
}

/// Whether the text starts right to left: its first strong character decides,
/// like Slint's own paragraph direction.
pub(crate) fn text_is_rtl(s: &str) -> bool {
    strong_direction(s).unwrap_or(false)
}

// ---- Visual Left / Right arrows ------------------------------------------------------
//
// Slint's TextInput moves by logical order on Left/Right. In a right-to-left paragraph the
// user expects the cursor to follow the arrow on screen: Left = logically forward, Right =
// logically backward. The direction is that of the paragraph holding the cursor (the same
// first-strong rule as everywhere else), also inside mixed text: simple and predictable,
// at the price of not following the exact visual order of an embedded LTR run.

/// Marks that attach to the previous character: combining marks (incl. Arabic and
/// Hebrew vowel signs), variation selectors, emoji skin tones, tags, ZWJ, keycap.
fn is_extend(c: char) -> bool {
    matches!(c as u32,
        0x0300..=0x036F | 0x0483..=0x0489 | 0x0591..=0x05BD | 0x05BF | 0x05C1..=0x05C2
        | 0x05C4..=0x05C5 | 0x05C7 | 0x0610..=0x061A | 0x064B..=0x065F | 0x0670
        | 0x06D6..=0x06DC | 0x06DF..=0x06E4 | 0x06E7..=0x06E8 | 0x06EA..=0x06ED
        | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x200D | 0x20D0..=0x20FF | 0xFE00..=0xFE0F
        | 0xFE20..=0xFE2F | 0x1F3FB..=0x1F3FF | 0xE0020..=0xE007F | 0xE0100..=0xE01EF)
}

/// Emoji and symbols that can be joined into one cluster by a ZWJ.
fn is_pictographic(c: char) -> bool {
    matches!(c as u32,
        0x2190..=0x21FF | 0x2300..=0x23FF | 0x2600..=0x27BF | 0x2B00..=0x2BFF
        | 0x1F000..=0x1FAFF)
}

fn is_regional_indicator(c: char) -> bool {
    matches!(c as u32, 0x1F1E6..=0x1F1FF)
}

fn floor_boundary(text: &str, mut at: usize) -> usize {
    at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// End of the grapheme cluster starting at `at` (`at` itself at the end of the text).
fn grapheme_end(text: &str, at: usize) -> usize {
    let mut chars = text[at..].chars();
    let Some(first) = chars.next() else { return at };
    let mut end = at + first.len_utf8();
    let mut prev = first;
    let mut regional = usize::from(is_regional_indicator(first));
    for c in chars {
        // A ZWJ glues an emoji to the next emoji only; between letters it is just a
        // joiner attached to the previous one.
        let after_zwj = prev == '\u{200D}' && is_pictographic(c);
        let flag_pair = is_regional_indicator(c) && is_regional_indicator(prev) && regional % 2 == 1;
        if !(is_extend(c) || after_zwj || flag_pair) {
            break;
        }
        regional += usize::from(is_regional_indicator(c));
        end += c.len_utf8();
        prev = c;
    }
    end
}

/// Start of the grapheme cluster that ends at (or contains) `at`, one step back.
fn grapheme_start_before(text: &str, at: usize) -> usize {
    if at == 0 {
        return 0;
    }
    let line = text[..at].rfind('\n').map_or(0, |i| i + 1);
    if line == at {
        return at - 1; // over the newline into the previous paragraph
    }
    let mut pos = line;
    loop {
        let end = grapheme_end(text, pos);
        if end >= at {
            return pos;
        }
        pos = end;
    }
}

/// 0 whitespace, 1 word, 2 punctuation / symbols / emoji.
fn word_class(c: char) -> u8 {
    if c.is_whitespace() {
        0
    } else if c.is_alphanumeric() || c == '_' || is_extend(c) {
        1
    } else {
        2
    }
}

fn word_forward(text: &str, at: usize) -> usize {
    let mut pos = at;
    if let Some(c) = text[pos..].chars().next().filter(|c| word_class(*c) != 0) {
        let class = word_class(c);
        while text[pos..].chars().next().is_some_and(|c| word_class(c) == class) {
            pos = grapheme_end(text, pos);
        }
    }
    while text[pos..].chars().next().is_some_and(|c| word_class(c) == 0) {
        pos = grapheme_end(text, pos);
    }
    pos
}

fn word_backward(text: &str, at: usize) -> usize {
    let mut pos = at;
    while text[..pos].chars().next_back().is_some_and(|c| word_class(c) == 0) {
        pos = grapheme_start_before(text, pos);
    }
    if let Some(class) = text[..pos].chars().next_back().map(word_class) {
        while text[..pos].chars().next_back().is_some_and(|c| word_class(c) == class) {
            pos = grapheme_start_before(text, pos);
        }
    }
    pos
}

/// Where the cursor goes on the Left (`left`) or Right arrow (`word`: with Ctrl) when it is
/// inside a right-to-left paragraph; `None` when the paragraph is left to right and Slint's
/// own movement is already right. Offsets are UTF-8 bytes; steps are whole graphemes.
pub(crate) fn visual_step(text: &str, cursor: usize, left: bool, word: bool) -> Option<usize> {
    let cursor = floor_boundary(text, cursor);
    let start = text[..cursor].rfind('\n').map_or(0, |i| i + 1);
    let end = text[cursor..].find('\n').map_or(text.len(), |i| cursor + i);
    if !text_is_rtl(&text[start..end]) {
        return None;
    }
    // Visual left is logically forward in right-to-left text.
    let forward = left;
    Some(match (forward, word) {
        (true, false) if cursor < text.len() => grapheme_end(text, cursor),
        (true, true) => word_forward(text, cursor),
        (false, false) => grapheme_start_before(text, cursor),
        (false, true) => word_backward(text, cursor),
        (true, false) => cursor,
    })
}

/// [`visual_step`] with the selection: returns the new `(anchor, cursor)`. `shift` keeps the
/// anchor (extends the selection); otherwise a plain arrow over a selection collapses it to
/// the edge on the arrow's visual side, like a native text box.
pub(crate) fn arrow_nav(
    text: &str,
    anchor: usize,
    cursor: usize,
    left: bool,
    word: bool,
    shift: bool,
) -> Option<(usize, usize)> {
    let cursor = floor_boundary(text, cursor);
    let anchor = floor_boundary(text, anchor);
    if !shift && !word && anchor != cursor {
        visual_step(text, cursor, left, false)?;
        let edge = if left { anchor.max(cursor) } else { anchor.min(cursor) };
        return Some((edge, edge));
    }
    let to = visual_step(text, cursor, left, word)?;
    Some((if shift { anchor } else { to }, to))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtl_arrows_move_visually() {
        let t = "سلام"; // 4 letters, 2 bytes each
        assert_eq!(visual_step(t, 0, true, false), Some(2)); // Left = forward
        assert_eq!(visual_step(t, 4, false, false), Some(2)); // Right = backward
        assert_eq!(visual_step(t, 0, false, false), Some(0));
        assert_eq!(visual_step(t, 8, true, false), Some(8));
        for s in ["مرحبا", "שלום"] {
            assert_eq!(visual_step(s, 0, true, false), Some(2), "{s}");
            assert_eq!(visual_step(s, 2, false, false), Some(0), "{s}");
        }
    }

    #[test]
    fn ltr_paragraphs_are_left_to_slint() {
        assert_eq!(visual_step("hello", 2, true, false), None);
        assert_eq!(visual_step("hello سلام", 8, false, false), None);
        assert_eq!(visual_step("", 0, true, false), None);
        assert_eq!(arrow_nav("hello", 2, 2, false, true, true), None);
    }

    #[test]
    fn mixed_text_uses_the_paragraph_direction() {
        let t = "سلام hello"; // RTL paragraph
        assert_eq!(visual_step(t, 9, true, false), Some(10)); // inside the Latin run: still forward
        assert_eq!(visual_step(t, 9, false, false), Some(8));
        // Each paragraph decides on its own.
        let two = "سلام\nhello";
        assert_eq!(visual_step(two, 2, true, false), Some(4));
        assert_eq!(visual_step(two, 12, true, false), None);
    }

    #[test]
    fn emoji_and_marks_step_as_whole_graphemes() {
        let family = "👨\u{200D}👩\u{200D}👧"; // ZWJ sequence
        let t = format!("س{family}ب");
        let after_family = 2 + family.len();
        assert_eq!(visual_step(&t, 2, true, false), Some(after_family));
        assert_eq!(visual_step(&t, after_family, false, false), Some(2));
        let flag = "🇮🇷🇮🇷"; // two flags, two clusters
        let t = format!("س{flag}");
        assert_eq!(visual_step(&t, 2, true, false), Some(2 + 8));
        let thumbs = "👍🏽";
        let t = format!("{thumbs}س"); // first strong is س
        assert_eq!(visual_step(&t, 0, true, false), Some(thumbs.len()));
        // Vowel sign (fatha) stays with its letter; variation selector with its symbol.
        let t = "بَس";
        assert_eq!(visual_step(t, 0, true, false), Some(4));
        assert_eq!(visual_step(t, 4, false, false), Some(0));
        let t = "س❤\u{FE0F}";
        assert_eq!(visual_step(t, 2, true, false), Some(t.len()));
    }

    #[test]
    fn zwj_between_letters_does_not_swallow_the_next_letter() {
        let t = "ب\u{200D}ب"; // 2 + 3 + 2 bytes
        assert_eq!(visual_step(t, 0, true, false), Some(5)); // first letter keeps its ZWJ
        assert_eq!(visual_step(t, 5, true, false), Some(7));
        assert_eq!(visual_step(t, 7, false, false), Some(5));
        assert_eq!(visual_step(t, 5, false, false), Some(0));
    }

    #[test]
    fn newline_is_crossed_one_step() {
        let t = "سلام\nسلام";
        assert_eq!(visual_step(t, 8, true, false), Some(9)); // end of line 1, Left = forward
        assert_eq!(visual_step(t, 9, false, false), Some(8));
    }

    #[test]
    fn word_steps_follow_the_visual_direction() {
        let t = "سلام دنیا خوب"; // words at 0, 9, 19 (bytes: 8+1, 8+1, 8)
        assert_eq!(visual_step(t, 0, true, true), Some(9));
        assert_eq!(visual_step(t, 9, true, true), Some(18));
        assert_eq!(visual_step(t, 18, true, true), Some(t.len()));
        assert_eq!(visual_step(t, t.len(), false, true), Some(18));
        assert_eq!(visual_step(t, 18, false, true), Some(9));
        assert_eq!(visual_step(t, 9, false, true), Some(0));
        assert_eq!(visual_step(t, 0, false, true), Some(0));
        assert_eq!(visual_step("سلام!! x", 8, true, true), Some(11));
    }

    #[test]
    fn selection_is_extended_or_collapsed() {
        let t = "سلام";
        assert_eq!(arrow_nav(t, 2, 2, true, false, false), Some((4, 4)));
        assert_eq!(arrow_nav(t, 2, 2, true, false, true), Some((2, 4)));
        assert_eq!(arrow_nav(t, 2, 6, false, false, true), Some((2, 4)));
        assert_eq!(arrow_nav(t, 2, 2, false, true, true), Some((2, 0)));
        // A plain arrow over a selection collapses to the visual side: Left = logical max.
        assert_eq!(arrow_nav(t, 2, 6, true, false, false), Some((6, 6)));
        assert_eq!(arrow_nav(t, 2, 6, false, false, false), Some((2, 2)));
        assert_eq!(arrow_nav(t, 6, 2, true, false, false), Some((6, 6)));
        assert_eq!(arrow_nav("abc", 0, 2, true, false, false), None);
    }

    #[test]
    fn text_direction_follows_the_first_strong_character() {
        for rtl in ["سلام", "مرحبا بالعالم", "שלום", "اردو زبان", "123 😀 سلام", "... @98912 سلام", "سلام hello"] {
            assert!(text_is_rtl(rtl), "{rtl}");
        }
        for ltr in ["hello", "hello سلام", "1) 😀 hello", "", "   ", "123 !?", "😀"] {
            assert!(!text_is_rtl(ltr), "{ltr}");
        }
    }

    #[test]
    fn rtl_scripts_are_recognised() {
        for c in ['א', 'ب', 'پ', 'ڈ', 'ܐ', 'ހ', 'ߊ', 'ﭐ', 'ﻻ'] {
            assert!(is_rtl_char(c), "{c}");
        }
        for c in ['a', 'я', '1', '😀', '!', ' '] {
            assert!(!is_rtl_char(c), "{c}");
        }
    }

    #[test]
    fn strong_direction_is_none_without_letters() {
        assert_eq!(strong_direction("12 😀"), None);
        assert_eq!(strong_direction("1 سلام"), Some(true));
        assert_eq!(strong_direction("1 a سلام"), Some(false));
    }
}
