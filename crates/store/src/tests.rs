use super::*;

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn store_is_send_sync() {
    assert_send_sync::<Store>();
}

const A: &str = "111@s.whatsapp.net";

fn msg(chat: &str, id: &str, ts: i64, from_me: bool) -> NewMessage {
    NewMessage {
        chat_jid: chat.into(),
        id: id.into(),
        sender_jid: if from_me { "me@s.whatsapp.net".into() } else { chat.into() },
        from_me,
        ts,
        kind: MessageKind::Text,
        text: Some(format!("m{id}")),
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

fn chat(s: &Store, jid: &str) -> ChatRow {
    s.chat(jid).unwrap().unwrap()
}

fn user_version(s: &Store) -> i64 {
    s.lock().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap()
}

#[test]
fn migration_sets_version_and_is_not_rerun() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.sqlite");
    {
        let s = Store::open(&path).unwrap();
        assert_eq!(user_version(&s), 10);
        s.insert_message(&msg(A, "1", 10, false), true).unwrap();
    }
    // Re-running migration 1 would fail with "table already exists".
    let s = Store::open(&path).unwrap();
    assert!(s.message(A, "1").unwrap().is_some());
    assert_eq!(user_version(&s), 10);
}

#[test]
fn duplicate_insert_returns_false_and_keeps_unread() {
    let s = Store::open_in_memory().unwrap();
    assert!(s.insert_message(&msg(A, "1", 10, false), true).unwrap());
    assert!(!s.insert_message(&msg(A, "1", 10, false), true).unwrap());
    assert_eq!(chat(&s, A).unread, 1);
}

#[test]
fn outgoing_is_not_sorted_before_newer_incoming_rows() {
    let s = Store::open_in_memory().unwrap();
    // Contact's photo carries a sender clock ahead of our (behind) PC clock.
    s.insert_message(&msg(A, "photo", 1000, false), true).unwrap();
    assert!(s.insert_outgoing(&msg(A, "text", 990, true)).unwrap());
    let ids: Vec<_> = s.messages_page(A, None, 10).unwrap().into_iter().map(|m| m.id).collect();
    assert_eq!(ids, ["photo", "text"]);
    assert_eq!(chat(&s, A).last_ts, 1000);
    // A correct clock is left alone, and a far-future row does not drag us.
    s.insert_outgoing(&msg(A, "later", 1500, true)).unwrap();
    s.insert_message(&msg(A, "bogus", 1500 + 10 * 24 * 3600, false), false).unwrap();
    s.insert_outgoing(&msg(A, "after", 1600, true)).unwrap();
    let ts = |id: &str| s.messages_page(A, None, 10).unwrap().into_iter().find(|m| m.id == id).unwrap().ts;
    assert_eq!(ts("later"), 1500);
    assert_eq!(ts("after"), 1600, "rows beyond the clamp window are ignored");
}

#[test]
fn unread_counting() {
    let s = Store::open_in_memory().unwrap();
    s.insert_message(&msg(A, "1", 10, false), true).unwrap();
    s.insert_message(&msg(A, "2", 11, true), true).unwrap();
    s.insert_message(&msg(A, "3", 12, false), false).unwrap();
    assert_eq!(chat(&s, A).unread, 1);
    let n = s.insert_messages_bulk(&[msg(A, "4", 13, false), msg(A, "5", 14, false)]).unwrap();
    assert_eq!(n, 2);
    assert_eq!(chat(&s, A).unread, 1);
    s.mark_chat_read(A).unwrap();
    assert_eq!(chat(&s, A).unread, 0);
    s.set_unread(A, 7).unwrap();
    assert_eq!(chat(&s, A).unread, 7);
}

#[test]
fn preview_and_last_ts_logic() {
    let s = Store::open_in_memory().unwrap();
    s.insert_message(&msg(A, "1", 100, false), true).unwrap();
    s.insert_message(&msg(A, "0", 50, false), true).unwrap();
    let c = chat(&s, A);
    assert_eq!((c.last_ts, c.last_preview.as_deref()), (100, Some("m1")));

    let mut v = msg(A, "2", 200, false);
    v.kind = MessageKind::Voice;
    v.text = None;
    s.insert_message(&v, true).unwrap();
    assert_eq!(chat(&s, A).last_preview.as_deref(), Some("🎤 Voice message"));

    let mut d = msg(A, "3", 300, false);
    d.kind = MessageKind::Document;
    d.text = None;
    d.media_name = Some("a.pdf".into());
    s.insert_messages_bulk(&[d]).unwrap();
    let c = chat(&s, A);
    assert_eq!((c.last_ts, c.last_preview.as_deref()), (300, Some("📄 a.pdf")));
}

#[test]
fn chat_kind_inferred() {
    let s = Store::open_in_memory().unwrap();
    for (j, k) in [
        ("1-2@g.us", ChatKind::Group),
        ("5@lid", ChatKind::Dm),
        (A, ChatKind::Dm),
        ("status@broadcast", ChatKind::Other),
    ] {
        s.insert_message(&msg(j, "1", 1, false), true).unwrap();
        assert_eq!(chat(&s, j).kind, k);
    }
}

#[test]
fn status_never_downgrades() {
    let s = Store::open_in_memory().unwrap();
    s.insert_message(&msg(A, "1", 1, true), false).unwrap();
    let st = |s: &Store, id: &str| s.message(A, id).unwrap().unwrap().status;
    s.update_status(A, "1", MessageStatus::Delivered).unwrap();
    s.update_status(A, "1", MessageStatus::Sent).unwrap();
    assert_eq!(st(&s, "1"), MessageStatus::Delivered);
    s.update_status(A, "1", MessageStatus::Failed).unwrap();
    assert_eq!(st(&s, "1"), MessageStatus::Delivered);
    s.update_status(A, "1", MessageStatus::Played).unwrap();
    assert_eq!(st(&s, "1"), MessageStatus::Played);

    s.insert_message(&msg(A, "2", 2, true), false).unwrap();
    s.update_status(A, "2", MessageStatus::Failed).unwrap();
    assert_eq!(st(&s, "2"), MessageStatus::Failed);
}

#[test]
fn list_chats_order_and_display_name_chain() {
    let s = Store::open_in_memory().unwrap();
    let b = "222@s.whatsapp.net";
    let c = "333@s.whatsapp.net";
    let d = "444@s.whatsapp.net";
    let g = "9-9@g.us";
    for (j, ts) in [(A, 10), (b, 20), (c, 30), (d, 40), (g, 5)] {
        s.insert_message(&msg(j, "1", ts, false), true).unwrap();
    }
    s.upsert_chat(A, ChatKind::Dm, Some("ChatName")).unwrap();
    let p = |jid: &str, f: Option<&str>, pu: Option<&str>, ph: Option<&str>| ContactPatch {
        jid: jid.into(),
        full_name: f.map(Into::into),
        push_name: pu.map(Into::into),
        phone: ph.map(Into::into),
    };
    s.upsert_contact(&p(A, Some("Full"), None, None)).unwrap();
    s.upsert_contact(&p(b, Some("Full B"), Some("Push B"), Some("1"))).unwrap();
    s.upsert_contact(&p(c, None, Some("Push C"), Some("2"))).unwrap();
    s.upsert_contact(&p(d, None, None, Some("+44 phone"))).unwrap();
    s.set_chat_flags(g, Some(true), None, None).unwrap();

    let list = s.list_chats(false).unwrap();
    let order: Vec<_> = list.iter().map(|r| r.jid.as_str()).collect();
    assert_eq!(order, [g, d, c, b, A]);
    let names: Vec<_> = list.iter().map(|r| r.display_name.as_str()).collect();
    assert_eq!(names, ["9-9@g.us", "+44 phone", "Push C", "Full B", "ChatName"]);

    s.insert_message(&msg("555@s.whatsapp.net", "1", 1, false), true).unwrap();
    assert_eq!(chat(&s, "555@s.whatsapp.net").display_name, "+555");

    s.set_chat_flags(b, None, Some(true), None).unwrap();
    assert!(s.list_chats(false).unwrap().iter().all(|r| r.jid != b));
    assert!(s.list_chats(true).unwrap().iter().any(|r| r.jid == b));

    let found = s.search_chats("PUSH c").unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].jid, c);
}

#[test]
fn messages_page_paging() {
    let s = Store::open_in_memory().unwrap();
    let all: Vec<_> = (1..=25).map(|i| msg(A, &i.to_string(), i, false)).collect();
    s.insert_messages_bulk(&all).unwrap();

    let p1 = s.messages_page(A, None, 10).unwrap();
    assert_eq!(p1.iter().map(|m| m.ts).collect::<Vec<_>>(), (16..=25).collect::<Vec<_>>());
    let p2 = s.messages_page(A, Some(p1[0].ts), 10).unwrap();
    assert_eq!(p2.iter().map(|m| m.ts).collect::<Vec<_>>(), (6..=15).collect::<Vec<_>>());
    let p3 = s.messages_page(A, Some(p2[0].ts), 10).unwrap();
    assert_eq!(p3.iter().map(|m| m.ts).collect::<Vec<_>>(), (1..=5).collect::<Vec<_>>());
    assert!(s.messages_page(A, Some(1), 10).unwrap().is_empty());
}

#[test]
fn messages_page_before_pages_through_equal_timestamps() {
    let s = Store::open_in_memory().unwrap();
    for i in 0..30 {
        s.insert_message(&msg(A, &format!("m{i:02}"), 100, false), false).unwrap();
    }
    let mut got: Vec<String> = Vec::new();
    let mut anchor: Option<(i64, String)> = None;
    loop {
        let page = s.messages_page_before(A, anchor.clone(), 10).unwrap();
        if page.is_empty() {
            break;
        }
        let ids: Vec<String> = page.iter().map(|m| m.id.clone()).collect();
        anchor = Some((page[0].ts, page[0].id.clone()));
        got.splice(0..0, ids);
    }
    let expected: Vec<String> = (0..30).map(|i| format!("m{i:02}")).collect();
    assert_eq!(got, expected);
}

#[test]
fn messages_page_before_mixes_timestamps() {
    let s = Store::open_in_memory().unwrap();
    for (id, ts) in [("a", 1), ("b", 2), ("c", 2), ("d", 2), ("e", 3)] {
        s.insert_message(&msg(A, id, ts, false), false).unwrap();
    }
    let p = s.messages_page_before(A, Some((3, "e".into())), 2).unwrap();
    assert_eq!(p.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["c", "d"]);
    let p = s.messages_page_before(A, Some((2, "c".into())), 5).unwrap();
    assert_eq!(p.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["a", "b"]);
}

#[test]
fn messages_page_resolves_sender_name_and_ties_by_rowid() {
    let s = Store::open_in_memory().unwrap();
    s.upsert_contact(&ContactPatch {
        jid: A.into(),
        full_name: None,
        push_name: Some("Ann".into()),
        phone: None,
    })
    .unwrap();
    s.insert_message(&msg(A, "b", 5, false), false).unwrap();
    s.insert_message(&msg(A, "a", 5, false), false).unwrap();
    let page = s.messages_page(A, None, 10).unwrap();
    assert_eq!(page.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["b", "a"]);
    assert_eq!(page[0].sender_name.as_deref(), Some("Ann"));
}

#[test]
fn contact_merge_and_chat_name_preserved() {
    let s = Store::open_in_memory().unwrap();
    s.upsert_contact(&ContactPatch {
        jid: A.into(),
        full_name: Some("Full".into()),
        push_name: Some("Push".into()),
        phone: None,
    })
    .unwrap();
    s.upsert_contact(&ContactPatch {
        jid: A.into(),
        full_name: None,
        push_name: Some("Push2".into()),
        phone: Some("123".into()),
    })
    .unwrap();
    s.set_contact_avatar(A, "av1", "/tmp/a.jpg").unwrap();
    let c = s.contact(A).unwrap().unwrap();
    assert_eq!(c.full_name.as_deref(), Some("Full"));
    assert_eq!(c.push_name.as_deref(), Some("Push2"));
    assert_eq!(c.phone.as_deref(), Some("123"));
    assert_eq!(c.avatar_id.as_deref(), Some("av1"));
    s.clear_contact_avatar(A).unwrap();
    let c = s.contact(A).unwrap().unwrap();
    assert_eq!((c.avatar_id, c.avatar_path), (None, None));
    assert_eq!(c.push_name.as_deref(), Some("Push2"));
    assert!(s.contact("nobody").unwrap().is_none());

    s.upsert_chat(A, ChatKind::Dm, Some("Name")).unwrap();
    s.upsert_chat(A, ChatKind::Dm, None).unwrap();
    assert_eq!(chat(&s, A).display_name, "Name");
}

#[test]
fn raw_and_local_path() {
    let s = Store::open_in_memory().unwrap();
    let mut m = msg(A, "1", 1, false);
    m.raw = Some(vec![1, 2, 3]);
    s.insert_message(&m, true).unwrap();
    assert_eq!(s.message_raw(A, "1").unwrap(), Some(vec![1, 2, 3]));
    assert_eq!(s.message_raw(A, "x").unwrap(), None);
    s.set_local_path(A, "1", "/f").unwrap();
    assert_eq!(s.message(A, "1").unwrap().unwrap().local_path.as_deref(), Some("/f"));
}

#[test]
fn chat_lookup_resolves_display_name() {
    let s = Store::open_in_memory().unwrap();
    assert!(s.chat(A).unwrap().is_none());
    s.insert_message(&msg(A, "1", 10, false), true).unwrap();
    assert_eq!(s.chat(A).unwrap().unwrap().display_name, "+111");
    s.upsert_contact(&ContactPatch {
        jid: A.into(),
        full_name: Some("Alice".into()),
        push_name: None,
        phone: None,
    })
    .unwrap();
    let c = s.chat(A).unwrap().unwrap();
    assert_eq!(c.display_name, "Alice");
    assert_eq!(c, chat(&s, A));
}

#[test]
fn clear_all_empties_every_table() {
    let s = Store::open_in_memory().unwrap();
    s.insert_message(&msg(A, "1", 10, false), true).unwrap();
    s.set_contact_avatar(A, "av", "/p.jpg").unwrap();
    s.clear_all().unwrap();
    assert!(s.list_chats(true).unwrap().is_empty());
    assert!(s.message(A, "1").unwrap().is_none());
    assert!(s.contact(A).unwrap().is_none());
    // Usable afterwards.
    assert!(s.insert_message(&msg(A, "1", 10, false), true).unwrap());
}

#[test]
fn set_raw_replaces_raw_bytes() {
    let s = Store::open_in_memory().unwrap();
    s.insert_message(&msg(A, "1", 10, true), false).unwrap();
    assert_eq!(s.message_raw(A, "1").unwrap(), None);
    s.set_raw(A, "1", &[1, 2, 3]).unwrap();
    assert_eq!(s.message_raw(A, "1").unwrap(), Some(vec![1, 2, 3]));
}

#[test]
fn migration_1_to_2_upgrades_existing_file_db() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v1.sqlite");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(MIGRATIONS[0]).unwrap();
        conn.execute_batch("PRAGMA user_version = 1").unwrap();
        conn.execute(
            "INSERT INTO messages (chat_jid, id, sender_jid, from_me, ts, kind, text) \
             VALUES (?1, 'old', ?1, 0, 5, 'text', 'legacy')",
            [A],
        )
        .unwrap();
    }
    let s = Store::open(&path).unwrap();
    assert_eq!(user_version(&s), 10);
    let m = s.message(A, "old").unwrap().unwrap();
    assert_eq!(m.text.as_deref(), Some("legacy"));
    assert!(!m.revoked && !m.edited);
    s.revoke_message(A, "old").unwrap();
    assert!(s.message(A, "old").unwrap().unwrap().revoked);
}

#[test]
fn revoke_clears_content_and_updates_preview_only_if_latest() {
    let s = Store::open_in_memory().unwrap();
    let mut media = msg(A, "1", 10, false);
    media.kind = MessageKind::Image;
    media.thumbnail = Some(vec![1, 2]);
    media.media_mime = Some("image/jpeg".into());
    media.local_path = Some("/tmp/x.jpg".into());
    s.insert_message(&media, true).unwrap();
    s.insert_message(&msg(A, "2", 20, false), true).unwrap();

    s.revoke_message(A, "1").unwrap();
    let m = s.message(A, "1").unwrap().unwrap();
    assert!(m.revoked);
    assert!(m.text.is_none() && m.thumbnail.is_none() && m.local_path.is_none());
    assert!(m.media_mime.is_none());
    assert_eq!(m.sender_jid, A);
    assert_eq!(chat(&s, A).last_preview.as_deref(), Some("m2"));

    s.revoke_message(A, "2").unwrap();
    assert_eq!(
        chat(&s, A).last_preview.as_deref(),
        Some("🚫 This message was deleted")
    );
    s.revoke_message(A, "missing").unwrap();
}

#[test]
fn edit_sets_text_flag_and_refreshes_latest_preview() {
    let s = Store::open_in_memory().unwrap();
    s.insert_message(&msg(A, "1", 10, false), true).unwrap();
    s.insert_message(&msg(A, "2", 20, false), true).unwrap();

    s.edit_message(A, "1", "fixed").unwrap();
    let m = s.message(A, "1").unwrap().unwrap();
    assert_eq!(m.text.as_deref(), Some("fixed"));
    assert!(m.edited);
    assert_eq!(chat(&s, A).last_preview.as_deref(), Some("m2"));

    s.edit_message(A, "2", "newer").unwrap();
    assert_eq!(chat(&s, A).last_preview.as_deref(), Some("newer"));

    s.revoke_message(A, "1").unwrap();
    s.edit_message(A, "1", "zombie").unwrap();
    assert!(s.message(A, "1").unwrap().unwrap().text.is_none());
}

#[test]
fn merge_chat_combines_everything() {
    let s = Store::open_in_memory().unwrap();
    let lid = "100@lid";
    s.upsert_chat(lid, ChatKind::Dm, Some("Lid Name")).unwrap();
    s.insert_message(&msg(lid, "1", 10, false), true).unwrap();
    s.insert_message(&msg(lid, "dup", 30, false), true).unwrap();
    s.set_chat_flags(lid, Some(true), None, Some(50)).unwrap();
    s.upsert_chat(A, ChatKind::Dm, None).unwrap();
    s.insert_message(&msg(A, "2", 20, false), true).unwrap();
    let mut dup = msg(A, "dup", 15, false);
    dup.text = Some("keep me".into());
    s.insert_message(&dup, true).unwrap();
    s.set_chat_flags(A, None, Some(true), None).unwrap();
    s.upsert_contact(&ContactPatch {
        jid: lid.into(),
        full_name: Some("Lid Full".into()),
        push_name: Some("Lid Push".into()),
        phone: None,
    })
    .unwrap();
    s.upsert_contact(&ContactPatch {
        jid: A.into(),
        full_name: None,
        push_name: Some("Pn Push".into()),
        phone: None,
    })
    .unwrap();

    assert!(s.merge_chat(lid, A).unwrap());

    assert!(s.chat(lid).unwrap().is_none());
    let c = chat(&s, A);
    assert_eq!(c.unread, 4);
    assert_eq!(c.last_ts, 30);
    assert_eq!(c.last_preview.as_deref(), Some("mdup"));
    assert_eq!(c.display_name, "Lid Name");
    assert!(c.pinned && c.archived);
    assert_eq!(c.muted_until, 50);
    let page = s.messages_page(A, None, 10).unwrap();
    assert_eq!(page.len(), 3);
    assert_eq!(s.message(A, "dup").unwrap().unwrap().text.as_deref(), Some("keep me"));
    assert_eq!(s.message(A, "1").unwrap().unwrap().sender_jid, A);
    assert!(s.messages_page(lid, None, 10).unwrap().is_empty());

    assert!(s.contact(lid).unwrap().is_none());
    let ct = s.contact(A).unwrap().unwrap();
    assert_eq!(ct.full_name.as_deref(), Some("Lid Full"));
    assert_eq!(ct.push_name.as_deref(), Some("Pn Push"));
}

#[test]
fn merge_chat_noops_and_contact_only() {
    let s = Store::open_in_memory().unwrap();
    s.insert_message(&msg(A, "1", 10, false), true).unwrap();
    assert!(!s.merge_chat(A, A).unwrap());
    assert!(!s.merge_chat("ghost@lid", A).unwrap());
    assert_eq!(s.messages_page(A, None, 10).unwrap().len(), 1);
    assert_eq!(chat(&s, A).unread, 1);

    s.upsert_contact(&ContactPatch {
        jid: "9@lid".into(),
        full_name: None,
        push_name: Some("Only".into()),
        phone: None,
    })
    .unwrap();
    assert!(s.merge_chat("9@lid", A).unwrap());
    assert!(s.contact("9@lid").unwrap().is_none());
    assert_eq!(s.contact(A).unwrap().unwrap().push_name.as_deref(), Some("Only"));
    assert!(s.chat("9@lid").unwrap().is_none());
}

#[test]
fn chats_missing_name_lists_unnamed_groups() {
    let s = Store::open_in_memory().unwrap();
    s.upsert_chat("g1@g.us", ChatKind::Group, None).unwrap();
    s.upsert_chat("g2@g.us", ChatKind::Group, Some("Named")).unwrap();
    s.upsert_chat(A, ChatKind::Dm, None).unwrap();
    assert_eq!(s.chats_missing_name(ChatKind::Group).unwrap(), vec!["g1@g.us".to_string()]);
}

#[test]
fn fail_stale_pending_only_touches_outgoing_pending() {
    let s = Store::open_in_memory().unwrap();
    let b = "222@s.whatsapp.net";
    s.insert_message(&msg(A, "1", 1, true), false).unwrap();
    s.insert_message(&msg(A, "2", 2, true), false).unwrap();
    s.update_status(A, "2", MessageStatus::Sent).unwrap();
    s.insert_message(&msg(A, "3", 3, false), false).unwrap();
    s.insert_message(&msg(b, "4", 4, true), false).unwrap();
    s.insert_message(&msg(b, "5", 5, true), false).unwrap();

    let mut chats = s.fail_stale_pending().unwrap();
    chats.sort();
    assert_eq!(chats, [A, b]);
    let st = |chat: &str, id: &str| s.message(chat, id).unwrap().unwrap().status;
    assert_eq!(st(A, "1"), MessageStatus::Failed);
    assert_eq!(st(A, "2"), MessageStatus::Sent);
    assert_eq!(st(A, "3"), MessageStatus::Pending);
    assert_eq!(st(b, "4"), MessageStatus::Failed);
    assert!(s.fail_stale_pending().unwrap().is_empty());
}

#[test]
fn avatar_checked_at_roundtrip_and_migration() {
    let s = Store::open_in_memory().unwrap();
    assert!(s.contact(A).unwrap().is_none());
    s.mark_avatar_checked(A, 1234).unwrap();
    assert_eq!(s.contact(A).unwrap().unwrap().avatar_checked_at, 1234);
    s.upsert_contact(&ContactPatch {
        jid: A.into(),
        full_name: Some("Ann".into()),
        push_name: None,
        phone: None,
    })
    .unwrap();
    s.set_contact_avatar(A, "id", "/p.jpg").unwrap();
    let c = s.contact(A).unwrap().unwrap();
    assert_eq!(c.avatar_checked_at, 1234);
    s.mark_avatar_checked(A, 5000).unwrap();
    assert_eq!(s.contact(A).unwrap().unwrap().avatar_checked_at, 5000);
}

#[test]
fn bulk_counting_counts_unread_for_new_incoming_only() {
    let s = Store::open_in_memory().unwrap();
    s.insert_message(&msg(A, "1", 1, false), true).unwrap();
    let batch = [
        msg(A, "1", 1, false), // duplicate
        msg(A, "2", 2, false),
        msg(A, "3", 3, true),
        msg(A, "4", 4, false),
    ];
    let inserted = s.insert_messages_bulk_counting(&batch).unwrap();
    assert_eq!(inserted, [false, true, true, true]);
    let c = chat(&s, A);
    assert_eq!(c.unread, 3);
    assert_eq!(c.last_ts, 4);
}

#[test]
fn list_chats_hides_empty_chats_but_search_finds_them() {
    let s = Store::open_in_memory().unwrap();
    let g = "1-2@g.us";
    let pinned = "3-4@g.us";
    s.upsert_chat(g, ChatKind::Group, Some("Quiet group")).unwrap();
    s.upsert_chat(pinned, ChatKind::Group, Some("Pinned quiet")).unwrap();
    s.set_chat_flags(pinned, Some(true), None, None).unwrap();
    s.insert_message(&msg(A, "1", 10, false), true).unwrap();

    let jids: Vec<_> = s.list_chats(true).unwrap().into_iter().map(|c| c.jid).collect();
    assert_eq!(jids, [pinned, A]);
    assert_eq!(s.search_chats("quiet group").unwrap().len(), 1);
    assert!(s.chat(g).unwrap().is_some());
}

#[test]
fn decrement_unread_subtracts_and_clamps_at_zero() {
    let s = Store::open_in_memory().unwrap();
    s.upsert_chat(A, ChatKind::Dm, None).unwrap();
    s.set_unread(A, 5).unwrap();
    s.decrement_unread(A, 3).unwrap();
    assert_eq!(chat(&s, A).unread, 2);
    s.decrement_unread(A, 10).unwrap();
    assert_eq!(chat(&s, A).unread, 0);
    s.set_unread(A, 4).unwrap();
    s.decrement_unread(A, -2).unwrap();
    assert_eq!(chat(&s, A).unread, 4, "negative count is ignored");
    s.decrement_unread("missing@s.whatsapp.net", 1).unwrap();
}

fn status_msg(sender: &str, id: &str, ts: i64) -> NewMessage {
    let mut m = msg(STATUS_JID, id, ts, false);
    m.sender_jid = sender.into();
    m
}

#[test]
fn migration_4_upgrades_existing_v3_db() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v3.sqlite");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        for sql in &MIGRATIONS[..3] {
            conn.execute_batch(sql).unwrap();
        }
        conn.execute_batch("PRAGMA user_version = 3").unwrap();
        conn.execute(
            "INSERT INTO messages (chat_jid, id, sender_jid, from_me, ts, kind, text) \
             VALUES (?1, 'old', ?1, 0, 5, 'text', 'legacy')",
            [A],
        )
        .unwrap();
    }
    let s = Store::open(&path).unwrap();
    assert_eq!(user_version(&s), 10);
    assert_eq!(s.message(A, "old").unwrap().unwrap().text.as_deref(), Some("legacy"));
    let viewed: i64 = s
        .lock()
        .query_row("SELECT viewed FROM messages WHERE id = 'old'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(viewed, 0, "viewed defaults to 0 on existing rows");
    s.upsert_reaction(A, "old", A, "👍", 1, false).unwrap();
    assert_eq!(s.reactions_for(A, &["old".to_string()]).unwrap().len(), 1);
    s.upsert_poll(A, "p", "Q", &["a".into(), "b".into()], 1).unwrap();
    assert!(s.poll_for(A, "p").unwrap().is_some());
}

#[test]
fn reactions_aggregate_and_remove() {
    let s = Store::open_in_memory().unwrap();
    let ids = ["1".to_string(), "2".to_string()];
    // Reactions may arrive before the message they target.
    s.upsert_reaction(A, "1", A, "👍", 10, false).unwrap();
    s.upsert_reaction(A, "1", "me@s.whatsapp.net", "👍", 11, true).unwrap();
    s.upsert_reaction(A, "1", "other@s.whatsapp.net", "❤", 12, false).unwrap();
    let r = s.reactions_for(A, &ids).unwrap();
    assert_eq!(
        r["1"],
        [("👍".to_string(), 2, true), ("❤".to_string(), 1, false)]
    );
    assert!(!r.contains_key("2"));

    // Changing a reaction replaces it; an older one does not.
    s.upsert_reaction(A, "1", A, "😂", 20, false).unwrap();
    s.upsert_reaction(A, "1", A, "👍", 5, false).unwrap();
    let r = s.reactions_for(A, &ids).unwrap();
    assert!(r["1"].contains(&("😂".to_string(), 1, false)));
    assert!(r["1"].contains(&("👍".to_string(), 1, true)));

    // Empty emoji removes; own reaction no longer "mine".
    s.upsert_reaction(A, "1", "me@s.whatsapp.net", "", 30, true).unwrap();
    let r = s.reactions_for(A, &ids).unwrap();
    assert!(r["1"].iter().all(|(_, _, mine)| !mine));
    s.remove_reaction(A, "1", A).unwrap();
    s.remove_reaction(A, "1", "other@s.whatsapp.net").unwrap();
    assert!(s.reactions_for(A, &ids).unwrap().is_empty());
    assert!(s.reactions_for(A, &[]).unwrap().is_empty());
}

#[test]
fn list_and_search_chats_exclude_status() {
    let s = Store::open_in_memory().unwrap();
    s.insert_message(&msg(A, "1", 10, false), true).unwrap();
    s.insert_message(&status_msg(A, "s1", 20), true).unwrap();
    s.upsert_chat(STATUS_JID, ChatKind::Other, Some("Status")).unwrap();
    s.set_chat_flags(STATUS_JID, Some(true), None, None).unwrap();
    let jids: Vec<_> = s.list_chats(true).unwrap().into_iter().map(|c| c.jid).collect();
    assert_eq!(jids, [A]);
    assert!(s.search_chats("status").unwrap().is_empty());
    assert_eq!(chat(&s, A).unread, 1, "status does not touch other chats");
    assert!(s.chat(STATUS_JID).unwrap().is_some(), "row exists but is never listed");
}

#[test]
fn statuses_since_groups_orders_and_tracks_viewed() {
    let s = Store::open_in_memory().unwrap();
    let (bob, cy) = ("222@s.whatsapp.net", "333@s.whatsapp.net");
    s.upsert_contact(&ContactPatch {
        jid: bob.into(),
        full_name: Some("Bob".into()),
        push_name: None,
        phone: None,
    })
    .unwrap();
    for (sender, id, ts) in [(bob, "b1", 100), (cy, "c1", 150), (bob, "b2", 200), (cy, "old", 10)] {
        s.insert_message(&status_msg(sender, id, ts), true).unwrap();
    }
    let groups = s.statuses_since(50).unwrap();
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].sender_jid, bob, "newest group first");
    assert_eq!(groups[0].sender_name.as_deref(), Some("Bob"));
    let ids: Vec<_> = groups[0].items.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["b1", "b2"], "items ascending");
    assert_eq!(groups[1].items.len(), 1, "older than the cutoff is excluded");
    assert!(groups.iter().all(|g| !g.all_viewed));

    assert!(s.mark_status_viewed("b1").unwrap());
    assert!(!s.mark_status_viewed("b1").unwrap(), "only the first view counts");
    assert!(!s.mark_status_viewed("missing").unwrap());
    assert!(!s.statuses_since(50).unwrap()[0].all_viewed);
    s.mark_status_viewed("b2").unwrap();
    let groups = s.statuses_since(50).unwrap();
    assert!(groups[0].all_viewed && !groups[1].all_viewed);
}

#[test]
fn purge_statuses_removes_only_old_status_rows() {
    let s = Store::open_in_memory().unwrap();
    let mut old = status_msg(A, "old", 100);
    old.local_path = Some("/cache/old.jpg".into());
    s.insert_message(&old, true).unwrap();
    s.insert_message(&status_msg(A, "new", 5000), true).unwrap();
    s.insert_message(&msg(A, "chatmsg", 100, false), true).unwrap();
    s.upsert_reaction(STATUS_JID, "old", A, "👍", 101, false).unwrap();
    s.record_receipt(STATUS_JID, "old", A, ReceiptKind::Read, 102).unwrap();

    let purge = s.purge_statuses(1000).unwrap();
    assert_eq!(purge.removed, 1);
    assert_eq!(purge.media_paths, ["/cache/old.jpg"]);
    assert!(s.message(STATUS_JID, "old").unwrap().is_none());
    assert!(s.message(STATUS_JID, "new").unwrap().is_some());
    assert!(s.message(A, "chatmsg").unwrap().is_some(), "chat messages are kept");
    assert!(s.reactions_for(STATUS_JID, &["old".to_string()]).unwrap().is_empty());
    assert!(s.message_receipts(STATUS_JID, "old").unwrap().is_empty());
    assert_eq!(s.purge_statuses(1000).unwrap().removed, 0);
}

#[test]
fn poll_for_tallies_votes() {
    let s = Store::open_in_memory().unwrap();
    assert!(s.poll_for(A, "p").unwrap().is_none());
    let opts = ["Yes".to_string(), "No \"quoted\"".to_string(), "Maybe".to_string()];
    s.upsert_poll(A, "p", "Lunch?", &opts, 2).unwrap();
    // A repeated definition does not reset anything.
    s.upsert_poll(A, "p", "Other", &["x".into()], 1).unwrap();

    s.upsert_poll_vote(A, "p", A, &["Yes".into(), "Maybe".into()], 10, false).unwrap();
    s.upsert_poll_vote(A, "p", "me@s.whatsapp.net", &["Yes".into()], 11, true).unwrap();
    s.upsert_poll_vote(A, "p", "z@s.whatsapp.net", &[opts[1].clone(), "Unknown".into()], 12, false)
        .unwrap();
    let p = s.poll_for(A, "p").unwrap().unwrap();
    assert_eq!(p.question, "Lunch?");
    assert_eq!(p.selectable_count, 2);
    assert_eq!(
        p.options,
        [
            ("Yes".to_string(), 2, true),
            (opts[1].clone(), 1, false),
            ("Maybe".to_string(), 1, false),
        ]
    );

    // Re-vote replaces; an older vote is ignored; empty withdraws.
    s.upsert_poll_vote(A, "p", A, &["No \"quoted\"".into()], 20, false).unwrap();
    s.upsert_poll_vote(A, "p", A, &["Yes".into()], 5, false).unwrap();
    s.upsert_poll_vote(A, "p", "me@s.whatsapp.net", &[], 30, true).unwrap();
    let p = s.poll_for(A, "p").unwrap().unwrap();
    assert_eq!(
        p.options.iter().map(|o| (o.1, o.2)).collect::<Vec<_>>(),
        [(0, false), (2, false), (0, false)]
    );
    assert_eq!(s.poll_options(A, "p").unwrap().unwrap(), opts);
}

fn seeded_chat(s: &Store) {
    let mut m = msg(A, "m1", 1, false);
    m.local_path = Some("/cache/a.jpg".into());
    s.insert_message(&m, true).unwrap();
    s.insert_message(&msg(A, "m2", 2, false), true).unwrap();
    s.insert_message(&msg("other@s.whatsapp.net", "o1", 3, false), true).unwrap();
    s.upsert_reaction(A, "m1", A, "👍", 5, false).unwrap();
    s.upsert_poll(A, "p", "Q", &["a".to_string()], 1).unwrap();
    s.upsert_poll_vote(A, "p", A, &["a".into()], 6, false).unwrap();
}

#[test]
fn clear_chat_keeps_the_chat_row_and_other_chats() {
    let s = Store::open_in_memory().unwrap();
    seeded_chat(&s);
    assert_eq!(chat(&s, A).unread, 2);
    assert_eq!(s.clear_chat(A, None).unwrap(), ["/cache/a.jpg"]);
    let c = chat(&s, A);
    assert_eq!((c.unread, c.last_ts, c.last_preview), (0, 0, None));
    assert!(s.message(A, "m1").unwrap().is_none());
    assert!(s.poll_for(A, "p").unwrap().is_none());
    assert!(s.message("other@s.whatsapp.net", "o1").unwrap().is_some());
    assert!(s.clear_chat("none@s.whatsapp.net", None).unwrap().is_empty());
}

#[test]
fn clear_chat_keep_starred_leaves_starred_messages_and_their_extras() {
    let s = Store::open_in_memory().unwrap();
    seeded_chat(&s);
    assert!(s.set_starred(A, "m2", true).unwrap());
    s.upsert_reaction(A, "m2", A, "x", 6, false).unwrap();
    assert_eq!(s.clear_chat_keep_starred(A, None).unwrap(), ["/cache/a.jpg"]);
    assert!(s.message(A, "m1").unwrap().is_none());
    assert!(s.message(A, "m2").unwrap().is_some());
    assert!(s.poll_for(A, "p").unwrap().is_none());
    assert!(s.message("other@s.whatsapp.net", "o1").unwrap().is_some());
    let c = chat(&s, A);
    assert_eq!((c.unread, c.last_ts), (0, 2));
    assert!(c.last_preview.is_some());
}

#[test]
fn bounded_clear_keeps_messages_after_the_range() {
    let s = Store::open_in_memory().unwrap();
    seeded_chat(&s);
    // Same timestamp as m2, inserted later: ordered after the bound.
    s.insert_message(&msg(A, "m3", 2, false), true).unwrap();
    s.insert_message(&msg(A, "m4", 9, false), true).unwrap();
    s.upsert_reaction(A, "m4", A, "x", 10, false).unwrap();
    assert_eq!(s.clear_chat(A, Some((2, "m2"))).unwrap(), ["/cache/a.jpg"]);
    assert!(s.message(A, "m1").unwrap().is_none());
    assert!(s.message(A, "m2").unwrap().is_none());
    assert!(s.message(A, "m3").unwrap().is_some());
    assert!(s.message(A, "m4").unwrap().is_some());
    let c = chat(&s, A);
    assert_eq!(c.last_ts, 9);
    assert!(c.unread <= 2 && c.last_preview.is_some());
}

#[test]
fn bounded_clear_with_a_vanished_bound_removes_only_older_rows() {
    let s = Store::open_in_memory().unwrap();
    seeded_chat(&s);
    assert!(s.clear_chat_keep_starred(A, Some((2, "gone"))).unwrap().len() <= 1);
    assert!(s.message(A, "m1").unwrap().is_none());
    assert!(s.message(A, "m2").unwrap().is_some());
}

#[test]
fn delete_chat_removes_row_messages_and_polls() {
    let s = Store::open_in_memory().unwrap();
    seeded_chat(&s);
    assert_eq!(s.delete_chat(A).unwrap(), ["/cache/a.jpg"]);
    assert!(s.chat(A).unwrap().is_none());
    assert!(s.message(A, "m2").unwrap().is_none());
    assert!(s.poll_for(A, "p").unwrap().is_none());
    assert!(s.chat("other@s.whatsapp.net").unwrap().is_some());
}

#[test]
fn poll_options_and_votes_are_deduplicated() {
    let s = Store::open_in_memory().unwrap();
    let dup = ["a".to_string(), "b".to_string(), "a".to_string()];
    s.upsert_poll(A, "p", "Q", &dup, 0).unwrap();
    assert_eq!(s.poll_options(A, "p").unwrap().unwrap(), ["a", "b"]);

    // Repeated names in one vote count once.
    assert!(s.upsert_poll_vote(A, "p", A, &dup, 1, false).unwrap());
    let p = s.poll_for(A, "p").unwrap().unwrap();
    assert_eq!(p.options.iter().map(|o| o.1).collect::<Vec<_>>(), [1, 1]);
}

#[test]
fn votes_over_the_selectable_count_are_ignored() {
    let s = Store::open_in_memory().unwrap();
    let opts = ["a".to_string(), "b".to_string(), "c".to_string()];
    s.upsert_poll(A, "p", "Q", &opts, 1).unwrap();
    assert!(s.upsert_poll_vote(A, "p", A, &["a".into()], 1, false).unwrap());
    // Two distinct options for a single-choice poll: nothing stored, old vote stays.
    assert!(!s.upsert_poll_vote(A, "p", A, &["b".into(), "c".into()], 2, false).unwrap());
    let p = s.poll_for(A, "p").unwrap().unwrap();
    assert_eq!(p.options.iter().map(|o| o.1).collect::<Vec<_>>(), [1, 0, 0]);
    // Duplicates collapse first, so this is a single selection.
    assert!(s.upsert_poll_vote(A, "p", A, &["c".into(), "c".into()], 3, false).unwrap());
    let p = s.poll_for(A, "p").unwrap().unwrap();
    assert_eq!(p.options.iter().map(|o| o.1).collect::<Vec<_>>(), [0, 0, 1]);

    // selectable_count 0 means unlimited.
    s.upsert_poll(A, "q", "Q", &opts, 0).unwrap();
    assert!(s.upsert_poll_vote(A, "q", A, &opts, 1, false).unwrap());
}

#[test]
fn purge_reactions_drops_old_orphans_and_caps_per_chat() {
    let s = Store::open_in_memory().unwrap();
    let other = "other@s.whatsapp.net";
    s.insert_message(&msg(A, "m1", 1, false), true).unwrap();
    // A reaction on a stored message survives however old it is.
    s.upsert_reaction(A, "m1", A, "👍", 5, false).unwrap();
    // Orphans: the old one goes, the recent one waits for its message.
    s.upsert_reaction(A, "gone", A, "👍", 5, false).unwrap();
    s.upsert_reaction(A, "soon", A, "👍", 500, false).unwrap();
    assert_eq!(s.purge_reactions(100, 10).unwrap(), 1);
    assert!(s.reaction_of(A, "gone", A).unwrap().is_none());
    assert!(s.reaction_of(A, "m1", A).unwrap().is_some());
    assert!(s.reaction_of(A, "soon", A).unwrap().is_some());

    // Cap: the oldest beyond it go; other chats are untouched.
    for i in 0..5 {
        s.upsert_reaction(A, "m1", &format!("u{i}@s.whatsapp.net"), "❤", 1000 + i, false)
            .unwrap();
    }
    s.upsert_reaction(other, "x", A, "👍", 1, false).unwrap();
    // Chat A holds m1/A (5), soon (500) and five newer ones; keep the 3 newest.
    assert_eq!(s.purge_reactions(0, 3).unwrap(), 4);
    assert!(s.reaction_of(A, "m1", "u4@s.whatsapp.net").unwrap().is_some());
    assert!(s.reaction_of(A, "m1", "u2@s.whatsapp.net").unwrap().is_some());
    assert!(s.reaction_of(A, "m1", "u1@s.whatsapp.net").unwrap().is_none());
    assert!(s.reaction_of(A, "soon", A).unwrap().is_none());
    assert!(s.reaction_of(other, "x", A).unwrap().is_some());
}

#[test]
fn merge_chat_moves_reactions_and_polls() {
    let s = Store::open_in_memory().unwrap();
    let lid = "9@lid";
    s.insert_message(&msg(A, "1", 1, false), true).unwrap();
    s.insert_message(&msg(lid, "2", 2, false), true).unwrap();
    s.upsert_reaction(lid, "2", lid, "👍", 3, false).unwrap();
    s.upsert_poll(lid, "2", "Q", &["a".into()], 1).unwrap();
    s.upsert_poll_vote(lid, "2", lid, &["a".into()], 4, false).unwrap();
    assert!(s.merge_chat(lid, A).unwrap());
    assert_eq!(s.reactions_for(A, &["2".to_string()]).unwrap()["2"][0].1, 1);
    assert_eq!(s.poll_for(A, "2").unwrap().unwrap().options[0].1, 1);
    assert!(s.reactions_for(lid, &["2".to_string()]).unwrap().is_empty());
}

#[test]
fn clear_all_removes_reactions_and_polls() {
    let s = Store::open_in_memory().unwrap();
    s.upsert_reaction(A, "1", A, "👍", 1, false).unwrap();
    s.upsert_poll(A, "p", "Q", &["a".into()], 1).unwrap();
    s.clear_all().unwrap();
    assert!(s.reactions_for(A, &["1".to_string()]).unwrap().is_empty());
    assert!(s.poll_for(A, "p").unwrap().is_none());
}

#[test]
fn delete_message_removes_rows_and_recomputes_the_chat_summary() {
    let s = Store::open_in_memory().unwrap();
    s.insert_message(&msg(A, "1", 10, false), true).unwrap();
    let mut media = msg(A, "2", 20, false);
    media.local_path = Some("/cache/file.jpg".into());
    s.insert_message(&media, true).unwrap();
    s.upsert_reaction(A, "2", A, "👍", 21, false).unwrap();
    s.upsert_poll(A, "2", "Q", &["a".into()], 1).unwrap();
    s.upsert_poll_vote(A, "2", A, &["a".into()], 22, false).unwrap();
    s.upsert_reaction(A, "1", A, "❤", 11, false).unwrap();
    assert_eq!(chat(&s, A).last_preview.as_deref(), Some("m2"));

    assert_eq!(s.delete_message(A, "2").unwrap().as_deref(), Some("/cache/file.jpg"));
    assert!(s.message(A, "2").unwrap().is_none());
    assert!(s.reaction_of(A, "2", A).unwrap().is_none());
    assert!(s.poll_for(A, "2").unwrap().is_none());
    let votes: i64 = s
        .lock()
        .query_row("SELECT COUNT(*) FROM poll_votes WHERE msg_id = '2'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(votes, 0);
    assert!(s.reaction_of(A, "1", A).unwrap().is_some(), "other messages keep their rows");
    let c = chat(&s, A);
    assert_eq!((c.last_ts, c.last_preview.as_deref()), (10, Some("m1")));

    // Deleting an older message leaves the summary alone; unknown ids are a no-op.
    s.insert_message(&msg(A, "3", 30, false), true).unwrap();
    assert_eq!(s.delete_message(A, "1").unwrap(), None);
    assert_eq!(chat(&s, A).last_ts, 30);
    assert_eq!(s.delete_message(A, "nope").unwrap(), None);

    // Emptying the chat resets it.
    s.delete_message(A, "3").unwrap();
    let c = chat(&s, A);
    assert_eq!((c.last_ts, c.last_preview), (0, None));
}

#[test]
fn a_new_message_does_not_unarchive_a_chat_and_archiving_can_unpin() {
    let s = Store::open_in_memory().unwrap();
    s.insert_message(&msg(A, "1", 10, false), true).unwrap();
    s.set_chat_flags(A, Some(true), None, None).unwrap();
    s.set_chat_flags(A, Some(false), Some(true), None).unwrap();
    let c = chat(&s, A);
    assert!(c.archived && !c.pinned);
    s.insert_message(&msg(A, "2", 20, false), true).unwrap();
    s.insert_message(&msg(A, "3", 30, true), true).unwrap();
    assert!(chat(&s, A).archived, "incoming and own messages keep the chat archived");
}

#[test]
fn deleting_an_unread_message_decrements_the_badge_but_a_read_one_does_not() {
    let s = Store::open_in_memory().unwrap();
    for (id, ts) in [("1", 10), ("2", 20), ("3", 30), ("4", 40)] {
        s.insert_message(&msg(A, id, ts, false), true).unwrap();
    }
    s.insert_message(&msg(A, "mine", 50, true), true).unwrap();
    s.set_unread(A, 2).unwrap(); // "3" and "4" are unread
    assert_eq!(chat(&s, A).unread, 2);

    s.delete_message(A, "1").unwrap();
    assert_eq!(chat(&s, A).unread, 2, "an old read message");
    s.delete_message(A, "mine").unwrap();
    assert_eq!(chat(&s, A).unread, 2, "an own message");
    s.delete_message(A, "3").unwrap();
    assert_eq!(chat(&s, A).unread, 1, "an unread message");
    s.delete_message(A, "2").unwrap();
    assert_eq!(chat(&s, A).unread, 1, "now-oldest read message");
    s.delete_message(A, "4").unwrap();
    assert_eq!(chat(&s, A).unread, 0);
    s.delete_message(A, "nope").unwrap();
    assert_eq!(chat(&s, A).unread, 0);
}

#[test]
fn delete_message_shows_the_revoked_preview_of_a_remaining_deleted_message() {
    let s = Store::open_in_memory().unwrap();
    s.insert_message(&msg(A, "1", 10, false), true).unwrap();
    s.insert_message(&msg(A, "2", 20, false), true).unwrap();
    s.revoke_message(A, "1").unwrap();
    s.delete_message(A, "2").unwrap();
    assert_eq!(chat(&s, A).last_preview.as_deref(), Some(crate::queries::REVOKED_PREVIEW));
}

#[test]
fn migration_5_upgrades_existing_v4_db_and_stores_about() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v4.sqlite");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        for sql in &MIGRATIONS[..4] {
            conn.execute_batch(sql).unwrap();
        }
        conn.execute_batch("PRAGMA user_version = 4").unwrap();
        conn.execute(
            "INSERT INTO contacts (jid, full_name) VALUES (?1, 'Ann')",
            [A],
        )
        .unwrap();
    }
    let s = Store::open(&path).unwrap();
    assert_eq!(user_version(&s), 10);
    let c = s.contact(A).unwrap().unwrap();
    assert_eq!(c.full_name.as_deref(), Some("Ann"));
    assert_eq!((c.about, c.about_set_at), (None, None));

    s.set_contact_about(A, Some("Busy"), Some(77)).unwrap();
    let c = s.contact(A).unwrap().unwrap();
    assert_eq!((c.about.as_deref(), c.about_set_at), (Some("Busy"), Some(77)));
    assert_eq!(c.full_name.as_deref(), Some("Ann"), "other columns are untouched");
    s.set_contact_about(A, None, None).unwrap();
    assert_eq!(s.contact(A).unwrap().unwrap().about, None);

    s.set_contact_about("222@s.whatsapp.net", Some("Hi"), None).unwrap();
    assert!(s.merge_chat("222@s.whatsapp.net", A).unwrap());
    assert_eq!(s.contact(A).unwrap().unwrap().about.as_deref(), Some("Hi"));
}

#[test]
fn migration_6_upgrades_existing_v5_db_and_stores_link_preview() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v5.sqlite");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        for sql in &MIGRATIONS[..5] {
            conn.execute_batch(sql).unwrap();
        }
        conn.execute_batch("PRAGMA user_version = 5").unwrap();
        conn.execute(
            "INSERT INTO messages (chat_jid, id, sender_jid, from_me, ts, kind, text) \
             VALUES (?1, 'old', ?1, 0, 1, 'text', 'hi')",
            [A],
        )
        .unwrap();
    }
    let s = Store::open(&path).unwrap();
    assert_eq!(user_version(&s), 10);
    let old = s.message(A, "old").unwrap().unwrap();
    assert_eq!(old.text.as_deref(), Some("hi"));
    assert_eq!((old.link_url, old.link_title, old.link_desc, old.link_thumb), (None, None, None, None));

    let with_link = |id: &str| NewMessage {
        link_url: Some("https://example.com/".into()),
        link_title: Some("Title".into()),
        link_desc: Some("Desc".into()),
        link_thumb: Some(vec![1, 2, 3]),
        ..msg(A, id, 10, false)
    };
    assert!(s.insert_message(&with_link("single"), false).unwrap());
    assert_eq!(s.insert_messages_bulk(&[with_link("bulk")]).unwrap(), 1);
    assert_eq!(s.insert_messages_bulk_counting(&[with_link("counting")]).unwrap(), vec![true]);
    for id in ["single", "bulk", "counting"] {
        let m = s.message(A, id).unwrap().unwrap();
        assert_eq!(m.link_url.as_deref(), Some("https://example.com/"));
        assert_eq!(m.link_title.as_deref(), Some("Title"));
        assert_eq!(m.link_desc.as_deref(), Some("Desc"));
        assert_eq!(m.link_thumb.as_deref(), Some(&[1u8, 2, 3][..]));
    }

    assert!(s.merge_chat(A, "222@s.whatsapp.net").unwrap());
    let moved = s.message("222@s.whatsapp.net", "single").unwrap().unwrap();
    assert_eq!(moved.link_url.as_deref(), Some("https://example.com/"));

    s.revoke_message("222@s.whatsapp.net", "single").unwrap();
    let revoked = s.message("222@s.whatsapp.net", "single").unwrap().unwrap();
    assert_eq!((revoked.link_url, revoked.link_thumb), (None, None));
}

#[test]
fn edit_keeps_link_preview_only_while_the_text_contains_the_link() {
    let s = Store::open_in_memory().unwrap();
    let with_link = |id: &str| NewMessage {
        text: Some("see https://example.com/a".into()),
        link_url: Some("https://example.com/a".into()),
        link_title: Some("Title".into()),
        link_desc: Some("Desc".into()),
        link_thumb: Some(vec![1, 2, 3]),
        ..msg(A, id, 10, true)
    };
    s.insert_message(&with_link("keep"), false).unwrap();
    s.insert_message(&with_link("drop"), false).unwrap();
    s.insert_message(&with_link("case"), false).unwrap();

    s.edit_message(A, "keep", "look https://example.com/a now").unwrap();
    let m = s.message(A, "keep").unwrap().unwrap();
    assert_eq!(m.link_url.as_deref(), Some("https://example.com/a"));
    assert_eq!(m.link_title.as_deref(), Some("Title"));
    assert_eq!(m.link_desc.as_deref(), Some("Desc"));
    assert_eq!(m.link_thumb.as_deref(), Some(&[1u8, 2, 3][..]));

    for (id, text) in [("drop", "no link now"), ("case", "see HTTPS://EXAMPLE.COM/A")] {
        s.edit_message(A, id, text).unwrap();
        let m = s.message(A, id).unwrap().unwrap();
        assert_eq!(m.text.as_deref(), Some(text));
        assert_eq!((m.link_url, m.link_title, m.link_desc, m.link_thumb), (None, None, None, None));
    }
}

#[test]
fn set_link_preview_updates_and_clears_the_card() {
    let s = Store::open_in_memory().unwrap();
    s.insert_message(&msg(A, "m", 1, true), false).unwrap();
    s.set_link_preview(A, "m", Some("https://x.test/"), Some("T"), None, Some(&[7, 8])).unwrap();
    let m = s.message(A, "m").unwrap().unwrap();
    assert_eq!(m.link_url.as_deref(), Some("https://x.test/"));
    assert_eq!(m.link_title.as_deref(), Some("T"));
    assert_eq!(m.link_desc, None);
    assert_eq!(m.link_thumb.as_deref(), Some(&[7u8, 8][..]));
    s.set_link_preview(A, "m", None, None, None, None).unwrap();
    let m = s.message(A, "m").unwrap().unwrap();
    assert_eq!((m.link_url, m.link_title, m.link_thumb), (None, None, None));
}

#[test]
fn explicit_zero_clears_mute_and_none_keeps_it() {
    let s = Store::open_in_memory().unwrap();
    s.insert_message(&msg(A, "1", 1, false), true).unwrap();
    s.set_chat_flags(A, None, None, Some(i64::MAX)).unwrap();
    s.set_chat_flags(A, Some(true), None, None).unwrap();
    assert_eq!(chat(&s, A).muted_until, i64::MAX);
    s.set_chat_flags(A, None, None, Some(0)).unwrap();
    assert_eq!(chat(&s, A).muted_until, 0);
}

#[test]
fn shared_contact_shows_its_name_in_the_chat_preview() {
    let s = Store::open_in_memory().unwrap();
    let card = NewMessage {
        kind: MessageKind::Other,
        text: Some("Jane\n+1 555 0100".into()),
        media_mime: Some("text/vcard".into()),
        media_name: Some("Jane".into()),
        ..msg(A, "c1", 5, false)
    };
    s.insert_message(&card, true).unwrap();
    assert_eq!(chat(&s, A).last_preview.as_deref(), Some("👤 Jane"));

    s.insert_message(&msg(A, "t", 6, false), true).unwrap();
    s.delete_message(A, "t").unwrap();
    assert_eq!(chat(&s, A).last_preview.as_deref(), Some("👤 Jane"), "recomputed from the stored row");

    let nameless = NewMessage { media_name: None, ..card.clone() };
    s.insert_message(&NewMessage { id: "c2".into(), ts: 9, ..nameless }, true).unwrap();
    assert_eq!(chat(&s, A).last_preview.as_deref(), Some("👤 Contact"));
}

#[test]
fn status_rows_never_count_as_unread() {
    let s = Store::open_in_memory().unwrap();
    let bob = "222@s.whatsapp.net";
    assert!(s.insert_message(&status_msg(bob, "s1", 1), true).unwrap());
    assert_eq!(s.insert_messages_bulk_counting(&[status_msg(bob, "s2", 2)]).unwrap(), vec![true]);
    assert_eq!(chat(&s, STATUS_JID).unread, 0);
    s.insert_message(&msg(A, "1", 3, false), true).unwrap();
    assert_eq!(chat(&s, A).unread, 1, "ordinary chats still count");
}

#[test]
fn migration_7_upgrades_existing_v6_db() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v6.sqlite");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        for sql in &MIGRATIONS[..6] {
            conn.execute_batch(sql).unwrap();
        }
        conn.execute_batch("PRAGMA user_version = 6").unwrap();
        conn.execute("INSERT INTO chats (jid, kind, unread) VALUES (?1, 'dm', 1)", [A]).unwrap();
        conn.execute(
            "INSERT INTO messages (chat_jid, id, sender_jid, from_me, ts, kind, text) \
             VALUES (?1, 'old', ?1, 0, 1, 'text', 'hi')",
            [A],
        )
        .unwrap();
    }
    let s = Store::open(&path).unwrap();
    assert_eq!(user_version(&s), 10);
    assert!(!chat(&s, A).receipts_off);
    assert_eq!(s.unreceipted_incoming(A, 0, 10).unwrap(), vec![("old".to_string(), A.to_string())]);
}

#[test]
fn migration_8_marks_incoming_of_read_chats_as_receipted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v7.sqlite");
    let read_chat = A;
    let unread_chat = "222@s.whatsapp.net";
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        for sql in &MIGRATIONS[..7] {
            conn.execute_batch(sql).unwrap();
        }
        conn.execute_batch("PRAGMA user_version = 7").unwrap();
        conn.execute("INSERT INTO chats (jid, kind, unread) VALUES (?1, 'dm', 0)", [read_chat]).unwrap();
        conn.execute("INSERT INTO chats (jid, kind, unread) VALUES (?1, 'dm', 1)", [unread_chat]).unwrap();
        for (chat, id) in [(read_chat, "r1"), (unread_chat, "u1")] {
            conn.execute(
                "INSERT INTO messages (chat_jid, id, sender_jid, from_me, ts, kind, text) \
                 VALUES (?1, ?2, ?1, 0, 1, 'text', 'hi')",
                [chat, id],
            )
            .unwrap();
        }
    }
    let s = Store::open(&path).unwrap();
    assert_eq!(user_version(&s), 10);
    assert!(s.unreceipted_incoming(read_chat, 0, 10).unwrap().is_empty());
    assert_eq!(
        s.unreceipted_incoming(unread_chat, 0, 10).unwrap(),
        vec![("u1".to_string(), unread_chat.to_string())]
    );
}

#[test]
fn chat_receipts_off_roundtrips_and_survives_merge() {
    let s = Store::open_in_memory().unwrap();
    s.insert_message(&msg(A, "1", 10, false), true).unwrap();
    assert!(!chat(&s, A).receipts_off);
    s.set_chat_receipts_off(A, true).unwrap();
    assert!(chat(&s, A).receipts_off);
    s.set_chat_receipts_off(A, false).unwrap();
    assert!(!chat(&s, A).receipts_off);

    let other = "222@s.whatsapp.net";
    s.set_chat_receipts_off(A, true).unwrap();
    s.insert_message(&msg(other, "2", 11, false), true).unwrap();
    assert!(s.merge_chat(A, other).unwrap());
    assert!(chat(&s, other).receipts_off);
}

#[test]
fn unreceipted_incoming_filters_and_orders_newest_first() {
    let s = Store::open_in_memory().unwrap();
    for (id, ts, from_me) in [("a", 10, false), ("b", 20, false), ("c", 30, false), ("d", 40, false), ("mine", 50, true), ("old", 1, false)] {
        s.insert_message(&msg(A, id, ts, from_me), true).unwrap();
    }
    s.revoke_message(A, "d").unwrap();
    s.mark_receipts_sent(A, &["a".to_string()]).unwrap();

    let ids = |since, limit| -> Vec<String> {
        s.unreceipted_incoming(A, since, limit).unwrap().into_iter().map(|(id, _)| id).collect()
    };
    assert_eq!(ids(5, 10), ["c", "b"]);
    assert_eq!(ids(5, 1), ["c"]);
    assert_eq!(ids(0, 10), ["c", "b", "old"]);
    assert_eq!(ids(25, 10), ["c"]);
    assert_eq!(s.unreceipted_incoming(A, 5, 10).unwrap()[0].1, A);
    assert!(s.unreceipted_incoming("other@s.whatsapp.net", 0, 10).unwrap().is_empty());
}

#[test]
fn mark_receipts_sent_only_touches_given_ids() {
    let s = Store::open_in_memory().unwrap();
    for id in ["1", "2"] {
        s.insert_message(&msg(A, id, 10, false), true).unwrap();
    }
    s.mark_receipts_sent(A, &["1".to_string(), "missing".to_string()]).unwrap();
    let left = s.unreceipted_incoming(A, 0, 10).unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].0, "2");
}

#[test]
fn migration_9_upgrades_a_v8_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v8.sqlite");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        for sql in &MIGRATIONS[..8] {
            conn.execute_batch(sql).unwrap();
        }
        conn.execute_batch("PRAGMA user_version = 8").unwrap();
        conn.execute("INSERT INTO chats (jid, kind) VALUES (?1, 'dm')", [A]).unwrap();
        conn.execute(
            "INSERT INTO messages (chat_jid, id, sender_jid, from_me, ts, kind, text) \
             VALUES (?1, 'old', ?1, 0, 5, 'text', 'legacy')",
            [A],
        )
        .unwrap();
    }
    let s = Store::open(&path).unwrap();
    assert_eq!(user_version(&s), 10);
    let m = s.message(A, "old").unwrap().unwrap();
    assert_eq!(m.text.as_deref(), Some("legacy"));
    assert!(!m.starred && !m.mentions_me);
    assert_eq!(s.pinned_message(A).unwrap(), None);
    assert!(s.message_receipts(A, "old").unwrap().is_empty());
    assert!(s.set_starred(A, "old", true).unwrap());
    assert_eq!(s.starred_messages(10, None, None).unwrap().len(), 1);
}

#[test]
fn pinned_message_expires_and_clears() {
    let s = Store::open_in_memory().unwrap();
    s.insert_message(&msg(A, "1", 10, false), true).unwrap();
    assert!(s.set_pinned_message(A, Some("1"), 1_000).unwrap());
    // Same value again is not a change.
    assert!(!s.set_pinned_message(A, Some("1"), 1_000).unwrap());
    assert_eq!(s.pinned_message_at(A, 999).unwrap(), Some(("1".to_string(), 1_000)));
    assert_eq!(s.pinned_message_at(A, 1_000).unwrap(), None);
    assert_eq!(s.pinned_message_at(A, 5_000).unwrap(), None);
    assert_eq!(s.pinned_message_at("unknown@s.whatsapp.net", 0).unwrap(), None);

    assert!(!s.clear_pinned_message_if(A, "other").unwrap());
    assert!(s.clear_pinned_message_if(A, "1").unwrap());
    assert_eq!(s.pinned_message_at(A, 0).unwrap(), None);

    // Deleting or revoking the pinned message drops the pin.
    s.set_pinned_message(A, Some("1"), 1_000).unwrap();
    s.revoke_message(A, "1").unwrap();
    assert_eq!(s.pinned_message_at(A, 0).unwrap(), None);
    s.insert_message(&msg(A, "2", 11, false), true).unwrap();
    s.set_pinned_message(A, Some("2"), 1_000).unwrap();
    s.delete_message(A, "2").unwrap();
    assert_eq!(s.pinned_message_at(A, 0).unwrap(), None);
}

#[test]
fn starred_messages_list_newest_first_across_chats() {
    let s = Store::open_in_memory().unwrap();
    let b = "222@s.whatsapp.net";
    s.insert_message(&msg(A, "1", 10, false), true).unwrap();
    s.insert_message(&msg(b, "2", 30, true), true).unwrap();
    s.insert_message(&msg(A, "3", 20, false), true).unwrap();
    s.insert_message(&msg(A, "4", 40, false), true).unwrap();
    assert!(s.set_starred(A, "1", true).unwrap());
    assert!(s.set_starred(b, "2", true).unwrap());
    assert!(s.set_starred(A, "3", true).unwrap());
    assert!(!s.set_starred(A, "3", true).unwrap());
    assert!(!s.set_starred(A, "missing", true).unwrap());

    let ids = |rows: Vec<MessageRow>| rows.into_iter().map(|m| m.id).collect::<Vec<_>>();
    assert_eq!(ids(s.starred_messages(10, None, None).unwrap()), ["2", "3", "1"]);
    assert_eq!(ids(s.starred_messages(2, None, None).unwrap()), ["2", "3"]);
    assert_eq!(ids(s.starred_messages(10, None, Some((30, b, "2"))).unwrap()), ["3", "1"]);
    assert_eq!(ids(s.starred_messages(10, Some(A), None).unwrap()), ["3", "1"]);
    assert!(s.message(A, "1").unwrap().unwrap().starred);

    s.set_starred(b, "2", false).unwrap();
    s.revoke_message(A, "3").unwrap();
    assert_eq!(ids(s.starred_messages(10, None, None).unwrap()), ["1"]);
}

#[test]
fn starred_paging_visits_rows_tied_on_one_second_exactly_once() {
    let s = Store::open_in_memory().unwrap();
    let b = "222@s.whatsapp.net";
    for i in 0..250 {
        let chat = if i % 2 == 0 { A } else { b };
        let id = format!("m{i:03}");
        s.insert_message(&msg(chat, &id, 500, true), true).unwrap();
        assert!(s.set_starred(chat, &id, true).unwrap());
    }
    let page_all = |chat: Option<&str>| {
        let mut got: Vec<(String, String)> = Vec::new();
        loop {
            let before = got.last().map(|(c, i)| (500, c.as_str(), i.as_str()));
            let rows = s.starred_messages(37, chat, before).unwrap();
            let n = rows.len();
            got.extend(rows.into_iter().map(|m| (m.chat_jid, m.id)));
            if n < 37 {
                return got;
            }
        }
    };
    let all = page_all(None);
    let unique: std::collections::HashSet<_> = all.iter().collect();
    assert_eq!((all.len(), unique.len()), (250, 250));
    let in_a = page_all(Some(A));
    let unique: std::collections::HashSet<_> = in_a.iter().collect();
    assert_eq!((in_a.len(), unique.len()), (125, 125));
    assert!(in_a.iter().all(|(c, _)| c == A));
}

#[test]
fn mentions_me_is_stored_at_insert() {
    let s = Store::open_in_memory().unwrap();
    s.insert_message(&NewMessage { mentions_me: true, ..msg(A, "1", 10, false) }, true).unwrap();
    s.insert_message(&msg(A, "2", 11, false), true).unwrap();
    assert!(s.message(A, "1").unwrap().unwrap().mentions_me);
    assert!(!s.message(A, "2").unwrap().unwrap().mentions_me);
    let page = s.messages_page(A, None, 10).unwrap();
    assert_eq!(page.iter().map(|m| m.mentions_me).collect::<Vec<_>>(), [true, false]);
}

#[test]
fn receipts_keep_the_earliest_time_per_kind_and_sort_read_first() {
    let s = Store::open_in_memory().unwrap();
    let g = "1@g.us";
    s.insert_message(&msg(g, "m", 10, true), false).unwrap();
    let (p1, p2, p3, p4) = ("1@s.whatsapp.net", "2@s.whatsapp.net", "3@s.whatsapp.net", "4@s.whatsapp.net");
    s.record_receipt(g, "m", p1, ReceiptKind::Delivered, 100).unwrap();
    s.record_receipt(g, "m", p1, ReceiptKind::Delivered, 90).unwrap();
    s.record_receipt(g, "m", p1, ReceiptKind::Delivered, 120).unwrap();
    s.record_receipt(g, "m", p2, ReceiptKind::Delivered, 50).unwrap();
    s.record_receipt(g, "m", p2, ReceiptKind::Read, 200).unwrap();
    s.record_receipt(g, "m", p2, ReceiptKind::Read, 300).unwrap();
    s.record_receipt(g, "m", p3, ReceiptKind::Delivered, 60).unwrap();
    s.record_receipt(g, "m", p3, ReceiptKind::Read, 150).unwrap();
    s.record_receipt(g, "m", p3, ReceiptKind::Played, 160).unwrap();
    s.record_receipt(g, "m", p4, ReceiptKind::Played, 70).unwrap();

    let rows = s.message_receipts(g, "m").unwrap();
    assert_eq!(
        rows,
        vec![
            // Played counts as read.
            ReceiptRow { jid: p4.into(), delivered_at: None, read_at: None, played_at: Some(70) },
            ReceiptRow { jid: p3.into(), delivered_at: Some(60), read_at: Some(150), played_at: Some(160) },
            ReceiptRow { jid: p2.into(), delivered_at: Some(50), read_at: Some(200), played_at: None },
            ReceiptRow { jid: p1.into(), delivered_at: Some(90), read_at: None, played_at: None },
        ]
    );
    assert!(s.message_receipts(g, "other").unwrap().is_empty());

    s.delete_message(g, "m").unwrap();
    assert!(s.message_receipts(g, "m").unwrap().is_empty());
}

fn seed_clear_fixture(s: &Store) {
    // Order: i1(ts1) i2(ts2) i3(ts2) i4(ts5) i5(ts9); i3 starred, i4 pinned.
    for (id, ts) in [("i1", 1), ("i2", 2), ("i3", 2), ("i4", 5), ("i5", 9)] {
        s.insert_message(&msg(A, id, ts, false), true).unwrap();
        s.upsert_reaction(A, id, A, "x", 20, false).unwrap();
        s.record_receipt(A, id, A, ReceiptKind::Read, 30).unwrap();
    }
    assert!(s.set_starred(A, "i3", true).unwrap());
    s.set_pinned_message(A, Some("i4"), i64::MAX).unwrap();
}

fn live_ids(s: &Store) -> Vec<String> {
    s.messages_page(A, None, 50).unwrap().into_iter().map(|m| m.id).collect()
}

fn extras(s: &Store, id: &str) -> (usize, usize) {
    (s.reactions_for(A, &[id.to_string()]).unwrap().len(), s.message_receipts(A, id).unwrap().len())
}

#[test]
fn bounded_clear_removes_only_rows_up_to_the_bound_with_their_extras() {
    let s = Store::open_in_memory().unwrap();
    seed_clear_fixture(&s);
    assert_eq!(chat(&s, A).unread, 5);
    s.clear_chat(A, Some((2, "i2"))).unwrap();
    // i3 shares the bound's timestamp but was inserted later: kept.
    assert_eq!(live_ids(&s), ["i3", "i4", "i5"]);
    for gone in ["i1", "i2"] {
        assert_eq!(extras(&s, gone), (0, 0), "{gone}");
    }
    for kept in ["i3", "i4", "i5"] {
        assert_eq!(extras(&s, kept), (1, 1), "{kept}");
    }
    let c = chat(&s, A);
    assert_eq!((c.last_ts, c.last_preview.as_deref()), (9, Some("mi5")));
    assert_eq!(c.unread, 3, "unread falls back to the incoming rows that remain");
    assert_eq!(s.pinned_message_at(A, 0).unwrap().map(|p| p.0), Some("i4".to_string()), "pin survives");
}

#[test]
fn bounded_clear_releases_the_pin_only_when_its_message_is_removed() {
    let s = Store::open_in_memory().unwrap();
    seed_clear_fixture(&s);
    s.clear_chat(A, Some((5, "i4"))).unwrap();
    assert_eq!(live_ids(&s), ["i5"]);
    assert_eq!(s.pinned_message_at(A, 0).unwrap(), None);
    let c = chat(&s, A);
    assert_eq!((c.last_ts, c.unread), (9, 1));
}

#[test]
fn bounded_clear_keeping_starred_keeps_them_with_extras_and_recomputes_summary() {
    let s = Store::open_in_memory().unwrap();
    seed_clear_fixture(&s);
    s.clear_chat_keep_starred(A, Some((5, "i4"))).unwrap();
    assert_eq!(live_ids(&s), ["i3", "i5"]);
    assert_eq!(extras(&s, "i3"), (1, 1), "starred message keeps reactions and receipts");
    for gone in ["i1", "i2", "i4"] {
        assert_eq!(extras(&s, gone), (0, 0), "{gone}");
    }
    assert_eq!(extras(&s, "i5"), (1, 1));
    assert_eq!(s.pinned_message_at(A, 0).unwrap(), None, "pinned i4 was removed");
    let c = chat(&s, A);
    assert_eq!((c.last_ts, c.last_preview.as_deref()), (9, Some("mi5")));
    assert_eq!(c.unread, 2);
}

#[test]
fn unbounded_clear_keeping_starred_resets_unread_and_keeps_a_starred_pin() {
    let s = Store::open_in_memory().unwrap();
    seed_clear_fixture(&s);
    s.set_pinned_message(A, Some("i3"), i64::MAX).unwrap();
    s.clear_chat_keep_starred(A, None).unwrap();
    assert_eq!(live_ids(&s), ["i3"]);
    assert_eq!(extras(&s, "i3"), (1, 1));
    assert_eq!(s.pinned_message_at(A, 0).unwrap().map(|p| p.0), Some("i3".to_string()));
    let c = chat(&s, A);
    assert_eq!((c.unread, c.last_ts, c.last_preview.as_deref()), (0, 2, Some("mi3")));
}

#[test]
fn bounded_clear_leaves_other_chats_receipts_and_reactions_alone() {
    let s = Store::open_in_memory().unwrap();
    let b = "222@s.whatsapp.net";
    seed_clear_fixture(&s);
    // Same message id in another chat must not be touched.
    s.insert_message(&msg(b, "i1", 1, false), true).unwrap();
    s.upsert_reaction(b, "i1", b, "x", 20, false).unwrap();
    s.record_receipt(b, "i1", b, ReceiptKind::Read, 30).unwrap();
    s.clear_chat(A, Some((9, "i5"))).unwrap();
    assert!(live_ids(&s).is_empty());
    assert_eq!(s.reactions_for(b, &["i1".to_string()]).unwrap().len(), 1);
    assert_eq!(s.message_receipts(b, "i1").unwrap().len(), 1);
}

#[test]
fn purge_statuses_keeps_receipts_of_recent_statuses_and_chat_messages() {
    let s = Store::open_in_memory().unwrap();
    s.insert_message(&status_msg(A, "old", 100), true).unwrap();
    s.insert_message(&status_msg(A, "new", 5000), true).unwrap();
    s.insert_message(&msg(A, "old", 100, true), false).unwrap();
    for (chat, id) in [(STATUS_JID, "old"), (STATUS_JID, "new"), (A, "old")] {
        s.record_receipt(chat, id, A, ReceiptKind::Read, 1).unwrap();
    }
    assert_eq!(s.purge_statuses(1000).unwrap().removed, 1);
    assert!(s.message_receipts(STATUS_JID, "old").unwrap().is_empty());
    assert_eq!(s.message_receipts(STATUS_JID, "new").unwrap().len(), 1);
    assert_eq!(s.message_receipts(A, "old").unwrap().len(), 1, "same id in a normal chat is untouched");
}

#[test]
fn delete_message_removes_its_receipts_and_pin_but_not_a_neighbours() {
    let s = Store::open_in_memory().unwrap();
    seed_clear_fixture(&s);
    s.delete_message(A, "i3").unwrap();
    assert_eq!(s.pinned_message_at(A, 0).unwrap().map(|p| p.0), Some("i4".to_string()));
    assert_eq!(extras(&s, "i3"), (0, 0));
    s.delete_message(A, "i4").unwrap();
    assert_eq!(s.pinned_message_at(A, 0).unwrap(), None);
    assert_eq!(extras(&s, "i4"), (0, 0));
    assert_eq!(extras(&s, "i5"), (1, 1));
}

#[test]
fn migration_9_keeps_rows_and_adds_pin_and_receipt_columns() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v8b.sqlite");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        for sql in &MIGRATIONS[..8] {
            conn.execute_batch(sql).unwrap();
        }
        conn.execute_batch("PRAGMA user_version = 8").unwrap();
        conn.execute("INSERT INTO chats (jid, kind, unread, last_ts) VALUES (?1, 'dm', 2, 7)", [A]).unwrap();
        for (id, ts) in [("a", 5), ("b", 7)] {
            conn.execute(
                "INSERT INTO messages (chat_jid, id, sender_jid, from_me, ts, kind, text) \
                 VALUES (?1, ?2, ?1, 0, ?3, 'text', ?2)",
                rusqlite::params![A, id, ts],
            )
            .unwrap();
        }
    }
    let s = Store::open(&path).unwrap();
    assert_eq!(user_version(&s), 10);
    assert_eq!(live_ids(&s), ["a", "b"]);
    assert_eq!((chat(&s, A).unread, chat(&s, A).last_ts), (2, 7));
    // New columns/tables are usable on the migrated file.
    assert!(s.set_pinned_message(A, Some("a"), i64::MAX).unwrap());
    s.record_receipt(A, "a", A, ReceiptKind::Delivered, 9).unwrap();
    assert_eq!(s.message_receipts(A, "a").unwrap().len(), 1);
    drop(s);
    let again = Store::open(&path).unwrap();
    assert_eq!(user_version(&again), 10);
    assert_eq!(again.pinned_message_at(A, 0).unwrap().map(|p| p.0), Some("a".to_string()));
}

#[test]
fn migration_10_keeps_rows_and_adds_geo_expiry_and_timer_columns() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v9.sqlite");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        for sql in &MIGRATIONS[..9] {
            conn.execute_batch(sql).unwrap();
        }
        conn.execute_batch("PRAGMA user_version = 9").unwrap();
        conn.execute("INSERT INTO chats (jid, kind, unread, last_ts) VALUES (?1, 'dm', 1, 7)", [A]).unwrap();
        conn.execute(
            "INSERT INTO messages (chat_jid, id, sender_jid, from_me, ts, kind, text) \
             VALUES (?1, 'a', ?1, 0, 7, 'text', 'hi')",
            [A],
        )
        .unwrap();
    }
    let s = Store::open(&path).unwrap();
    assert_eq!(user_version(&s), 10);
    assert_eq!(live_ids(&s), ["a"]);
    let c = chat(&s, A);
    assert_eq!((c.unread, c.last_ts, c.ephemeral_secs, c.ephemeral_ts), (1, 7, 0, 0));
    let m = s.message(A, "a").unwrap().unwrap();
    assert_eq!((m.geo_lat, m.geo_lon, m.expires_at), (None, None, None));
    assert!(s.set_chat_ephemeral(A, 86_400, 5).unwrap());
    s.meta_set("k", "v").unwrap();
    drop(s);
    let again = Store::open(&path).unwrap();
    assert_eq!(chat(&again, A).ephemeral_secs, 86_400);
    assert_eq!(again.meta_get("k").unwrap().as_deref(), Some("v"));
}

#[test]
fn chat_ephemeral_applies_only_when_not_older() {
    let s = Store::open_in_memory().unwrap();
    s.insert_message(&msg(A, "1", 10, true), false).unwrap();
    assert!(s.set_chat_ephemeral(A, 604_800, 100).unwrap());
    assert!(!s.set_chat_ephemeral(A, 0, 99).unwrap());
    assert_eq!((chat(&s, A).ephemeral_secs, chat(&s, A).ephemeral_ts), (604_800, 100));
    assert!(s.set_chat_ephemeral(A, 86_400, 100).unwrap());
    assert!(s.set_chat_ephemeral(A, 0, 101).unwrap());
    assert_eq!((chat(&s, A).ephemeral_secs, chat(&s, A).ephemeral_ts), (0, 101));
    assert!(!s.set_chat_ephemeral("nochat@s.whatsapp.net", 5, 500).unwrap());
}

#[test]
fn expired_messages_excludes_starred_unexpired_and_never_expiring() {
    let s = Store::open_in_memory().unwrap();
    let with = |id: &str, expires_at: Option<i64>| NewMessage {
        expires_at,
        local_path: Some(format!("/tmp/{id}")),
        ..msg(A, id, 10, false)
    };
    for m in [
        with("old", Some(50)),
        with("edge", Some(100)),
        with("future", Some(101)),
        with("forever", None),
        with("starred", Some(10)),
    ] {
        s.insert_message(&m, false).unwrap();
    }
    s.set_starred(A, "starred", true).unwrap();
    let got = s.expired_messages(100, 10).unwrap();
    assert_eq!(
        got,
        vec![
            (A.to_string(), "old".to_string(), Some("/tmp/old".to_string())),
            (A.to_string(), "edge".to_string(), Some("/tmp/edge".to_string())),
        ]
    );
    assert_eq!(s.expired_messages(100, 1).unwrap().len(), 1);
}

#[test]
fn meta_roundtrip_and_overwrite() {
    let s = Store::open_in_memory().unwrap();
    assert_eq!(s.meta_get("x").unwrap(), None);
    s.meta_set("x", "1").unwrap();
    s.meta_set("x", "2").unwrap();
    assert_eq!(s.meta_get("x").unwrap().as_deref(), Some("2"));
}

#[test]
fn address_book_contacts_are_those_with_a_synced_full_name() {
    let s = Store::open_in_memory().unwrap();
    let p = |jid: &str, f: Option<&str>, pu: Option<&str>| ContactPatch {
        jid: jid.into(),
        full_name: f.map(Into::into),
        push_name: pu.map(Into::into),
        phone: None,
    };
    s.upsert_contact(&p("2@s.whatsapp.net", Some("Bob"), Some("bobby"))).unwrap();
    s.upsert_contact(&p("1@s.whatsapp.net", Some("Ann"), None)).unwrap();
    s.upsert_contact(&p("3@s.whatsapp.net", None, Some("Push only"))).unwrap();
    s.upsert_contact(&p("4@s.whatsapp.net", Some("  "), None)).unwrap();
    s.upsert_contact(&p("5@s.whatsapp.net", None, None)).unwrap();
    assert_eq!(
        s.address_book_contacts().unwrap(),
        ["1@s.whatsapp.net", "2@s.whatsapp.net"]
    );
}
