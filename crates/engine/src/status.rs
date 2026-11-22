//! Status updates live for 24 hours; this removes the expired ones.

use std::path::{Component, Path};
use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;
use wa_store::Store;

use crate::api::UiEvent;
use crate::events::now_secs;

/// How long a status stays available.
pub const STATUS_TTL_SECS: i64 = 24 * 60 * 60;
/// A reaction whose message is still unknown after this long never gets one.
pub const ORPHAN_REACTION_TTL_SECS: i64 = 7 * 24 * 60 * 60;
/// Most reactions kept per chat.
pub const REACTIONS_PER_CHAT_CAP: u32 = 50_000;
const PURGE_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Whether `path` is safe to delete as part of our media cache: it must lie
/// under `media_dir` without any `..` component (which `starts_with` would
/// not see through) and, when it exists, resolve there after following links.
pub(crate) fn is_inside_cache(path: &Path, media_dir: &Path) -> bool {
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return false;
    }
    if !path.starts_with(media_dir) {
        return false;
    }
    match (path.canonicalize(), media_dir.canonicalize()) {
        (Ok(real), Ok(root)) => real.starts_with(root),
        // Nothing to delete (or nothing to compare against): the removal below
        // then fails with NotFound, which is ignored.
        (Err(e), _) if e.kind() == std::io::ErrorKind::NotFound => true,
        _ => false,
    }
}

/// Deletes the statuses older than 24 hours (rows and cached media) as seen
/// from `now`, and trims orphaned/excess reactions in the same pass. Returns
/// how many statuses were removed.
pub fn purge_expired(store: &Store, media_dir: &Path, now: i64) -> Result<usize, wa_store::StoreError> {
    match store.purge_reactions(now.saturating_sub(ORPHAN_REACTION_TTL_SECS), REACTIONS_PER_CHAT_CAP) {
        Ok(0) => {}
        Ok(n) => log::info!("purged {n} stale reactions"),
        Err(e) => log::warn!("could not purge stale reactions: {e}"),
    }
    let purge = store.purge_statuses(now.saturating_sub(STATUS_TTL_SECS))?;
    for path in purge.media_paths.iter().map(Path::new) {
        if !is_inside_cache(path, media_dir) {
            continue;
        }
        if let Err(e) = std::fs::remove_file(path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!("cannot delete expired status media {}: {e}", path.display());
        }
    }
    Ok(purge.removed)
}

/// Purges once an hour until aborted. The first purge is the caller's
/// (at start); this waits a full interval before the next one.
pub async fn purge_loop(store: std::sync::Arc<Store>, media_dir: std::path::PathBuf, ui: UnboundedSender<UiEvent>) {
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + PURGE_INTERVAL, PURGE_INTERVAL);
    loop {
        ticker.tick().await;
        let (store, media_dir, ui_alive) = (store.clone(), media_dir.clone(), ui.clone());
        let job = move || crate::events::with_store_and_ui(store, ui_alive, |s| purge_expired(s, &media_dir, now_secs()));
        match tokio::task::spawn_blocking(job).await {
            Ok(Ok(0)) => {}
            Ok(Ok(n)) => {
                log::info!("purged {n} expired statuses");
                if ui.send(UiEvent::StatusesChanged).is_err() {
                    log::debug!("UI receiver is gone, dropping event");
                }
            }
            Ok(Err(e)) => log::error!("purging statuses failed: {e}"),
            Err(e) => log::error!("status purge task failed: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wa_store::{MessageKind, NewMessage, STATUS_JID};

    fn status(id: &str, ts: i64, path: Option<String>) -> NewMessage {
        NewMessage {
            chat_jid: STATUS_JID.into(),
            id: id.into(),
            sender_jid: "1@s.whatsapp.net".into(),
            from_me: false,
            ts,
            kind: MessageKind::Image,
            text: None,
            quoted_id: None,
            quoted_preview: None,
            media_mime: None,
            media_name: None,
            media_size: None,
            media_duration: None,
            thumbnail: None,
            local_path: path,
            raw: None,
            ..Default::default()
        }
    }

    #[test]
    fn expired_statuses_and_their_cached_media_are_removed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let media = tmp.path().join("media");
        std::fs::create_dir_all(&media).expect("mkdir");
        let cached = media.join("old.jpg");
        let outside = tmp.path().join("precious.jpg");
        std::fs::write(&cached, b"x").expect("write");
        std::fs::write(&outside, b"y").expect("write");

        let now = 1_000_000;
        let store = Store::open_in_memory().expect("store");
        let path = |p: &Path| Some(p.to_string_lossy().into_owned());
        store.insert_message(&status("old", now - STATUS_TTL_SECS - 1, path(&cached)), false).expect("insert");
        store.insert_message(&status("old2", now - STATUS_TTL_SECS - 5, path(&outside)), false).expect("insert");
        store.insert_message(&status("fresh", now - STATUS_TTL_SECS + 60, None), false).expect("insert");

        assert_eq!(purge_expired(&store, &media, now).expect("purge"), 2);
        assert!(store.message(STATUS_JID, "old").expect("q").is_none());
        assert!(store.message(STATUS_JID, "fresh").expect("q").is_some());
        assert!(!cached.exists(), "cached media of an expired status is deleted");
        assert!(outside.exists(), "files outside the media cache are never touched");
        assert_eq!(purge_expired(&store, &media, now).expect("purge"), 0);
    }

    #[test]
    fn paths_with_parent_components_or_outside_the_cache_are_rejected() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let media = tmp.path().join("media");
        std::fs::create_dir_all(&media).expect("mkdir");
        let outside = tmp.path().join("precious.jpg");
        std::fs::write(&outside, b"y").expect("write");
        let inside = media.join("ok.jpg");
        std::fs::write(&inside, b"x").expect("write");

        assert!(is_inside_cache(&inside, &media));
        assert!(is_inside_cache(&media.join("missing.jpg"), &media));
        assert!(!is_inside_cache(&media.join("..").join("precious.jpg"), &media));
        assert!(!is_inside_cache(&media.join("sub").join("..").join("..").join("precious.jpg"), &media));
        assert!(!is_inside_cache(&outside, &media));

        let now = 1_000_000;
        let store = Store::open_in_memory().expect("store");
        let sneaky = media.join("..").join("precious.jpg");
        let path = Some(sneaky.to_string_lossy().into_owned());
        store.insert_message(&status("old", now - STATUS_TTL_SECS - 1, path), false).expect("insert");
        assert_eq!(purge_expired(&store, &media, now).expect("purge"), 1);
        assert!(outside.exists(), "a `..` path never deletes outside the cache");
    }

    #[test]
    fn purge_also_trims_old_orphan_reactions() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let now = 10_000_000;
        let store = Store::open_in_memory().expect("store");
        let chat = "1@s.whatsapp.net";
        store.upsert_reaction(chat, "gone", chat, "x", now - ORPHAN_REACTION_TTL_SECS - 1, false).expect("r");
        store.upsert_reaction(chat, "recent", chat, "x", now - 60, false).expect("r");
        purge_expired(&store, tmp.path(), now).expect("purge");
        assert!(store.reaction_of(chat, "gone", chat).expect("q").is_none());
        assert!(store.reaction_of(chat, "recent", chat).expect("q").is_some());
    }
}
