//! WhatsApp text formatting: `*bold*`, `_italic_`, `~strike~`, `` `code` ``,
//! ```` ```monospace``` ````, `> quote` lines (lists keep their markers).
//!
//! The markup is plain text on the wire, so this module only (1) parses it for
//! display ([`styled_text`], [`strip`]) and (2) edits the composer text for the
//! shortcuts ([`apply`]).
//!
//! Rules, as WhatsApp applies them: an opening marker must not follow a letter
//! or digit and must touch non-space text; a closing marker must follow
//! non-space text and must not be followed by a letter or digit (so `2*3*4`
//! and `snake_case_name` stay plain); nothing spans lines except monospace
//! blocks, whose text is verbatim; markers inside a link do not count.

use crate::links;
use slint::StyledText;

/// Longer lines are shown unformatted: finding the closing marker is a scan
/// per opening marker, which must stay cheap on hostile input.
const MAX_FORMATTED_LINE: usize = 8 * 1024;

/// Colour of the quote bar.
const QUOTE_COLOR: &str = "#8696a0";

#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    Text(String),
    Bold(Vec<Item>),
    Italic(Vec<Item>),
    Strike(Vec<Item>),
    /// Inline `code`, verbatim.
    Code(String),
    /// A ``` block, verbatim, possibly several lines.
    Mono(String),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Line {
    pub quote: bool,
    pub items: Vec<Item>,
}

fn is_marker(c: char) -> bool {
    matches!(c, '*' | '_' | '~' | '`')
}

fn in_link(links: &[(usize, usize)], at: usize) -> bool {
    links.iter().any(|&(s, e)| s <= at && at < e)
}

/// Byte offset of the closing marker for the opening marker `m` at `open`.
fn find_close(s: &str, open: usize, hi: usize, m: char, links: &[(usize, usize)]) -> Option<usize> {
    if s[..open].chars().next_back().is_some_and(char::is_alphanumeric) {
        return None;
    }
    let start = open + m.len_utf8();
    let first = s[start..hi].chars().next()?;
    if first.is_whitespace() || first == m {
        return None;
    }
    for (off, c) in s[start..hi].char_indices() {
        let pos = start + off;
        if c != m || pos == start || in_link(links, pos) {
            continue;
        }
        let prev_ok = s[..pos].chars().next_back().is_some_and(|p| !p.is_whitespace());
        let next_ok = !s[pos + m.len_utf8()..].chars().next().is_some_and(char::is_alphanumeric);
        if prev_ok && next_ok {
            return Some(pos);
        }
    }
    None
}

fn inline(s: &str, lo: usize, hi: usize, links: &[(usize, usize)]) -> Vec<Item> {
    let mut out = Vec::new();
    let mut text_from = lo;
    let mut i = lo;
    while i < hi {
        let Some(c) = s[i..hi].chars().next() else { break };
        if is_marker(c) && !in_link(links, i) {
            if let Some(close) = find_close(s, i, hi, c, links) {
                if text_from < i {
                    out.push(Item::Text(s[text_from..i].to_string()));
                }
                let inner = i + c.len_utf8();
                out.push(match c {
                    '*' => Item::Bold(inline(s, inner, close, links)),
                    '_' => Item::Italic(inline(s, inner, close, links)),
                    '~' => Item::Strike(inline(s, inner, close, links)),
                    _ => Item::Code(s[inner..close].to_string()),
                });
                i = close + c.len_utf8();
                text_from = i;
                continue;
            }
        }
        i += c.len_utf8();
    }
    if text_from < hi {
        out.push(Item::Text(s[text_from..hi].to_string()));
    }
    out
}

fn parse_inline(s: &str) -> Vec<Item> {
    if s.is_empty() {
        return Vec::new();
    }
    if s.len() > MAX_FORMATTED_LINE || !s.contains(is_marker) {
        return vec![Item::Text(s.to_string())];
    }
    let spans: Vec<(usize, usize)> = links::find_links(s).into_iter().map(|(a, b, _)| (a, b)).collect();
    inline(s, 0, s.len(), &spans)
}

/// The first complete, non-blank ``` block of `s`: (text before, body, text
/// after). A blank block (``` ```) stays literal text and scanning goes on.
fn mono_block(s: &str) -> Option<(&str, &str, &str)> {
    let mut from = 0;
    loop {
        let open = from + s[from..].find("```")?;
        let body_from = open + 3;
        let close = body_from + s[body_from..].find("```")?;
        let body = &s[body_from..close];
        if body.trim().is_empty() {
            from = close + 3;
            continue;
        }
        // A fence on its own line: its line break is not part of the text.
        let body = body.strip_prefix('\n').unwrap_or(body);
        let body = body.strip_suffix('\n').unwrap_or(body);
        return Some((&s[..open], body, &s[close + 3..]));
    }
}

fn push_plain(lines: &mut Vec<Line>, s: &str) {
    for (n, seg) in s.split('\n').enumerate() {
        if n > 0 || lines.is_empty() {
            lines.push(Line::default());
        }
        let Some(line) = lines.last_mut() else { return };
        let mut seg = seg;
        if line.items.is_empty() && !line.quote {
            if let Some(rest) = seg.strip_prefix("> ") {
                line.quote = true;
                seg = rest;
            }
        }
        line.items.extend(parse_inline(seg));
    }
}

/// Parses `text` into lines of formatted items.
pub fn parse(text: &str) -> Vec<Line> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut rest = text.trim_end_matches('\n');
    let mut lines = vec![Line::default()];
    while let Some((before, body, after)) = mono_block(rest) {
        push_plain(&mut lines, before);
        if let Some(line) = lines.last_mut() {
            line.items.push(Item::Mono(body.to_string()));
        }
        rest = after;
    }
    push_plain(&mut lines, rest);
    lines
}

fn strip_items(items: &[Item], out: &mut String) {
    for item in items {
        match item {
            Item::Text(t) | Item::Code(t) | Item::Mono(t) => out.push_str(t),
            Item::Bold(c) | Item::Italic(c) | Item::Strike(c) => strip_items(c, out),
        }
    }
}

/// `text` with the formatting markers removed (chat list preview, reply bar).
pub fn strip(text: &str) -> String {
    if !text.contains(['*', '_', '~', '`', '>']) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    for (n, line) in parse(text).iter().enumerate() {
        if n > 0 {
            out.push('\n');
        }
        strip_items(&line.items, &mut out);
    }
    out
}

fn push_code_span(s: &str, md: &mut String) {
    let mut longest = 0;
    let mut run = 0;
    for c in s.chars() {
        run = if c == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    let fence = "`".repeat(longest + 1);
    // One space inside each fence is dropped by the parser; it also keeps a
    // leading or trailing backtick from merging with the fence.
    // A span of only spaces is kept as it is by the parser: padding would add two.
    let all_space = s.bytes().all(|b| b == b' ');
    let pad = !all_space && (s.starts_with('`') || s.ends_with('`') || (s.starts_with(' ') && s.ends_with(' ')));
    md.push_str(&fence);
    if pad {
        md.push(' ');
    }
    md.push_str(s);
    if pad {
        md.push(' ');
    }
    md.push_str(&fence);
}

/// Plain text with its links and phone numbers as markdown links.
fn push_text(t: &str, lead: bool, md: &mut String) {
    let mut t = t;
    if lead {
        // Leading blanks are kept, as no-break spaces: plain ones would be
        // dropped, or turn into an indented code block.
        let body = t.trim_start_matches([' ', '\t']);
        for c in t[..t.len() - body.len()].chars() {
            md.push_str(if c == '\t' { "\u{a0}\u{a0}\u{a0}\u{a0}" } else { "\u{a0}" });
        }
        t = body;
    }
    let mut pos = 0;
    for links::Span { start, end, target, .. } in links::find_spans(t) {
        links::escape_markdown(&t[pos..start], md);
        md.push('[');
        links::escape_markdown(&t[start..end], md);
        md.push_str("](<");
        links::link_target(&target, md);
        md.push_str(">)");
        pos = end;
    }
    links::escape_markdown(&t[pos..], md);
}

fn wrap(mark: &str, children: &[Item], md: &mut String) {
    md.push_str(mark);
    push_items(children, false, md);
    md.push_str(mark);
}

fn push_items(items: &[Item], lead: bool, md: &mut String) {
    for (n, item) in items.iter().enumerate() {
        let lead = lead && n == 0;
        match item {
            Item::Text(t) => push_text(t, lead, md),
            Item::Bold(c) => wrap("**", c, md),
            Item::Italic(c) => wrap("*", c, md),
            Item::Strike(c) => wrap("~~", c, md),
            Item::Code(t) => {
                        push_code_span(t, md);
            }
            Item::Mono(body) => {
                        for (k, l) in body.split('\n').enumerate() {
                    if k > 0 {
                        md.push_str("\\\n");
                    }
                    if !l.is_empty() {
                        push_code_span(l, md);
                    }
                }
            }
        }
    }
}

fn is_plain(lines: &[Line]) -> bool {
    lines.iter().all(|l| !l.quote && l.items.iter().all(|i| matches!(i, Item::Text(_))))
}

/// Markdown for `text` (formatting and clickable links); `None` when it has
/// no formatting (the links module handles that case).
pub fn markdown(text: &str) -> Option<String> {
    let lines = parse(text);
    if is_plain(&lines) {
        return None;
    }
    let mut md = String::with_capacity(text.len() + 32);
    for (n, line) in lines.iter().enumerate() {
        if n > 0 {
            // A hard break; a soft one would join the lines.
            md.push_str("\\\n");
        }
        if line.quote {
            md.push_str(&format!("<font color=\"{QUOTE_COLOR}\">\u{258e}</font>\u{a0}"));
        }
        push_items(&line.items, !line.quote, &mut md);
    }
    Some(md)
}

/// Styled text for a message, or `None` when it has no formatting and no link
/// (it is then shown as a plain, right-clickable `Text`).
pub fn styled_text(text: &str) -> Option<StyledText> {
    let Some(md) = markdown(text) else { return links::styled_text(text) };
    match StyledText::from_markdown(&md) {
        Ok(s) => Some(s),
        Err(e) => {
            log::warn!("text markup rejected, showing plain text: {e}");
            None
        }
    }
}

// ---- paragraphs ----------------------------------------------------------------------

/// More paragraphs than this are not split further: the rest becomes one last
/// paragraph, so a huge message cannot create thousands of bubble children.
pub const MAX_PARAGRAPHS: usize = 200;

/// One display paragraph of a message: a line, or a ``` block (with the line it
/// sits on), with its own text direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Para {
    /// Source text (formatting markers included), without its line break.
    pub text: String,
    pub rtl: bool,
    /// Nothing but whitespace: only keeps the vertical spacing.
    pub blank: bool,
}

/// Byte ranges of the complete ``` blocks of `text`, found like [`parse`] does.
fn mono_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some((before, _, after)) = mono_block(&text[from..]) {
        let start = from + before.len();
        let end = text.len() - after.len();
        out.push((start, end));
        from = end;
    }
    out
}

/// Whether a key's text may be put into the composer as typed text: not empty, no
/// control characters and nothing from the private-use range (Slint sends its special
/// keys, such as the function keys, as characters of that range).
pub fn insertable_text(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| !c.is_control() && !('\u{e000}'..='\u{f8ff}').contains(&c))
}

/// Splits a message into paragraphs on line breaks (CRLF, CR and LF); a ``` block
/// stays in one paragraph. A paragraph without a letter (digits, emoji, ...)
/// takes the direction of the one before it (the first one: of the next).
pub fn paragraphs(text: &str) -> Vec<Para> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let text = text.trim_end_matches('\n');
    let blocks = mono_ranges(text);
    let mut texts: Vec<&str> = Vec::new();
    let mut line_start = 0;
    let mut block = 0;
    for (i, c) in text.char_indices() {
        if texts.len() + 1 >= MAX_PARAGRAPHS {
            break;
        }
        while blocks.get(block).is_some_and(|&(_, end)| end <= i) {
            block += 1;
        }
        let inside = blocks.get(block).is_some_and(|&(start, _)| start <= i);
        if c == '\n' && !inside {
            texts.push(&text[line_start..i]);
            line_start = i + 1;
        }
    }
    texts.push(&text[line_start..]);

    let mut dirs: Vec<Option<bool>> = texts
        .iter()
        .map(|t| {
            let t = t.trim_start();
            if t.starts_with("```") {
                return Some(false);
            }
            crate::textdir::strong_direction(t)
        })
        .collect();
    let mut last = dirs.iter().flatten().next().copied().unwrap_or(false);
    for d in &mut dirs {
        last = d.unwrap_or(last);
        *d = Some(last);
    }
    texts
        .into_iter()
        .zip(dirs)
        .map(|(t, d)| Para { text: t.to_string(), rtl: d.unwrap_or(false), blank: t.trim().is_empty() })
        .collect()
}

/// Whether the paragraphs of a message need their own alignment: at least two
/// text paragraphs differ in direction.
pub fn mixed_direction(paras: &[Para]) -> bool {
    let mut seen = paras.iter().filter(|p| !p.blank).map(|p| p.rtl);
    seen.next().is_some_and(|first| seen.any(|d| d != first))
}

// ---- composer editing --------------------------------------------------------------

/// What a composer shortcut or toolbar button applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Bold,
    Italic,
    Strike,
    Mono,
    Quote,
}

impl Kind {
    /// The numbers the UI uses: 0 bold, 1 italic, 2 strike, 3 mono, 4 quote.
    pub fn from_code(code: i32) -> Option<Kind> {
        Some(match code {
            0 => Kind::Bold,
            1 => Kind::Italic,
            2 => Kind::Strike,
            3 => Kind::Mono,
            4 => Kind::Quote,
            _ => return None,
        })
    }

    fn marker(self) -> &'static str {
        match self {
            Kind::Bold => "*",
            Kind::Italic => "_",
            Kind::Strike => "~",
            Kind::Mono => "```",
            Kind::Quote => "> ",
        }
    }
}

/// The composer text after an edit, with the selection to restore (byte
/// offsets, `anchor` .. `cursor`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub text: String,
    pub anchor: usize,
    pub cursor: usize,
}

fn floor_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn replace(text: &str, from: usize, to: usize, with: &str) -> String {
    format!("{}{}{}", &text[..from], with, &text[to..])
}

/// Applies `kind` to the selection `anchor` .. `cursor` of `text`: wraps it in
/// the markers, or unwraps it when it is already wrapped. With no selection
/// the markers are inserted as a pair around the cursor (or the empty pair
/// around the cursor removed).
pub fn apply(text: &str, anchor: usize, cursor: usize, kind: Kind) -> Edit {
    let a = floor_boundary(text, anchor);
    let c = floor_boundary(text, cursor);
    let (start, end) = (a.min(c), a.max(c));
    if kind == Kind::Quote {
        return apply_quote(text, start, end);
    }
    let m = kind.marker();
    let ml = m.len();
    let sel = &text[start..end];

    // Already wrapped: markers inside the selection, or right around it.
    if sel.len() > 2 * ml && sel.starts_with(m) && sel.ends_with(m) {
        let inner = &sel[ml..sel.len() - ml];
        return Edit { text: replace(text, start, end, inner), anchor: start, cursor: start + inner.len() };
    }
    if start >= ml && text[..start].ends_with(m) && text[end..].starts_with(m) {
        let (from, to) = (start - ml, end + ml);
        return Edit { text: replace(text, from, to, sel), anchor: from, cursor: from + sel.len() };
    }

    // The markers must touch non-space text, so blanks stay outside them.
    let core = sel.trim();
    if core.is_empty() {
        let at = end;
        let new = format!("{}{m}{m}{}", &text[..at], &text[at..]);
        return Edit { text: new, anchor: at + ml, cursor: at + ml };
    }
    let lead = sel.len() - sel.trim_start().len();
    let from = start + lead;
    let to = from + core.len();
    let new = format!("{}{m}{core}{m}{}", &text[..from], &text[to..]);
    Edit { text: new, anchor: from + ml, cursor: from + ml + core.len() }
}

/// Quote: every line touched by the selection gets `> `, or loses it when all
/// of them have it.
fn apply_quote(text: &str, start: usize, end: usize) -> Edit {
    let from = text[..start].rfind('\n').map_or(0, |i| i + 1);
    // A selection ending right after a line break does not include the next line.
    let last = if end > start && text[..end].ends_with('\n') { end - 1 } else { end };
    let to = text[last..].find('\n').map_or(text.len(), |i| last + i);
    let block = &text[from..to];
    let lines: Vec<&str> = block.split('\n').collect();
    let all_quoted = lines.iter().all(|l| l.starts_with("> "));
    let new_block: Vec<String> = lines
        .iter()
        .map(|l| if all_quoted { l[2..].to_string() } else { format!("> {l}") })
        .collect();
    let new_block = new_block.join("\n");
    let text_new = replace(text, from, to, &new_block);
    if start == end {
        let col = start - from;
        let shift = |c: usize| if all_quoted { c.saturating_sub(2) } else { c + 2 };
        let at = from + shift(col);
        return Edit { text: text_new, anchor: at, cursor: at };
    }
    Edit { text: text_new, anchor: from, cursor: from + new_block.len() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_text_is_plain_characters_only() {
        assert!(insertable_text("a"));
        assert!(insertable_text("ز"));
        assert!(insertable_text(" "));
        assert!(insertable_text("\u{1F44D}\u{1F3FD}"));
        assert!(!insertable_text(""));
        assert!(!insertable_text("\u{8}"));
        assert!(!insertable_text("\u{1b}"));
        assert!(!insertable_text("\u{f704}"));
        assert!(!insertable_text("a\u{f700}"));
    }

    fn paras(s: &str) -> Vec<(String, bool, bool)> {
        paragraphs(s).into_iter().map(|p| (p.text, p.rtl, p.blank)).collect()
    }

    #[test]
    fn paragraphs_split_by_direction() {
        let p = paras("سلام\nHello world\nخداحافظ");
        assert_eq!(
            p,
            [("سلام".into(), true, false), ("Hello world".into(), false, false), ("خداحافظ".into(), true, false)]
        );
        assert!(mixed_direction(&paragraphs("سلام\nHello")));
        assert!(!mixed_direction(&paragraphs("سلام\nدوست")));
        assert!(!mixed_direction(&paragraphs("one\n\ntwo")));
    }

    #[test]
    fn paragraphs_keep_blank_lines_and_crlf() {
        let p = paras("a\r\n\r\nب\rc\n\n");
        assert_eq!(
            p,
            [
                ("a".into(), false, false),
                ("".into(), false, true),
                ("ب".into(), true, false),
                ("c".into(), false, false)
            ]
        );
        assert_eq!(paragraphs("").len(), 1);
    }

    #[test]
    fn paragraphs_neutral_lines_inherit_direction() {
        let p = paras("123\nسلام\n456\nhello\n:)");
        let dirs: Vec<bool> = p.iter().map(|x| x.1).collect();
        assert_eq!(dirs, [true, true, true, false, false]);
    }

    #[test]
    fn paragraphs_keep_code_blocks_whole() {
        let p = paras("سلام\n```\nfn a() {\n\n  b\n}\n```\nخداحافظ");
        assert_eq!(p.len(), 3);
        assert_eq!(p[1], ("```\nfn a() {\n\n  b\n}\n```".into(), false, false));
        assert!(p[2].1);
        // An unclosed or blank fence is plain text: split like any other lines.
        assert_eq!(paras("```a\nb").len(), 2);
        assert_eq!(paras("``` ```\nx").len(), 2);
        // A block inside a line stays with its line.
        assert_eq!(paras("a ```b\nc``` d\ne").len(), 2);
    }

    #[test]
    fn paragraphs_format_and_link_per_line() {
        let p = paragraphs("*bold*\nplain\nsee https://a.com");
        assert_eq!(p.len(), 3);
        assert!(styled_text(&p[0].text).is_some());
        assert!(styled_text(&p[1].text).is_none());
        assert!(styled_text(&p[2].text).is_some());
        // Markers do not pair across lines.
        let p = paragraphs("*a\nb*");
        assert!(styled_text(&p[0].text).is_none() && styled_text(&p[1].text).is_none());
    }

    #[test]
    fn paragraphs_are_capped() {
        let text = "x\n".repeat(MAX_PARAGRAPHS * 5);
        let p = paragraphs(&text);
        assert_eq!(p.len(), MAX_PARAGRAPHS);
        assert!(p.last().is_some_and(|l| l.text.contains('\n')));
        let total: usize = p.iter().map(|x| x.text.len()).sum();
        assert!(total > MAX_PARAGRAPHS * 5);
    }

    fn t(s: &str) -> Item {
        Item::Text(s.to_string())
    }

    fn items(s: &str) -> Vec<Item> {
        parse(s).remove(0).items
    }

    #[test]
    fn blank_mono_block_does_not_hide_a_later_block() {
        let lines = parse("``` ``` then ```code```");
        assert_eq!(lines[0].items, [t("``` ``` then "), Item::Mono("code".into())]);
        let lines = parse("``````\n```a```");
        assert!(lines.iter().flat_map(|l| &l.items).any(|i| *i == Item::Mono("a".into())));
    }

    #[test]
    fn all_space_mono_line_is_not_padded() {
        let md = markdown("```a\n  \nb```").expect("formatted");
        assert_eq!(md, "`a`\\\n`  `\\\n`b`");
    }

    #[test]
    fn basic_markers() {
        assert_eq!(items("*hi*"), [Item::Bold(vec![t("hi")])]);
        assert_eq!(items("_hi_"), [Item::Italic(vec![t("hi")])]);
        assert_eq!(items("~hi~"), [Item::Strike(vec![t("hi")])]);
        assert_eq!(items("`hi`"), [Item::Code("hi".into())]);
        assert_eq!(items("a *b* c"), [t("a "), Item::Bold(vec![t("b")]), t(" c")]);
    }

    #[test]
    fn markers_must_touch_text() {
        assert_eq!(items("* a *"), [t("* a *")]);
        assert_eq!(items("*a *"), [t("*a *")]);
        assert_eq!(items("* a*"), [t("* a*")]);
        assert_eq!(items("**"), [t("**")]);
        assert_eq!(items("~ ~"), [t("~ ~")]);
    }

    #[test]
    fn markers_inside_words_do_not_trigger() {
        assert_eq!(items("2*3*4"), [t("2*3*4")]);
        assert_eq!(items("snake_case_name"), [t("snake_case_name")]);
        assert_eq!(items("a*b* c"), [t("a*b* c")]);
        // closing marker followed by a letter
        assert_eq!(items("*ab*c"), [t("*ab*c")]);
        assert_eq!(items("*ab*."), [Item::Bold(vec![t("ab")]), t(".")]);
    }

    #[test]
    fn nesting_and_double_markers() {
        assert_eq!(items("*_a_*"), [Item::Bold(vec![Item::Italic(vec![t("a")])])]);
        assert_eq!(items("*a _b_ c*"), [Item::Bold(vec![t("a "), Item::Italic(vec![t("b")]), t(" c")])]);
        // WhatsApp treats `**a**` as bold `a` between literal stars
        assert_eq!(items("**a**"), [t("*"), Item::Bold(vec![t("a")]), t("*")]);
    }

    #[test]
    fn unmatched_markers_stay_literal() {
        assert_eq!(items("*a"), [t("*a")]);
        assert_eq!(items("a_"), [t("a_")]);
        assert_eq!(items("*a _b*"), [Item::Bold(vec![t("a _b")])]);
        assert_eq!(items("~"), [t("~")]);
    }

    #[test]
    fn no_formatting_across_lines() {
        let l = parse("*a\nb*");
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].items, [t("*a")]);
        assert_eq!(l[1].items, [t("b*")]);
    }

    #[test]
    fn inline_code_is_verbatim() {
        assert_eq!(items("`*a* _b_`"), [Item::Code("*a* _b_".into())]);
    }

    #[test]
    fn mono_blocks_are_verbatim() {
        assert_eq!(items("```*a* _b_```"), [Item::Mono("*a* _b_".into())]);
        let l = parse("x\n```\nfn a() {\n  *b*\n}\n```\ny");
        assert_eq!(l.len(), 3);
        assert_eq!(l[1].items, [Item::Mono("fn a() {\n  *b*\n}".into())]);
        assert_eq!(l[2].items, [t("y")]);
        assert_eq!(items("a ```b``` c"), [t("a "), Item::Mono("b".into()), t(" c")]);
        assert_eq!(items("```unclosed"), [t("```unclosed")]);
        assert_eq!(items("``````"), [t("``````")]);
    }

    #[test]
    fn quotes_and_lists() {
        let l = parse("> *q*\n- item\n* star\n1. one\n>not quote");
        assert!(l[0].quote);
        assert_eq!(l[0].items, [Item::Bold(vec![t("q")])]);
        assert!(!l[1].quote);
        assert_eq!(l[1].items, [t("- item")]);
        assert_eq!(l[2].items, [t("* star")]);
        assert_eq!(l[3].items, [t("1. one")]);
        assert!(!l[4].quote);
    }

    #[test]
    fn star_bullet_with_bold_item() {
        assert_eq!(items("* *a*"), [t("* "), Item::Bold(vec![t("a")])]);
    }

    #[test]
    fn links_are_not_formatted_inside_and_work_inside_formatting() {
        assert_eq!(items("https://x.com/_a_b_"), [t("https://x.com/_a_b_")]);
        let md = markdown("*https://example.com/a*").unwrap_or_default();
        assert_eq!(md, "**[https\\:\\/\\/example\\.com\\/a](<https://example.com/a>)**");
    }

    #[test]
    fn persian_rtl_and_emoji() {
        assert_eq!(items("*سلام*"), [Item::Bold(vec![t("سلام")])]);
        assert_eq!(items("سلام *دنیا* خوبی"), [t("سلام "), Item::Bold(vec![t("دنیا")]), t(" خوبی")]);
        // Persian letters and digits count as word characters
        assert_eq!(items("س*ل*م"), [t("س*ل*م")]);
        assert_eq!(items("۲*۳*۴"), [t("۲*۳*۴")]);
        assert_eq!(items("🙂*hi*🙂"), [t("🙂"), Item::Bold(vec![t("hi")]), t("🙂")]);
        assert_eq!(items("*🙂*"), [Item::Bold(vec![t("🙂")])]);
    }

    #[test]
    fn strip_removes_markers_only() {
        assert_eq!(strip("*a* _b_ ~c~ `d`"), "a b c d");
        assert_eq!(strip("2*3*4 and snake_case"), "2*3*4 and snake_case");
        assert_eq!(strip("> quote\n- item"), "quote\n- item");
        assert_eq!(strip("```*x*```"), "*x*");
        assert_eq!(strip("plain"), "plain");
        assert_eq!(strip("*a"), "*a");
    }

    #[test]
    fn markdown_is_none_for_plain_text() {
        assert_eq!(markdown("hello 2*3*4 https://a.com"), None);
        assert!(styled_text("hello 2*3*4").is_none());
        assert!(styled_text("hello 2*3*4 https://a.com").is_some());
        assert_eq!(markdown(""), None);
    }

    #[test]
    fn markdown_shapes() {
        assert_eq!(markdown("*a* _b_ ~c~").as_deref(), Some("**a** *b* ~~c~~"));
        assert_eq!(markdown("a\n*b*").as_deref(), Some("a\\\n**b**"));
        assert_eq!(markdown("```x\ny```").as_deref(), Some("`x`\\\n`y`"));
        assert_eq!(markdown("*- a*").as_deref(), Some("**\\- a**"));
        assert_eq!(markdown("  *a*").as_deref(), Some("\u{a0}\u{a0}**a**"));
        assert!(markdown("> q").is_some_and(|m| m.starts_with("<font")));
    }

    #[test]
    fn markdown_parses_into_styled_text() {
        for s in [
            "*a* _b_ ~c~ `d` ```e``` > f\n> *g*\n- h\n1. i www.example.com +98 912 345 6789",
            "```a`b```",
            "`a b` `c`",
            "*<b>&amp;*",
        ] {
            assert!(styled_text(s).is_some(), "{s}");
        }
    }

    #[test]
    fn very_long_lines_stay_plain() {
        let s = format!("*{}*", "a".repeat(MAX_FORMATTED_LINE));
        assert_eq!(items(&s), [Item::Text(s.clone())]);
    }

    #[test]
    fn crlf_and_trailing_newlines() {
        assert_eq!(parse("a\r\nb\n\n").len(), 2);
    }

    fn ed(text: &str, a: usize, c: usize, k: Kind) -> (String, usize, usize) {
        let e = apply(text, a, c, k);
        (e.text, e.anchor, e.cursor)
    }

    #[test]
    fn wrap_selection() {
        assert_eq!(ed("hello world", 6, 11, Kind::Bold), ("hello *world*".into(), 7, 12));
        assert_eq!(ed("hello world", 0, 5, Kind::Italic), ("_hello_ world".into(), 1, 6));
        assert_eq!(ed("a b", 0, 3, Kind::Mono), ("```a b```".into(), 3, 6));
        // backwards selection
        assert_eq!(ed("hello world", 11, 6, Kind::Strike), ("hello ~world~".into(), 7, 12));
    }

    #[test]
    fn blanks_stay_outside_the_markers() {
        assert_eq!(ed("a  b  c", 1, 6, Kind::Bold), ("a  *b*  c".into(), 4, 5));
    }

    #[test]
    fn toggle_unwraps() {
        // markers inside the selection
        assert_eq!(ed("*hi*", 0, 4, Kind::Bold), ("hi".into(), 0, 2));
        // markers just outside the selection
        assert_eq!(ed("*hi*", 1, 3, Kind::Bold), ("hi".into(), 0, 2));
        assert_eq!(ed("```x```", 3, 4, Kind::Mono), ("x".into(), 0, 1));
        // another marker wraps instead
        assert_eq!(ed("*hi*", 1, 3, Kind::Italic), ("*_hi_*".into(), 2, 4));
    }

    #[test]
    fn no_selection_inserts_a_pair() {
        assert_eq!(ed("ab", 1, 1, Kind::Bold), ("a**b".into(), 2, 2));
        assert_eq!(ed("", 0, 0, Kind::Mono), ("``````".into(), 3, 3));
        assert_eq!(ed("**", 1, 1, Kind::Bold), ("".into(), 0, 0));
    }

    #[test]
    fn whitespace_only_selection_inserts_a_pair() {
        assert_eq!(ed("a b", 1, 2, Kind::Italic), ("a __b".into(), 3, 3));
    }

    #[test]
    fn multibyte_offsets_are_clamped() {
        // 'é' is two bytes; an offset inside it moves back
        let e = apply("é", 1, 2, Kind::Bold);
        assert_eq!(e.text, "*é*");
    }

    #[test]
    fn quote_prefixes_and_unprefixes_lines() {
        assert_eq!(ed("a\nb\nc", 0, 3, Kind::Quote), ("> a\n> b\nc".into(), 0, 7));
        assert_eq!(ed("> a\n> b\nc", 0, 7, Kind::Quote), ("a\nb\nc".into(), 0, 3));
        // mixed: all get the prefix
        assert_eq!(ed("> a\nb", 0, 5, Kind::Quote), ("> > a\n> b".into(), 0, 9));
        // no selection: the cursor's line, cursor follows the text
        assert_eq!(ed("ab\ncd", 4, 4, Kind::Quote), ("ab\n> cd".into(), 6, 6));
        assert_eq!(ed("> ab", 3, 3, Kind::Quote), ("ab".into(), 1, 1));
        // a selection ending right after a line break leaves the next line alone
        assert_eq!(ed("a\nb", 0, 2, Kind::Quote), ("> a\nb".into(), 0, 3));
    }

    #[test]
    fn apply_output_parses_back() {
        let e = apply("say hi now", 4, 6, Kind::Bold);
        assert_eq!(items(&e.text), [t("say "), Item::Bold(vec![t("hi")]), t(" now")]);
    }
}
