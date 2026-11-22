//! Profile registry (`profiles.json`), per-profile data directories and the
//! one-time migration of the pre-profiles single-account layout.
//!
//! Pure file-system logic with no UI or engine dependencies.
//!
//! Safety rules this module upholds (the user's account data goes through it):
//! * the legacy migration is journaled: the registry entry is saved with
//!   `migrating: true` before the first rename and resumed after a crash;
//! * a registry that cannot be *read* is never replaced by an empty one, only
//!   one that cannot be *parsed* is backed up;
//! * unknown directories are never deleted; unregistered profile folders are
//!   re-adopted.

use std::collections::hash_map::RandomState;
use std::collections::HashSet;
use std::hash::{BuildHasher, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Accounts that can be open at once (each one is a full engine).
pub const MAX_PROFILES: usize = 5;
pub const MAX_NAME_CHARS: usize = 24;
pub const REGISTRY_FILE: &str = "profiles.json";
const PROFILES_DIR: &str = "profiles";
/// Files and folders of one account, as laid out by the engine, grouped so a
/// database always travels together with its `-wal` and `-shm` files.
const GROUPS: [&[&str]; 4] = [
    &["session.db", "session.db-wal", "session.db-shm"],
    &["messages.db", "messages.db-wal", "messages.db-shm"],
    &["media"],
    &["settings.json"],
];
/// Leftovers of the settings writer that are also removed with a root profile.
const SETTINGS_LEFTOVERS: [&str; 2] = ["settings.json.bad", "settings.json.tmp"];
const LEGACY_PROFILE_NAME: &str = "Main";
/// Used for a path that must never exist when an invalid id is given.
const INVALID_DIR: &str = ".invalid-profile-id";
const READ_ATTEMPTS: u32 = 5;
const READ_RETRY_DELAY: Duration = Duration::from_millis(200);
const COLORS: [&str; 8] = [
    "#00a884", "#53bdeb", "#e56b6f", "#f5a623", "#a57be6", "#4cc28a", "#e67eb8", "#6a8ff0",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub name: String,
    /// `#rrggbb`, used for the initials circle.
    pub color: String,
    pub created_at: i64,
    pub order: u32,
    /// Our own account, learned when pairing succeeds (for the rail avatar).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub own_jid: Option<String>,
    /// The legacy files could not be moved into a profile folder, so this
    /// profile keeps using the root data directory.
    #[serde(default, skip_serializing_if = "is_false")]
    pub in_root: bool,
    /// Journal flag: the legacy files are (still) being moved into this
    /// profile's folder. No engine may open the profile while it is set.
    #[serde(default, skip_serializing_if = "is_false")]
    pub migrating: bool,
    /// Why the profile must not be started (a rollback failed, so its files
    /// are split between the root and its folder). Shown to the user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub broken: Option<String>,
}

impl Profile {
    /// Safe for an engine to open: not mid-migration and not broken.
    pub fn is_ready(&self) -> bool {
        !self.migrating && self.broken.is_none()
    }
}

fn is_false(v: &bool) -> bool {
    !*v
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registry {
    pub profiles: Vec<Profile>,
    #[serde(default)]
    pub last_active: Option<String>,
    /// Removed profiles whose folders still have to be deleted (the engine's
    /// files may be locked for a while, or the app may crash first).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_delete: Vec<String>,
    /// Entries beyond [`MAX_PROFILES`]: kept (and their folders untouched) but
    /// never started.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overflow: Vec<Profile>,
    /// Set when `profiles.json` exists but could not be read. The registry is
    /// then only a best-effort view and [`Registry::save`] refuses to write.
    #[serde(skip)]
    pub load_error: Option<String>,
    /// Profile folders found on disk that could not be re-adopted (limit).
    #[serde(skip)]
    pub unadopted: Vec<String>,
    /// Pending deletes that failed during start-up and will be retried.
    #[serde(skip)]
    pub sweep_failed: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AddError {
    LimitReached,
}

impl std::fmt::Display for AddError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AddError::LimitReached => write!(f, "At most {MAX_PROFILES} profiles are supported"),
        }
    }
}

impl std::error::Error for AddError {}

/// Eight hex characters from the OS-seeded hasher (no `rand` dependency).
pub fn new_id() -> String {
    let mut h = RandomState::new().build_hasher();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    h.write_u128(nanos);
    format!("{:08x}", h.finish() as u32)
}

/// Ids are `^[0-9a-f]{8}$`: they become folder names, so nothing else
/// (separators, `..`, drive letters) may ever get through.
pub fn valid_id(id: &str) -> bool {
    id.len() == 8 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn registry_path(root: &Path) -> PathBuf {
    root.join(REGISTRY_FILE)
}

/// Where the engine keeps the files of `p`. An invalid id (which a loaded
/// registry never contains) maps to a path that is never created by us.
pub fn data_dir(root: &Path, p: &Profile) -> PathBuf {
    try_data_dir(root, p).unwrap_or_else(|_| root.join(PROFILES_DIR).join(INVALID_DIR))
}

fn try_data_dir(root: &Path, p: &Profile) -> io::Result<PathBuf> {
    if p.in_root {
        return Ok(root.to_path_buf());
    }
    profile_dir(root, &p.id)
}

fn profile_dir(root: &Path, id: &str) -> io::Result<PathBuf> {
    if valid_id(id) {
        Ok(root.join(PROFILES_DIR).join(id))
    } else {
        Err(io::Error::new(io::ErrorKind::InvalidInput, format!("invalid profile id {id:?}")))
    }
}

/// Opens the registry, finishing or starting the legacy migration, re-adopting
/// orphaned profile folders and sweeping pending deletes.
///
/// Never returns an empty registry for a `profiles.json` that exists but cannot
/// be read: the result then carries `load_error`, refuses to save, and holds at
/// most the profile folders found on disk. The caller must show that and must
/// not start engines on it without telling the user (see [`open_strict`]).
pub fn open(root: &Path) -> Registry {
    match open_strict(root) {
        Ok(reg) => reg,
        Err(e) => {
            log::error!("cannot open the profile registry: {e}");
            let mut reg = Registry { load_error: Some(e.to_string()), ..Registry::default() };
            let (_, unadopted) = reg.adopt_orphans(root, now_secs());
            reg.unadopted = unadopted;
            reg
        }
    }
}

/// Like [`open`] but reports an unreadable registry as an error and touches nothing.
pub fn open_strict(root: &Path) -> io::Result<Registry> {
    let mut reg = load_strict(root)?;
    let registry_existed = registry_path(root).exists();
    let now = now_secs();
    let mut dirty = false;

    let resuming: Vec<usize> =
        reg.profiles.iter().enumerate().filter(|(_, p)| p.migrating).map(|(i, _)| i).collect();
    for idx in resuming {
        finish_migration(root, &mut reg, idx);
    }

    reg.sweep_failed = reg.sweep_pending_deletes(root);

    let (adopted, unadopted) = reg.adopt_orphans(root, now);
    reg.unadopted = unadopted;
    dirty |= !adopted.is_empty();

    if !registry_existed && legacy_present(root) && !reg.profiles.iter().any(|p| p.in_root) {
        if !reg.can_add() {
            log::warn!("legacy data stays in {} (profile limit reached)", root.display());
        } else if let (1, Some(id)) = (adopted.len(), adopted.first()) {
            // A migration that crashed before the registry was lost: its
            // half-filled folder is the only candidate, so finish into it.
            adopt_legacy_into(root, &mut reg, id);
        } else {
            migrate_legacy(root, &mut reg, now);
        }
        dirty = false;
    }
    if dirty {
        if let Err(e) = reg.save(root) {
            log::error!("could not save the re-adopted profiles: {e}");
        }
    }
    Ok(reg)
}

/// Reads the registry. Equivalent to [`load_strict`], but an unreadable file
/// yields an empty registry with `load_error` set (and untouched on disk).
#[allow(dead_code)] // kept for callers that only want a view of the file
pub fn load(root: &Path) -> Registry {
    match load_strict(root) {
        Ok(reg) => reg,
        Err(e) => {
            log::error!("cannot read the profile registry: {e}");
            Registry { load_error: Some(e.to_string()), ..Registry::default() }
        }
    }
}

/// Reads the registry. A missing file gives an empty registry. Only a JSON
/// parse error counts as corruption: the file is then renamed to
/// `profiles.json.corrupt-<ts>`. Any I/O error (sharing violation, permission)
/// is retried and finally returned, leaving the file alone.
pub fn load_strict(root: &Path) -> io::Result<Registry> {
    let path = registry_path(root);
    let Some(bytes) = read_with_retry(&path, READ_ATTEMPTS, READ_RETRY_DELAY)? else {
        return Ok(Registry::default());
    };
    match serde_json::from_slice::<Registry>(&bytes) {
        Ok(mut reg) => {
            reg.normalize();
            Ok(reg)
        }
        Err(e) => {
            log::error!("{} is corrupt: {e}", path.display());
            backup_corrupt(&path)?;
            Ok(Registry::default())
        }
    }
}

/// `None` when the file does not exist.
fn read_with_retry(path: &Path, attempts: u32, delay: Duration) -> io::Result<Option<Vec<u8>>> {
    let mut attempt = 1;
    loop {
        match std::fs::read(path) {
            Ok(b) => return Ok(Some(b)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) if attempt >= attempts => {
                log::error!("cannot read {} after {attempts} attempts: {e}", path.display());
                return Err(e);
            }
            Err(e) => {
                log::warn!("cannot read {} (attempt {attempt}): {e}", path.display());
                attempt += 1;
                std::thread::sleep(delay);
            }
        }
    }
}

fn backup_corrupt(path: &Path) -> io::Result<()> {
    let backup = path.with_file_name(format!("{REGISTRY_FILE}.corrupt-{}", now_secs()));
    // A rename would silently replace an existing backup of the same second.
    if backup.exists() {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, "a corrupt-registry backup already exists"));
    }
    std::fs::rename(path, &backup).inspect_err(|e| log::error!("could not back up {}: {e}", path.display()))?;
    log::warn!("kept the unreadable registry as {}", backup.display());
    Ok(())
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

impl Registry {
    /// Drops invalid and duplicate ids (their folders stay untouched), sorts by
    /// `order`, moves entries over the limit to `overflow`, and forgets a
    /// `last_active` that points nowhere.
    fn normalize(&mut self) {
        let mut seen = HashSet::new();
        self.profiles.retain(|p| {
            if !valid_id(&p.id) {
                log::warn!("ignoring the profile {:?}: invalid id (its folder is left untouched)", p.id);
                return false;
            }
            seen.insert(p.id.clone())
        });
        self.profiles.sort_by_key(|p| p.order);
        if self.profiles.len() > MAX_PROFILES {
            let extra = self.profiles.split_off(MAX_PROFILES);
            log::warn!("{} profiles exceed the limit and are kept but not started", extra.len());
            self.overflow.extend(extra);
        }
        let mut overflow_seen = HashSet::new();
        let profiles = &self.profiles;
        self.overflow
            .retain(|p| valid_id(&p.id) && profiles.iter().all(|q| q.id != p.id) && overflow_seen.insert(p.id.clone()));
        let mut pending_seen = HashSet::new();
        self.pending_delete.retain(|id| valid_id(id) && pending_seen.insert(id.clone()));
        self.renumber();
        if self.last_active.as_ref().is_some_and(|id| self.get(id).is_none()) {
            self.last_active = None;
        }
    }

    fn renumber(&mut self) {
        for (i, p) in self.profiles.iter_mut().enumerate() {
            p.order = i as u32;
        }
    }

    /// Writes the registry atomically (temporary file, flushed to disk, then
    /// rename). Refuses when the registry could not be read at start-up.
    pub fn save(&self, root: &Path) -> io::Result<()> {
        if let Some(why) = &self.load_error {
            return Err(io::Error::other(format!("not saving over an unreadable registry: {why}")));
        }
        std::fs::create_dir_all(root)?;
        let json = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        let path = registry_path(root);
        let tmp = path.with_file_name(format!("{REGISTRY_FILE}.tmp"));
        let written = (|| {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(json.as_bytes())?;
            f.sync_all()
        })();
        let result = written.and_then(|()| std::fs::rename(&tmp, &path));
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    pub fn get(&self, id: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    pub fn can_add(&self) -> bool {
        self.profiles.len() < MAX_PROFILES
    }

    /// An id must not be reused while anything could still refer to it: a
    /// registered or overflow entry, a queued delete, or a folder on disk.
    fn id_taken(&self, root: Option<&Path>, id: &str) -> bool {
        self.get(id).is_some()
            || self.overflow.iter().any(|p| p.id == id)
            || self.pending_delete.iter().any(|p| p == id)
            || root.is_some_and(|r| r.join(PROFILES_DIR).join(id).exists())
    }

    fn fresh_id(&self, root: Option<&Path>) -> String {
        loop {
            let id = new_id();
            if !self.id_taken(root, &id) {
                return id;
            }
        }
    }

    fn new_profile(&self, id: String, name: String, now: i64) -> Profile {
        let color = COLORS
            .iter()
            .find(|c| self.profiles.iter().all(|p| p.color != **c))
            .unwrap_or(&COLORS[self.profiles.len() % COLORS.len()]);
        Profile {
            id,
            name,
            color: (*color).to_string(),
            created_at: now,
            order: self.profiles.len() as u32,
            own_jid: None,
            in_root: false,
            migrating: false,
            broken: None,
        }
    }

    /// Appends a profile named `name` (or "Account N") with the first unused colour.
    /// Does not look at the disk; prefer [`Registry::add_in`].
    #[cfg(test)]
    pub fn add(&mut self, name: Option<&str>, now: i64) -> Result<Profile, AddError> {
        self.add_checked(None, name, now)
    }

    /// Like [`Registry::add`], but never picks an id whose folder already exists.
    pub fn add_in(&mut self, root: &Path, name: Option<&str>, now: i64) -> Result<Profile, AddError> {
        self.add_checked(Some(root), name, now)
    }

    fn add_checked(&mut self, root: Option<&Path>, name: Option<&str>, now: i64) -> Result<Profile, AddError> {
        if !self.can_add() {
            return Err(AddError::LimitReached);
        }
        let id = self.fresh_id(root);
        let name = name
            .and_then(clean_name)
            .unwrap_or_else(|| format!("Account {}", self.profiles.len() + 1));
        let profile = self.new_profile(id, name, now);
        self.profiles.push(profile.clone());
        Ok(profile)
    }

    pub fn remove(&mut self, id: &str) -> Option<Profile> {
        let idx = self.profiles.iter().position(|p| p.id == id)?;
        let removed = self.profiles.remove(idx);
        self.renumber();
        if self.last_active.as_deref() == Some(id) {
            self.last_active = None;
        }
        Some(removed)
    }

    /// Removes the entry and records its folder for deletion, then saves. If the
    /// save fails nothing changes and the error is returned (do not delete).
    /// A profile living in the root is only removed from the list: its files
    /// are not in a folder of their own, use [`remove_data`] for them.
    pub fn mark_pending_delete(&mut self, root: &Path, id: &str) -> io::Result<Option<Profile>> {
        profile_dir(root, id)?;
        let before = self.clone();
        let Some(removed) = self.remove(id) else { return Ok(None) };
        if !removed.in_root && !self.pending_delete.iter().any(|p| p == id) {
            self.pending_delete.push(id.to_string());
        }
        if let Err(e) = self.save(root) {
            *self = before;
            return Err(e);
        }
        Ok(Some(removed))
    }

    /// Deletes the folders of queued deletes (valid id, inside `profiles/`
    /// only). Succeeded ids leave the queue; the returned ids failed and stay.
    pub fn sweep_pending_deletes(&mut self, root: &Path) -> Vec<String> {
        if self.pending_delete.is_empty() {
            return Vec::new();
        }
        let mut failed = Vec::new();
        let queue = std::mem::take(&mut self.pending_delete);
        let mut changed = false;
        for id in queue {
            let dir = match profile_dir(root, &id) {
                Ok(d) => d,
                Err(e) => {
                    log::warn!("dropping a queued delete: {e}");
                    changed = true;
                    continue;
                }
            };
            if self.get(&id).is_some() {
                log::warn!("profile {id} is registered again; not deleting its folder");
                changed = true;
                continue;
            }
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => {
                    log::info!("deleted the data of profile {id}");
                    changed = true;
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => changed = true,
                Err(e) => {
                    log::warn!("could not delete the data of profile {id} yet: {e}");
                    failed.push(id);
                }
            }
        }
        self.pending_delete.clone_from(&failed);
        if changed && self.load_error.is_none() {
            if let Err(e) = self.save(root) {
                log::error!("could not save the registry after deleting profile data: {e}");
            }
        }
        failed
    }

    /// Registers unregistered `profiles/<id>` folders that hold a database as
    /// "Recovered <id>". Returns `(adopted, left_alone)`: folders over the
    /// profile limit stay on disk untouched. Nothing is ever deleted.
    pub fn adopt_orphans(&mut self, root: &Path, now: i64) -> (Vec<String>, Vec<String>) {
        let dir = root.join(PROFILES_DIR);
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => {
                if e.kind() != io::ErrorKind::NotFound {
                    log::warn!("cannot scan {} for profiles: {e}", dir.display());
                }
                return (Vec::new(), Vec::new());
            }
        };
        let mut candidates: Vec<String> = entries
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().to_str().map(str::to_owned))
            .filter(|name| valid_id(name))
            .filter(|name| !self.id_taken(None, name))
            .filter(|name| {
                let p = dir.join(name);
                p.join("session.db").exists() || p.join("messages.db").exists()
            })
            .collect();
        candidates.sort();
        let (mut adopted, mut left) = (Vec::new(), Vec::new());
        for id in candidates {
            if !self.can_add() {
                log::warn!("profile folder {id} is not registered and the limit is reached; leaving it alone");
                left.push(id);
                continue;
            }
            log::warn!("re-adopting the unregistered profile folder {id}");
            let profile = self.new_profile(id.clone(), format!("Recovered {id}"), now);
            self.profiles.push(profile);
            adopted.push(id);
        }
        (adopted, left)
    }

    /// Returns false for an unknown id or a blank name.
    pub fn rename(&mut self, id: &str, name: &str) -> bool {
        let Some(name) = clean_name(name) else { return false };
        match self.profiles.iter_mut().find(|p| p.id == id) {
            Some(p) => {
                p.name = name;
                true
            }
            None => false,
        }
    }

    pub fn set_own_jid(&mut self, id: &str, jid: &str) -> bool {
        match self.profiles.iter_mut().find(|p| p.id == id) {
            Some(p) if p.own_jid.as_deref() != Some(jid) => {
                p.own_jid = Some(jid.to_string());
                true
            }
            _ => false,
        }
    }

    /// Moves `id` to position `to` (clamped). False when nothing moved
    /// (unknown id, or already at that position), so callers can skip saving.
    pub fn reorder(&mut self, id: &str, to: usize) -> bool {
        let Some(from) = self.profiles.iter().position(|p| p.id == id) else {
            return false;
        };
        let to = to.min(self.profiles.len() - 1);
        if to == from {
            return false;
        }
        let p = self.profiles.remove(from);
        self.profiles.insert(to, p);
        self.renumber();
        true
    }
}

/// Trims and limits a profile name; `None` when nothing is left.
pub fn clean_name(name: &str) -> Option<String> {
    let name: String = name.trim().chars().take(MAX_NAME_CHARS).collect();
    let name = name.trim_end().to_string();
    (!name.is_empty()).then_some(name)
}

/// `#rrggbb` to its components; a neutral grey if malformed.
pub fn parse_color(hex: &str) -> (u8, u8, u8) {
    let h = hex.strip_prefix('#').unwrap_or(hex);
    let parse = |i: usize| h.get(i..i + 2).and_then(|s| u8::from_str_radix(s, 16).ok());
    match (h.len(), parse(0), parse(2), parse(4)) {
        (6, Some(r), Some(g), Some(b)) => (r, g, b),
        _ => (0x86, 0x96, 0xa0),
    }
}

// ---- legacy migration -----------------------------------------------------

fn legacy_present(root: &Path) -> bool {
    root.join("session.db").exists() || root.join("messages.db").exists()
}

fn path_exists(p: &Path) -> io::Result<bool> {
    match std::fs::symlink_metadata(p) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Renames `from` to `to` if `from` exists. Never replaces an existing `to`
/// (a plain rename would silently overwrite a file on Windows).
fn move_item(from: &Path, to: &Path) -> io::Result<bool> {
    if !path_exists(from)? {
        return Ok(false);
    }
    if path_exists(to)? {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} already exists", to.display()),
        ));
    }
    std::fs::rename(from, to)?;
    Ok(true)
}

/// Moves every legacy item still in `root` into `dest`, group by group, main
/// file first and its `-wal`/`-shm` right after. Safe to run again after a
/// crash: items already moved are skipped.
fn move_groups(root: &Path, dest: &Path) -> io::Result<usize> {
    std::fs::create_dir_all(dest)?;
    let mut moved = 0;
    for item in GROUPS.iter().copied().flatten() {
        if move_item(&root.join(item), &dest.join(item))? {
            moved += 1;
        }
    }
    Ok(moved)
}

/// Puts every item found in `dest` back into `root`; all items are attempted
/// and the errors are collected.
fn roll_back(root: &Path, dest: &Path) -> Result<(), String> {
    let mut errors = Vec::new();
    for item in GROUPS.iter().copied().flatten().rev() {
        if let Err(e) = move_item(&dest.join(item), &root.join(item)) {
            log::error!("could not put {item} back: {e}");
            errors.push(format!("{item}: {e}"));
        }
    }
    if errors.is_empty() {
        // Only removes the folder if it is empty.
        let _ = std::fs::remove_dir(dest);
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// Registers the legacy files as a "Main" profile and moves them into its
/// folder. The entry is saved with `migrating: true` *before* the first
/// rename, so a crash is resumed into the same folder by [`open_strict`].
fn migrate_legacy(root: &Path, reg: &mut Registry, now: i64) {
    let id = reg.fresh_id(Some(root));
    let mut profile = reg.new_profile(id.clone(), LEGACY_PROFILE_NAME.to_string(), now);
    profile.migrating = true;
    reg.profiles.push(profile);
    reg.last_active = Some(id);
    let idx = reg.profiles.len() - 1;
    if let Err(e) = reg.save(root) {
        log::error!("could not save the migration journal: {e}; keeping the data in place");
        if let Some(p) = reg.profiles.get_mut(idx) {
            p.migrating = false;
            p.in_root = true;
        }
        return;
    }
    finish_migration(root, reg, idx);
}

/// A migration whose registry was lost: continue into the adopted folder.
fn adopt_legacy_into(root: &Path, reg: &mut Registry, id: &str) {
    let Some(idx) = reg.profiles.iter().position(|p| p.id == id) else { return };
    let dest = root.join(PROFILES_DIR).join(id);
    let overlaps = GROUPS
        .iter()
        .copied()
        .flatten()
        .any(|item| root.join(item).exists() && dest.join(item).exists());
    if overlaps {
        log::warn!("legacy data in the root overlaps the recovered profile {id}; leaving it in place");
        return;
    }
    if let Some(p) = reg.profiles.get_mut(idx) {
        p.migrating = true;
    }
    if let Err(e) = reg.save(root) {
        log::error!("could not save the migration journal: {e}; leaving the legacy data in place");
        if let Some(p) = reg.profiles.get_mut(idx) {
            p.migrating = false;
        }
        return;
    }
    finish_migration(root, reg, idx);
}

/// Moves the remaining legacy items of the migrating profile `idx` into its
/// folder and persists `migrating: false`. If that cannot be completed the
/// moves are undone and the profile keeps using the root; if even the undo
/// fails the profile is marked broken and must not be started.
fn finish_migration(root: &Path, reg: &mut Registry, idx: usize) {
    let Some(profile) = reg.profiles.get_mut(idx) else { return };
    profile.broken = None;
    let Ok(dest) = try_data_dir(root, profile) else { return };
    let id = profile.id.clone();

    let reason = match move_groups(root, &dest) {
        Ok(n) => {
            if let Some(p) = reg.profiles.get_mut(idx) {
                p.migrating = false;
            }
            match reg.save(root) {
                Ok(()) => {
                    log::info!("migrated into {} ({n} items moved now)", dest.display());
                    return;
                }
                Err(e) => format!("could not save the registry: {e}"),
            }
        }
        Err(e) => format!("could not move the existing data into {}: {e}", dest.display()),
    };
    log::error!("migration of profile {id} failed: {reason}; undoing it");

    let Some(p) = reg.profiles.get_mut(idx) else { return };
    match roll_back(root, &dest) {
        Ok(()) => {
            p.migrating = false;
            p.in_root = true;
        }
        Err(rb) => {
            log::error!("rollback of profile {id} failed: {rb}; the profile is not started");
            p.migrating = true;
            p.broken = Some(format!("{reason}; undoing it failed: {rb}"));
        }
    }
    // Best effort: when this works, the registry matches where the files are.
    if let Err(e) = reg.save(root) {
        log::error!("could not save the registry after a failed migration: {e}");
    }
}

/// Deletes everything that belongs to `p`. For a profile living in the root
/// only its own files are removed, never the root itself. Refuses an invalid id.
pub fn remove_data(root: &Path, p: &Profile) -> io::Result<()> {
    let dir = try_data_dir(root, p)?;
    if p.in_root {
        for item in GROUPS.iter().copied().flatten().chain(SETTINGS_LEFTOVERS.iter()) {
            let path = root.join(item);
            let r = if path.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
            match r {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        }
        return Ok(());
    }
    match std::fs::remove_dir_all(dir) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0a1b2c3d";

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("waclient-profiles-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    fn profile(id: &str) -> Profile {
        Registry::default().new_profile(id.to_string(), "P".into(), 1)
    }

    fn backups(root: &Path) -> usize {
        std::fs::read_dir(root)
            .expect("read")
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("profiles.json.corrupt-"))
            .count()
    }

    /// A registry holding the journal entry of a started migration, saved.
    fn journal(root: &Path, id: &str) -> Registry {
        let mut reg = Registry::default();
        let mut p = profile(id);
        p.name = "Main".into();
        p.migrating = true;
        reg.last_active = Some(id.to_string());
        reg.profiles.push(p);
        reg.save(root).expect("journal");
        reg
    }

    fn seed_legacy(root: &Path) {
        std::fs::write(root.join("session.db"), b"session").expect("write");
        std::fs::write(root.join("session.db-wal"), b"wal").expect("write");
        std::fs::write(root.join("messages.db"), b"messages").expect("write");
        std::fs::write(root.join("messages.db-wal"), b"mwal").expect("write");
        std::fs::create_dir_all(root.join("media").join("outgoing")).expect("dir");
        std::fs::write(root.join("media").join("pic.jpg"), b"pic").expect("write");
        std::fs::write(root.join("waclient.log"), b"log").expect("write");
    }

    fn seed_profile_dir(root: &Path, id: &str, db: &str) {
        let dir = root.join("profiles").join(id);
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(dir.join(db), b"db").expect("write");
    }

    #[test]
    fn missing_file_is_empty() {
        let root = temp_root("missing");
        assert_eq!(load(&root), Registry::default());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn save_and_load_roundtrip() {
        let root = temp_root("roundtrip");
        let mut reg = Registry::default();
        let a = reg.add(Some("Work"), 10).expect("add");
        let b = reg.add(None, 20).expect("add");
        reg.set_own_jid(&a.id, "123@s.whatsapp.net");
        reg.last_active = Some(b.id.clone());
        reg.pending_delete.push("deadbeef".into());
        reg.save(&root).expect("save");
        assert!(!root.join("profiles.json.tmp").exists());
        assert_eq!(load(&root), reg);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn corrupt_file_is_backed_up_and_ignored() {
        let root = temp_root("corrupt");
        std::fs::write(registry_path(&root), "{ not json").expect("write");
        assert_eq!(load(&root), Registry::default());
        assert!(!registry_path(&root).exists());
        assert_eq!(backups(&root), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn corrupt_registry_is_backed_up_and_existing_profile_dirs_are_adopted() {
        let root = temp_root("adopt");
        std::fs::write(registry_path(&root), "{ not json").expect("write");
        seed_profile_dir(&root, "0a0a0a0a", "messages.db");
        seed_profile_dir(&root, "1b1b1b1b", "session.db");
        seed_profile_dir(&root, "2c2c2c2c", "other.txt");
        std::fs::create_dir_all(root.join("profiles").join("notanid")).expect("dir");
        let reg = open_strict(&root).expect("open");
        assert_eq!(backups(&root), 1);
        let names: Vec<_> = reg.profiles.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Recovered 0a0a0a0a", "Recovered 1b1b1b1b"]);
        assert_eq!(load(&root), reg);
        assert!(root.join("profiles").join("2c2c2c2c").is_dir());
        assert!(root.join("profiles").join("notanid").is_dir());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn adoption_respects_the_limit_and_keeps_extra_folders() {
        let root = temp_root("adopt-limit");
        for i in 0..=MAX_PROFILES {
            seed_profile_dir(&root, &format!("0000000{i}"), "messages.db");
        }
        let reg = open_strict(&root).expect("open");
        assert_eq!(reg.profiles.len(), MAX_PROFILES);
        assert_eq!(reg.unadopted, [format!("0000000{MAX_PROFILES}")]);
        assert!(root.join("profiles").join(format!("0000000{MAX_PROFILES}")).join("messages.db").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unreadable_registry_is_an_error_and_stays_untouched() {
        let root = temp_root("locked");
        // A directory cannot be read as a file: an I/O error, not corruption.
        std::fs::create_dir_all(registry_path(&root)).expect("dir");
        seed_legacy(&root);
        assert!(open_strict(&root).is_err());
        assert!(registry_path(&root).is_dir());
        assert_eq!(backups(&root), 0);
        assert!(root.join("session.db").exists());

        let reg = open(&root);
        assert!(reg.load_error.is_some());
        assert!(reg.profiles.is_empty());
        assert!(reg.save(&root).is_err());
        assert!(root.join("session.db").exists());
        assert!(registry_path(&root).is_dir());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(windows)]
    #[test]
    fn registry_held_exclusively_is_an_error_and_not_renamed() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = temp_root("sharing");
        std::fs::write(registry_path(&root), r#"{"profiles":[]}"#).expect("write");
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(registry_path(&root))
            .expect("hold");
        assert!(read_with_retry(&registry_path(&root), 2, Duration::from_millis(10)).is_err());
        assert!(load_strict(&root).is_err());
        drop(held);
        assert_eq!(backups(&root), 0);
        assert_eq!(std::fs::read(registry_path(&root)).expect("read"), br#"{"profiles":[]}"#);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn add_assigns_names_colors_and_enforces_the_limit() {
        let mut reg = Registry::default();
        for _ in 0..MAX_PROFILES {
            assert!(reg.add(None, 1).is_ok());
        }
        assert_eq!(reg.add(None, 1), Err(AddError::LimitReached));
        assert_eq!(reg.profiles[2].name, "Account 3");
        let colors: HashSet<_> = reg.profiles.iter().map(|p| &p.color).collect();
        assert_eq!(colors.len(), MAX_PROFILES);
        let ids: HashSet<_> = reg.profiles.iter().map(|p| &p.id).collect();
        assert_eq!(ids.len(), MAX_PROFILES);
        assert!(reg.profiles.iter().all(|p| valid_id(&p.id)));
    }

    #[test]
    fn ids_skip_existing_dirs_pending_deletes_and_overflow() {
        let root = temp_root("ids");
        seed_profile_dir(&root, "aaaaaaaa", "messages.db");
        let mut reg = Registry::default();
        reg.pending_delete.push("bbbbbbbb".into());
        reg.overflow.push(profile("cccccccc"));
        assert!(reg.id_taken(Some(&root), "aaaaaaaa"));
        assert!(!reg.id_taken(None, "aaaaaaaa"));
        assert!(reg.id_taken(None, "bbbbbbbb"));
        assert!(reg.id_taken(None, "cccccccc"));
        assert!(!reg.id_taken(Some(&root), "dddddddd"));
        for _ in 0..50 {
            let p = reg.add_in(&root, None, 1).expect("add");
            assert!(!["aaaaaaaa", "bbbbbbbb", "cccccccc"].contains(&p.id.as_str()));
            reg.profiles.pop();
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn remove_renumbers_and_clears_last_active() {
        let mut reg = Registry::default();
        let a = reg.add(Some("A"), 1).expect("add");
        let b = reg.add(Some("B"), 1).expect("add");
        let c = reg.add(Some("C"), 1).expect("add");
        reg.last_active = Some(b.id.clone());
        assert_eq!(reg.remove(&b.id).map(|p| p.name), Some("B".to_string()));
        assert_eq!(reg.last_active, None);
        assert_eq!(reg.get(&a.id).map(|p| p.order), Some(0));
        assert_eq!(reg.get(&c.id).map(|p| p.order), Some(1));
        assert!(reg.remove("nope").is_none());
    }

    #[test]
    fn rename_trims_and_rejects_blank() {
        let mut reg = Registry::default();
        let a = reg.add(Some("A"), 1).expect("add");
        assert!(reg.rename(&a.id, "  Personal  "));
        assert_eq!(reg.get(&a.id).map(|p| p.name.as_str()), Some("Personal"));
        assert!(!reg.rename(&a.id, "   "));
        assert!(!reg.rename("nope", "x"));
        assert!(reg.rename(&a.id, &"x".repeat(100)));
        assert_eq!(reg.get(&a.id).map(|p| p.name.chars().count()), Some(MAX_NAME_CHARS));
    }

    #[test]
    fn reorder_moves_and_clamps() {
        let mut reg = Registry::default();
        let a = reg.add(Some("A"), 1).expect("add");
        let _b = reg.add(Some("B"), 1).expect("add");
        let c = reg.add(Some("C"), 1).expect("add");
        assert!(reg.reorder(&c.id, 0));
        let names: Vec<_> = reg.profiles.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["C", "A", "B"]);
        assert!(reg.reorder(&a.id, 99));
        let names: Vec<_> = reg.profiles.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["C", "B", "A"]);
        assert!(reg.profiles.iter().enumerate().all(|(i, p)| p.order == i as u32));
        assert!(!reg.reorder("nope", 0));
    }

    #[test]
    fn reorder_persists_and_survives_reload() {
        let root = temp_root("reorder");
        let mut reg = Registry::default();
        let a = reg.add_in(&root, Some("A"), 1).expect("add");
        let b = reg.add_in(&root, Some("B"), 1).expect("add");
        let c = reg.add_in(&root, Some("C"), 1).expect("add");
        assert!(reg.reorder(&c.id, 0));
        reg.save(&root).expect("save");
        let loaded = load_strict(&root).expect("load");
        let ids: Vec<_> = loaded.profiles.iter().map(|p| p.id.clone()).collect();
        assert_eq!(ids, [c.id.clone(), a.id.clone(), b.id.clone()]);
        assert!(loaded.profiles.iter().enumerate().all(|(i, p)| p.order == i as u32));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reorder_bounds_and_noop_moves() {
        let mut reg = Registry::default();
        let a = reg.add(Some("A"), 1).expect("add");
        let b = reg.add(Some("B"), 1).expect("add");
        // Same position and an out-of-range target keep a valid permutation.
        assert!(!reg.reorder(&a.id, 0));
        assert_eq!(reg.profiles[0].id, a.id);
        assert!(reg.reorder(&a.id, usize::MAX));
        assert_eq!(reg.profiles.len(), 2);
        assert_eq!(reg.profiles[1].id, a.id);
        assert_eq!(reg.profiles[0].id, b.id);
        // Moving the last one "down" stays last; moving the first "up" stays first.
        assert!(!reg.reorder(&a.id, 2));
        assert_eq!(reg.profiles[1].id, a.id);
        assert!(!reg.reorder(&b.id, 0));
        assert_eq!(reg.profiles[0].id, b.id);
        assert!(!reg.reorder("nope", 0));
        assert_eq!(reg.profiles.len(), 2);
    }

    #[test]
    fn load_repairs_duplicates_and_stale_last_active() {
        let root = temp_root("repair");
        let json = r##"{"profiles":[
            {"id":"aaaaaaaa","name":"A","color":"#000000","created_at":1,"order":5},
            {"id":"aaaaaaaa","name":"Dup","color":"#000000","created_at":1,"order":1},
            {"id":"bbbbbbbb","name":"B","color":"#000000","created_at":1,"order":2}],
            "last_active":"gone"}"##;
        std::fs::write(registry_path(&root), json).expect("write");
        let reg = load(&root);
        let names: Vec<_> = reg.profiles.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["B", "A"]);
        assert_eq!(reg.last_active, None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn invalid_ids_are_rejected_everywhere() {
        for bad in ["..", "C:\\x", "ABCDEFGH", "123", "", "../abcdef", "0a1b2c3g", "0a1b2c3d0"] {
            assert!(!valid_id(bad), "{bad}");
        }
        assert!(valid_id("0a1b2c3d"));

        let root = temp_root("invalid-ids");
        let json = r##"{"profiles":[
            {"id":"..","name":"A","color":"#000000","created_at":1,"order":0},
            {"id":"ABCDEFGH","name":"B","color":"#000000","created_at":1,"order":1},
            {"id":"0a1b2c3d","name":"C","color":"#000000","created_at":1,"order":2}],
            "pending_delete":["..","C:\\x","deadbeef"]}"##;
        std::fs::write(registry_path(&root), json).expect("write");
        let mut reg = load_strict(&root).expect("load");
        assert_eq!(reg.profiles.len(), 1);
        assert_eq!(reg.pending_delete, ["deadbeef"]);

        let bad = profile("abc");
        assert!(!data_dir(&root, &bad).starts_with(root.join("profiles").join("abc")));
        assert!(remove_data(&root, &bad).is_err());
        assert!(root.exists());
        assert!(reg.mark_pending_delete(&root, "..").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn pending_delete_sweeps_what_it_can_and_keeps_failures() {
        let root = temp_root("pending");
        let mut reg = Registry::default();
        let a = reg.add_in(&root, Some("A"), 1).expect("add");
        let b = reg.add_in(&root, Some("B"), 1).expect("add");
        seed_profile_dir(&root, &a.id, "messages.db");
        reg.save(&root).expect("save");

        let removed = reg.mark_pending_delete(&root, &a.id).expect("mark").expect("some");
        assert_eq!(removed.id, a.id);
        assert!(reg.get(&a.id).is_none());
        assert_eq!(load(&root).pending_delete, [a.id.clone()]);
        // The data is still there until the sweep.
        assert!(root.join("profiles").join(&a.id).is_dir());

        // A queued id whose path cannot be removed as a folder stays queued.
        let stuck = "feedf00d".to_string();
        std::fs::write(root.join("profiles").join(&stuck), b"file").expect("write");
        reg.pending_delete.push(stuck.clone());
        let failed = reg.sweep_pending_deletes(&root);
        assert_eq!(failed, [stuck.clone()]);
        assert!(!root.join("profiles").join(&a.id).exists());
        assert!(reg.get(&b.id).is_some());
        assert_eq!(load(&root).pending_delete, [stuck]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn pending_deletes_run_when_opening() {
        let root = temp_root("pending-open");
        let mut reg = Registry::default();
        let a = reg.add_in(&root, Some("A"), 1).expect("add");
        seed_profile_dir(&root, &a.id, "messages.db");
        reg.save(&root).expect("save");
        reg.mark_pending_delete(&root, &a.id).expect("mark");
        let opened = open_strict(&root).expect("open");
        assert!(opened.pending_delete.is_empty());
        assert!(opened.profiles.is_empty());
        assert!(!root.join("profiles").join(&a.id).exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn color_parsing() {
        assert_eq!(parse_color("#00a884"), (0, 0xa8, 0x84));
        assert_eq!(parse_color("zzz"), (0x86, 0x96, 0xa0));
    }

    #[test]
    fn overflow_entries_are_kept_not_dropped() {
        let root = temp_root("overflow");
        let mut reg = Registry::default();
        for i in 0..=MAX_PROFILES {
            let mut p = profile(&format!("0000000{i}"));
            p.order = i as u32;
            reg.profiles.push(p);
        }
        reg.save(&root).expect("save");
        let loaded = load_strict(&root).expect("load");
        assert_eq!(loaded.profiles.len(), MAX_PROFILES);
        assert_eq!(loaded.overflow.len(), 1);
        loaded.save(&root).expect("save again");
        assert_eq!(load_strict(&root).expect("load").overflow, loaded.overflow);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn legacy_data_moves_into_a_main_profile() {
        let root = temp_root("migrate");
        seed_legacy(&root);
        let reg = open(&root);
        assert_eq!(reg.profiles.len(), 1);
        let p = &reg.profiles[0];
        assert_eq!(p.name, "Main");
        assert!(!p.in_root && !p.migrating && p.is_ready());
        assert_eq!(reg.last_active.as_deref(), Some(p.id.as_str()));
        let dir = data_dir(&root, p);
        assert_eq!(std::fs::read(dir.join("session.db")).expect("session"), b"session");
        assert!(dir.join("session.db-wal").exists());
        assert!(dir.join("messages.db").exists());
        assert!(dir.join("messages.db-wal").exists());
        assert!(dir.join("media").join("pic.jpg").exists());
        assert!(dir.join("media").join("outgoing").is_dir());
        assert!(!root.join("session.db").exists());
        assert!(!root.join("messages.db").exists());
        assert!(!root.join("media").exists());
        // The log stays at the root, and the registry is persisted.
        assert!(root.join("waclient.log").exists());
        assert_eq!(load(&root), reg);
        // A second start migrates nothing.
        assert_eq!(open(&root), reg);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn crash_after_the_journal_before_any_move_resumes_into_the_same_id() {
        let root = temp_root("resume-start");
        seed_legacy(&root);
        journal(&root, ID);
        let reg = open_strict(&root).expect("open");
        assert_eq!(reg.profiles.len(), 1);
        assert_eq!(reg.profiles[0].id, ID);
        assert!(reg.profiles[0].is_ready());
        let dir = root.join("profiles").join(ID);
        assert!(dir.join("session.db").exists() && dir.join("messages.db").exists());
        assert!(dir.join("media").join("pic.jpg").exists());
        assert!(!legacy_present(&root));
        assert_eq!(std::fs::read_dir(root.join("profiles")).expect("read").count(), 1);
        assert!(!load(&root).profiles[0].migrating);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn crash_between_a_database_and_its_wal_completes_the_group() {
        let root = temp_root("resume-group");
        seed_legacy(&root);
        journal(&root, ID);
        let dir = root.join("profiles").join(ID);
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::rename(root.join("session.db"), dir.join("session.db")).expect("pre-move");
        std::fs::rename(root.join("messages.db"), dir.join("messages.db")).expect("pre-move");
        let reg = open_strict(&root).expect("open");
        assert_eq!(reg.profiles.len(), 1);
        assert_eq!(reg.profiles[0].id, ID);
        assert!(reg.profiles[0].is_ready());
        assert_eq!(std::fs::read(dir.join("messages.db-wal")).expect("wal"), b"mwal");
        assert_eq!(std::fs::read(dir.join("session.db-wal")).expect("wal"), b"wal");
        assert_eq!(std::fs::read(dir.join("messages.db")).expect("db"), b"messages");
        assert!(!root.join("messages.db-wal").exists() && !root.join("session.db-wal").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_lost_registry_mid_migration_resumes_into_the_recovered_folder() {
        let root = temp_root("resume-lost");
        seed_legacy(&root);
        let dir = root.join("profiles").join(ID);
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::rename(root.join("session.db"), dir.join("session.db")).expect("pre-move");
        std::fs::write(registry_path(&root), "{ broken").expect("write");
        let reg = open_strict(&root).expect("open");
        assert_eq!(reg.profiles.len(), 1);
        assert_eq!(reg.profiles[0].id, ID);
        assert!(reg.profiles[0].is_ready());
        assert!(dir.join("messages.db").exists() && !legacy_present(&root));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(windows)]
    #[test]
    fn failed_move_rolls_back_and_uses_the_root() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = temp_root("rollback");
        seed_legacy(&root);
        let mut reg = journal(&root, ID);
        let dest = root.join("profiles").join(ID);
        // An open handle without delete sharing makes renaming messages.db
        // fail after the session group was already moved.
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(1 | 2)
            .open(root.join("messages.db"))
            .expect("hold");
        finish_migration(&root, &mut reg, 0);
        drop(held);
        let p = &reg.profiles[0];
        assert!(p.in_root && !p.migrating && p.broken.is_none());
        assert_eq!(data_dir(&root, p), root);
        assert_eq!(std::fs::read(root.join("session.db")).expect("session"), b"session");
        assert!(root.join("session.db-wal").exists());
        assert!(root.join("messages.db").exists());
        assert!(root.join("messages.db-wal").exists());
        assert!(root.join("media").join("pic.jpg").exists());
        assert!(!dest.join("session.db").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn failed_final_save_rolls_back_and_uses_the_root() {
        let root = temp_root("save-fails");
        seed_legacy(&root);
        let mut reg = journal(&root, ID);
        // The journal was written; now the registry path becomes unwritable.
        std::fs::remove_file(registry_path(&root)).expect("remove");
        std::fs::create_dir_all(registry_path(&root)).expect("dir");
        finish_migration(&root, &mut reg, 0);
        let p = &reg.profiles[0];
        assert!(p.in_root && !p.migrating && p.broken.is_none());
        assert_eq!(std::fs::read(root.join("session.db")).expect("session"), b"session");
        assert!(root.join("messages.db-wal").exists());
        assert!(root.join("media").join("pic.jpg").exists());
        assert!(!root.join("profiles").join(ID).join("session.db").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn failed_rollback_marks_the_profile_broken() {
        let root = temp_root("rollback-fails");
        seed_legacy(&root);
        let mut reg = journal(&root, ID);
        // The same file in both places: the move stops, and so does the undo,
        // which must never overwrite either copy.
        let dest = root.join("profiles").join(ID);
        std::fs::create_dir_all(&dest).expect("dir");
        std::fs::write(dest.join("messages.db"), b"other").expect("write");
        finish_migration(&root, &mut reg, 0);
        let p = &reg.profiles[0];
        assert!(p.broken.is_some() && p.migrating && !p.is_ready() && !p.in_root);
        assert_eq!(std::fs::read(root.join("messages.db")).expect("db"), b"messages");
        assert_eq!(std::fs::read(dest.join("messages.db")).expect("db"), b"other");
        assert_eq!(std::fs::read(root.join("session.db")).expect("db"), b"session");
        assert!(load(&root).profiles[0].broken.is_some());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn no_legacy_data_and_no_registry_gives_an_empty_registry() {
        let root = temp_root("fresh");
        assert!(open(&root).profiles.is_empty());
        assert!(!registry_path(&root).exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn remove_data_deletes_only_the_profile() {
        let root = temp_root("remove");
        let mut reg = Registry::default();
        let a = reg.add(Some("A"), 1).expect("add");
        let dir = data_dir(&root, &a);
        std::fs::create_dir_all(dir.join("media")).expect("dir");
        std::fs::write(dir.join("messages.db"), b"x").expect("write");
        std::fs::write(root.join("waclient.log"), b"log").expect("write");
        remove_data(&root, &a).expect("remove");
        assert!(!dir.exists());
        assert!(root.join("waclient.log").exists());
        // Already gone is fine.
        remove_data(&root, &a).expect("again");

        let mut legacy = a.clone();
        legacy.in_root = true;
        seed_legacy(&root);
        std::fs::write(root.join("settings.json"), b"{}").expect("write");
        std::fs::write(root.join("emoji_recents.json"), b"[]").expect("write");
        remove_data(&root, &legacy).expect("remove legacy");
        assert!(!root.join("session.db").exists());
        assert!(!root.join("media").exists());
        assert!(!root.join("settings.json").exists());
        assert!(root.join("waclient.log").exists());
        assert!(root.join("emoji_recents.json").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
