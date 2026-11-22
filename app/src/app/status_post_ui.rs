//! Posting a status: the text / photo / video editor, the audience chooser and
//! the engine's answer.
//!
//! The editor belongs to one profile (the one active when it opened); the
//! remembered audience is kept per profile in its settings.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use slint::{Color, ComponentHandle, Image, ModelRc, SharedPixelBuffer, Rgba8Pixel, Timer, TimerMode, VecModel};
use wa_engine::{Command, STATUS_JID, StatusAudience};
use wa_store::{ChatKind, ChatRow};

use super::{App, ProfileState, with_app};
use crate::settings::StatusAudiencePref;
use crate::{MainWindow, PickItem, StatusPost, models};

/// WhatsApp's limits for a status text and for a caption.
const MAX_TEXT: usize = 700;
const MAX_CAPTION: usize = 1024;
/// Longest side of the picture shown in the editor.
const PREVIEW_DIM: u32 = 640;
/// The engine answers within this time or the post is given up.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(45);

/// Background colours (0xAARRGGBB) of a text status; every one carries white text.
const PALETTE: [u32; 8] = [
    0xFF0A7A66, 0xFF1F6FD0, 0xFF6F42C1, 0xFFC2255C, 0xFFD9480F, 0xFFC92A2A, 0xFF495057, 0xFF2B8A3E,
];

/// The five fonts of a text status: the code the engine accepts (WhatsApp's
/// font type), the name shown on the button and the family drawn in the preview.
const FONTS: [(u8, &str, &str); 5] = [
    (0, "Sans", "Segoe UI"),
    (1, "Serif", "Georgia"),
    (2, "Script", "Segoe Script"),
    (6, "Handwriting", "Ink Free"),
    (7, "Condensed", "Bahnschrift"),
];

const IMAGE_EXTS: [&str; 4] = ["jpg", "jpeg", "png", "webp"];
const VIDEO_EXTS: [&str; 3] = ["mp4", "mov", "3gp"];

fn char_count(text: &str) -> usize {
    text.chars().count()
}

fn limit_of(mode: Mode) -> usize {
    if mode == Mode::Text { MAX_TEXT } else { MAX_CAPTION }
}

fn counter_text(count: usize, limit: usize) -> String {
    format!("{count}/{limit}")
}

fn over_limit(count: usize, limit: usize) -> bool {
    count > limit
}

/// The colour of palette entry `index` (an out-of-range index uses the first).
fn palette_argb(index: usize) -> u32 {
    PALETTE.get(index).copied().unwrap_or(PALETTE[0])
}

fn argb_color(argb: u32) -> Color {
    Color::from_argb_encoded(argb)
}

/// The next font index of the cycle.
fn next_font(font: u8) -> u8 {
    u8::try_from((usize::from(font) + 1) % FONTS.len()).unwrap_or(0)
}

/// Engine font code, button name and preview family of font index `font`.
fn font_entry(font: u8) -> (u8, &'static str, &'static str) {
    FONTS.get(usize::from(font)).copied().unwrap_or(FONTS[0])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MediaKind {
    Image,
    Video,
}

fn media_kind(path: &Path) -> Option<MediaKind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if IMAGE_EXTS.contains(&ext.as_str()) {
        Some(MediaKind::Image)
    } else if VIDEO_EXTS.contains(&ext.as_str()) {
        Some(MediaKind::Video)
    } else {
        None
    }
}

fn size_text(bytes: u64) -> String {
    let b = bytes as f64;
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", b / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{} KB", bytes / 1024)
    } else {
        format!("{bytes} B")
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum AudienceKind {
    #[default]
    MyContacts,
    Except,
    OnlyWith,
}

impl AudienceKind {
    fn from_index(i: i64) -> Self {
        match i {
            1 => AudienceKind::Except,
            2 => AudienceKind::OnlyWith,
            _ => AudienceKind::MyContacts,
        }
    }

    fn index(self) -> u8 {
        match self {
            AudienceKind::MyContacts => 0,
            AudienceKind::Except => 1,
            AudienceKind::OnlyWith => 2,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
struct Audience {
    kind: AudienceKind,
    jids: Vec<String>,
}

impl Audience {
    fn from_pref(pref: &StatusAudiencePref) -> Self {
        Audience { kind: AudienceKind::from_index(i64::from(pref.kind)), jids: unique(&pref.jids) }
    }

    fn to_pref(&self) -> StatusAudiencePref {
        StatusAudiencePref { kind: self.kind.index(), jids: self.jids.clone() }
    }

    fn toggle(&mut self, jid: &str) {
        match self.jids.iter().position(|j| j == jid) {
            Some(i) => {
                self.jids.remove(i);
            }
            None => self.jids.push(jid.to_string()),
        }
    }
}

/// Non-blank entries, first occurrence only.
fn unique(jids: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for j in jids.iter().filter(|j| !j.trim().is_empty()) {
        if !out.contains(j) {
            out.push(j.clone());
        }
    }
    out
}

/// The audience of the command. "Except" nobody is everybody; "only share
/// with" nobody is refused.
fn build_audience(a: &Audience) -> Result<StatusAudience, &'static str> {
    let jids = unique(&a.jids);
    match a.kind {
        AudienceKind::MyContacts => Ok(StatusAudience::MyContacts),
        AudienceKind::Except if jids.is_empty() => Ok(StatusAudience::MyContacts),
        AudienceKind::Except => Ok(StatusAudience::Except(jids)),
        AudienceKind::OnlyWith if jids.is_empty() => Err("Pick at least one contact to share with"),
        AudienceKind::OnlyWith => Ok(StatusAudience::OnlyShareWith(jids)),
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The summary on the audience row of the editor.
fn audience_label(a: &Audience) -> String {
    let n = unique(&a.jids).len();
    match a.kind {
        AudienceKind::MyContacts => "My contacts".to_string(),
        AudienceKind::Except if n == 0 => "My contacts".to_string(),
        AudienceKind::Except => format!("My contacts except {n}"),
        AudienceKind::OnlyWith if n == 0 => "Only share with...".to_string(),
        AudienceKind::OnlyWith => format!("Only {}", plural(n, "contact", "contacts")),
    }
}

/// The line at the bottom of the audience chooser.
fn picker_note(a: &Audience) -> String {
    let n = unique(&a.jids).len();
    match a.kind {
        AudienceKind::MyContacts => "Everyone in your contacts can see it".to_string(),
        AudienceKind::Except if n == 0 => "Pick the contacts to hide it from".to_string(),
        AudienceKind::Except => format!("Hidden from {}", plural(n, "contact", "contacts")),
        AudienceKind::OnlyWith if n == 0 => "Pick at least one contact".to_string(),
        AudienceKind::OnlyWith => format!("Shared with {}", plural(n, "contact", "contacts")),
    }
}

/// Direct chats a status can be hidden from / shared with, by name.
fn pickable_contacts(all: &[ChatRow], query: &str) -> Vec<ChatRow> {
    let q = query.trim().to_lowercase();
    let mut rows: Vec<ChatRow> = all
        .iter()
        .filter(|r| r.kind == ChatKind::Dm && r.jid != STATUS_JID)
        .filter(|r| q.is_empty() || r.display_name.to_lowercase().contains(&q))
        .cloned()
        .collect();
    rows.sort_by_key(|r| r.display_name.to_lowercase());
    rows
}

fn failure_text(message: Option<&str>) -> String {
    message.map(str::trim).filter(|m| !m.is_empty()).unwrap_or("Could not post the status").to_string()
}

/// The caption as sent: none when blank.
fn clean_caption(caption: &str) -> Option<String> {
    let c = caption.trim();
    (!c.is_empty()).then(|| c.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Mode {
    #[default]
    Text,
    Media,
}

struct Media {
    path: PathBuf,
    kind: MediaKind,
}

#[derive(Default)]
struct Draft {
    open: bool,
    /// The profile the editor was opened for.
    profile: String,
    mode: Mode,
    bg: usize,
    font: u8,
    media: Option<Media>,
    audience: Audience,
    /// The chooser's own copy, applied by Done.
    working: Audience,
    query: String,
    /// The profile whose post has been sent and not yet answered.
    awaiting: Option<String>,
}

/// What the file dialog thread hands over.
struct Picked {
    path: PathBuf,
    size: Option<u64>,
    preview: Option<SharedPixelBuffer<Rgba8Pixel>>,
}

thread_local! {
    static DRAFT: RefCell<Draft> = RefCell::new(Draft::default());
    static ANSWER_TIMER: Timer = Timer::default();
    /// Profiles whose post got no answer in time. A late "posted" answer is
    /// still reported, so the user does not post the same status twice.
    static TIMED_OUT: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
    /// A file dialog is open: the native dialog blocks on its own thread, so a
    /// second click must not stack another one.
    static PICKING: Cell<bool> = const { Cell::new(false) };
}

/// The success toast; the engine may add a note (e.g. a limited audience).
fn posted_text(message: Option<&str>) -> String {
    match message.map(str::trim).filter(|m| !m.is_empty()) {
        Some(m) => format!("Status posted. {m}"),
        None => "Status posted".to_string(),
    }
}

fn can_post(mode: Mode, text: &str, has_media: bool, awaiting: bool) -> bool {
    let ready = match mode {
        Mode::Text => !text.trim().is_empty(),
        Mode::Media => has_media,
    };
    ready && !over_limit(char_count(text), limit_of(mode)) && !awaiting
}

pub(super) fn wire(win: &MainWindow) {
    let g = win.global::<StatusPost>();
    g.on_open_text(|| with_app(App::open_text_status));
    g.on_open_media(|| with_app(App::open_media_status));
    g.on_pick_media(|| with_app(App::pick_status_media));
    g.on_text_edited(|_| with_app(App::status_post_publish));
    g.on_cycle_font(|| with_app(App::status_cycle_font));
    g.on_set_bg(|i| with_app(|a| a.status_set_bg(i)));
    g.on_audience_open(|| with_app(App::audience_open));
    g.on_picker_kind(|i| with_app(|a| a.audience_kind(i)));
    g.on_picker_search(|q| with_app(|a| a.audience_search(q.as_str())));
    g.on_picker_toggle(|jid| with_app(|a| a.audience_toggle(jid.as_str())));
    g.on_picker_done(|| with_app(App::audience_done));
    g.on_picker_cancel(|| with_app(App::audience_cancel));
    g.on_post(|| with_app(App::status_post_send));
    g.on_cancel(|| with_app(App::status_post_cancel));
}

impl App {
    /// Starts an editor for the active profile; false when it cannot be used.
    fn status_post_begin(&self, mode: Mode) -> bool {
        let Some(win) = self.win() else { return false };
        let p = self.p();
        if !p.login.borrow().logged_in {
            self.show_toast("Connect to WhatsApp to post a status");
            return false;
        }
        let audience = Audience::from_pref(&p.settings.borrow().status_audience);
        // A post still in flight keeps Post disabled in the reopened editor.
        let in_flight = DRAFT.with(|d| d.borrow().awaiting.clone()).filter(|a| *a == p.id);
        let waiting = in_flight.is_some();
        DRAFT.with(|d| {
            *d.borrow_mut() = Draft { profile: p.id.clone(), mode, audience, awaiting: in_flight, ..Draft::default() };
        });
        if !waiting {
            ANSWER_TIMER.with(Timer::stop);
        }
        let g = win.global::<StatusPost>();
        g.set_mode(if mode == Mode::Text { 0 } else { 1 });
        g.set_text(Default::default());
        g.set_caption(Default::default());
        g.set_error(Default::default());
        g.set_posting(waiting);
        g.set_picker_open(false);
        g.set_palette(ModelRc::new(VecModel::from(PALETTE.iter().map(|c| argb_color(*c)).collect::<Vec<_>>())));
        g.set_media_name(Default::default());
        g.set_media_detail(Default::default());
        g.set_media_preview(Image::default());
        g.set_has_preview(false);
        true
    }

    fn status_post_show(&self) {
        DRAFT.with(|d| d.borrow_mut().open = true);
        self.status_post_publish();
        if let Some(win) = self.win() {
            win.global::<StatusPost>().set_open(true);
        }
    }

    pub(super) fn open_text_status(&self) {
        if self.status_post_begin(Mode::Text) {
            self.status_post_show();
        }
    }

    pub(super) fn open_media_status(&self) {
        if self.status_post_begin(Mode::Media) {
            self.pick_status_media();
        }
    }

    /// The file dialog blocks, so it runs on its own thread, and so does the
    /// decoding of the picture shown in the editor.
    fn pick_status_media(&self) {
        if PICKING.with(|c| c.replace(true)) {
            return;
        }
        let spawned = std::thread::Builder::new().name("status-file".into()).spawn(|| {
            let picked = rfd::FileDialog::new()
                .set_title("Choose a photo or video for your status")
                .add_filter("Photos", &IMAGE_EXTS)
                .add_filter("Videos", &VIDEO_EXTS)
                .pick_file();
            let Some(path) = picked else {
                // Cancelled: an editor that never got a file is closed again.
                let queued = slint::invoke_from_event_loop(|| with_app(App::status_pick_cancelled));
                if let Err(e) = queued {
                    log::debug!("file dialog result not delivered: {e}");
                }
                return;
            };
            let size = std::fs::metadata(&path).ok().map(|m| m.len());
            let preview = (media_kind(&path) == Some(MediaKind::Image))
                .then(|| models::decode_file_scaled(&path, PREVIEW_DIM))
                .and_then(|r| r.map_err(|e| log::warn!("no preview for the status picture: {e}")).ok());
            let queued = slint::invoke_from_event_loop(move || {
                with_app(|a| a.status_media_picked(Picked { path, size, preview }));
            });
            if let Err(e) = queued {
                log::error!("could not deliver the picked file to the UI: {e}");
            }
        });
        if let Err(e) = spawned {
            PICKING.with(|c| c.set(false));
            self.show_banner(1, &format!("Could not open the file dialog: {e}"));
        }
    }

    fn status_pick_cancelled(&self) {
        PICKING.with(|c| c.set(false));
        let (open, has_media) = DRAFT.with(|d| {
            let d = d.borrow();
            (d.open, d.media.is_some())
        });
        if !open && !has_media {
            DRAFT.with(|d| d.borrow_mut().profile.clear());
        }
    }

    fn status_media_picked(&self, picked: Picked) {
        PICKING.with(|c| c.set(false));
        let Some(kind) = media_kind(&picked.path) else {
            self.show_toast("Choose a photo (JPG, PNG, WebP) or a video (MP4, MOV, 3GP)");
            return;
        };
        let Some(win) = self.win() else { return };
        // A pick that outlived its editor (profile switched meanwhile) is dropped.
        let usable = DRAFT.with(|d| {
            let d = d.borrow();
            d.mode == Mode::Media && d.profile == self.p().id
        });
        if !usable {
            return;
        }
        let name = picked.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let detail = [
            Some(if kind == MediaKind::Image { "Photo".to_string() } else { "Video".to_string() }),
            picked.size.map(size_text),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ");
        let g = win.global::<StatusPost>();
        g.set_media_name(name.into());
        g.set_media_detail(detail.into());
        g.set_has_preview(picked.preview.is_some());
        g.set_media_preview(picked.preview.map(Image::from_rgba8).unwrap_or_default());
        g.set_error(Default::default());
        DRAFT.with(|d| d.borrow_mut().media = Some(Media { path: picked.path, kind }));
        let opened = DRAFT.with(|d| d.borrow().open);
        if opened {
            self.status_post_publish();
        } else {
            self.status_post_show();
        }
    }

    /// Everything the editor shows that follows from the draft or the typed text.
    fn status_post_publish(&self) {
        let Some(win) = self.win() else { return };
        let g = win.global::<StatusPost>();
        let (mode, bg, font, has_media, awaiting, audience) = DRAFT.with(|d| {
            let d = d.borrow();
            (d.mode, d.bg, d.font, d.media.is_some(), d.awaiting.is_some(), d.audience.clone())
        });
        let text = if mode == Mode::Text { g.get_text() } else { g.get_caption() };
        let count = char_count(text.as_str());
        g.set_count(i32::try_from(count).unwrap_or(i32::MAX));
        g.set_counter(counter_text(count, limit_of(mode)).into());
        g.set_over_limit(over_limit(count, limit_of(mode)));
        g.set_bg_index(i32::try_from(bg).unwrap_or(0));
        let (_, font_name, family) = font_entry(font);
        g.set_font_name(font_name.into());
        g.set_font_family(family.into());
        g.set_audience_label(audience_label(&audience).into());
        g.set_can_post(can_post(mode, text.as_str(), has_media, awaiting));
        g.set_posting(awaiting);
    }

    fn status_cycle_font(&self) {
        DRAFT.with(|d| {
            let mut d = d.borrow_mut();
            d.font = next_font(d.font);
        });
        self.status_post_publish();
    }

    fn status_set_bg(&self, index: i32) {
        let Ok(i) = usize::try_from(index) else { return };
        if i < PALETTE.len() {
            DRAFT.with(|d| d.borrow_mut().bg = i);
            self.status_post_publish();
        }
    }

    pub(super) fn status_post_cancel(&self) {
        DRAFT.with(|d| d.borrow_mut().open = false);
        if let Some(win) = self.win() {
            let g = win.global::<StatusPost>();
            g.set_picker_open(false);
            g.set_open(false);
        }
    }

    fn status_post_send(&self) {
        let Some(win) = self.win() else { return };
        let g = win.global::<StatusPost>();
        let p = self.p();
        let draft = DRAFT.with(|d| {
            let d = d.borrow();
            (d.profile.clone(), d.mode, d.bg, d.font, d.audience.clone(), d.media.as_ref().map(|m| (m.path.clone(), m.kind)))
        });
        let (profile, mode, bg, font, audience, media) = draft;
        if profile != p.id {
            self.status_post_cancel();
            self.show_toast("The profile changed: open the status editor again");
            return;
        }
        let audience = match build_audience(&audience) {
            Ok(a) => a,
            Err(why) => {
                g.set_error(why.into());
                return;
            }
        };
        let command = match mode {
            Mode::Text => {
                let text = g.get_text().trim().to_string();
                if text.is_empty() || over_limit(char_count(&text), MAX_TEXT) {
                    return;
                }
                Command::PostTextStatus { text, background_argb: palette_argb(bg), font: font_entry(font).0, audience }
            }
            Mode::Media => {
                let Some((path, _)) = media else { return };
                if !path.is_file() {
                    g.set_error("That file is no longer there. Choose it again.".into());
                    return;
                }
                let caption = clean_caption(g.get_caption().as_str());
                if caption.as_deref().is_some_and(|c| over_limit(char_count(c), MAX_CAPTION)) {
                    return;
                }
                Command::PostMediaStatus { path, caption, audience }
            }
        };
        let pref = DRAFT.with(|d| d.borrow().audience.to_pref());
        self.remember_status_audience(pref);
        DRAFT.with(|d| d.borrow_mut().awaiting = Some(p.id.clone()));
        g.set_error(Default::default());
        self.status_post_publish();
        ANSWER_TIMER.with(|t| t.start(TimerMode::SingleShot, ANSWER_TIMEOUT, || with_app(App::status_post_timed_out)));
        self.send(command);
    }

    fn status_post_timed_out(&self) {
        let Some(was) = DRAFT.with(|d| d.borrow_mut().awaiting.take()) else { return };
        TIMED_OUT.with(|t| t.borrow_mut().insert(was));
        let Some(win) = self.win() else { return };
        let open = DRAFT.with(|d| d.borrow().open);
        if open {
            win.global::<StatusPost>().set_error("No answer from WhatsApp. Check your connection and try again.".into());
        }
        self.status_post_publish();
    }

    /// The engine's answer to a post.
    pub(super) fn on_status_posted(&self, p: &ProfileState, ok: bool, message: Option<String>) {
        let mine = DRAFT.with(|d| {
            let mut d = d.borrow_mut();
            if d.awaiting.as_deref() == Some(p.id.as_str()) {
                d.awaiting = None;
                true
            } else {
                false
            }
        });
        let late = TIMED_OUT.with(|t| t.borrow_mut().remove(&p.id));
        if !mine {
            if late && ok && self.is_active(p) {
                self.show_toast(&posted_text(message.as_deref()));
                self.refresh_statuses();
            }
            return;
        }
        ANSWER_TIMER.with(Timer::stop);
        let open = DRAFT.with(|d| d.borrow().open);
        let active = self.is_active(p);
        if ok {
            self.status_post_cancel();
            if active {
                self.show_toast(&posted_text(message.as_deref()));
                self.refresh_statuses();
            }
            return;
        }
        let why = failure_text(message.as_deref());
        if open && active {
            if let Some(win) = self.win() {
                win.global::<StatusPost>().set_error(why.into());
            }
            self.status_post_publish();
        } else if active {
            self.show_toast(&why);
        }
    }

    // ---- audience chooser ------------------------------------------------------------

    fn audience_open(&self) {
        let Some(win) = self.win() else { return };
        DRAFT.with(|d| {
            let mut d = d.borrow_mut();
            d.working = d.audience.clone();
            d.query.clear();
        });
        win.global::<StatusPost>().set_picker_open(true);
        self.audience_publish();
    }

    fn audience_kind(&self, index: i32) {
        DRAFT.with(|d| d.borrow_mut().working.kind = AudienceKind::from_index(i64::from(index)));
        self.audience_publish();
    }

    fn audience_search(&self, query: &str) {
        DRAFT.with(|d| d.borrow_mut().query = query.trim().to_string());
        self.audience_publish();
    }

    fn audience_toggle(&self, jid: &str) {
        DRAFT.with(|d| d.borrow_mut().working.toggle(jid));
        self.audience_publish();
    }

    pub(super) fn audience_cancel(&self) {
        if let Some(win) = self.win() {
            win.global::<StatusPost>().set_picker_open(false);
        }
    }

    fn audience_done(&self) {
        let working = DRAFT.with(|d| d.borrow().working.clone());
        if build_audience(&working).is_err() {
            return;
        }
        DRAFT.with(|d| d.borrow_mut().audience = working.clone());
        self.remember_status_audience(working.to_pref());
        self.audience_cancel();
        self.status_post_publish();
    }

    /// Rows and note of the chooser for its working copy and search text.
    fn audience_publish(&self) {
        let Some(win) = self.win() else { return };
        let (working, query) = DRAFT.with(|d| {
            let d = d.borrow();
            (d.working.clone(), d.query.clone())
        });
        let p = self.p();
        let rows = pickable_contacts(&p.st.borrow().chats, &query);
        let mut items = Vec::with_capacity(rows.len());
        {
            let mut st = p.st.borrow_mut();
            for r in &rows {
                let avatar = self.avatar(&p, &mut st, &r.jid);
                items.push(PickItem {
                    jid: r.jid.as_str().into(),
                    name: r.display_name.as_str().into(),
                    initials: models::initials(&r.display_name).into(),
                    tint: models::tint_for(&r.jid),
                    has_avatar: avatar.is_some(),
                    avatar: avatar.unwrap_or_default(),
                    selected: working.jids.contains(&r.jid),
                });
            }
        }
        let g = win.global::<StatusPost>();
        g.set_audience_kind(i32::from(working.kind.index()));
        g.set_picker_rows(ModelRc::new(VecModel::from(items)));
        g.set_picker_note(picker_note(&working).into());
        g.set_picker_can_done(build_audience(&working).is_ok());
    }

    /// Demo only: the editor with some text, optionally with the chooser open.
    #[cfg(debug_assertions)]
    pub(super) fn demo_status_post(&self, picker: bool) {
        self.open_text_status();
        let Some(win) = self.win() else { return };
        win.global::<StatusPost>().set_text("Back from the mountains: three days, no signal, zero regrets.".into());
        self.status_post_publish();
        if picker {
            self.audience_open();
            self.audience_kind(1);
            self.audience_toggle("222@s.whatsapp.net");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aud(kind: AudienceKind, jids: &[&str]) -> Audience {
        Audience { kind, jids: jids.iter().map(|j| j.to_string()).collect() }
    }

    #[test]
    fn viewer_fonts_match_the_editor_fonts() {
        for (code, _, family) in FONTS {
            assert_eq!(crate::status_view::font_family(Some(code)), family);
        }
    }

    #[test]
    fn posted_toast_carries_the_engine_note() {
        assert_eq!(posted_text(None), "Status posted");
        assert_eq!(posted_text(Some("  ")), "Status posted");
        assert_eq!(posted_text(Some("Shared with the first 5000 contacts")), "Status posted. Shared with the first 5000 contacts");
    }

    #[test]
    fn counter_counts_characters_not_bytes() {
        assert_eq!(char_count("héllo \u{1F600}"), 7);
        assert_eq!(counter_text(12, MAX_TEXT), "12/700");
        assert!(!over_limit(700, MAX_TEXT));
        assert!(over_limit(701, MAX_TEXT));
        assert!(!over_limit(1024, MAX_CAPTION));
        assert!(over_limit(1025, MAX_CAPTION));
    }

    #[test]
    fn palette_and_fonts_map_to_command_values() {
        assert_eq!(PALETTE.len(), 8);
        assert!(PALETTE.iter().all(|c| c >> 24 == 0xFF));
        assert_eq!(palette_argb(1), 0xFF1F6FD0);
        assert_eq!(palette_argb(99), PALETTE[0]);
        assert_eq!(argb_color(0xFF1F6FD0).to_argb_u8().red, 0x1F);
        assert_eq!(FONTS.len(), 5);
        assert_eq!((0..5).map(|f| font_entry(f).0).collect::<Vec<_>>(), vec![0, 1, 2, 6, 7]);
        assert_eq!(font_entry(9), FONTS[0]);
    }

    #[test]
    fn font_cycle_visits_the_five_fonts_and_wraps() {
        let mut f = 0u8;
        let mut seen = vec![f];
        for _ in 0..5 {
            f = next_font(f);
            seen.push(f);
        }
        assert_eq!(seen, vec![0, 1, 2, 3, 4, 0]);
    }

    #[test]
    fn audience_is_built_from_the_choice() {
        assert_eq!(build_audience(&Audience::default()), Ok(StatusAudience::MyContacts));
        assert_eq!(
            build_audience(&aud(AudienceKind::Except, &["a@s.whatsapp.net", "a@s.whatsapp.net", " "])),
            Ok(StatusAudience::Except(vec!["a@s.whatsapp.net".to_string()]))
        );
        assert_eq!(build_audience(&aud(AudienceKind::Except, &[])), Ok(StatusAudience::MyContacts));
        assert_eq!(
            build_audience(&aud(AudienceKind::OnlyWith, &["a@s.whatsapp.net", "b@s.whatsapp.net"])),
            Ok(StatusAudience::OnlyShareWith(vec!["a@s.whatsapp.net".to_string(), "b@s.whatsapp.net".to_string()]))
        );
        assert!(build_audience(&aud(AudienceKind::OnlyWith, &[])).is_err());
    }

    #[test]
    fn audience_survives_the_saved_preference() {
        let a = aud(AudienceKind::OnlyWith, &["a@s.whatsapp.net", "b@s.whatsapp.net"]);
        assert_eq!(Audience::from_pref(&a.to_pref()), a);
        let unknown = StatusAudiencePref { kind: 9, jids: vec!["x".into()] };
        assert_eq!(Audience::from_pref(&unknown).kind, AudienceKind::MyContacts);
    }

    #[test]
    fn toggling_adds_then_removes_a_contact() {
        let mut a = Audience::default();
        a.toggle("a");
        a.toggle("b");
        a.toggle("a");
        assert_eq!(a.jids, vec!["b".to_string()]);
    }

    #[test]
    fn labels_describe_the_audience() {
        assert_eq!(audience_label(&Audience::default()), "My contacts");
        assert_eq!(audience_label(&aud(AudienceKind::Except, &["a", "b", "c"])), "My contacts except 3");
        assert_eq!(audience_label(&aud(AudienceKind::OnlyWith, &["a"])), "Only 1 contact");
        assert_eq!(audience_label(&aud(AudienceKind::OnlyWith, &["a", "b"])), "Only 2 contacts");
        assert_eq!(picker_note(&aud(AudienceKind::OnlyWith, &[])), "Pick at least one contact");
        assert_eq!(picker_note(&aud(AudienceKind::Except, &["a", "b"])), "Hidden from 2 contacts");
    }

    fn chat(jid: &str, kind: ChatKind, name: &str) -> ChatRow {
        ChatRow {
            jid: jid.into(),
            kind,
            display_name: name.into(),
            last_ts: 0,
            unread: 0,
            pinned: false,
            archived: false,
            muted_until: 0,
            last_preview: None,
            receipts_off: false,
            ephemeral_secs: 0,
            ephemeral_ts: 0,
        }
    }

    #[test]
    fn contact_picker_lists_named_direct_chats_by_name() {
        let all = vec![
            chat("2@s.whatsapp.net", ChatKind::Dm, "bob"),
            chat("9@g.us", ChatKind::Group, "Family"),
            chat(STATUS_JID, ChatKind::Other, "Status"),
            chat("1@s.whatsapp.net", ChatKind::Dm, "Alice"),
        ];
        let names = |q: &str| pickable_contacts(&all, q).into_iter().map(|r| r.display_name).collect::<Vec<_>>();
        assert_eq!(names(""), vec!["Alice", "bob"]);
        assert_eq!(names(" BO "), vec!["bob"]);
        assert!(names("family").is_empty());
    }

    #[test]
    fn media_is_recognised_by_extension() {
        assert_eq!(media_kind(Path::new("a/b.JPG")), Some(MediaKind::Image));
        assert_eq!(media_kind(Path::new("c.webp")), Some(MediaKind::Image));
        assert_eq!(media_kind(Path::new("d.mov")), Some(MediaKind::Video));
        assert_eq!(media_kind(Path::new("d.3GP")), Some(MediaKind::Video));
        assert_eq!(media_kind(Path::new("e.pdf")), None);
        assert_eq!(media_kind(Path::new("noext")), None);
    }

    #[test]
    fn posting_needs_content_room_and_no_post_in_flight() {
        assert!(can_post(Mode::Text, "hi", false, false));
        assert!(!can_post(Mode::Text, "   ", false, false));
        assert!(!can_post(Mode::Text, &"x".repeat(701), false, false));
        assert!(!can_post(Mode::Text, "hi", false, true));
        assert!(can_post(Mode::Media, "", true, false));
        assert!(!can_post(Mode::Media, "", false, false));
        assert!(can_post(Mode::Media, &"x".repeat(1024), true, false));
        assert!(!can_post(Mode::Media, &"x".repeat(1025), true, false));
    }

    #[test]
    fn captions_and_failures_are_cleaned() {
        assert_eq!(clean_caption("  "), None);
        assert_eq!(clean_caption(" hi "), Some("hi".to_string()));
        assert_eq!(failure_text(None), "Could not post the status");
        assert_eq!(failure_text(Some("  ")), "Could not post the status");
        assert_eq!(failure_text(Some("Too large")), "Too large");
    }

    #[test]
    fn file_sizes_read_naturally() {
        assert_eq!(size_text(900), "900 B");
        assert_eq!(size_text(3 * 1024), "3 KB");
        assert_eq!(size_text(5 * 1024 * 1024 / 2), "2.5 MB");
    }
}
