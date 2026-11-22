//! "Save as…" for media messages: a copy of the cached file goes wherever the
//! user picks. The file dialog and the copy run off the UI thread.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{Local, NaiveDateTime};
use wa_store::MessageKind;

use super::{App, run_launcher, validate_launch_path, with_app};

/// The "Saved" toast stays long enough to click its action.
const SAVED_TOAST_TTL: Duration = Duration::from_secs(6);
const MAX_NAME_CHARS: usize = 120;

thread_local! {
    /// The last saved file, for the toast's "Show in folder".
    static SAVED: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// Extension for a MIME type: the usual ones by name, otherwise the subtype
/// when it is short and plain.
fn ext_for_mime(mime: Option<&str>) -> Option<String> {
    let essence = mime?.split(';').next()?.trim().to_ascii_lowercase();
    let known = match essence.as_str() {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "video/mp4" => "mp4",
        "video/3gpp" => "3gp",
        "video/quicktime" => "mov",
        "video/webm" => "webm",
        "audio/ogg" | "audio/opus" => "ogg",
        "audio/mpeg" => "mp3",
        "audio/mp4" | "audio/aac" | "audio/x-m4a" => "m4a",
        "application/pdf" => "pdf",
        _ => "",
    };
    if !known.is_empty() {
        return Some(known.to_string());
    }
    let sub = essence.split_once('/')?.1;
    let plain = !sub.is_empty() && sub.len() <= 5 && sub.chars().all(|c| c.is_ascii_alphanumeric());
    plain.then(|| sub.to_string())
}

/// Makes `name` a valid Windows file name: no path separators, wildcards or
/// control characters, no trailing dots or spaces, no reserved device names,
/// bounded length. Never empty.
pub fn sanitize_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| !c.is_control() && !"\\/:*?\"<>|".contains(*c))
        .collect();
    let cleaned = cleaned.trim().trim_end_matches(['.', ' ']).to_string();
    let (stem, ext) = match cleaned.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() && e.chars().count() <= 10 => (s.to_string(), format!(".{e}")),
        _ => (cleaned.clone(), String::new()),
    };
    let room = MAX_NAME_CHARS.saturating_sub(ext.chars().count()).max(1);
    let stem: String = stem.chars().take(room).collect();
    let stem = stem.trim_end_matches(['.', ' ']);
    if stem.is_empty() {
        return format!("file{ext}");
    }
    let device = stem.split('.').next().unwrap_or(stem).trim_end().to_ascii_uppercase();
    let reserved = matches!(device.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"]
            .iter()
            .any(|p| device.strip_prefix(p).is_some_and(|n| matches!(n.as_bytes(), [b'1'..=b'9'])));
    if reserved { format!("_{stem}{ext}") } else { format!("{stem}{ext}") }
}

/// Suggested file name in the save dialog.
pub fn default_save_name(
    kind: MessageKind,
    media_name: Option<&str>,
    mime: Option<&str>,
    when: NaiveDateTime,
) -> String {
    let stamp = when.format("%Y-%m-%d %H.%M");
    let ext = ext_for_mime(mime);
    let original = media_name.map(str::trim).filter(|n| !n.is_empty());
    let with_ext = |label: &str, fallback: &str| {
        format!("{label} {stamp}.{}", ext.as_deref().unwrap_or(fallback))
    };
    let name = match kind {
        // WhatsApp voice notes are Ogg/Opus whatever the MIME type says.
        MessageKind::Voice => format!("Voice message {stamp}.ogg"),
        MessageKind::Audio => original.map_or_else(|| with_ext("Audio", "ogg"), str::to_string),
        MessageKind::Document => original.map_or_else(|| with_ext("Document", "bin"), str::to_string),
        MessageKind::Image => with_ext("Image", "jpg"),
        MessageKind::Video => with_ext("Video", "mp4"),
        MessageKind::Sticker => with_ext("Sticker", "webp"),
        MessageKind::Text | MessageKind::Other => with_ext("File", "bin"),
    };
    sanitize_file_name(&name)
}

/// Copies `src` to `dest`. Refuses to write through a link at `dest` and
/// never copies a file onto itself (that would truncate it).
fn copy_file(src: &Path, dest: &Path) -> std::io::Result<()> {
    if let Ok(md) = std::fs::symlink_metadata(dest) {
        if md.file_type().is_symlink() {
            return Err(std::io::Error::other("refusing to write through a link"));
        }
        if md.is_dir() {
            return Err(std::io::Error::other("the destination is a folder"));
        }
        if let (Ok(a), Ok(b)) = (std::fs::canonicalize(src), std::fs::canonicalize(dest))
            && a == b
        {
            return Ok(());
        }
    }
    std::fs::copy(src, dest).map(drop)
}

/// The user's Downloads folder, else Documents.
fn start_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        known_folder::path(&known_folder::DOWNLOADS)
            .or_else(|| known_folder::path(&known_folder::DOCUMENTS))
            .or_else(|| std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join("Downloads")))
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(|p| PathBuf::from(p).join("Downloads"))
    }
}

#[cfg(windows)]
mod known_folder {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStringExt;
    use std::path::PathBuf;

    #[repr(C)]
    pub struct Guid(u32, u16, u16, [u8; 8]);

    pub const DOWNLOADS: Guid = Guid(0x374D_E290, 0x123F, 0x4565, [0x91, 0x64, 0x39, 0xC4, 0x92, 0x5E, 0x46, 0x7B]);
    pub const DOCUMENTS: Guid = Guid(0xFDD3_9AD0, 0x238F, 0x46AF, [0xAD, 0xB4, 0x6C, 0x85, 0x48, 0x03, 0x69, 0xC7]);

    #[link(name = "shell32")]
    unsafe extern "system" {
        fn SHGetKnownFolderPath(id: *const Guid, flags: u32, token: isize, out: *mut *mut u16) -> i32;
    }

    #[link(name = "ole32")]
    unsafe extern "system" {
        fn CoTaskMemFree(p: *mut c_void);
    }

    pub fn path(id: &Guid) -> Option<PathBuf> {
        let mut raw: *mut u16 = std::ptr::null_mut();
        // SAFETY: `id` and `raw` are valid for the call; on success the shell
        // returns a NUL-terminated UTF-16 string that is copied here and then
        // released with CoTaskMemFree, as the API requires (also on failure).
        unsafe {
            let hr = SHGetKnownFolderPath(id, 0, 0, &mut raw);
            let found = if hr >= 0 && !raw.is_null() {
                let mut len = 0;
                while *raw.add(len) != 0 {
                    len += 1;
                }
                Some(PathBuf::from(std::ffi::OsString::from_wide(std::slice::from_raw_parts(raw, len))))
            } else {
                None
            };
            CoTaskMemFree(raw.cast());
            found
        }
    }
}

impl App {
    /// Menu action: saves the message's file, downloading it first when needed.
    pub(super) fn save_as(&self, id: &str) {
        let row = {
            let p = self.p();
            let st = p.st.borrow();
            st.msgs.iter().find(|m| m.id == id && !m.revoked).cloned()
        };
        let Some(m) = row else { return };
        if matches!(m.kind, MessageKind::Text | MessageKind::Other) {
            return;
        }
        let Some(local) = m.local_path.clone() else {
            self.p().st.borrow_mut().save_after.insert(id.to_string());
            self.show_toast("Downloading…");
            self.download(id);
            return;
        };
        let src = match validate_launch_path(&local, &self.media_roots()) {
            Ok(real) if std::fs::symlink_metadata(&local).is_ok_and(|md| md.is_file()) => real,
            Ok(_) => {
                self.show_banner(1, "The downloaded file is missing and cannot be saved");
                return;
            }
            Err(e) => {
                log::warn!("refused to save {local}: {e}");
                self.show_banner(1, "That file is outside the app's media folder and was not saved");
                return;
            }
        };
        let when = chrono::DateTime::from_timestamp(m.ts, 0)
            .map_or_else(|| Local::now().naive_local(), |d| d.with_timezone(&Local).naive_local());
        let name = default_save_name(m.kind, m.media_name.as_deref(), m.media_mime.as_deref(), when);
        let dir = start_dir();
        let spawned = std::thread::Builder::new().name("save-dialog".into()).spawn(move || {
            let mut dialog = rfd::FileDialog::new().set_title("Save as").set_file_name(name);
            if let Some(d) = dir.filter(|d| d.is_dir()) {
                dialog = dialog.set_directory(d);
            }
            // The dialog asks before replacing an existing file.
            let Some(dest) = dialog.save_file() else { return };
            let result = copy_file(&src, &dest).map_err(|e| e.to_string());
            let queued = slint::invoke_from_event_loop(move || {
                with_app(|a| a.save_finished(dest, result));
            });
            if let Err(e) = queued {
                log::error!("save result not delivered: {e}");
            }
        });
        if let Err(e) = spawned {
            log::error!("could not start the save dialog thread: {e}");
            self.show_banner(1, &format!("Could not open the save dialog: {e}"));
        }
    }

    fn save_finished(&self, dest: PathBuf, result: Result<(), String>) {
        match result {
            Ok(()) => {
                SAVED.with(|s| *s.borrow_mut() = Some(dest));
                self.show_toast_action("Saved", "Show in folder", SAVED_TOAST_TTL);
            }
            Err(e) => {
                log::error!("could not save {}: {e}", dest.display());
                self.show_banner(1, &format!("Could not save the file: {e}"));
            }
        }
    }

    /// The toast's action: reveal the saved file in the file manager.
    pub(super) fn toast_action(&self) {
        let saved = SAVED.with(|s| s.borrow().clone());
        if let Some(path) = saved {
            run_launcher(path, true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn when() -> NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2026, 3, 7)
            .and_then(|d| d.and_hms_opt(9, 5, 0))
            .expect("valid date")
    }

    #[test]
    fn voice_notes_are_ogg() {
        let n = default_save_name(MessageKind::Voice, None, Some("audio/ogg; codecs=opus"), when());
        assert_eq!(n, "Voice message 2026-03-07 09.05.ogg");
    }

    #[test]
    fn images_and_videos_use_the_mime_extension() {
        assert_eq!(
            default_save_name(MessageKind::Image, None, Some("image/jpeg"), when()),
            "Image 2026-03-07 09.05.jpg"
        );
        assert_eq!(
            default_save_name(MessageKind::Video, Some("clip.mov"), Some("video/mp4"), when()),
            "Video 2026-03-07 09.05.mp4"
        );
        assert_eq!(
            default_save_name(MessageKind::Sticker, None, None, when()),
            "Sticker 2026-03-07 09.05.webp"
        );
    }

    #[test]
    fn documents_keep_their_name_sanitized() {
        let n = default_save_name(MessageKind::Document, Some("a/b:c*d?.pdf"), None, when());
        assert_eq!(n, "abcd.pdf");
        let n = default_save_name(MessageKind::Document, None, Some("application/pdf"), when());
        assert_eq!(n, "Document 2026-03-07 09.05.pdf");
    }

    #[test]
    fn sanitizing_handles_the_awkward_cases() {
        assert_eq!(sanitize_file_name("  report. . "), "report");
        assert_eq!(sanitize_file_name("CON"), "_CON");
        assert_eq!(sanitize_file_name("nul.txt"), "_nul.txt");
        assert_eq!(sanitize_file_name("com1.log"), "_com1.log");
        assert_eq!(sanitize_file_name("COM0"), "COM0");
        assert_eq!(sanitize_file_name("..."), "file");
        assert_eq!(sanitize_file_name("a\u{0}b\tc|d"), "abcd");
        assert_eq!(sanitize_file_name("..\\..\\evil.exe"), "....evil.exe");
    }

    #[test]
    fn long_names_are_cut_but_keep_the_extension() {
        let n = sanitize_file_name(&format!("{}.pdf", "x".repeat(300)));
        assert!(n.ends_with(".pdf"));
        assert_eq!(n.chars().count(), MAX_NAME_CHARS);
    }

    #[test]
    fn copy_keeps_the_source_and_never_truncates_it() {
        let dir = std::env::temp_dir().join(format!("waclient-save-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let src = dir.join("a.bin");
        let dst = dir.join("b.bin");
        std::fs::write(&src, b"payload").expect("write");
        copy_file(&src, &dst).expect("copy");
        assert_eq!(std::fs::read(&dst).expect("read"), b"payload");
        assert!(src.exists());
        copy_file(&src, &src).expect("self copy is a no-op");
        assert_eq!(std::fs::read(&src).expect("read"), b"payload");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
