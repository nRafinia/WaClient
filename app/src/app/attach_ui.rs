//! Files that arrive by paste (Ctrl+V) or drag and drop: staged in a preview
//! overlay, then sent one `SendMedia` command per file.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use slint::{ComponentHandle, Image, ModelRc, VecModel};
use wa_engine::{Command, OutgoingMediaKind};

use super::{App, with_app};
use crate::drop_paste::{self, Attachment, MAX_FILE_BYTES, MAX_FILES, Rejection};
use crate::{AttachItem, AttachOptions, MainWindow, models, profiles};

/// Longest edge of a preview thumbnail (tiles are 120 px; this keeps HiDPI sharp).
const THUMB_DIM: u32 = 240;

struct Pending {
    att: Attachment,
    /// A pasted image saved by us: removed when it is not sent.
    temp: bool,
    thumb: Option<Image>,
}

pub(super) struct AttachState {
    items: RefCell<Vec<Pending>>,
    model: Rc<VecModel<AttachItem>>,
    /// Chat the staged files will be sent to (as profile id, chat jid).
    target: RefCell<Option<(String, String)>>,
    /// Bumped when the staged set is cleared, so late thumbnails are dropped.
    generation: Cell<u64>,
    /// A pasted image is being encoded and saved on a worker thread.
    pasting: Cell<bool>,
}

impl AttachState {
    pub(super) fn new() -> Self {
        AttachState {
            items: RefCell::new(Vec::new()),
            model: Rc::new(VecModel::default()),
            target: RefCell::new(None),
            generation: Cell::new(0),
            pasting: Cell::new(false),
        }
    }

    pub(super) fn attach(&self, win: &MainWindow) {
        win.set_attach_items(ModelRc::from(self.model.clone()));
        drop_paste::enable_file_drop_with_hover(
            win.window(),
            |on| with_app(|a| a.set_drop_hover(on)),
            |paths| with_app(|a| a.drop_files(paths)),
        );
    }
}

fn kind_label(att: &Attachment) -> String {
    match att.kind {
        OutgoingMediaKind::Image => "IMAGE".to_string(),
        OutgoingMediaKind::Video => "VIDEO".to_string(),
        OutgoingMediaKind::Audio | OutgoingMediaKind::Voice => "AUDIO".to_string(),
        OutgoingMediaKind::Document => att
            .path
            .extension()
            .and_then(|e| e.to_str())
            .filter(|e| !e.is_empty() && e.chars().count() <= 4)
            .map_or_else(|| "FILE".to_string(), str::to_uppercase),
    }
}

/// View once applies to exactly one photo or video.
pub fn view_once_allowed(kinds: &[OutgoingMediaKind]) -> bool {
    matches!(kinds, [OutgoingMediaKind::Image | OutgoingMediaKind::Video])
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
}

/// One line for the toast that reports files that were not accepted.
fn rejection_text(rejected: &[(PathBuf, Rejection)]) -> String {
    let Some((path, why)) = rejected.first() else { return String::new() };
    let reason = match why {
        Rejection::NotAFile => "folders cannot be sent".to_string(),
        Rejection::Empty => "the file is empty".to_string(),
        Rejection::TooLarge(_) => format!("larger than {}", drop_paste::format_size(MAX_FILE_BYTES)),
        Rejection::TooMany => format!("at most {MAX_FILES} files at a time"),
        Rejection::Unreadable(_) => "cannot be read".to_string(),
    };
    let name = file_name(path);
    if rejected.len() == 1 {
        format!("Not added: {name} ({reason})")
    } else {
        format!("{} files not added, e.g. {name} ({reason})", rejected.len())
    }
}

fn remove_temp(path: &Path) {
    if let Err(e) = std::fs::remove_file(path) {
        log::warn!("could not delete the pasted image {}: {e}", path.display());
    }
}

/// The composer text after a paste outside the text field: clipboard text is
/// appended (CRLF folded to LF, like the native paste).
fn append_pasted(existing: &str, pasted: &str) -> String {
    let mut out = String::with_capacity(existing.len() + pasted.len());
    out.push_str(existing);
    out.push_str(&pasted.replace("\r\n", "\n").replace('\r', "\n"));
    out
}

impl App {
    /// Ctrl+V while no text field has the keyboard focus: files and images go
    /// to the attach flow, text is appended to the composer.
    pub(super) fn paste_unfocused(&self) {
        if self.is_locked() {
            return;
        }
        if self.paste_from_clipboard() {
            return;
        }
        // `false` above also covers "no chat open"; gated chats were refused there.
        if self.p().st.borrow().selected.is_none() {
            return;
        }
        let text = match arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
            Ok(t) if !t.is_empty() => t,
            Ok(_) | Err(arboard::Error::ContentNotAvailable) => return,
            Err(e) => {
                log::warn!("paste: {e}");
                return;
            }
        };
        let Some(win) = self.win() else { return };
        let new = append_pasted(win.invoke_composer_text().as_str(), &text);
        win.invoke_set_composer(new.as_str().into(), i32::try_from(new.len()).unwrap_or(i32::MAX));
        win.invoke_focus_composer();
        self.on_edit(&new);
    }

    /// `true` when Ctrl+V was taken (files or an image were staged); `false`
    /// leaves the paste to the text field.
    pub(super) fn paste_from_clipboard(&self) -> bool {
        // Behind the lock screen nothing may be staged, and the paste is not
        // left to the (covered) text field either.
        if self.is_locked() {
            return true;
        }
        let p = self.p();
        if p.st.borrow().selected.is_none() {
            return false;
        }
        if self.chat_is_blocked_dm() {
            self.show_toast("Unblock this contact to send messages");
            return true;
        }
        if self.refuse_gated() {
            return true;
        }
        let files = drop_paste::clipboard_file_list();
        if !files.is_empty() {
            self.stage(files, false);
            return true;
        }
        if self.staged.pasting.get() {
            return true;
        }
        let img = match drop_paste::read_clipboard_image() {
            Ok(Some(img)) => img,
            Ok(None) => return false,
            Err(e) => {
                log::warn!("paste: {e:#}");
                self.show_toast("Could not read the clipboard image");
                return false;
            }
        };
        let Some(jid) = p.st.borrow().selected.clone() else { return false };
        let media_dir = profiles::data_dir(&self.root, &p.meta.borrow()).join("media");
        let generation = self.staged.generation.get();
        let spawned = std::thread::Builder::new().name("paste-image".into()).spawn(move || {
            let result = drop_paste::save_clipboard_png(&media_dir, &img).map_err(|e| format!("{e:#}"));
            let queued = slint::invoke_from_event_loop(move || {
                with_app(|a| a.paste_saved(generation, result));
            });
            if let Err(e) = queued {
                log::debug!("pasted image not delivered: {e}");
            }
        });
        if let Err(e) = spawned {
            log::warn!("could not start the paste thread: {e}");
            self.show_toast("Could not read the clipboard image");
            return false;
        }
        self.staged.pasting.set(true);
        *self.staged.target.borrow_mut() = Some((p.id.clone(), jid));
        self.staged_refresh();
        if let Some(win) = self.win() {
            win.set_attach_busy(true);
            win.set_attach_open(true);
        }
        true
    }

    /// The worker finished saving a pasted image.
    fn paste_saved(&self, generation: u64, result: Result<PathBuf, String>) {
        if generation != self.staged.generation.get() {
            if let Ok(path) = result {
                remove_temp(&path);
            }
            return;
        }
        self.staged.pasting.set(false);
        if let Some(win) = self.win() {
            win.set_attach_busy(false);
        }
        match result {
            Ok(path) => self.stage(vec![path], true),
            Err(e) => {
                log::warn!("paste: {e}");
                self.show_toast("Could not save the clipboard image");
            }
        }
        if self.staged.items.borrow().is_empty() {
            self.attach_cancel();
        } else {
            self.staged_refresh();
        }
    }

    /// Files dropped on the window.
    pub(super) fn drop_files(&self, paths: Vec<PathBuf>) {
        if paths.is_empty() || self.is_locked() {
            return;
        }
        if self.chat_is_blocked_dm() {
            self.show_toast("Unblock this contact to send messages");
            return;
        }
        if self.refuse_gated() {
            return;
        }
        self.stage(paths, false);
    }

    pub(super) fn set_drop_hover(&self, on: bool) {
        let Some(win) = self.win() else { return };
        let chat_open = self.p().st.borrow().selected.is_some();
        win.set_drop_hover(on && chat_open && !self.is_locked());
    }

    /// Validates `paths` and adds the acceptable ones to the preview. `pasted`
    /// marks files we wrote ourselves, which are deleted if they are not sent.
    fn stage(&self, paths: Vec<PathBuf>, pasted: bool) {
        let discard = |paths: &[PathBuf]| {
            if pasted {
                paths.iter().for_each(|p| remove_temp(p));
            }
        };
        // Last line of defence for every path (a pasted image finishing after
        // the lock came up included).
        if self.is_locked() {
            discard(&paths);
            return;
        }
        let p = self.p();
        let Some(jid) = p.st.borrow().selected.clone() else {
            discard(&paths);
            self.show_toast("Open a chat first");
            return;
        };
        let mut sel = drop_paste::select_attachments(paths);
        let room = MAX_FILES.saturating_sub(self.staged.items.borrow().len());
        while sel.accepted.len() > room {
            if let Some(extra) = sel.accepted.pop() {
                sel.rejected.push((extra.path, Rejection::TooMany));
            }
        }
        if !sel.rejected.is_empty() {
            let rejected_paths: Vec<PathBuf> = sel.rejected.iter().map(|(p, _)| p.clone()).collect();
            discard(&rejected_paths);
            self.show_toast(&rejection_text(&sel.rejected));
        }
        if sel.accepted.is_empty() {
            return;
        }
        let fresh: Vec<PathBuf> = sel
            .accepted
            .iter()
            .filter(|a| a.kind == OutgoingMediaKind::Image)
            .map(|a| a.path.clone())
            .collect();
        self.staged.items.borrow_mut().extend(sel.accepted.into_iter().map(|att| Pending {
            att,
            temp: pasted,
            thumb: None,
        }));
        *self.staged.target.borrow_mut() = Some((p.id.clone(), jid));
        self.staged_refresh();
        if let Some(win) = self.win() {
            win.set_attach_open(true);
        }
        self.staged_thumbs(fresh);
    }

    /// Decodes thumbnails off the UI thread; the tiles show a label until then.
    fn staged_thumbs(&self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        let generation = self.staged.generation.get();
        let spawned = std::thread::Builder::new().name("attach-thumbs".into()).spawn(move || {
            for path in paths {
                let buf = match models::decode_file_scaled(&path, THUMB_DIM) {
                    Ok(b) => Some(b),
                    Err(e) => {
                        log::warn!("no preview for {}: {e}", path.display());
                        None
                    }
                };
                let queued = slint::invoke_from_event_loop(move || {
                    with_app(|a| a.staged_thumb(generation, &path, buf.map(Image::from_rgba8)));
                });
                if let Err(e) = queued {
                    log::debug!("preview thumbnail not delivered: {e}");
                    return;
                }
            }
        });
        if let Err(e) = spawned {
            log::warn!("could not start the thumbnail thread: {e}");
        }
    }

    fn staged_thumb(&self, generation: u64, path: &Path, img: Option<Image>) {
        if generation != self.staged.generation.get() {
            return;
        }
        if let Some(item) = self.staged.items.borrow_mut().iter_mut().find(|i| i.att.path == path) {
            item.thumb = img;
        }
        self.staged_refresh();
    }

    fn staged_refresh(&self) {
        let mut items: Vec<AttachItem> = self
            .staged
            .items
            .borrow()
            .iter()
            .map(|i| AttachItem {
                name: file_name(&i.att.path).into(),
                size_text: drop_paste::format_size(i.att.size).into(),
                kind_label: kind_label(&i.att).into(),
                has_thumb: i.thumb.is_some(),
                thumb: i.thumb.clone().unwrap_or_default(),
            })
            .collect();
        if self.staged.pasting.get() {
            items.push(AttachItem {
                name: "Pasted image".into(),
                size_text: "Saving...".into(),
                kind_label: "IMAGE".into(),
                has_thumb: false,
                thumb: Image::default(),
            });
        }
        self.staged.model.set_vec(items);
    }

    /// Empties the staging area; the files themselves are left alone.
    fn staged_clear(&self) -> Vec<Pending> {
        self.staged.generation.set(self.staged.generation.get() + 1);
        self.staged.pasting.set(false);
        *self.staged.target.borrow_mut() = None;
        let items = std::mem::take(&mut *self.staged.items.borrow_mut());
        self.staged.model.set_vec(Vec::new());
        if let Some(win) = self.win() {
            win.set_attach_open(false);
            win.set_attach_busy(false);
            win.set_attach_caption(Default::default());
            win.global::<AttachOptions>().set_view_once(false);
        }
        items
    }

    /// Cancel, or the chat / profile changed under the preview: pasted images
    /// that were not sent are deleted.
    pub(super) fn attach_cancel(&self) {
        if self.staged.items.borrow().is_empty() && self.staged.target.borrow().is_none() {
            return;
        }
        for item in self.staged_clear() {
            if item.temp {
                remove_temp(&item.att.path);
            }
        }
        if let Some(win) = self.win() {
            win.invoke_focus_composer();
        }
    }

    pub(super) fn attach_remove(&self, index: i32) {
        let Ok(index) = usize::try_from(index) else { return };
        let removed = {
            let mut items = self.staged.items.borrow_mut();
            if index >= items.len() {
                return;
            }
            items.remove(index)
        };
        if removed.temp {
            remove_temp(&removed.att.path);
        }
        if self.staged.items.borrow().is_empty() {
            self.attach_cancel();
        } else {
            self.staged_refresh();
        }
    }

    /// Sends every staged file; the caption and the quoted message belong to
    /// the first one only.
    pub(super) fn attach_send(&self, caption: &str) {
        let Some((profile, jid)) = self.staged.target.borrow().clone() else { return };
        let still_here = {
            let p = self.p();
            p.id == profile && p.st.borrow().selected.as_deref() == Some(jid.as_str())
        };
        if !still_here {
            self.attach_cancel();
            return;
        }
        if self.refuse_blocked(&jid) {
            return;
        }
        let mut quoted = {
            let p = self.p();
            let mut st = p.st.borrow_mut();
            st.force_bottom = true;
            st.reply.take().map(|r| r.0)
        };
        let caption = Some(caption.trim().to_string()).filter(|c| !c.is_empty());
        let wants_once = self.win().is_some_and(|w| w.global::<AttachOptions>().get_view_once());
        let kinds: Vec<OutgoingMediaKind> = self.staged.items.borrow().iter().map(|i| i.att.kind).collect();
        let view_once = wants_once && view_once_allowed(&kinds);
        let items = self.staged_clear();
        log::info!("attach: sending {} file(s), caption {}, view once {view_once}", items.len(), caption.is_some());
        for (i, item) in items.into_iter().enumerate() {
            self.send(Command::SendMedia {
                chat_jid: jid.clone(),
                path: item.att.path,
                kind: item.att.kind,
                caption: if i == 0 { caption.clone() } else { None },
                quoted_id: if i == 0 { quoted.take() } else { None },
                view_once,
            });
        }
        self.sync_reply();
        if let Some(win) = self.win() {
            win.invoke_focus_composer();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pasted_text_is_appended_with_unix_newlines() {
        assert_eq!(append_pasted("hi ", "a\r\nb\rc"), "hi a\nb\nc");
        assert_eq!(append_pasted("", "x"), "x");
    }

    #[test]
    fn view_once_needs_exactly_one_photo_or_video() {
        use OutgoingMediaKind::{Audio, Document, Image, Video};
        assert!(view_once_allowed(&[Image]));
        assert!(view_once_allowed(&[Video]));
        assert!(!view_once_allowed(&[Document]));
        assert!(!view_once_allowed(&[Audio]));
        assert!(!view_once_allowed(&[Image, Image]));
        assert!(!view_once_allowed(&[]));
    }
}
