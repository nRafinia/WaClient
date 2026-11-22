//! Settings > Storage: what this profile uses on disk and clearing the media
//! cache (per chat, by type, by age). Messages are never deleted: a cleared
//! file only loses its local path, so the UI offers the download again. The
//! rules about what may be deleted live in `wa_store::plan_deletion`.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use slint::{ComponentHandle, ModelRc, VecModel};
use wa_store::{MediaClass, MediaFile, MediaFilter, STATUS_JID, Store, is_cache_file, plan_deletion};

use super::{App, with_app};
use crate::{MainWindow, StorageChat, StorageSegment, StorageType, StorageUsage, location};

const TOP_CHATS: usize = 50;
/// "Older than" choices, in days; 0 = any age. Matches `StorageUsage.ages`.
const AGE_DAYS: [i64; 5] = [0, 7, 30, 90, 365];

/// Everything the background scan learned about one profile.
struct Scan {
    media_root: PathBuf,
    /// Messages whose cached file exists inside the media cache.
    files: Vec<MediaFile>,
    sizes: HashMap<PathBuf, u64>,
    names: HashMap<String, String>,
    db: u64,
    avatars: u64,
    /// Downloaded media, avatars excluded.
    media: u64,
}

#[derive(Clone)]
struct Dialog {
    chat: Option<String>,
    types: [bool; 6],
    age: usize,
}

struct Cleared {
    freed: u64,
    failed: usize,
    /// Starred or unsent files the plan left alone.
    kept: usize,
}

thread_local! {
    static SCAN: RefCell<Option<Scan>> = const { RefCell::new(None) };
    static DIALOG: RefCell<Option<Dialog>> = const { RefCell::new(None) };
    /// Newest scan request; older answers are dropped.
    static GENERATION: Cell<u64> = const { Cell::new(0) };
}

pub(super) fn wire(win: &MainWindow) {
    let g = win.global::<StorageUsage>();
    g.on_refresh(|| with_app(App::storage_scan));
    g.on_ask_chat(|jid| with_app(|a| a.storage_ask_chat(jid.as_str())));
    g.on_open_dialog(|| with_app(App::storage_open_dialog));
    g.on_toggle_type(|i| with_app(|a| a.storage_toggle_type(i)));
    g.on_set_age(|i| with_app(|a| a.storage_set_age(i)));
    g.on_confirm(|| with_app(App::cache_dialog_confirm));
    g.on_cancel(|| with_app(App::cache_dialog_cancel));
}

fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// The filter a dialog stands for; `None` when no type is ticked (an empty
/// type list would mean "every type" to the store).
fn build_filter(d: &Dialog, now: i64) -> Option<MediaFilter> {
    let classes: Vec<MediaClass> =
        MediaClass::ALL.iter().zip(d.types).filter(|(_, on)| *on).map(|(c, _)| *c).collect();
    if classes.is_empty() {
        return None;
    }
    let days = AGE_DAYS.get(d.age).copied().unwrap_or(0);
    Some(MediaFilter {
        chat: d.chat.clone(),
        classes,
        older_than: (days > 0).then(|| now - days * 86_400),
    })
}

fn file_size(path: &Path) -> Option<u64> {
    fs::metadata(path).ok().filter(|m| m.is_file()).map(|m| m.len())
}

fn db_bytes(dir: &Path) -> u64 {
    ["messages.db", "messages.db-wal", "messages.db-shm", "session.db", "session.db-wal", "session.db-shm"]
        .iter()
        .filter_map(|n| fs::metadata(dir.join(n)).ok())
        .map(|m| m.len())
        .sum()
}

fn chat_name(jid: &str, names: &HashMap<String, String>) -> String {
    if jid == STATUS_JID {
        return "Status updates".to_string();
    }
    match names.get(jid).filter(|n| !n.trim().is_empty()) {
        Some(n) => n.clone(),
        None => jid.split('@').next().unwrap_or(jid).to_string(),
    }
}

fn scan_profile(store: &Store, dir: &Path) -> Result<Scan, String> {
    let media_root = dir.join("media");
    let mut files = store.media_files(None).map_err(|e| format!("read the media list: {e}"))?;
    let mut sizes = HashMap::new();
    files.retain(|f| {
        let path = Path::new(&f.local_path);
        if !is_cache_file(path, &media_root) {
            return false;
        }
        match file_size(path) {
            Some(n) => {
                sizes.insert(path.to_path_buf(), n);
                true
            }
            None => false,
        }
    });
    let names = store
        .list_chats(true)
        .map_err(|e| format!("read the chats: {e}"))?
        .into_iter()
        .map(|c| (c.jid, c.display_name))
        .collect();
    let avatars = location::data_size(&media_root.join("avatars"));
    let media = location::data_size(&media_root).saturating_sub(avatars);
    Ok(Scan { media_root, files, sizes, names, db: db_bytes(dir), avatars, media })
}

/// Deletes what `plan_deletion` allows. Each file is canonicalized again right
/// before removal, so a link or a swapped path can never lead outside the cache.
fn run_clear(store: &Store, media_root: &Path, filter: &MediaFilter) -> Result<Cleared, String> {
    let rows = store.media_files(filter.chat.as_deref()).map_err(|e| format!("read the media list: {e}"))?;
    let plan = plan_deletion(&rows, filter, media_root);
    let (kept, delete) = (plan.kept_starred + plan.kept_pending, plan.delete);
    let root = fs::canonicalize(media_root).ok();
    let mut freed = 0;
    let mut failed = 0;
    let mut done = Vec::new();
    for d in delete {
        match remove_cached(&d.path, root.as_deref()) {
            Ok(n) => {
                freed += n;
                done.push((d.chat_jid, d.id));
            }
            Err(e) => {
                failed += 1;
                log::debug!("media file not removed: {e}");
            }
        }
    }
    store.clear_local_paths(&done).map_err(|e| format!("update the messages: {e}"))?;
    if failed > 0 {
        log::warn!("{failed} cached media files could not be removed");
    }
    Ok(Cleared { freed, failed, kept })
}

/// Bytes freed; a file that is already gone counts as removed (0 bytes).
fn remove_cached(path: &Path, root: Option<&Path>) -> Result<u64, String> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(format!("stat: {e}")),
    };
    let root = root.ok_or("media folder is not readable")?;
    let real = fs::canonicalize(path).map_err(|e| format!("resolve: {e}"))?;
    if !is_cache_file(&real, root) {
        return Err("outside the media cache".to_string());
    }
    let len = fs::metadata(&real).map_err(|e| format!("stat: {e}"))?;
    if !len.is_file() || meta.is_dir() {
        return Err("not a file".to_string());
    }
    fs::remove_file(&real).map_err(|e| format!("remove: {e}"))?;
    Ok(len.len())
}

fn cleared_text(c: &Cleared) -> String {
    let mut text = format!("Freed {}", location::fmt_bytes(c.freed));
    if c.kept > 0 {
        text.push_str(&format!("; {} starred or unsent files kept", c.kept));
    }
    if c.failed > 0 {
        text.push_str(&format!("; {} files could not be removed", c.failed));
    }
    text
}

fn model<T: Clone + 'static>(rows: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(rows))
}

impl App {
    pub(super) fn storage_scan(&self) {
        let Some(win) = self.win() else { return };
        let g = win.global::<StorageUsage>();
        g.set_busy(true);
        SCAN.with(|s| *s.borrow_mut() = None);
        let generation = GENERATION.with(|c| {
            c.set(c.get() + 1);
            c.get()
        });
        let p = self.p();
        let id = p.id.clone();
        let store = p.store.clone();
        let dir = self.profile_dir(&p);
        let spawned = std::thread::Builder::new().name("storage-scan".into()).spawn(move || {
            let result = scan_profile(&store, &dir);
            let queued = slint::invoke_from_event_loop(move || {
                with_app(|a| a.storage_scanned(&id, generation, result));
            });
            if let Err(e) = queued {
                log::debug!("storage scan dropped: {e}");
            }
        });
        if let Err(e) = spawned {
            log::warn!("could not start the storage scan: {e}");
            g.set_busy(false);
        }
    }

    fn storage_scanned(&self, id: &str, generation: u64, result: Result<Scan, String>) {
        if GENERATION.with(Cell::get) != generation || self.p().id != id {
            return;
        }
        let Some(win) = self.win() else { return };
        let g = win.global::<StorageUsage>();
        let scan = match result {
            Ok(s) => s,
            Err(e) => {
                log::warn!("storage scan failed: {e}");
                g.set_busy(false);
                g.set_status(format!("Could not read the storage usage: {e}").into());
                return;
            }
        };
        let mut by_class = [0u64; 6];
        let mut by_chat: HashMap<&str, u64> = HashMap::new();
        for f in &scan.files {
            let n = scan.sizes.get(Path::new(&f.local_path)).copied().unwrap_or(0);
            let class = MediaClass::ALL.iter().position(|c| *c == MediaClass::of(f.kind)).unwrap_or(5);
            by_class[class] += n;
            *by_chat.entry(f.chat_jid.as_str()).or_default() += n;
        }
        // Files no message points to (staged sends, leftovers) belong to "Other".
        by_class[5] += scan.media.saturating_sub(by_class.iter().sum());
        let media_total: u64 = by_class.iter().sum();
        let mut segments = Vec::new();
        let mut start = 0u64;
        for (i, c) in MediaClass::ALL.iter().enumerate().filter(|(i, _)| by_class[*i] > 0) {
            segments.push(StorageSegment {
                idx: i32::try_from(i).unwrap_or(5),
                label: c.label().into(),
                size: location::fmt_bytes(by_class[i]).into(),
                start: start as f32 / media_total as f32,
                frac: by_class[i] as f32 / media_total as f32,
            });
            start += by_class[i];
        }
        let mut chats: Vec<(&str, u64)> = by_chat.into_iter().filter(|(_, n)| *n > 0).collect();
        chats.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        let rows: Vec<StorageChat> = chats
            .into_iter()
            .take(TOP_CHATS)
            .map(|(jid, n)| StorageChat {
                jid: jid.into(),
                name: chat_name(jid, &scan.names).into(),
                size: location::fmt_bytes(n).into(),
            })
            .collect();
        g.set_total_text(location::fmt_bytes(scan.db + media_total + scan.avatars).into());
        g.set_db_text(location::fmt_bytes(scan.db).into());
        g.set_media_text(location::fmt_bytes(media_total).into());
        g.set_avatars_text(location::fmt_bytes(scan.avatars).into());
        g.set_segments(model(segments));
        g.set_chats(model(rows));
        g.set_busy(false);
        SCAN.with(|s| *s.borrow_mut() = Some(scan));
        if DIALOG.with(|d| d.borrow().is_some()) {
            self.storage_sync();
        }
    }

    fn storage_ask_chat(&self, jid: &str) {
        let Some(win) = self.win() else { return };
        let name = SCAN.with(|s| s.borrow().as_ref().map(|s| chat_name(jid, &s.names))).unwrap_or_default();
        DIALOG.with(|d| *d.borrow_mut() = Some(Dialog { chat: Some(jid.to_string()), types: [true; 6], age: 0 }));
        let g = win.global::<StorageUsage>();
        g.set_dialog_title(format!("Clear media of {name}?").into());
        g.set_dialog(1);
        self.storage_sync();
    }

    pub(super) fn storage_open_dialog(&self) {
        let Some(win) = self.win() else { return };
        DIALOG.with(|d| *d.borrow_mut() = Some(Dialog { chat: None, types: [true; 6], age: 0 }));
        let g = win.global::<StorageUsage>();
        g.set_dialog_title("Clear cache".into());
        g.set_dialog(2);
        self.storage_sync();
    }

    fn storage_toggle_type(&self, i: i32) {
        let Ok(i) = usize::try_from(i) else { return };
        DIALOG.with(|d| {
            if let Some(t) = d.borrow_mut().as_mut().and_then(|d| d.types.get_mut(i)) {
                *t = !*t;
            }
        });
        self.storage_sync();
    }

    fn storage_set_age(&self, i: i32) {
        let Ok(i) = usize::try_from(i) else { return };
        if i < AGE_DAYS.len() {
            DIALOG.with(|d| {
                if let Some(d) = d.borrow_mut().as_mut() {
                    d.age = i;
                }
            });
            self.storage_sync();
        }
    }

    /// Shows the dialog's choices and how much they would free.
    fn storage_sync(&self) {
        let Some(win) = self.win() else { return };
        let Some(d) = DIALOG.with(|d| d.borrow().clone()) else { return };
        let g = win.global::<StorageUsage>();
        let types: Vec<StorageType> = MediaClass::ALL
            .iter()
            .zip(d.types)
            .map(|(c, on)| StorageType { label: c.label().into(), on })
            .collect();
        g.set_types(model(types));
        g.set_age(i32::try_from(d.age).unwrap_or(0));
        let (text, can_clear) = match (build_filter(&d, now_secs()), SCAN.with(|s| s.borrow().is_some())) {
            (None, _) => ("Select at least one type.".to_string(), false),
            (_, false) => ("Calculating…".to_string(), false),
            (Some(filter), true) => SCAN.with(|s| {
                let s = s.borrow();
                let Some(s) = s.as_ref() else { return ("Calculating…".to_string(), false) };
                let plan = plan_deletion(&s.files, &filter, &s.media_root);
                let bytes: u64 = plan.delete.iter().filter_map(|x| s.sizes.get(&x.path)).sum();
                let kept = plan.kept_starred + plan.kept_pending;
                let mut text = if plan.delete.is_empty() {
                    "Nothing to clear.".to_string()
                } else {
                    format!("{} will be freed ({} files).", location::fmt_bytes(bytes), plan.delete.len())
                };
                if kept > 0 {
                    text.push_str(&format!(" {kept} starred or unsent files are kept."));
                }
                (text, !plan.delete.is_empty())
            }),
        };
        g.set_dialog_text(text.into());
        g.set_dialog_can_clear(can_clear);
    }

    fn cache_dialog_cancel(&self) {
        DIALOG.with(|d| *d.borrow_mut() = None);
        if let Some(win) = self.win() {
            win.global::<StorageUsage>().set_dialog(0);
        }
    }

    fn cache_dialog_confirm(&self) {
        let Some(d) = DIALOG.with(|d| d.borrow().clone()) else { return };
        let Some(filter) = build_filter(&d, now_secs()) else { return };
        self.start_clear(filter, true);
    }

    /// The "Clear all media cache" button: every type, any age, through the
    /// same planner (starred and unsent files stay).
    pub(super) fn clear_cache(&self) {
        self.start_clear(MediaFilter::default(), false);
    }

    fn start_clear(&self, filter: MediaFilter, from_dialog: bool) {
        if self.cache_busy.replace(true) {
            return;
        }
        if from_dialog {
            self.cache_dialog_cancel();
        }
        let Some(win) = self.win() else {
            self.cache_busy.set(false);
            return;
        };
        let g = win.global::<StorageUsage>();
        g.set_busy(true);
        g.set_status("Clearing…".into());
        win.set_cache_status("Clearing…".into());
        if let Some(player) = &self.player {
            // A playing voice note keeps its file open.
            player.stop();
        }
        let p = self.p();
        let id = p.id.clone();
        let store = p.store.clone();
        let media_root = self.profile_dir(&p).join("media");
        let spawned = std::thread::Builder::new().name("storage-clear".into()).spawn(move || {
            let result = run_clear(&store, &media_root, &filter);
            let queued = slint::invoke_from_event_loop(move || {
                with_app(|a| a.storage_cleared(&id, result));
            });
            if let Err(e) = queued {
                log::error!("could not deliver the clearing result to the UI: {e}");
            }
        });
        if let Err(e) = spawned {
            self.cache_busy.set(false);
            g.set_busy(false);
            g.set_status("".into());
            win.set_cache_status("".into());
            self.show_banner(1, &format!("Could not clear the media: {e}"));
        }
    }

    fn storage_cleared(&self, id: &str, result: Result<Cleared, String>) {
        self.cache_busy.set(false);
        let Some(p) = self.find(id) else { return };
        let text = match &result {
            Ok(c) => cleared_text(c),
            Err(e) => {
                log::error!("clearing the media of {id} failed: {e}");
                self.show_banner(1, &format!("Could not clear the media: {e}"));
                "Clearing failed".to_string()
            }
        };
        {
            let mut st = p.st.borrow_mut();
            st.media.clear();
            st.decoding.clear();
            st.seen_paths.clear();
        }
        if !self.is_active(&p) {
            return;
        }
        // An open status viewer holds decoded images of the deleted files.
        self.status_close();
        if p.st.borrow().selected.is_some() {
            self.rebuild_rows();
        }
        self.storage_scan();
        if let Some(win) = self.win() {
            win.set_cache_status(text.as_str().into());
            win.global::<StorageUsage>().set_status(text.into());
        }
    }
}

#[cfg(debug_assertions)]
impl App {
    /// Demo data for the Storage page: sparse files under the demo profile's
    /// media folder, attached to new messages of the demo chats.
    pub(super) fn demo_storage(&self) {
        use wa_store::{MessageKind, NewMessage};
        let p = self.p();
        let media = self.profile_dir(&p).join("media");
        let Ok(chats) = p.store.list_chats(true) else { return };
        let kinds = [
            (MessageKind::Image, "jpg", 18_400_000u64),
            (MessageKind::Video, "mp4", 96_000_000),
            (MessageKind::Voice, "ogg", 1_900_000),
            (MessageKind::Document, "pdf", 7_300_000),
            (MessageKind::Sticker, "webp", 400_000),
        ];
        let now = now_secs();
        for (ci, chat) in chats.iter().take(4).enumerate() {
            let dir = media.join(format!("demo{ci}"));
            if fs::create_dir_all(&dir).is_err() {
                return;
            }
            for (ki, (kind, ext, size)) in kinds.iter().enumerate() {
                let path = dir.join(format!("f{ki}.{ext}"));
                let made = fs::File::create(&path).and_then(|f| f.set_len(size / (ci as u64 + 1)));
                if made.is_err() {
                    return;
                }
                let msg = NewMessage {
                    chat_jid: chat.jid.clone(),
                    id: format!("demo-storage-{ci}-{ki}"),
                    sender_jid: chat.jid.clone(),
                    ts: now - (ki as i64 + 1) * 20 * 86_400,
                    kind: *kind,
                    local_path: Some(path.to_string_lossy().into_owned()),
                    ..Default::default()
                };
                if let Err(e) = p.store.insert_message(&msg, false) {
                    log::warn!("demo storage message: {e}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_needs_a_type_and_turns_the_age_into_a_cutoff() {
        let mut d = Dialog { chat: None, types: [false; 6], age: 0 };
        assert_eq!(build_filter(&d, 1_000_000), None);
        d.types[1] = true;
        d.age = 1;
        let f = build_filter(&d, 1_000_000).expect("filter");
        assert_eq!(f.classes, vec![MediaClass::Videos]);
        assert_eq!(f.older_than, Some(1_000_000 - 7 * 86_400));
        d.age = 0;
        assert_eq!(build_filter(&d, 1_000_000).expect("filter").older_than, None);
    }

    #[test]
    fn unnamed_chats_show_the_user_part_and_status_has_a_label() {
        let names = HashMap::from([("a@g.us".to_string(), "Team".to_string())]);
        assert_eq!(chat_name("a@g.us", &names), "Team");
        assert_eq!(chat_name("123@s.whatsapp.net", &names), "123");
        assert_eq!(chat_name(STATUS_JID, &names), "Status updates");
    }

    #[test]
    fn removal_stays_inside_the_cache() {
        let base = std::env::temp_dir().join(format!("waclient-storage-test-{}", std::process::id()));
        let media = base.join("media");
        fs::create_dir_all(media.join("chat")).expect("dirs");
        fs::create_dir_all(media.join("avatars")).expect("dirs");
        let inside = media.join("chat").join("a.bin");
        let avatar = media.join("avatars").join("a.jpg");
        let outside = base.join("keep.txt");
        for (p, n) in [(&inside, 5), (&avatar, 3), (&outside, 7)] {
            fs::write(p, vec![0u8; n]).expect("write");
        }
        let root = fs::canonicalize(&media).expect("root");
        assert_eq!(remove_cached(&inside, Some(&root)), Ok(5));
        assert!(!inside.exists());
        assert_eq!(remove_cached(&inside, Some(&root)), Ok(0), "already gone");
        assert!(remove_cached(&avatar, Some(&root)).is_err());
        assert!(remove_cached(&outside, Some(&root)).is_err());
        assert!(remove_cached(&media.join("chat").join("..").join("..").join("keep.txt"), Some(&root)).is_err());
        assert!(avatar.exists() && outside.exists());
        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn clear_all_keeps_starred_and_unsent_media() {
        use wa_store::{MessageKind, NewMessage};
        let base = std::env::temp_dir().join(format!("waclient-clear-all-test-{}", std::process::id()));
        let media = base.join("media");
        fs::create_dir_all(media.join("chat")).expect("dirs");
        let store = Store::open_in_memory().expect("store");
        let chat = "c@s.whatsapp.net";
        let mut paths = Vec::new();
        for id in ["plain", "starred"] {
            let path = media.join("chat").join(format!("{id}.bin"));
            fs::write(&path, b"12345").expect("write");
            let msg = NewMessage {
                chat_jid: chat.into(),
                id: id.into(),
                sender_jid: chat.into(),
                ts: 100,
                kind: MessageKind::Image,
                local_path: Some(path.to_string_lossy().into_owned()),
                ..Default::default()
            };
            store.insert_message(&msg, false).expect("insert");
            paths.push(path);
        }
        assert!(store.set_starred(chat, "starred", true).expect("star"));
        let done = run_clear(&store, &media, &MediaFilter::default()).expect("clear");
        assert_eq!((done.freed, done.kept, done.failed), (5, 1, 0));
        assert!(!paths[0].exists());
        assert!(paths[1].exists(), "starred media must survive");
        fs::remove_dir_all(&base).expect("cleanup");
    }
}
