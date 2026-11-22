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

#[cfg(test)]
mod tests {
    use super::*;

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
