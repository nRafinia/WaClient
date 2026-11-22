//! Status list (tab above the chat list) and the full-window status viewer.
//!
//! The list always reflects the active profile. A viewer session belongs to
//! one profile and is closed when another one becomes active, so a view
//! receipt can only ever go to the active profile's engine.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use slint::{Color, ComponentHandle, Image, ModelRc, Timer, TimerMode, VecModel};
use wa_engine::{Command, ConnState, STATUS_JID};
use wa_store::StatusGroup;

use super::{App, ProfileState, with_app};
use crate::decode::DecodeJob;
use crate::models;
use crate::status_view::{
    FullState, Nav, PlayerGroup, SlideKind, SlidePhase, StatusListItem, StatusPlayer, StatusSlide,
    build_slides, build_status_list, dwell_met, ordered_groups, request_due, should_request, slide_phase,
};
use crate::{MainWindow, StatusEntry, StatusSlideItem};

/// Statuses expire after 24 hours.
const STATUS_WINDOW_SECS: i64 = 24 * 60 * 60;
const TICK: Duration = Duration::from_millis(50);
/// A stalled UI thread must not skip whole slides.
const MAX_STEP_MS: u128 = 200;
/// Longest side of a decoded status photo.
const STATUS_IMAGE_MAX_DIM: u32 = 2048;

struct SenderInfo {
    name: String,
    initials: String,
    color: Color,
}

struct Session {
    profile_id: String,
    player: StatusPlayer,
    /// Slides per group, in the player's group order.
    slides: Vec<Vec<StatusSlide>>,
    senders: Vec<SenderInfo>,
    paused: bool,
    /// Ids whose download is running.
    in_flight: HashSet<String>,
    /// Ids already asked for in this session (each is requested once).
    requested: HashSet<String>,
    /// Ids whose download failed; they wait for Retry.
    failed: HashSet<String>,
    /// Ids that were failed (or cut off) only because the profile is offline;
    /// reconnecting clears them so they are requested again.
    offline_failed: HashSet<String>,
    /// Last connection state seen by `sync_connection`.
    connected: bool,
    last_tick: Instant,
    /// When the current slide became current (delays its download).
    current_since: Instant,
    /// Since when the current slide's content has been on screen, running.
    shown_since: Option<Instant>,
    thumbs: HashMap<String, Option<Image>>,
    full: HashMap<String, Option<Image>>,
    decoding: HashSet<String>,
}

pub(super) struct StatusState {
    entries: Rc<VecModel<StatusEntry>>,
    /// What the list was last built from, so a click opens what was shown.
    groups: RefCell<Vec<StatusGroup>>,
    own: RefCell<String>,
    session: RefCell<Option<Session>>,
    timer: Timer,
}

impl StatusState {
    pub(super) fn new() -> Self {
        StatusState {
            entries: Rc::new(VecModel::default()),
            groups: RefCell::new(Vec::new()),
            own: RefCell::new(String::new()),
            session: RefCell::new(None),
            timer: Timer::default(),
        }
    }

    pub(super) fn attach(&self, win: &MainWindow) {
        win.set_status_entries(ModelRc::from(self.entries.clone()));
    }
}

/// Our own poster id: the account jid if it posted, otherwise whoever's
/// statuses are marked as sent by us (the stored jid may differ in form).
fn resolve_own(groups: &[StatusGroup], account: Option<&str>) -> String {
    if let Some(a) = account.filter(|a| groups.iter().any(|g| g.sender_jid == *a)) {
        return a.to_string();
    }
    groups
        .iter()
        .find(|g| g.items.iter().any(|m| m.from_me))
        .map(|g| g.sender_jid.clone())
        .or_else(|| account.map(str::to_string))
        .unwrap_or_default()
}

fn entry(i: &StatusListItem) -> StatusEntry {
    StatusEntry {
        name: i.name.as_str().into(),
        initials: i.initials.as_str().into(),
        color: i.color,
        count: i32::try_from(i.count).unwrap_or(i32::MAX),
        all_viewed: i.all_viewed,
        time: i.last_time_label.as_str().into(),
        preview: i.preview.as_str().into(),
    }
}

/// Own statuses first (if any), then the list order. `index` -1 is "My status".
fn build_session(
    groups: &[StatusGroup],
    own: &str,
    index: i32,
    now: DateTime<Local>,
    profile_id: &str,
) -> Option<Session> {
    let list = build_status_list(groups, own, now);
    let mine = groups.iter().find(|g| g.sender_jid == own && !g.items.is_empty());
    let mut order: Vec<(&StatusGroup, bool, &StatusListItem)> = Vec::new();
    if let (Some(g), Some(item)) = (mine, list.mine.as_ref()) {
        order.push((g, false, item));
    }
    let offset = order.len();
    for (g, item) in ordered_groups(groups, own).into_iter().zip(list.others.iter()) {
        order.push((g, true, item));
    }
    let start = match usize::try_from(index) {
        Ok(i) => offset + i,
        Err(_) if offset > 0 => 0,
        Err(_) => return None,
    };
    if start >= order.len() {
        return None;
    }
    let slides: Vec<Vec<StatusSlide>> = order.iter().map(|(g, _, _)| build_slides(g, now)).collect();
    let player_groups = order
        .iter()
        .zip(&slides)
        .map(|((g, trackable, _), s)| PlayerGroup::from_slides(&g.sender_jid, s, *trackable))
        .collect();
    let player = StatusPlayer::new(player_groups, start);
    if player.is_empty() {
        return None;
    }
    let senders = order
        .iter()
        .map(|(_, _, i)| SenderInfo { name: i.name.clone(), initials: i.initials.clone(), color: i.color })
        .collect();
    Some(Session {
        profile_id: profile_id.to_string(),
        player,
        slides,
        senders,
        paused: false,
        in_flight: HashSet::new(),
        requested: HashSet::new(),
        failed: HashSet::new(),
        offline_failed: HashSet::new(),
        connected: true,
        last_tick: Instant::now(),
        current_since: Instant::now(),
        shown_since: None,
        thumbs: HashMap::new(),
        full: HashMap::new(),
        decoding: HashSet::new(),
    })
}

/// What `Session::request_current` decided.
#[derive(Debug, PartialEq, Eq)]
enum RequestOutcome {
    Nothing,
    /// `(profile, id)` to download.
    Requested(String, String),
    /// Offline: the slide now shows Retry.
    MarkedFailed,
}

impl Session {
    fn current_slide(&self) -> Option<&StatusSlide> {
        self.slides.get(self.player.group_index()).and_then(|v| v.get(self.player.slide_index()))
    }

    fn full_state(&self, slide: &StatusSlide) -> FullState {
        match slide.local_path.as_deref().and_then(|p| self.full.get(p)) {
            None => FullState::Pending,
            Some(Some(_)) => FullState::Decoded,
            Some(None) => FullState::Failed,
        }
    }

    fn phase_of(&self, slide: &StatusSlide) -> SlidePhase {
        slide_phase(
            slide.kind,
            slide.local_path.is_some(),
            self.full_state(slide),
            self.in_flight.contains(&slide.id),
            self.failed.contains(&slide.id),
        )
    }

    /// Whether the current slide's content is on screen (not just a thumbnail).
    fn current_phase(&self) -> SlidePhase {
        self.current_slide().map_or(SlidePhase::Loading, |sl| self.phase_of(sl))
    }

    /// Asks for the current slide's media, once per id, once it has been
    /// current for a moment and a download slot is free. A slot that is not
    /// free leaves the id unrequested so a later call picks it up. Offline,
    /// the slide fails at once (Retry) instead of waiting for a reply that
    /// cannot come.
    fn request_current(&mut self, now: Instant, connected: bool) -> RequestOutcome {
        let Some(slide) = self.current_slide() else { return RequestOutcome::Nothing };
        if !should_request(slide.kind, slide.local_path.is_some(), self.requested.contains(&slide.id)) {
            return RequestOutcome::Nothing;
        }
        let id = slide.id.clone();
        if !connected {
            if !self.failed.insert(id.clone()) {
                return RequestOutcome::Nothing;
            }
            self.offline_failed.insert(id);
            return RequestOutcome::MarkedFailed;
        }
        if !request_due(self.current_since, now, self.in_flight.len()) {
            return RequestOutcome::Nothing;
        }
        self.requested.insert(id.clone());
        self.in_flight.insert(id.clone());
        RequestOutcome::Requested(self.profile_id.clone(), id)
    }

    /// Follows the profile's connection. Going offline fails the downloads
    /// that can no longer answer; coming back forgets those failures and the
    /// requests that never got a response. True when anything changed.
    fn sync_connection(&mut self, connected: bool) -> bool {
        if connected == self.connected {
            return false;
        }
        self.connected = connected;
        if connected {
            let ids: Vec<String> = self.offline_failed.drain().collect();
            for id in &ids {
                self.failed.remove(id);
                self.requested.remove(id);
            }
            !ids.is_empty()
        } else {
            let ids: Vec<String> = self.in_flight.drain().collect();
            for id in &ids {
                self.failed.insert(id.clone());
                self.offline_failed.insert(id.clone());
            }
            !ids.is_empty()
        }
    }

    /// Any navigation: the new slide starts its dwell and download delay over.
    fn on_moved(&mut self, now: Instant) {
        self.current_since = now;
        self.shown_since = None;
    }

    /// Keeps the dwell clock honest: it only runs while the content is shown
    /// and playback is running (window active, not held). While held it is
    /// pushed forward by the time that passed.
    fn update_dwell(&mut self, now: Instant, elapsed: Duration, running: bool) {
        if self.current_phase() != SlidePhase::Shown {
            self.shown_since = None;
        } else if !running {
            self.shown_since = self.shown_since.and_then(|t| t.checked_add(elapsed));
        } else if self.shown_since.is_none() {
            self.shown_since = Some(now);
        }
    }

    /// The view receipt to send for the current slide, at most once per id.
    /// Videos are only reported when the user presses Play.
    fn mark_current(&mut self) -> Option<(String, String, String)> {
        let id = self.player.on_enter_slide()?;
        let sender = self.player.current_sender()?.to_string();
        Some((self.profile_id.clone(), sender, id))
    }

    fn current_is_video(&self) -> bool {
        self.current_slide().is_some_and(|sl| sl.kind == SlideKind::Video)
    }

    /// The Retry button: forgets the failure so the slide loads again.
    fn retry_current(&mut self) {
        let Some(slide) = self.current_slide() else { return };
        let (id, path) = (slide.id.clone(), slide.local_path.clone());
        self.failed.remove(&id);
        self.offline_failed.remove(&id);
        self.requested.remove(&id);
        if let Some(path) = path {
            // On disk but undecodable: decode it again.
            self.full.remove(&path);
        }
    }

    /// Records a finished download on every slide with that id; true when it
    /// is the slide on screen.
    fn media_ready(&mut self, id: &str, path: &Path) -> bool {
        if !self.has_slide(id) {
            return false;
        }
        for slide in self.slides.iter_mut().flatten().filter(|sl| sl.id == id) {
            slide.local_path = Some(path.to_string_lossy().into_owned());
        }
        self.in_flight.remove(id);
        self.failed.remove(id);
        self.offline_failed.remove(id);
        self.player.current_id() == Some(id)
    }

    fn has_slide(&self, id: &str) -> bool {
        self.slides.iter().flatten().any(|sl| sl.id == id)
    }

    fn media_failed(&mut self, id: &str) -> bool {
        if !self.has_slide(id) {
            return false;
        }
        self.in_flight.remove(id);
        self.failed.insert(id.to_string());
        self.player.current_id() == Some(id)
    }
}

impl App {
    /// Rebuilds the status list from the active profile's store.
    pub(super) fn refresh_statuses(&self) {
        let Some(win) = self.win() else { return };
        let p = self.p();
        let now = Local::now();
        let groups = if p.login.borrow().logged_in {
            match p.store.statuses_since(now.timestamp() - STATUS_WINDOW_SECS) {
                Ok(g) => g,
                Err(e) => {
                    log::warn!("could not load statuses of {}: {e}", p.id);
                    return;
                }
            }
        } else {
            Vec::new()
        };
        let own = resolve_own(&groups, p.meta.borrow().own_jid.as_deref());
        let list = build_status_list(&groups, &own, now);
        self.status.entries.set_vec(list.others.iter().map(entry).collect::<Vec<_>>());
        win.set_status_has_mine(list.mine.is_some());
        win.set_status_mine(list.mine.as_ref().map(entry).unwrap_or_default());
        win.set_status_unviewed(list.others.iter().any(|i| !i.all_viewed));
        *self.status.groups.borrow_mut() = groups;
        *self.status.own.borrow_mut() = own;
    }

    pub(super) fn open_status(&self, index: i32) {
        let Some(win) = self.win() else { return };
        let p = self.p();
        let built = build_session(
            &self.status.groups.borrow(),
            &self.status.own.borrow(),
            index,
            Local::now(),
            &p.id,
        );
        let Some(session) = built else { return };
        *self.status.session.borrow_mut() = Some(session);
        win.set_viewer_open(true);
        self.status_show_slide();
        self.status.timer.start(TimerMode::Repeated, TICK, || with_app(App::status_tick));
    }

    pub(super) fn status_close(&self) {
        self.status.timer.stop();
        let had = self.status.session.borrow_mut().take().is_some();
        if had {
            if let Some(win) = self.win() {
                win.set_viewer_open(false);
            }
        }
    }

    pub(super) fn status_nav(&self, forward: bool) {
        let nav = {
            let mut guard = self.status.session.borrow_mut();
            let Some(s) = guard.as_mut() else { return };
            let now = Instant::now();
            s.last_tick = now;
            s.paused = false;
            s.on_moved(now);
            if forward { s.player.next() } else { s.player.prev() }
        };
        match nav {
            Nav::Moved => self.status_show_slide(),
            Nav::Finished => self.status_close(),
            Nav::Stayed => {}
        }
    }

    pub(super) fn status_pause(&self, paused: bool) {
        if let Some(s) = self.status.session.borrow_mut().as_mut() {
            s.paused = paused;
        }
    }

    /// The Retry button after a failed download or decode.
    pub(super) fn status_download(&self) {
        if let Some(s) = self.status.session.borrow_mut().as_mut() {
            s.retry_current();
        }
        self.status_show_slide();
    }

    /// Whether the session's profile is connected (false if it is gone).
    fn session_connected(&self) -> bool {
        let profile = self.status.session.borrow().as_ref().map(|s| s.profile_id.clone());
        profile
            .and_then(|id| self.find(&id))
            .is_some_and(|p| p.st.borrow().conn == ConnState::Connected)
    }

    /// Asks for the current slide's media when it is not on disk yet.
    fn status_auto_download(&self) {
        let connected = self.session_connected();
        let outcome = self.status.session.borrow_mut().as_mut().map(|s| {
            s.sync_connection(connected);
            s.request_current(Instant::now(), connected)
        });
        match outcome {
            Some(RequestOutcome::Requested(profile, id)) => {
                if let Some(p) = self.find(&profile) {
                    (p.send)(Command::DownloadMedia { chat_jid: STATUS_JID.to_string(), id: id.clone() });
                    #[cfg(debug_assertions)]
                    self.demo_status_download(&profile, &id);
                }
                self.status_show_slide();
            }
            Some(RequestOutcome::MarkedFailed) => self.status_show_slide(),
            Some(RequestOutcome::Nothing) | None => {}
        }
    }

    /// Demo only (`WACLIENT_DEMO=1`): there is no engine, so a download
    /// "finishes" after 500 ms with a photo that is already on disk.
    #[cfg(debug_assertions)]
    fn demo_status_download(&self, profile: &str, id: &str) {
        if !std::env::var("WACLIENT_DEMO").is_ok_and(|v| v == "1") {
            return;
        }
        let path = self.status.session.borrow().as_ref().and_then(|s| {
            s.slides
                .iter()
                .flatten()
                .find(|sl| sl.kind == SlideKind::Image)
                .and_then(|sl| sl.local_path.clone())
        });
        let Some(path) = path else { return };
        let (profile, id) = (profile.to_string(), id.to_string());
        slint::Timer::single_shot(Duration::from_millis(500), move || {
            with_app(|a| {
                a.on_event(
                    &profile,
                    wa_engine::UiEvent::MediaReady {
                        chat_jid: STATUS_JID.to_string(),
                        id,
                        path: PathBuf::from(path),
                    },
                );
            });
        });
    }

    /// "Open" on a downloaded video: same allowlist and folder check as chats.
    pub(super) fn status_open_media(&self) {
        let path = {
            let guard = self.status.session.borrow();
            let Some(s) = guard.as_ref() else { return };
            s.slides
                .get(s.player.group_index())
                .and_then(|v| v.get(s.player.slide_index()))
                .and_then(|slide| slide.local_path.clone())
        };
        if let Some(path) = path {
            // The video plays outside: hold the timer until the user moves on.
            self.status_pause(true);
            self.open_path(&path);
            self.status_mark_played_video();
        }
    }

    /// Pressing Play counts as viewing the video (and only that does), under
    /// the same gating as the timed receipts.
    fn status_mark_played_video(&self) {
        if !self.window_active() {
            return;
        }
        let mark = {
            let mut guard = self.status.session.borrow_mut();
            let Some(s) = guard.as_mut().filter(|s| s.current_is_video()) else { return };
            s.mark_current()
        };
        self.send_status_viewed(mark);
    }

    /// Sends a receipt, but only to the active profile's engine.
    fn send_status_viewed(&self, mark: Option<(String, String, String)>) {
        if let Some((profile, sender_jid, id)) = mark {
            if let Some(p) = self.find(&profile).filter(|p| self.is_active(p)) {
                (p.send)(Command::MarkStatusViewed { sender_jid, id });
            }
        }
    }

    pub(super) fn on_status_media_ready(&self, p: &ProfileState, id: &str, path: &Path) {
        let current = {
            let mut guard = self.status.session.borrow_mut();
            let Some(s) = guard.as_mut().filter(|s| s.profile_id == p.id) else { return };
            s.media_ready(id, path)
        };
        if current {
            self.status_show_slide();
        } else {
            // A slot freed up: the current slide may have been waiting for one.
            self.status_auto_download();
        }
    }

    pub(super) fn on_status_media_failed(&self, p: &ProfileState, id: &str, error: &str) {
        let current = {
            let mut guard = self.status.session.borrow_mut();
            let Some(s) = guard.as_mut().filter(|s| s.profile_id == p.id) else { return };
            s.media_failed(id)
        };
        log::warn!("status download of {id} failed: {error}");
        if current {
            self.status_show_slide();
        } else {
            self.status_auto_download();
        }
    }

    pub(super) fn on_status_decoded(&self, path: &str, buf: Option<slint::SharedPixelBuffer<slint::Rgba8Pixel>>) {
        let current = {
            let mut guard = self.status.session.borrow_mut();
            let Some(s) = guard.as_mut() else { return };
            if !s.decoding.remove(path) {
                return;
            }
            s.full.insert(path.to_string(), buf.map(Image::from_rgba8));
            s.slides
                .get(s.player.group_index())
                .and_then(|v| v.get(s.player.slide_index()))
                .is_some_and(|slide| slide.local_path.as_deref() == Some(path))
        };
        if current {
            self.status_show_slide();
        }
    }

    /// Pushes the current slide and its header state into the window.
    fn status_show_slide(&self) {
        let Some(win) = self.win() else { return };
        let mut guard = self.status.session.borrow_mut();
        let Some(s) = guard.as_mut() else { return };
        let g = s.player.group_index();
        let Some(slide) = s.slides.get(g).and_then(|v| v.get(s.player.slide_index())).cloned() else {
            return;
        };
        let item = self.slide_item(s, &slide);
        win.set_status_slide(item);
        win.set_status_slide_count(i32::try_from(s.player.slide_count()).unwrap_or(i32::MAX));
        win.set_status_current(i32::try_from(s.player.slide_index()).unwrap_or(0));
        win.set_status_progress(s.player.progress());
        if let Some(sender) = s.senders.get(g) {
            win.set_status_sender_name(sender.name.as_str().into());
            win.set_status_sender_initials(sender.initials.as_str().into());
            win.set_status_sender_color(sender.color);
        }
        drop(guard);
        self.status_auto_download();
    }

    fn slide_item(&self, s: &mut Session, slide: &StatusSlide) -> StatusSlideItem {
        let thumb = slide.thumbnail.as_deref().and_then(|bytes| {
            s.thumbs
                .entry(slide.id.clone())
                .or_insert_with(|| models::decode_image_bytes(bytes))
                .clone()
        });
        let full = match (slide.kind, slide.local_path.as_deref()) {
            (SlideKind::Image, Some(path)) => match s.full.get(path) {
                Some(img) => img.clone(),
                None => {
                    if s.decoding.insert(path.to_string()) {
                        let job = DecodeJob::Media { path: PathBuf::from(path), max_dim: STATUS_IMAGE_MAX_DIM };
                        if !self.pool.submit(job) {
                            s.decoding.remove(path);
                            s.full.insert(path.to_string(), None);
                        }
                    }
                    None
                }
            },
            _ => None,
        };
        let phase = s.phase_of(slide);
        StatusSlideItem {
            kind: slide.kind.as_int(),
            text: slide.text.as_str().into(),
            bg: slide.color,
            font_family: slide.font.into(),
            has_thumb: thumb.is_some(),
            thumb: thumb.unwrap_or_default(),
            has_full: full.is_some(),
            full: full.unwrap_or_default(),
            failed: phase == SlidePhase::Failed,
            downloading: phase == SlidePhase::Loading,
            has_local: slide.local_path.is_some(),
            time: slide.time_label.as_str().into(),
        }
    }

    /// Drives playback every 50 ms. Nothing advances (and nothing is reported
    /// as viewed) unless the window is the active one, so a status is only
    /// marked viewed while it is really on screen.
    fn status_tick(&self) {
        // A hidden window (tray) must not keep a viewer ticking: close it.
        if self.win().is_some_and(|w| !w.window().is_visible()) {
            self.status_close();
            return;
        }
        let active = self.window_active();
        let connected = self.session_connected();
        let changed;
        let (nav, mark) = {
            let mut guard = self.status.session.borrow_mut();
            let Some(s) = guard.as_mut() else {
                self.status.timer.stop();
                return;
            };
            changed = s.sync_connection(connected);
            let now = Instant::now();
            let elapsed = now.duration_since(s.last_tick);
            let dt = elapsed.as_millis().min(MAX_STEP_MS);
            s.last_tick = now;
            let running = active && !s.paused;
            let mut nav = None;
            if running && s.current_phase() == SlidePhase::Shown {
                nav = s.player.advance_time(u32::try_from(dt).unwrap_or(0));
            }
            let mut mark = None;
            match nav {
                Some(Nav::Moved) => s.on_moved(now),
                Some(Nav::Finished) => {}
                _ => {
                    // Only after the content has been on screen for the dwell
                    // time: skipped-through slides and thumbnails are not
                    // "viewed". Videos count when Play is pressed instead.
                    s.update_dwell(now, elapsed, running);
                    if running && !s.current_is_video() && dwell_met(s.shown_since, now) {
                        mark = s.mark_current();
                    }
                }
            }
            (nav, mark)
        };
        self.send_status_viewed(mark);
        match nav {
            Some(Nav::Finished) => self.status_close(),
            Some(Nav::Moved) => self.status_show_slide(),
            _ => {
                if changed {
                    self.status_show_slide();
                } else {
                    self.status_auto_download();
                }
                let progress = self.status.session.borrow().as_ref().map(|s| s.player.progress());
                if let (Some(win), Some(progress)) = (self.win(), progress) {
                    win.set_status_progress(progress);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status_view::MAX_IN_FLIGHT;
    use chrono::TimeZone;
    use wa_store::{MessageKind, MessageRow, MessageStatus};

    fn now() -> DateTime<Local> {
        Local.timestamp_opt(1_700_100_000, 0).single().expect("time")
    }

    fn group(jid: &str, from_me: bool, ts: &[i64], viewed: bool) -> StatusGroup {
        StatusGroup {
            sender_jid: jid.into(),
            sender_name: Some(jid.into()),
            items: ts
                .iter()
                .enumerate()
                .map(|(i, t)| MessageRow {
                    chat_jid: STATUS_JID.into(),
                    id: format!("{jid}-{i}"),
                    sender_jid: jid.into(),
                    sender_name: None,
                    from_me,
                    ts: *t,
                    kind: MessageKind::Text,
                    text: Some("hi".into()),
                    quoted_id: None,
                    quoted_preview: None,
                    media_mime: None,
                    media_name: None,
                    media_size: None,
                    media_duration: None,
                    thumbnail: None,
                    local_path: None,
                    status: MessageStatus::from_i64(0),
                    revoked: false,
                    edited: false,
                    ..Default::default()
                })
                .collect(),
            all_viewed: viewed,
        }
    }

    #[test]
    fn own_group_plays_first_and_is_not_tracked() {
        let groups = vec![
            group("a@s", false, &[100], true),
            group("me@s", true, &[50, 60], true),
            group("b@s", false, &[200], false),
        ];
        // List order: b (unviewed) then a; index 0 is therefore b.
        let mut s = build_session(&groups, "me@s", 0, now(), "p").expect("session");
        assert_eq!(s.player.current_sender(), Some("b@s"));
        assert_eq!(s.player.on_enter_slide().as_deref(), Some("b@s-0"));
        let mut mine = build_session(&groups, "me@s", -1, now(), "p").expect("mine");
        assert_eq!(mine.player.current_sender(), Some("me@s"));
        assert_eq!(mine.player.slide_count(), 2);
        assert_eq!(mine.player.on_enter_slide(), None);
        assert_eq!(mine.senders[0].name, "My status");
        assert!(build_session(&groups, "me@s", 2, now(), "p").is_none());
        assert!(build_session(&groups, "nobody", -1, now(), "p").is_none());
    }

    #[test]
    fn own_account_is_found_by_the_from_me_flag() {
        let groups = vec![group("a@s", false, &[1], true), group("me:3@s", true, &[2], true)];
        assert_eq!(resolve_own(&groups, Some("me@s")), "me:3@s");
        assert_eq!(resolve_own(&groups, Some("a@s")), "a@s");
        assert_eq!(resolve_own(&[], Some("me@s")), "me@s");
        assert_eq!(resolve_own(&[], None), "");
    }

    fn stored_status(sender: &str, id: &str, ts: i64, kind: MessageKind) -> wa_store::NewMessage {
        wa_store::NewMessage {
            chat_jid: STATUS_JID.into(),
            id: id.into(),
            sender_jid: sender.into(),
            from_me: false,
            ts,
            kind,
            text: None,
            quoted_id: None,
            quoted_preview: None,
            media_mime: None,
            media_name: None,
            media_size: None,
            media_duration: None,
            thumbnail: None,
            local_path: None,
            raw: None,
            ..Default::default()
        }
    }

    /// Statuses as the engine stores them for a real account: sender in phone
    /// form, no contact name, no caption, media not downloaded.
    #[test]
    fn stored_statuses_without_names_make_a_list_entry() {
        let store = wa_store::Store::open_in_memory().expect("store");
        let now_ts = now().timestamp();
        let poster = "96891292410@s.whatsapp.net";
        for (id, age_h, kind) in [("v1", 20, MessageKind::Video), ("v2", 19, MessageKind::Video), ("i1", 3, MessageKind::Image)] {
            store.insert_message(&stored_status(poster, id, now_ts - age_h * 3600, kind), false).expect("insert");
        }
        // Older than 24 hours: not listed.
        store.insert_message(&stored_status(poster, "old", now_ts - 25 * 3600, MessageKind::Image), false).expect("insert");
        let groups = store.statuses_since(now_ts - STATUS_WINDOW_SECS).expect("statuses");
        let own = resolve_own(&groups, Some("96877560317@s.whatsapp.net"));
        assert_eq!(own, "96877560317@s.whatsapp.net");
        let list = build_status_list(&groups, &own, now());
        assert!(list.mine.is_none());
        assert_eq!(list.others.len(), 1);
        let item = &list.others[0];
        assert_eq!(item.name, "96891292410");
        assert_eq!(item.count, 3);
        assert!(!item.all_viewed);
        assert_eq!(item.preview, "📷 Photo");
        assert!(build_session(&groups, &own, 0, now(), "p").is_some());
    }

    fn media_group(jid: &str) -> StatusGroup {
        let mut g = group(jid, false, &[10, 20], false);
        for m in &mut g.items {
            m.kind = MessageKind::Image;
            m.text = None;
        }
        g
    }

    fn later(s: &Session, ms: u64) -> Instant {
        s.current_since + Duration::from_millis(ms)
    }

    fn requested(o: RequestOutcome) -> Option<String> {
        match o {
            RequestOutcome::Requested(_, id) => Some(id),
            _ => None,
        }
    }

    #[test]
    fn entering_a_slide_requests_its_media_once_per_id() {
        let groups = vec![media_group("a@s")];
        let mut s = build_session(&groups, "me@s", 0, now(), "p").expect("session");
        let at = later(&s, 300);
        let first = s.request_current(at, true);
        assert_eq!(first, RequestOutcome::Requested("p".into(), "a@s-0".into()));
        assert_eq!(s.request_current(at, true), RequestOutcome::Nothing);
        assert_eq!(s.current_phase(), SlidePhase::Loading);
        // Moving on while the first is still downloading requests the next one.
        s.player.next();
        s.on_moved(at);
        let at = later(&s, 300);
        assert_eq!(requested(s.request_current(at, true)).as_deref(), Some("a@s-1"));
        // Coming back does not request again.
        s.player.prev();
        s.on_moved(at);
        assert_eq!(s.request_current(later(&s, 300), true), RequestOutcome::Nothing);
    }

    #[test]
    fn a_slide_is_requested_only_after_it_has_been_current_a_while() {
        let groups = vec![media_group("a@s")];
        let mut s = build_session(&groups, "me@s", 0, now(), "p").expect("session");
        assert_eq!(s.request_current(later(&s, 299), true), RequestOutcome::Nothing);
        assert!(!s.requested.contains("a@s-0"));
        assert!(requested(s.request_current(later(&s, 300), true)).is_some());
    }

    #[test]
    fn in_flight_is_capped_and_the_waiting_slide_is_requested_later() {
        let mut g = media_group("a@s");
        g.items.push(g.items[0].clone());
        g.items[2].id = "a@s-2".into();
        g.items[2].ts = 30;
        g.items.push(g.items[0].clone());
        g.items[3].id = "a@s-3".into();
        g.items[3].ts = 40;
        let mut s = build_session(&[g], "me@s", 0, now(), "p").expect("session");
        for i in 0..3 {
            let at = later(&s, 300);
            assert!(requested(s.request_current(at, true)).is_some(), "slide {i}");
            s.player.next();
            s.on_moved(at);
        }
        let at = later(&s, 300);
        assert_eq!(s.in_flight.len(), MAX_IN_FLIGHT);
        assert_eq!(s.request_current(at, true), RequestOutcome::Nothing);
        assert!(!s.requested.contains("a@s-3"), "left unrequested so it is picked up later");
        s.media_ready("a@s-0", Path::new("x/a.jpg"));
        assert_eq!(requested(s.request_current(at, true)).as_deref(), Some("a@s-3"));
    }

    #[test]
    fn offline_fails_at_once_and_reconnect_allows_a_request() {
        let groups = vec![media_group("a@s")];
        let mut s = build_session(&groups, "me@s", 0, now(), "p").expect("session");
        assert_eq!(s.request_current(later(&s, 0), false), RequestOutcome::MarkedFailed);
        assert_eq!(s.current_phase(), SlidePhase::Failed);
        assert!(!s.in_flight.contains("a@s-0"));
        assert_eq!(s.request_current(later(&s, 0), false), RequestOutcome::Nothing);
        s.connected = false;
        assert!(s.sync_connection(true));
        assert_eq!(s.current_phase(), SlidePhase::Loading);
        assert!(requested(s.request_current(later(&s, 300), true)).is_some());
    }

    #[test]
    fn losing_the_connection_fails_running_downloads_and_reconnect_retries_them() {
        let groups = vec![media_group("a@s")];
        let mut s = build_session(&groups, "me@s", 0, now(), "p").expect("session");
        assert!(requested(s.request_current(later(&s, 300), true)).is_some());
        assert!(s.sync_connection(false));
        assert_eq!(s.current_phase(), SlidePhase::Failed, "no stuck spinner");
        assert!(s.sync_connection(true));
        assert!(!s.requested.contains("a@s-0"));
        assert!(requested(s.request_current(later(&s, 300), true)).is_some());
    }

    #[test]
    fn dwell_clock_resets_on_moves_and_pauses_while_held() {
        let groups = vec![group("a@s", false, &[10, 20], false)];
        let mut s = build_session(&groups, "me@s", 0, now(), "p").expect("session");
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        s.update_dwell(t0, ms(50), true);
        assert!(!dwell_met(s.shown_since, t0 + ms(799)));
        // Held for 500 ms: the clock does not run.
        s.update_dwell(t0 + ms(500), ms(500), false);
        assert!(!dwell_met(s.shown_since, t0 + ms(1_000)));
        assert!(dwell_met(s.shown_since, t0 + ms(1_300)));
        // Moving restarts it; the new slide starts from the next tick.
        s.on_moved(t0 + ms(1_300));
        assert!(!dwell_met(s.shown_since, t0 + ms(5_000)));
        s.update_dwell(t0 + ms(1_350), ms(50), true);
        assert!(!dwell_met(s.shown_since, t0 + ms(1_400)));
        assert!(dwell_met(s.shown_since, t0 + ms(2_150)));
    }

    #[test]
    fn dwell_does_not_start_before_the_content_is_shown() {
        let groups = vec![media_group("a@s")];
        let mut s = build_session(&groups, "me@s", 0, now(), "p").expect("session");
        let t0 = Instant::now();
        s.update_dwell(t0, Duration::from_millis(50), true);
        assert_eq!(s.shown_since, None, "still loading");
        s.slides[0][0].local_path = Some("x/a.jpg".into());
        s.full.insert("x/a.jpg".into(), Some(Image::default()));
        s.update_dwell(t0, Duration::from_millis(50), true);
        assert_eq!(s.shown_since, Some(t0));
    }

    #[test]
    fn receipts_are_once_per_id_and_videos_are_known() {
        let mut g = group("a@s", false, &[10], false);
        g.items[0].kind = MessageKind::Video;
        let mut s = build_session(&[g], "me@s", 0, now(), "p").expect("session");
        assert!(s.current_is_video());
        assert_eq!(s.mark_current(), Some(("p".into(), "a@s".into(), "a@s-0".into())));
        assert_eq!(s.mark_current(), None);
    }

    #[test]
    fn unknown_ids_are_ignored() {
        let groups = vec![media_group("a@s")];
        let mut s = build_session(&groups, "me@s", 0, now(), "p").expect("session");
        assert!(!s.media_ready("zzz", Path::new("x/z.jpg")));
        assert!(!s.media_failed("zzz"));
        assert!(s.failed.is_empty() && s.in_flight.is_empty());
    }

    #[test]
    fn media_ready_updates_any_slide_not_only_the_current_one() {
        let groups = vec![media_group("a@s")];
        let mut s = build_session(&groups, "me@s", 0, now(), "p").expect("session");
        let path = Path::new("x/a.jpg");
        assert!(!s.media_ready("a@s-1", path), "slide 1 is not the one on screen");
        assert_eq!(s.slides[0][1].local_path.as_deref(), Some("x/a.jpg"));
        assert!(s.slides[0][0].local_path.is_none());
        assert!(s.media_ready("a@s-0", path));
        // Downloaded but not decoded yet: still loading, nothing is "viewed".
        assert_eq!(s.current_phase(), SlidePhase::Loading);
        s.full.insert("x/a.jpg".into(), Some(Image::default()));
        assert_eq!(s.current_phase(), SlidePhase::Shown);
    }

    #[test]
    fn failed_download_waits_for_retry() {
        let groups = vec![media_group("a@s")];
        let mut s = build_session(&groups, "me@s", 0, now(), "p").expect("session");
        let at = later(&s, 300);
        s.request_current(at, true);
        assert!(s.media_failed("a@s-0"));
        assert_eq!(s.current_phase(), SlidePhase::Failed);
        assert_eq!(s.request_current(at, true), RequestOutcome::Nothing);
        s.retry_current();
        assert!(requested(s.request_current(at, true)).is_some());
        assert_eq!(s.current_phase(), SlidePhase::Loading);
    }
}
