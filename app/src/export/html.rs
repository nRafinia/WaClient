//! The HTML export: one self-contained page with inline CSS (light and dark by
//! the viewer's system setting). Every piece of chat text goes through
//! `html_escape` or `linkify`; nothing from a message is ever written raw.

use std::io::{self, Write};

use chrono::NaiveDate;

use super::util::{day_title, html_escape, linkify, safe_href, url_path};
use super::{Entry, Head, Writer};

const CSS: &str = "\
:root{--bg:#efeae2;--in:#fff;--out:#d9fdd3;--text:#111b21;--sub:#667781;--note:#fff3c4;--chip:#0000000d;--link:#027eb5}\
@media (prefers-color-scheme:dark){:root{--bg:#0b141a;--in:#202c33;--out:#005c4b;--text:#e9edef;--sub:#8696a0;--note:#182229;--chip:#ffffff14;--link:#53bdeb}}\
*{box-sizing:border-box}\
body{margin:0;background:var(--bg);color:var(--text);font:15px/1.4 'Segoe UI',system-ui,sans-serif}\
header{padding:14px 20px;background:var(--in);border-bottom:1px solid var(--chip);position:sticky;top:0}\
h1{margin:0;font-size:18px}header p{margin:2px 0 0;color:var(--sub);font-size:13px}\
main{max-width:820px;margin:0 auto;padding:12px 12px 40px}\
.day,.note{margin:14px auto 8px;width:fit-content;max-width:90%;padding:4px 12px;border-radius:8px;background:var(--note);color:var(--sub);font-size:12.5px;text-align:center}\
.msg{display:flex;margin:3px 0}.msg.out{justify-content:flex-end}\
.bubble{max-width:75%;padding:6px 9px 4px;border-radius:9px;background:var(--in);box-shadow:0 1px 1px #0002;overflow-wrap:anywhere}\
.out .bubble{background:var(--out)}\
.who{font-size:13px;font-weight:600;color:var(--link)}\
.text{white-space:pre-wrap}\
.quote{margin:2px 0 4px;padding:3px 8px;border-left:3px solid var(--link);border-radius:4px;background:var(--chip);color:var(--sub);font-size:13px;white-space:pre-wrap}\
.meta{margin-top:2px;text-align:right;color:var(--sub);font-size:11.5px}\
.react{display:inline-block;margin-top:3px;padding:1px 7px;border-radius:10px;background:var(--chip);font-size:13px}\
.media img{display:block;max-width:100%;max-height:420px;border-radius:6px;margin-bottom:3px}\
.media,.card{margin:2px 0}.card{padding:5px 8px;border-radius:6px;background:var(--chip)}\
.opt{display:flex;justify-content:space-between;gap:16px}\
a{color:var(--link)}\
footer{margin-top:24px;text-align:center;color:var(--sub);font-size:12px}";

pub struct HtmlWriter<W: Write> {
    out: W,
    day: Option<NaiveDate>,
    /// Sender names are only shown in groups.
    group: bool,
}

impl<W: Write> HtmlWriter<W> {
    pub fn new(out: W) -> Self {
        HtmlWriter { out, day: None, group: false }
    }
}

/// The inner HTML of the body of a message (media, cards and text).
fn body(e: &Entry) -> String {
    let mut h = String::new();
    if let Some(m) = &e.media {
        h.push_str("<div class=\"media\">");
        match (&m.file, m.inline) {
            (Some(f), true) => {
                let src = html_escape(&url_path(f));
                h.push_str(&format!("<a href=\"{src}\"><img src=\"{src}\" alt=\"{}\" loading=\"lazy\"></a>", html_escape(&m.label)));
            }
            (Some(f), false) => {
                h.push_str(&format!("<a href=\"{}\" download rel=\"noopener noreferrer\">{}</a>", html_escape(&url_path(f)), html_escape(&m.label)));
            }
            (None, _) => {
                h.push_str(&html_escape(&m.label));
                if m.missing {
                    h.push_str(" <em>(not downloaded)</em>");
                }
            }
        }
        h.push_str("</div>");
    }
    if let Some(p) = &e.poll {
        h.push_str(&format!("<div class=\"card\"><b>{}</b>", html_escape(&p.question)));
        h.push_str(if p.multi { "<div class=\"meta\" style=\"text-align:left\">Select one or more</div>" } else { "" });
        for (name, votes) in &p.options {
            h.push_str(&format!("<div class=\"opt\"><span>{}</span><span>{votes}</span></div>", html_escape(name)));
        }
        h.push_str("</div>");
    }
    if let Some((lat, lon)) = e.location {
        let coords = format!("{lat:.5}, {lon:.5}");
        let map = format!("https://www.openstreetmap.org/?mlat={lat:.5}&mlon={lon:.5}#map=16/{lat:.5}/{lon:.5}");
        h.push_str(&format!("<div class=\"card\">Location: {} <a href=\"{}\" rel=\"noopener noreferrer\">map</a></div>", html_escape(&coords), html_escape(&map)));
    }
    for (name, phone) in &e.contacts {
        h.push_str(&format!("<div class=\"card\">Contact: <b>{}</b> {}</div>", html_escape(name), html_escape(phone)));
    }
    if let Some((url, title)) = &e.link {
        let label = if title.is_empty() { url.as_str() } else { title.as_str() };
        match safe_href(url) {
            Some(href) => h.push_str(&format!("<div class=\"card\"><a href=\"{}\" rel=\"noopener noreferrer\">{}</a></div>", html_escape(href), html_escape(label))),
            None => h.push_str(&format!("<div class=\"card\">{}</div>", html_escape(label))),
        }
    }
    if let Some(t) = &e.text {
        h.push_str(&format!("<div class=\"text\" dir=\"auto\">{}</div>", linkify(t)));
    }
    h
}

impl<W: Write> Writer for HtmlWriter<W> {
    fn start(&mut self, head: &Head) -> io::Result<()> {
        self.group = head.is_group;
        let title = html_escape(&head.title);
        let kind = if head.is_group { "Group" } else { "Chat" };
        write!(
            self.out,
            "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
             <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\n\
             <title>{title}</title>\n<style>{CSS}</style>\n</head>\n<body>\n\
             <header><h1 dir=\"auto\">{title}</h1><p>{kind} export &middot; {} &middot; exported {}</p></header>\n<main>\n",
            html_escape(&head.range_text),
            html_escape(&head.exported_at),
        )
    }

    fn entry(&mut self, e: &Entry) -> io::Result<()> {
        let day = e.when.date();
        if self.day != Some(day) {
            self.day = Some(day);
            writeln!(self.out, "<div class=\"day\">{}</div>", html_escape(&day_title(day)))?;
        }
        if e.notice {
            return writeln!(self.out, "<div class=\"note\" dir=\"auto\">{}</div>", linkify(e.text.as_deref().unwrap_or_default()));
        }
        let side = if e.from_me { "out" } else { "in" };
        let mut h = format!("<div class=\"msg {side}\"><div class=\"bubble\">");
        if self.group && !e.from_me && !e.sender.is_empty() {
            h.push_str(&format!("<div class=\"who\" dir=\"auto\">{}</div>", html_escape(&e.sender)));
        }
        if let Some(q) = &e.quoted_text {
            h.push_str(&format!("<div class=\"quote\" dir=\"auto\">{}</div>", html_escape(q)));
        }
        h.push_str(&body(e));
        let mut meta = Vec::new();
        if e.starred {
            meta.push("&#9733; starred".to_string());
        }
        if e.edited {
            meta.push("edited".to_string());
        }
        meta.push(html_escape(&e.when.format("%H:%M").to_string()));
        h.push_str(&format!("<div class=\"meta\">{}</div>", meta.join(" &middot; ")));
        if !e.reactions.is_empty() {
            let chips: Vec<String> = e
                .reactions
                .iter()
                .map(|(emoji, n)| format!("<span class=\"react\">{}{}</span>", html_escape(emoji), if *n > 1 { format!(" {n}") } else { String::new() }))
                .collect();
            h.push_str(&format!("<div>{}</div>", chips.join(" ")));
        }
        h.push_str("</div></div>");
        writeln!(self.out, "{h}")
    }

    fn finish(&mut self, total: u64) -> io::Result<()> {
        writeln!(self.out, "</main>\n<footer>{total} messages</footer>\n</body>\n</html>")?;
        self.out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{entry, row};
    use super::super::MediaRef;
    use super::*;

    fn head() -> Head {
        Head { title: "A <b>chat</b>".into(), is_group: true, exported_at: "2026-03-08 10:00".into(), range_text: "All messages".into() }
    }

    fn render(entries: &[Entry]) -> String {
        let mut buf = Vec::new();
        let mut w = HtmlWriter::new(&mut buf);
        w.start(&head()).expect("start");
        for e in entries {
            w.entry(e).expect("entry");
        }
        w.finish(entries.len() as u64).expect("finish");
        String::from_utf8(buf).expect("utf8")
    }

    #[test]
    fn message_text_is_escaped_everywhere() {
        let mut m = row("a", "<script>alert(1)</script> <img src=x onerror=alert(2)>");
        m.sender_name = Some("<i>Mallory</i>".into());
        m.quoted_preview = Some("<script>quote</script>".into());
        let mut e = entry(&m);
        e.reactions = vec![("<u>".into(), 2)];
        e.media = Some(MediaRef { label: "<b>doc</b>".into(), file: None, missing: true, inline: false, mime: None });
        let out = render(&[e]);
        assert!(!out.contains("<script>"), "{out}");
        assert!(!out.contains("<img src=x"));
        assert!(!out.contains("<i>Mallory"));
        assert!(!out.contains("<u>"));
        assert!(!out.contains("<b>doc"));
        assert!(!out.contains("<b>chat</b>"));
        assert!(out.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(out.contains("(not downloaded)"));
    }

    #[test]
    fn sender_names_show_in_groups_only() {
        let e = entry(&row("a", "hi"));
        assert!(render(&[e.clone()]).contains("<div class=\"who\" dir=\"auto\">Ann</div>"));
        let mut buf = Vec::new();
        let mut w = HtmlWriter::new(&mut buf);
        let mut h = head();
        h.is_group = false;
        w.start(&h).expect("start");
        w.entry(&e).expect("entry");
        assert!(!String::from_utf8(buf).expect("utf8").contains("class=\"who\""));
    }

    #[test]
    fn only_safe_links_become_anchors() {
        let out = render(&[entry(&row("a", "https://example.com/x javascript:alert(1) data:text/html,x"))]);
        assert!(out.contains("<a href=\"https://example.com/x\""));
        assert!(!out.contains("href=\"javascript"));
        assert!(!out.contains("href=\"data"));
        assert!(out.contains("javascript:alert(1)"));
    }

    #[test]
    fn layout_has_days_sides_direction_and_markers() {
        let a = entry(&row("a", "سلام"));
        let mut b = entry(&row("b", "hi"));
        b.from_me = true;
        b.edited = true;
        b.starred = true;
        let mut c = entry(&row("c", "next day"));
        c.when = c.when + chrono::Duration::days(1);
        let n = entry(&row("GRP-1-0", "Ann added Bob"));
        let out = render(&[a, b, n, c]);
        assert_eq!(out.matches("class=\"day\"").count(), 2);
        assert!(out.contains("7 March 2026"));
        assert!(out.contains("class=\"msg in\""));
        assert!(out.contains("class=\"msg out\""));
        assert!(out.contains("dir=\"auto\">سلام</div>"));
        assert!(out.contains("starred") && out.contains("edited"));
        assert!(out.contains("class=\"note\""));
        assert!(out.contains("prefers-color-scheme:dark"));
        assert!(out.contains("<footer>4 messages</footer>"));
    }

    #[test]
    fn copied_images_are_inline_and_other_media_are_links() {
        let mut img = entry(&row("a", ""));
        img.media = Some(MediaRef { label: "Photo".into(), file: Some("media/00001-a b.jpg".into()), missing: false, inline: true, mime: None });
        let mut doc = entry(&row("b", ""));
        doc.media = Some(MediaRef { label: "report.pdf".into(), file: Some("media/00002-report.pdf".into()), missing: false, inline: false, mime: None });
        let out = render(&[img, doc]);
        assert!(out.contains("<img src=\"media/00001-a%20b.jpg\""));
        assert!(out.contains("<a href=\"media/00002-report.pdf\" download rel=\"noopener noreferrer\">report.pdf</a>"));
    }

    #[test]
    fn polls_locations_and_contacts_render_as_cards() {
        let mut e = entry(&row("a", ""));
        e.text = None;
        e.poll = Some(super::super::PollEntry { question: "Lunch?".into(), options: vec![("Pizza".into(), 3)], multi: true });
        e.location = Some((35.7, 51.4));
        e.contacts = vec![("Carol".into(), "+491511".into())];
        let out = render(&[e]);
        assert!(out.contains("<b>Lunch?</b>") && out.contains("<span>Pizza</span><span>3</span>"));
        assert!(out.contains("35.70000, 51.40000") && out.contains("openstreetmap.org"));
        assert!(out.contains("Contact: <b>Carol</b> +491511"));
    }
}
