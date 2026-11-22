//! Debug-only demo data (WACLIENT_DEMO=1): an in-memory store with a few
//! chats so the main screen can be checked without a paired account.

use std::io::Cursor;
use std::sync::Arc;

use crate::audio;
use wa_store::{
    ChatKind, ContactPatch, MessageKind, MessageStatus, NewMessage, STATUS_JID, Store,
};

const ME: &str = "1000@s.whatsapp.net";

fn msg(chat: &str, id: &str, sender: &str, ts: i64, text: &str) -> NewMessage {
    NewMessage {
        chat_jid: chat.into(),
        id: id.into(),
        sender_jid: sender.into(),
        from_me: sender == ME,
        ts,
        kind: MessageKind::Text,
        text: Some(text.into()),
        quoted_id: None,
        quoted_preview: None,
        media_mime: None,
        media_name: None,
        media_size: None,
        media_duration: None,
        thumbnail: None,
        local_path: None,
        raw: None,
        link_url: None,
        link_title: None,
        link_desc: None,
        link_thumb: None,
        ..Default::default()
    }
}

fn thumbnail() -> Option<Vec<u8>> {
    let img = image::RgbImage::from_fn(96, 64, |x, y| image::Rgb([(x * 2) as u8, (y * 4) as u8, 160]));
    let mut out = Vec::new();
    img.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png).ok()?;
    Some(out)
}

/// A round cartoon face on a transparent background: colour `hue` and mouth `mood`.
fn sticker_face(hue: usize, mood: usize) -> image::RgbaImage {
    const SIZE: u32 = 192;
    const PALETTE: [[u8; 3]; 8] = [
        [255, 196, 61],
        [255, 120, 100],
        [110, 200, 120],
        [100, 160, 240],
        [190, 130, 230],
        [250, 160, 200],
        [120, 220, 220],
        [245, 140, 60],
    ];
    let [r, g, b] = PALETTE[hue % PALETTE.len()];
    let dark = image::Rgba([60, 40, 30, 255]);
    image::RgbaImage::from_fn(SIZE, SIZE, |x, y| {
        let (dx, dy) = (x as f32 - 96.0, y as f32 - 96.0);
        let radius = (dx * dx + dy * dy).sqrt();
        if radius > 88.0 {
            return image::Rgba([0, 0, 0, 0]);
        }
        if radius > 80.0 {
            return dark;
        }
        let eye = ((dx.abs() - 30.0).powi(2) + (dy + 22.0).powi(2)).sqrt() < 11.0;
        let arc = |cy: f32| ((dx * dx + (dy - cy).powi(2)).sqrt() - 38.0).abs() < 4.5;
        let mouth = match mood % 4 {
            0 => arc(5.0) && dy > 14.0,
            1 => (dx / 22.0).powi(2) + ((dy - 30.0) / 16.0).powi(2) < 1.0,
            2 => (dy - 32.0).abs() < 4.0 && dx.abs() < 26.0,
            _ => arc(66.0) && dy < 40.0,
        };
        if eye || mouth { dark } else { image::Rgba([r, g, b, 255]) }
    })
}

/// Eight stickers (WebP, like real ones) in Bob's chat, long ago so the chat
/// itself looks as before; the first three are favourites.
fn seed_stickers(
    store: &Store,
    dir: &std::path::Path,
    chat: &str,
    now: i64,
) -> Result<(), Box<dyn std::error::Error>> {
    use sha2::{Digest, Sha256};
    for n in 0..8usize {
        let path = dir.join(format!("sticker-{n}.webp"));
        sticker_face(n, n / 2).save(&path)?;
        let bytes = std::fs::read(&path)?;
        let mut s = msg(chat, &format!("stk{n}"), if n % 3 == 0 { ME } else { chat }, now - 40 * 86_400 + n as i64, "");
        s.text = None;
        s.kind = MessageKind::Sticker;
        s.media_mime = Some("image/webp".into());
        s.media_size = Some(bytes.len() as i64);
        s.local_path = Some(path.to_string_lossy().into_owned());
        store.insert_message(&s, false)?;
        if n < 3 {
            let sha: String = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
            store.add_sticker_fav(&sha, chat, &s.id, now - n as i64)?;
        }
    }
    Ok(())
}

fn gradient(w: u32, h: u32) -> image::RgbImage {
    image::RgbImage::from_fn(w, h, |x, y| {
        image::Rgb([(x * 255 / w) as u8, (y * 255 / h) as u8, 140])
    })
}

pub fn store() -> Result<Arc<Store>, Box<dyn std::error::Error>> {
    let store = Store::open_in_memory()?;
    let now = chrono::Utc::now().timestamp();
    let day = 86_400;

    let alice = "111@s.whatsapp.net";
    let bob = "222@s.whatsapp.net";
    let carol = "333@s.whatsapp.net";
    let group = "999@g.us";

    store.upsert_chat(alice, ChatKind::Dm, Some("Alice Johnson"))?;
    store.upsert_chat(bob, ChatKind::Dm, Some("Bob"))?;
    store.upsert_chat(group, ChatKind::Group, Some("Family Group"))?;
    store.upsert_chat("444@s.whatsapp.net", ChatKind::Dm, Some("Old chat"))?;
    for (jid, name) in [(alice, "Alice Johnson"), (bob, "Bob"), (carol, "Carol Smith")] {
        store.upsert_contact(&ContactPatch {
            jid: jid.into(),
            full_name: Some(name.into()),
            push_name: None,
            phone: None,
        })?;
    }

    // Alice: every status, a quoted reply, media with and without thumbnail.
    let m = msg(alice, "a1", alice, now - 2 * day, "Hey! Are we still on for Friday?");
    store.insert_message(&m, false)?;
    let m = msg(alice, "a2", ME, now - day, "Yes, see you at 7. Bringing the long message test: the quick brown fox jumps over the lazy dog, again and again until the bubble has to wrap onto several lines.");
    store.insert_message(&m, false)?;
    for (i, st) in [
        MessageStatus::Pending,
        MessageStatus::Sent,
        MessageStatus::Delivered,
        MessageStatus::Read,
        MessageStatus::Played,
        MessageStatus::Failed,
    ]
    .into_iter()
    .enumerate()
    {
        let id = format!("s{i}");
        store.insert_message(&msg(alice, &id, ME, now - 3600 + i as i64, &format!("status {st:?}")), false)?;
        store.update_status(alice, &id, st)?;
    }
    // Recent, delivered and ours: still editable (15 minutes) and has receipts.
    store.insert_message(&msg(alice, "a7", ME, now - 120, "See you at seven then"), false)?;
    store.update_status(alice, "a7", MessageStatus::Delivered)?;
    let mut q = msg(alice, "a3", alice, now - 600, "Great, thanks");
    q.quoted_id = Some("a2".into());
    q.quoted_preview = Some("Yes, see you at 7. Bringing the long message test".into());
    store.insert_message(&q, true)?;
    let mut img = msg(alice, "a4", alice, now - 300, "");
    img.text = None;
    img.kind = MessageKind::Image;
    img.media_name = Some("IMG_2041.jpg".into());
    img.media_size = Some(2_400_000);
    img.media_mime = Some("image/jpeg".into());
    store.insert_message(&img, true)?;
    let mut thumb = msg(alice, "a5", alice, now - 200, "With thumbnail and caption");
    thumb.kind = MessageKind::Image;
    thumb.thumbnail = thumbnail();
    thumb.media_size = Some(310_000);
    store.insert_message(&thumb, true)?;
    let mut doc = msg(alice, "a6", ME, now - 100, "");
    doc.text = None;
    doc.kind = MessageKind::Document;
    doc.media_name = Some("report.pdf".into());
    doc.media_size = Some(88_000);
    store.insert_message(&doc, false)?;

    // Media that exists on disk: voice notes, a document, a photo and a sticker.
    let dir = std::env::temp_dir().join("waclient-demo");
    std::fs::create_dir_all(&dir)?;
    let tone = dir.join("tone.ogg");
    std::fs::write(&tone, audio::synth_tone_ogg(440.0, 3.0)?)?;
    let doc_path = dir.join("Meeting notes.txt");
    std::fs::write(&doc_path, "Agenda\n1. Voice notes\n2. Documents\n")?;
    let photo = dir.join("photo.png");
    let sticker = dir.join("sticker.png");
    gradient(640, 420).save(&photo)?;
    gradient(256, 256).save(&sticker)?;

    let path_str = |p: &std::path::Path| Some(p.to_string_lossy().into_owned());
    let mut v = msg(alice, "a7", alice, now - 90, "");
    v.text = None;
    v.kind = MessageKind::Voice;
    v.media_duration = Some(3);
    v.media_mime = Some("audio/ogg; codecs=opus".into());
    v.local_path = path_str(&tone);
    store.insert_message(&v, false)?;
    let mut v = msg(alice, "a8", ME, now - 80, "");
    v.text = None;
    v.kind = MessageKind::Voice;
    v.media_duration = Some(3);
    v.local_path = path_str(&tone);
    store.insert_message(&v, false)?;
    store.update_status(alice, "a8", MessageStatus::Read)?;
    let mut v = msg(alice, "a9", alice, now - 70, "");
    v.text = None;
    v.kind = MessageKind::Voice;
    v.media_duration = Some(12);
    store.insert_message(&v, false)?;
    let mut d = msg(alice, "a10", alice, now - 60, "");
    d.text = None;
    d.kind = MessageKind::Document;
    d.media_name = Some("Meeting notes.txt".into());
    d.media_size = Some(std::fs::metadata(&doc_path)?.len() as i64);
    d.local_path = path_str(&doc_path);
    store.insert_message(&d, false)?;
    let mut d = msg(alice, "a11", alice, now - 55, "");
    d.text = None;
    d.kind = MessageKind::Document;
    d.media_name = Some("invoice.exe".into());
    d.media_size = Some(1_200);
    d.local_path = path_str(&doc_path.with_file_name("invoice.exe"));
    store.insert_message(&d, false)?;
    let mut p = msg(alice, "a12", alice, now - 50, "Downloaded photo");
    p.kind = MessageKind::Image;
    p.thumbnail = thumbnail();
    p.local_path = path_str(&photo);
    store.insert_message(&p, false)?;
    let mut s = msg(alice, "a13", ME, now - 40, "");
    s.text = None;
    s.kind = MessageKind::Sticker;
    s.local_path = path_str(&sticker);
    store.insert_message(&s, false)?;
    seed_stickers(&store, &dir, bob, now)?;
    let mut vid = msg(alice, "a14", alice, now - 30, "");
    vid.text = None;
    vid.kind = MessageKind::Video;
    vid.thumbnail = thumbnail();
    vid.media_size = Some(5_000_000);
    store.insert_message(&vid, false)?;
    store.set_chat_flags(alice, Some(true), None, None)?;
    // A chat whose timer the other person set: shows the avatar badge.
    store.set_chat_ephemeral(bob, 86_400, 1)?;

    // A poll with votes, and reactions on a few messages.
    let mut poll = msg(alice, "a15", alice, now - 20, "📊 Where should we eat on Friday?");
    poll.kind = MessageKind::Other;
    store.insert_message(&poll, false)?;
    let options: Vec<String> = ["Pizza place", "Sushi bar", "Home cooking"].map(String::from).to_vec();
    store.upsert_poll(alice, "a15", "Where should we eat on Friday?", &options, 1)?;
    store.upsert_poll_vote(alice, "a15", alice, &options[..1], now - 15, false)?;
    store.upsert_poll_vote(alice, "a15", ME, &options[..1], now - 14, true)?;
    store.upsert_poll_vote(alice, "a15", carol, &options[1..2], now - 13, false)?;
    store.upsert_poll_vote(alice, "a15", bob, &options[1..2], now - 12, false)?;
    store.upsert_poll_vote(alice, "a15", "444@s.whatsapp.net", &options[1..2], now - 11, false)?;
    // A multi-answer poll (selectable count 0 = no limit).
    let mut multi = msg(alice, "a15m", alice, now - 10, "📊 Which days work?");
    multi.kind = MessageKind::Other;
    store.insert_message(&multi, false)?;
    let days: Vec<String> = ["Monday", "Wednesday", "Saturday"].map(String::from).to_vec();
    store.upsert_poll(alice, "a15m", "Which days work?", &days, 0)?;
    store.upsert_poll_vote(alice, "a15m", ME, &days[..2], now - 9, true)?;
    store.upsert_poll_vote(alice, "a15m", carol, &days[1..], now - 8, false)?;
    // Quizzes: one we answered wrongly, one not answered yet.
    let mut quiz = msg(alice, "a15q", alice, now - 7, "📊 What is the capital of Australia?");
    quiz.kind = MessageKind::Other;
    store.insert_message(&quiz, false)?;
    let cities: Vec<String> = ["Sydney", "Canberra", "Melbourne"].map(String::from).to_vec();
    store.upsert_poll(alice, "a15q", "What is the capital of Australia?", &cities, 1)?;
    store.set_poll_correct(alice, "a15q", Some("Canberra"))?;
    store.upsert_poll_vote(alice, "a15q", ME, &cities[..1], now - 6, true)?;
    store.upsert_poll_vote(alice, "a15q", carol, &cities[1..2], now - 5, false)?;
    store.upsert_poll_vote(alice, "a15q", bob, &cities[..1], now - 4, false)?;
    let mut quiz2 = msg(alice, "a15r", alice, now - 3, "📊 Which planet is the largest?");
    quiz2.kind = MessageKind::Other;
    store.insert_message(&quiz2, false)?;
    let planets: Vec<String> = ["Earth", "Jupiter", "Mars"].map(String::from).to_vec();
    store.upsert_poll(alice, "a15r", "Which planet is the largest?", &planets, 1)?;
    store.set_poll_correct(alice, "a15r", Some("Jupiter"))?;
    store.upsert_poll_vote(alice, "a15r", carol, &planets[1..2], now - 2, false)?;
    // Right-to-left poll and quiz, and a Persian line the global search finds.
    let mut fa_poll = msg(alice, "a15p", alice, now - 2, "📊 ناهار جمعه کجا برویم؟");
    fa_poll.kind = MessageKind::Other;
    store.insert_message(&fa_poll, false)?;
    let fa_options: Vec<String> = ["پیتزا", "سوشی", "غذای خانگی"].map(String::from).to_vec();
    store.upsert_poll(alice, "a15p", "ناهار جمعه کجا برویم؟", &fa_options, 1)?;
    store.upsert_poll_vote(alice, "a15p", alice, &fa_options[..1], now - 2, false)?;
    store.upsert_poll_vote(alice, "a15p", ME, &fa_options[..1], now - 2, true)?;
    store.upsert_poll_vote(alice, "a15p", carol, &fa_options[1..2], now - 2, false)?;
    let mut fa_quiz = msg(alice, "a15f", alice, now - 1, "📊 پایتخت ایران کدام شهر است؟");
    fa_quiz.kind = MessageKind::Other;
    store.insert_message(&fa_quiz, false)?;
    let fa_cities: Vec<String> = ["اصفهان", "تهران", "شیراز"].map(String::from).to_vec();
    store.upsert_poll(alice, "a15f", "پایتخت ایران کدام شهر است؟", &fa_cities, 1)?;
    store.set_poll_correct(alice, "a15f", Some("تهران"))?;
    store.upsert_poll_vote(alice, "a15f", ME, &fa_cities[..1], now - 1, true)?;
    store.upsert_poll_vote(alice, "a15f", carol, &fa_cities[1..2], now - 1, false)?;
    store.insert_message(
        &msg(alice, "a16", alice, now - 3 * day, "این پیام برای آزمایش Message number و جست‌وجوی راست‌به‌چپ نوشته شده است"),
        false,
    )?;
    // An archived chat with unread messages, and a muted group.
    let old_chat = "555@s.whatsapp.net";
    store.upsert_chat(old_chat, ChatKind::Dm, Some("Dave (old project)"))?;
    store.insert_message(&msg(old_chat, "o1", old_chat, now - 3 * day, "Can you send the final invoice?"), true)?;
    store.insert_message(&msg(old_chat, "o2", old_chat, now - 3 * day + 60, "Ping, still waiting"), true)?;
    store.set_chat_flags(old_chat, None, Some(true), None)?;
    store.set_chat_flags(group, None, None, Some(i64::MAX))?;
    // Links: plain, with trailing punctuation and parentheses, e-mail, and RTL text.
    store.insert_message(
        &msg(
            alice,
            "a16",
            alice,
            now - 8,
            "Docs: https://example.com/docs/getting-started?lang=en (and www.example.org/a_very_long_path/that/keeps/going/and/going/for/wrapping.html), write to hi@example.com.",
        ),
        false,
    )?;
    store.insert_message(&msg(alice, "a17", ME, now - 6, "سلام، این لینک را ببین https://example.ir/page، ممنون"), false)?;
    // Shared contacts: one person, then several.
    let mut card = msg(alice, "a18", alice, now - 4, "👤 Maria Garcia
+34600123456");
    card.kind = MessageKind::Other;
    card.media_mime = Some("text/vcard".into());
    card.media_name = Some("Maria Garcia".into());
    store.insert_message(&card, false)?;
    let mut cards = msg(alice, "a19", ME, now - 2, "👤 Omar Khalil
+971501234567

👤 Li Wei
+8613800138000

👤 Sam
+15551234567");
    cards.kind = MessageKind::Other;
    cards.media_mime = Some("text/vcard".into());
    cards.media_name = Some("3 contacts".into());
    store.insert_message(&cards, false)?;
    // Link preview cards: one received with a picture, one sent without.
    let mut linked = msg(alice, "a20", alice, now - 1, "Look at this: https://www.example.com/articles/rust-in-production");
    linked.link_url = Some("https://www.example.com/articles/rust-in-production".into());
    linked.link_title = Some("Rust in production: lessons from two years of running it at scale".into());
    linked.link_desc = Some("What worked, what did not, and the three decisions we would take differently if we started again today.".into());
    linked.link_thumb = thumbnail();
    store.insert_message(&linked, false)?;
    let mut sent = msg(alice, "a21", ME, now, "https://example.org/changelog");
    sent.link_url = Some("https://example.org/changelog".into());
    sent.link_title = Some("Changelog".into());
    store.insert_message(&sent, false)?;
    // A shared location with a name and an address.
    let mut place = msg(alice, "a30", alice, now + 1, "📍 Eiffel Tower
Champ de Mars, 5 Avenue Anatole France, Paris");
    place.kind = MessageKind::Other;
    place.geo_lat = Some(48.85837);
    place.geo_lon = Some(2.29448);
    store.insert_message(&place, false)?;
    // Disappearing messages: the notice row, then messages that carry a timer.
    let mut notice = msg(alice, "EPH-1", alice, now + 2, "Disappearing messages were turned on — 7 days");
    notice.kind = MessageKind::Other;
    store.insert_message(&notice, false)?;
    let mut e1 = msg(alice, "e1", alice, now + 3, "This one will disappear");
    e1.expires_at = Some(now + 604_800);
    store.insert_message(&e1, false)?;
    let mut e2 = msg(alice, "e2", ME, now + 4, "Mine too");
    e2.expires_at = Some(now + 604_800);
    store.insert_message(&e2, false)?;
    store.upsert_reaction(alice, "a3", ME, "👍", now - 500, true)?;
    store.upsert_reaction(alice, "a3", alice, "👍", now - 490, false)?;
    store.upsert_reaction(alice, "a3", carol, "❤️", now - 480, false)?;
    store.upsert_reaction(alice, "a1", bob, "😂", now - 2 * day + 60, false)?;
    store.upsert_reaction(alice, "a12", alice, "🔥", now - 45, false)?;

    // Text formatting (`WACLIENT_DEMO_UI=seq:open:666`): every WhatsApp format, both directions.
    let fmt = "666@s.whatsapp.net";
    store.upsert_chat(fmt, ChatKind::Dm, Some("Formatting"))?;
    store.upsert_contact(&ContactPatch { jid: fmt.into(), full_name: Some("Formatting".into()), push_name: None, phone: None })?;
    let theirs = "*Bold* _italic_ ~strike~ `inline code` and *_both_*
2*3*4 and snake_case_name stay plain
> a quoted line with *bold*
- bullet one
1. numbered one
```
mono *verbatim*
  keeps   spacing
```
*https://example.com/docs* and +98 912 345 6789
*سلام* دنیا 🙂*ok*";
    store.insert_message(&msg(fmt, "f1", fmt, now - 60, theirs), false)?;
    store.insert_message(&msg(fmt, "f2", ME, now - 30, "I *agree*, _really_ ~not~ `x = 1`
> your quote
* star bullet"), false)?;
    // Right-to-left bubbles (`WACLIENT_DEMO_UI=rtl-bubbles`): Persian, Arabic, mixed, English, multi-line, a quoted Persian reply.
    let rtl = "777@s.whatsapp.net";
    store.upsert_chat(rtl, ChatKind::Dm, Some("RTL"))?;
    store.upsert_contact(&ContactPatch { jid: rtl.into(), full_name: Some("RTL".into()), push_name: None, phone: None })?;
    store.insert_message(&msg(rtl, "r1", rtl, now - 600, "سلام، حالت چطوره؟ امروز هوا خیلی خوبه"), false)?;
    store.insert_message(&msg(rtl, "r2", ME, now - 540, "I am fine, thanks"), false)?;
    store.insert_message(&msg(rtl, "r3", rtl, now - 480, "مرحبا بالعالم، كيف حالك اليوم؟"), false)?;
    store.insert_message(&msg(rtl, "r4", ME, now - 420, "سلام hello دوست من"), false)?;
    store.insert_message(&msg(rtl, "r5", rtl, now - 360, "Hello, this one is English"), false)?;
    store.insert_message(&msg(rtl, "r6", rtl, now - 300, "خط اول پیام
خط دوم پیام کمی طولانی‌تر است تا بشکند و چند خطی شود"), false)?;
    let mut rq = msg(rtl, "r7", ME, now - 120, "ممنون از لطفت");
    rq.quoted_id = Some("r1".into());
    rq.quoted_preview = Some("سلام، حالت چطوره؟ امروز هوا خیلی خوبه".into());
    store.insert_message(&rq, false)?;
    // Lines in different directions inside one message: each line aligns on its own.
    store.insert_message(&msg(rtl, "r8", rtl, now - 110, "سلام\nHello world\nخداحافظ"), false)?;
    store.insert_message(&msg(rtl, "r9", ME, now - 100, "*سلام* دوست من\nI am _fine_, thanks\n\nمرسی از `کمکت`"), false)?;
    store.insert_message(&msg(rtl, "r10", rtl, now - 90, "لینک مفید:\nhttps://example.com/docs\nمنبع: Docs"), false)?;
    store.insert_message(&msg(rtl, "r11", ME, now - 80, "کد من:\n```\nlet x = 1;\nprintln!(x);\n```\nممنون"), false)?;
    // Emoji-only messages (`WACLIENT_DEMO_UI=emoji-big`): 1, 2 and 3 emoji, and text with an emoji.
    store.insert_message(&msg(fmt, "em1", fmt, now - 25, "\u{1F602}"), false)?;
    store.insert_message(&msg(fmt, "em2", ME, now - 20, "\u{1F44D}\u{1F3FD}\u{2764}\u{FE0F}"), false)?;
    store.insert_message(&msg(fmt, "em3", fmt, now - 15, "\u{1F1EE}\u{1F1F7} \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467} \u{1F525}"), false)?;
    store.insert_message(&msg(fmt, "em4", ME, now - 10, "ok \u{1F44D}"), false)?;

    // Statuses: text, a photo that is on disk, a photo to download, and our own.
    let status = |id: &str, sender: &str, ts: i64, text: Option<&str>, kind: MessageKind| {
        let mut m = msg(STATUS_JID, id, sender, ts, "");
        m.text = text.map(String::from);
        m.kind = kind;
        m
    };
    let mut s1 = status("st1", carol, now - 7_200, Some("Coffee first, then everything else"), MessageKind::Text);
    s1.sender_jid = carol.into();
    store.insert_message(&s1, false)?;
    let mut s2 = status("st2", carol, now - 3_600, Some("Sunset from the roof"), MessageKind::Image);
    s2.thumbnail = thumbnail();
    s2.local_path = path_str(&photo);
    store.insert_message(&s2, false)?;
    let mut s3 = status("st3", bob, now - 1_800, None, MessageKind::Image);
    s3.thumbnail = thumbnail();
    store.insert_message(&s3, false)?;
    store.insert_message(&status("st4", bob, now - 900, Some("Back online tomorrow"), MessageKind::Text), false)?;
    store.insert_message(&status("st5", ME, now - 600, Some("My own status"), MessageKind::Text), false)?;
    store.mark_status_viewed("st4")?;
    store.mark_status_viewed("st3")?;

    // Group with sender names.
    store.insert_message(&msg(group, "g1", carol, now - day - 50, "Who is bringing dessert?"), true)?;
    store.insert_message(&msg(group, "g2", bob, now - day - 40, "Me!"), true)?;
    store.insert_message(&msg(group, "g3", ME, now - day - 30, "Thanks Bob"), false)?;
    store.insert_message(&msg(group, "g4", carol, now - 5000, "See everyone Sunday"), true)?;
    let mut mention = msg(group, "g5", carol, now - 4000, "@You can you pick up the cake on Saturday?");
    mention.mentions_me = true;
    store.insert_message(&mention, true)?;
    // Message operations: a pinned message and starred ones.
    store.set_pinned_message(group, Some("g1"), now + 7 * day)?;
    store.set_starred(group, "g4", true)?;
    store.set_starred(alice, "a1", true)?;
    store.set_starred(alice, "a2", true)?;

    // Long history to exercise paging and virtualization.
    for i in 0..400 {
        let from_me = i % 3 == 0;
        let sender = if from_me { ME } else { bob };
        store.insert_message(
            &msg(bob, &format!("b{i}"), sender, now - 10 * day + i * 60, &format!("Message number {i}")),
            false,
        )?;
    }
    store.set_unread(bob, 0)?;
    Ok(Arc::new(store))
}

/// A second, clearly different account for checking profile switching.
pub fn work_store() -> Result<Arc<Store>, Box<dyn std::error::Error>> {
    const WORK_ME: &str = "2000@s.whatsapp.net";
    let store = Store::open_in_memory()?;
    let now = chrono::Utc::now().timestamp();
    let dana = "555@s.whatsapp.net";
    let erin = "666@s.whatsapp.net";
    let team = "888@g.us";

    store.upsert_chat(dana, ChatKind::Dm, Some("Dana Reyes"))?;
    store.upsert_chat(erin, ChatKind::Dm, Some("Erin (Accounting)"))?;
    store.upsert_chat(team, ChatKind::Group, Some("Project Falcon"))?;
    for (jid, name) in [(dana, "Dana Reyes"), (erin, "Erin (Accounting)")] {
        store.upsert_contact(&ContactPatch {
            jid: jid.into(),
            full_name: Some(name.into()),
            push_name: None,
            phone: None,
        })?;
    }
    let work = |chat: &str, id: &str, sender: &str, ts: i64, text: &str| {
        let mut m = msg(chat, id, sender, ts, text);
        m.from_me = sender == WORK_ME;
        m
    };
    store.insert_message(&work(dana, "w1", dana, now - 7200, "Can you send the Q3 numbers?"), true)?;
    store.insert_message(&work(dana, "w2", WORK_ME, now - 7000, "Sending them after lunch"), false)?;
    store.insert_message(&work(dana, "w3", dana, now - 600, "Perfect, thanks!"), true)?;
    store.insert_message(&work(erin, "w4", erin, now - 86_400, "Invoice 4471 is approved"), true)?;
    store.set_unread(erin, 0)?;
    store.insert_message(&work(team, "w5", dana, now - 3000, "Standup moved to 10:30"), true)?;
    store.insert_message(&work(team, "w6", erin, now - 2900, "Works for me"), true)?;
    store.insert_message(&work(team, "w7", WORK_ME, now - 2800, "Same here"), false)?;
    Ok(Arc::new(store))
}

/// Variant name of a command; demo logging must never include message text.
pub fn command_name(cmd: &wa_engine::Command) -> &'static str {
    use wa_engine::Command;
    match cmd {
        Command::SendText { .. } => "SendText",
        Command::EditMessage { .. } => "EditMessage",
        Command::ForwardMessages { .. } => "ForwardMessages",
        Command::PinMessage { .. } => "PinMessage",
        Command::StarMessage { .. } => "StarMessage",
        Command::FetchMessageInfo { .. } => "FetchMessageInfo",
        Command::MarkChatUnread { .. } => "MarkChatUnread",
        Command::ClearChat { .. } => "ClearChat",
        Command::SetBlocked { .. } => "SetBlocked",
        Command::FetchBlocklist => "FetchBlocklist",
        Command::SetOwnName { .. } => "SetOwnName",
        Command::SetOwnAbout { .. } => "SetOwnAbout",
        Command::SetOwnPicture { .. } => "SetOwnPicture",
        Command::FetchOwnProfile => "FetchOwnProfile",
        Command::SendMedia { .. } => "SendMedia",
        Command::SendSticker { .. } => "SendSticker",
        Command::SendContact { .. } => "SendContact",
        Command::LookupPhones { .. } => "LookupPhones",
        Command::SendLocation { .. } => "SendLocation",
        Command::PostTextStatus { .. } => "PostTextStatus",
        Command::PostMediaStatus { .. } => "PostMediaStatus",
        Command::SetChatTimer { .. } => "SetChatTimer",
        Command::SetDefaultTimer { .. } => "SetDefaultTimer",
        Command::FetchDefaultTimer => "FetchDefaultTimer",
        Command::MarkChatRead { .. } => "MarkChatRead",
        Command::SubscribePresence { .. } => "SubscribePresence",
        Command::SetChatReceipts { .. } => "SetChatReceipts",
        Command::SendReadReceiptsNow { .. } => "SendReadReceiptsNow",
        Command::SetTyping { .. } => "SetTyping",
        Command::SendReaction { .. } => "SendReaction",
        Command::MarkStatusViewed { .. } => "MarkStatusViewed",
        Command::UpdateSettings(_) => "UpdateSettings",
        Command::DownloadMedia { .. } => "DownloadMedia",
        Command::FetchAvatar { .. } => "FetchAvatar",
        Command::Logout => "Logout",
        Command::RetryPairing => "RetryPairing",
        Command::Reconnect => "Reconnect",
        Command::DeleteMessage { .. } => "DeleteMessage",
        Command::SetArchived { .. } => "SetArchived",
        Command::SetPinned { .. } => "SetPinned",
        Command::SetMuted { .. } => "SetMuted",
        Command::FetchContactInfo { .. } => "FetchContactInfo",
        Command::RejectCall { .. } => "RejectCall",
        Command::TranslateMessage { .. } => "TranslateMessage",
        Command::FetchTranslations { .. } => "FetchTranslations",
        Command::TranslateText { .. } => "TranslateText",
        Command::VotePoll { .. } => "VotePoll",
        Command::CreatePoll { .. } => "CreatePoll",
        Command::CreateGroup { .. } => "CreateGroup",
        Command::UpdateGroupMembers { .. } => "UpdateGroupMembers",
        Command::FetchMembershipRequests { .. } => "FetchMembershipRequests",
        Command::AnswerMembershipRequests { .. } => "AnswerMembershipRequests",
        Command::SetGroupSubject { .. } => "SetGroupSubject",
        Command::SetGroupDescription { .. } => "SetGroupDescription",
        Command::SetGroupPicture { .. } => "SetGroupPicture",
        Command::SetGroupSetting { .. } => "SetGroupSetting",
        Command::LeaveGroup { .. } => "LeaveGroup",
        Command::FetchInviteLink { .. } => "FetchInviteLink",
        Command::PreviewInvite { .. } => "PreviewInvite",
        Command::JoinGroup { .. } => "JoinGroup",
        Command::Shutdown => "Shutdown",
    }
}

/// Applies archive / pin / mute commands to the demo store the way the engine
/// does; true when a chat changed.
pub fn apply_chat_flags(store: &Store, cmd: &wa_engine::Command) -> bool {
    use wa_engine::Command;
    let result = match cmd {
        // Archiving unpins, as the engine does.
        Command::SetArchived { chat_jid, archived: true } => store.set_chat_flags(chat_jid, Some(false), Some(true), None),
        Command::SetArchived { chat_jid, archived: false } => store.set_chat_flags(chat_jid, None, Some(false), None),
        Command::SetPinned { chat_jid, pinned } => store.set_chat_flags(chat_jid, Some(*pinned), None, None),
        Command::SetChatReceipts { chat_jid, enabled } => store.set_chat_receipts_off(chat_jid, !*enabled),
        Command::SetMuted { chat_jid, until } => store.set_chat_flags(chat_jid, None, None, Some(until.unwrap_or(0))),
        _ => return false,
    };
    if let Err(e) = &result {
        log::warn!("demo: could not apply the command: {e}");
    }
    result.is_ok()
}

/// Stands in for the engine's message operations (edit, pin, star, forward,
/// message info) on the demo store; returns the events the engine would emit.
pub fn apply_msg_ops(store: &Store, cmd: &wa_engine::Command) -> Vec<wa_engine::UiEvent> {
    use wa_engine::{Command, ReceiptEntry, UiEvent};
    let now = chrono::Utc::now().timestamp();
    let changed = |chat: &str| UiEvent::MessagesChanged { chat_jid: chat.to_string() };
    let done = |r: wa_store::Result<bool>, chat: &str| -> Vec<UiEvent> {
        match r {
            Ok(_) => vec![changed(chat), UiEvent::ChatsChanged],
            Err(e) => {
                log::warn!("demo: could not apply the command: {e}");
                Vec::new()
            }
        }
    };
    match cmd {
        // The demo has no engine: a sent text is stored as the engine would.
        Command::SendText { chat_jid, text, quoted_id, .. } => {
            let id = format!("demo-{}", chrono::Utc::now().timestamp_nanos_opt().unwrap_or(now));
            let mut m = msg(chat_jid, &id, ME, now, text);
            m.quoted_id.clone_from(quoted_id);
            done(store.insert_message(&m, false).map(|_| true), chat_jid)
        }
        Command::EditMessage { chat_jid, id, text } => {
            done(store.edit_message(chat_jid, id, text).map(|()| true), chat_jid)
        }
        Command::PinMessage { chat_jid, id, pin, duration_secs } => {
            let r = if *pin {
                store.set_pinned_message(chat_jid, Some(id), now + i64::from(*duration_secs))
            } else {
                store.clear_pinned_message_if(chat_jid, id)
            };
            done(r, chat_jid)
        }
        Command::StarMessage { chat_jid, id, starred } => done(store.set_starred(chat_jid, id, *starred), chat_jid),
        Command::ForwardMessages { from_chat, ids, to_chats } => {
            let mut events = Vec::new();
            for to in to_chats {
                for (n, id) in ids.iter().enumerate() {
                    let Ok(Some(src)) = store.message(from_chat, id) else { continue };
                    let copy = msg(to, &format!("fwd-{id}-{now}-{n}"), ME, now + n as i64, src.text.as_deref().unwrap_or(""));
                    if let Err(e) = store.insert_message(&copy, false) {
                        log::warn!("demo: forward failed: {e}");
                    }
                }
                events.push(changed(to));
            }
            events.push(UiEvent::ChatsChanged);
            events
        }
        Command::SendSticker { chat_jid, source_chat, source_id, .. } => {
            let Ok(Some(src)) = store.message(source_chat, source_id) else { return Vec::new() };
            let mut copy = msg(chat_jid, &format!("stk-{source_id}-{now}"), ME, now, "");
            copy.text = None;
            copy.kind = MessageKind::Sticker;
            copy.media_mime = src.media_mime.clone();
            copy.media_size = src.media_size;
            copy.local_path = src.local_path.clone();
            if let Err(e) = store.insert_message(&copy, false) {
                log::warn!("demo: sticker send failed: {e}");
            }
            vec![changed(chat_jid), UiEvent::ChatsChanged]
        }
        Command::FetchMessageInfo { chat_jid, id } => {
            let receipts = if chat_jid.ends_with("@g.us") {
                vec![
                    ReceiptEntry { jid: "222@s.whatsapp.net".into(), delivered_at: Some(now - 3000), read_at: Some(now - 2500), played_at: None },
                    ReceiptEntry { jid: "333@s.whatsapp.net".into(), delivered_at: Some(now - 2900), read_at: None, played_at: None },
                ]
            } else {
                vec![ReceiptEntry { jid: chat_jid.clone(), delivered_at: Some(now - 7200), read_at: Some(now - 3600), played_at: None }]
            };
            vec![UiEvent::MessageInfo { chat_jid: chat_jid.clone(), id: id.clone(), receipts }]
        }
        _ => Vec::new(),
    }
}
