//! Wiring of the contact / group info panel: what is open, a short-lived
//! cache of the engine's answers, the profile photo decode, and the panel's
//! actions.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, Timer, TimerMode, VecModel};
use wa_engine::{Command, ConnState, ContactInfo};
use wa_store::{ChatKind, ChatRow, MessageRow};

use crate::models::OpenPolicy;

use super::contact_info::{self, ContactInfoView, LinkView, MemberView};
use super::{App, State, with_app};
use crate::{
    ContactInfoModel, GalleryDoc, GalleryLink, GalleryThumb, GroupMemberItem, InfoGallery, MainWindow, models,
};

/// An answer younger than this is shown without asking the engine again.
const INFO_FRESH: Duration = Duration::from_secs(60);
/// Longest edge of the decoded profile photo.
const PHOTO_DIM: u32 = 640;
/// A panel still loading after this long shows what is known instead.
const INFO_LOAD_TIMEOUT: Duration = Duration::from_secs(8);

/// Rows loaded per page of the "Media, links and docs" view.
const GALLERY_PAGE: u32 = 60;
/// Thumbnails in the strip under the "Media, links and docs" row.
const PREVIEW_COUNT: u32 = 6;
/// A download that has not finished after this long may be started again.
const PENDING_TTL: Duration = Duration::from_secs(120);

type Thumbs = Vec<(String, SharedPixelBuffer<Rgba8Pixel>)>;

/// A download started from the gallery.
struct Pending {
    since: Instant,
    /// Connection state when it was started; a change drops the request.
    connected: bool,
}

/// Rows of the first page of all three tabs, loaded off the UI thread.
struct FirstPage {
    media: Vec<MessageRow>,
    docs: Vec<MessageRow>,
    links: Vec<MessageRow>,
    thumbs: Thumbs,
}

/// The "Media, links and docs" view of the open panel.
#[derive(Default)]
struct Gallery {
    loaded: bool,
    counts: (u32, u32, u32),
    preview: Vec<MessageRow>,
    media: Vec<MessageRow>,
    media_more: bool,
    docs: Vec<MessageRow>,
    docs_more: bool,
    links: Vec<LinkView>,
    links_limit: u32,
    links_more: bool,
    /// Decoded stored thumbnails by message id.
    thumbs: HashMap<String, Image>,
    /// The first page of the tabs is being loaded.
    loading: bool,
    /// A "load more" request is in flight, per tab.
    more_busy: [bool; 3],
    /// Downloads started from the view, `(profile, chat, message id)`; the
    /// file is opened when they finish.
    pending: HashMap<(String, String, String), Pending>,
}

struct Cached {
    view: ContactInfoView,
    phone: String,
    at: Instant,
}

#[derive(Default)]
struct InfoState {
    /// (profile id, chat jid) of the open panel.
    open: Option<(String, String)>,
    view: Option<ContactInfoView>,
    /// Raw phone number (digits), for "copy".
    phone: String,
    query: String,
    cache: HashMap<(String, String), Cached>,
    photo: Option<(PathBuf, Image)>,
    photo_loading: Option<PathBuf>,
    gallery: Gallery,
    /// Bumped whenever the panel opens or closes; answers of an older panel
    /// are dropped.
    generation: u64,
    /// Picture keys of members still to ask for, drained in batches.
    avatar_queue: VecDeque<String>,
    /// Keys whose picture changed and is being decoded again; their row is
    /// updated once the new image is ready.
    avatar_stale: HashSet<String>,
    /// Members shown in the list by jid, set when the list is built, so avatar
    /// updates need not rebuild a map of every member.
    member_rows: HashMap<String, MemberView>,
}

thread_local! {
    static INFO: RefCell<InfoState> = RefCell::default();
    static INFO_TIMEOUT: Timer = Timer::default();
    static INFO_AVATARS: Timer = Timer::default();
}

/// Members asked for at once when the list opens (the top of the list is what
/// is in view; the panel has no viewport report).
const AVATAR_FIRST: usize = 12;
/// Most pictures asked for per panel open (the first batch included).
const AVATAR_MAX: usize = 100;
/// The remaining members are asked for in batches of this size ...
const AVATAR_BATCH: usize = 10;
/// ... this far apart.
const AVATAR_BATCH_EVERY: Duration = Duration::from_secs(2);

/// The picture of `key` is on its way or already known.
fn avatar_settled(st: &State, key: &str) -> bool {
    st.avatar_requested.contains(key) || matches!(st.avatar_paths.get(key), Some(Some(_)))
}

/// Nothing to ask for: already settled, or an address that is never requested
/// (it would only occupy a batch slot).
fn avatar_skip(st: &State, key: &str) -> bool {
    ChatKind::from_jid(key) == ChatKind::Other || avatar_settled(st, key)
}

/// Whether an engine answer about `info_jid` / `info_phone` belongs to the
/// panel of `open_jid`: same user part, or (LID vs phone-number address) the
/// answer's phone digits equal the open jid's user part.
fn answer_matches(open_jid: &str, info_jid: &str, info_phone: Option<&str>) -> bool {
    let user = user_part(open_jid);
    if user == user_part(info_jid) {
        return true;
    }
    let digits: String = info_phone.unwrap_or_default().chars().filter(char::is_ascii_digit).collect();
    !digits.is_empty() && digits == user
}

/// "123:5@s.whatsapp.net" and "123@lid" -> "123"; the two address forms of one
/// person differ only in the server part and the device suffix.
fn user_part(jid: &str) -> &str {
    let user = jid.split('@').next().unwrap_or(jid);
    user.split(':').next().unwrap_or(user)
}

fn placeholder(jid: &str, row: Option<&ChatRow>) -> ContactInfoView {
    let name = row
        .map(|r| r.display_name.clone())
        .unwrap_or_else(|| user_part(jid).to_string());
    ContactInfoView {
        jid: jid.to_string(),
        initials: models::initials(&name),
        title: name.clone(),
        name,
        tint_key: jid.to_string(),
        is_group: ChatKind::from_jid(jid) == ChatKind::Group,
        loading: true,
        ..Default::default()
    }
}

fn strings_model(v: &[String]) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(v.iter().map(|s| SharedString::from(s.as_str())).collect::<Vec<_>>()))
}

impl App {
    pub(super) fn open_info(&self) {
        let p = self.p();
        let (jid, row) = {
            let st = p.st.borrow();
            match &st.selected {
                Some(j) => (j.clone(), st.sel_row.clone()),
                None => return,
            }
        };
        let key = (p.id.clone(), jid.clone());
        let fetch = INFO.with(|i| {
            let mut i = i.borrow_mut();
            i.query.clear();
            i.photo = None;
            i.photo_loading = None;
            i.avatar_queue.clear();
            i.avatar_stale.clear();
            i.gallery = Gallery { pending: std::mem::take(&mut i.gallery.pending), ..Gallery::default() };
            i.generation += 1;
            i.open = Some(key.clone());
            let cached = i.cache.get(&key).map(|c| (c.view.clone(), c.phone.clone(), c.at.elapsed() >= INFO_FRESH));
            let (view, phone, stale) = cached.unwrap_or_else(|| (placeholder(&jid, row.as_ref()), String::new(), true));
            let loading = view.loading;
            i.view = Some(view);
            i.phone = phone;
            stale || loading
        });
        self.group_admin_reset();
        self.render_info();
        self.gallery_open_panel(&jid);
        INFO_TIMEOUT.with(|t| t.start(TimerMode::SingleShot, INFO_LOAD_TIMEOUT, || with_app(App::info_load_timeout)));
        self.subscribe_presence(&jid);
        if fetch {
            self.send(Command::FetchContactInfo { jid });
        }
    }

    /// A presence report arrived: refreshes the open panel when it is about that contact.
    pub(super) fn info_presence_changed(&self, profile: &str, jid: &str) {
        let concerned = INFO.with(|i| {
            i.borrow().open.as_ref().is_some_and(|(pid, open)| pid == profile && user_part(open) == user_part(jid))
        });
        if concerned {
            self.render_info();
        }
    }

    fn info_load_timeout(&self) {
        let gave_up = INFO.with(|i| {
            let mut i = i.borrow_mut();
            let open = i.open.is_some();
            match i.view.as_mut() {
                Some(v) if open && v.loading => {
                    v.loading = false;
                    true
                }
                _ => false,
            }
        });
        if gave_up {
            self.render_info();
        }
    }

    pub(super) fn info_close(&self) {
        INFO_TIMEOUT.with(Timer::stop);
        INFO_AVATARS.with(Timer::stop);
        INFO.with(|i| {
            let mut i = i.borrow_mut();
            i.open = None;
            i.view = None;
            i.photo = None;
            i.photo_loading = None;
            i.avatar_queue.clear();
            i.avatar_stale.clear();
            i.member_rows.clear();
            i.gallery = Gallery { pending: std::mem::take(&mut i.gallery.pending), ..Gallery::default() };
            i.generation += 1;
        });
        self.group_admin_reset();
        if let Some(win) = self.win() {
            win.set_contact_info_open(false);
            win.global::<InfoGallery>().set_open(false);
        }
    }

    /// The group of the open panel, when it is a group.
    pub(super) fn info_group_jid(&self) -> Option<String> {
        INFO.with(|i| {
            let i = i.borrow();
            let (_, jid) = i.open.as_ref()?;
            i.view.as_ref().filter(|v| v.is_group).map(|_| jid.clone())
        })
    }

    /// Whether the panel of `profile` is open on `jid`.
    pub(super) fn info_is_open_for(&self, profile: &str, jid: &str) -> bool {
        INFO.with(|i| i.borrow().open.as_ref().is_some_and(|(pid, open)| pid == profile && open == jid))
    }

    /// What the viewer may do in the open group (nothing while it is unknown).
    pub(super) fn info_controls(&self) -> contact_info::GroupControls {
        INFO.with(|i| i.borrow().view.as_ref().map(|v| v.controls).unwrap_or_default())
    }

    /// `(jid, phone digits)` of every member of the open group.
    pub(super) fn info_member_keys(&self) -> Vec<(String, String)> {
        INFO.with(|i| {
            i.borrow().view.as_ref().map(|v| v.members.iter().map(|m| (m.jid.clone(), m.phone_digits.clone())).collect()).unwrap_or_default()
        })
    }

    /// The name shown for a member of the open group.
    pub(super) fn info_member_name(&self, jid: &str) -> Option<String> {
        INFO.with(|i| i.borrow().view.as_ref()?.members.iter().find(|m| m.jid == jid).map(|m| m.name.clone()))
    }

    /// The name and description of the open group, as shown.
    pub(super) fn info_group_texts(&self) -> (String, String) {
        INFO.with(|i| i.borrow().view.as_ref().map(|v| (v.name.clone(), v.description.clone())).unwrap_or_default())
    }

    /// Engine answer (it sends two per request: local data, then the network's).
    pub(super) fn on_contact_info(&self, profile: &str, active: bool, info: ContactInfo) {
        if !active {
            return;
        }
        let open = INFO.with(|i| i.borrow().open.clone());
        let Some((pid, jid)) = open else { return };
        if pid != profile || !answer_matches(&jid, &info.jid, info.phone.as_deref()) {
            return;
        }
        let view = contact_info::to_model(&info, chrono::Utc::now().timestamp());
        let phone: String = info.phone.as_deref().unwrap_or_default().chars().filter(char::is_ascii_digit).collect();
        INFO.with(|i| {
            let mut i = i.borrow_mut();
            i.cache.insert((pid, jid), Cached { view: view.clone(), phone: phone.clone(), at: Instant::now() });
            i.view = Some(view);
            i.phone = phone;
        });
        self.render_info();
        self.info_request_avatars();
    }

    /// The picture of a member: under its picture key, else under its own address.
    fn member_avatar(&self, p: &super::ProfileState, st: &mut State, m: &MemberView) -> Option<Image> {
        self.avatar(p, st, &m.avatar_key)
            .or_else(|| if m.avatar_key != m.jid { self.avatar(p, st, &m.jid) } else { None })
    }

    /// Asks for the pictures of members that have none: the top of the list now,
    /// the rest in batches until the panel closes.
    fn info_request_avatars(&self) {
        let p = self.p();
        let members = INFO.with(|i| i.borrow().view.as_ref().filter(|v| v.is_group).map(|v| v.members.clone()));
        let Some(members) = members else { return };
        let connected = p.st.borrow().conn == ConnState::Connected;
        let (mut now, mut later) = {
            let st = p.st.borrow();
            contact_info::request_plan(
                &members,
                |m| avatar_skip(&st, &m.avatar_key) || (m.avatar_key != m.jid && matches!(st.avatar_paths.get(&m.jid), Some(Some(_)))),
                AVATAR_FIRST,
                AVATAR_MAX,
            )
        };
        if !connected {
            // The engine drops requests while offline; the ticks ask once it is back.
            now.extend(later.drain(..));
            later = now.drain(..).collect();
        }
        let more = !later.is_empty();
        INFO.with(|i| i.borrow_mut().avatar_queue = later);
        self.request_avatars(&p, &now);
        if more {
            INFO_AVATARS.with(|t| {
                if !t.running() {
                    t.start(TimerMode::Repeated, AVATAR_BATCH_EVERY, || with_app(App::info_avatar_tick));
                }
            });
        }
    }

    fn info_avatar_tick(&self) {
        let p = self.p();
        if INFO.with(|i| i.borrow().open.as_ref().is_none_or(|(pid, _)| *pid != p.id)) {
            INFO_AVATARS.with(Timer::stop);
            return;
        }
        if p.st.borrow().conn != ConnState::Connected {
            return;
        }
        let batch = {
            let st = p.st.borrow();
            INFO.with(|i| contact_info::take_batch(&mut i.borrow_mut().avatar_queue, AVATAR_BATCH, |k| !avatar_skip(&st, k)))
        };
        if INFO.with(|i| i.borrow().avatar_queue.is_empty()) {
            INFO_AVATARS.with(Timer::stop);
        }
        self.request_avatars(&p, &batch);
    }

    /// A picture was stored (`only` = its key) or finished decoding (`None`):
    /// updates the member rows concerned, not the whole list.
    pub(super) fn info_member_avatars(&self, only: Option<&str>) {
        let Some(win) = self.win() else { return };
        let p = self.p();
        let ours = INFO.with(|i| {
            let i = i.borrow();
            i.open.as_ref().is_some_and(|(pid, _)| *pid == p.id) && !i.member_rows.is_empty()
        });
        if !ours {
            return;
        }
        let model = win.get_contact_members();
        let mut st = p.st.borrow_mut();
        for idx in 0..model.row_count() {
            let Some(mut row) = model.row_data(idx) else { continue };
            let Some(m) = INFO.with(|i| i.borrow().member_rows.get(row.jid.as_str()).cloned()) else { continue };
            let m = &m;
            let stale = INFO.with(|i| i.borrow().avatar_stale.contains(&m.avatar_key));
            let targeted = match only {
                Some(j) => j == m.avatar_key || j == m.jid,
                None => !row.has_avatar || stale,
            };
            if !targeted {
                continue;
            }
            let img = self.member_avatar(&p, &mut st, m);
            INFO.with(|i| {
                let stale = &mut i.borrow_mut().avatar_stale;
                if img.is_some() {
                    stale.remove(&m.avatar_key);
                } else if only.is_some() {
                    stale.insert(m.avatar_key.clone());
                }
            });
            // A changed picture is still decoding: keep the old one meanwhile.
            let Some(img) = img else { continue };
            row.has_avatar = true;
            row.avatar = img;
            model.set_row_data(idx, row);
        }
    }

    /// Copies the open panel's state into the window.
    pub(super) fn render_info(&self) {
        let Some(win) = self.win() else { return };
        let p = self.p();
        let (Some(view), query) = INFO.with(|i| {
            let i = i.borrow();
            (i.view.clone(), i.query.clone())
        }) else {
            return;
        };
        let wanted = view.picture_path.clone();
        let (photo, start_decode) = INFO.with(|i| {
            let i = i.borrow();
            match &wanted {
                Some(path) => match &i.photo {
                    Some((pp, img)) if pp == path => (Some(img.clone()), false),
                    _ => (None, i.photo_loading.as_ref() != Some(path)),
                },
                None => (None, false),
            }
        });
        if let (true, Some(path)) = (start_decode, wanted) {
            self.load_info_photo(path);
        }
        let (photo, members, presence) = {
            let mut st = p.st.borrow_mut();
            let photo = photo.or_else(|| self.avatar(&p, &mut st, &view.jid));
            let shown = contact_info::filter_members(&view.members, &query);
            INFO.with(|i| i.borrow_mut().member_rows = shown.iter().map(|m| (m.jid.clone(), m.clone())).collect());
            let members: Vec<GroupMemberItem> = shown
                .iter()
                .map(|m| {
                    let avatar = self.member_avatar(&p, &mut st, m);
                    GroupMemberItem {
                        jid: m.jid.as_str().into(),
                        name: m.name.as_str().into(),
                        phone: m.phone.as_str().into(),
                        badge: m.badge.into(),
                        is_me: m.is_me,
                        initials: models::initials(&m.name).into(),
                        tint: models::tint_for(&m.jid),
                        has_avatar: avatar.is_some(),
                        avatar: avatar.unwrap_or_default(),
                        can_promote: m.can_promote,
                        can_demote: m.can_demote,
                        can_remove: m.can_remove,
                    }
                })
                .collect();
            let presence = if view.is_group {
                String::new()
            } else {
                super::privacy::presence_subtitle(st.presence.get(&view.jid), chrono::Local::now())
            };
            (photo, members, presence)
        };
        win.set_contact_info(ContactInfoModel {
            jid: view.jid.as_str().into(),
            title: view.title.as_str().into(),
            name: view.name.as_str().into(),
            phone: view.phone.as_str().into(),
            presence_online: presence == "online",
            presence: presence.into(),
            subtitle: view.subtitle.as_str().into(),
            about: view.about.as_str().into(),
            about_date: view.about_date.as_str().into(),
            is_group: view.is_group,
            is_business: view.is_business,
            loading: view.loading,
            has_photo: photo.is_some(),
            photo: photo.unwrap_or_default(),
            initials: view.initials.as_str().into(),
            tint: models::tint_for(&view.tint_key),
            description: view.description.as_str().into(),
            description_paras: models::plain_paras(&view.description),
            description_long: view.description_long,
            created_label: view.created_label.as_str().into(),
            members_total: i32::try_from(view.members.len()).unwrap_or(i32::MAX),
            is_admin: view.controls.is_admin,
            can_edit_info: view.controls.can_edit_info,
            can_leave: view.controls.can_leave,
            left: view.controls.left,
            announce_only: view.controls.announce_only,
            locked: view.controls.locked,
            approval_required: view.controls.approval_required,
            pending_requests: i32::try_from(view.controls.pending_requests).unwrap_or(i32::MAX),
            has_business: !view.business.is_empty(),
            biz_category: view.business.category.as_str().into(),
            biz_description: view.business.description.as_str().into(),
            biz_address: view.business.address.as_str().into(),
            biz_email: view.business.email.as_str().into(),
            biz_websites: strings_model(&view.business.websites),
            biz_website_hosts: strings_model(
                &view.business.websites.iter().map(|w| contact_info::website_host(w).unwrap_or_default()).collect::<Vec<_>>(),
            ),
            biz_hours: strings_model(&view.business.hours),
        });
        win.set_contact_members(ModelRc::new(VecModel::from(members)));
        win.set_contact_info_open(true);
    }

    fn load_info_photo(&self, path: PathBuf) {
        INFO.with(|i| i.borrow_mut().photo_loading = Some(path.clone()));
        let job = path.clone();
        let spawned = std::thread::Builder::new().name("info-photo".into()).spawn(move || {
            let result = models::decode_file_scaled(&job, PHOTO_DIM);
            let queued = slint::invoke_from_event_loop(move || with_app(|a| a.info_photo_ready(job, result)));
            if let Err(e) = queued {
                log::debug!("info photo not delivered: {e}");
            }
        });
        if let Err(e) = spawned {
            log::error!("could not start the photo decoder thread: {e}");
        }
    }

    fn info_photo_ready(&self, path: PathBuf, result: Result<SharedPixelBuffer<Rgba8Pixel>, String>) {
        let buf = match result {
            Ok(b) => b,
            Err(e) => {
                log::warn!("could not decode the profile photo: {e}");
                return;
            }
        };
        let current = INFO.with(|i| {
            let mut i = i.borrow_mut();
            let current = i.view.as_ref().and_then(|v| v.picture_path.as_ref()) == Some(&path);
            if current {
                i.photo = Some((path, Image::from_rgba8(buf)));
            }
            current
        });
        if current {
            self.render_info();
        }
    }

    pub(super) fn info_open_photo(&self) {
        let path = INFO.with(|i| i.borrow().view.as_ref().and_then(|v| v.picture_path.clone()));
        if let Some(path) = path {
            self.launch_checked(&path.to_string_lossy(), false);
        }
    }

    pub(super) fn info_copy_phone(&self) {
        let digits = INFO.with(|i| i.borrow().phone.clone());
        if !digits.is_empty() {
            self.copy_text(&format!("+{digits}"));
        }
    }

    /// The person shown in the open panel as a shareable card; `None` for
    /// groups and while the number is unknown.
    pub(super) fn info_shared_contact(&self) -> Option<wa_engine::SharedContact> {
        INFO.with(|i| {
            let i = i.borrow();
            let view = i.view.as_ref().filter(|v| !v.is_group)?;
            if i.phone.is_empty() {
                return None;
            }
            let name = [&view.name, &view.title].into_iter().find(|n| !n.is_empty()).cloned().unwrap_or_else(|| format!("+{}", i.phone));
            Some(wa_engine::SharedContact { name, phones: vec![format!("+{}", i.phone)] })
        })
    }

    pub(super) fn info_member_search(&self, query: &str) {
        INFO.with(|i| i.borrow_mut().query = query.to_string());
        self.render_info();
    }

    /// Opens (creating it if needed) the one-to-one chat with a group member.
    pub(super) fn info_open_member(&self, jid: &str) {
        let is_me = INFO.with(|i| {
            i.borrow().view.as_ref().is_some_and(|v| v.members.iter().any(|m| m.jid == jid && m.is_me))
        });
        if is_me {
            return;
        }
        self.info_close();
        let p = self.p();
        let known = self.ok("look up the chat", p.store.chat(jid));
        if known.is_some_and(|c| c.is_none())
            && self.ok("create the chat", p.store.upsert_chat(jid, ChatKind::Dm, None)).is_none()
        {
            return;
        }
        self.select_chat(jid);
        let row = self.ok("look up the chat", p.store.chat(jid)).flatten();
        p.st.borrow_mut().sel_row = row;
        self.update_header();
    }
}

/// Decodes the stored thumbnails of `rows` (worker thread only).
fn decode_thumbs(rows: &[&[MessageRow]]) -> Thumbs {
    rows.iter()
        .flat_map(|r| r.iter())
        .filter_map(|m| {
            let buf = crate::thumbs::decode_thumbnail_cached(&m.id, m.thumbnail.as_deref()?)?;
            Some((m.id.clone(), buf))
        })
        .collect()
}

fn thumb_item(thumbs: &HashMap<String, Image>, m: &MessageRow) -> GalleryThumb {
    let img = thumbs.get(&m.id).cloned();
    GalleryThumb {
        id: m.id.as_str().into(),
        has_thumb: img.is_some(),
        thumb: img.unwrap_or_default(),
        is_video: m.kind == wa_store::MessageKind::Video,
    }
}

/// Wires the callbacks of the "Media, links and docs" view.
pub(super) fn wire_gallery(win: &MainWindow) {
    let g = win.global::<InfoGallery>();
    g.on_show(|tab| with_app(|a| a.gallery_show(tab)));
    g.on_load_more(|tab| with_app(|a| a.gallery_more(tab)));
    g.on_open_item(|id| with_app(|a| a.gallery_open_item(id.as_str())));
    g.on_open_url(|url| {
        with_app(|a| match contact_info::normalize_url(url.as_str()) {
            Some(u) => a.open_link(&u),
            None => a.show_toast("That address looks unsafe and was not opened"),
        });
    });
}

impl App {
    fn gallery_chat(&self) -> Option<String> {
        INFO.with(|i| i.borrow().open.as_ref().map(|(_, jid)| jid.clone()))
    }

    /// Runs `work` on a worker thread and hands its result to `done` on the UI
    /// thread, unless the panel was closed or reopened meanwhile.
    fn gallery_job<T: Send + 'static>(&self, work: impl FnOnce() -> T + Send + 'static, done: fn(&App, T)) {
        let (generation, open) = INFO.with(|i| {
            let i = i.borrow();
            (i.generation, i.open.clone())
        });
        let spawned = std::thread::Builder::new().name("info-gallery".into()).spawn(move || {
            let result = work();
            let queued = slint::invoke_from_event_loop(move || {
                with_app(|a| {
                    let current = INFO.with(|i| {
                        let i = i.borrow();
                        i.generation == generation && i.open == open
                    });
                    if current {
                        done(a, result);
                    }
                });
            });
            if let Err(e) = queued {
                log::debug!("gallery result not delivered: {e}");
            }
        });
        if let Err(e) = spawned {
            log::error!("could not start the gallery worker thread: {e}");
        }
    }

    /// Counts and the preview strip, loaded when the panel opens.
    fn gallery_open_panel(&self, jid: &str) {
        let store = self.p().store.clone();
        let jid = jid.to_string();
        self.gallery_job(
            move || {
                let counts = store.chat_media_counts(&jid).map_err(|e| format!("count the shared media: {e}"));
                let preview = store.chat_media(&jid, None, PREVIEW_COUNT).map_err(|e| format!("load the shared media: {e}"));
                let thumbs = preview.as_ref().map(|p| decode_thumbs(&[p])).unwrap_or_default();
                (counts, preview, thumbs)
            },
            |app, (counts, preview, thumbs)| {
                INFO.with(|i| {
                    let g = &mut i.borrow_mut().gallery;
                    match counts {
                        Ok(c) => g.counts = c,
                        Err(e) => log::warn!("{e}"),
                    }
                    match preview {
                        Ok(p) => g.preview = p,
                        Err(e) => log::warn!("{e}"),
                    }
                    g.thumbs.extend(thumbs.into_iter().map(|(id, b)| (id, Image::from_rgba8(b))));
                });
                app.sync_gallery();
            },
        );
    }

    fn gallery_show(&self, tab: i32) {
        let Some(jid) = self.gallery_chat() else { return };
        let start = INFO.with(|i| {
            let g = &mut i.borrow_mut().gallery;
            let start = !g.loaded && !g.loading;
            g.loading |= start;
            start
        });
        if start {
            let store = self.p().store.clone();
            self.gallery_job(
                move || -> Result<FirstPage, String> {
                    let media = store.chat_media(&jid, None, GALLERY_PAGE).map_err(|e| format!("load the shared media: {e}"))?;
                    let docs = store.chat_docs(&jid, None, GALLERY_PAGE).map_err(|e| format!("load the shared documents: {e}"))?;
                    let links = store.chat_links(&jid, GALLERY_PAGE).map_err(|e| format!("load the shared links: {e}"))?;
                    let thumbs = decode_thumbs(&[&media]);
                    Ok(FirstPage { media, docs, links, thumbs })
                },
                |app, result| {
                    let now = chrono::Utc::now().timestamp();
                    INFO.with(|i| {
                        let g = &mut i.borrow_mut().gallery;
                        g.loading = false;
                        match result {
                            Ok(p) => {
                                g.loaded = true;
                                g.media_more = p.media.len() >= GALLERY_PAGE as usize;
                                g.media = p.media;
                                g.docs_more = p.docs.len() >= GALLERY_PAGE as usize;
                                g.docs = p.docs;
                                g.links_limit = GALLERY_PAGE;
                                g.links_more = p.links.len() >= GALLERY_PAGE as usize;
                                g.links = contact_info::link_views(&p.links, now);
                                g.thumbs.extend(p.thumbs.into_iter().map(|(id, b)| (id, Image::from_rgba8(b))));
                            }
                            Err(e) => log::warn!("{e}"),
                        }
                    });
                    app.sync_gallery();
                },
            );
        }
        self.sync_gallery();
        if let Some(win) = self.win() {
            let g = win.global::<InfoGallery>();
            g.set_tab(tab.clamp(0, 2));
            g.set_open(true);
        }
    }

    fn gallery_more(&self, tab: i32) {
        let Some(jid) = self.gallery_chat() else { return };
        let tab = tab.clamp(0, 2);
        if INFO.with(|i| std::mem::replace(&mut i.borrow_mut().gallery.more_busy[tab as usize], true)) {
            return;
        }
        let store = self.p().store.clone();
        if tab == 2 {
            let limit = INFO.with(|i| i.borrow().gallery.links_limit) + GALLERY_PAGE;
            self.gallery_job(
                move || store.chat_links(&jid, limit).map(|rows| (limit, rows)).map_err(|e| format!("load more shared links: {e}")),
                |app, result| {
                    let now = chrono::Utc::now().timestamp();
                    INFO.with(|i| {
                        let g = &mut i.borrow_mut().gallery;
                        g.more_busy[2] = false;
                        match result {
                            Ok((limit, rows)) => {
                                g.links_limit = limit;
                                g.links_more = rows.len() >= limit as usize;
                                g.links = contact_info::link_views(&rows, now);
                            }
                            Err(e) => log::warn!("{e}"),
                        }
                    });
                    app.sync_gallery();
                },
            );
            return;
        }
        let last = INFO.with(|i| {
            let g = &i.borrow().gallery;
            let rows = if tab == 0 { &g.media } else { &g.docs };
            rows.last().map(|m| (m.ts, m.id.clone()))
        });
        self.gallery_job(
            move || {
                let page = if tab == 0 {
                    store.chat_media(&jid, last, GALLERY_PAGE)
                } else {
                    store.chat_docs(&jid, last, GALLERY_PAGE)
                };
                let result = page.map_err(|e| format!("load more shared items: {e}")).map(|page| {
                    let thumbs = if tab == 0 { decode_thumbs(&[&page]) } else { Vec::new() };
                    (page, thumbs)
                });
                (tab, result)
            },
            |app, (tab, result)| {
                INFO.with(|i| {
                    let g = &mut i.borrow_mut().gallery;
                    g.more_busy[tab as usize] = false;
                    match result {
                        Ok((page, thumbs)) => {
                            let more = page.len() >= GALLERY_PAGE as usize;
                            if tab == 0 {
                                g.media.extend(page);
                                g.media_more = more;
                            } else {
                                g.docs.extend(page);
                                g.docs_more = more;
                            }
                            g.thumbs.extend(thumbs.into_iter().map(|(id, b)| (id, Image::from_rgba8(b))));
                        }
                        Err(e) => log::warn!("{e}"),
                    }
                });
                app.sync_gallery();
            },
        );
    }

    /// Opens a media item or document, downloading it first when needed.
    fn gallery_open_item(&self, id: &str) {
        let Some(jid) = self.gallery_chat() else { return };
        let local = INFO.with(|i| {
            let g = &i.borrow().gallery;
            g.media.iter().chain(&g.docs).chain(&g.preview).find(|m| m.id == id).map(|m| m.local_path.clone())
        });
        let Some(local) = local else { return };
        if let Some(path) = local.filter(|p| Path::new(p).is_file()) {
            self.open_or_reveal(&path);
            return;
        }
        let connected = self.p().st.borrow().conn == ConnState::Connected;
        let key = (self.p().id.clone(), jid.clone(), id.to_string());
        let already = INFO.with(|i| {
            let pending = &mut i.borrow_mut().gallery.pending;
            // A request outlives neither a connection change nor PENDING_TTL.
            pending.retain(|_, p| p.connected == connected && p.since.elapsed() < PENDING_TTL);
            if pending.contains_key(&key) {
                return true;
            }
            pending.insert(key, Pending { since: Instant::now(), connected });
            false
        });
        if already {
            self.show_toast("Already downloading");
            return;
        }
        self.show_toast("Downloading...");
        self.send(Command::DownloadMedia { chat_jid: jid, id: id.to_string() });
    }

    /// Files that are never opened from the app are shown in their folder.
    fn open_or_reveal(&self, path: &str) {
        if models::open_policy(path) == OpenPolicy::RevealOnly {
            self.show_toast("Showing in folder");
            self.launch_checked(path, true);
        } else {
            self.open_path(path);
        }
    }

    /// A download finished; opens it when it was started from the gallery.
    pub(super) fn info_media_ready(&self, profile: &str, chat_jid: &str, id: &str, path: &Path) {
        let path = path.to_string_lossy().into_owned();
        let wanted = INFO.with(|i| {
            let g = &mut i.borrow_mut().gallery;
            for m in g.media.iter_mut().chain(&mut g.docs).chain(&mut g.preview) {
                if m.id == id && m.chat_jid == chat_jid && m.local_path.is_none() {
                    m.local_path = Some(path.clone());
                }
            }
            g.pending.remove(&(profile.to_string(), chat_jid.to_string(), id.to_string())).is_some()
        });
        if wanted {
            self.open_or_reveal(&path);
        }
    }

    pub(super) fn info_media_failed(&self, profile: &str, chat_jid: &str, id: &str) {
        INFO.with(|i| {
            i.borrow_mut().gallery.pending.remove(&(profile.to_string(), chat_jid.to_string(), id.to_string()));
        });
    }

    /// Copies the gallery state into the window.
    fn sync_gallery(&self) {
        let Some(win) = self.win() else { return };
        let now = chrono::Utc::now().timestamp();
        INFO.with(|i| {
            let mut i = i.borrow_mut();
            let g = &mut i.gallery;
            let preview: Vec<GalleryThumb> = g.preview.iter().map(|m| thumb_item(&g.thumbs, m)).collect();
            let media: Vec<GalleryThumb> = g.media.iter().map(|m| thumb_item(&g.thumbs, m)).collect();
            let docs: Vec<GalleryDoc> = g
                .docs
                .iter()
                .map(|m| {
                    let d = contact_info::doc_view(m, now);
                    GalleryDoc {
                        id: d.id.into(),
                        name: d.name.into(),
                        size: d.size.into(),
                        date: d.date.into(),
                        ext: d.ext.into(),
                    }
                })
                .collect();
            let links: Vec<GalleryLink> = g
                .links
                .iter()
                .map(|l| GalleryLink { url: l.url.as_str().into(), domain: l.domain.as_str().into(), date: l.date.as_str().into() })
                .collect();
            let total = g.counts.0 + g.counts.1 + g.counts.2;
            let ui = win.global::<InfoGallery>();
            ui.set_loading(g.loading);
            ui.set_total(i32::try_from(total).unwrap_or(i32::MAX));
            ui.set_preview(ModelRc::new(VecModel::from(preview)));
            ui.set_media(ModelRc::new(VecModel::from(media)));
            ui.set_docs(ModelRc::new(VecModel::from(docs)));
            ui.set_links(ModelRc::new(VecModel::from(links)));
            ui.set_media_more(g.media_more);
            ui.set_docs_more(g.docs_more);
            ui.set_links_more(g.links_more);
        });
    }
}

#[cfg(debug_assertions)]
impl App {
    /// Debug aid for the demo (`WACLIENT_DEMO_UI=info|info-online|info-group|
    /// info-group-admin|info-group-invite|info-group-settings|info-group-left|info-biz|
    /// info-media|info-docs|info-links`): opens the panel and feeds it a
    /// canned engine answer.
    pub(super) fn demo_info(&self, what: &str) {
        use wa_engine::{BusinessProfile, GroupInfo, GroupMember};
        let group = what.starts_with("info-group");
        let requests = what == "info-group-requests";
        let admin = requests || what == "info-group-admin" || what == "info-group-invite" || what == "info-group-settings";
        let left = what == "info-group-left";
        let business = what == "info-biz";
        let jid = if group { "999@g.us" } else { "111@s.whatsapp.net" };
        self.select_chat(jid);
        self.demo_gallery_messages(jid);
        self.open_info();
        let member = |n: &str, name: &str, phone: &str, admin: bool, me: bool| GroupMember {
            jid: format!("{n}@s.whatsapp.net"),
            name: (!name.is_empty()).then(|| name.to_string()),
            phone: Some(phone.to_string()),
            is_admin: admin,
            is_super_admin: false,
            is_me: me,
        };
        let info = ContactInfo {
            jid: jid.to_string(),
            name: Some(if group { "Family Group" } else { "Alice Johnson" }.to_string()),
            push_name: Some("Ali".to_string()),
            phone: (!group).then(|| "989123456789".to_string()),
            about: (!group).then(|| "Busy. Message me on Signal if urgent.".to_string()),
            about_set_at: Some(chrono::Utc::now().timestamp() - 86_400 * 40),
            picture_path: None,
            is_business: business,
            business: business.then(|| BusinessProfile {
                description: Some("Handmade ceramics and pottery classes. Orders ship within three days.".to_string()),
                category: Some("Arts & crafts".to_string()),
                address: Some("12 Potters Lane, Kensington, London W8 5SA".to_string()),
                email: Some("hello@clayhouse.example".to_string()),
                websites: vec!["https://www.clayhouse.example".to_string(), "shop.clayhouse.example/gifts".to_string()],
                hours: vec![
                    "Mon-Fri: 09:00 - 17:30".to_string(),
                    "Sat: 10:00 - 14:00".to_string(),
                    "Sun: closed".to_string(),
                ],
            }),
            complete: true,
            group: group.then(|| GroupInfo {
                subject: "Family Group".to_string(),
                description: Some(
                    "Sunday lunch is at noon, bring dessert.\nNo spam, no chain messages, and please keep the photos to a reasonable number so everybody can still find the important announcements about dates and places.".to_string(),
                ),
                created_at: Some(chrono::Utc::now().timestamp() - 86_400 * 400),
                creator_jid: Some("111@s.whatsapp.net".to_string()),
                participants: vec![
                    member("1000", "You", "441234567890", admin, true),
                    GroupMember { is_super_admin: admin, ..member("111", "Alice Johnson", "989123456789", true, false) },
                    member("222", "Bob", "14155552671", false, false),
                    member("333", "Carol Smith", "447911123456", false, false),
                    member("444", "", "4915123456789", false, false),
                ],
                i_am_admin: admin,
                i_am_member: !left,
                announce_only: admin,
                approval_required: requests,
                pending_requests: if requests { 4 } else { 0 },
                ..GroupInfo::default()
            }),
        };
        if group {
            self.demo_member_avatars(&["1000", "111", "222", "333"]);
        }
        self.on_contact_info(&self.p().id.clone(), true, info);
        if let Some(win) = self.win() {
            let ga = win.global::<crate::GroupAdmin>();
            match what {
                "info-group-invite" => {
                    ga.set_view(1);
                    ga.set_invite_link("https://chat.whatsapp.com/DemoInviteCode0123456".into());
                }
                "info-group-settings" => ga.set_view(2),
                "info-group-requests" => self.demo_requests(),
                _ => {}
            }
        }
        if what == "info" || what == "info-online" {
            let seen = Some(chrono::Utc::now().timestamp() - 25 * 60);
            self.on_presence(&self.p(), jid.to_string(), what == "info-online", seen);
        }
        if let Some(tab) = ["info-media", "info-docs", "info-links"].iter().position(|w| *w == what) {
            self.gallery_show(tab as i32);
        }
    }

    /// Gives the demo members stored profile pictures (generated files).
    fn demo_member_avatars(&self, users: &[&str]) {
        let dir = std::env::temp_dir().join("waclient-demo-avatars");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            log::warn!("demo: could not create the avatar folder: {e}");
            return;
        }
        for (n, user) in users.iter().enumerate() {
            let n = n as u32;
            let img = image::RgbImage::from_fn(128, 128, |x, y| {
                let (dx, dy) = (x as i32 - 64, y as i32 - 40 - (n as i32 % 2) * 10);
                let head = dx * dx + dy * dy < 24 * 24;
                let body = y > 78 && (x as i32 - 64).pow(2) / 2 + (y as i32 - 128).pow(2) < 52 * 52;
                if head || body {
                    image::Rgb([245, 225 - (n * 20) as u8, 200])
                } else {
                    image::Rgb([(40 + n * 55) as u8, (120 + n * 25 % 100) as u8, (200 - n * 40) as u8])
                }
            });
            let path = dir.join(format!("{user}.png"));
            if let Err(e) = img.save(&path) {
                log::warn!("demo: could not write an avatar: {e}");
                continue;
            }
            let jid = format!("{user}@s.whatsapp.net");
            if let Err(e) = self.p().store.set_contact_avatar(&jid, &format!("demo{n}"), &path.to_string_lossy()) {
                log::warn!("demo: could not store an avatar: {e}");
            }
            // The chat list may already have looked this contact up (no picture then).
            self.p().st.borrow_mut().avatar_paths.remove(&jid);
        }
    }

    /// Shared media, documents and links for the demo chat.
    fn demo_gallery_messages(&self, jid: &str) {
        use wa_store::{MessageKind, NewMessage};
        let now = chrono::Utc::now().timestamp();
        let thumb = |n: u32| -> Option<Vec<u8>> {
            let img = image::RgbImage::from_fn(96, 96, |x, y| {
                image::Rgb([((x * 2 + n * 37) % 256) as u8, ((y * 2 + n * 71) % 256) as u8, (90 + n * 29 % 160) as u8])
            });
            let mut out = Vec::new();
            img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Jpeg).ok()?;
            Some(out)
        };
        let base = |id: String, ts: i64, kind: MessageKind| NewMessage {
            chat_jid: jid.to_string(),
            id,
            sender_jid: jid.to_string(),
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
            link_url: None,
            link_title: None,
            link_desc: None,
            link_thumb: None,
            ..Default::default()
        };
        let store = &self.p().store;
        let mut rows = Vec::new();
        for n in 0..14u32 {
            let video = n % 5 == 2;
            let kind = if video { MessageKind::Video } else { MessageKind::Image };
            let mut m = base(format!("gm{n}"), now - 3_000 - i64::from(n) * 9_000, kind);
            m.thumbnail = thumb(n);
            rows.push(m);
        }
        let docs = [("Invoice March.pdf", 184_320), ("Lease agreement.docx", 2_516_582), ("notes.txt", 900), ("Budget 2024.xlsx", 48_128)];
        for (n, (name, size)) in docs.iter().enumerate() {
            let mut m = base(format!("gd{n}"), now - 5_000 - n as i64 * 70_000, MessageKind::Document);
            m.media_name = Some((*name).to_string());
            m.media_size = Some(*size);
            rows.push(m);
        }
        let links = [
            "Docs are here https://docs.example.com/guide/getting-started?ref=chat",
            "check www.news.example.org/story/42 out",
            "https://github.com/example/project/issues/7",
        ];
        for (n, text) in links.iter().enumerate() {
            let mut m = base(format!("gl{n}"), now - 7_000 - n as i64 * 120_000, MessageKind::Text);
            m.text = Some((*text).to_string());
            rows.push(m);
        }
        if let Err(e) = store.insert_messages_bulk(&rows) {
            log::warn!("demo: could not add the shared items: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{answer_matches, user_part};

    #[test]
    fn user_part_ignores_server_and_device() {
        assert_eq!(user_part("123@s.whatsapp.net"), "123");
        assert_eq!(user_part("123:5@s.whatsapp.net"), "123");
        assert_eq!(user_part("123@lid"), "123");
        assert_eq!(user_part("123"), "123");
    }

    #[test]
    fn answer_matches_by_user_or_phone_digits() {
        assert!(answer_matches("123@s.whatsapp.net", "123:4@s.whatsapp.net", None));
        assert!(answer_matches("999@lid", "123@s.whatsapp.net", Some("999")));
        assert!(answer_matches("123@s.whatsapp.net", "999@lid", Some("+123")));
        assert!(!answer_matches("123@s.whatsapp.net", "999@lid", Some("456")));
        assert!(!answer_matches("123@s.whatsapp.net", "999@lid", None));
        assert!(!answer_matches("123@s.whatsapp.net", "999@lid", Some("")));
    }
}
