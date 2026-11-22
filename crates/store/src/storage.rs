//! Storage manager support: which messages hold a cached media file, and the
//! pure rules that decide which of those files may be deleted. Messages are
//! never removed; clearing only forgets the local path of the row.

use std::path::{Component, Path, PathBuf};

use rusqlite::params;

use crate::model::{MessageKind, MessageStatus};
use crate::{Result, Store};

/// Sub-folders of the media directory that are not downloaded media: profile
/// pictures and files staged for sending.
const PROTECTED_DIRS: [&str; 2] = ["avatars", "outgoing"];

/// The groups shown in the storage bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaClass {
    Photos,
    Videos,
    Voice,
    Documents,
    Stickers,
    Other,
}

impl MediaClass {
    pub const ALL: [MediaClass; 6] = [
        MediaClass::Photos,
        MediaClass::Videos,
        MediaClass::Voice,
        MediaClass::Documents,
        MediaClass::Stickers,
        MediaClass::Other,
    ];

    pub fn of(kind: MessageKind) -> MediaClass {
        match kind {
            MessageKind::Image => MediaClass::Photos,
            MessageKind::Video => MediaClass::Videos,
            MessageKind::Audio | MessageKind::Voice => MediaClass::Voice,
            MessageKind::Document => MediaClass::Documents,
            MessageKind::Sticker => MediaClass::Stickers,
            MessageKind::Text | MessageKind::Other => MediaClass::Other,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            MediaClass::Photos => "Photos",
            MediaClass::Videos => "Videos",
            MediaClass::Voice => "Voice",
            MediaClass::Documents => "Documents",
            MediaClass::Stickers => "Stickers",
            MediaClass::Other => "Other",
        }
    }
}

/// A message that has a cached file.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaFile {
    pub chat_jid: String,
    pub id: String,
    pub kind: MessageKind,
    pub ts: i64,
    pub local_path: String,
    pub starred: bool,
    pub from_me: bool,
    pub status: MessageStatus,
}

/// What to clear. Empty `classes` means every type; `older_than` is a unix
/// second cutoff (messages strictly older are matched).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MediaFilter {
    pub chat: Option<String>,
    pub classes: Vec<MediaClass>,
    pub older_than: Option<i64>,
}

impl MediaFilter {
    fn matches(&self, f: &MediaFile) -> bool {
        self.chat.as_deref().is_none_or(|c| c == f.chat_jid)
            && (self.classes.is_empty() || self.classes.contains(&MediaClass::of(f.kind)))
            && self.older_than.is_none_or(|t| f.ts < t)
    }
}

/// One file the plan allows deleting.
#[derive(Debug, Clone, PartialEq)]
pub struct Doomed {
    pub chat_jid: String,
    pub id: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeletionPlan {
    pub delete: Vec<Doomed>,
    pub kept_starred: usize,
    pub kept_pending: usize,
    /// Matched rows whose path is not inside the media cache.
    pub rejected: usize,
}

/// Whether `path` lies inside the downloaded-media part of `media_root`.
/// Purely lexical (no `..`, no protected sub-folder); the deleter must still
/// canonicalize before touching the file.
pub fn is_cache_file(path: &Path, media_root: &Path) -> bool {
    if path.components().any(|c| matches!(c, Component::ParentDir | Component::CurDir)) {
        return false;
    }
    let Ok(rest) = path.strip_prefix(media_root) else { return false };
    let mut parts = rest.components();
    let Some(Component::Normal(first)) = parts.next() else { return false };
    !PROTECTED_DIRS.iter().any(|p| first.eq_ignore_ascii_case(p))
}

/// Chooses the files to delete. Starred messages and our own messages that
/// are still pending (or failed, so a retry needs the file) are always kept.
pub fn plan_deletion(files: &[MediaFile], filter: &MediaFilter, media_root: &Path) -> DeletionPlan {
    let mut plan = DeletionPlan::default();
    for f in files.iter().filter(|f| filter.matches(f)) {
        if f.starred {
            plan.kept_starred += 1;
        } else if f.from_me && matches!(f.status, MessageStatus::Pending | MessageStatus::Failed) {
            plan.kept_pending += 1;
        } else if !is_cache_file(Path::new(&f.local_path), media_root) {
            plan.rejected += 1;
        } else {
            plan.delete.push(Doomed {
                chat_jid: f.chat_jid.clone(),
                id: f.id.clone(),
                path: PathBuf::from(&f.local_path),
            });
        }
    }
    plan
}

impl Store {
    /// Every message with a cached file (optionally of one chat).
    pub fn media_files(&self, chat_jid: Option<&str>) -> Result<Vec<MediaFile>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT chat_jid, id, kind, ts, local_path, starred, from_me, status FROM messages \
             WHERE local_path IS NOT NULL AND local_path <> '' AND (?1 IS NULL OR chat_jid = ?1)",
        )?;
        let rows = stmt
            .query_map(params![chat_jid], |r| {
                Ok(MediaFile {
                    chat_jid: r.get(0)?,
                    id: r.get(1)?,
                    kind: MessageKind::parse(&r.get::<_, String>(2)?),
                    ts: r.get(3)?,
                    local_path: r.get(4)?,
                    starred: r.get::<_, i64>(5)? != 0,
                    from_me: r.get::<_, i64>(6)? != 0,
                    status: MessageStatus::from_i64(r.get(7)?),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Forgets the local file of each `(chat_jid, id)` so the UI offers the
    /// download again. Starred rows are never changed. Returns the rows updated.
    pub fn clear_local_paths(&self, items: &[(String, String)]) -> Result<usize> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut n = 0;
        {
            let mut stmt = tx.prepare(
                "UPDATE messages SET local_path = NULL WHERE chat_jid = ?1 AND id = ?2 AND starred = 0",
            )?;
            for (chat, id) in items {
                n += stmt.execute(params![chat, id])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::NewMessage;

    const ROOT: &str = "/data/p1/media";

    fn file(id: &str, kind: MessageKind, ts: i64) -> MediaFile {
        MediaFile {
            chat_jid: "a@s.whatsapp.net".into(),
            id: id.into(),
            kind,
            ts,
            local_path: format!("{ROOT}/a/{id}.bin"),
            starred: false,
            from_me: false,
            status: MessageStatus::Read,
        }
    }

    fn ids(p: &DeletionPlan) -> Vec<&str> {
        p.delete.iter().map(|d| d.id.as_str()).collect()
    }

    #[test]
    fn starred_media_is_kept() {
        let mut s = file("s", MessageKind::Image, 1);
        s.starred = true;
        let plan = plan_deletion(&[s, file("x", MessageKind::Image, 1)], &MediaFilter::default(), Path::new(ROOT));
        assert_eq!(ids(&plan), ["x"]);
        assert_eq!(plan.kept_starred, 1);
    }

    #[test]
    fn pending_and_failed_uploads_are_kept_but_received_media_is_not() {
        let mut p = file("p", MessageKind::Video, 1);
        p.from_me = true;
        p.status = MessageStatus::Pending;
        let mut f = file("f", MessageKind::Video, 1);
        f.from_me = true;
        f.status = MessageStatus::Failed;
        let mut sent = file("sent", MessageKind::Video, 1);
        sent.from_me = true;
        sent.status = MessageStatus::Delivered;
        let mut incoming = file("in", MessageKind::Video, 1);
        incoming.status = MessageStatus::Pending;
        let plan = plan_deletion(&[p, f, sent, incoming], &MediaFilter::default(), Path::new(ROOT));
        assert_eq!(ids(&plan), ["sent", "in"]);
        assert_eq!(plan.kept_pending, 2);
    }

    #[test]
    fn paths_outside_the_cache_are_rejected() {
        let mut outside = file("o", MessageKind::Image, 1);
        outside.local_path = "/home/me/Pictures/o.jpg".into();
        let mut dotdot = file("d", MessageKind::Image, 1);
        dotdot.local_path = format!("{ROOT}/../../secret.db");
        let mut sibling = file("sib", MessageKind::Image, 1);
        sibling.local_path = "/data/p1/media-evil/x.jpg".into();
        let mut avatar = file("av", MessageKind::Image, 1);
        avatar.local_path = format!("{ROOT}/avatars/x.jpg");
        let mut outgoing = file("og", MessageKind::Image, 1);
        outgoing.local_path = format!("{ROOT}/outgoing/x.jpg");
        let mut root_itself = file("r", MessageKind::Image, 1);
        root_itself.local_path = ROOT.into();
        let ok = file("ok", MessageKind::Image, 1);
        let all = [outside, dotdot, sibling, avatar, outgoing, root_itself, ok];
        let plan = plan_deletion(&all, &MediaFilter::default(), Path::new(ROOT));
        assert_eq!(ids(&plan), ["ok"]);
        assert_eq!(plan.rejected, 6);
    }

    #[test]
    fn filters_by_chat_type_and_age() {
        let mut other_chat = file("c", MessageKind::Image, 1);
        other_chat.chat_jid = "b@s.whatsapp.net".into();
        let all = [
            file("old-photo", MessageKind::Image, 10),
            file("new-photo", MessageKind::Image, 100),
            file("old-voice", MessageKind::Voice, 10),
            file("old-audio", MessageKind::Audio, 10),
            other_chat,
        ];
        let root = Path::new(ROOT);
        let f = MediaFilter { chat: Some("a@s.whatsapp.net".into()), classes: vec![MediaClass::Photos], older_than: Some(50) };
        assert_eq!(ids(&plan_deletion(&all, &f, root)), ["old-photo"]);
        let f = MediaFilter { classes: vec![MediaClass::Voice], ..Default::default() };
        assert_eq!(ids(&plan_deletion(&all, &f, root)), ["old-voice", "old-audio"]);
        assert_eq!(plan_deletion(&all, &MediaFilter::default(), root).delete.len(), 5);
    }

    fn new_msg(id: &str, kind: MessageKind, path: Option<&str>) -> NewMessage {
        NewMessage {
            chat_jid: "a@s.whatsapp.net".into(),
            id: id.into(),
            sender_jid: "a@s.whatsapp.net".into(),
            ts: 5,
            kind,
            local_path: path.map(String::from),
            ..Default::default()
        }
    }

    #[test]
    fn media_files_lists_rows_with_a_path_and_clearing_keeps_the_rows() {
        let store = Store::open_in_memory().expect("open");
        store.upsert_chat("a@s.whatsapp.net", crate::ChatKind::Dm, None).expect("chat");
        store.insert_message(&new_msg("1", MessageKind::Image, Some("/m/1.jpg")), false).expect("insert");
        store.insert_message(&new_msg("2", MessageKind::Voice, Some("/m/2.ogg")), false).expect("insert");
        store.insert_message(&new_msg("3", MessageKind::Text, None), false).expect("insert");
        store.set_starred("a@s.whatsapp.net", "2", true).expect("star");

        let files = store.media_files(None).expect("files");
        assert_eq!(files.len(), 2);
        assert!(files.iter().any(|f| f.id == "2" && f.starred));
        assert!(store.media_files(Some("other@s.whatsapp.net")).expect("files").is_empty());

        let items = vec![("a@s.whatsapp.net".to_string(), "1".to_string()), ("a@s.whatsapp.net".to_string(), "2".to_string())];
        assert_eq!(store.clear_local_paths(&items).expect("clear"), 1);
        let left = store.media_files(None).expect("files");
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, "2");
        assert_eq!(store.messages_page("a@s.whatsapp.net", None, 10).expect("page").len(), 3);
    }
}
