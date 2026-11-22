//! Stickers: the Stickers tab of the emoji picker (favourites, then the ones
//! seen in chats), sending one to the open chat, and the favourite toggle of
//! the message menu. The Slint side is the `StickerState` global
//! (ui/emoji_picker.slint). A sticker's identity is the SHA-256 of its file,
//! so the same sticker received in several chats counts once.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::Read as _;
use std::path::Path;
use std::rc::Rc;
use std::sync::Mutex;

use sha2::{Digest, Sha256};
use slint::{ComponentHandle, Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel};
use wa_engine::Command;
use wa_store::{MessageRow, Store};

use super::{App, with_app};
use crate::{MainWindow, StickerItem, StickerState, models};

/// Recent stickers shown.
const RECENT_LIMIT: u32 = 60;
/// Longest edge of a grid thumbnail.
const THUMB_DIM: u32 = 128;
/// Stickers are small; anything larger is not hashed or listed.
const MAX_STICKER_BYTES: u64 = 4 * 1024 * 1024;
/// Hashes remembered before the cache starts over.
const HASH_CACHE_CAP: usize = 4096;

/// File hashes by path: cached files never change once written, and the
/// picker would otherwise re-read every sticker each time it opens.
static HASHES: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn hash_file(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut data = Vec::new();
    file.take(MAX_STICKER_BYTES + 1).read_to_end(&mut data).ok()?;
    if data.is_empty() || u64::try_from(data.len()).map_or(true, |n| n > MAX_STICKER_BYTES) {
        return None;
    }
    Some(hex(&Sha256::digest(&data)))
}

/// SHA-256 (hex) of the sticker file at `path`; `None` when it is missing,
/// empty or too large.
pub(crate) fn file_sha256(path: &Path) -> Option<String> {
    let key = path.to_string_lossy().into_owned();
    {
        let guard = HASHES.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(sha) = guard.as_ref().and_then(|m| m.get(&key)) {
            return Some(sha.clone());
        }
    }
    let sha = hash_file(path)?;
    let mut guard = HASHES.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let map = guard.get_or_insert_with(HashMap::new);
    if map.len() >= HASH_CACHE_CAP {
        map.clear();
    }
    map.insert(key, sha.clone());
    Some(sha)
}

fn row_sha(row: &MessageRow) -> Option<String> {
    file_sha256(Path::new(row.local_path.as_deref()?))
}

/// The sticker message a grid cell stands for.
#[derive(Clone, Debug, PartialEq)]
struct StickerRef {
    chat_jid: String,
    id: String,
    sha: String,
}

type Thumb = (StickerRef, SharedPixelBuffer<Rgba8Pixel>);

/// Both lists, decoded; built off the UI thread.
struct Loaded {
    favs: Vec<Thumb>,
    recent: Vec<Thumb>,
}

fn thumb(row: &MessageRow, sha: String) -> Option<Thumb> {
    let path = Path::new(row.local_path.as_deref()?);
    match models::decode_file_scaled(path, THUMB_DIM) {
        Ok(buf) => Some((StickerRef { chat_jid: row.chat_jid.clone(), id: row.id.clone(), sha }, buf)),
        Err(e) => {
            log::debug!("sticker does not decode: {e}");
            None
        }
    }
}

fn load(store: &Store) -> Result<Loaded, String> {
    let favs = store.sticker_favs().map_err(|e| format!("could not read the favourite stickers: {e}"))?;
    let favs = favs.into_iter().filter_map(|(sha, row)| thumb(&row, sha)).collect();
    let rows = store
        .recent_stickers(RECENT_LIMIT, row_sha)
        .map_err(|e| format!("could not read the recent stickers: {e}"))?;
    let recent = rows.iter().filter_map(|row| thumb(row, row_sha(row)?)).collect();
    Ok(Loaded { favs, recent })
}

pub(super) struct StickerUi {
    favs: Rc<VecModel<StickerItem>>,
    recent: Rc<VecModel<StickerItem>>,
    fav_refs: RefCell<Vec<StickerRef>>,
    recent_refs: RefCell<Vec<StickerRef>>,
    /// Number of the newest load; older answers are dropped.
    generation: Cell<u64>,
}

impl StickerUi {
    pub(super) fn new() -> Self {
        StickerUi {
            favs: Rc::new(VecModel::default()),
            recent: Rc::new(VecModel::default()),
            fav_refs: RefCell::new(Vec::new()),
            recent_refs: RefCell::new(Vec::new()),
            generation: Cell::new(0),
        }
    }

    pub(super) fn attach(&self, win: &MainWindow) {
        let g = win.global::<StickerState>();
        g.set_favs(ModelRc::from(self.favs.clone()));
        g.set_recent(ModelRc::from(self.recent.clone()));
    }

    fn show(&self, items: &[Thumb], model: &VecModel<StickerItem>, refs: &RefCell<Vec<StickerRef>>) {
        model.set_vec(
            items
                .iter()
                .map(|(r, buf)| StickerItem {
                    img: Image::from_rgba8(buf.clone()),
                    key: SharedString::from(r.id.as_str()),
                })
                .collect::<Vec<_>>(),
        );
        *refs.borrow_mut() = items.iter().map(|(r, _)| r.clone()).collect();
    }
}

pub(super) fn wire(win: &MainWindow) {
    let g = win.global::<StickerState>();
    g.on_refresh(|| with_app(App::stickers_refresh));
    g.on_send(|favs, i| with_app(|a| a.stickers_send(favs, i)));
    g.on_toggle_fav(|favs, i| with_app(|a| a.stickers_toggle_fav(favs, i)));
    g.on_is_fav_message(|id| {
        let mut fav = false;
        with_app(|a| fav = a.sticker_is_fav(id.as_str()));
        fav
    });
    g.on_toggle_fav_message(|id| with_app(|a| a.sticker_toggle_fav(id.as_str())));
}

impl App {
    /// Reloads both lists in the background.
    pub(super) fn stickers_refresh(&self) {
        let Some(win) = self.win() else { return };
        let p = self.p();
        let (store, profile) = (p.store.clone(), p.id.clone());
        let generation = self.stickers.generation.get() + 1;
        self.stickers.generation.set(generation);
        win.global::<StickerState>().set_loading(true);
        let spawned = std::thread::Builder::new().name("sticker-load".into()).spawn(move || {
            let loaded = load(&store);
            let queued = slint::invoke_from_event_loop(move || {
                with_app(|a| a.stickers_loaded(&profile, generation, loaded));
            });
            if let Err(e) = queued {
                log::error!("could not deliver the stickers to the UI: {e}");
            }
        });
        if let Err(e) = spawned {
            log::error!("could not start the sticker loader: {e}");
            win.global::<StickerState>().set_loading(false);
        }
    }

    fn stickers_loaded(&self, profile: &str, generation: u64, loaded: Result<Loaded, String>) {
        if self.p().id != profile || self.stickers.generation.get() != generation {
            return;
        }
        let Some(win) = self.win() else { return };
        win.global::<StickerState>().set_loading(false);
        let loaded = loaded.unwrap_or_else(|e| {
            log::warn!("{e}");
            Loaded { favs: Vec::new(), recent: Vec::new() }
        });
        let ui = &self.stickers;
        ui.show(&loaded.favs, &ui.favs, &ui.fav_refs);
        ui.show(&loaded.recent, &ui.recent, &ui.recent_refs);
    }

    fn sticker_ref(&self, from_favs: bool, index: i32) -> Option<StickerRef> {
        let refs = if from_favs { &self.stickers.fav_refs } else { &self.stickers.recent_refs };
        refs.borrow().get(usize::try_from(index).ok()?).cloned()
    }

    /// Sends a grid sticker to the open chat (as a reply while the reply bar is up).
    pub(super) fn stickers_send(&self, from_favs: bool, index: i32) {
        let Some(sticker) = self.sticker_ref(from_favs, index) else { return };
        let Some(chat_jid) = self.p().st.borrow().selected.clone() else { return };
        if self.refuse_blocked(&chat_jid) || self.refuse_gated() {
            return;
        }
        let quoted = {
            let p = self.p();
            let mut st = p.st.borrow_mut();
            st.force_bottom = true;
            st.reply.take().map(|r| r.0)
        };
        self.send(Command::SendSticker {
            chat_jid,
            source_chat: sticker.chat_jid,
            source_id: sticker.id,
            quoted_id: quoted,
        });
        self.sync_reply();
    }

    /// Right-click in the grid: adds a recent sticker to, or removes a
    /// favourite from, the favourites.
    pub(super) fn stickers_toggle_fav(&self, from_favs: bool, index: i32) {
        let Some(sticker) = self.sticker_ref(from_favs, index) else { return };
        if self.set_sticker_fav(&sticker, !from_favs) {
            self.stickers_refresh();
        }
    }

    /// The sticker of message `id` in the open chat, if its file is on disk.
    fn open_chat_sticker(&self, id: &str) -> Option<StickerRef> {
        let p = self.p();
        let chat_jid = p.st.borrow().selected.clone()?;
        let row = match p.store.message(&chat_jid, id) {
            Ok(Some(row)) => row,
            Ok(None) => return None,
            Err(e) => {
                log::warn!("sticker lookup failed: {e}");
                return None;
            }
        };
        let sha = row_sha(&row)?;
        Some(StickerRef { chat_jid, id: row.id, sha })
    }

    /// Whether the sticker of message `id` (open chat) is a favourite.
    pub(super) fn sticker_is_fav(&self, id: &str) -> bool {
        let Some(sticker) = self.open_chat_sticker(id) else { return false };
        self.p().store.is_sticker_fav(&sticker.sha).unwrap_or_else(|e| {
            log::warn!("favourite lookup failed: {e}");
            false
        })
    }

    /// Message menu: "Add to favourites" / "Remove from favourites".
    pub(super) fn sticker_toggle_fav(&self, id: &str) {
        let Some(sticker) = self.open_chat_sticker(id) else {
            self.show_toast("The sticker is not available on this computer");
            return;
        };
        let add = !self.sticker_is_fav(id);
        self.set_sticker_fav(&sticker, add);
    }

    /// Adds or removes a favourite, telling the user; `false` on failure.
    fn set_sticker_fav(&self, sticker: &StickerRef, add: bool) -> bool {
        let store = &self.p().store;
        let done = if add {
            store.add_sticker_fav(&sticker.sha, &sticker.chat_jid, &sticker.id, chrono::Utc::now().timestamp())
        } else {
            store.remove_sticker_fav(&sticker.sha).map(|_| ())
        };
        match done {
            Ok(()) => {
                self.show_toast(if add { "Added to favourites" } else { "Removed from favourites" });
                true
            }
            Err(e) => {
                log::error!("changing the favourite stickers failed: {e}");
                self.show_toast("Could not change the favourite stickers");
                false
            }
        }
    }
}

#[cfg(debug_assertions)]
impl App {
    /// Demo: Alice's chat with the picker open on the Stickers tab.
    pub(super) fn demo_stickers(&self) {
        let Some(win) = self.win() else { return };
        self.select_chat("111@s.whatsapp.net");
        win.global::<StickerState>().set_tab(1);
        self.stickers_refresh();
        slint::Timer::single_shot(std::time::Duration::from_millis(1500), || {
            with_app(|a| {
                if let Some(w) = a.win() {
                    w.invoke_show_emoji_picker();
                }
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wa_store::{MessageKind, NewMessage};

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("waclient-sticker-test-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    fn png(path: &Path, shade: u8) {
        image::RgbaImage::from_pixel(8, 8, image::Rgba([shade, 20, 30, 255])).save(path).expect("png");
    }

    fn sticker(chat: &str, id: &str, ts: i64, path: &Path) -> NewMessage {
        NewMessage {
            chat_jid: chat.into(),
            id: id.into(),
            sender_jid: chat.into(),
            ts,
            kind: MessageKind::Sticker,
            local_path: Some(path.to_string_lossy().into_owned()),
            ..Default::default()
        }
    }

    #[test]
    fn hex_is_lowercase_and_padded() {
        assert_eq!(hex(&[0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
    }

    #[test]
    fn identical_files_share_a_hash_and_missing_files_have_none() {
        let dir = temp_dir("hash");
        let (a, b, c) = (dir.join("a.png"), dir.join("b.png"), dir.join("c.png"));
        png(&a, 1);
        std::fs::copy(&a, &b).expect("copy");
        png(&c, 2);
        assert_eq!(file_sha256(&a), file_sha256(&b));
        assert_ne!(file_sha256(&a), file_sha256(&c));
        assert_eq!(file_sha256(&a).map(|s| s.len()), Some(64));
        assert_eq!(file_sha256(&dir.join("missing.png")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_dedupes_recents_and_lists_favourites() {
        let dir = temp_dir("load");
        let (a, b, c) = (dir.join("a.png"), dir.join("b.png"), dir.join("c.png"));
        png(&a, 1);
        std::fs::copy(&a, &b).expect("copy");
        png(&c, 2);
        let store = Store::open_in_memory().expect("store");
        for m in [
            sticker("111@s.whatsapp.net", "s1", 10, &a),
            sticker("222@s.whatsapp.net", "s2", 20, &b),
            sticker("222@s.whatsapp.net", "s3", 30, &c),
            sticker("222@s.whatsapp.net", "gone", 40, &dir.join("missing.png")),
        ] {
            assert!(store.insert_message(&m, false).expect("insert"));
        }
        let sha_a = file_sha256(&a).expect("hash");
        store.add_sticker_fav(&sha_a, "111@s.whatsapp.net", "s1", 5).expect("fav");

        let loaded = load(&store).expect("load");
        // s3 is the newest; s2 and s1 are the same file, the newer one wins.
        assert_eq!(loaded.recent.iter().map(|(r, _)| r.id.as_str()).collect::<Vec<_>>(), ["s3", "s2"]);
        assert_eq!(loaded.favs.len(), 1);
        assert_eq!(loaded.favs[0].0.sha, sha_a);
        assert_eq!(loaded.favs[0].0.id, "s1");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
