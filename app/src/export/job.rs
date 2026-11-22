//! Runs one export: pages through the chat, copies cached media files and
//! feeds the writer. Meant for a worker thread; it checks the cancel flag
//! between messages and removes what it wrote when cancelled or failed.

use std::fs::{self, OpenOptions};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, Local, SecondsFormat};
use wa_store::{is_cache_file, ExportCursor, MessageRow, Store};

use super::util::media_file_name;
use super::{entry_from, is_view_once, writer, ChatCtx, Extras, Format, Head, Writer, MEDIA_DIR};
use crate::models::{is_media, is_poll_row};

const PAGE: u32 = 500;

pub struct Job {
    pub chat_jid: String,
    pub ctx: ChatCtx,
    pub format: Format,
    /// `from <= ts < to` in Unix seconds; a missing bound is open.
    pub from: Option<i64>,
    pub to: Option<i64>,
    pub range_text: String,
    pub include_media: bool,
    pub dest: PathBuf,
    pub media_root: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// `dest` is the folder actually written (may differ from the requested one).
    Done { messages: u64, media_copied: usize, media_missing: usize, dest: PathBuf },
    Cancelled,
}

/// What one run created, so cleanup removes exactly that and nothing else.
#[derive(Default)]
struct Created {
    files: Vec<PathBuf>,
    /// In creation order; removed in reverse and only when empty.
    dirs: Vec<PathBuf>,
}

impl Created {
    fn clean_up(&self) {
        for f in &self.files {
            let _ = fs::remove_file(f);
        }
        for d in self.dirs.iter().rev() {
            let _ = fs::remove_dir(d);
        }
    }
}

/// Picks and creates the folder the export writes into: `wanted` when it is
/// new or an empty folder, otherwise "<name> (2)", "(3)", … (the first free
/// one). The bool tells whether this call created the folder. Existing files
/// are therefore never written to.
pub(super) fn claim_dest(wanted: &Path) -> std::io::Result<(PathBuf, bool)> {
    use std::io::{Error, ErrorKind};
    let name = wanted
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "the path has no folder name"))?;
    if let Some(parent) = wanted.parent() {
        fs::create_dir_all(parent)?;
    }
    for n in 1..=1000u32 {
        let cand = if n == 1 { wanted.to_path_buf() } else { wanted.with_file_name(format!("{name} ({n})")) };
        match fs::create_dir(&cand) {
            Ok(()) => return Ok((cand, true)),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                let empty = cand.is_dir() && fs::read_dir(&cand).is_ok_and(|mut d| d.next().is_none());
                if empty {
                    return Ok((cand, false));
                }
            }
            Err(e) => return Err(e),
        }
    }
    Err(Error::new(ErrorKind::AlreadyExists, "no free folder name"))
}

/// Exports the chat. `progress(done, total)` is called after every page.
pub fn run(
    store: &Store,
    job: &Job,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<Outcome, String> {
    let (dest, created_dest) = claim_dest(&job.dest).map_err(|e| format!("Could not create the folder: {e}"))?;
    let mut created = Created::default();
    if created_dest {
        created.dirs.push(dest.clone());
    }
    let result = export_into(store, job, cancel, progress, &dest, &mut created);
    if !matches!(result, Ok(Outcome::Done { .. })) {
        // Nothing half-written stays behind, and nothing older is touched.
        created.clean_up();
    }
    result
}

fn export_into(
    store: &Store,
    job: &Job,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, u64),
    dest: &Path,
    created: &mut Created,
) -> Result<Outcome, String> {
    let io_err = |e: std::io::Error| format!("Could not write the export: {e}");
    let store_err = |e: wa_store::StoreError| format!("Could not read the chat: {e}");
    let total = store.export_count(&job.chat_jid, job.from, job.to).map_err(store_err)?;
    let out_path = dest.join(job.format.file_name());
    let partial = dest.join(format!("{}.partial", job.format.file_name()));
    let file = OpenOptions::new().write(true).create_new(true).open(&partial).map_err(io_err)?;
    created.files.push(partial.clone());
    let mut w: Box<dyn Writer + '_> = writer(job.format, BufWriter::new(file));
    w.start(&Head {
        title: job.ctx.name.clone(),
        is_group: job.ctx.is_group,
        exported_at: Local::now().format("%Y-%m-%d %H:%M").to_string(),
        range_text: job.range_text.clone(),
    })
    .map_err(io_err)?;
    progress(0, total);
    let root = fs::canonicalize(&job.media_root).ok();
    let (mut done, mut media_copied, mut media_missing) = (0u64, 0usize, 0usize);
    let mut after: Option<ExportCursor> = None;
    loop {
        let page = store.export_page(&job.chat_jid, job.from, job.to, after, PAGE).map_err(store_err)?;
        let Some(last) = page.last().map(|(cursor, _)| *cursor) else { break };
        let ids: Vec<String> = page.iter().map(|(_, m)| m.id.clone()).collect();
        let mut reactions = store.reactions_for(&job.chat_jid, &ids).map_err(store_err)?;
        for (_, m) in &page {
            if cancel.load(Ordering::Relaxed) {
                return Ok(Outcome::Cancelled);
            }
            let poll = if is_poll_row(m) { store.poll_for(&job.chat_jid, &m.id).map_err(store_err)? } else { None };
            let relative = match (job.include_media, root.as_deref()) {
                (true, Some(root)) => copy_media(m, root, dest, media_copied + 1, created),
                _ => None,
            };
            let (when, iso) = local_time(m.ts);
            let entry = entry_from(
                m,
                &job.ctx,
                Extras {
                    when,
                    iso,
                    reactions: reactions.remove(&m.id).unwrap_or_default().into_iter().map(|(e, n, _)| (e, n)).collect(),
                    poll,
                    include_media: job.include_media,
                    copied: relative,
                },
            );
            match &entry.media {
                Some(media) if media.file.is_some() => media_copied += 1,
                Some(media) if media.missing => media_missing += 1,
                _ => {}
            }
            w.entry(&entry).map_err(io_err)?;
            done += 1;
        }
        progress(done, total);
        after = Some(last);
    }
    w.finish(done).map_err(io_err)?;
    drop(w);
    fs::rename(&partial, &out_path).map_err(io_err)?;
    created.files.retain(|f| *f != partial);
    created.files.push(out_path);
    log::info!("chat export finished: {done} messages, {media_copied} media files");
    Ok(Outcome::Done { messages: done, media_copied, media_missing, dest: dest.to_path_buf() })
}

/// Local wall-clock time and its ISO 8601 form (with the UTC offset).
fn local_time(ts: i64) -> (chrono::NaiveDateTime, String) {
    let t = DateTime::from_timestamp(ts, 0).unwrap_or_default().with_timezone(&Local);
    (t.naive_local(), t.to_rfc3339_opts(SecondsFormat::Secs, false))
}

/// Copies the cached media file of `m` into `<dest>/media/` and returns its
/// path relative to `dest`. `None` when the message has no downloaded file, it
/// is not inside the media cache, or the copy failed. View-once media is never
/// copied.
fn copy_media(m: &MessageRow, root: &Path, dest: &Path, n: usize, created: &mut Created) -> Option<String> {
    if !is_media(m.kind) || m.revoked || is_view_once(m) {
        return None;
    }
    let src = fs::canonicalize(m.local_path.as_deref().filter(|p| !p.is_empty())?).ok()?;
    if !is_cache_file(&src, root) || !fs::metadata(&src).is_ok_and(|md| md.is_file()) {
        return None;
    }
    let cached_name = src.file_name()?.to_string_lossy().into_owned();
    let name = media_file_name(n, m.media_name.as_deref(), &cached_name);
    let dir = dest.join(MEDIA_DIR);
    if !dir.exists() {
        if let Err(e) = fs::create_dir(&dir) {
            log::warn!("export: could not create the media folder: {:?}", e.kind());
            return None;
        }
        created.dirs.push(dir.clone());
    }
    let target = dir.join(&name);
    match fs::copy(&src, &target) {
        Ok(_) => {
            created.files.push(target);
            Some(format!("{MEDIA_DIR}/{name}"))
        }
        Err(e) => {
            log::warn!("export: a media file could not be copied: {:?}", e.kind());
            let _ = fs::remove_file(&target);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wa_store::{ChatKind, MessageKind, NewMessage};

    const CHAT: &str = "111@s.whatsapp.net";

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("waclient-export-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn msg(id: &str, ts: i64, text: &str) -> NewMessage {
        NewMessage {
            chat_jid: CHAT.into(),
            id: id.into(),
            sender_jid: CHAT.into(),
            from_me: false,
            ts,
            kind: MessageKind::Text,
            text: Some(text.into()),
            ..Default::default()
        }
    }

    fn job(dir: &Path, format: Format, include_media: bool) -> Job {
        Job {
            chat_jid: CHAT.into(),
            ctx: ChatCtx { name: "Ann".into(), is_group: false },
            format,
            from: None,
            to: None,
            range_text: "All messages".into(),
            include_media,
            dest: dir.join("out"),
            media_root: dir.join("media"),
        }
    }

    #[test]
    fn exports_text_and_copies_only_cached_media() {
        let dir = temp_dir("media");
        let media_root = dir.join("media");
        fs::create_dir_all(&media_root).expect("media dir");
        let cached = media_root.join("abc.jpg");
        fs::write(&cached, b"jpegdata").expect("file");
        let outside = dir.join("secret.jpg");
        fs::write(&outside, b"secret").expect("file");
        let store = Store::open_in_memory().expect("store");
        store.upsert_chat(CHAT, ChatKind::Dm, Some("Ann")).expect("chat");
        let mut photo = msg("p1", 100, "look");
        photo.kind = MessageKind::Image;
        photo.local_path = Some(cached.to_string_lossy().into_owned());
        let mut stray = msg("p2", 110, "outside the cache");
        stray.kind = MessageKind::Image;
        stray.local_path = Some(outside.to_string_lossy().into_owned());
        let mut pending = msg("p3", 120, "never downloaded");
        pending.kind = MessageKind::Image;
        for m in [msg("t1", 90, "hello <b>"), photo, stray, pending] {
            store.insert_message(&m, false).expect("insert");
        }
        let j = job(&dir, Format::Text, true);
        let mut last = (0, 0);
        let out = run(&store, &j, &AtomicBool::new(false), &mut |d, t| last = (d, t)).expect("export");
        assert_eq!(out, Outcome::Done { messages: 4, media_copied: 1, media_missing: 2, dest: j.dest.clone() });
        assert_eq!(last, (4, 4));
        let text = fs::read_to_string(j.dest.join("chat.txt")).expect("chat.txt");
        assert!(text.contains("Ann: hello <b>"));
        assert!(text.contains("<attached: 00001-abc.jpg>"));
        assert_eq!(text.matches("<Media omitted>").count(), 2);
        assert_eq!(fs::read(j.dest.join("media").join("00001-abc.jpg")).expect("copy"), b"jpegdata");
        assert_eq!(fs::read_dir(j.dest.join("media")).expect("dir").count(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn without_media_nothing_is_copied_and_json_is_valid() {
        let dir = temp_dir("json");
        let store = Store::open_in_memory().expect("store");
        store.upsert_chat(CHAT, ChatKind::Dm, Some("Ann")).expect("chat");
        store.insert_message(&msg("a", 10, "one"), false).expect("insert");
        store.insert_message(&msg("b", 20, "two"), false).expect("insert");
        let mut j = job(&dir, Format::Json, false);
        j.from = Some(15);
        let out = run(&store, &j, &AtomicBool::new(false), &mut |_, _| {}).expect("export");
        assert_eq!(out, Outcome::Done { messages: 1, media_copied: 0, media_missing: 0, dest: j.dest.clone() });
        let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(j.dest.join("chat.json")).expect("file")).expect("json");
        assert_eq!(v[0]["id"], "b");
        assert!(!j.dest.join("media").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancelling_removes_everything_it_wrote() {
        let dir = temp_dir("cancel");
        let store = Store::open_in_memory().expect("store");
        store.upsert_chat(CHAT, ChatKind::Dm, Some("Ann")).expect("chat");
        store.insert_message(&msg("a", 10, "one"), false).expect("insert");
        let j = job(&dir, Format::Html, true);
        let out = run(&store, &j, &AtomicBool::new(true), &mut |_, _| {}).expect("export");
        assert_eq!(out, Outcome::Cancelled);
        assert!(!j.dest.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    fn one_message_store() -> Store {
        let store = Store::open_in_memory().expect("store");
        store.upsert_chat(CHAT, ChatKind::Dm, Some("Ann")).expect("chat");
        store.insert_message(&msg("a", 10, "one"), false).expect("insert");
        store
    }

    #[test]
    fn a_non_empty_folder_is_never_written_into() {
        let dir = temp_dir("nonempty");
        let store = one_message_store();
        let j = job(&dir, Format::Text, false);
        fs::create_dir_all(&j.dest).expect("dest");
        fs::write(j.dest.join("chat.txt"), b"earlier export").expect("old");
        let out = run(&store, &j, &AtomicBool::new(false), &mut |_, _| {}).expect("export");
        let second = dir.join("out (2)");
        assert!(matches!(&out, Outcome::Done { dest, .. } if *dest == second));
        assert_eq!(fs::read(j.dest.join("chat.txt")).expect("old"), b"earlier export");
        assert!(second.join("chat.txt").is_file());
        assert!(!second.join("chat.txt.partial").exists());
        let out = run(&store, &j, &AtomicBool::new(false), &mut |_, _| {}).expect("export");
        assert!(matches!(&out, Outcome::Done { dest, .. } if *dest == dir.join("out (3)")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_existing_folder_is_reused_and_kept_on_cancel() {
        let dir = temp_dir("empty");
        let store = one_message_store();
        let j = job(&dir, Format::Text, false);
        fs::create_dir_all(&j.dest).expect("dest");
        let out = run(&store, &j, &AtomicBool::new(true), &mut |_, _| {}).expect("export");
        assert_eq!(out, Outcome::Cancelled);
        assert!(j.dest.is_dir());
        assert_eq!(fs::read_dir(&j.dest).expect("dir").count(), 0);
        let out = run(&store, &j, &AtomicBool::new(false), &mut |_, _| {}).expect("export");
        assert!(matches!(&out, Outcome::Done { dest, .. } if *dest == j.dest));
        assert!(j.dest.join("chat.txt").is_file());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancel_after_partial_write_removes_only_what_the_run_created() {
        let dir = temp_dir("partial");
        let store = one_message_store();
        let j = job(&dir, Format::Text, false);
        fs::create_dir_all(&j.dest).expect("dest");
        fs::write(j.dest.join("keep.txt"), b"mine").expect("old");
        let cancel = AtomicBool::new(false);
        // The first progress call comes after the file was opened and the head
        // written, so cancelling there leaves a partial file to clean up.
        let out = run(&store, &j, &cancel, &mut |done, _| {
            if done == 0 {
                cancel.store(true, Ordering::Relaxed);
            }
        })
        .expect("export");
        assert_eq!(out, Outcome::Cancelled);
        assert!(!dir.join("out (2)").exists());
        assert_eq!(fs::read(j.dest.join("keep.txt")).expect("old"), b"mine");
        let _ = fs::remove_dir_all(&dir);
    }
}
