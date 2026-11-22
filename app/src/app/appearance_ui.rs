//! Appearance: Light / Dark / System / Scheduled theme, accent colours and
//! chat wallpapers (one for the profile plus per-chat overrides).
//!
//! Everything is stored per profile in `settings.json` (see
//! `settings::AppearanceSettings`). Wallpaper images are copied into the
//! profile's `wallpapers` folder, scaled down and re-encoded as JPEG on a
//! worker thread. A missing image silently falls back to "no wallpaper".
//!
//! Blur is done in Rust (Slint has no live blur): once the slider settles, a
//! worker writes `<name>-b<radius>.jpg` next to the original, which stays
//! untouched so the blur can go back to 0. Until that cache exists the sharp
//! image is shown; the dialog previews the blur on a small thumbnail.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Timelike;
use image::ImageEncoder;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::{FilterType, fast_blur};
use sha2::{Digest, Sha256};
use slint::{Color, ComponentHandle, Image, ModelRc, Rgb8Pixel, SharedPixelBuffer, Timer, TimerMode, VecModel};

use super::prefs::SAVE_DELAY;
use super::{App, ProfileState, with_app};
use crate::settings::{
    ACCENT_COUNT, AppearanceSettings, WALLPAPER_BLUR_MAX, WALLPAPER_DIM_MAX, WALLPAPER_PRESETS, WallpaperPref,
};
use crate::{AppearanceState, MainWindow, Theme, WallpaperPreset};

const WALLPAPER_DIR: &str = "wallpapers";
/// Longest side of a stored wallpaper image.
const MAX_SIDE: u32 = 2560;
const JPEG_QUALITY: u8 = 85;
/// Minimum WCAG contrast between a bubble and the chat text colour.
const MIN_CONTRAST: f32 = 4.5;
/// Night mode is re-evaluated (and the OS theme re-read) this often.
const TICK: Duration = Duration::from_secs(30);
/// The blur slider has to rest this long before a blurred file is generated.
const BLUR_DEBOUNCE: Duration = Duration::from_millis(300);
/// Longest side of a blurred wallpaper file (blur hides the lost detail).
const BLUR_SIDE: u32 = 1280;
/// Longest side of the dialog's live preview thumbnail.
const PREVIEW_SIDE: u32 = 240;

const TEXT_DARK: Rgb = Rgb(0xe9, 0xed, 0xef);
const TEXT_LIGHT: Rgb = Rgb(0x11, 0x1b, 0x21);
const BLACK: Rgb = Rgb(0, 0, 0);
const WHITE: Rgb = Rgb(0xff, 0xff, 0xff);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    const fn hex(v: u32) -> Rgb {
        Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
    }

    fn color(self) -> Color {
        Color::from_rgb_u8(self.0, self.1, self.2)
    }
}

/// Accent presets, in the order of the swatches in Settings. Mid-tone on
/// purpose: buttons and badges carry the dark text colour.
const ACCENTS: [(&str, Rgb); ACCENT_COUNT as usize] = [
    ("WhatsApp green", Rgb::hex(0x00a884)),
    ("Blue", Rgb::hex(0x3d9bf0)),
    ("Purple", Rgb::hex(0xa07ae8)),
    ("Pink", Rgb::hex(0xe8649a)),
    ("Red", Rgb::hex(0xef5350)),
    ("Orange", Rgb::hex(0xf0932b)),
    ("Teal", Rgb::hex(0x1ab8c4)),
    ("Graphite", Rgb::hex(0x7d8f99)),
];

/// Wallpaper presets: a two-colour gradient for light and one for dark mode.
struct Preset {
    name: &'static str,
    light: (Rgb, Rgb),
    dark: (Rgb, Rgb),
}

const fn preset(name: &'static str, light: (u32, u32), dark: (u32, u32)) -> Preset {
    Preset {
        name,
        light: (Rgb::hex(light.0), Rgb::hex(light.1)),
        dark: (Rgb::hex(dark.0), Rgb::hex(dark.1)),
    }
}

const PRESETS: [Preset; WALLPAPER_PRESETS as usize] = [
    preset("Sand", (0xefe6d5, 0xe3d5bb), (0x1e1a14, 0x14110d)),
    preset("Mint", (0xd8efe3, 0xbfe3d0), (0x10231c, 0x0b1612)),
    preset("Sky", (0xd6e8f7, 0xbcd7ef), (0x0f1c2b, 0x0a121b)),
    preset("Lavender", (0xe6dff5, 0xd2c6ee), (0x1a1530, 0x110e20)),
    preset("Rose", (0xf6dde6, 0xeec3d3), (0x2b141e, 0x1c0d14)),
    preset("Peach", (0xfbe3d0, 0xf6c9ab), (0x2b1b10, 0x1d1209)),
    preset("Slate", (0xdfe4e8, 0xdfe4e8), (0x1b2328, 0x1b2328)),
    preset("Aurora", (0xd3f0e8, 0xd6d3f5), (0x0e2a2a, 0x1b1636)),
];

// ---- pure helpers ----

/// Whether `now` (minutes after midnight) is inside the night range
/// `from..to`. The range may wrap past midnight (22:00-07:00); equal ends
/// mean "never".
pub fn night_active(from: u16, to: u16, now: u16) -> bool {
    if from == to {
        false
    } else if from < to {
        now >= from && now < to
    } else {
        now >= from || now < to
    }
}

/// Dark or light for a `theme_mode` (0 system, 1 light, 2 dark, 3 scheduled).
pub fn effective_dark(mode: u8, system_dark: bool, from: u16, to: u16, now: u16) -> bool {
    match mode {
        0 => system_dark,
        1 => false,
        3 => night_active(from, to, now),
        _ => true,
    }
}

fn channel(c: u8) -> f32 {
    let v = f32::from(c) / 255.0;
    if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
}

/// WCAG relative luminance.
fn luminance(c: Rgb) -> f32 {
    0.2126 * channel(c.0) + 0.7152 * channel(c.1) + 0.0722 * channel(c.2)
}

/// WCAG contrast ratio (1 to 21).
pub fn contrast(a: Rgb, b: Rgb) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// `a` moved `t` (0 to 1) of the way towards `b`.
fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let m = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round().clamp(0.0, 255.0) as u8;
    Rgb(m(a.0, b.0), m(a.1, b.1), m(a.2, b.2))
}

/// Colours derived from the accent for one mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Derived {
    pub my_bubble: Rgb,
    /// Quoted block inside an outgoing bubble.
    pub my_quote: Rgb,
}

/// Outgoing bubble tint: the accent darkened (dark mode) or lightened (light
/// mode) until the chat text on top of it reaches [`MIN_CONTRAST`].
pub fn derive(accent: Rgb, dark: bool) -> Derived {
    let (toward, text, start, step) = if dark { (BLACK, TEXT_DARK, 0.45, 0.05) } else { (WHITE, TEXT_LIGHT, 0.82, 0.02) };
    let mut t: f32 = start;
    let mut bubble = mix(accent, toward, t);
    while contrast(bubble, text) < MIN_CONTRAST && t < 1.0 {
        t = (t + step).min(1.0);
        bubble = mix(accent, toward, t);
    }
    let my_quote = if dark { mix(bubble, BLACK, 0.2) } else { mix(bubble, BLACK, 0.06) };
    Derived { my_bubble: bubble, my_quote }
}

/// Size that fits `w` x `h` inside `max` x `max`, same aspect ratio, never
/// enlarged and never zero.
pub fn fit_within(w: u32, h: u32, max: u32) -> (u32, u32) {
    let longest = w.max(h);
    if longest <= max || longest == 0 {
        return (w.max(1), h.max(1));
    }
    let scale = f64::from(max) / f64::from(longest);
    let s = |v: u32| ((f64::from(v) * scale).round() as u32).clamp(1, max);
    (s(w), s(h))
}

/// Gaussian sigma (in pixels of a [`BLUR_SIDE`] image) for a blur radius 0..=20.
pub fn blur_sigma(radius: u8) -> f32 {
    f32::from(radius.min(WALLPAPER_BLUR_MAX)) * 1.2
}

/// Name of the blurred copy of `file`; None for radius 0 (no copy) or a bad name.
pub fn blur_cache_name(file: &str, radius: u8) -> Option<String> {
    let stem = Path::new(file).file_stem()?.to_str()?;
    (radius > 0).then(|| format!("{stem}-b{radius}.jpg"))
}

/// The radius when `name` is a blurred copy of `file`.
fn parse_blur_name(name: &str, file: &str) -> Option<u8> {
    let stem = Path::new(file).file_stem()?.to_str()?;
    let rest = name.strip_prefix(stem)?.strip_prefix("-b")?.strip_suffix(".jpg")?;
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    rest.parse().ok()
}

/// Names (out of `names`) that are blurred copies of `file` with a radius
/// other than `keep`. Never matches the original or other files.
fn stale_blur_names(names: &[String], file: &str, keep: Option<u8>) -> Vec<String> {
    names.iter().filter(|n| parse_blur_name(n, file).is_some_and(|r| Some(r) != keep)).cloned().collect()
}

/// Whether the settled blur request should start a worker: the radius is
/// still the wanted one, nothing is cached for it and no worker is busy.
pub fn should_generate(requested: u8, current: u8, cached: bool, in_flight: bool) -> bool {
    requested > 0 && requested == current && !cached && !in_flight
}

fn blur_rgb(img: &image::RgbImage, sigma: f32) -> image::RgbImage {
    if sigma < 0.3 { img.clone() } else { fast_blur(img, sigma) }
}

fn format_minutes(m: u16) -> String {
    format!("{:02}:{:02}", m / 60, m % 60)
}

fn now_minutes() -> u16 {
    let now = chrono::Local::now();
    u16::try_from(now.hour() * 60 + now.minute()).unwrap_or(0)
}

/// The Windows "apps" theme; dark when it cannot be read (the app's default look).
#[cfg(windows)]
fn system_dark() -> bool {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
        .and_then(|k| k.get_value::<u32, _>("AppsUseLightTheme"))
        .map_or(true, |v| v == 0)
}

#[cfg(not(windows))]
fn system_dark() -> bool {
    true
}

/// File name for a new wallpaper image of `scope`; unique per import so the
/// image cache never serves a replaced file.
fn new_file_name(scope: &Scope) -> String {
    let stamp = chrono::Utc::now().timestamp_millis();
    match scope {
        Scope::Global => format!("global-{stamp}.jpg"),
        Scope::Chat(jid) => {
            let digest = Sha256::digest(jid.as_bytes());
            let tag: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
            format!("chat-{tag}-{stamp}.jpg")
        }
    }
}

/// Reads `src`, scales it to at most [`MAX_SIDE`] and writes it as JPEG to
/// `dir/name` (through a temp file). Runs on a worker thread.
fn import_image(src: &Path, dir: &Path, name: &str) -> Result<(), String> {
    let img = image::open(src).map_err(|e| format!("Could not read that image: {e}"))?;
    let (w, h) = fit_within(img.width(), img.height(), MAX_SIDE);
    let rgb = if (w, h) == (img.width(), img.height()) {
        img.to_rgb8()
    } else {
        img.resize_exact(w, h, FilterType::Triangle).to_rgb8()
    };
    write_jpeg(&rgb, dir, name)
}

/// Writes `rgb` as JPEG to `dir/name` through a temp file.
fn write_jpeg(rgb: &image::RgbImage, dir: &Path, name: &str) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("Could not create the wallpaper folder: {e}"))?;
    let tmp = dir.join(format!("{name}.tmp"));
    let written = (|| -> Result<(), String> {
        let file = fs::File::create(&tmp).map_err(|e| e.to_string())?;
        JpegEncoder::new_with_quality(BufWriter::new(file), JPEG_QUALITY)
            .write_image(rgb.as_raw(), rgb.width(), rgb.height(), image::ExtendedColorType::Rgb8)
            .map_err(|e| e.to_string())
    })();
    if let Err(e) = written {
        // Best effort: a leftover temp file is harmless.
        let _ = fs::remove_file(&tmp);
        return Err(format!("Could not save the wallpaper: {e}"));
    }
    fs::rename(&tmp, dir.join(name)).map_err(|e| format!("Could not save the wallpaper: {e}"))
}

/// Writes the blurred copy of `dir/file` for `radius`. Runs on a worker thread.
fn generate_blur(dir: &Path, file: &str, radius: u8) -> Result<(), String> {
    let name = blur_cache_name(file, radius).ok_or("nothing to blur")?;
    let src = Path::new(file).file_name().ok_or("bad wallpaper name")?;
    let img = image::open(dir.join(src)).map_err(|e| format!("Could not read the wallpaper: {e}"))?;
    let (w, h) = fit_within(img.width(), img.height(), BLUR_SIDE);
    let small = if (w, h) == (img.width(), img.height()) {
        img.to_rgb8()
    } else {
        img.resize_exact(w, h, FilterType::Triangle).to_rgb8()
    };
    write_jpeg(&blur_rgb(&small, blur_sigma(radius)), dir, &name)
}

/// Deletes blurred copies of `file` in `dir` except radius `keep`. Only
/// names matching `<stem>-b<N>.jpg` inside the wallpapers folder are touched.
/// Returns the deleted paths.
fn delete_stale_blurs(dir: &Path, file: &str, keep: Option<u8>) -> Vec<PathBuf> {
    let Ok(rd) = fs::read_dir(dir) else { return Vec::new() };
    let names: Vec<String> = rd.filter_map(|e| e.ok()?.file_name().into_string().ok()).collect();
    let mut gone = Vec::new();
    for name in stale_blur_names(&names, file, keep) {
        let path = dir.join(&name);
        match fs::remove_file(&path) {
            Ok(()) => gone.push(path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log::warn!("could not delete an old blurred wallpaper: {e}"),
        }
    }
    gone
}

fn slint_image(rgb: &image::RgbImage) -> Image {
    Image::from_rgb8(SharedPixelBuffer::<Rgb8Pixel>::clone_from_slice(rgb.as_raw(), rgb.width(), rgb.height()))
}

/// A blurred copy being waited for or made.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BlurJob {
    profile: String,
    file: String,
    radius: u8,
}

/// Blur radius of the preference (global or per chat) that uses `file`.
fn current_blur(a: &AppearanceSettings, file: &str) -> Option<u8> {
    std::iter::once(&a.wallpaper)
        .chain(a.chat_wallpapers.values())
        .find(|w| w.kind == 2 && w.file == file)
        .map(|w| w.blur)
}

// ---- state ----

/// Whose wallpaper the dialog edits.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Scope {
    Global,
    Chat(String),
}

thread_local! {
    static TICKER: Timer = Timer::default();
    /// Profile id and scope of the open wallpaper dialog.
    static DIALOG: RefCell<Option<(String, Scope)>> = const { RefCell::new(None) };
    static IMAGES: RefCell<HashMap<PathBuf, Image>> = RefCell::new(HashMap::new());
    static BLUR_TIMER: Timer = Timer::default();
    /// Waiting for the slider to settle.
    static BLUR_PENDING: RefCell<Option<BlurJob>> = const { RefCell::new(None) };
    /// On a worker thread right now.
    static BLUR_FLIGHT: RefCell<Option<BlurJob>> = const { RefCell::new(None) };
    /// Thumbnail of the dialog's image (path, thumbnail, longest side of the stored file).
    static THUMB: RefCell<Option<(PathBuf, image::RgbImage, u32)>> = const { RefCell::new(None) };
}

/// The wallpaper layer of a chat view (`AppearanceState.wp-*`).
pub(super) struct WallpaperLook {
    kind: i32,
    c1: Color,
    c2: Color,
    image: Image,
    dim: f32,
}

impl WallpaperLook {
    pub(super) fn apply(self, g: &AppearanceState<'_>) {
        g.set_wp_kind(self.kind);
        g.set_wp_c1(self.c1);
        g.set_wp_c2(self.c2);
        g.set_wp_image(self.image);
        g.set_wp_dim(self.dim);
    }
}

pub(super) fn wire(win: &MainWindow) {
    let g = win.global::<AppearanceState>();
    let accents: Vec<Color> = ACCENTS.iter().map(|a| a.1.color()).collect();
    g.set_accents(ModelRc::new(VecModel::from(accents)));
    g.on_set_theme_mode(|m| with_app(|a| a.appearance_edit(|s| s.theme_mode = u8::try_from(m).unwrap_or(2))));
    g.on_set_accent(|i| with_app(|a| a.appearance_edit(|s| s.accent = u8::try_from(i).unwrap_or(0))));
    g.on_night_step(|which, delta| {
        with_app(|a| {
            a.appearance_edit(|s| {
                let slot = if which == 0 { &mut s.night_from } else { &mut s.night_to };
                let next = (i32::from(*slot) + delta).rem_euclid(24 * 60);
                *slot = u16::try_from(next).unwrap_or(0);
            });
        });
    });
    g.on_open_wallpaper(|chat| with_app(|a| a.wallpaper_open(chat)));
    g.on_pick_none(|| with_app(|a| a.wallpaper_edit(|w| w.kind = 0)));
    g.on_pick_preset(|i| {
        with_app(|a| {
            a.wallpaper_edit(|w| {
                w.kind = 1;
                w.preset = u8::try_from(i).unwrap_or(0);
            });
        });
    });
    g.on_set_dim(|v| with_app(|a| a.wallpaper_edit(|w| w.dim = u8::try_from(v.clamp(0, i32::from(WALLPAPER_DIM_MAX))).unwrap_or(0))));
    g.on_set_blur(|v| {
        with_app(|a| a.wallpaper_edit(|w| w.blur = u8::try_from(v.clamp(0, i32::from(WALLPAPER_BLUR_MAX))).unwrap_or(0)));
    });
    g.on_choose_image(|| with_app(App::wallpaper_choose_image));
    g.on_use_default(|| with_app(App::wallpaper_use_default));
    g.on_close_dialog(|| with_app(App::wallpaper_close));
    TICKER.with(|t| t.start(TimerMode::Repeated, TICK, || with_app(App::appearance_tick)));
}

impl App {
    /// Chat wallpaper of `chat` (or of the profile when it has no override).
    fn wallpaper_pref<'a>(a: &'a AppearanceSettings, chat: Option<&str>) -> &'a WallpaperPref {
        chat.and_then(|c| a.chat_wallpapers.get(c)).unwrap_or(&a.wallpaper)
    }

    fn wallpaper_image(&self, p: &ProfileState, pref: &WallpaperPref) -> Option<Image> {
        if pref.kind != 2 {
            return None;
        }
        // The stored name is only ever one the app generated; keep the join inside the folder anyway.
        let name = Path::new(&pref.file).file_name()?;
        let mut path = self.profile_dir(p).join(WALLPAPER_DIR).join(name);
        if !path.is_file() {
            return None;
        }
        if let Some(blurred) = blur_cache_name(&pref.file, pref.blur) {
            let cached = path.with_file_name(blurred);
            if cached.is_file() {
                path = cached;
            } else {
                self.blur_request(p, pref);
            }
        }
        IMAGES.with(|c| {
            let mut cache = c.borrow_mut();
            if let Some(img) = cache.get(&path) {
                return Some(img.clone());
            }
            let img = Image::load_from_path(&path).ok()?;
            if cache.len() > 8 {
                cache.clear();
            }
            cache.insert(path, img.clone());
            Some(img)
        })
    }

    fn forget_wallpaper_file(&self, p: &ProfileState, pref: &WallpaperPref) {
        if pref.kind != 2 {
            return;
        }
        let Some(name) = Path::new(&pref.file).file_name() else { return };
        let dir = self.profile_dir(p).join(WALLPAPER_DIR);
        let path = dir.join(name);
        IMAGES.with(|c| c.borrow_mut().remove(&path));
        if let Err(e) = fs::remove_file(&path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                log::warn!("could not delete an old wallpaper: {e}");
            }
        }
        self.drop_blurs(&dir, &pref.file, None);
    }

    fn drop_blurs(&self, dir: &Path, file: &str, keep: Option<u8>) {
        let gone = delete_stale_blurs(dir, file, keep);
        IMAGES.with(|c| {
            let mut cache = c.borrow_mut();
            for path in &gone {
                cache.remove(path);
            }
        });
    }

    /// Asks for the blurred copy of `pref`'s image; it is made once the slider rests.
    fn blur_request(&self, p: &ProfileState, pref: &WallpaperPref) {
        let job = BlurJob { profile: p.id.clone(), file: pref.file.clone(), radius: pref.blur };
        let known = BLUR_PENDING.with(|j| j.borrow().as_ref() == Some(&job)) || BLUR_FLIGHT.with(|j| j.borrow().as_ref() == Some(&job));
        if known {
            return;
        }
        BLUR_PENDING.with(|j| *j.borrow_mut() = Some(job));
        BLUR_TIMER.with(|t| t.start(TimerMode::SingleShot, BLUR_DEBOUNCE, || with_app(App::blur_fire)));
    }

    /// The slider rested: start the worker unless the wish changed or is already met.
    fn blur_fire(&self) {
        let Some(job) = BLUR_PENDING.with(|j| j.borrow_mut().take()) else { return };
        let Some(p) = self.find(&job.profile) else { return };
        let current = current_blur(&p.settings.borrow().appearance, &job.file).unwrap_or(0);
        let dir = self.profile_dir(&p).join(WALLPAPER_DIR);
        let cached = blur_cache_name(&job.file, job.radius).is_some_and(|n| dir.join(n).is_file());
        let busy = BLUR_FLIGHT.with(|j| j.borrow().is_some());
        if !should_generate(job.radius, current, cached, busy) {
            return;
        }
        BLUR_FLIGHT.with(|j| *j.borrow_mut() = Some(job.clone()));
        let spawned = std::thread::Builder::new().name("wallpaper-blur".into()).spawn({
            let job = job.clone();
            move || {
                let result = generate_blur(&dir, &job.file, job.radius);
                let queued = slint::invoke_from_event_loop(move || with_app(|a| a.blur_done(&job, result)));
                if let Err(e) = queued {
                    log::error!("could not deliver the blurred wallpaper to the UI: {e}");
                }
            }
        });
        if let Err(e) = spawned {
            BLUR_FLIGHT.with(|j| *j.borrow_mut() = None);
            log::warn!("could not start the wallpaper blur: {e}");
        }
    }

    fn blur_done(&self, job: &BlurJob, result: Result<(), String>) {
        BLUR_FLIGHT.with(|j| *j.borrow_mut() = None);
        if let Err(e) = result {
            log::warn!("wallpaper blur failed: {e}");
            return;
        }
        let Some(p) = self.find(&job.profile) else { return };
        let keep = current_blur(&p.settings.borrow().appearance, &job.file).filter(|r| *r > 0);
        self.drop_blurs(&self.profile_dir(&p).join(WALLPAPER_DIR), &job.file, keep);
        // Also shows a radius that was asked for while this one was being made.
        if p.id == self.p().id {
            self.appearance_apply(&p);
        }
        self.popouts_wallpaper_changed(&p.id);
    }

    /// The dialog's image with the blur applied to a small thumbnail (fast enough to follow the slider).
    fn wallpaper_preview(&self, p: &ProfileState, pref: &WallpaperPref) -> Option<Image> {
        if pref.kind != 2 {
            return None;
        }
        let name = Path::new(&pref.file).file_name()?;
        let path = self.profile_dir(p).join(WALLPAPER_DIR).join(name);
        THUMB.with(|t| {
            let mut t = t.borrow_mut();
            if t.as_ref().is_none_or(|(cached, ..)| *cached != path) {
                let img = image::open(&path).ok()?;
                let side = img.width().max(img.height());
                let (w, h) = fit_within(img.width(), img.height(), PREVIEW_SIDE);
                *t = Some((path.clone(), img.resize_exact(w, h, FilterType::Triangle).to_rgb8(), side));
            }
            let (_, thumb, side) = t.as_ref()?;
            // Same look as the stored copy: its sigma, scaled to the thumbnail.
            let scale = thumb.width().max(thumb.height()) as f32 / (*side).min(BLUR_SIDE) as f32;
            Some(slint_image(&blur_rgb(thumb, blur_sigma(pref.blur) * scale)))
        })
    }

    fn selected_chat(p: &ProfileState) -> Option<String> {
        p.st.try_borrow().ok().and_then(|st| st.selected.clone())
    }

    fn schedule_settings_save(&self, id: &str) {
        self.settings_dirty.borrow_mut().insert(id.to_string());
        self.settings_timer.start(TimerMode::SingleShot, SAVE_DELAY, || with_app(App::flush_settings));
    }

    /// Pushes the profile's appearance into the window: theme, accent, the
    /// wallpaper of the open chat and the state of the Settings rows / dialog.
    pub(super) fn appearance_apply(&self, p: &ProfileState) {
        let Some(win) = self.win() else { return };
        let a = p.settings.borrow().appearance.clone();
        let dark = effective_dark(a.theme_mode, system_dark(), a.night_from, a.night_to, now_minutes());
        let accent = ACCENTS[usize::from(a.accent.min(ACCENT_COUNT - 1))].1;
        let derived = derive(accent, dark);

        let theme = win.global::<Theme>();
        theme.set_dark(dark);
        theme.set_accent(accent.color());
        theme.set_badge(accent.color());
        theme.set_my_bubble(derived.my_bubble.color());
        theme.set_my_quote(derived.my_quote.color());

        let g = win.global::<AppearanceState>();
        g.set_theme_mode(i32::from(a.theme_mode));
        g.set_accent(i32::from(a.accent));
        g.set_night_from(format_minutes(a.night_from).into());
        g.set_night_to(format_minutes(a.night_to).into());
        let presets: Vec<WallpaperPreset> = PRESETS
            .iter()
            .map(|pr| {
                let (c1, c2) = if dark { pr.dark } else { pr.light };
                WallpaperPreset { c1: c1.color(), c2: c2.color() }
            })
            .collect();
        g.set_presets(ModelRc::new(VecModel::from(presets)));
        g.set_wp_summary(Self::wallpaper_summary(&a.wallpaper).into());

        // Layer behind the open chat.
        let chat = Self::selected_chat(p);
        self.wallpaper_look(p, chat.as_deref(), dark).apply(&g);

        self.wallpaper_sync_dialog(&win, p, &a);
    }

    /// The wallpaper behind `chat` (its own or the profile's) as the layer shows it.
    pub(super) fn wallpaper_look(&self, p: &ProfileState, chat: Option<&str>, dark: bool) -> WallpaperLook {
        let a = p.settings.borrow().appearance.clone();
        let pref = Self::wallpaper_pref(&a, chat);
        let image = self.wallpaper_image(p, pref);
        let (kind, c1, c2) = match pref.kind {
            1 => {
                let pr = &PRESETS[usize::from(pref.preset.min(WALLPAPER_PRESETS - 1))];
                let (c1, c2) = if dark { pr.dark } else { pr.light };
                (1, c1.color(), c2.color())
            }
            2 if image.is_some() => (2, Color::default(), Color::default()),
            _ => (0, Color::default(), Color::default()),
        };
        // Only dark mode dims an image.
        let dim = if dark { f32::from(pref.dim) / 100.0 } else { 0.0 };
        WallpaperLook { kind, c1, c2, image: image.unwrap_or_default(), dim }
    }

    /// The wallpaper preference in force for `chat`; changes when the user edits it.
    pub(super) fn wallpaper_pref_of(p: &ProfileState, chat: &str) -> WallpaperPref {
        Self::wallpaper_pref(&p.settings.borrow().appearance, Some(chat)).clone()
    }

    fn wallpaper_summary(pref: &WallpaperPref) -> String {
        match pref.kind {
            1 => PRESETS[usize::from(pref.preset.min(WALLPAPER_PRESETS - 1))].name.to_string(),
            2 => "Custom image".to_string(),
            _ => "None".to_string(),
        }
    }

    /// Shows the wallpaper of the dialog's scope in the dialog.
    fn wallpaper_sync_dialog(&self, win: &MainWindow, p: &ProfileState, a: &AppearanceSettings) {
        let g = win.global::<AppearanceState>();
        let open = DIALOG.with(|d| d.borrow().clone());
        let Some((profile, scope)) = open else {
            g.set_dialog_open(false);
            return;
        };
        if profile != p.id {
            // The profile changed behind the dialog.
            DIALOG.with(|d| *d.borrow_mut() = None);
            g.set_dialog_open(false);
            return;
        }
        let (pref, is_override) = match &scope {
            Scope::Global => (a.wallpaper.clone(), false),
            Scope::Chat(jid) => match a.chat_wallpapers.get(jid) {
                Some(w) => (w.clone(), true),
                None => (a.wallpaper.clone(), false),
            },
        };
        g.set_dialog_chat(matches!(scope, Scope::Chat(_)));
        g.set_dlg_override(is_override);
        g.set_dlg_kind(i32::from(pref.kind));
        g.set_dlg_preset(i32::from(pref.preset));
        g.set_dlg_dim(i32::from(pref.dim));
        g.set_dlg_blur(i32::from(pref.blur));
        g.set_dlg_image(self.wallpaper_image(p, &pref).unwrap_or_default());
        g.set_dlg_preview(self.wallpaper_preview(p, &pref).unwrap_or_default());
        g.set_dialog_open(true);
    }

    /// Applies an edit to the active profile's appearance and saves it.
    fn appearance_edit(&self, f: impl FnOnce(&mut AppearanceSettings)) {
        let p = self.p();
        self.appearance_edit_in(&p, f);
    }

    fn appearance_edit_in(&self, p: &ProfileState, f: impl FnOnce(&mut AppearanceSettings)) {
        {
            let mut s = p.settings.borrow_mut();
            let mut next = s.appearance.clone();
            f(&mut next);
            let next = next.clamped();
            if next == s.appearance {
                return;
            }
            s.appearance = next;
        }
        self.schedule_settings_save(&p.id);
        if p.id == self.p().id {
            self.apply_profile_prefs(p);
        }
    }

    /// Re-evaluates System / Scheduled mode; the palette only changes when the answer does.
    pub(super) fn appearance_tick(&self) {
        let Some(win) = self.win() else { return };
        let p = self.p();
        let a = p.settings.borrow().appearance.clone();
        let dark = effective_dark(a.theme_mode, system_dark(), a.night_from, a.night_to, now_minutes());
        if dark != win.global::<Theme>().get_dark() {
            self.appearance_apply(&p);
        }
    }

    /// The open chat changed: show its wallpaper.
    pub(super) fn appearance_chat_changed(&self) {
        self.appearance_apply(&self.p());
    }

    fn wallpaper_open(&self, chat: bool) {
        let p = self.p();
        let scope = if chat {
            match Self::selected_chat(&p) {
                Some(jid) => Scope::Chat(jid),
                None => return,
            }
        } else {
            Scope::Global
        };
        DIALOG.with(|d| *d.borrow_mut() = Some((p.id.clone(), scope)));
        self.appearance_apply(&p);
    }

    pub(super) fn wallpaper_close(&self) {
        DIALOG.with(|d| *d.borrow_mut() = None);
        if let Some(win) = self.win() {
            win.global::<AppearanceState>().set_dialog_open(false);
        }
    }

    /// Edits the wallpaper the dialog is about. An image that gets replaced is deleted.
    fn wallpaper_edit(&self, f: impl FnOnce(&mut WallpaperPref)) {
        let p = self.p();
        let Some((profile, scope)) = DIALOG.with(|d| d.borrow().clone()) else { return };
        if profile != p.id {
            return;
        }
        self.wallpaper_edit_scope(&p, &scope, f);
    }

    fn wallpaper_edit_scope(&self, p: &ProfileState, scope: &Scope, f: impl FnOnce(&mut WallpaperPref)) {
        let mut dropped: Option<WallpaperPref> = None;
        let mut unblurred: Option<String> = None;
        self.appearance_edit_in(p, |a| {
            let global_dim = a.wallpaper.dim;
            let slot = match scope {
                Scope::Global => &mut a.wallpaper,
                Scope::Chat(jid) => a
                    .chat_wallpapers
                    .entry(jid.clone())
                    .or_insert_with(|| WallpaperPref { dim: global_dim, ..WallpaperPref::default() }),
            };
            let before = slot.clone();
            f(slot);
            if before.kind == 2 && (slot.kind != 2 || slot.file != before.file) {
                dropped = Some(before);
            } else if slot.kind == 2 && slot.blur == 0 && before.blur != 0 {
                // Back to sharp: the copies are useless now. Other radii are cleaned up when theirs is made.
                unblurred = Some(slot.file.clone());
            }
        });
        if let Some(old) = dropped {
            self.forget_wallpaper_file(p, &old);
        } else if let Some(file) = unblurred {
            self.drop_blurs(&self.profile_dir(p).join(WALLPAPER_DIR), &file, None);
        }
    }

    fn wallpaper_use_default(&self) {
        let p = self.p();
        let Some((profile, Scope::Chat(jid))) = DIALOG.with(|d| d.borrow().clone()) else { return };
        if profile != p.id {
            return;
        }
        let mut removed: Option<WallpaperPref> = None;
        self.appearance_edit_in(&p, |a| removed = a.chat_wallpapers.remove(&jid));
        if let Some(old) = removed {
            self.forget_wallpaper_file(&p, &old);
        }
    }

    fn wallpaper_choose_image(&self) {
        let Some(win) = self.win() else { return };
        let g = win.global::<AppearanceState>();
        if g.get_dlg_busy() {
            return;
        }
        let p = self.p();
        let Some((profile, scope)) = DIALOG.with(|d| d.borrow().clone()) else { return };
        if profile != p.id {
            return;
        }
        g.set_dlg_busy(true);
        let dir = self.profile_dir(&p).join(WALLPAPER_DIR);
        let name = new_file_name(&scope);
        let spawned = std::thread::Builder::new().name("wallpaper".into()).spawn(move || {
            let picked = rfd::FileDialog::new()
                .set_title("Choose a wallpaper")
                .add_filter("Images", &["jpg", "jpeg", "png", "webp"])
                .pick_file();
            let result = picked.map(|src| import_image(&src, &dir, &name).map(|()| name));
            let queued = slint::invoke_from_event_loop(move || with_app(|a| a.wallpaper_imported(&profile, &scope, result)));
            if let Err(e) = queued {
                log::error!("could not deliver the wallpaper to the UI: {e}");
            }
        });
        if let Err(e) = spawned {
            g.set_dlg_busy(false);
            self.show_banner(1, &format!("Could not open the file picker: {e}"));
        }
    }

    /// `result`: None = the picker was cancelled.
    fn wallpaper_imported(&self, profile: &str, scope: &Scope, result: Option<Result<String, String>>) {
        if let Some(win) = self.win() {
            win.global::<AppearanceState>().set_dlg_busy(false);
        }
        // The profile may have been removed while the picker was open.
        let Some(p) = self.find(profile) else { return };
        match result {
            None => {}
            Some(Err(e)) => self.show_banner(1, &e),
            Some(Ok(file)) => {
                self.wallpaper_edit_scope(&p, scope, |w| {
                    w.kind = 2;
                    w.file = file;
                });
            }
        }
    }
}

#[cfg(debug_assertions)]
impl App {
    /// A sharp test picture in the demo profile's wallpapers folder (temporary, never the real data).
    fn demo_wallpaper_image(&self) -> Option<String> {
        let name = "global-demo.jpg";
        let img = image::RgbImage::from_fn(1600, 900, |x, y| {
            let (dx, dy) = (x as f32 - 800.0, y as f32 - 450.0);
            if dx * dx + dy * dy < 260.0 * 260.0 {
                image::Rgb([245, 220, 120])
            } else if (x / 100 + y / 100) % 2 == 0 {
                image::Rgb([40, 90 + (y / 6) as u8 % 120, 160])
            } else {
                image::Rgb([200, 70, 90 + (x / 8) as u8 % 120])
            }
        });
        match write_jpeg(&img, &self.profile_dir(&self.p()).join(WALLPAPER_DIR), name) {
            Ok(()) => Some(name.to_string()),
            Err(e) => {
                log::warn!("demo: {e}");
                None
            }
        }
    }

    /// Demo (`WACLIENT_DEMO_UI=theme-accent[-light]|wallpaper[-light]|wallpaper-dialog`).
    /// The demo keeps its settings in a temporary folder, never in the real data.
    pub(super) fn demo_appearance(&self, what: &str) {
        let Some(win) = self.win() else { return };
        let light = what.ends_with("-light");
        let image = if what.starts_with("wallpaper-blur") || what.starts_with("empty-wallpaper") {
            self.demo_wallpaper_image()
        } else {
            None
        };
        let blur = if what.starts_with("wallpaper-blur0") {
            0
        } else if what.starts_with("empty-wallpaper") {
            8
        } else {
            12
        };
        self.appearance_edit(|a| {
            a.theme_mode = if light { 1 } else { 2 };
            a.accent = if what.starts_with("theme-accent") { if light { 2 } else { 1 } } else { 0 };
            a.wallpaper = if let Some(file) = &image {
                WallpaperPref { kind: 2, preset: 0, dim: 25, file: file.clone(), blur }
            } else if what.starts_with("wallpaper") {
                WallpaperPref { kind: 1, preset: if light { 4 } else { 3 }, dim: 25, file: String::new(), blur: 0 }
            } else {
                WallpaperPref::default()
            };
        });
        if what.starts_with("theme-accent") {
            self.open_settings();
            win.set_settings_start_category(2);
            win.set_show_settings(true);
        } else if !what.starts_with("empty-wallpaper") {
            self.select_chat("111@s.whatsapp.net");
            if what.ends_with("-dialog") {
                self.wallpaper_open(false);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn night_range_within_a_day() {
        assert!(night_active(60, 120, 60));
        assert!(night_active(60, 120, 119));
        assert!(!night_active(60, 120, 120));
        assert!(!night_active(60, 120, 30));
    }

    #[test]
    fn night_range_wrapping_midnight() {
        let (from, to) = (22 * 60, 7 * 60);
        assert!(night_active(from, to, 22 * 60));
        assert!(night_active(from, to, 23 * 60 + 59));
        assert!(night_active(from, to, 0));
        assert!(night_active(from, to, 6 * 60 + 59));
        assert!(!night_active(from, to, 7 * 60));
        assert!(!night_active(from, to, 12 * 60));
        assert!(!night_active(from, to, 21 * 60 + 59));
    }

    #[test]
    fn equal_ends_mean_never_night() {
        assert!(!night_active(600, 600, 600));
        assert!(!night_active(0, 0, 0));
    }

    #[test]
    fn mode_decides_dark() {
        assert!(effective_dark(0, true, 0, 0, 0));
        assert!(!effective_dark(0, false, 0, 0, 0));
        assert!(!effective_dark(1, true, 0, 0, 0));
        assert!(effective_dark(2, false, 0, 0, 0));
        assert!(effective_dark(3, false, 22 * 60, 7 * 60, 23 * 60));
        assert!(!effective_dark(3, true, 22 * 60, 7 * 60, 12 * 60));
    }

    #[test]
    fn contrast_extremes() {
        assert!((contrast(BLACK, WHITE) - 21.0).abs() < 0.01);
        assert!((contrast(WHITE, WHITE) - 1.0).abs() < 0.001);
        assert!((contrast(BLACK, WHITE) - contrast(WHITE, BLACK)).abs() < 0.001);
    }

    #[test]
    fn mix_endpoints() {
        let a = Rgb(10, 20, 30);
        assert_eq!(mix(a, WHITE, 0.0), a);
        assert_eq!(mix(a, WHITE, 1.0), WHITE);
        assert_eq!(mix(Rgb(0, 100, 200), BLACK, 0.5), Rgb(0, 50, 100));
    }

    #[test]
    fn bubbles_are_readable_for_every_accent_in_both_modes() {
        for (name, accent) in ACCENTS {
            let dark = derive(accent, true);
            assert!(contrast(dark.my_bubble, TEXT_DARK) >= MIN_CONTRAST, "{name} dark");
            let light = derive(accent, false);
            assert!(contrast(light.my_bubble, TEXT_LIGHT) >= MIN_CONTRAST, "{name} light");
        }
    }

    #[test]
    fn extreme_accents_still_get_readable_bubbles() {
        for accent in [BLACK, WHITE, Rgb(255, 255, 0)] {
            assert!(contrast(derive(accent, true).my_bubble, TEXT_DARK) >= MIN_CONTRAST);
            assert!(contrast(derive(accent, false).my_bubble, TEXT_LIGHT) >= MIN_CONTRAST);
        }
    }

    #[test]
    fn green_bubble_matches_the_original_look() {
        let d = derive(Rgb::hex(0x00a884), true).my_bubble;
        assert!(d.0 <= 2 && (0x58..=0x60).contains(&d.1) && (0x44..=0x4e).contains(&d.2), "{d:?}");
    }

    #[test]
    fn light_bubble_is_lighter_than_the_accent() {
        let accent = Rgb::hex(0x3d9bf0);
        assert!(luminance(derive(accent, false).my_bubble) > luminance(accent));
        assert!(luminance(derive(accent, true).my_bubble) < luminance(accent));
    }

    #[test]
    fn fit_keeps_small_images_and_scales_big_ones() {
        assert_eq!(fit_within(800, 600, 2560), (800, 600));
        assert_eq!(fit_within(2560, 1000, 2560), (2560, 1000));
        assert_eq!(fit_within(5120, 2560, 2560), (2560, 1280));
        assert_eq!(fit_within(3000, 6000, 2560), (1280, 2560));
        assert_eq!(fit_within(10000, 1, 2560), (2560, 1));
        assert_eq!(fit_within(0, 0, 2560), (1, 1));
    }

    #[test]
    fn every_preset_has_light_and_dark_variants_that_differ() {
        for p in &PRESETS {
            assert_ne!(p.light, p.dark, "{}", p.name);
            assert!(luminance(p.light.0) > luminance(p.dark.0), "{}", p.name);
        }
    }

    #[test]
    fn import_scales_down_and_writes_a_jpeg() {
        let dir = std::env::temp_dir().join(format!("waclient-wallpaper-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create dir");
        let src = dir.join("src.png");
        image::RgbImage::from_pixel(3200, 800, image::Rgb([200, 30, 30])).save(&src).expect("save png");
        let out = dir.join("out");
        import_image(&src, &out, "w.jpg").expect("import");
        let made = image::open(out.join("w.jpg")).expect("open result");
        assert_eq!((made.width(), made.height()), (2560, 640));
        assert!(!out.join("w.jpg.tmp").exists());
        let bytes = fs::read(out.join("w.jpg")).expect("read");
        assert_eq!(&bytes[..2], &[0xff, 0xd8], "JPEG magic");
        assert!(import_image(&dir.join("missing.png"), &out, "x.jpg").is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn blur_names_are_next_to_the_original_and_only_for_a_blur() {
        assert_eq!(blur_cache_name("global-170.jpg", 12).as_deref(), Some("global-170-b12.jpg"));
        assert_eq!(blur_cache_name("global-170.jpg", 0), None);
        assert_eq!(blur_cache_name("", 5), None);
        assert_eq!(parse_blur_name("global-170-b12.jpg", "global-170.jpg"), Some(12));
        assert_eq!(parse_blur_name("global-170.jpg", "global-170.jpg"), None);
        assert_eq!(parse_blur_name("global-1700-b3.jpg", "global-170.jpg"), None);
        assert_eq!(parse_blur_name("global-170-bx.jpg", "global-170.jpg"), None);
        assert_eq!(parse_blur_name("global-170-b3.jpg.tmp", "global-170.jpg"), None);
    }

    #[test]
    fn stale_blurs_skip_the_original_the_kept_radius_and_other_files() {
        let names: Vec<String> =
            ["g-1.jpg", "g-1-b4.jpg", "g-1-b12.jpg", "g-2-b4.jpg", "notes.txt"].iter().map(|s| s.to_string()).collect();
        assert_eq!(stale_blur_names(&names, "g-1.jpg", Some(12)), vec!["g-1-b4.jpg"]);
        assert_eq!(stale_blur_names(&names, "g-1.jpg", None), vec!["g-1-b4.jpg", "g-1-b12.jpg"]);
    }

    #[test]
    fn blur_radius_maps_to_a_growing_sigma() {
        assert_eq!(blur_sigma(0), 0.0);
        assert!(blur_sigma(1) < blur_sigma(10) && blur_sigma(10) < blur_sigma(20));
        assert_eq!(blur_sigma(200), blur_sigma(WALLPAPER_BLUR_MAX));
    }

    #[test]
    fn blur_starts_only_when_settled_missing_and_idle() {
        assert!(should_generate(8, 8, false, false));
        assert!(!should_generate(8, 9, false, false), "slider moved on");
        assert!(!should_generate(8, 8, true, false), "already cached");
        assert!(!should_generate(8, 8, false, true), "worker busy");
        assert!(!should_generate(0, 0, false, false), "no blur");
    }

    #[test]
    fn blurring_changes_pixels_and_keeps_the_size() {
        let img = image::RgbImage::from_fn(64, 40, |x, _| if x < 32 { image::Rgb([0, 0, 0]) } else { image::Rgb([255, 255, 255]) });
        let out = blur_rgb(&img, blur_sigma(10));
        assert_eq!(out.dimensions(), img.dimensions());
        assert_ne!(out.as_raw(), img.as_raw());
        assert_eq!(blur_rgb(&img, 0.0).as_raw(), img.as_raw());
    }

    #[test]
    fn blur_cache_is_generated_next_to_the_original_and_stale_ones_deleted() {
        let dir = std::env::temp_dir().join(format!("waclient-blur-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let img = image::RgbImage::from_fn(300, 200, |x, _| if x < 150 { image::Rgb([10, 10, 10]) } else { image::Rgb([240, 240, 240]) });
        write_jpeg(&img, &dir, "w.jpg").expect("original");
        generate_blur(&dir, "w.jpg", 6).expect("blur 6");
        generate_blur(&dir, "w.jpg", 9).expect("blur 9");
        let blurred = image::open(dir.join("w-b6.jpg")).expect("open cache");
        assert_eq!((blurred.width(), blurred.height()), (300, 200));
        let gone = delete_stale_blurs(&dir, "w.jpg", Some(9));
        assert_eq!(gone, vec![dir.join("w-b6.jpg")]);
        assert!(dir.join("w.jpg").is_file() && dir.join("w-b9.jpg").is_file() && !dir.join("w-b6.jpg").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn minutes_format() {
        assert_eq!(format_minutes(22 * 60), "22:00");
        assert_eq!(format_minutes(7 * 60 + 5), "07:05");
    }

    #[test]
    fn chat_image_names_differ_per_chat_and_stay_in_the_folder() {
        let a = new_file_name(&Scope::Chat("a@s.whatsapp.net".into()));
        let b = new_file_name(&Scope::Chat("b@s.whatsapp.net".into()));
        assert_ne!(a.split('-').nth(1), b.split('-').nth(1));
        assert!(!a.contains('/') && !a.contains('\\') && !a.contains('@'));
    }
}
