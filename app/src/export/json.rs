//! The JSON export: one array of messages with a fixed set of fields (see the
//! README for the schema). Written one element at a time.

use std::io::{self, Write};

use serde_json::{json, Value};

use super::{Entry, Head, Writer};

pub struct JsonWriter<W: Write> {
    out: W,
    first: bool,
}

impl<W: Write> JsonWriter<W> {
    pub fn new(out: W) -> Self {
        JsonWriter { out, first: true }
    }
}

pub fn entry_json(e: &Entry) -> Value {
    json!({
        "id": e.id,
        "ts": e.iso,
        "sender": e.sender,
        "sender_jid": e.sender_jid,
        "from_me": e.from_me,
        "kind": e.kind,
        "notice": e.notice,
        "text": e.text,
        "media": e.media.as_ref().and_then(|m| m.file.as_deref()),
        "quoted_id": e.quoted_id,
        "reactions": e.reactions.iter().map(|(emoji, count)| json!({ "emoji": emoji, "count": count })).collect::<Vec<_>>(),
        "edited": e.edited,
        "starred": e.starred,
        "poll": e.poll.as_ref().map(|p| json!({
            "question": p.question,
            "multiple": p.multi,
            "options": p.options.iter().map(|(name, votes)| json!({ "name": name, "votes": votes })).collect::<Vec<_>>(),
        })),
        "location": e.location.map(|(lat, lon)| json!({ "lat": lat, "lon": lon })),
        "contacts": e.contacts.iter().map(|(name, phone)| json!({ "name": name, "phone": phone })).collect::<Vec<_>>(),
    })
}

impl<W: Write> Writer for JsonWriter<W> {
    fn start(&mut self, _head: &Head) -> io::Result<()> {
        writeln!(self.out, "[")
    }

    fn entry(&mut self, e: &Entry) -> io::Result<()> {
        if !self.first {
            writeln!(self.out, ",")?;
        }
        self.first = false;
        let text = serde_json::to_string_pretty(&entry_json(e)).map_err(io::Error::other)?;
        write!(self.out, "{text}")
    }

    fn finish(&mut self, _total: u64) -> io::Result<()> {
        writeln!(self.out, "\n]")?;
        self.out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{entry, row};
    use super::*;

    fn head() -> Head {
        Head { title: "Ann".into(), is_group: false, exported_at: String::new(), range_text: String::new() }
    }

    #[test]
    fn output_is_a_valid_array_with_stable_fields() {
        let mut buf = Vec::new();
        let mut w = JsonWriter::new(&mut buf);
        w.start(&head()).expect("start");
        let mut a = entry(&row("a", "he said \"hi\" <b>\nnext"));
        a.starred = true;
        a.reactions = vec![("👍".into(), 2)];
        w.entry(&a).expect("entry");
        w.entry(&entry(&row("b", "second"))).expect("entry");
        w.finish(2).expect("finish");
        let v: Value = serde_json::from_slice(&buf).expect("valid json");
        let items = v.as_array().expect("array");
        assert_eq!(items.len(), 2);
        let first = &items[0];
        for key in ["id", "ts", "sender", "sender_jid", "from_me", "kind", "text", "media", "quoted_id", "reactions", "edited", "starred", "notice"] {
            assert!(first.get(key).is_some(), "missing {key}");
        }
        assert_eq!(first["text"], "he said \"hi\" <b>\nnext");
        assert_eq!(first["starred"], true);
        assert_eq!(first["reactions"][0]["count"], 2);
        assert!(first["media"].is_null());
        assert_eq!(items[1]["id"], "b");
    }

    #[test]
    fn an_empty_chat_is_an_empty_array() {
        let mut buf = Vec::new();
        let mut w = JsonWriter::new(&mut buf);
        w.start(&head()).expect("start");
        w.finish(0).expect("finish");
        let v: Value = serde_json::from_slice(&buf).expect("valid json");
        assert_eq!(v.as_array().map(Vec::len), Some(0));
    }
}
