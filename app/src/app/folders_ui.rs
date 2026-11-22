//! Chat folders: the tabs above the chat list (All, Unread, Personal, Groups
//! and the user's own), their dialogs and the per-profile `folders.json`.
//! The Slint side is the `FolderState` global (ui/folders.slint). Everything
//! decided here is pure (membership, counts, order, names, file format) and
//! tested; `App` only carries it out. Folders are local: nothing is sent.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use slint::{ComponentHandle, Model, ModelRc, Timer, TimerMode, VecModel};
use wa_store::{ChatKind, ChatRow};

use super::share_ui::{Candidate, chat_candidates, filter_candidates};
use super::{App, ProfileState, with_app};
use crate::{FolderMenuRow, FolderPickRow, FolderState, FolderTab, MainWindow, models};

const FILE: &str = "folders.json";
/// Longest folder name, in characters.
pub const MAX_NAME_CHARS: usize = 24;
/// Most custom folders per profile.
pub const MAX_FOLDERS: usize = 20;
const ALL: &str = "all";
/// Built-in folders: id and tab label, in tab order.
const BUILTIN: [(&str, &str); 4] = [(ALL, "All"), ("unread", "Unread"), ("personal", "Personal"), ("groups", "Groups")];
/// Dialog numbers shared with `folders.slint`.
const DIALOG_NONE: i32 = 0;
const DIALOG_FORM: i32 = 1;
const DIALOG_DELETE: i32 = 2;
/// A tab click is written this long after the last one (the file is also
/// written on a profile switch and at exit).
const SELECT_SAVE_DELAY: Duration = Duration::from_secs(1);

/// A user-defined folder.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Folder {
    pub id: String,
    pub name: String,
    /// Chats added by hand. A chat that no longer exists just never matches.
    pub chats: Vec<String>,
    pub include_unread: bool,
    pub include_groups: bool,
    pub include_personal: bool,
    /// Muted chats are left out of what the rules above pull in (chats added
    /// by hand always stay).
    pub exclude_muted: bool,
}

/// Everything remembered about the folders of one profile; the order of
/// `folders` is the order of the tabs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FolderConfig {
    /// The tab strip is shown.
    pub enabled: bool,
    pub folders: Vec<Folder>,
    /// Id of the selected tab.
    pub selected: String,
}

impl Default for FolderConfig {
    fn default() -> Self {
        FolderConfig { enabled: true, folders: Vec::new(), selected: ALL.to_string() }
    }
}

impl FolderConfig {
    /// Drops what cannot be used (unnamed, duplicate or built-in-colliding ids,
    /// folders beyond the limit) and falls back to "All" for an unknown tab.
    pub fn sanitized(mut self) -> Self {
        let mut seen: Vec<String> = Vec::new();
        self.folders.retain(|f| {
            let fresh = !f.name.trim().is_empty() && !f.id.is_empty() && !is_builtin(&f.id) && !seen.contains(&f.id);
            if fresh {
                seen.push(f.id.clone());
            }
            fresh
        });
        self.folders.truncate(MAX_FOLDERS);
        if !self.has_tab(&self.selected) {
            self.selected = ALL.to_string();
        }
        self
    }

    fn has_tab(&self, id: &str) -> bool {
        is_builtin(id) || self.folders.iter().any(|f| f.id == id)
    }

    /// Ids of the tabs, left to right.
    pub fn tab_ids(&self) -> Vec<String> {
        BUILTIN.iter().map(|(id, _)| id.to_string()).chain(self.folders.iter().map(|f| f.id.clone())).collect()
    }

    fn folder_mut(&mut self, id: &str) -> Option<&mut Folder> {
        self.folders.iter_mut().find(|f| f.id == id)
    }
}

fn is_builtin(id: &str) -> bool {
    BUILTIN.iter().any(|(b, _)| *b == id)
}

/// What membership depends on, for one chat.
#[derive(Debug, Clone, Copy)]
pub struct ChatFacts<'a> {
    pub jid: &'a str,
    pub kind: ChatKind,
    pub unread: i64,
    pub muted: bool,
}

pub fn facts(r: &ChatRow, now: i64) -> ChatFacts<'_> {
    ChatFacts { jid: &r.jid, kind: r.kind, unread: r.unread, muted: models::is_muted_until(r.muted_until, now) }
}

fn folder_has(f: &Folder, c: &ChatFacts) -> bool {
    if f.chats.iter().any(|j| j == c.jid) {
        return true;
    }
    // Status updates and other non-chats are never pulled in by a rule.
    if c.kind == ChatKind::Other || (f.exclude_muted && c.muted) {
        return false;
    }
    (f.include_unread && c.unread > 0)
        || (f.include_groups && c.kind == ChatKind::Group)
        || (f.include_personal && c.kind == ChatKind::Dm)
}

/// Whether the chat is listed under the tab `id`. Archived chats must be
/// filtered out by the caller: they belong to no folder.
pub fn in_folder(cfg: &FolderConfig, id: &str, c: &ChatFacts) -> bool {
    match id {
        ALL => true,
        "unread" => c.unread > 0,
        "personal" => c.kind == ChatKind::Dm,
        "groups" => c.kind == ChatKind::Group,
        custom => cfg.folders.iter().find(|f| f.id == custom).is_some_and(|f| folder_has(f, c)),
    }
}

/// Chats with unread messages in one folder, and how many of them are not muted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Count {
    pub unread: usize,
    pub unmuted: usize,
}

/// One entry per tab, in tab order, over the chats that are not archived.
pub fn tab_counts(cfg: &FolderConfig, chats: &[ChatFacts]) -> Vec<(String, Count)> {
    cfg.tab_ids()
        .into_iter()
        .map(|id| {
            let mut n = Count::default();
            for c in chats.iter().filter(|c| c.unread > 0 && in_folder(cfg, &id, c)) {
                n.unread += 1;
                if !c.muted {
                    n.unmuted += 1;
                }
            }
            (id, n)
        })
        .collect()
}

/// Moves a folder one place left (`delta < 0`) or right. False when it is
/// unknown or already at that end.
pub fn move_folder(folders: &mut [Folder], id: &str, delta: i32) -> bool {
    let Some(i) = folders.iter().position(|f| f.id == id) else { return false };
    let j = if delta < 0 { i.checked_sub(1) } else { Some(i + 1).filter(|j| *j < folders.len()) };
    match j {
        Some(j) => {
            folders.swap(i, j);
            true
        }
        None => false,
    }
}

/// The trimmed name, or why it cannot be used. `editing` is the folder being
/// renamed (it may keep its own name).
pub fn validate_name(raw: &str, folders: &[Folder], editing: Option<&str>) -> Result<String, &'static str> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("Enter a folder name");
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err("The name can be at most 24 characters");
    }
    let same = |other: &str| other.trim().to_lowercase() == name.to_lowercase();
    let taken = BUILTIN.iter().any(|(_, label)| same(label))
        || folders.iter().any(|f| Some(f.id.as_str()) != editing && same(&f.name));
    if taken {
        return Err("A folder with this name already exists");
    }
    Ok(name.to_string())
}

/// An id no folder uses: `f` and the next number.
pub fn new_id(folders: &[Folder]) -> String {
    let max = folders.iter().filter_map(|f| f.id.strip_prefix('f')?.parse::<u64>().ok()).max().unwrap_or(0);
    format!("f{}", max + 1)
}

/// Adds the chat to the folder, or removes it when present. True when the
/// chat is in the folder afterwards.
pub fn toggle_chat(f: &mut Folder, jid: &str) -> bool {
    if let Some(pos) = f.chats.iter().position(|j| j == jid) {
        f.chats.remove(pos);
        false
    } else {
        f.chats.push(jid.to_string());
        true
    }
}

/// The tabs as the UI shows them.
pub fn build_tabs(cfg: &FolderConfig, counts: &[(String, Count)]) -> Vec<FolderTab> {
    let count_of = |id: &str| counts.iter().find(|(i, _)| i == id).map(|(_, n)| *n).unwrap_or_default();
    let last = cfg.folders.len().saturating_sub(1);
    let tab = |id: &str, name: &str, builtin: bool, pos: usize| {
        let n = count_of(id);
        FolderTab {
            id: id.into(),
            name: name.into(),
            unread: i32::try_from(n.unread).unwrap_or(i32::MAX),
            grey: n.unread > 0 && n.unmuted == 0,
            builtin,
            can_left: !builtin && pos > 0,
            can_right: !builtin && pos < last,
        }
    };
    BUILTIN
        .iter()
        .map(|(id, name)| tab(id, name, true, 0))
        .chain(cfg.folders.iter().enumerate().map(|(i, f)| tab(&f.id, &f.name, false, i)))
        .collect()
}

fn path(dir: &Path) -> std::path::PathBuf {
    dir.join(FILE)
}

/// What `load` found.
#[derive(Debug, PartialEq)]
pub struct Loaded {
    pub cfg: FolderConfig,
    /// False when an unusable file could not be set aside: saving would
    /// overwrite it, so the folders stay read-only for the session.
    pub writable: bool,
}

/// What to do with a `folders.json` that could not be read.
#[derive(Debug, PartialEq, Eq)]
enum ReadFailure {
    /// There is no file yet: use the defaults.
    Absent,
    /// The file is there but unreadable: set it aside before using defaults,
    /// so a later save cannot destroy it.
    MoveAside,
}

fn classify_read_error(kind: std::io::ErrorKind) -> ReadFailure {
    if kind == std::io::ErrorKind::NotFound { ReadFailure::Absent } else { ReadFailure::MoveAside }
}

/// Renames the unusable file to `folders.json.bad`; false if that failed.
fn move_aside(dir: &Path) -> bool {
    let file = path(dir);
    match fs::rename(&file, dir.join(format!("{FILE}.bad"))) {
        Ok(()) => true,
        Err(e) => {
            log::warn!("could not set aside {}: {e}; the folders are read-only this session", file.display());
            false
        }
    }
}

/// Loads `<dir>/folders.json`; a missing file gives the defaults, a corrupt or
/// unreadable one is moved aside to `folders.json.bad` and the defaults are used.
pub fn load(dir: &Path) -> Loaded {
    let file = path(dir);
    let bytes = match fs::read(&file) {
        Ok(b) => b,
        Err(e) => {
            return match classify_read_error(e.kind()) {
                ReadFailure::Absent => Loaded { cfg: FolderConfig::default(), writable: true },
                ReadFailure::MoveAside => {
                    log::warn!("reading {}: {e}; using defaults", file.display());
                    Loaded { cfg: FolderConfig::default(), writable: move_aside(dir) }
                }
            };
        }
    };
    match serde_json::from_slice::<FolderConfig>(&bytes) {
        Ok(cfg) => Loaded { cfg: cfg.sanitized(), writable: true },
        Err(e) => {
            log::warn!("corrupt {}: {e}; using defaults", file.display());
            Loaded { cfg: FolderConfig::default(), writable: move_aside(dir) }
        }
    }
}

/// When a pending tab-click write is due: every click restarts the wait, so a
/// burst of clicks causes one write.
#[derive(Debug, Default)]
struct Debounce {
    last: Option<Instant>,
}

#[derive(Debug, PartialEq, Eq)]
enum Wait {
    Nothing,
    Due,
    Pending(Duration),
}

impl Debounce {
    fn touch(&mut self, now: Instant) {
        self.last = Some(now);
    }

    fn clear(&mut self) {
        self.last = None;
    }

    fn state(&self, now: Instant, delay: Duration) -> Wait {
        let Some(last) = self.last else { return Wait::Nothing };
        let waited = now.saturating_duration_since(last);
        if waited >= delay { Wait::Due } else { Wait::Pending(delay - waited) }
    }
}

/// Atomically writes `<dir>/folders.json` (temp file and rename).
pub fn save(dir: &Path, cfg: &FolderConfig) -> anyhow::Result<()> {
    fs::create_dir_all(dir).context("creating the profile folder")?;
    let json = serde_json::to_vec_pretty(cfg).context("serializing the folders")?;
    let tmp = dir.join(format!("{FILE}.tmp"));
    {
        let mut f = fs::File::create(&tmp).context("creating the temporary folders file")?;
        f.write_all(&json).context("writing the temporary folders file")?;
        f.sync_all().context("flushing the temporary folders file")?;
    }
    fs::rename(&tmp, path(dir)).context("replacing the folders file")?;
    Ok(())
}

/// Makes `model` show `new`, touching only the rows that differ so open menus
/// and scroll positions survive the frequent refreshes.
fn sync_vec<T: Clone + PartialEq + 'static>(model: &VecModel<T>, new: Vec<T>) {
    if model.row_count() != new.len() {
        model.set_vec(new);
        return;
    }
    for (i, row) in new.into_iter().enumerate() {
        if model.row_data(i).as_ref() != Some(&row) {
            model.set_row_data(i, row);
        }
    }
}

/// The picker's rows: candidates matching `query`, with their pick state.
fn pick_rows(all: &[Candidate], query: &str, picked: &[String]) -> Vec<FolderPickRow> {
    filter_candidates(all, query)
        .into_iter()
        .map(|c| FolderPickRow {
            key: c.jid.as_str().into(),
            name: c.name.as_str().into(),
            initials: models::initials(&c.name).into(),
            tint: models::tint_for(&c.jid),
            group: c.jid.ends_with("@g.us"),
            selected: picked.contains(&c.jid),
        })
        .collect()
}

/// The folder form that is open.
struct Draft {
    profile: String,
    /// The folder being edited; `None` for a new one.
    editing: Option<String>,
    picked: Vec<String>,
    query: String,
    all: Vec<Candidate>,
}

impl Draft {
    /// Picked chats that still exist (dangling addresses are kept but not counted).
    fn picked_count(&self) -> i32 {
        i32::try_from(self.all.iter().filter(|c| self.picked.contains(&c.jid)).count()).unwrap_or(i32::MAX)
    }
}

#[derive(Default)]
struct Runtime {
    /// Loaded folder settings, by profile id.
    cfgs: HashMap<String, FolderConfig>,
    draft: Option<Draft>,
    /// Profile and folder of the open delete confirmation.
    delete: Option<(String, String)>,
    /// Profiles whose folders file must not be written (see `Loaded::writable`).
    read_only: HashSet<String>,
    /// Profiles with a selected-tab change not yet written.
    dirty: HashSet<String>,
    debounce: Debounce,
    save_timer: Timer,
    tabs: Rc<VecModel<FolderTab>>,
    rows: Rc<VecModel<FolderPickRow>>,
    menu: Rc<VecModel<FolderMenuRow>>,
}

thread_local! {
    static RT: RefCell<Runtime> = RefCell::default();
}

/// Connects the `FolderState` callbacks and models.
pub fn wire(win: &MainWindow) {
    let st = win.global::<FolderState>();
    RT.with(|rt| {
        let rt = rt.borrow();
        st.set_tabs(ModelRc::from(rt.tabs.clone()));
        st.set_rows(ModelRc::from(rt.rows.clone()));
        st.set_menu_rows(ModelRc::from(rt.menu.clone()));
    });
    st.on_select(|id| with_app(|a| a.folder_select(id.as_str())));
    st.on_select_at(|i| with_app(|a| a.folder_select_at(i)));
    st.on_new_folder(|| with_app(|a| a.folder_open_form(None)));
    st.on_edit(|id| with_app(|a| a.folder_open_form(Some(id.as_str()))));
    st.on_delete_ask(|id| with_app(|a| a.folder_delete_ask(id.as_str())));
    st.on_move(|id, delta| with_app(|a| a.folder_move(id.as_str(), delta)));
    st.on_toggle_enabled(|| with_app(App::folder_toggle_enabled));
    st.on_prepare_menu(|jid| with_app(|a| a.folder_prepare_menu(jid.as_str())));
    st.on_menu_toggle(|jid, id| with_app(|a| a.folder_menu_toggle(jid.as_str(), id.as_str())));
    st.on_pick_search(|q| with_app(|a| a.folder_pick_search(q.as_str())));
    st.on_toggle_pick(|key| with_app(|a| a.folder_toggle_pick(key.as_str())));
    st.on_save(|| with_app(App::folder_save));
    st.on_confirm_delete(|| with_app(App::folder_confirm_delete));
    st.on_cancel(|| with_app(App::folder_cancel));
}

/// Which chats the active folder lets through; taken once per list refresh.
pub(super) struct FolderView {
    cfg: FolderConfig,
    now: i64,
}

impl FolderView {
    /// Whether a non-archived chat is listed under the selected folder.
    pub(super) fn shows(&self, r: &ChatRow) -> bool {
        !self.cfg.enabled || in_folder(&self.cfg, &self.cfg.selected, &facts(r, self.now))
    }
}

impl App {
    fn folder_cfg(&self, p: &ProfileState) -> FolderConfig {
        RT.with(|rt| {
            let mut rt = rt.borrow_mut();
            if !rt.cfgs.contains_key(&p.id) {
                let loaded = load(&self.profile_dir(p));
                if !loaded.writable {
                    rt.read_only.insert(p.id.clone());
                }
                rt.cfgs.insert(p.id.clone(), loaded.cfg);
            }
            rt.cfgs.get(&p.id).cloned().unwrap_or_default()
        })
    }

    /// Writes the settings of `p`, unless its file had to be left alone.
    fn folder_write(&self, p: &ProfileState, cfg: &FolderConfig) {
        if RT.with(|rt| rt.borrow().read_only.contains(&p.id)) {
            return;
        }
        if let Err(e) = save(&self.profile_dir(p), cfg) {
            log::error!("could not save the chat folders of {}: {e:#}", p.id);
            self.show_banner(1, &format!("Could not save the folders: {e:#}"));
        }
    }

    /// Applies `change` to the profile's folder settings and writes them.
    fn folder_update(&self, p: &ProfileState, change: impl FnOnce(&mut FolderConfig)) {
        let mut cfg = self.folder_cfg(p);
        change(&mut cfg);
        let cfg = cfg.sanitized();
        self.folder_write(p, &cfg);
        RT.with(|rt| {
            let mut rt = rt.borrow_mut();
            rt.dirty.remove(&p.id);
            rt.cfgs.insert(p.id.clone(), cfg);
        });
    }

    /// Like `folder_update` for the selected tab: the change is applied now and
    /// written a second after the last click, on a profile switch or at exit.
    fn folder_update_lazily(&self, p: &ProfileState, change: impl FnOnce(&mut FolderConfig)) {
        let mut cfg = self.folder_cfg(p);
        change(&mut cfg);
        let cfg = cfg.sanitized();
        RT.with(|rt| {
            let mut rt = rt.borrow_mut();
            rt.cfgs.insert(p.id.clone(), cfg);
            rt.dirty.insert(p.id.clone());
            rt.debounce.touch(Instant::now());
            rt.save_timer.start(TimerMode::SingleShot, SELECT_SAVE_DELAY, || with_app(App::folders_flush_due));
        });
    }

    fn folders_flush_due(&self) {
        let wait = RT.with(|rt| rt.borrow().debounce.state(Instant::now(), SELECT_SAVE_DELAY));
        match wait {
            Wait::Nothing => {}
            Wait::Due => self.folders_flush(),
            Wait::Pending(left) => RT.with(|rt| {
                rt.borrow().save_timer.start(TimerMode::SingleShot, left, || with_app(App::folders_flush_due));
            }),
        }
    }

    /// Writes every selected-tab change that is still waiting.
    pub(super) fn folders_flush(&self) {
        let ids: Vec<String> = RT.with(|rt| {
            let mut rt = rt.borrow_mut();
            rt.debounce.clear();
            rt.save_timer.stop();
            rt.dirty.drain().collect()
        });
        for id in ids {
            let Some(p) = self.find(&id) else { continue };
            if let Some(cfg) = RT.with(|rt| rt.borrow().cfgs.get(&id).cloned()) {
                self.folder_write(&p, &cfg);
            }
        }
    }

    pub(super) fn folders_view(&self, p: &ProfileState) -> FolderView {
        FolderView { cfg: self.folder_cfg(p), now: chrono::Utc::now().timestamp() }
    }

    /// Updates the tab strip (names, badges, selection) from the chats of the
    /// active profile. `full` is every chat, archived ones included.
    pub(super) fn folders_sync(&self, p: &ProfileState, full: &[ChatRow]) {
        let Some(win) = self.win() else { return };
        let cfg = self.folder_cfg(p);
        let now = chrono::Utc::now().timestamp();
        let live: Vec<ChatFacts> = full.iter().filter(|r| !r.archived).map(|r| facts(r, now)).collect();
        let tabs = if cfg.enabled { build_tabs(&cfg, &tab_counts(&cfg, &live)) } else { Vec::new() };
        let st = win.global::<FolderState>();
        st.set_enabled(cfg.enabled);
        st.set_has_custom(cfg.enabled && !cfg.folders.is_empty());
        st.set_selected(cfg.selected.as_str().into());
        RT.with(|rt| sync_vec(&rt.borrow().tabs, tabs));
    }

    /// The list is built from the folder settings again.
    fn folders_changed(&self) {
        self.refresh_chats();
    }

    /// The active profile changed: an open dialog belonged to the old one.
    pub(super) fn folders_profile_changed(&self) {
        self.folders_flush();
        let open = self.win().is_some_and(|w| w.global::<FolderState>().get_dialog() != DIALOG_NONE);
        if open {
            self.folder_cancel();
        }
    }

    fn folder_select(&self, id: &str) {
        let p = self.p();
        if !self.folder_cfg(&p).has_tab(id) {
            return;
        }
        self.folder_update_lazily(&p, |c| c.selected = id.to_string());
        self.folders_changed();
    }

    fn folder_select_at(&self, i: i32) {
        let p = self.p();
        let cfg = self.folder_cfg(&p);
        let id = usize::try_from(i).ok().and_then(|i| cfg.tab_ids().get(i).cloned());
        if let Some(id) = id.filter(|_| cfg.enabled) {
            self.folder_select(&id);
        }
    }

    fn folder_toggle_enabled(&self) {
        let p = self.p();
        self.folder_update(&p, |c| c.enabled = !c.enabled);
        self.folders_changed();
    }

    fn folder_move(&self, id: &str, delta: i32) {
        let p = self.p();
        self.folder_update(&p, |c| {
            move_folder(&mut c.folders, id, delta);
        });
        self.folders_changed();
    }

    // ---- the folder form ---------------------------------------------------

    /// Opens the form for a new folder (`None`) or an existing one.
    fn folder_open_form(&self, editing: Option<&str>) {
        let Some(win) = self.win() else { return };
        let p = self.p();
        let cfg = self.folder_cfg(&p);
        let current = editing.and_then(|id| cfg.folders.iter().find(|f| f.id == id));
        if editing.is_some() && current.is_none() {
            return;
        }
        if current.is_none() && cfg.folders.len() >= MAX_FOLDERS {
            self.show_toast(&format!("You can have up to {MAX_FOLDERS} folders"));
            return;
        }
        let blank = Folder::default();
        let f = current.unwrap_or(&blank);
        let chats = p.st.borrow().chats.clone();
        let draft = Draft {
            profile: p.id.clone(),
            editing: editing.map(str::to_string),
            picked: f.chats.clone(),
            query: String::new(),
            all: chat_candidates(&chats, |_| false),
        };
        let st = win.global::<FolderState>();
        st.set_form_title(if editing.is_some() { "Edit folder" } else { "New folder" }.into());
        st.set_form_action(if editing.is_some() { "Save" } else { "Create" }.into());
        st.set_name(f.name.as_str().into());
        st.set_error("".into());
        st.set_rule_unread(f.include_unread);
        st.set_rule_groups(f.include_groups);
        st.set_rule_personal(f.include_personal);
        st.set_rule_exclude_muted(f.exclude_muted);
        st.set_picked(draft.picked_count());
        let rows = pick_rows(&draft.all, "", &draft.picked);
        RT.with(|rt| {
            let mut rt = rt.borrow_mut();
            rt.rows.set_vec(rows);
            rt.draft = Some(draft);
            rt.delete = None;
        });
        st.set_dialog(DIALOG_FORM);
    }

    fn folder_pick_search(&self, query: &str) {
        RT.with(|rt| {
            let mut rt = rt.borrow_mut();
            let Some(d) = rt.draft.as_mut() else { return };
            d.query = query.to_string();
            let rows = pick_rows(&d.all, &d.query, &d.picked);
            rt.rows.set_vec(rows);
        });
    }

    fn folder_toggle_pick(&self, key: &str) {
        let Some(win) = self.win() else { return };
        let count = RT.with(|rt| {
            let mut rt = rt.borrow_mut();
            let rows = rt.rows.clone();
            let d = rt.draft.as_mut()?;
            let on = match d.picked.iter().position(|j| j == key) {
                Some(pos) => {
                    d.picked.remove(pos);
                    false
                }
                None => {
                    d.picked.push(key.to_string());
                    true
                }
            };
            if let Some(i) = (0..rows.row_count()).find(|i| rows.row_data(*i).is_some_and(|r| r.key == key)) {
                if let Some(mut row) = rows.row_data(i) {
                    row.selected = on;
                    rows.set_row_data(i, row);
                }
            }
            Some(d.picked_count())
        });
        if let Some(n) = count {
            win.global::<FolderState>().set_picked(n);
        }
    }

    fn folder_save(&self) {
        let Some(win) = self.win() else { return };
        let st = win.global::<FolderState>();
        if st.get_dialog() != DIALOG_FORM {
            return;
        }
        let p = self.p();
        let Some((editing, picked)) = RT.with(|rt| {
            let rt = rt.borrow();
            let d = rt.draft.as_ref().filter(|d| d.profile == p.id)?;
            Some((d.editing.clone(), d.picked.clone()))
        }) else {
            self.folder_cancel();
            return;
        };
        let cfg = self.folder_cfg(&p);
        let name = match validate_name(st.get_name().as_str(), &cfg.folders, editing.as_deref()) {
            Ok(n) => n,
            Err(why) => {
                st.set_error(why.into());
                return;
            }
        };
        let rules = (st.get_rule_unread(), st.get_rule_groups(), st.get_rule_personal(), st.get_rule_exclude_muted());
        let fill = move |f: &mut Folder| {
            f.name = name;
            f.chats = picked;
            (f.include_unread, f.include_groups, f.include_personal, f.exclude_muted) = rules;
        };
        self.folder_update(&p, |c| match &editing {
            Some(id) => {
                if let Some(f) = c.folder_mut(id) {
                    fill(f);
                }
            }
            None => {
                let mut f = Folder { id: new_id(&c.folders), ..Folder::default() };
                fill(&mut f);
                c.selected = f.id.clone();
                c.folders.push(f);
            }
        });
        self.folder_close_dialog();
        self.folders_changed();
    }

    fn folder_close_dialog(&self) {
        RT.with(|rt| {
            let mut rt = rt.borrow_mut();
            rt.draft = None;
            rt.delete = None;
            rt.rows.set_vec(Vec::new());
        });
        if let Some(win) = self.win() {
            win.global::<FolderState>().set_dialog(DIALOG_NONE);
        }
    }

    /// Esc, Cancel, or a profile switch.
    pub(super) fn folder_cancel(&self) {
        self.folder_close_dialog();
    }

    // ---- delete ------------------------------------------------------------

    fn folder_delete_ask(&self, id: &str) {
        let Some(win) = self.win() else { return };
        let p = self.p();
        let Some(name) = self.folder_cfg(&p).folders.iter().find(|f| f.id == id).map(|f| f.name.clone()) else { return };
        RT.with(|rt| rt.borrow_mut().delete = Some((p.id.clone(), id.to_string())));
        let st = win.global::<FolderState>();
        st.set_delete_name(format!("\u{201c}{name}\u{201d}").into());
        st.set_dialog(DIALOG_DELETE);
    }

    fn folder_confirm_delete(&self) {
        let p = self.p();
        let target = RT.with(|rt| rt.borrow_mut().delete.take());
        self.folder_close_dialog();
        let Some((profile, id)) = target else { return };
        if profile != p.id {
            return;
        }
        self.folder_update(&p, |c| {
            c.folders.retain(|f| f.id != id);
            if c.selected == id {
                c.selected = ALL.to_string();
            }
        });
        self.folders_changed();
    }

    // ---- "Add to folder" in the chat menu ----------------------------------

    fn menu_rows(cfg: &FolderConfig, jid: &str) -> Vec<FolderMenuRow> {
        cfg.folders
            .iter()
            .map(|f| FolderMenuRow { id: f.id.as_str().into(), name: f.name.as_str().into(), member: f.chats.iter().any(|j| j == jid) })
            .collect()
    }

    fn folder_prepare_menu(&self, jid: &str) {
        let rows = Self::menu_rows(&self.folder_cfg(&self.p()), jid);
        RT.with(|rt| rt.borrow().menu.set_vec(rows));
    }

    fn folder_menu_toggle(&self, jid: &str, folder: &str) {
        let p = self.p();
        self.folder_update(&p, |c| {
            if let Some(f) = c.folder_mut(folder) {
                toggle_chat(f, jid);
            }
        });
        let rows = Self::menu_rows(&self.folder_cfg(&p), jid);
        RT.with(|rt| rt.borrow().menu.set_vec(rows));
        self.folders_changed();
    }
}

#[cfg(debug_assertions)]
impl App {
    /// Demo (`WACLIENT_DEMO_UI=folders|folders-form`): two custom folders, the
    /// first one selected, written to the demo's temporary settings folder.
    pub fn demo_folders(&self, form: bool) {
        let p = self.p();
        let demo = FolderConfig {
            enabled: true,
            folders: vec![
                Folder {
                    id: "f1".into(),
                    name: "Work".into(),
                    chats: vec!["111@s.whatsapp.net".into(), "999@g.us".into()],
                    ..Folder::default()
                },
                Folder { id: "f2".into(), name: "Family".into(), include_groups: true, ..Folder::default() },
            ],
            selected: "f1".into(),
        };
        self.folder_update(&p, |c| *c = demo);
        self.folders_changed();
        if form {
            self.folder_open_form(Some("f1"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(jid: &str, kind: ChatKind, unread: i64, muted: bool) -> ChatFacts<'_> {
        ChatFacts { jid, kind, unread, muted }
    }

    fn folder(id: &str, name: &str) -> Folder {
        Folder { id: id.into(), name: name.into(), ..Folder::default() }
    }

    fn cfg_with(folders: Vec<Folder>) -> FolderConfig {
        FolderConfig { folders, ..FolderConfig::default() }
    }

    #[test]
    fn built_in_folders() {
        let cfg = FolderConfig::default();
        let dm = chat("1@s.whatsapp.net", ChatKind::Dm, 0, false);
        let group = chat("2@g.us", ChatKind::Group, 3, false);
        let status = chat("status@broadcast", ChatKind::Other, 0, false);
        assert!(in_folder(&cfg, "all", &status));
        assert!(!in_folder(&cfg, "unread", &dm));
        assert!(in_folder(&cfg, "unread", &group));
        assert!(in_folder(&cfg, "personal", &dm));
        assert!(!in_folder(&cfg, "personal", &group));
        assert!(!in_folder(&cfg, "personal", &status));
        assert!(in_folder(&cfg, "groups", &group));
        assert!(!in_folder(&cfg, "groups", &dm));
        assert!(!in_folder(&cfg, "missing", &dm));
    }

    #[test]
    fn custom_folder_rules_and_explicit_chats() {
        let mut f = folder("f1", "Work");
        f.chats = vec!["1@s.whatsapp.net".into(), "gone@s.whatsapp.net".into()];
        f.include_groups = true;
        f.include_unread = true;
        f.exclude_muted = true;
        let cfg = cfg_with(vec![f]);
        let listed = |c: ChatFacts| in_folder(&cfg, "f1", &c);
        // Added by hand: always, even muted.
        assert!(listed(chat("1@s.whatsapp.net", ChatKind::Dm, 0, true)));
        // Pulled in by a rule, unless muted.
        assert!(listed(chat("2@g.us", ChatKind::Group, 0, false)));
        assert!(!listed(chat("2@g.us", ChatKind::Group, 0, true)));
        assert!(listed(chat("3@s.whatsapp.net", ChatKind::Dm, 2, false)));
        assert!(!listed(chat("3@s.whatsapp.net", ChatKind::Dm, 2, true)));
        // No rule matches a read personal chat nobody added.
        assert!(!listed(chat("4@s.whatsapp.net", ChatKind::Dm, 0, false)));
        // Status updates never match a rule.
        assert!(!listed(chat("status@broadcast", ChatKind::Other, 5, false)));
    }

    #[test]
    fn personal_rule_takes_direct_chats_only() {
        let mut f = folder("f1", "DMs");
        f.include_personal = true;
        let cfg = cfg_with(vec![f]);
        assert!(in_folder(&cfg, "f1", &chat("1@lid", ChatKind::Dm, 0, false)));
        assert!(!in_folder(&cfg, "f1", &chat("2@g.us", ChatKind::Group, 0, false)));
    }

    #[test]
    fn counts_per_tab_with_muted_chats() {
        let mut f = folder("f1", "Groups+");
        f.include_groups = true;
        let cfg = cfg_with(vec![f]);
        let chats = [
            chat("1@s.whatsapp.net", ChatKind::Dm, 2, false),
            chat("2@s.whatsapp.net", ChatKind::Dm, 0, false),
            chat("3@g.us", ChatKind::Group, 5, true),
            chat("4@g.us", ChatKind::Group, 1, false),
        ];
        let counts = tab_counts(&cfg, &chats);
        let get = |id: &str| counts.iter().find(|(i, _)| i == id).map(|(_, n)| *n).unwrap_or_default();
        assert_eq!(get("all"), Count { unread: 3, unmuted: 2 });
        assert_eq!(get("unread"), Count { unread: 3, unmuted: 2 });
        assert_eq!(get("personal"), Count { unread: 1, unmuted: 1 });
        assert_eq!(get("groups"), Count { unread: 2, unmuted: 1 });
        assert_eq!(get("f1"), Count { unread: 2, unmuted: 1 });
        assert_eq!(counts.len(), 5);
    }

    #[test]
    fn tabs_are_grey_only_when_every_unread_chat_is_muted() {
        let cfg = cfg_with(vec![folder("f1", "A"), folder("f2", "B"), folder("f3", "C")]);
        let counts = vec![
            ("all".to_string(), Count { unread: 2, unmuted: 1 }),
            ("groups".to_string(), Count { unread: 3, unmuted: 0 }),
        ];
        let tabs = build_tabs(&cfg, &counts);
        assert_eq!(tabs.len(), 7);
        assert!(!tabs[0].grey && tabs[0].unread == 2);
        assert!(tabs[3].grey && tabs[3].unread == 3);
        assert!(!tabs[1].grey && tabs[1].unread == 0);
        assert!(tabs[0].builtin && !tabs[4].builtin);
        assert!(!tabs[4].can_left && tabs[4].can_right);
        assert!(tabs[5].can_left && tabs[5].can_right);
        assert!(tabs[6].can_left && !tabs[6].can_right);
    }

    #[test]
    fn reorder_moves_one_place_and_stops_at_the_ends() {
        let mut fs = vec![folder("f1", "A"), folder("f2", "B"), folder("f3", "C")];
        assert!(move_folder(&mut fs, "f3", -1));
        assert_eq!(fs.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(), ["f1", "f3", "f2"]);
        assert!(move_folder(&mut fs, "f1", 1));
        assert_eq!(fs.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(), ["f3", "f1", "f2"]);
        assert!(!move_folder(&mut fs, "f3", -1));
        assert!(!move_folder(&mut fs, "f2", 1));
        assert!(!move_folder(&mut fs, "nope", 1));
    }

    #[test]
    fn names_are_validated() {
        let fs = vec![folder("f1", "Work")];
        assert_eq!(validate_name("  Family  ", &fs, None), Ok("Family".to_string()));
        assert!(validate_name("   ", &fs, None).is_err());
        assert!(validate_name(&"x".repeat(24), &fs, None).is_ok());
        assert!(validate_name(&"x".repeat(25), &fs, None).is_err());
        assert!(validate_name("work", &fs, None).is_err());
        assert!(validate_name("work", &fs, Some("f1")).is_ok());
        assert!(validate_name("Unread", &fs, None).is_err());
        // Length counts characters, not bytes.
        assert!(validate_name(&"\u{e9}".repeat(24), &fs, None).is_ok());
    }

    #[test]
    fn new_ids_never_repeat() {
        assert_eq!(new_id(&[]), "f1");
        let fs = vec![folder("f1", "A"), folder("f7", "B")];
        assert_eq!(new_id(&fs), "f8");
    }

    #[test]
    fn toggling_a_chat_adds_then_removes_it() {
        let mut f = folder("f1", "A");
        assert!(toggle_chat(&mut f, "1@g.us"));
        assert_eq!(f.chats, ["1@g.us"]);
        assert!(!toggle_chat(&mut f, "1@g.us"));
        assert!(f.chats.is_empty());
    }

    #[test]
    fn sanitizing_drops_unusable_folders() {
        let mut dup = folder("f1", "Again");
        dup.chats = vec!["x".into()];
        let cfg = FolderConfig {
            enabled: true,
            folders: vec![folder("f1", "A"), dup, folder("", "No id"), folder("f2", "  "), folder("all", "Clash")],
            selected: "f9".into(),
        }
        .sanitized();
        assert_eq!(cfg.folders.len(), 1);
        assert_eq!(cfg.selected, "all");
        let mut keep = cfg_with(vec![folder("f1", "A")]);
        keep.selected = "f1".into();
        assert_eq!(keep.clone().sanitized().selected, "f1");
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("waclient-folders-test-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn config_survives_a_save_and_load() {
        let dir = temp_dir("roundtrip");
        let mut f = folder("f1", "Work");
        f.chats = vec!["1@s.whatsapp.net".into(), "2@g.us".into()];
        f.include_unread = true;
        f.exclude_muted = true;
        let cfg = FolderConfig { enabled: false, folders: vec![f, folder("f2", "Home")], selected: "f2".into() };
        save(&dir, &cfg).expect("save");
        assert_eq!(load(&dir), Loaded { cfg, writable: true });
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_gives_defaults_and_a_corrupt_one_is_set_aside() {
        let dir = temp_dir("corrupt");
        assert_eq!(load(&dir), Loaded { cfg: FolderConfig::default(), writable: true });
        fs::create_dir_all(&dir).expect("dir");
        fs::write(dir.join(FILE), b"{not json").expect("write");
        assert_eq!(load(&dir), Loaded { cfg: FolderConfig::default(), writable: true });
        assert!(dir.join(format!("{FILE}.bad")).exists());
        assert!(!dir.join(FILE).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_a_missing_file_is_not_set_aside() {
        use std::io::ErrorKind;
        assert_eq!(classify_read_error(ErrorKind::NotFound), ReadFailure::Absent);
        for kind in [ErrorKind::PermissionDenied, ErrorKind::InvalidData, ErrorKind::Other, ErrorKind::IsADirectory] {
            assert_eq!(classify_read_error(kind), ReadFailure::MoveAside, "{kind:?}");
        }
    }

    #[test]
    fn an_unreadable_file_is_set_aside_not_overwritten() {
        let dir = temp_dir("unreadable");
        fs::create_dir_all(dir.join(FILE)).expect("a directory where the file should be");
        let loaded = load(&dir);
        assert_eq!(loaded.cfg, FolderConfig::default());
        assert!(loaded.writable, "the unusable entry was moved out of the way");
        assert!(dir.join(format!("{FILE}.bad")).exists());
        assert!(!dir.join(FILE).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_that_cannot_be_set_aside_makes_the_folders_read_only() {
        let dir = temp_dir("stuck");
        fs::create_dir_all(dir.join(FILE)).expect("a directory where the file should be");
        // A non-empty directory already named like the backup blocks the rename.
        let bad = dir.join(format!("{FILE}.bad"));
        fs::create_dir_all(&bad).expect("bad dir");
        fs::write(bad.join("keep"), b"x").expect("keep");
        let loaded = load(&dir);
        assert_eq!(loaded.cfg, FolderConfig::default());
        assert!(!loaded.writable);
        assert!(dir.join(FILE).exists(), "the original is untouched");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tab_clicks_are_written_one_second_after_the_last() {
        let delay = Duration::from_secs(1);
        let t0 = Instant::now();
        let mut d = Debounce::default();
        assert_eq!(d.state(t0, delay), Wait::Nothing);
        d.touch(t0);
        assert_eq!(d.state(t0, delay), Wait::Pending(delay));
        assert_eq!(d.state(t0 + Duration::from_millis(400), delay), Wait::Pending(Duration::from_millis(600)));
        // A second click restarts the wait.
        d.touch(t0 + Duration::from_millis(900));
        assert_eq!(d.state(t0 + Duration::from_millis(1200), delay), Wait::Pending(Duration::from_millis(700)));
        assert_eq!(d.state(t0 + Duration::from_millis(1900), delay), Wait::Due);
        assert_eq!(d.state(t0 + Duration::from_secs(60), delay), Wait::Due);
        d.clear();
        assert_eq!(d.state(t0 + Duration::from_secs(60), delay), Wait::Nothing);
    }

    #[test]
    fn old_or_partial_files_fill_in_defaults() {
        let dir = temp_dir("partial");
        fs::create_dir_all(&dir).expect("dir");
        fs::write(dir.join(FILE), br#"{"folders":[{"id":"f1","name":"A"}]}"#).expect("write");
        let cfg = load(&dir).cfg;
        assert!(cfg.enabled);
        assert_eq!(cfg.selected, "all");
        assert_eq!(cfg.folders.len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }
}
