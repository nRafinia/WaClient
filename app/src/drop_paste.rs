//! Paste-image (Ctrl+V) and drag-and-drop file support for the composer.
//!
//! Slint 1.19 does not expose file drops itself, but its winit backend lets an
//! application filter raw winit window events (`unstable-winit-030`). winit
//! already owns the OLE drop target of the window, so `DroppedFile` events
//! arrive there; no Win32 subclassing (which would not even see WM_DROPFILES
//! behind winit's drop target) and no unsafe code is needed.

use std::cell::RefCell;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use slint::winit_030::winit::event::WindowEvent;
use slint::winit_030::{EventResult, WinitWindowAccessor};
use wa_engine::api::OutgoingMediaKind;

/// WhatsApp rejects bigger media; refuse early instead of after an upload.
pub const MAX_FILE_BYTES: u64 = 100 * 1024 * 1024;
pub const MAX_FILES: usize = 10;
/// winit reports one event per dropped file with no end marker, so files are
/// collected for this long before the batch is delivered.
const DROP_BATCH_WINDOW: Duration = Duration::from_millis(60);

// ---------------------------------------------------------------- clipboard

/// Raw clipboard image: straight RGBA8 pixels.
pub struct ClipboardImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Reads an image from the clipboard (cheap enough for the UI thread; the
/// slow PNG encode and write are [`save_clipboard_png`]). `Ok(None)` when the
/// clipboard holds no image.
pub fn read_clipboard_image() -> Result<Option<ClipboardImage>> {
    let mut clipboard = arboard::Clipboard::new().context("cannot open the clipboard")?;
    let img = match clipboard.get_image() {
        Ok(img) => img,
        Err(arboard::Error::ContentNotAvailable) => return Ok(None),
        Err(e) => return Err(anyhow::Error::new(e).context("cannot read the clipboard image")),
    };
    let width = u32::try_from(img.width).context("clipboard image too wide")?;
    let height = u32::try_from(img.height).context("clipboard image too tall")?;
    Ok(Some(ClipboardImage { width, height, rgba: img.bytes.into_owned() }))
}

/// Encodes `img` and saves it as `<dir>/outgoing/paste-<timestamp>.png`. `dir`
/// is the media directory (the one the outgoing cleanup scans is
/// `<data_dir>/media/outgoing`). Meant for a worker thread.
pub fn save_clipboard_png(dir: &Path, img: &ClipboardImage) -> Result<PathBuf> {
    let png = encode_png(img.width, img.height, &img.rgba)?;
    let out_dir = dir.join("outgoing");
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("cannot create {}", out_dir.display()))?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let path = out_dir.join(format!("paste-{stamp}.png"));
    write_atomic(&path, &png)?;
    Ok(path)
}

/// Files copied in Explorer (CF_HDROP). Empty when there are none.
pub fn clipboard_file_list() -> Vec<PathBuf> {
    let Ok(mut clipboard) = arboard::Clipboard::new() else {
        return Vec::new();
    };
    clipboard.get().file_list().unwrap_or_default()
}

/// Encodes straight (non-premultiplied) RGBA8 pixels as PNG.
pub fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>> {
    use image::ImageEncoder;
    use image::codecs::png::PngEncoder;

    let expected = (width as usize).checked_mul(height as usize).and_then(|n| n.checked_mul(4));
    anyhow::ensure!(
        width > 0 && height > 0 && expected == Some(rgba.len()),
        "invalid RGBA buffer: {width}x{height} with {} bytes",
        rgba.len()
    );
    let mut out = Vec::new();
    PngEncoder::new(&mut out)
        .write_image(rgba, width, height, image::ExtendedColorType::Rgba8)
        .context("PNG encoding failed")?;
    Ok(out)
}

/// Writes to a sibling temp file and renames, so a half-written image is never
/// picked up by the sender or the cleanup.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut tmp_name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    let write = (|| -> std::io::Result<()> {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if let Err(e) = write {
        let _ = std::fs::remove_file(&tmp);
        return Err(anyhow::Error::new(e).context(format!("cannot write {}", path.display())));
    }
    Ok(())
}

// ------------------------------------------------------------ classification

/// Picks the media kind from the file header, falling back to the extension.
/// Never returns `Voice`: push-to-talk notes come only from the recorder.
pub fn kind_for_path(p: &Path) -> OutgoingMediaKind {
    let mut head = [0u8; 16];
    let n = File::open(p).and_then(|mut f| f.read(&mut head)).unwrap_or(0);
    sniff_kind(&head[..n]).unwrap_or_else(|| kind_for_extension(p))
}

fn sniff_kind(h: &[u8]) -> Option<OutgoingMediaKind> {
    use OutgoingMediaKind::*;
    if h.starts_with(&[0x89, b'P', b'N', b'G'])
        || h.starts_with(&[0xFF, 0xD8, 0xFF])
        || h.starts_with(b"GIF8")
        || h.starts_with(b"BM")
    {
        return Some(Image);
    }
    if h.len() >= 12 && &h[..4] == b"RIFF" {
        return match &h[8..12] {
            b"WEBP" => Some(Image),
            b"AVI " => Some(Video),
            b"WAVE" => Some(Audio),
            _ => None,
        };
    }
    if h.len() >= 12 && &h[4..8] == b"ftyp" {
        return Some(match &h[8..12] {
            b"M4A " | b"M4B " | b"M4P " => Audio,
            b"heic" | b"heix" | b"mif1" | b"avif" => Image,
            _ => Video,
        });
    }
    if h.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return Some(Video);
    }
    if h.starts_with(b"OggS") || h.starts_with(b"fLaC") || h.starts_with(b"ID3") {
        return Some(Audio);
    }
    if h.len() >= 2 && h[0] == 0xFF && (h[1] & 0xE0) == 0xE0 {
        return Some(Audio); // MPEG audio frame sync
    }
    if h.starts_with(b"%PDF") {
        return Some(Document);
    }
    None
}

fn kind_for_extension(p: &Path) -> OutgoingMediaKind {
    use OutgoingMediaKind::*;
    let ext = p.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "heic" | "avif") => Image,
        Some("mp4" | "mov" | "mkv" | "webm" | "avi" | "m4v" | "3gp") => Video,
        Some("mp3" | "m4a" | "aac" | "ogg" | "oga" | "opus" | "wav" | "flac" | "amr") => Audio,
        _ => Document,
    }
}

/// Why a dropped or pasted path was not accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    NotAFile,
    Empty,
    TooLarge(u64),
    TooMany,
    Unreadable(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub path: PathBuf,
    pub kind: OutgoingMediaKind,
    pub size: u64,
}

#[derive(Debug, Default)]
pub struct Selection {
    pub accepted: Vec<Attachment>,
    pub rejected: Vec<(PathBuf, Rejection)>,
}

/// Validates paths from a drop or paste: only regular, non-empty files up to
/// [`MAX_FILE_BYTES`], at most [`MAX_FILES`] of them (the rest are `TooMany`).
pub fn select_attachments(paths: Vec<PathBuf>) -> Selection {
    let mut sel = Selection::default();
    for path in paths {
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => {
                sel.rejected.push((path, Rejection::Unreadable(e.to_string())));
                continue;
            }
        };
        let verdict = if !meta.is_file() {
            Some(Rejection::NotAFile)
        } else if meta.len() == 0 {
            Some(Rejection::Empty)
        } else if meta.len() > MAX_FILE_BYTES {
            Some(Rejection::TooLarge(meta.len()))
        } else if sel.accepted.len() >= MAX_FILES {
            Some(Rejection::TooMany)
        } else {
            None
        };
        match verdict {
            Some(r) => sel.rejected.push((path, r)),
            None => {
                let kind = kind_for_path(&path);
                sel.accepted.push(Attachment { path, kind, size: meta.len() });
            }
        }
    }
    sel
}

/// "1.5 MB" style size for the preview tiles.
pub fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < KB * KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{:.1} MB", b / (KB * KB))
    }
}

// ---------------------------------------------------------------- drag & drop

/// Delivers files dropped on the window. `on_drop` runs on the UI thread, so it
/// may touch Slint handles directly. Replaces any event filter previously set
/// on this window with `on_winit_window_event`.
#[allow(dead_code)] // the app also wants the hover state: it uses `enable_file_drop_with_hover`
pub fn enable_file_drop(window: &slint::Window, on_drop: impl Fn(Vec<PathBuf>) + 'static) {
    enable_file_drop_with_hover(window, |_| {}, on_drop);
}

/// Like [`enable_file_drop`], and calls `on_hover(true)` when files are dragged
/// over the window and `on_hover(false)` when they leave or are dropped (drives
/// `DropOverlay`).
pub fn enable_file_drop_with_hover(
    window: &slint::Window,
    on_hover: impl Fn(bool) + 'static,
    on_drop: impl Fn(Vec<PathBuf>) + 'static,
) {
    let on_hover: Rc<dyn Fn(bool)> = Rc::new(on_hover);
    let on_drop: Rc<dyn Fn(Vec<PathBuf>)> = Rc::new(on_drop);
    let batch: Rc<RefCell<Vec<PathBuf>>> = Rc::default();
    let hovering = Rc::new(RefCell::new(false));

    window.on_winit_window_event(move |_, event| {
        match event {
            WindowEvent::HoveredFile(_) => {
                if !hovering.replace(true) {
                    on_hover(true);
                }
            }
            WindowEvent::HoveredFileCancelled => {
                if hovering.replace(false) {
                    on_hover(false);
                }
            }
            WindowEvent::DroppedFile(path) => {
                if hovering.replace(false) {
                    on_hover(false);
                }
                let first = {
                    let mut b = batch.borrow_mut();
                    b.push(path.clone());
                    b.len() == 1
                };
                if first {
                    let batch = batch.clone();
                    let on_drop = on_drop.clone();
                    slint::Timer::single_shot(DROP_BATCH_WINDOW, move || {
                        let files = std::mem::take(&mut *batch.borrow_mut());
                        on_drop(files);
                    });
                }
            }
            _ => {}
        }
        EventResult::Propagate
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use OutgoingMediaKind::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("waclient-drop-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    fn file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, bytes).expect("write");
        p
    }

    #[test]
    fn kind_by_extension_when_content_is_unknown() {
        let dir = temp_dir("ext");
        for (name, want) in [
            ("a.JPG", Image),
            ("a.webp", Image),
            ("a.mp4", Video),
            ("a.mkv", Video),
            ("a.mp3", Audio),
            ("a.opus", Audio),
            ("a.pdf", Document),
            ("a.docx", Document),
            ("noext", Document),
        ] {
            let p = file(&dir, name, b"plain text content");
            assert_eq!(kind_for_path(&p), want, "{name}");
        }
        assert_eq!(kind_for_path(&dir.join("missing.mp4")), Video);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn magic_bytes_beat_a_misleading_extension() {
        let dir = temp_dir("magic");
        let png = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0];
        let jpg = [0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10, b'J', b'F', b'I', b'F', 0, 1];
        let mut mp4 = vec![0, 0, 0, 0x18];
        mp4.extend_from_slice(b"ftypisom");
        let mut m4a = vec![0, 0, 0, 0x18];
        m4a.extend_from_slice(b"ftypM4A ");
        let mut webp = b"RIFF\x00\x00\x00\x00WEBPVP8 ".to_vec();
        webp.truncate(16);
        let wav = b"RIFF\x00\x00\x00\x00WAVEfmt ";
        let ogg = b"OggS\x00\x02\x00\x00\x00\x00\x00\x00";
        let pdf = b"%PDF-1.7\n";
        assert_eq!(kind_for_path(&file(&dir, "x.bin", &png)), Image);
        assert_eq!(kind_for_path(&file(&dir, "x.txt", &jpg)), Image);
        assert_eq!(kind_for_path(&file(&dir, "x.dat", &mp4)), Video);
        assert_eq!(kind_for_path(&file(&dir, "x.mp4", &m4a)), Audio);
        assert_eq!(kind_for_path(&file(&dir, "x.doc", &webp)), Image);
        assert_eq!(kind_for_path(&file(&dir, "x.doc", wav)), Audio);
        assert_eq!(kind_for_path(&file(&dir, "x.png", ogg)), Audio);
        assert_eq!(kind_for_path(&file(&dir, "x.png", pdf)), Document);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn selection_rejects_directories_empty_and_oversized_files() {
        let dir = temp_dir("limits");
        let ok = file(&dir, "ok.png", &[0x89, b'P', b'N', b'G']);
        let empty = file(&dir, "empty.txt", b"");
        let big = dir.join("big.bin");
        let f = File::create(&big).expect("create");
        f.set_len(MAX_FILE_BYTES + 1).expect("sparse");
        let exact = dir.join("exact.bin");
        File::create(&exact).expect("create").set_len(MAX_FILE_BYTES).expect("sparse");
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).expect("subdir");

        let sel = select_attachments(vec![
            ok.clone(),
            empty.clone(),
            big.clone(),
            sub.clone(),
            exact.clone(),
            dir.join("gone.txt"),
        ]);
        let paths: Vec<_> = sel.accepted.iter().map(|a| a.path.clone()).collect();
        assert_eq!(paths, vec![ok, exact]);
        assert_eq!(sel.accepted[0].kind, Image);
        assert_eq!(sel.accepted[0].size, 4);
        let why = |p: &PathBuf| sel.rejected.iter().find(|(q, _)| q == p).map(|(_, r)| r.clone());
        assert_eq!(why(&empty), Some(Rejection::Empty));
        assert_eq!(why(&big), Some(Rejection::TooLarge(MAX_FILE_BYTES + 1)));
        assert_eq!(why(&sub), Some(Rejection::NotAFile));
        assert!(matches!(why(&dir.join("gone.txt")), Some(Rejection::Unreadable(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn selection_caps_the_file_count() {
        let dir = temp_dir("count");
        let paths: Vec<PathBuf> =
            (0..MAX_FILES + 3).map(|i| file(&dir, &format!("f{i}.txt"), b"x")).collect();
        let sel = select_attachments(paths);
        assert_eq!(sel.accepted.len(), MAX_FILES);
        assert_eq!(sel.rejected.len(), 3);
        assert!(sel.rejected.iter().all(|(_, r)| *r == Rejection::TooMany));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn png_encoding_round_trips_an_rgba_buffer() {
        let (w, h) = (3u32, 2u32);
        let rgba: Vec<u8> = (0..w * h * 4).map(|i| (i * 7 % 256) as u8).collect();
        let png = encode_png(w, h, &rgba).expect("encode");
        assert!(png.starts_with(&[0x89, b'P', b'N', b'G']));
        let back = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
            .expect("decode")
            .to_rgba8();
        assert_eq!((back.width(), back.height()), (w, h));
        assert_eq!(back.into_raw(), rgba);
    }

    #[test]
    fn png_encoding_rejects_a_mismatched_buffer() {
        assert!(encode_png(2, 2, &[0; 15]).is_err());
        assert!(encode_png(0, 2, &[]).is_err());
    }

    #[test]
    fn atomic_write_leaves_no_temp_file() {
        let dir = temp_dir("atomic");
        let target = dir.join("paste-1.png");
        write_atomic(&target, b"data").expect("write");
        assert_eq!(std::fs::read(&target).expect("read"), b"data");
        let names: Vec<_> = std::fs::read_dir(&dir)
            .expect("dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["paste-1.png".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn size_text() {
        assert_eq!(format_size(12), "12 B");
        assert_eq!(format_size(2048), "2 KB");
        assert_eq!(format_size(1_572_864), "1.5 MB");
    }
}
