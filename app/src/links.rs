//! Link detection for message text, and the markdown that lets Slint's
//! `StyledText` render (and click) the links.
//!
//! Detection is a conservative scanner: `http(s)://`, `www.`, e-mail addresses
//! and phone numbers. Phone numbers become links with the internal
//! [`PHONE_SCHEME`], which [`is_safe_link`] never lets reach the OS. Message text is untrusted, so [`styled_text`] escapes every
//! ASCII punctuation character: the only markup that survives is the link
//! syntax this module writes itself.

use slint::StyledText;

/// Bytes that may appear in a link. Anything else, including all non-ASCII
/// text (Persian punctuation, RTL words), ends it.
fn is_url_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"-._~:/?#[]@!$&'()*+,;=%".contains(&b)
}

fn starts_with_ci(s: &str, prefix: &str) -> bool {
    s.as_bytes().get(..prefix.len()).is_some_and(|h| h.eq_ignore_ascii_case(prefix.as_bytes()))
}

/// Cuts sentence punctuation and unbalanced closing brackets off the end.
/// Linear: brackets are counted once and the counts follow the trimming.
fn trim_link_end(mut s: &str) -> &str {
    let count = |c: char| s.matches(c).count();
    let (round_open, mut round_close) = (count('('), count(')'));
    let (square_open, mut square_close) = (count('['), count(']'));
    loop {
        let Some(last) = s.chars().next_back() else { return s };
        match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '*' | '_' | '~' => {}
            ')' if round_close > round_open => round_close -= 1,
            ']' if square_close > square_open => square_close -= 1,
            _ => return s,
        }
        s = &s[..s.len() - last.len_utf8()];
    }
}

fn host_of(rest: &str) -> &str {
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    &rest[..end]
}

/// Cheap check that `s` starts like a web link; `web_url` needs this to pass,
/// so non-candidates can be skipped before any trimming work.
fn has_web_prefix(s: &str) -> bool {
    let scheme_len = if starts_with_ci(s, "https://") {
        8
    } else if starts_with_ci(s, "http://") {
        7
    } else if starts_with_ci(s, "www.") {
        4
    } else {
        return false;
    };
    s.as_bytes().get(scheme_len).is_some_and(u8::is_ascii_alphanumeric)
}

/// Normalised URL when `s` (starting at a candidate) is a usable web link.
fn web_url(s: &str) -> Option<String> {
    let (scheme_len, add_scheme) = if starts_with_ci(s, "https://") {
        (8, false)
    } else if starts_with_ci(s, "http://") {
        (7, false)
    } else if starts_with_ci(s, "www.") {
        (4, true)
    } else {
        return None;
    };
    let rest = &s[scheme_len..];
    if !rest.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    if add_scheme && !host_of(rest).contains('.') {
        return None;
    }
    Some(if add_scheme { format!("https://{s}") } else { s.to_string() })
}

fn is_local_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"._%+-".contains(&b)
}

/// `(start, end)` of an e-mail address inside `run` (a run of URL bytes).
fn email_in(run: &str) -> Option<(usize, usize)> {
    let b = run.as_bytes();
    let at = run.find('@')?;
    let start = b[..at].iter().rposition(|&c| !is_local_byte(c)).map_or(0, |i| i + 1);
    if start == at {
        return None;
    }
    let dom_len = b[at + 1..].iter().position(|&c| !(c.is_ascii_alphanumeric() || c == b'-' || c == b'.'));
    let mut end = at + 1 + dom_len.unwrap_or(b.len() - at - 1);
    while end > at + 1 && matches!(b[end - 1], b'.' | b'-') {
        end -= 1;
    }
    let domain = &run[at + 1..end];
    let tld = domain.rsplit_once('.')?.1;
    let labels_ok = domain.split('.').all(|l| !l.is_empty() && !l.starts_with('-') && !l.ends_with('-'));
    (labels_ok && tld.len() >= 2 && tld.bytes().all(|c| c.is_ascii_alphabetic())).then_some((start, end))
}

/// Most links detected in one message; the rest stay plain text.
const MAX_LINKS: usize = 500;
/// Bytes of candidate text trimmed per message before detection stops, which
/// bounds the work an adversarial message can cause.
const MAX_TRIM_WORK: usize = 1 << 22;

/// Links in `text` as `(start, end, url)` with byte offsets into `text`.
/// `www.` links get `https://`, e-mail addresses `mailto:`.
pub fn find_links(text: &str) -> Vec<(usize, usize, String)> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut work = MAX_TRIM_WORK;
    let mut i = 0;
    while i < b.len() && out.len() < MAX_LINKS {
        if !is_url_byte(b[i]) {
            i += 1;
            continue;
        }
        let run_start = i;
        while i < b.len() && is_url_byte(b[i]) {
            i += 1;
        }
        let run = &text[run_start..i];
        if let Some(found) = link_in_run(run, &mut work) {
            out.push((run_start + found.0, run_start + found.1, found.2));
        }
        if work == 0 {
            break;
        }
    }
    out
}

fn link_in_run(run: &str, work: &mut usize) -> Option<(usize, usize, String)> {
    let rb = run.as_bytes();
    for at in 0..rb.len() {
        // A candidate must not continue a word ("xhttp://").
        if at > 0 && rb[at - 1].is_ascii_alphanumeric() {
            continue;
        }
        if !matches!(rb[at], b'h' | b'H' | b'w' | b'W') || !has_web_prefix(&run[at..]) {
            continue;
        }
        let tail = &run[at..];
        *work = work.saturating_sub(tail.len());
        let cand = trim_link_end(tail);
        if let Some(url) = web_url(cand) {
            return Some((at, at + cand.len(), url));
        }
        if *work == 0 {
            return None;
        }
    }
    let (s, e) = email_in(run)?;
    Some((s, e, format!("mailto:{}", &run[s..e])))
}

/// Scheme of the internal link a phone number gets in the markdown. It is
/// not openable: the UI turns a click on it into a small menu.
pub const PHONE_SCHEME: &str = "wa-phone:";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpanKind {
    Url,
    Phone,
}

/// A link-like part of a message: byte range, kind and the link target (a
/// URL, or for phone numbers [`PHONE_SCHEME`] plus [`phone_digits`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub kind: SpanKind,
    pub target: String,
}

/// ASCII value of a decimal digit, including Persian and Arabic-Indic ones.
fn digit_value(c: char) -> Option<u8> {
    match c {
        '0'..='9' => Some(c as u8 - b'0'),
        '\u{6f0}'..='\u{6f9}' => Some((c as u32 - 0x6f0) as u8),
        '\u{660}'..='\u{669}' => Some((c as u32 - 0x660) as u8),
        _ => None,
    }
}

fn is_phone_sep(c: char) -> bool {
    matches!(c, ' ' | '-' | '.' | '(' | ')')
}

/// `raw` with ASCII digits and a leading `+` kept: `"+98 (912)"` -> `"+98912"`.
/// `None` when `raw` holds anything but digits and phone separators.
pub fn phone_digits(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let mut out = String::with_capacity(raw.len());
    for (i, c) in raw.chars().enumerate() {
        if let Some(d) = digit_value(c) {
            out.push((b'0' + d) as char);
        } else if c == '+' && i == 0 {
            out.push('+');
        } else if !is_phone_sep(c) {
            return None;
        }
    }
    Some(out)
}

/// Digits (no `+`) of a phone number as written in a message.
///
/// International numbers (`+` and 8-15 digits) are returned as they are. A
/// local number (a leading `0` and 9-11 more digits) gets `own_cc` (the
/// country code of the account's own number) in place of the `0`; without
/// `own_cc` its digits are returned as typed, still starting with `0`.
pub fn normalize_phone(raw: &str, own_cc: Option<&str>) -> Option<String> {
    let digits = phone_digits(raw)?;
    if let Some(intl) = digits.strip_prefix('+') {
        return ((8..=15).contains(&intl.len()) && !intl.starts_with('0')).then(|| intl.to_string());
    }
    let national = digits.strip_prefix('0')?;
    if !(9..=11).contains(&national.len()) || national.starts_with('0') {
        return None;
    }
    match own_cc.filter(|cc| !cc.is_empty() && cc.bytes().all(|b| b.is_ascii_digit())) {
        Some(cc) if cc.len() + national.len() <= 15 => Some(format!("{cc}{national}")),
        Some(_) => None,
        None => Some(digits),
    }
}

fn is_currency(c: char) -> bool {
    matches!(c, '$' | '€' | '£' | '¥' | '₹' | '﷼' | '₽' | '%' | '٪')
}

/// Whether the text before a candidate allows it to start a phone number.
fn phone_start_ok(text: &str, at: usize) -> bool {
    let mut before = text[..at].chars().rev();
    match before.next() {
        None => true,
        Some(c) if c.is_alphanumeric() || is_currency(c) => false,
        Some('#' | '/' | '@' | '_' | '=' | ':' | '\\' | '+' | '<') => false,
        Some('.' | ',' | '-') => !before.next().is_some_and(char::is_alphanumeric),
        Some(_) => true,
    }
}

/// Whether the text after a candidate allows it to end a phone number.
fn phone_end_ok(text: &str, end: usize) -> bool {
    let mut after = text[end..].chars();
    match after.next() {
        None => true,
        Some(c) if c.is_alphanumeric() || is_currency(c) => false,
        Some('/' | '@' | '_' | ':') => false,
        Some('.' | ',') => !after.next().is_some_and(|c| digit_value(c).is_some()),
        Some(_) => true,
    }
}

/// `(start, end)` of every phone number in `text`, in order.
fn phone_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        let international = c == '+';
        if !(international || digit_value(c) == Some(0)) ||!phone_start_ok(text, start) {
            continue;
        }
        // Take digits and separators greedily, then cut the tail back to the
        // last digit.
        let mut last_digit = (!international).then(|| start + c.len_utf8());
        let mut open = 0i32;
        let mut prev_sep = false;
        while let Some(&(i, n)) = chars.peek() {
            if digit_value(n).is_some() {
                prev_sep = false;
                last_digit = Some(i + n.len_utf8());
            } else if is_phone_sep(n) {
                // One separator between digit groups; " (" and ") " are two.
                if prev_sep && !matches!(n, '(' | ')' | ' ') {
                    break;
                }
                match n {
                    '(' => open += 1,
                    ')' => open -= 1,
                    _ => {}
                }
                if open < 0 {
                    break;
                }
                prev_sep = true;
            } else {
                break;
            }
            chars.next();
        }
        let Some(stop) = last_digit else { continue };
        if normalize_phone(&text[start..stop], None).is_some() && phone_end_ok(text, stop) {
            out.push((start, stop));
        }
    }
    out
}

/// Links and phone numbers of `text`, in order. A phone number inside a link
/// or an e-mail address is not reported.
pub fn find_spans(text: &str) -> Vec<Span> {
    let mut spans: Vec<Span> = find_links(text)
        .into_iter()
        .map(|(start, end, target)| Span { start, end, kind: SpanKind::Url, target })
        .collect();
    for (start, end) in phone_ranges(text) {
        let overlaps = spans.iter().any(|s| start < s.end && s.start < end);
        if let (false, Some(digits)) = (overlaps, phone_digits(&text[start..end])) {
            spans.push(Span { start, end, kind: SpanKind::Phone, target: format!("{PHONE_SCHEME}{digits}") });
        }
    }
    spans.sort_by_key(|s| s.start);
    spans
}

/// The phone number (as [`phone_digits`] wrote it) in a phone link target.
pub fn phone_of_target(target: &str) -> Option<&str> {
    target.strip_prefix(PHONE_SCHEME)
}

/// Only these schemes are ever handed to the OS for a clicked link.
pub fn is_safe_link(url: &str) -> bool {
    let scheme_ok = starts_with_ci(url, "https://") || starts_with_ci(url, "http://") || starts_with_ci(url, "mailto:");
    scheme_ok && url.is_ascii() && !url.chars().any(|c| c.is_control() || c.is_whitespace() || c == '"')
}

/// Backslash-escapes ASCII punctuation (valid for every such character in
/// CommonMark), so nothing in the text is read as markup or HTML.
pub(crate) fn escape_markdown(s: &str, out: &mut String) {
    for c in s.chars() {
        if c.is_ascii_punctuation() {
            out.push('\\');
        }
        out.push(c);
    }
}

pub(crate) fn link_target(url: &str, out: &mut String) {
    for c in url.chars() {
        match c {
            '<' => out.push_str("%3C"),
            '>' => out.push_str("%3E"),
            '\\' => out.push_str("%5C"),
            // Else the markdown parser decodes entities ("&amp;", "&#x202E;")
            // and the opened URL would differ from the shown one.
            '&' => out.push_str("%26"),
            c => out.push(c),
        }
    }
}

/// Markdown for `text` with its links clickable; `None` when it has none.
pub fn markdown_with_links(text: &str) -> Option<String> {
    let mut any = false;
    let mut md = String::with_capacity(text.len() + 32);
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    // A hard break at the very end would show as a literal backslash.
    for (n, line) in text.trim_end_matches('\n').split('\n').enumerate() {
        if n > 0 {
            // A hard break; a soft one would join the lines.
            md.push_str("\\\n");
        }
        // Leading blanks are kept, as no-break spaces: plain ones would be
        // dropped, or turn into an indented code block.
        let body = line.trim_start_matches([' ', '\t']);
        for c in line[..line.len() - body.len()].chars() {
            md.push_str(if c == '\t' { "\u{a0}\u{a0}\u{a0}\u{a0}" } else { "\u{a0}" });
        }
        let line = body;
        let mut pos = 0;
        for Span { start: s, end: e, target: url, .. } in find_spans(line) {
            any = true;
            escape_markdown(&line[pos..s], &mut md);
            md.push('[');
            escape_markdown(&line[s..e], &mut md);
            md.push_str("](<");
            link_target(&url, &mut md);
            md.push_str(">)");
            pos = e;
        }
        escape_markdown(&line[pos..], &mut md);
    }
    any.then_some(md)
}

/// Styled text with clickable links, or `None` when `text` has no link (it is
/// then shown as a plain, right-clickable `Text`).
pub fn styled_text(text: &str) -> Option<StyledText> {
    let md = markdown_with_links(text)?;
    match StyledText::from_markdown(&md) {
        Ok(s) => Some(s),
        Err(e) => {
            log::warn!("link markup rejected, showing plain text: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(text: &str) -> Vec<String> {
        find_links(text).into_iter().map(|l| l.2).collect()
    }

    fn shown(text: &str) -> Vec<&str> {
        find_links(text).into_iter().map(|(s, e, _)| &text[s..e]).collect()
    }

    #[test]
    fn plain_links() {
        assert_eq!(urls("see https://example.com/a?b=1#c ok"), ["https://example.com/a?b=1#c"]);
        assert_eq!(urls("HTTP://EXAMPLE.COM"), ["HTTP://EXAMPLE.COM"]);
        assert_eq!(urls("http://localhost:8080/x"), ["http://localhost:8080/x"]);
    }

    #[test]
    fn www_gets_https() {
        assert_eq!(urls("go to www.example.com/x now"), ["https://www.example.com/x"]);
        assert_eq!(shown("go to www.example.com/x now"), ["www.example.com/x"]);
        assert!(urls("www.localhost").is_empty());
        assert!(urls("www.").is_empty());
    }

    #[test]
    fn trailing_punctuation_is_not_part_of_the_link() {
        assert_eq!(shown("Visit https://a.com/x."), ["https://a.com/x"]);
        assert_eq!(shown("https://a.com/x, then"), ["https://a.com/x"]);
        assert_eq!(shown("really https://a.com/x?!"), ["https://a.com/x"]);
        assert_eq!(shown("*https://a.com/x*"), ["https://a.com/x"]);
        assert_eq!(shown("a.com: https://a.com/x;"), ["https://a.com/x"]);
    }

    #[test]
    fn parentheses() {
        assert_eq!(shown("(see https://a.com/x)"), ["https://a.com/x"]);
        assert_eq!(shown("(https://a.com/x)."), ["https://a.com/x"]);
        assert_eq!(shown("https://en.wikipedia.org/wiki/Rust_(language)"), ["https://en.wikipedia.org/wiki/Rust_(language)"]);
        assert_eq!(shown("(https://en.wikipedia.org/wiki/Rust_(language))"), ["https://en.wikipedia.org/wiki/Rust_(language)"]);
        assert_eq!(shown("[https://a.com/x]"), ["https://a.com/x"]);
    }

    #[test]
    fn persian_and_rtl_text_around_links() {
        let t = "سلام https://example.com/page، لطفا ببین";
        assert_eq!(shown(t), ["https://example.com/page"]);
        let t = "لینک:https://example.com/a؟";
        assert_eq!(shown(t), ["https://example.com/a"]);
        let t = "سایت www.example.ir را ببین.";
        assert_eq!(urls(t), ["https://www.example.ir"]);
        let t = "ایمیل: info@example.com، ممنون";
        assert_eq!(urls(t), ["mailto:info@example.com"]);
        // Offsets are byte offsets into the original text.
        let t = "سلام https://a.com";
        let (s, e, _) = find_links(t).remove(0);
        assert_eq!(&t[s..e], "https://a.com");
    }

    #[test]
    fn adversarial_input_is_handled_quickly() {
        let started = std::time::Instant::now();
        let dots = format!("{}{}", "h.".repeat(30_000), ")".repeat(30_000));
        let www = format!("{}{}", "www.a".repeat(6_000), "]".repeat(30_000));
        let parens = format!("{}{}", "www.".repeat(15_000), ")".repeat(15_000));
        for t in [&dots, &www, &parens] {
            let _ = find_links(t);
            let _ = markdown_with_links(t);
        }
        assert!(started.elapsed() < std::time::Duration::from_millis(200), "{:?}", started.elapsed());
    }

    #[test]
    fn non_candidates_cost_no_trim_work_and_candidates_are_budgeted() {
        let dots = format!("{}{}", "h.".repeat(30_000), ")".repeat(30_000));
        let mut work = MAX_TRIM_WORK;
        assert!(link_in_run(&dots, &mut work).is_none());
        assert_eq!(work, MAX_TRIM_WORK, "the prefix check must run before any trimming");

        // Every "www.a/" candidate fails (no dot in the host), so all are tried.
        let www = format!("{}{}", "www.a/".repeat(10_000), ")".repeat(10_000));
        let mut work = MAX_TRIM_WORK;
        assert!(link_in_run(&www, &mut work).is_none());
        assert_eq!(work, 0, "candidate scanning must stop at the work budget");
    }

    #[test]
    fn trimming_balanced_and_unbalanced_brackets_is_unchanged() {
        assert_eq!(trim_link_end("a.com/x(y))))"), "a.com/x(y)");
        assert_eq!(trim_link_end("a.com/[x]]]"), "a.com/[x]");
        assert_eq!(trim_link_end("a.com/(x)."), "a.com/(x)");
        assert_eq!(trim_link_end("a.com/x)]"), "a.com/x");
        assert_eq!(trim_link_end(")))"), "");
    }

    #[test]
    fn link_count_per_message_is_capped() {
        let t = "http://a.com ".repeat(MAX_LINKS + 50);
        assert_eq!(find_links(&t).len(), MAX_LINKS);
    }

    #[test]
    fn emails() {
        assert_eq!(urls("write to john.doe+x@mail.example.org."), ["mailto:john.doe+x@mail.example.org"]);
        assert_eq!(urls("(a@b.co)"), ["mailto:a@b.co"]);
        assert!(urls("@name and a@b and a@b.c and x@.com").is_empty());
        assert!(urls("price @ 5.00").is_empty());
    }

    #[test]
    fn multiple_links_in_order() {
        let t = "a https://one.com b www.two.org c me@three.net";
        assert_eq!(urls(t), ["https://one.com", "https://www.two.org", "mailto:me@three.net"]);
    }

    #[test]
    fn non_links_are_ignored() {
        for t in ["", "hello world", "e.g. this", "xhttp://a.com", "awww.example.com", "ftp://a.com", "javascript:alert(1)", "file:///c:/x", "http://", "https://.", "3.14"] {
            assert!(find_links(t).is_empty(), "{t}");
        }
    }

    #[test]
    fn only_web_and_mail_links_are_openable() {
        assert!(is_safe_link("https://a.com"));
        assert!(is_safe_link("HTTP://a.com/x"));
        assert!(is_safe_link("mailto:a@b.co"));
        for u in ["file:///c:/x.exe", "javascript:alert(1)", "ms-msdt:x", "ftp://a.com", "\\\\host\\share", "https://a.com/ x", "https://a.com/\n", "c:\\x.exe", ""] {
            assert!(!is_safe_link(u), "{u}");
        }
    }

    #[test]
    fn markdown_escapes_everything_but_the_links() {
        let md = markdown_with_links("*bold* <b>x</b> [a](b) https://a.com/x_y.").expect("has link");
        assert_eq!(
            md,
            "\\*bold\\* \\<b\\>x\\<\\/b\\> \\[a\\]\\(b\\) [https\\:\\/\\/a\\.com\\/x\\_y](<https://a.com/x_y>)\\."
        );
        assert!(markdown_with_links("no links *here*").is_none());
    }

    #[test]
    fn entities_in_urls_cannot_change_the_opened_target() {
        for (text, target) in [
            ("https://a.com/?x=1&amp;y=2", "https://a.com/?x=1%26amp;y=2"),
            ("https://a.com/&#x202E;evil", "https://a.com/%26#x202E;evil"),
            ("https://a.com/a&#10;b", "https://a.com/a%26#10;b"),
        ] {
            let md = markdown_with_links(text).expect("has link");
            assert!(md.contains(&format!("](<{target}>)")), "{md}");
            assert!(styled_text(text).is_some(), "{text}");
        }
    }

    #[test]
    fn non_ascii_urls_are_not_openable() {
        assert!(!is_safe_link("https://a.com/\u{202e}x"));
        assert!(!is_safe_link("https://a.com/\u{430}"));
        assert!(is_safe_link("https://a.com/%D8%B3"));
    }

    #[test]
    fn line_breaks_and_leading_spaces_are_kept() {
        let md = markdown_with_links("one https://a.com\n  two\n\nfour\r\nfive\n\n").expect("has link");
        assert_eq!(
            md,
            "one [https\\:\\/\\/a\\.com](<https://a.com>)\\\n\u{a0}\u{a0}two\\\n\\\nfour\\\nfive"
        );
        assert!(styled_text("one https://a.com\n    two\n\nfour").is_some());
    }

    #[test]
    fn markdown_parses_into_styled_text() {
        for t in [
            "https://a.com/x",
            "- list https://a.com\n1. one\n# head www.b.org\n> quote me@c.io",
            "    indented https://a.com\r\nline two\n\n```code``` https://a.com",
            "<font color=\"red\">x</font> https://a.com &amp; | --- |",
        ] {
            assert!(styled_text(t).is_some(), "{t}");
        }
        assert!(styled_text("plain").is_none());
    }

    fn phones(text: &str) -> Vec<&str> {
        find_spans(text).into_iter().filter(|s| s.kind == SpanKind::Phone).map(|s| &text[s.start..s.end]).collect()
    }

    #[test]
    fn international_numbers_are_found() {
        assert_eq!(phones("call +98 912 345 6789 now"), ["+98 912 345 6789"]);
        assert_eq!(phones("+1 (415) 555-0100"), ["+1 (415) 555-0100"]);
        assert_eq!(phones("tel: +44.20.7946.0958."), ["+44.20.7946.0958"]);
        assert_eq!(phones("(+98912345678)"), ["+98912345678"]);
        assert_eq!(phones("+98912345678, +4915112345678;"), ["+98912345678", "+4915112345678"]);
    }

    #[test]
    fn local_numbers_are_found() {
        assert_eq!(phones("09123456789"), ["09123456789"]);
        assert_eq!(phones("شماره 0912 345 6789 است"), ["0912 345 6789"]);
        assert_eq!(phones("0912-345-6789"), ["0912-345-6789"]);
        assert_eq!(phones("a 0912345678 b 09351112222"), ["0912345678", "09351112222"]);
    }

    #[test]
    fn persian_and_arabic_digits_are_found_and_normalised() {
        let t = "تماس: ۰۹۱۲ ۳۴۵ ۶۷۸۹ یا ٠٩١٢٣٤٥٦٧٨٩";
        assert_eq!(phones(t), ["۰۹۱۲ ۳۴۵ ۶۷۸۹", "٠٩١٢٣٤٥٦٧٨٩"]);
        let spans = find_spans(t);
        assert_eq!(spans[0].target, "wa-phone:09123456789");
        assert_eq!(spans[1].target, "wa-phone:09123456789");
        assert_eq!(phones("+۹۸ ۹۱۲ ۳۴۵ ۶۷۸۹"), ["+۹۸ ۹۱۲ ۳۴۵ ۶۷۸۹"]);
        assert_eq!(normalize_phone("+۹۸ ۹۱۲ ۳۴۵ ۶۷۸۹", None).as_deref(), Some("989123456789"));
    }

    #[test]
    fn non_phones_are_ignored() {
        for t in [
            "2024-11-20",
            "on 2024.11.20 at 09:30",
            "09:30:15",
            "03-11-2024",
            "$09123456789",
            "09123456789$",
            "15% 0912345678%",
            "order 1234567890123456789",
            "+1234567890123456",
            "abc09123456789",
            "09123456789abc",
            "ab+989123456789",
            "https://example.com/09123456789",
            "https://example.com/?p=+989123456789",
            "09123456789@mail.com",
            "x09123456789@mail.com",
            "www.example.com/09123456789",
            "+1234567",
            "0123456",
            "012345678901234",
            "1234567890",
            "09123456789.50",
            "00123456789",
            "v1.0.0.12345678",
            "#09123456789",
        ] {
            assert!(phones(t).is_empty(), "{t}: {:?}", phones(t));
        }
    }

    #[test]
    fn links_and_phones_come_out_in_order() {
        let t = "a +989123456789 b https://x.com c 09123456789 d me@x.io";
        let kinds: Vec<SpanKind> = find_spans(t).iter().map(|s| s.kind).collect();
        assert_eq!(kinds, [SpanKind::Phone, SpanKind::Url, SpanKind::Phone, SpanKind::Url]);
        assert_eq!(find_links(t).len(), 2);
    }

    #[test]
    fn normalize_international_and_local() {
        assert_eq!(normalize_phone("+98 912 345 6789", None).as_deref(), Some("989123456789"));
        assert_eq!(normalize_phone("+1 (415) 555-0100", Some("98")).as_deref(), Some("14155550100"));
        assert_eq!(normalize_phone("0912 345 6789", Some("98")).as_deref(), Some("989123456789"));
        assert_eq!(normalize_phone("09123456789", None).as_deref(), Some("09123456789"));
        assert_eq!(normalize_phone("09123456789", Some("")).as_deref(), Some("09123456789"));
        assert_eq!(normalize_phone("09123456789", Some("9x")).as_deref(), Some("09123456789"));
        assert_eq!(normalize_phone("09123456789", Some("12345")).as_deref(), Some("123459123456789"));
        assert!(normalize_phone("09123456789", Some("123456")).is_none());
        assert!(normalize_phone("+0123456789", None).is_none());
        assert!(normalize_phone("+1234567", None).is_none());
        assert!(normalize_phone("912345678", None).is_none());
        assert!(normalize_phone("+98 abc", None).is_none());
        assert!(normalize_phone("", None).is_none());
    }

    #[test]
    fn phone_links_are_markdown_links_but_never_openable() {
        let md = markdown_with_links("call +98 912 345 6789.").expect("has phone");
        assert_eq!(md, "call [\\+98 912 345 6789](<wa-phone:+989123456789>)\\.");
        assert!(styled_text("call 09123456789").is_some());
        assert!(!is_safe_link("wa-phone:+989123456789"));
        assert_eq!(phone_of_target("wa-phone:+98912"), Some("+98912"));
        assert_eq!(phone_of_target("https://a.com"), None);
    }
}
