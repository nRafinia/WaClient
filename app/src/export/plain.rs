//! The plain-text export, laid out like WhatsApp's own "Export chat".

use std::io::{self, Write};

use super::util::text_line;
use super::{Entry, Head, Writer};

pub struct TextWriter<W: Write> {
    out: W,
}

impl<W: Write> TextWriter<W> {
    pub fn new(out: W) -> Self {
        TextWriter { out }
    }
}

/// The lines after the first line of a message's body.
fn body(e: &Entry) -> String {
    let mut lines: Vec<String> = Vec::new();
    if let Some(m) = &e.media {
        match (&m.file, m.missing) {
            (Some(f), _) => lines.push(format!("<attached: {}>", f.rsplit('/').next().unwrap_or(f))),
            _ => lines.push("<Media omitted>".to_string()),
        }
    }
    if let Some(p) = &e.poll {
        lines.push(format!("POLL: {}", p.question));
        for (name, votes) in &p.options {
            lines.push(format!("OPTION: {name} ({votes})"));
        }
    }
    if let Some((lat, lon)) = e.location {
        lines.push(format!("Location: {lat:.5}, {lon:.5}"));
    }
    for (name, phone) in &e.contacts {
        lines.push(if phone.is_empty() { format!("Contact: {name}") } else { format!("Contact: {name} ({phone})") });
    }
    if let Some(t) = &e.text {
        lines.push(t.clone());
    }
    if e.edited {
        lines.push("<This message was edited>".to_string());
    }
    if e.starred {
        lines.push("<Starred>".to_string());
    }
    if !e.reactions.is_empty() {
        let r: Vec<String> = e.reactions.iter().map(|(emoji, n)| format!("{emoji} {n}")).collect();
        lines.push(format!("Reactions: {}", r.join("  ")));
    }
    if lines.is_empty() {
        lines.push("<Unsupported message>".to_string());
    }
    lines.join("\n")
}

impl<W: Write> Writer for TextWriter<W> {
    fn start(&mut self, _head: &Head) -> io::Result<()> {
        Ok(())
    }

    fn entry(&mut self, e: &Entry) -> io::Result<()> {
        let sender = (!e.notice).then_some(e.sender.as_str());
        writeln!(self.out, "{}", text_line(e.when, sender, &body(e)))
    }

    fn finish(&mut self, _total: u64) -> io::Result<()> {
        self.out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{entry, row};
    use super::super::MediaRef;
    use super::*;

    fn render(entries: &[Entry]) -> String {
        let mut buf = Vec::new();
        let mut w = TextWriter::new(&mut buf);
        for e in entries {
            w.entry(e).expect("write");
        }
        w.finish(entries.len() as u64).expect("finish");
        String::from_utf8(buf).expect("utf8")
    }

    #[test]
    fn messages_become_whatsapp_lines() {
        let a = entry(&row("a", "hello\nsecond line"));
        let mut b = entry(&row("b", "x"));
        b.from_me = true;
        b.sender = "You".into();
        let n = entry(&row("GRP-1-0", "Ann added Bob"));
        assert_eq!(
            render(&[a, b, n]),
            "07/03/2026, 09:05 - Ann: hello\nsecond line\n07/03/2026, 09:05 - You: x\n07/03/2026, 09:05 - Ann added Bob\n"
        );
    }

    #[test]
    fn media_is_attached_or_omitted() {
        let mut e = entry(&row("m", "caption"));
        e.media = Some(MediaRef { label: "Photo".into(), file: Some("media/00001-a.jpg".into()), missing: false, inline: true, mime: None });
        assert_eq!(render(&[e.clone()]), "07/03/2026, 09:05 - Ann: <attached: 00001-a.jpg>\ncaption\n");
        e.media = Some(MediaRef { label: "Photo".into(), file: None, missing: true, inline: false, mime: None });
        assert_eq!(render(&[e]), "07/03/2026, 09:05 - Ann: <Media omitted>\ncaption\n");
    }

    #[test]
    fn polls_locations_and_markers_are_spelled_out() {
        let mut e = entry(&row("p", ""));
        e.text = None;
        e.poll = Some(super::super::PollEntry { question: "Lunch?".into(), options: vec![("Pizza".into(), 2)], multi: false });
        e.location = Some((35.7, 51.4));
        e.edited = true;
        e.reactions = vec![("👍".into(), 2)];
        let out = render(&[e]);
        assert!(out.contains("POLL: Lunch?\nOPTION: Pizza (2)\nLocation: 35.70000, 51.40000\n<This message was edited>\nReactions: 👍 2"));
    }
}
