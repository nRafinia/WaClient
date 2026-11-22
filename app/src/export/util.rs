//! Pure helpers of the chat export: escaping, link and file-name safety, text
//! lines and date ranges. No I/O, so everything here is unit tested.

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, TimeZone};

/// Escapes text for HTML element content and attribute values.
pub fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// The link target if it is safe to put in an `href`: only `http://`,
/// `https://` and `mailto:` with something after the scheme, and no spaces,
/// control characters, quotes or angle brackets.
pub fn safe_href(s: &str) -> Option<&str> {
    let t = s.trim();
    let scheme = ["http://", "https://", "mailto:"]
        .iter()
        .find(|p| t.get(..p.len()).is_some_and(|head| head.eq_ignore_ascii_case(p)))?;
    if t.len() == scheme.len() {
        return None;
    }
    let bad = |c: char| c.is_control() || c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\'' | '`' | '\\');
    if t.chars().any(bad) {
        return None;
    }
    Some(t)
}

/// Escapes `text` and turns the words that are safe links into anchors; every
/// other word, including `javascript:` and `data:` links, stays plain text.
pub fn linkify(text: &str) -> String {
    fn flush(word: &mut String, out: &mut String) {
        if word.is_empty() {
            return;
        }
        // Closing punctuation belongs to the sentence, not to the link.
        // A ")" stays when the link has a matching "(" (Wikipedia-style URLs).
        let mut end = word.len();
        while let Some(c) = word[..end].chars().next_back() {
            let head = &word[..end];
            let strip = match c {
                '.' | ',' | ';' | ':' | '!' | '?' | ']' | '}' => true,
                ')' => head.matches(')').count() > head.matches('(').count(),
                _ => false,
            };
            if !strip {
                break;
            }
            end -= c.len_utf8();
        }
        let (core, tail) = word.split_at(end);
        match safe_href(core) {
            Some(href) => {
                out.push_str("<a href=\"");
                out.push_str(&html_escape(href));
                out.push_str("\" rel=\"noopener noreferrer\">");
                out.push_str(&html_escape(core));
                out.push_str("</a>");
                out.push_str(&html_escape(tail));
            }
            None => out.push_str(&html_escape(word)),
        }
        word.clear();
    }
    let mut out = String::with_capacity(text.len() + 16);
    let mut word = String::new();
    for c in text.chars() {
        if c.is_whitespace() {
            flush(&mut word, &mut out);
            out.push(c);
        } else {
            word.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

/// Percent-encodes a relative file path for a `src` / `href` attribute.
pub fn url_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for b in path.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A valid Windows file or folder name made from `name`: no separators,
/// wildcards or control characters, no trailing dots or spaces, no reserved
/// device names, at most `max_chars` characters. Never empty (`fallback`).
pub fn sanitize_name(name: &str, max_chars: usize, fallback: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_control() || "\\/:*?\"<>|".contains(c) { ' ' } else { c })
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let collapsed = collapsed.trim_matches(['.', ' ']);
    let (stem, ext) = match collapsed.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() && !e.is_empty() && e.chars().count() <= 10 && !e.contains(' ') => {
            (s.to_string(), format!(".{e}"))
        }
        _ => (collapsed.to_string(), String::new()),
    };
    let room = max_chars.saturating_sub(ext.chars().count()).max(1);
    let stem: String = stem.chars().take(room).collect();
    let stem = stem.trim_end_matches(['.', ' ']);
    if stem.is_empty() {
        return format!("{fallback}{ext}");
    }
    let device = stem.split('.').next().unwrap_or(stem).trim_end().to_ascii_uppercase();
    let reserved = matches!(device.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"]
            .iter()
            .any(|p| device.strip_prefix(p).is_some_and(|n| matches!(n.as_bytes(), [b'1'..=b'9'])));
    if reserved { format!("_{stem}{ext}") } else { format!("{stem}{ext}") }
}

/// Name of the export folder: "<chat name> <YYYY-MM-DD>".
pub fn folder_name(chat_name: &str, date: NaiveDate) -> String {
    format!("{} {}", sanitize_name(chat_name, 60, "Chat"), date.format("%Y-%m-%d"))
}

/// Name a copied media file gets: a running number keeps names unique and in
/// chat order. The message's own file name wins when it has an extension,
/// otherwise the cached file's name (which carries the real one).
pub fn media_file_name(n: usize, media_name: Option<&str>, cached_name: &str) -> String {
    let own = media_name.map(|m| sanitize_name(m, 100, "file")).filter(|m| m.contains('.'));
    let base = own.unwrap_or_else(|| sanitize_name(cached_name, 100, "file"));
    format!("{n:05}-{base}")
}

/// One line of the text export: "DD/MM/YYYY, HH:MM - Name: text". A message
/// without sender (a notice) has no "Name:". Further lines of `text` follow
/// unchanged, as WhatsApp writes them.
pub fn text_line(when: NaiveDateTime, sender: Option<&str>, text: &str) -> String {
    let stamp = when.format("%d/%m/%Y, %H:%M");
    match sender {
        Some(name) => format!("{stamp} - {name}: {text}"),
        None => format!("{stamp} - {text}"),
    }
}

/// Which messages to export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    All,
    /// The last 30 days including today.
    Last30Days,
    /// Whole days, both ends included.
    Custom { from: NaiveDate, to: NaiveDate },
}

/// The range as `(from, to)` Unix seconds, `from` included and `to` excluded,
/// in the time zone `tz`. A custom range that ends before it starts is an
/// error.
pub fn range_bounds<Tz: TimeZone>(
    range: Range,
    today: NaiveDate,
    tz: &Tz,
) -> Result<(Option<i64>, Option<i64>), String> {
    let start = |d: NaiveDate| -> Result<i64, String> {
        let midnight = d.and_hms_opt(0, 0, 0).ok_or("invalid date")?;
        tz.from_local_datetime(&midnight)
            .earliest()
            .map(|t| t.timestamp())
            .ok_or_else(|| "invalid date".to_string())
    };
    let next_day = |d: NaiveDate| d.checked_add_signed(Duration::days(1)).ok_or_else(|| "invalid date".to_string());
    match range {
        Range::All => Ok((None, None)),
        Range::Last30Days => {
            let from = today.checked_sub_signed(Duration::days(29)).ok_or("invalid date")?;
            Ok((Some(start(from)?), Some(start(next_day(today)?)?)))
        }
        Range::Custom { from, to } => {
            if from > to {
                return Err("The start date is after the end date".to_string());
            }
            Ok((Some(start(from)?), Some(start(next_day(to)?)?)))
        }
    }
}

/// Parses "YYYY-MM-DD" (also with `/` or `.` separators).
pub fn parse_date(s: &str) -> Option<NaiveDate> {
    let norm: String = s.trim().chars().map(|c| if matches!(c, '/' | '.') { '-' } else { c }).collect();
    let mut parts = norm.split('-');
    let (y, m, d) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || y.len() != 4 {
        return None;
    }
    NaiveDate::from_ymd_opt(y.parse().ok()?, m.parse().ok()?, d.parse().ok()?)
}

/// "22 November 2026" for the day separators.
pub fn day_title(d: NaiveDate) -> String {
    format!("{} {} {}", d.day(), d.format("%B"), d.year())
}

/// "1,240": thousands separators for the progress text.
pub fn group_digits(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("valid date")
    }

    #[test]
    fn escaping_covers_markup_and_quotes() {
        assert_eq!(
            html_escape("<script>alert('x')</script> & \"q\""),
            "&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt; &amp; &quot;q&quot;"
        );
        assert_eq!(html_escape("سلام"), "سلام");
    }

    #[test]
    fn only_web_and_mail_links_are_safe() {
        assert_eq!(safe_href("https://example.com/a?b=1&c=2"), Some("https://example.com/a?b=1&c=2"));
        assert_eq!(safe_href("HTTP://EXAMPLE.COM"), Some("HTTP://EXAMPLE.COM"));
        assert_eq!(safe_href("mailto:a@b.co"), Some("mailto:a@b.co"));
        for bad in [
            "javascript:alert(1)",
            "data:text/html,x",
            "file:///c:/x",
            "ftp://x",
            "http://",
            "https://a b",
            "https://a\"onclick=\"x",
            "//evil.com",
            "vbscript:x",
            " java\nscript:x",
        ] {
            assert_eq!(safe_href(bad), None, "{bad}");
        }
    }

    #[test]
    fn linkify_escapes_everything_and_links_only_safe_words() {
        let out = linkify("see <b>https://a.example/x?y=1&z=2</b>, or https://ok.example/path. javascript:alert(1)");
        assert!(!out.contains("<b>"));
        assert!(out.contains("&lt;b&gt;https://a.example/x?y=1&amp;z=2&lt;/b&gt;,"));
        assert!(out.contains(
            "<a href=\"https://ok.example/path\" rel=\"noopener noreferrer\">https://ok.example/path</a>."
        ));
        assert!(!out.contains("href=\"javascript"));
        assert!(out.ends_with("javascript:alert(1)"));
        let wiki = linkify("see https://en.wikipedia.org/wiki/Rust_(language). Or https://a.example/x)");
        assert!(wiki.contains("href=\"https://en.wikipedia.org/wiki/Rust_(language)\""));
        assert!(wiki.contains("href=\"https://a.example/x\""));
        assert_eq!(linkify("<script>alert(1)</script>"), "&lt;script&gt;alert(1)&lt;/script&gt;");
    }

    #[test]
    fn file_paths_are_percent_encoded() {
        assert_eq!(url_path("media/00001-a b&c#d.jpg"), "media/00001-a%20b%26c%23d.jpg");
        assert_eq!(url_path("media/ü.png"), "media/%C3%BC.png");
    }

    #[test]
    fn names_are_valid_on_windows() {
        assert_eq!(sanitize_name("a/b:c*d?.pdf", 100, "file"), "a b c d.pdf");
        assert_eq!(sanitize_name("CON", 100, "file"), "_CON");
        assert_eq!(sanitize_name("com1.txt", 100, "file"), "_com1.txt");
        assert_eq!(sanitize_name("COM0", 100, "file"), "COM0");
        assert_eq!(sanitize_name("  ..  ", 100, "Chat"), "Chat");
        assert_eq!(sanitize_name("report. . ", 100, "file"), "report");
        assert_eq!(sanitize_name("", 100, "Chat"), "Chat");
        let long = sanitize_name(&"x".repeat(500), 60, "Chat");
        assert_eq!(long.chars().count(), 60);
        let with_ext = sanitize_name(&format!("{}.jpeg", "y".repeat(300)), 40, "file");
        assert_eq!(with_ext.chars().count(), 40);
        assert!(with_ext.ends_with(".jpeg"));
        assert_eq!(sanitize_name("Family \u{1F46A} chat", 60, "Chat"), "Family \u{1F46A} chat");
    }

    #[test]
    fn folder_and_media_names() {
        assert_eq!(folder_name("Ann/Bob", date(2026, 3, 7)), "Ann Bob 2026-03-07");
        assert_eq!(media_file_name(7, Some("scan.pdf"), "abc123"), "00007-scan.pdf");
        assert_eq!(media_file_name(8, Some("noext"), "abc123.jpg"), "00008-abc123.jpg");
        assert_eq!(media_file_name(9, None, "../../evil.exe"), "00009-evil.exe");
    }

    #[test]
    fn text_lines_follow_the_whatsapp_layout() {
        let when = date(2026, 3, 7).and_hms_opt(9, 5, 0).expect("valid time");
        assert_eq!(text_line(when, Some("Ann"), "hi"), "07/03/2026, 09:05 - Ann: hi");
        assert_eq!(text_line(when, Some("Ann"), "a\nb"), "07/03/2026, 09:05 - Ann: a\nb");
        assert_eq!(text_line(when, None, "Ann joined"), "07/03/2026, 09:05 - Ann joined");
    }

    #[test]
    fn range_bounds_are_whole_local_days() {
        let tz = FixedOffset::east_opt(2 * 3600).expect("offset");
        let today = date(2026, 3, 10);
        assert_eq!(range_bounds(Range::All, today, &tz), Ok((None, None)));
        let midnight = |d: NaiveDate| {
            tz.from_local_datetime(&d.and_hms_opt(0, 0, 0).expect("time")).earliest().expect("local time").timestamp()
        };
        let (from, to) = range_bounds(Range::Last30Days, today, &tz).expect("range");
        assert_eq!(from, Some(midnight(date(2026, 2, 9))));
        assert_eq!(to, Some(midnight(date(2026, 3, 11))));
        let custom = Range::Custom { from: date(2026, 1, 31), to: date(2026, 1, 31) };
        let (from, to) = range_bounds(custom, today, &tz).expect("range");
        assert_eq!(to.zip(from).map(|(t, f)| t - f), Some(86_400));
        let backwards = Range::Custom { from: date(2026, 2, 2), to: date(2026, 2, 1) };
        assert!(range_bounds(backwards, today, &tz).is_err());
    }

    #[test]
    fn dates_parse_strictly() {
        assert_eq!(parse_date("2026-03-07"), Some(date(2026, 3, 7)));
        assert_eq!(parse_date(" 2026/3/7 "), Some(date(2026, 3, 7)));
        assert_eq!(parse_date("2026.03.07"), Some(date(2026, 3, 7)));
        for bad in ["", "2026-13-01", "2026-02-30", "26-01-01", "2026-01", "2026-01-01-01", "abc"] {
            assert_eq!(parse_date(bad), None, "{bad}");
        }
    }

    #[test]
    fn numbers_and_days_read_well() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(1_240), "1,240");
        assert_eq!(group_digits(1_234_567), "1,234,567");
        assert_eq!(day_title(date(2026, 11, 2)), "2 November 2026");
    }
}
