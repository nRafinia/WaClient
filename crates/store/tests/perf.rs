//! Performance tests for the store. Heavy ones are `#[ignore]`; run with
//! `cargo test -p wa_store --release -- --ignored --nocapture`.

use std::time::{Duration, Instant};

use wa_store::{ChatKind, MessageKind, NewMessage, Store};

const CHATS: usize = 500;
const TOTAL: usize = 200_000;
const BATCH: usize = 1_000;

fn msg(chat: &str, id: String, ts: i64) -> NewMessage {
    NewMessage {
        chat_jid: chat.to_string(),
        id,
        sender_jid: chat.to_string(),
        from_me: false,
        ts,
        kind: MessageKind::Text,
        text: Some("hello world, this is a typical message body".to_string()),
        quoted_id: None,
        quoted_preview: None,
        media_mime: None,
        media_name: None,
        media_size: None,
        media_duration: None,
        thumbnail: None,
        local_path: None,
        raw: None,
        ..Default::default()
    }
}

fn chat_jid(i: usize) -> String {
    format!("{:05}@s.whatsapp.net", i)
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

fn time_median<F: FnMut()>(runs: usize, mut f: F) -> Duration {
    median(
        (0..runs)
            .map(|_| {
                let t = Instant::now();
                f();
                t.elapsed()
            })
            .collect(),
    )
}

/// Chat 0 is the largest (skewed): half of all messages.
fn target_chat(n: usize) -> usize {
    if n % 2 == 0 {
        0
    } else {
        1 + (n / 2) % (CHATS - 1)
    }
}

#[test]
#[ignore]
fn store_perf_200k_messages() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("perf.db")).unwrap();
    for i in 0..CHATS {
        let name = if i % 10 == 0 { format!("Abc Person {i}") } else { format!("Contact {i}") };
        store.upsert_chat(&chat_jid(i), ChatKind::Dm, Some(&name)).unwrap();
    }

    // Bulk insert.
    let t = Instant::now();
    let mut n = 0usize;
    while n < TOTAL {
        let batch: Vec<NewMessage> = (n..n + BATCH)
            .map(|k| {
                let c = chat_jid(target_chat(k));
                msg(&c, format!("M{k}"), 1_600_000_000 + k as i64)
            })
            .collect();
        assert_eq!(store.insert_messages_bulk(&batch).unwrap(), BATCH);
        n += BATCH;
    }
    let bulk = t.elapsed();
    println!("PERF bulk insert {TOTAL} msgs / {CHATS} chats: {bulk:?}");
    assert!(bulk < Duration::from_secs(20), "bulk insert too slow: {bulk:?}");

    // list_chats
    let mut count = 0;
    let d = time_median(5, || count = store.list_chats(true).unwrap().len());
    println!("PERF list_chats ({count} chats): median {d:?}");
    assert_eq!(count, CHATS);
    assert!(d < Duration::from_millis(50), "list_chats slow: {d:?}");

    // messages_page
    let big = chat_jid(0);
    let d = time_median(5, || {
        assert_eq!(store.messages_page(&big, None, 50).unwrap().len(), 50);
    });
    println!("PERF messages_page(first 50): median {d:?}");
    assert!(d < Duration::from_millis(10), "first page slow: {d:?}");

    let mut before: Option<i64> = None;
    let mut durs = Vec::new();
    for _ in 0..20 {
        let t = Instant::now();
        let page = store.messages_page(&big, before, 50).unwrap();
        durs.push(t.elapsed());
        assert_eq!(page.len(), 50);
        let first = page.first().unwrap().ts;
        assert!(before.map_or(true, |b| first < b));
        before = Some(first);
    }
    let d = median(durs.clone());
    println!("PERF messages_page paging 20 pages: median {d:?}, max {:?}", durs.iter().max().unwrap());
    assert!(d < Duration::from_millis(10), "paging slow: {d:?}");

    // search_chats
    let mut hits = 0;
    let d = time_median(5, || hits = store.search_chats("abc").unwrap().len());
    println!("PERF search_chats(\"abc\") ({hits} hits): median {d:?}");
    assert_eq!(hits, CHATS / 10);
    assert!(d < Duration::from_millis(50), "search slow: {d:?}");

    // live insert latency
    let mut k = 0usize;
    let d = time_median(101, || {
        k += 1;
        let m = msg(&chat_jid(k % CHATS), format!("LIVE{k}"), 1_700_000_000 + k as i64);
        assert!(store.insert_message(&m, true).unwrap());
    });
    println!("PERF insert_message live: median {d:?}");
    assert!(d < Duration::from_millis(2), "live insert slow: {d:?}");
}

#[test]
fn store_small_smoke_perf_paths_work() {
    let store = Store::open_in_memory().unwrap();
    let c = chat_jid(1);
    let batch: Vec<_> = (0..120).map(|k| msg(&c, format!("M{k}"), k as i64)).collect();
    assert_eq!(store.insert_messages_bulk(&batch).unwrap(), 120);
    assert_eq!(store.insert_messages_bulk(&batch).unwrap(), 0, "duplicates ignored");
    let p1 = store.messages_page(&c, None, 50).unwrap();
    let p2 = store.messages_page(&c, Some(p1[0].ts), 50).unwrap();
    assert_eq!((p1.len(), p2.len()), (50, 50));
    assert_eq!(p1.last().unwrap().ts, 119);
    assert_eq!(p2.last().unwrap().ts + 1, p1[0].ts);
}
