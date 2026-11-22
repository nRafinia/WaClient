//! The photo/video viewer of a chat: zoom (wheel, keys, double-click, drag to
//! pan), rotate, and browsing all media of the chat. The geometry is pure
//! (see [`place`]); the window only draws the rectangle it is given.
//!
//! Videos are not played here: they show their thumbnail with an "Open video"
//! button that hands the file to the system player, as a click on a video
//! bubble does.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use chrono::Local;
use slint::{ComponentHandle, Image, Rgba8Pixel, SharedPixelBuffer};
use wa_store::{ChatKind, ChatRow, MessageKind, MessageRow};

use super::{App, validate_launch_path, with_app};
use crate::decode::DecodeJob;
use crate::{MainWindow, MediaViewerState, MsgOps, models};

/// Zoom is relative to "fit": 1.0 shows the whole image.
const MAX_ZOOM: f32 = 8.0;
/// Zoom factor of one + / - key press.
const KEY_ZOOM_STEP: f32 = 1.25;
/// A full image is never enlarged beyond this when fitted to a large window.
const MAX_UPSCALE: f32 = 2.0;
/// A thumbnail (shown while the file loads, and for videos) may grow more.
const MAX_THUMB_UPSCALE: f32 = 4.0;
/// Longest edge of the decoded full image (memory: 4 bytes per pixel).
const VIEWER_MAX_DIM: u32 = 3072;
/// Most media rows loaded for browsing.
const MEDIA_LIMIT: u32 = 2000;
/// A double-click zooms in to at least this much.
const DOUBLE_CLICK_ZOOM: f32 = 2.0;

// ---------------------------------------------------------------- pure helpers

/// Dimensions of a `w` x `h` image after `quarters` quarter turns.
pub fn rotated_dims(w: f32, h: f32, quarters: u8) -> (f32, f32) {
    if quarters % 2 == 1 { (h, w) } else { (w, h) }
}

/// Scale that makes the rotated image fit the viewport, at most `max_scale`.
pub fn fit_scale(w: f32, h: f32, vw: f32, vh: f32, quarters: u8, max_scale: f32) -> f32 {
    let (rw, rh) = rotated_dims(w, h, quarters);
    if rw <= 0.0 || rh <= 0.0 || vw <= 0.0 || vh <= 0.0 {
        return 1.0;
    }
    (vw / rw).min(vh / rh).min(max_scale)
}

pub fn clamp_zoom(z: f32) -> f32 {
    if z.is_finite() { z.clamp(1.0, MAX_ZOOM) } else { 1.0 }
}

/// Keeps the image covering the viewport on an axis where it is larger, and
/// centred where it is smaller. `bw`/`bh` is the on-screen size of the image.
pub fn clamp_pan(pan: (f32, f32), bw: f32, bh: f32, vw: f32, vh: f32) -> (f32, f32) {
    let limit = |b: f32, v: f32| ((b - v) / 2.0).max(0.0);
    let (lx, ly) = (limit(bw, vw), limit(bh, vh));
    (pan.0.clamp(-lx, lx), pan.1.clamp(-ly, ly))
}

/// New pan after changing the zoom from `z_old` to `z_new` so that the image
/// point under `cursor` (relative to the viewport centre) stays where it is.
/// Not clamped.
pub fn zoom_about(pan: (f32, f32), z_old: f32, z_new: f32, cursor: (f32, f32)) -> (f32, f32) {
    if z_old <= 0.0 {
        return pan;
    }
    let k = z_new / z_old;
    (cursor.0 - (cursor.0 - pan.0) * k, cursor.1 - (cursor.1 - pan.1) * k)
}

/// Zoom factor of one wheel event (`delta` > 0: wheel up = zoom in).
pub fn wheel_factor(delta: f32) -> f32 {
    if delta.is_finite() { (delta * 0.003).exp().clamp(0.5, 2.0) } else { 1.0 }
}

/// Double-click: back to fit when zoomed in, else to 100% (or 2x when that is
/// not more).
pub fn double_click_zoom(zoom: f32, fit: f32) -> f32 {
    if zoom > 1.01 {
        1.0
    } else if fit > 0.0 {
        clamp_zoom((1.0 / fit).max(DOUBLE_CLICK_ZOOM))
    } else {
        DOUBLE_CLICK_ZOOM
    }
}

/// Index of the neighbour of `idx` in a newest-first list of `len` items.
pub fn neighbor(idx: usize, len: usize, older: bool) -> Option<usize> {
    if older {
        let next = idx.checked_add(1)?;
        (next < len).then_some(next)
    } else {
        idx.checked_sub(1).filter(|_| idx < len)
    }
}

/// "12 / 87": position counted from the oldest item, so the newest is last.
pub fn position(idx: usize, len: usize) -> (usize, usize) {
    (len.saturating_sub(idx), len)
}

/// Where the image goes in the viewport.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    /// Pixels on screen per image pixel.
    pub scale: f32,
    /// Clamped offset of the image centre from the viewport centre.
    pub pan: (f32, f32),
    /// The image rectangle on screen (after rotation).
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

pub fn place(
    iw: f32,
    ih: f32,
    vw: f32,
    vh: f32,
    quarters: u8,
    zoom: f32,
    pan: (f32, f32),
    max_scale: f32,
) -> Placement {
    let scale = fit_scale(iw, ih, vw, vh, quarters, max_scale) * clamp_zoom(zoom);
    let (rw, rh) = rotated_dims(iw, ih, quarters);
    let pan = clamp_pan(pan, rw * scale, rh * scale, vw, vh);
    let (w, h) = (rw * scale, rh * scale);
    Placement { scale, pan, x: vw / 2.0 + pan.0 - w / 2.0, y: vh / 2.0 + pan.1 - h / 2.0, w, h }
}

fn sender_label(m: &MessageRow, chats: &[ChatRow]) -> String {
    if m.from_me {
        return "You".to_string();
    }
    if let Some(n) = m.sender_name.as_deref().filter(|n| !n.trim().is_empty()) {
        return n.to_string();
    }
    if let Some(c) = chats.iter().find(|c| c.jid == m.chat_jid && c.kind == ChatKind::Dm) {
        return c.display_name.clone();
    }
    m.sender_jid.split('@').next().unwrap_or("").to_string()
}

fn date_label(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| d.with_timezone(&Local).format("%d %b %Y, %H:%M").to_string())
        .unwrap_or_default()
}

// ---------------------------------------------------------------- state

struct Viewer {
    profile: String,
    chat: String,
    /// Images and videos of the chat, newest first.
    items: Vec<MessageRow>,
    idx: usize,
    quarters: u8,
    zoom: f32,
    pan: (f32, f32),
    /// Pan at the start of the current drag.
    pan_base: (f32, f32),
    /// Size of the area the image is drawn in.
    view: (f32, f32),
    thumb: Option<Image>,
    /// The displayed image turned by `quarters`, for the item/size it was made
    /// for. Turned in memory because not every renderer rotates images.
    turned: Option<((usize, bool, u8), Image)>,
    /// The full-size turn a worker thread is busy with.
    turning: Option<(usize, bool, u8)>,
    /// Decoded full images by file path (the current one and its neighbours).
    cache: HashMap<String, Image>,
    pending: HashSet<String>,
    bad: HashSet<String>,
    /// Ids whose download failed.
    dl_failed: HashSet<String>,
    save_when_ready: Option<String>,
    open_when_ready: Option<String>,
}

thread_local! {
    static VIEWER: RefCell<Option<Viewer>> = const { RefCell::new(None) };
}

/// Runs `f` on the viewer, if there is one and it is not borrowed right now
/// (a window callback fired while a borrow is held is dropped, never a panic).
fn with_viewer<R>(f: impl FnOnce(&mut Viewer) -> R) -> Option<R> {
    VIEWER.with(|v| v.try_borrow_mut().ok().and_then(|mut v| v.as_mut().map(f)))
}

struct Geom {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    zoom_text: String,
    zoomed: bool,
}

/// What is drawn: the image as shown (already turned) and the size in pixels
/// of the image before turning.
struct Shown {
    image: Image,
    w: f32,
    h: f32,
    full: bool,
}

/// `buf` turned clockwise by `quarters` quarter turns. Allocates full-size
/// copies, so a large image is turned on a worker thread (see `Viewer::start_turn`).
fn turn_pixels(buf: &SharedPixelBuffer<Rgba8Pixel>, quarters: u8) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let rgba = image::RgbaImage::from_raw(buf.width(), buf.height(), buf.as_bytes().to_vec())?;
    let out = match quarters % 4 {
        1 => image::imageops::rotate90(&rgba),
        2 => image::imageops::rotate180(&rgba),
        3 => image::imageops::rotate270(&rgba),
        _ => rgba,
    };
    let (w, h) = out.dimensions();
    Some(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(out.as_raw(), w, h))
}

/// `img` turned clockwise by `quarters` quarter turns, on the calling thread (small images only).
fn turn(img: &Image, quarters: u8) -> Option<Image> {
    turn_pixels(&img.to_rgba8()?, quarters).map(Image::from_rgba8)
}

struct ItemFrame {
    source: Option<Image>,
    loading: bool,
    failed: bool,
    is_video: bool,
    sender: String,
    date: String,
    counter: String,
    caption: String,
    has_older: bool,
    has_newer: bool,
    can_copy: bool,
}

impl Viewer {
    fn row(&self) -> Option<&MessageRow> {
        self.items.get(self.idx)
    }

    fn is_video(&self) -> bool {
        self.row().is_some_and(|m| m.kind == MessageKind::Video)
    }

    /// The full image when decoded, else the thumbnail (not turned).
    fn base(&self) -> Option<(Image, bool)> {
        let m = self.row()?;
        if m.kind == MessageKind::Image
            && let Some(img) = m.local_path.as_ref().and_then(|p| self.cache.get(p))
        {
            return Some((img.clone(), true));
        }
        self.thumb.clone().map(|t| (t, false))
    }

    fn shown(&mut self) -> Option<Shown> {
        let (base, full) = self.base()?;
        let size = base.size();
        let (w, h) = (size.width as f32, size.height as f32);
        if self.quarters % 4 == 0 {
            return Some(Shown { image: base, w, h, full });
        }
        let key = (self.idx, full, self.quarters);
        if let Some((k, img)) = &self.turned
            && *k == key
        {
            return Some(Shown { image: img.clone(), w, h, full });
        }
        if !full {
            let out = turn(&base, self.quarters).unwrap_or(base);
            self.turned = Some((key, out.clone()));
            return Some(Shown { image: out, w, h, full });
        }
        self.start_turn(key, &base);
        if let Some((k, img)) = &self.turned
            && *k == key
        {
            return Some(Shown { image: img.clone(), w, h, full });
        }
        // Until the worker is done the (small) thumbnail stands in, turned.
        let thumb = self.thumb.clone()?;
        let tkey = (self.idx, false, self.quarters);
        let image = match &self.turned {
            Some((k, img)) if *k == tkey => img.clone(),
            _ => {
                let out = turn(&thumb, self.quarters)?;
                self.turned = Some((tkey, out.clone()));
                out
            }
        };
        let tsize = thumb.size();
        Some(Shown { image, w: tsize.width as f32, h: tsize.height as f32, full: false })
    }

    /// Turns the full image on a worker thread so the window does not stall;
    /// `App::viewer_turned` receives the result.
    fn start_turn(&mut self, key: (usize, bool, u8), base: &Image) {
        if self.turning == Some(key) {
            return;
        }
        let Some(buf) = base.to_rgba8() else { return };
        let quarters = key.2;
        let spawned = std::thread::Builder::new().name("turn-image".into()).spawn(move || {
            let out = turn_pixels(&buf, quarters);
            let queued = slint::invoke_from_event_loop(move || with_app(|a| a.viewer_turned(key, out)));
            if let Err(e) = queued {
                log::error!("turned image not delivered: {e}");
            }
        });
        match spawned {
            Ok(_) => self.turning = Some(key),
            Err(e) => {
                log::error!("could not start the turn thread: {e}");
                let out = turn(base, quarters).unwrap_or_else(|| base.clone());
                self.turned = Some((key, out));
            }
        }
    }

    fn max_scale(full: bool) -> f32 {
        if full { MAX_UPSCALE } else { MAX_THUMB_UPSCALE }
    }

    fn fit(&self) -> f32 {
        match self.base() {
            Some((img, full)) => {
                let size = img.size();
                let (iw, ih) = (size.width as f32, size.height as f32);
                fit_scale(iw, ih, self.view.0, self.view.1, self.quarters, Self::max_scale(full))
            }
            None => 1.0,
        }
    }

    /// Recomputes the placement (clamping the pan) for the window.
    fn relayout(&mut self) -> Geom {
        let hidden = Geom { x: 0.0, y: 0.0, w: 0.0, h: 0.0, zoom_text: String::new(), zoomed: false };
        let Some(shown) = self.shown() else { return hidden };
        let max = Self::max_scale(shown.full);
        let pl = place(shown.w, shown.h, self.view.0, self.view.1, self.quarters, self.zoom, self.pan, max);
        self.pan = pl.pan;
        Geom {
            x: pl.x,
            y: pl.y,
            w: pl.w,
            h: pl.h,
            zoom_text: if self.is_video() { String::new() } else { format!("{}%", (pl.scale * 100.0).round() as i32) },
            zoomed: self.zoom > 1.001,
        }
    }

    fn zoom_to(&mut self, new_zoom: f32, anchor: (f32, f32)) {
        let z = clamp_zoom(new_zoom);
        self.pan = zoom_about(self.pan, self.zoom, z, anchor);
        self.zoom = z;
    }

    fn reset_view(&mut self) {
        self.quarters = 0;
        self.zoom = 1.0;
        self.pan = (0.0, 0.0);
        self.pan_base = (0.0, 0.0);
    }

    /// Switches to item `idx`: the view starts at fit, unrotated.
    fn goto(&mut self, idx: usize) {
        self.idx = idx;
        self.reset_view();
        self.thumb = self
            .row()
            .and_then(|m| m.thumbnail.as_deref())
            .and_then(models::decode_image_bytes);
        self.dl_failed.clear();
        self.turned = None;
        self.turning = None;
    }

    /// Keeps the decoded images of the current item and its neighbours only.
    fn trim_cache(&mut self) {
        let lo = self.idx.saturating_sub(1);
        let keep: HashSet<&str> = self
            .items
            .iter()
            .skip(lo)
            .take(3)
            .filter_map(|m| m.local_path.as_deref())
            .collect();
        self.cache.retain(|k, _| keep.contains(k.as_str()));
    }

    fn item_frame(&mut self, chats: &[ChatRow]) -> ItemFrame {
        let shown = self.shown();
        let m = self.row();
        let image = m.is_some_and(|m| m.kind == MessageKind::Image);
        let ready = shown.as_ref().is_some_and(|s| s.full);
        let path_bad = m.and_then(|m| m.local_path.as_ref()).is_some_and(|p| self.bad.contains(p));
        let dl_failed = m.is_some_and(|m| self.dl_failed.contains(&m.id));
        let failed = image && !ready && (path_bad || dl_failed);
        let (n, len) = position(self.idx, self.items.len());
        ItemFrame {
            source: shown.map(|s| s.image),
            loading: image && !ready && !failed,
            failed,
            is_video: self.is_video(),
            sender: m.map(|m| sender_label(m, chats)).unwrap_or_default(),
            date: m.map(|m| date_label(m.ts)).unwrap_or_default(),
            counter: format!("{n} / {len}"),
            caption: m.and_then(|m| m.text.as_deref()).map(|t| t.trim().to_string()).unwrap_or_default(),
            has_older: neighbor(self.idx, self.items.len(), true).is_some(),
            has_newer: neighbor(self.idx, self.items.len(), false).is_some(),
            can_copy: ready,
        }
    }
}

// ---------------------------------------------------------------- window glue

fn push_geom(win: &MainWindow, g: &Geom) {
    let s = win.global::<MediaViewerState>();
    s.set_img_x(g.x);
    s.set_img_y(g.y);
    s.set_img_w(g.w);
    s.set_img_h(g.h);
    s.set_zoom_text(g.zoom_text.as_str().into());
    s.set_zoomed(g.zoomed);
}

fn push_item(win: &MainWindow, f: &ItemFrame) {
    let s = win.global::<MediaViewerState>();
    s.set_has_image(f.source.is_some());
    if let Some(img) = &f.source {
        s.set_source(img.clone());
    }
    s.set_loading(f.loading);
    s.set_failed(f.failed);
    s.set_is_video(f.is_video);
    s.set_sender(f.sender.as_str().into());
    s.set_date(f.date.as_str().into());
    s.set_counter(f.counter.as_str().into());
    s.set_caption(f.caption.as_str().into());
    s.set_has_older(f.has_older);
    s.set_has_newer(f.has_newer);
    s.set_can_copy(f.can_copy);
}

pub(super) fn wire(win: &MainWindow) {
    let g = win.global::<MediaViewerState>();
    g.on_close(|| with_app(App::media_viewer_close));
    g.on_step(|older| with_app(|a| a.viewer_step(older)));
    g.on_zoom_at(|x, y, delta| with_app(|a| a.viewer_wheel(x, y, delta)));
    g.on_zoom_key(|zoom_in| with_app(|a| a.viewer_zoom_key(zoom_in)));
    g.on_zoom_fit(|| with_app(App::viewer_fit));
    g.on_toggle_zoom(|x, y| with_app(|a| a.viewer_toggle_zoom(x, y)));
    g.on_pan_begin(|| with_app(App::viewer_pan_begin));
    g.on_pan_by(|dx, dy| with_app(|a| a.viewer_pan_by(dx, dy)));
    g.on_viewport(|w, h| with_app(|a| a.viewer_resized(w, h)));
    g.on_rotate(|| with_app(App::viewer_rotate));
    g.on_save(|| with_app(App::viewer_save));
    g.on_forward(|| with_app(App::viewer_forward));
    g.on_show_in_chat(|| with_app(App::viewer_show_in_chat));
    g.on_copy(|| with_app(App::viewer_copy));
    g.on_open_video(|| with_app(App::viewer_open_video));
}

impl App {
    /// A click on a photo bubble: opens the viewer on it. False for anything
    /// else (the caller then opens the file as before).
    pub(super) fn viewer_try_open(&self, id: &str) -> bool {
        let Some(win) = self.win() else { return false };
        let (profile, chat, clicked, store) = {
            let p = self.p();
            let st = p.st.borrow();
            let Some(chat) = st.selected.clone() else { return false };
            let Some(m) = st.msgs.iter().find(|m| m.id == id && m.kind == MessageKind::Image && !m.revoked).cloned()
            else {
                return false;
            };
            (p.id.clone(), chat, m, p.store.clone())
        };
        let mut items = match store.chat_media(&chat, None, MEDIA_LIMIT) {
            Ok(v) => v,
            Err(e) => {
                log::warn!("could not list the chat media: {e}");
                Vec::new()
            }
        };
        let idx = match items.iter().position(|m| m.id == id) {
            Some(i) => {
                if items[i].local_path.is_none() {
                    items[i].local_path.clone_from(&clicked.local_path);
                }
                i
            }
            None => {
                items = vec![clicked];
                0
            }
        };
        let mut v = Viewer {
            profile,
            chat,
            items,
            idx,
            quarters: 0,
            zoom: 1.0,
            pan: (0.0, 0.0),
            pan_base: (0.0, 0.0),
            view: (0.0, 0.0),
            thumb: None,
            turned: None,
            turning: None,
            cache: HashMap::new(),
            pending: HashSet::new(),
            bad: HashSet::new(),
            dl_failed: HashSet::new(),
            save_when_ready: None,
            open_when_ready: None,
        };
        v.goto(idx);
        VIEWER.with(|slot| *slot.borrow_mut() = Some(v));
        win.global::<MediaViewerState>().set_open(true);
        self.viewer_show();
        true
    }

    pub(super) fn media_viewer_close(&self) {
        let was_open = VIEWER.with(|v| v.borrow_mut().take()).is_some();
        let Some(win) = self.win() else { return };
        let s = win.global::<MediaViewerState>();
        if s.get_open() {
            s.set_open(false);
            s.set_source(Image::default());
            s.set_has_image(false);
        }
        if was_open {
            win.invoke_focus_composer();
        }
    }

    /// False (and the viewer closed) when the profile or chat it was opened
    /// for is no longer the open one.
    fn viewer_live(&self) -> bool {
        let live = {
            let p = self.p();
            let sel = p.st.borrow().selected.clone();
            with_viewer(|v| v.profile == p.id && sel.as_deref() == Some(v.chat.as_str()))
        };
        if live != Some(true) {
            self.media_viewer_close();
            return false;
        }
        true
    }

    /// Pushes the current item and starts what it needs (decode, download).
    fn viewer_show(&self) {
        enum Need {
            Nothing,
            Decode(String),
            Download(String),
        }
        let Some(need) = with_viewer(|v| {
            let Some(m) = v.row() else { return Need::Nothing };
            match (m.kind, m.local_path.clone()) {
                (MessageKind::Image, Some(path)) => {
                    if v.cache.contains_key(&path) || v.bad.contains(&path) || !v.pending.insert(path.clone()) {
                        Need::Nothing
                    } else {
                        Need::Decode(path)
                    }
                }
                (MessageKind::Image, None) if !v.dl_failed.contains(&m.id) => Need::Download(m.id.clone()),
                _ => Need::Nothing,
            }
        }) else {
            return;
        };
        match need {
            Need::Decode(path) => self.viewer_submit_decode(path),
            Need::Download(id) => self.download(&id),
            Need::Nothing => {}
        }
        self.viewer_prefetch();
        self.viewer_push_all();
    }

    fn viewer_submit_decode(&self, path: String) {
        let job = DecodeJob::Media { path: PathBuf::from(&path), max_dim: VIEWER_MAX_DIM };
        if !self.pool.submit(job) {
            with_viewer(|v| {
                v.pending.remove(&path);
                v.bad.insert(path);
            });
        }
    }

    /// Decodes the neighbours that are already downloaded, so stepping is instant.
    fn viewer_prefetch(&self) {
        let paths: Vec<String> = with_viewer(|v| {
            let mut out = Vec::new();
            for older in [false, true] {
                let Some(i) = neighbor(v.idx, v.items.len(), older) else { continue };
                let Some(m) = v.items.get(i) else { continue };
                let Some(path) = m.local_path.clone().filter(|_| m.kind == MessageKind::Image) else { continue };
                if !v.cache.contains_key(&path) && !v.bad.contains(&path) && v.pending.insert(path.clone()) {
                    out.push(path);
                }
            }
            out
        })
        .unwrap_or_default();
        for p in paths {
            self.viewer_submit_decode(p);
        }
    }

    fn viewer_push_all(&self) {
        let Some(win) = self.win() else { return };
        let chats = self.p().st.borrow().chats.clone();
        let Some((frame, geom)) = with_viewer(|v| (v.item_frame(&chats), v.relayout())) else { return };
        push_item(&win, &frame);
        push_geom(&win, &geom);
    }

    fn viewer_push_geom(&self) {
        let Some(win) = self.win() else { return };
        let Some(geom) = with_viewer(Viewer::relayout) else { return };
        push_geom(&win, &geom);
    }

    /// A worker finished turning the full image for `key`; a result for a
    /// view that has moved on is dropped.
    fn viewer_turned(&self, key: (usize, bool, u8), out: Option<SharedPixelBuffer<Rgba8Pixel>>) {
        let current = with_viewer(|v| {
            if v.turning != Some(key) {
                return false;
            }
            v.turning = None;
            // A failed turn shows the picture unturned, as before.
            let image = out.map(Image::from_rgba8).or_else(|| v.base().map(|(img, _)| img));
            v.turned = image.map(|img| (key, img));
            true
        })
        .unwrap_or(false);
        if current {
            self.viewer_push_all();
        }
    }

    /// A full image finished decoding. Consumes only the paths the viewer asked for.
    pub(super) fn viewer_decoded(&self, path: &str, buf: &Option<SharedPixelBuffer<Rgba8Pixel>>) {
        let current = with_viewer(|v| {
            if !v.pending.remove(path) {
                return None;
            }
            match buf {
                Some(b) => {
                    v.cache.insert(path.to_string(), Image::from_rgba8(b.clone()));
                }
                None => {
                    v.bad.insert(path.to_string());
                }
            }
            v.trim_cache();
            Some(v.row().is_some_and(|m| m.local_path.as_deref() == Some(path)))
        })
        .flatten();
        if current == Some(true) {
            self.viewer_push_all();
        }
    }

    /// A download finished (any chat of any profile): updates the viewer's rows
    /// and finishes what was waiting for it.
    pub(super) fn viewer_media_ready(&self, profile: &str, chat: &str, id: &str, path: &std::path::Path) {
        let path_s = path.to_string_lossy().into_owned();
        let done = with_viewer(|v| {
            if v.profile != profile || v.chat != chat {
                return None;
            }
            for m in v.items.iter_mut().filter(|m| m.id == id) {
                if m.local_path.is_none() {
                    m.local_path = Some(path_s.clone());
                }
            }
            v.dl_failed.remove(id);
            let row = v.items.iter().find(|m| m.id == id).cloned()?;
            let current = v.row().is_some_and(|m| m.id == id);
            let save = v.save_when_ready.as_deref() == Some(id);
            let open = v.open_when_ready.as_deref() == Some(id);
            if save {
                v.save_when_ready = None;
            }
            if open {
                v.open_when_ready = None;
            }
            Some((row, current, save, open))
        })
        .flatten();
        let Some((row, current, save, open)) = done else { return };
        if save {
            self.save_row(&row);
        }
        if open {
            self.open_path(&path_s);
        }
        if current || row.kind == MessageKind::Image {
            self.viewer_show();
        }
    }

    pub(super) fn viewer_media_failed(&self, profile: &str, chat: &str, id: &str) {
        let current = with_viewer(|v| {
            if v.profile != profile || v.chat != chat {
                return None;
            }
            v.dl_failed.insert(id.to_string());
            if v.save_when_ready.as_deref() == Some(id) {
                v.save_when_ready = None;
            }
            if v.open_when_ready.as_deref() == Some(id) {
                v.open_when_ready = None;
            }
            Some(v.row().is_some_and(|m| m.id == id))
        })
        .flatten();
        if current == Some(true) {
            self.viewer_push_all();
        }
    }

    fn viewer_step(&self, older: bool) {
        if !self.viewer_live() {
            return;
        }
        let moved = with_viewer(|v| {
            let next = neighbor(v.idx, v.items.len(), older)?;
            v.goto(next);
            Some(())
        })
        .flatten();
        if moved.is_some() {
            self.viewer_show();
        }
    }

    fn viewer_wheel(&self, x: f32, y: f32, delta: f32) {
        if !self.viewer_live() {
            return;
        }
        with_viewer(|v| {
            if v.is_video() {
                return;
            }
            let anchor = (x - v.view.0 / 2.0, y - v.view.1 / 2.0);
            let z = v.zoom * wheel_factor(delta);
            v.zoom_to(z, anchor);
        });
        self.viewer_push_geom();
    }

    fn viewer_zoom_key(&self, zoom_in: bool) {
        if !self.viewer_live() {
            return;
        }
        with_viewer(|v| {
            if v.is_video() {
                return;
            }
            let z = if zoom_in { v.zoom * KEY_ZOOM_STEP } else { v.zoom / KEY_ZOOM_STEP };
            v.zoom_to(z, (0.0, 0.0));
        });
        self.viewer_push_geom();
    }

    fn viewer_fit(&self) {
        with_viewer(|v| {
            v.zoom = 1.0;
            v.pan = (0.0, 0.0);
        });
        self.viewer_push_geom();
    }

    fn viewer_toggle_zoom(&self, x: f32, y: f32) {
        if !self.viewer_live() {
            return;
        }
        with_viewer(|v| {
            if v.is_video() {
                return;
            }
            let anchor = (x - v.view.0 / 2.0, y - v.view.1 / 2.0);
            let z = double_click_zoom(v.zoom, v.fit());
            v.zoom_to(z, anchor);
        });
        self.viewer_push_geom();
    }

    fn viewer_pan_begin(&self) {
        with_viewer(|v| v.pan_base = v.pan);
    }

    fn viewer_pan_by(&self, dx: f32, dy: f32) {
        with_viewer(|v| v.pan = (v.pan_base.0 + dx, v.pan_base.1 + dy));
        self.viewer_push_geom();
    }

    fn viewer_resized(&self, w: f32, h: f32) {
        with_viewer(|v| v.view = (w, h));
        self.viewer_push_geom();
    }

    fn viewer_rotate(&self) {
        if !self.viewer_live() {
            return;
        }
        with_viewer(|v| {
            if v.is_video() {
                return;
            }
            let q = (v.quarters + 1) % 4;
            v.reset_view();
            v.quarters = q;
        });
        self.viewer_push_geom();
    }

    fn viewer_current(&self) -> Option<MessageRow> {
        if !self.viewer_live() {
            return None;
        }
        with_viewer(|v| v.row().cloned()).flatten()
    }

    fn viewer_save(&self) {
        let Some(m) = self.viewer_current() else { return };
        if m.local_path.is_some() {
            self.save_row(&m);
            return;
        }
        with_viewer(|v| v.save_when_ready = Some(m.id.clone()));
        self.show_toast("Downloading…");
        self.download(&m.id);
    }

    fn viewer_forward(&self) {
        let Some(m) = self.viewer_current() else { return };
        if let Some(win) = self.win() {
            win.global::<MsgOps>().invoke_forward_one(m.id.as_str().into());
        }
    }

    fn viewer_show_in_chat(&self) {
        let Some(m) = self.viewer_current() else { return };
        self.media_viewer_close();
        self.jump_to_message(&m.chat_jid, &m.id);
    }

    fn viewer_open_video(&self) {
        let Some(m) = self.viewer_current() else { return };
        match m.local_path {
            Some(p) => self.open_path(&p),
            None => {
                with_viewer(|v| v.open_when_ready = Some(m.id.clone()));
                self.show_toast("Downloading…");
                self.download(&m.id);
            }
        }
    }

    /// Puts the picture itself (not the rotation) on the clipboard.
    fn viewer_copy(&self) {
        let Some(m) = self.viewer_current() else { return };
        let Some(local) = m.local_path.filter(|_| m.kind == MessageKind::Image) else { return };
        let src = match validate_launch_path(&local, &self.media_roots()) {
            Ok(real) => real,
            Err(e) => {
                // The path holds the user's name and the file name: debug only.
                log::warn!("refused to copy an image: {e}");
                log::debug!("refused to copy {local}");
                self.show_toast("Could not copy the image");
                return;
            }
        };
        let spawned = std::thread::Builder::new().name("copy-image".into()).spawn(move || {
            let result = models::decode_file_scaled(&src, u32::MAX).and_then(|buf| {
                let data = arboard::ImageData {
                    width: buf.width() as usize,
                    height: buf.height() as usize,
                    bytes: std::borrow::Cow::Borrowed(buf.as_bytes()),
                };
                arboard::Clipboard::new().and_then(|mut c| c.set_image(data)).map_err(|e| e.to_string())
            });
            let queued = slint::invoke_from_event_loop(move || {
                with_app(|a| match result {
                    Ok(()) => a.show_toast("Image copied"),
                    Err(e) => {
                        log::warn!("could not copy the image: {e}");
                        a.show_toast("Could not copy the image");
                    }
                });
            });
            if let Err(e) = queued {
                log::error!("copy result not delivered: {e}");
            }
        });
        if let Err(e) = spawned {
            log::error!("could not start the copy thread: {e}");
            self.show_toast("Could not copy the image");
        }
    }
}

#[cfg(debug_assertions)]
impl App {
    /// Demo (`WACLIENT_DEMO_UI=viewer-fit|viewer-zoom|viewer-browse`): opens
    /// the viewer on the downloaded demo photo.
    pub(super) fn demo_media_viewer(&self, what: &str) {
        use std::time::Duration;
        self.select_chat("111@s.whatsapp.net");
        let what = what.to_string();
        slint::Timer::single_shot(Duration::from_millis(1200), move || {
            with_app(|a| {
                if !a.viewer_try_open("a12") {
                    log::warn!("demo: the demo photo is not in the chat");
                    return;
                }
                let what = what.clone();
                slint::Timer::single_shot(Duration::from_millis(700), move || {
                    with_app(|a| match what.as_str() {
                        "viewer-zoom" => {
                            a.viewer_rotate();
                            a.viewer_zoom_key(true);
                            a.viewer_zoom_key(true);
                            a.viewer_zoom_key(true);
                            a.viewer_pan_begin();
                            a.viewer_pan_by(-60.0, 30.0);
                        }
                        "viewer-browse" => a.viewer_step(true),
                        _ => {}
                    });
                });
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn fit_swaps_the_dimensions_for_odd_quarter_turns() {
        // A wide image in a wide window fills the width; turned, it is limited by the height.
        assert!(close(fit_scale(400.0, 200.0, 800.0, 400.0, 0, 10.0), 2.0));
        assert!(close(fit_scale(400.0, 200.0, 800.0, 400.0, 1, 10.0), 1.0));
        assert!(close(fit_scale(400.0, 200.0, 800.0, 400.0, 2, 10.0), 2.0));
        assert!(close(fit_scale(400.0, 200.0, 800.0, 400.0, 3, 10.0), 1.0));
    }

    #[test]
    fn fit_never_enlarges_past_the_cap_and_survives_empty_input() {
        assert!(close(fit_scale(100.0, 100.0, 2000.0, 2000.0, 0, MAX_UPSCALE), 2.0));
        assert!(close(fit_scale(0.0, 100.0, 800.0, 400.0, 0, 2.0), 1.0));
        assert!(close(fit_scale(100.0, 100.0, 0.0, 0.0, 0, 2.0), 1.0));
    }

    #[test]
    fn zoom_is_clamped_to_fit_and_eight_times() {
        assert!(close(clamp_zoom(0.2), 1.0));
        assert!(close(clamp_zoom(3.0), 3.0));
        assert!(close(clamp_zoom(100.0), MAX_ZOOM));
        assert!(close(clamp_zoom(f32::NAN), 1.0));
    }

    #[test]
    fn zoom_about_the_cursor_keeps_the_point_under_it() {
        // Image point under the cursor before and after, in image-centre coordinates.
        let (pan, z0, z1, cur) = ((30.0, -20.0), 2.0, 5.0, (120.0, 70.0));
        let pan1 = zoom_about(pan, z0, z1, cur);
        let before = ((cur.0 - pan.0) / z0, (cur.1 - pan.1) / z0);
        let after = ((cur.0 - pan1.0) / z1, (cur.1 - pan1.1) / z1);
        assert!(close(before.0, after.0) && close(before.1, after.1));
    }

    #[test]
    fn zooming_about_the_centre_keeps_a_centred_image_centred() {
        let pan = zoom_about((0.0, 0.0), 1.0, 4.0, (0.0, 0.0));
        assert_eq!(pan, (0.0, 0.0));
    }

    #[test]
    fn pan_is_limited_so_the_image_keeps_covering_the_view() {
        // Image 1000 wide in a 400 wide view: 300 either way; 200 tall in 400: centred.
        let p = clamp_pan((900.0, 50.0), 1000.0, 200.0, 400.0, 400.0);
        assert_eq!(p, (300.0, 0.0));
        let p = clamp_pan((-900.0, -50.0), 1000.0, 600.0, 400.0, 400.0);
        assert_eq!(p, (-300.0, -50.0));
        assert_eq!(clamp_pan((5.0, 5.0), 100.0, 100.0, 400.0, 400.0), (0.0, 0.0));
    }

    #[test]
    fn placement_centres_and_clamps() {
        let p = place(400.0, 200.0, 800.0, 400.0, 0, 1.0, (50.0, 50.0), 10.0);
        assert!(close(p.scale, 2.0));
        assert_eq!(p.pan, (0.0, 0.0));
        assert!(close(p.x, 0.0) && close(p.y, 0.0) && close(p.w, 800.0) && close(p.h, 400.0));
        // A quarter turn: the on-screen rectangle is the rotated one, fitted to the height.
        let r = place(400.0, 200.0, 800.0, 400.0, 1, 1.0, (0.0, 0.0), 10.0);
        assert!(close(r.scale, 1.0) && close(r.w, 200.0) && close(r.h, 400.0));
        assert!(close(r.x, 300.0) && close(r.y, 0.0));
        // Zoomed 4x the pan is clamped to the overhang.
        let z = place(400.0, 200.0, 800.0, 400.0, 0, 4.0, (9999.0, 0.0), 10.0);
        assert!(close(z.pan.0, (800.0 * 4.0 - 800.0) / 2.0));
    }

    #[test]
    fn wheel_zooms_in_for_positive_delta_and_is_bounded() {
        assert!(wheel_factor(60.0) > 1.0);
        assert!(wheel_factor(-60.0) < 1.0);
        assert!(close(wheel_factor(1e9), 2.0));
        assert!(close(wheel_factor(f32::NAN), 1.0));
    }

    #[test]
    fn double_click_toggles_between_fit_and_native_or_double() {
        assert!(close(double_click_zoom(3.0, 0.5), 1.0));
        // Large photo shown at 25%: native is 4x fit.
        assert!(close(double_click_zoom(1.0, 0.25), 4.0));
        // Small photo already above native: 2x.
        assert!(close(double_click_zoom(1.0, 1.5), 2.0));
        // Never beyond the zoom limit.
        assert!(close(double_click_zoom(1.0, 0.05), MAX_ZOOM));
    }

    #[test]
    fn neighbours_in_a_newest_first_list() {
        assert_eq!(neighbor(0, 3, true), Some(1));
        assert_eq!(neighbor(2, 3, true), None);
        assert_eq!(neighbor(2, 3, false), Some(1));
        assert_eq!(neighbor(0, 3, false), None);
        assert_eq!(neighbor(0, 0, true), None);
        assert_eq!(neighbor(5, 3, false), None);
    }

    #[test]
    fn position_counts_from_the_oldest() {
        assert_eq!(position(0, 87), (87, 87));
        assert_eq!(position(75, 87), (12, 87));
        assert_eq!(position(86, 87), (1, 87));
    }

    #[test]
    fn sender_prefers_you_then_the_name_then_the_chat_then_the_number() {
        let base = MessageRow {
            chat_jid: "1@s.whatsapp.net".into(),
            id: "x".into(),
            sender_jid: "1@s.whatsapp.net".into(),
            sender_name: None,
            from_me: false,
            ts: 0,
            kind: MessageKind::Image,
            text: None,
            quoted_id: None,
            quoted_preview: None,
            media_mime: None,
            media_name: None,
            media_size: None,
            media_duration: None,
            thumbnail: None,
            local_path: None,
            status: wa_store::MessageStatus::Delivered,
            revoked: false,
            edited: false,
            link_url: None,
            link_title: None,
            link_desc: None,
            link_thumb: None,
            starred: false,
            mentions_me: false,
            geo_lat: None,
            geo_lon: None,
            expires_at: None,
        };
        assert_eq!(sender_label(&base, &[]), "1");
        let named = MessageRow { sender_name: Some("Alice".into()), ..base.clone() };
        assert_eq!(sender_label(&named, &[]), "Alice");
        let mine = MessageRow { from_me: true, ..base };
        assert_eq!(sender_label(&mine, &[]), "You");
    }

    #[test]
    fn turning_pixels_swaps_the_dimensions_clockwise() {
        // 2x1: red, green.
        let buf = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&[255, 0, 0, 255, 0, 255, 0, 255], 2, 1);
        let q1 = turn_pixels(&buf, 1).expect("turned");
        assert_eq!((q1.width(), q1.height()), (1, 2));
        // Clockwise: the left pixel ends on top.
        assert_eq!(&q1.as_bytes()[..4], &[255, 0, 0, 255]);
        let q2 = turn_pixels(&buf, 2).expect("turned");
        assert_eq!((q2.width(), q2.height()), (2, 1));
        assert_eq!(&q2.as_bytes()[..4], &[0, 255, 0, 255]);
        let q4 = turn_pixels(&buf, 4).expect("same");
        assert_eq!(q4.as_bytes(), buf.as_bytes());
    }
}
