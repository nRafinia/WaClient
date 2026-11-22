//! Where the app's data lives, and moving it to another folder.
//!
//! The bootstrap folder (`%APPDATA%\WaClient`) only holds `location.json`, the
//! log and the notification icon. Everything else (profiles, databases,
//! sessions, media, emoji recents) lives under the data root that
//! `location.json` names; without that file the data root is the bootstrap
//! folder itself.
//!
//! A move is two-phase so that no data is ever lost: the running instance only
//! records a `pending_move` and restarts; the next start, before any engine
//! opens a database, copies the tree, verifies the copy, records the new root
//! atomically and only then deletes the old data.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub const LOCATION_FILE: &str = "location.json";
/// The folder created inside whatever the user picks.
pub const TARGET_SUBDIR: &str = "WaClient";
const PROBE_FILE: &str = ".waclient-write-test";
const COPY_ATTEMPTS: u32 = 4;
const REMOVE_ATTEMPTS: u32 = 4;
const RETRY_DELAY: Duration = Duration::from_millis(300);
const READ_ATTEMPTS: u32 = 3;
const READ_RETRY_DELAY: Duration = Duration::from_millis(100);
/// Free space needed on the target, relative to the data size (1.2x).
const SPACE_MARGIN_DIVISOR: u64 = 5;
const DB_EXTENSIONS: [&str; 3] = ["db", "sqlite", "sqlite3"];
/// Written into a move target before the first byte is copied. Only a target
/// carrying it may be wiped when a crashed attempt is resumed.
pub const MOVE_MARKER: &str = ".waclient-move-target";
/// Bytes compared at both ends of every copied file.
const EDGE_BYTES: u64 = 64 * 1024;
#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

static RELAUNCH: AtomicBool = AtomicBool::new(false);

/// Asks `main` to start a fresh instance once this one has shut down.
pub fn request_relaunch() {
    RELAUNCH.store(true, Ordering::SeqCst);
}

pub fn relaunch_requested() -> bool {
    RELAUNCH.load(Ordering::SeqCst)
}

/// Files that stay in the bootstrap folder and are never moved or deleted
/// (plus the move marker, which is bookkeeping and not data).
fn is_bootstrap_name(name: &str) -> bool {
    name.starts_with(LOCATION_FILE)
        || name.starts_with("waclient.log")
        || name == "waclient-icon.png"
        || name == MOVE_MARKER
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingMove {
    pub from: PathBuf,
    pub to: PathBuf,
    pub requested_at: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    #[serde(default)]
    pub data_root: Option<PathBuf>,
    #[serde(default)]
    pub pending_move: Option<PendingMove>,
    /// A fully copied, recorded move whose old data could not be deleted yet;
    /// deletion is retried at the next start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_copy: Option<PathBuf>,
}

impl Location {
    /// The folder holding all profile data.
    pub fn root(&self, bootstrap: &Path) -> PathBuf {
        match &self.data_root {
            Some(p) if p.is_absolute() => p.clone(),
            _ => bootstrap.to_path_buf(),
        }
    }

    /// The configured data root, when `location.json` names a usable one.
    pub fn configured_root(&self) -> Option<&Path> {
        self.data_root.as_deref().filter(|p| p.is_absolute())
    }

    pub fn needs_startup_work(&self) -> bool {
        self.pending_move.is_some() || self.old_copy.is_some()
    }
}

/// A configured data root that cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootError {
    pub path: PathBuf,
    pub reason: String,
}

/// The configured data root must be an existing folder; a missing one (drive
/// unplugged, folder renamed) is never recreated empty in its place.
pub fn check_root_present(loc: &Location) -> Result<(), RootError> {
    match loc.configured_root() {
        Some(root) if !root.is_dir() => {
            Err(RootError { path: root.to_path_buf(), reason: "the folder does not exist or cannot be reached".into() })
        }
        _ => Ok(()),
    }
}

/// Makes sure the folder that holds the profiles exists.
pub fn create_profiles_root(loc: &Location, bootstrap: &Path) -> Result<(), RootError> {
    // A configured root that vanished since the first check must not be
    // recreated empty by `create_dir_all`.
    check_root_present(loc)?;
    let root = loc.root(bootstrap);
    fs::create_dir_all(root.join("profiles"))
        .map_err(|e| RootError { path: root, reason: format!("cannot create the profiles folder: {e}") })
}

/// True when `root` is reachable and visibly holds WaClient data. A missing or
/// blank folder (unplugged or replaced drive) must never make old data deletable.
fn root_has_data(root: &Path) -> bool {
    root.is_dir() && (root.join("profiles.json").exists() || root.join("profiles").is_dir())
}

fn read_location_file(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let mut attempt = 1;
    loop {
        match fs::read(path) {
            Ok(b) => return Ok(Some(b)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) if attempt >= READ_ATTEMPTS => return Err(e),
            Err(_) => {
                attempt += 1;
                std::thread::sleep(READ_RETRY_DELAY);
            }
        }
    }
}

/// Reads `location.json`. A missing file is the default location. An
/// unreadable or corrupt one is an error, so the caller cannot silently run on
/// defaults while a recorded data root exists. A corrupt file is left in place
/// and a copy is kept next to it; nothing is deleted.
pub fn load(bootstrap: &Path) -> io::Result<Location> {
    let path = bootstrap.join(LOCATION_FILE);
    let bytes = match read_location_file(&path) {
        Ok(Some(b)) => b,
        Ok(None) => return Ok(Location::default()),
        Err(e) => {
            log::error!("cannot read {}: {e}", path.display());
            return Err(e);
        }
    };
    serde_json::from_slice::<Location>(&bytes).map_err(|e| {
        log::error!("{} is corrupt: {e}", path.display());
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let backup = bootstrap.join(format!("{LOCATION_FILE}.corrupt-{ts}"));
        if let Err(copy_err) = fs::write(&backup, &bytes) {
            log::error!("could not back up the corrupt {}: {copy_err}", path.display());
        }
        io::Error::new(io::ErrorKind::InvalidData, format!("{LOCATION_FILE} is corrupt: {e}"))
    })
}

/// Atomic: temp file, fsync, rename over the old file.
pub fn save(bootstrap: &Path, loc: &Location) -> io::Result<()> {
    fs::create_dir_all(bootstrap)?;
    let json = serde_json::to_vec_pretty(loc).map_err(io::Error::other)?;
    let tmp = bootstrap.join(format!("{LOCATION_FILE}.tmp"));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&json)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, bootstrap.join(LOCATION_FILE))
}

/// Like `load`, but never papers over a problem: an unreadable or corrupt
/// file is an error, so the caller cannot overwrite a recorded data root.
fn read_strict(bootstrap: &Path) -> io::Result<Option<Location>> {
    let path = bootstrap.join(LOCATION_FILE);
    match read_location_file(&path)? {
        None => Ok(None),
        Some(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{LOCATION_FILE} is unreadable: {e}"))),
    }
}

/// The old data folder of a recorded move that is still waiting to be deleted.
pub fn pending_cleanup(bootstrap: &Path) -> Option<PathBuf> {
    read_strict(bootstrap).ok().flatten().and_then(|l| l.old_copy)
}

/// Records that the next start must move the data from `from` to `to`.
pub fn begin_move(bootstrap: &Path, from: &Path, to: &Path, now: i64) -> io::Result<()> {
    let mut loc = read_strict(bootstrap)?.unwrap_or_default();
    if let Some(old) = &loc.old_copy {
        return Err(io::Error::other(format!("Previous move cleanup pending: {}", old.display())));
    }
    loc.pending_move = Some(PendingMove { from: from.to_path_buf(), to: to.to_path_buf(), requested_at: now });
    save(bootstrap, &loc)
}

// ---- paths ------------------------------------------------------------------

/// `\\?\C:\x` -> `C:\x`.
fn strip_verbatim(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => p.to_path_buf(),
    }
}

/// Canonicalizes the deepest existing ancestor and appends the rest, so paths
/// that do not exist yet compare correctly (symlinks, 8.3 names, `\\?\`).
fn resolve(p: &Path) -> PathBuf {
    let mut tail: Vec<OsString> = Vec::new();
    let mut cur = p.to_path_buf();
    loop {
        if let Ok(c) = cur.canonicalize() {
            let mut out = strip_verbatim(&c);
            for t in tail.iter().rev() {
                out.push(t);
            }
            return out;
        }
        match (cur.file_name(), cur.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name.to_owned());
                cur = parent.to_path_buf();
            }
            _ => return p.to_path_buf(),
        }
    }
}

fn comps(p: &Path) -> Vec<String> {
    p.components().map(|c| c.as_os_str().to_string_lossy().to_lowercase()).collect()
}

/// `child` equals or lies below `parent` (case-insensitive, like NTFS).
fn is_within(child: &Path, parent: &Path) -> bool {
    let c = comps(child);
    let p = comps(parent);
    c.len() >= p.len() && c[..p.len()] == p[..]
}

fn is_unc(p: &Path) -> bool {
    matches!(
        p.components().next(),
        Some(Component::Prefix(pre)) if matches!(pre.kind(), Prefix::UNC(..) | Prefix::VerbatimUNC(..))
    )
}

fn same_path(a: &Path, b: &Path) -> bool {
    comps(&resolve(a)) == comps(&resolve(b))
}

// ---- deletion safety --------------------------------------------------------

/// Why a folder must not be deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Relative,
    DriveRoot,
    NotWaClient,
    Reparse,
    Unreadable,
    OverlapsCurrentRoot,
    Bootstrap,
    ContainsBootstrap,
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Reason::Relative => "the path is not absolute",
            Reason::DriveRoot => "the path is a drive root",
            Reason::NotWaClient => "the path is not a WaClient folder",
            Reason::Reparse => "the path is a link or junction",
            Reason::Unreadable => "the path cannot be inspected",
            Reason::OverlapsCurrentRoot => "the path overlaps the current data folder",
            Reason::Bootstrap => "the path is the bootstrap folder",
            Reason::ContainsBootstrap => "the path contains the bootstrap folder",
        })
    }
}

/// What a deletion must be measured against.
#[derive(Debug, Clone, Copy)]
pub struct DeleteCtx<'a> {
    pub bootstrap: &'a Path,
    pub current_root: &'a Path,
}

fn has_waclient_leaf(p: &Path) -> bool {
    p.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(TARGET_SUBDIR))
}

/// The data location can only be changed when the bootstrap folder is the
/// regular `%APPDATA%\WaClient`; without `APPDATA` it is a relative fallback.
pub fn bootstrap_supports_move(bootstrap: &Path) -> bool {
    bootstrap.is_absolute() && has_waclient_leaf(bootstrap)
}

/// True for symlinks and junctions (any reparse point). A missing path is not one.
fn is_reparse(path: &Path) -> io::Result<bool> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if meta.file_type().is_symlink() {
        return Ok(true);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The invariants shared by every kind of deletion.
fn check_common(path: &Path, ctx: &DeleteCtx) -> Result<(), Reason> {
    if !path.is_absolute() {
        return Err(Reason::Relative);
    }
    let resolved = resolve(path);
    if path.parent().is_none() || resolved.parent().is_none() {
        return Err(Reason::DriveRoot);
    }
    if !has_waclient_leaf(path) || !has_waclient_leaf(&resolved) {
        return Err(Reason::NotWaClient);
    }
    if is_reparse(path).map_err(|_| Reason::Unreadable)? {
        return Err(Reason::Reparse);
    }
    let cur = resolve(ctx.current_root);
    if is_within(&resolved, &cur) || is_within(&cur, &resolved) {
        return Err(Reason::OverlapsCurrentRoot);
    }
    // Deleting an ancestor of the bootstrap folder would take location.json
    // and the log with it. The bootstrap folder itself is handled by the caller.
    let boot = resolve(ctx.bootstrap);
    if comps(&boot) != comps(&resolved) && is_within(&boot, &resolved) {
        return Err(Reason::ContainsBootstrap);
    }
    Ok(())
}

/// Single gate for deleting a whole folder tree. The bootstrap folder is
/// refused here: it only ever loses explicitly moved items (`remove_items`).
pub fn safe_to_delete(path: &Path, ctx: &DeleteCtx) -> Result<(), Reason> {
    if same_path(path, ctx.bootstrap) {
        return Err(Reason::Bootstrap);
    }
    check_common(path, ctx)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeleteMode {
    /// Remove the folder and everything in it.
    Tree,
    /// The bootstrap folder: remove the movable items only.
    ItemsOnly,
}

fn check_deletable(path: &Path, ctx: &DeleteCtx) -> Result<DeleteMode, Reason> {
    if same_path(path, ctx.bootstrap) {
        check_common(path, ctx)?;
        Ok(DeleteMode::ItemsOnly)
    } else {
        safe_to_delete(path, ctx)?;
        Ok(DeleteMode::Tree)
    }
}

/// Folders the data must never be moved into.
pub fn denied_dirs() -> Vec<PathBuf> {
    ["WINDIR", "SystemRoot", "ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"]
        .iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .collect()
}

/// The pure part of validation: where the data would go if the user picked
/// `picked`, or why that is not acceptable. Always `<picked>\WaClient`.
pub fn plan_target(current_root: &Path, picked: &Path, denied: &[PathBuf]) -> Result<PathBuf, String> {
    if !picked.is_absolute() {
        return Err("Choose a full folder path.".into());
    }
    if picked.components().any(|c| c == Component::ParentDir) {
        return Err("The folder path must not contain \"..\".".into());
    }
    if is_unc(picked) {
        return Err("Network folders are not supported; choose a folder on a local drive.".into());
    }
    let target = picked.join(TARGET_SUBDIR);
    let t = resolve(&target);
    let cur = resolve(current_root);
    if comps(&t) == comps(&cur) {
        return Err("The data is already stored in that folder.".into());
    }
    if is_within(&t, &cur) {
        return Err("The new folder cannot be inside the current data folder.".into());
    }
    if is_within(&cur, &t) {
        return Err("The current data folder is inside the chosen folder.".into());
    }
    if denied.iter().any(|d| is_within(&t, &resolve(d))) {
        return Err("The data cannot be stored in a system folder.".into());
    }
    Ok(target)
}

// ---- sizes ------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    rel: PathBuf,
    is_dir: bool,
    size: u64,
}

fn walk(base: &Path, rel: &Path, skip_bootstrap: bool, out: &mut Vec<Entry>) -> io::Result<()> {
    for item in fs::read_dir(base.join(rel))? {
        let item = item?;
        let name = item.file_name();
        if skip_bootstrap && rel.as_os_str().is_empty() && is_bootstrap_name(&name.to_string_lossy()) {
            continue;
        }
        let child = rel.join(&name);
        if item.file_type()?.is_dir() {
            out.push(Entry { rel: child.clone(), is_dir: true, size: 0 });
            walk(base, &child, skip_bootstrap, out)?;
        } else {
            let size = fs::metadata(item.path())?.len();
            out.push(Entry { rel: child, is_dir: false, size });
        }
    }
    Ok(())
}

fn list_tree(root: &Path, skip_bootstrap: bool) -> io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    walk(root, Path::new(""), skip_bootstrap, &mut out)?;
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(out)
}

/// Bytes of all movable data under `root`. Meant for a worker thread. Items
/// that cannot be read count as zero: the number is only informational and a
/// move re-checks everything.
pub fn data_size(root: &Path) -> u64 {
    list_tree(root, true).map_or(0, |v| v.iter().map(|e| e.size).sum())
}

pub fn fmt_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}

/// Drive plus last folder (`E:\…\WaClient`): enough for the log without
/// exposing the user name that is part of `%APPDATA%`.
pub fn short(p: &Path) -> String {
    let drive = p.components().next().map(|c| c.as_os_str().to_string_lossy().into_owned());
    let last = p.file_name().map(|n| n.to_string_lossy().into_owned());
    match (drive, last) {
        (Some(d), Some(l)) if comps(p).len() > 3 => format!("{d}\\…\\{l}"),
        _ => p.display().to_string(),
    }
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetDiskFreeSpaceExW(
        dir: *const u16,
        free_to_caller: *mut u64,
        total: *mut u64,
        total_free: *mut u64,
    ) -> i32;
}

/// Bytes available to this user on the volume holding `dir`.
#[cfg(windows)]
pub fn free_space(dir: &Path) -> io::Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let mut free = 0u64;
    let mut total = 0u64;
    let mut total_free = 0u64;
    // SAFETY: `wide` is NUL-terminated and outlives the call; the three
    // out-pointers refer to live locals.
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, &mut total, &mut total_free) };
    if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(free) }
}

#[cfg(not(windows))]
pub fn free_space(_dir: &Path) -> io::Result<u64> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "free space cannot be determined on this platform"))
}

/// No entries, ignoring the move marker and, for the bootstrap folder, the
/// bootstrap files.
fn dir_is_empty(dir: &Path, ignore_bootstrap: bool) -> io::Result<bool> {
    for item in fs::read_dir(dir)? {
        let name = item?.file_name();
        let name = name.to_string_lossy();
        if name == MOVE_MARKER || (ignore_bootstrap && is_bootstrap_name(&name)) {
            continue;
        }
        return Ok(false);
    }
    Ok(true)
}

fn probe_writable(dir: &Path) -> Result<(), String> {
    let probe = dir.join(format!("{PROBE_FILE}-{}", std::process::id()));
    let result = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .and_then(|mut f| f.write_all(b"x"))
        .and_then(|()| fs::remove_file(&probe));
    result.map_err(|e| format!("The folder is not writable: {e}"))
}

/// A validated move target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub target: PathBuf,
    pub size: u64,
}

/// Full validation of a folder the user picked. Walks the data and probes the
/// disk, so call it from a worker thread.
pub fn check_target(bootstrap: &Path, current_root: &Path, picked: &Path) -> Result<Plan, String> {
    let target = plan_target(current_root, picked, &denied_dirs())?;
    if !picked.is_dir() {
        return Err("The chosen folder does not exist.".into());
    }
    probe_writable(picked)?;
    // Moving back to the default folder: its bootstrap files do not count.
    match dir_is_empty(&target, same_path(&target, bootstrap)) {
        Ok(true) => {}
        Ok(false) => return Err(format!("{} already contains files.", target.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("Cannot use {}: {e}", target.display())),
    }
    let size = data_size(current_root);
    let free = free_space(picked).map_err(|e| format!("Cannot read the free disk space: {e}"))?;
    let needed = size.saturating_add(size / SPACE_MARGIN_DIVISOR);
    if free < needed {
        return Err(format!("Not enough free space: {} needed, {} available.", fmt_bytes(needed), fmt_bytes(free)));
    }
    Ok(Plan { target, size })
}

// ---- copy, verify, switch ---------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Copying,
    Verifying,
    Cleaning,
}

#[derive(Debug, Clone, Copy)]
pub struct Progress {
    pub phase: Phase,
    pub done: u64,
    pub total: u64,
}

impl Progress {
    /// 0.0..=1.0 across the whole startup job (copy is the bulk of it).
    pub fn fraction(&self) -> f32 {
        let part = if self.total == 0 { 1.0 } else { (self.done as f64 / self.total as f64).clamp(0.0, 1.0) };
        let v = match self.phase {
            Phase::Copying => part * 0.85,
            Phase::Verifying => 0.85 + part * 0.12,
            Phase::Cleaning => 0.97 + part * 0.03,
        };
        v as f32
    }

    pub fn label(&self) -> &'static str {
        match self.phase {
            Phase::Copying => "Copying your data…",
            Phase::Verifying => "Verifying the copy…",
            Phase::Cleaning => "Removing the old copy…",
        }
    }
}

/// The side effects of a move, replaceable in tests to inject failures.
pub struct Ops<'a> {
    pub copy_file: &'a dyn Fn(&Path, &Path) -> io::Result<u64>,
    pub check_db: &'a dyn Fn(&Path) -> Result<(), String>,
}

impl Ops<'static> {
    pub fn real() -> Self {
        Ops { copy_file: &real_copy, check_db: &sqlite_integrity }
    }
}

fn retry<T>(attempts: u32, mut f: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    let mut attempt = 1;
    loop {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) if attempt >= attempts => return Err(e),
            Err(_) => {
                attempt += 1;
                std::thread::sleep(RETRY_DELAY);
            }
        }
    }
}

fn real_copy(src: &Path, dst: &Path) -> io::Result<u64> {
    retry(COPY_ATTEMPTS, || fs::copy(src, dst))
}

/// Opens the copy read-only (so it stays byte-identical) and runs
/// `PRAGMA integrity_check`.
fn sqlite_integrity(path: &Path) -> Result<(), String> {
    use rusqlite::{Connection, OpenFlags};
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(|e| e.to_string())?;
    let verdict: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0)).map_err(|e| e.to_string())?;
    if verdict == "ok" { Ok(()) } else { Err(verdict) }
}

fn is_db(rel: &Path) -> bool {
    rel.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| DB_EXTENSIONS.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

/// Forces a copied file to disk. Read-only files (the attribute survives the
/// copy) are made writable just long enough to be flushed.
fn sync_file(path: &Path) -> io::Result<()> {
    let open = || fs::OpenOptions::new().write(true).open(path).and_then(|f| f.sync_all());
    match open() {
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            let perms = fs::metadata(path)?.permissions();
            if !perms.readonly() {
                return Err(e);
            }
            let mut writable = perms.clone();
            writable.set_readonly(false);
            fs::set_permissions(path, writable)?;
            let flushed = open();
            // The attribute is put back even if the flush failed; a second
            // failure is only reported, it must not abort a verified copy.
            if fs::set_permissions(path, perms.clone()).or_else(|_| fs::set_permissions(path, perms)).is_err() {
                log::warn!("could not restore the read-only attribute on {}", path.display());
            }
            flushed
        }
        other => other,
    }
}

/// Compares the first and last `EDGE_BYTES` of two files of `size` bytes.
fn same_edges(a: &Path, b: &Path, size: u64) -> io::Result<bool> {
    use std::io::{Read, Seek, SeekFrom};
    let mut fa = fs::File::open(a)?;
    let mut fb = fs::File::open(b)?;
    let head = size.min(EDGE_BYTES) as usize;
    let (mut ba, mut bb) = (vec![0u8; head], vec![0u8; head]);
    fa.read_exact(&mut ba)?;
    fb.read_exact(&mut bb)?;
    if ba != bb {
        return Ok(false);
    }
    if size > EDGE_BYTES {
        let tail = size.min(EDGE_BYTES);
        fa.seek(SeekFrom::Start(size - tail))?;
        fb.seek(SeekFrom::Start(size - tail))?;
        fa.read_exact(&mut ba)?;
        fb.read_exact(&mut bb)?;
        return Ok(ba == bb);
    }
    Ok(true)
}

/// Copies everything movable from `from` into `to` and verifies it (same items
/// and byte sizes, same first/last 64 KB of every file, every database passes
/// its integrity check). `to` must be empty or missing; the move marker is
/// written first and every copied file is flushed to disk. Returns the number
/// of items copied. On error the partial copy is left for the caller to remove.
pub fn copy_and_verify(
    from: &Path,
    to: &Path,
    ops: &Ops,
    progress: &mut dyn FnMut(Progress),
) -> Result<usize, String> {
    let entries = list_tree(from, true).map_err(|e| format!("cannot read {}: {e}", from.display()))?;
    let total: u64 = entries.iter().map(|e| e.size).sum();
    fs::create_dir_all(to).map_err(|e| format!("cannot create {}: {e}", to.display()))?;
    let marker = to.join(MOVE_MARKER);
    fs::write(&marker, b"WaClient data move in progress\n")
        .and_then(|()| sync_file(&marker))
        .map_err(|e| format!("cannot mark {} as a move target: {e}", to.display()))?;
    let mut done = 0u64;
    progress(Progress { phase: Phase::Copying, done, total });
    for e in &entries {
        let dst = to.join(&e.rel);
        if e.is_dir {
            fs::create_dir_all(&dst).map_err(|err| format!("cannot create {}: {err}", dst.display()))?;
        } else {
            let n = (ops.copy_file)(&from.join(&e.rel), &dst)
                .map_err(|err| format!("copying {} failed: {err}", e.rel.display()))?;
            sync_file(&dst).map_err(|err| format!("could not flush {} to disk: {err}", e.rel.display()))?;
            done += n;
            progress(Progress { phase: Phase::Copying, done, total });
        }
    }

    let copied = list_tree(to, true).map_err(|e| format!("cannot read the copy: {e}"))?;
    if copied != entries {
        let want_bytes: u64 = entries.iter().map(|e| e.size).sum();
        let got_bytes: u64 = copied.iter().map(|e| e.size).sum();
        return Err(format!(
            "verification failed: expected {} items and {want_bytes} bytes, found {} items and {got_bytes} bytes",
            entries.len(),
            copied.len()
        ));
    }
    let files: Vec<&Entry> = entries.iter().filter(|e| !e.is_dir).collect();
    let dbs: Vec<&Entry> = files.iter().copied().filter(|e| is_db(&e.rel)).collect();
    let steps = (files.len() + dbs.len()) as u64;
    let mut step = 0u64;
    for e in &files {
        progress(Progress { phase: Phase::Verifying, done: step, total: steps });
        step += 1;
        match same_edges(&from.join(&e.rel), &to.join(&e.rel), e.size) {
            Ok(true) => {}
            Ok(false) => return Err(format!("verification failed: the copy of {} differs from the original", e.rel.display())),
            Err(err) => return Err(format!("verification failed: cannot compare {}: {err}", e.rel.display())),
        }
    }
    for e in &dbs {
        progress(Progress { phase: Phase::Verifying, done: step, total: steps });
        step += 1;
        (ops.check_db)(&to.join(&e.rel))
            .map_err(|err| format!("the copy of {} failed its integrity check: {err}", e.rel.display()))?;
    }
    progress(Progress { phase: Phase::Verifying, done: 1, total: 1 });
    Ok(entries.len())
}

/// Deletes every movable item directly under `dir`; the bootstrap files and
/// the move marker stay. This is the only way the bootstrap folder is ever
/// cleaned: item by item, never as a tree. Links are unlinked, not followed.
/// Returns the number of items that could not be removed.
fn remove_items(dir: &Path) -> usize {
    let items = match fs::read_dir(dir) {
        Ok(i) => i,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return 0,
        Err(e) => {
            log::warn!("cannot list {}: {e}", dir.display());
            return 1;
        }
    };
    let mut failed = 0;
    for item in items.flatten() {
        if is_bootstrap_name(&item.file_name().to_string_lossy()) {
            continue;
        }
        let path = item.path();
        let removed = match is_reparse(&path) {
            Ok(true) => retry(REMOVE_ATTEMPTS, || fs::remove_dir(&path).or_else(|_| fs::remove_file(&path))),
            Ok(false) if item.file_type().is_ok_and(|t| t.is_dir()) => retry(REMOVE_ATTEMPTS, || fs::remove_dir_all(&path)),
            Ok(false) => retry(REMOVE_ATTEMPTS, || fs::remove_file(&path)),
            Err(e) => Err(e),
        };
        if let Err(e) = removed {
            log::warn!("could not remove {}: {e}", path.display());
            failed += 1;
        }
    }
    failed
}

/// Deletes `path` according to `mode` (already vetted by `check_deletable`).
/// Returns the number of items that could not be removed.
fn remove_vetted(path: &Path, mode: DeleteMode) -> usize {
    match mode {
        DeleteMode::ItemsOnly => remove_items(path),
        DeleteMode::Tree => match retry(REMOVE_ATTEMPTS, || fs::remove_dir_all(path)) {
            Ok(()) => 0,
            Err(e) if e.kind() == io::ErrorKind::NotFound => 0,
            Err(e) => {
                log::warn!("could not remove {}: {e}", path.display());
                1
            }
        },
    }
}

/// What the start-up job did, for the user-facing banner.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct MoveReport {
    pub moved: Option<PathBuf>,
    pub error: Option<String>,
    /// Old data folders that still contain data after a recorded move.
    pub leftover: Vec<PathBuf>,
    /// Deletions that were deliberately not done, with the reason.
    pub notices: Vec<String>,
}

fn clear_pending(bootstrap: &Path, loc: &mut Location) {
    loc.pending_move = None;
    if let Err(e) = save(bootstrap, loc) {
        log::error!("could not clear the pending move: {e}");
    }
}

fn validate_pending(current: &Path, p: &PendingMove) -> Result<(), String> {
    if !p.from.is_absolute() || !p.to.is_absolute() {
        return Err("the recorded move has a relative path".into());
    }
    let leaf_ok = p.to.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(TARGET_SUBDIR));
    if !leaf_ok {
        return Err("the recorded target is not a WaClient folder".into());
    }
    let (from, to) = (resolve(&p.from), resolve(&p.to));
    if comps(&from) != comps(&resolve(current)) {
        return Err("the recorded move does not start at the current data folder".into());
    }
    if is_within(&to, &from) || is_within(&from, &to) {
        return Err("the source and target folders overlap".into());
    }
    Ok(())
}

/// Empties a vetted move target (see `check_deletable`) and removes its marker.
fn wipe_target(to: &Path, mode: DeleteMode) -> Result<(), String> {
    if remove_vetted(to, mode) > 0 {
        return Err(format!("some items in {} could not be removed", to.display()));
    }
    if mode == DeleteMode::ItemsOnly {
        if let Err(e) = fs::remove_file(to.join(MOVE_MARKER)) {
            if e.kind() != io::ErrorKind::NotFound {
                return Err(format!("could not remove the move marker in {}: {e}", to.display()));
            }
        }
    }
    Ok(())
}

/// Readies the target for a copy: a missing or empty folder is fine, one that
/// carries our marker is the leftover of a crashed attempt and is emptied, and
/// anything else (a folder with someone's files) is refused untouched.
fn prepare_target(to: &Path, ctx: &DeleteCtx) -> Result<(), String> {
    match fs::symlink_metadata(to) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("cannot inspect {}: {e}", to.display())),
        Ok(_) => {}
    }
    let mode = check_deletable(to, ctx).map_err(|r| format!("cannot use {} as the target: {r}", to.display()))?;
    let empty = dir_is_empty(to, mode == DeleteMode::ItemsOnly)
        .map_err(|e| format!("cannot read {}: {e}", to.display()))?;
    if empty {
        return Ok(());
    }
    if !to.join(MOVE_MARKER).is_file() {
        return Err(format!("{} already contains files that do not come from an earlier move", to.display()));
    }
    wipe_target(to, mode).map_err(|e| format!("cannot empty {}: {e}", to.display()))
}

/// Removes the partial copy after a failed attempt. Only a target that carries
/// our marker is touched, and only after the deletion checks.
fn remove_target(to: &Path, ctx: &DeleteCtx) {
    if !to.join(MOVE_MARKER).is_file() {
        return;
    }
    let result = check_deletable(to, ctx)
        .map_err(|r| r.to_string())
        .and_then(|mode| wipe_target(to, mode));
    if let Err(e) = result {
        log::error!("could not remove the partial copy {}: {e}", to.display());
    }
}

fn remove_marker(to: &Path) {
    if let Err(e) = fs::remove_file(to.join(MOVE_MARKER)) {
        if e.kind() != io::ErrorKind::NotFound {
            log::warn!("could not remove the move marker in {}: {e}", to.display());
        }
    }
}

fn run_move(
    bootstrap: &Path,
    loc: &mut Location,
    p: &PendingMove,
    ops: &Ops,
    progress: &mut dyn FnMut(Progress),
) -> Result<(), String> {
    let current = loc.root(bootstrap);
    if let Err(e) = validate_pending(&current, p) {
        clear_pending(bootstrap, loc);
        return Err(e);
    }
    if !p.from.is_dir() {
        clear_pending(bootstrap, loc);
        return Err("the old data folder no longer exists".into());
    }
    let ctx = DeleteCtx { bootstrap, current_root: &current };
    if let Err(e) = prepare_target(&p.to, &ctx) {
        clear_pending(bootstrap, loc);
        return Err(e);
    }
    let items = match copy_and_verify(&p.from, &p.to, ops, progress) {
        Ok(n) => n,
        Err(e) => {
            remove_target(&p.to, &ctx);
            clear_pending(bootstrap, loc);
            return Err(e);
        }
    };
    // Nothing to delete later when there was no data to begin with.
    let next = Location { data_root: Some(p.to.clone()), pending_move: None, old_copy: (items > 0).then(|| p.from.clone()) };
    if let Err(e) = save(bootstrap, &next) {
        remove_target(&p.to, &ctx);
        clear_pending(bootstrap, loc);
        return Err(format!("could not record the new location: {e}"));
    }
    remove_marker(&p.to);
    *loc = next;
    Ok(())
}

/// Deletes the recorded old copy, but only when the new root is reachable and
/// holds data, `location.json` on disk confirms the new root, and the old
/// path passes every deletion check. Returns it when something is left over.
fn sweep_old_copy(
    bootstrap: &Path,
    loc: &mut Location,
    progress: &mut dyn FnMut(Progress),
    notices: &mut Vec<String>,
) -> Option<PathBuf> {
    let old = loc.old_copy.clone()?;
    let root = loc.root(bootstrap);
    if !root_has_data(&root) {
        log::warn!("not removing the old copy: the data folder {} is unavailable or empty", root.display());
        notices.push(format!(
            "The old copy at {} was kept because the data folder {} is not available.",
            old.display(),
            root.display()
        ));
        return None;
    }
    // The in-memory state is not enough: the deletion is irreversible, so the
    // file on disk must say that the data now lives in `root`.
    let confirmed = match read_strict(bootstrap) {
        Ok(Some(disk)) => disk.data_root.as_deref().is_some_and(|d| same_path(d, &root)) && disk.old_copy.as_ref() == Some(&old),
        _ => false,
    };
    if !confirmed {
        log::error!("not removing the old copy {}: {LOCATION_FILE} does not confirm the new data folder", old.display());
        notices.push(format!(
            "The old copy at {} was kept because the saved data location could not be confirmed.",
            old.display()
        ));
        return Some(old);
    }
    let ctx = DeleteCtx { bootstrap, current_root: &root };
    let mode = match check_deletable(&old, &ctx) {
        Ok(m) => m,
        Err(reason) => {
            log::error!("ignoring the old-copy record {}: {reason}", old.display());
            notices.push(format!("The recorded old copy {} was not removed: {reason}.", old.display()));
            loc.old_copy = None;
            if let Err(e) = save(bootstrap, loc) {
                log::error!("could not clear the old-copy record: {e}");
            }
            return None;
        }
    };
    progress(Progress { phase: Phase::Cleaning, done: 0, total: 1 });
    if remove_vetted(&old, mode) == 0 {
        loc.old_copy = None;
        if let Err(e) = save(bootstrap, loc) {
            log::error!("could not clear the old-copy record: {e}");
        }
        None
    } else {
        Some(old)
    }
}

/// The start-up job: finishes or retries a recorded move and removes a
/// leftover old copy. Runs before any engine opens a database, and never
/// while the configured data root is missing.
pub fn run_startup(bootstrap: &Path, ops: &Ops, progress: &mut dyn FnMut(Progress)) -> MoveReport {
    let mut report = MoveReport::default();
    let mut loc = match load(bootstrap) {
        Ok(l) => l,
        Err(e) => {
            report.error = Some(format!("{LOCATION_FILE} could not be read ({e})"));
            return report;
        }
    };
    if let Err(e) = check_root_present(&loc) {
        report.error = Some(format!("the data folder {} is unavailable", e.path.display()));
        return report;
    }
    if let Some(old) = sweep_old_copy(bootstrap, &mut loc, progress, &mut report.notices) {
        report.leftover.push(old);
    }
    if let Some(pending) = loc.pending_move.clone() {
        match run_move(bootstrap, &mut loc, &pending, ops, progress) {
            Ok(()) => report.moved = Some(pending.to),
            Err(e) => report.error = Some(e),
        }
        if report.moved.is_some() {
            if let Some(old) = sweep_old_copy(bootstrap, &mut loc, progress, &mut report.notices) {
                report.leftover.push(old);
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct Temp(PathBuf);

    impl Temp {
        fn new(tag: &str) -> Temp {
            let dir = std::env::temp_dir().join(format!("waclient-loc-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("temp dir");
            Temp(dir)
        }

        fn path(&self, rel: &str) -> PathBuf {
            self.0.join(rel)
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Read-only: true when `location.json` exists and records a pending move.
    fn peek_pending(bootstrap: &Path) -> bool {
        fs::read(bootstrap.join(LOCATION_FILE))
            .ok()
            .and_then(|b| serde_json::from_slice::<Location>(&b).ok())
            .is_some_and(|l| l.pending_move.is_some())
    }

    fn write(path: &Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(path, bytes).expect("write");
    }

    fn make_db(path: &Path) {
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        let conn = rusqlite::Connection::open(path).expect("open");
        conn.execute_batch("CREATE TABLE t(a TEXT); INSERT INTO t VALUES ('hello');").expect("fill");
    }

    /// A data root with two profiles, databases and media, plus bootstrap files.
    fn populate(root: &Path) {
        write(&root.join("profiles.json"), b"{}");
        write(&root.join("emoji_recents.json"), b"[]");
        make_db(&root.join("profiles/aaaa1111/messages.db"));
        make_db(&root.join("profiles/bbbb2222/session.db"));
        write(&root.join("profiles/aaaa1111/media/img/a.jpg"), &[7u8; 5000]);
        write(&root.join("profiles/bbbb2222/media/outgoing/v.ogg"), &[1u8; 300]);
        fs::create_dir_all(root.join("profiles/bbbb2222/empty")).expect("empty dir");
        write(&root.join("waclient.log"), b"log");
        write(&root.join("waclient-icon.png"), b"png");
    }

    fn real_ops_progress() -> impl FnMut(Progress) {
        |_| {}
    }

    fn assert_source_intact(root: &Path) {
        assert!(root.join("profiles.json").is_file());
        assert!(root.join("profiles/aaaa1111/messages.db").is_file());
        assert_eq!(fs::read(root.join("profiles/aaaa1111/media/img/a.jpg")).expect("media").len(), 5000);
        assert!(root.join("profiles/bbbb2222/empty").is_dir());
    }

    #[test]
    fn location_roundtrip_and_default() {
        let t = Temp::new("roundtrip");
        assert_eq!(load(&t.0).expect("load"), Location::default());
        assert_eq!(load(&t.0).expect("load").root(&t.0), t.0);
        let loc = Location {
            data_root: Some(t.path("elsewhere")),
            pending_move: Some(PendingMove { from: t.path("a"), to: t.path("b"), requested_at: 5 }),
            old_copy: None,
        };
        save(&t.0, &loc).expect("save");
        assert_eq!(load(&t.0).expect("load"), loc);
        assert!(!t.path("location.json.tmp").exists());
        assert!(peek_pending(&t.0));
    }

    #[test]
    fn corrupt_location_is_an_error_and_is_backed_up_without_deleting() {
        let t = Temp::new("corrupt");
        write(&t.path(LOCATION_FILE), b"{ not json");
        let err = load(&t.0).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read(t.path(LOCATION_FILE)).expect("original stays"), b"{ not json");
        let backups: Vec<_> = fs::read_dir(&t.0)
            .expect("list")
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("location.json.corrupt-"))
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(fs::read(backups[0].path()).expect("backup"), b"{ not json");
        // The error is stable: a retry does not turn into defaults.
        assert!(load(&t.0).is_err());
        let report = run_startup(&t.0, &Ops::real(), &mut real_ops_progress());
        assert!(report.error.as_deref().is_some_and(|e| e.contains("could not be read")), "{report:?}");
        assert!(!peek_pending(&t.0));
    }

    #[test]
    fn create_profiles_root_never_recreates_a_missing_configured_root() {
        let t = Temp::new("noroot");
        let boot = t.path("appdata/WaClient");
        let missing = t.path("gone/WaClient");
        let loc = Location { data_root: Some(missing.clone()), ..Location::default() };
        let err = create_profiles_root(&loc, &boot).unwrap_err();
        assert_eq!(err.path, missing);
        assert!(!t.path("gone").exists());
        // Present root, and the default root, get their profiles folder.
        fs::create_dir_all(&missing).expect("root");
        create_profiles_root(&loc, &boot).expect("present");
        assert!(missing.join("profiles").is_dir());
        create_profiles_root(&Location::default(), &boot).expect("default");
        assert!(boot.join("profiles").is_dir());
    }

    #[test]
    fn an_ancestor_of_the_bootstrap_folder_is_never_deletable() {
        let t = Temp::new("ancestor");
        let outer = t.path("WaClient");
        let boot = outer.join("inner/WaClient");
        fs::create_dir_all(&boot).expect("boot");
        let cur = t.path("cur/WaClient");
        fs::create_dir_all(&cur).expect("cur");
        let ctx = DeleteCtx { bootstrap: &boot, current_root: &cur };
        assert_eq!(safe_to_delete(&outer, &ctx), Err(Reason::ContainsBootstrap));
        // The bootstrap folder itself is still vetted for item-only cleaning.
        assert_eq!(check_deletable(&boot, &ctx), Ok(DeleteMode::ItemsOnly));
    }

    #[test]
    fn moving_is_offered_only_for_a_standard_bootstrap_folder() {
        assert!(bootstrap_supports_move(&std::env::temp_dir().join("WaClient")));
        assert!(!bootstrap_supports_move(Path::new("data")));
        assert!(!bootstrap_supports_move(&std::env::temp_dir().join("other")));
    }

    #[test]
    fn relative_data_root_is_ignored() {
        let t = Temp::new("relroot");
        let loc = Location { data_root: Some(PathBuf::from("relative")), ..Location::default() };
        assert_eq!(loc.root(&t.0), t.0);
    }

    #[test]
    fn plan_target_rules() {
        let t = Temp::new("plan");
        let current = t.path("cur/WaClient");
        fs::create_dir_all(&current).expect("cur");
        let other = t.path("other");
        fs::create_dir_all(&other).expect("other");

        assert_eq!(plan_target(&current, &other, &[]).expect("ok"), other.join("WaClient"));
        assert!(plan_target(&current, Path::new("relative"), &[]).is_err());
        // Picking the folder that already is the data folder's parent.
        assert!(plan_target(&current, &t.path("cur"), &[]).unwrap_err().contains("already"));
        // Inside the current root, and the current root inside the target.
        assert!(plan_target(&current, &current.join("sub"), &[]).unwrap_err().contains("inside the current"));
        let nested = t.path("cur/WaClient/x/y");
        assert!(plan_target(&nested, &t.path("cur"), &[]).unwrap_err().contains("inside the chosen"));
        // `..` tricks and case differences.
        assert!(plan_target(&current, &other.join("..").join("other"), &[]).is_err());
        let upper = PathBuf::from(t.path("CUR").to_string_lossy().to_uppercase());
        assert!(plan_target(&current, &upper, &[]).is_err());
        // System folders.
        let denied = vec![other.clone()];
        assert!(plan_target(&current, &other.join("deep"), &denied).unwrap_err().contains("system"));
        assert!(plan_target(&current, &t.path("fine"), &denied).is_ok());
        // Network paths.
        assert!(plan_target(&current, Path::new(r"\\server\share\data"), &[]).is_err() || !cfg!(windows));
    }

    #[test]
    fn drive_root_gets_a_subfolder() {
        let current = std::env::temp_dir().join("waclient-loc-cur-root/WaClient");
        let root = std::env::temp_dir().ancestors().last().expect("root").to_path_buf();
        let target = plan_target(&current, &root, &[]).expect("drive root allowed");
        assert_eq!(target, root.join("WaClient"));
    }

    #[test]
    fn size_ignores_bootstrap_files() {
        let t = Temp::new("size");
        write(&t.path("profiles/p/x.bin"), &[0u8; 1000]);
        write(&t.path("waclient.log"), &[0u8; 999]);
        write(&t.path("location.json"), &[0u8; 999]);
        assert_eq!(data_size(&t.0), 1000);
        assert_eq!(data_size(&t.path("missing")), 0);
        assert_eq!(fmt_bytes(1536 * 1024 * 1024), "1.5 GB");
        assert_eq!(fmt_bytes(12), "12 B");
    }

    #[test]
    fn free_space_is_reported_and_check_target_accepts_a_good_folder() {
        let t = Temp::new("check");
        let current = t.path("cur");
        populate(&current);
        let picked = t.path("picked");
        fs::create_dir_all(&picked).expect("picked");
        if cfg!(windows) {
            assert!(free_space(&picked).expect("free") > 0);
            let boot = t.path("boot/WaClient");
            let plan = check_target(&boot, &current, &picked).expect("valid");
            assert_eq!(plan.target, picked.join("WaClient"));
            assert!(plan.size > 5000);
            assert!(!picked.join(format!("{PROBE_FILE}-{}", std::process::id())).exists());
            // Non-empty target is refused.
            write(&picked.join("WaClient/x.txt"), b"x");
            assert!(check_target(&boot, &current, &picked).unwrap_err().contains("already contains"));
        }
        assert!(check_target(&t.path("boot"), &current, &t.path("nope")).is_err());
    }

    #[test]
    fn end_to_end_move_from_the_bootstrap_folder() {
        let t = Temp::new("e2e");
        let bootstrap = t.path("appdata/WaClient");
        let to = t.path("bigdisk/WaClient");
        populate(&bootstrap);
        begin_move(&bootstrap, &bootstrap, &to, 1).expect("begin");
        assert!(peek_pending(&bootstrap));

        let mut seen = Vec::new();
        let report = run_startup(&bootstrap, &Ops::real(), &mut |p| seen.push(p.phase));
        assert_eq!(report, MoveReport { moved: Some(to.clone()), ..MoveReport::default() });
        assert!(seen.contains(&Phase::Copying) && seen.contains(&Phase::Verifying) && seen.contains(&Phase::Cleaning));

        let loc = load(&bootstrap).expect("load");
        assert_eq!(loc, Location { data_root: Some(to.clone()), pending_move: None, old_copy: None });
        assert_source_intact(&to);
        assert_eq!(fs::read(to.join("profiles/bbbb2222/media/outgoing/v.ogg")).expect("ogg").len(), 300);
        // Old data is gone, bootstrap files stay and never move.
        assert!(!bootstrap.join("profiles").exists());
        assert!(!bootstrap.join("profiles.json").exists());
        assert!(bootstrap.join("waclient.log").is_file());
        assert!(bootstrap.join("waclient-icon.png").is_file());
        assert!(!to.join("waclient.log").exists());
        assert!(!peek_pending(&bootstrap));

        // The moved databases still open and hold their rows.
        let conn = rusqlite::Connection::open(to.join("profiles/aaaa1111/messages.db")).expect("open");
        let v: String = conn.query_row("SELECT a FROM t", [], |r| r.get(0)).expect("row");
        assert_eq!(v, "hello");
        drop(conn);

        // And it can be moved again, from a custom root: the old folder disappears entirely.
        let again = t.path("third/WaClient");
        begin_move(&bootstrap, &load(&bootstrap).expect("load").root(&bootstrap), &again, 2).expect("begin2");
        let report = run_startup(&bootstrap, &Ops::real(), &mut real_ops_progress());
        assert_eq!(report.moved, Some(again.clone()));
        assert!(!to.exists());
        assert_source_intact(&again);
        assert_eq!(load(&bootstrap).expect("load").root(&bootstrap), again);
    }

    #[test]
    fn failure_mid_copy_cleans_the_target_and_keeps_the_source() {
        let t = Temp::new("fail");
        let bootstrap = t.path("src");
        let to = t.path("dst/WaClient");
        populate(&bootstrap);
        begin_move(&bootstrap, &bootstrap, &to, 1).expect("begin");

        let calls = Cell::new(0);
        let failing = |s: &Path, d: &Path| -> io::Result<u64> {
            calls.set(calls.get() + 1);
            if calls.get() == 3 {
                return Err(io::Error::other("disk full"));
            }
            fs::copy(s, d)
        };
        let ops = Ops { copy_file: &failing, check_db: &sqlite_integrity };
        let report = run_startup(&bootstrap, &ops, &mut real_ops_progress());
        assert!(report.moved.is_none());
        assert!(report.error.as_deref().is_some_and(|e| e.contains("disk full")), "{report:?}");
        assert!(!to.exists());
        assert_source_intact(&bootstrap);
        let loc = load(&bootstrap).expect("load");
        assert_eq!(loc.pending_move, None);
        assert_eq!(loc.root(&bootstrap), bootstrap);
        assert_eq!(loc.old_copy, None);
    }

    #[test]
    fn verification_mismatch_rolls_back() {
        let t = Temp::new("mismatch");
        let bootstrap = t.path("src");
        let to = t.path("dst/WaClient");
        populate(&bootstrap);
        begin_move(&bootstrap, &bootstrap, &to, 1).expect("begin");
        let truncating = |s: &Path, d: &Path| -> io::Result<u64> {
            let bytes = fs::read(s)?;
            let keep = if s.extension().is_some_and(|e| e == "jpg") { bytes.len() - 1 } else { bytes.len() };
            fs::write(d, &bytes[..keep])?;
            Ok(keep as u64)
        };
        let ops = Ops { copy_file: &truncating, check_db: &sqlite_integrity };
        let report = run_startup(&bootstrap, &ops, &mut real_ops_progress());
        assert!(report.error.as_deref().is_some_and(|e| e.contains("verification failed")), "{report:?}");
        assert!(!to.exists());
        assert_source_intact(&bootstrap);
        assert_eq!(load(&bootstrap).expect("load").pending_move, None);
    }

    #[test]
    fn corrupt_database_copy_rolls_back() {
        let t = Temp::new("baddb");
        let bootstrap = t.path("src");
        let to = t.path("dst/WaClient");
        populate(&bootstrap);
        write(&bootstrap.join("profiles/cccc3333/broken.db"), b"this is not a sqlite database at all, no sir");
        begin_move(&bootstrap, &bootstrap, &to, 1).expect("begin");
        let report = run_startup(&bootstrap, &Ops::real(), &mut real_ops_progress());
        assert!(report.error.as_deref().is_some_and(|e| e.contains("integrity")), "{report:?}");
        assert!(!to.exists());
        assert_source_intact(&bootstrap);
    }

    #[test]
    fn crash_before_the_switch_resumes_into_an_emptied_target() {
        let t = Temp::new("resume");
        let bootstrap = t.path("appdata/WaClient");
        let to = t.path("dst/WaClient");
        populate(&bootstrap);
        begin_move(&bootstrap, &bootstrap, &to, 1).expect("begin");
        // What a crash after copying but before recording would leave behind.
        write(&to.join("profiles/stale/leftover.bin"), b"partial");
        write(&to.join("profiles.json"), b"old partial");
        write(&to.join(MOVE_MARKER), b"marker");
        assert!(peek_pending(&bootstrap));

        let report = run_startup(&bootstrap, &Ops::real(), &mut real_ops_progress());
        assert_eq!(report.moved, Some(to.clone()));
        assert!(!to.join("profiles/stale").exists());
        assert_eq!(fs::read(to.join("profiles.json")).expect("registry"), b"{}");
        assert_source_intact(&to);
        assert!(!to.join(MOVE_MARKER).exists(), "marker is removed after the switch");
    }

    #[test]
    fn unmarked_non_empty_target_is_refused_and_left_alone() {
        let t = Temp::new("nomarker");
        let bootstrap = t.path("appdata/WaClient");
        let to = t.path("dst/WaClient");
        populate(&bootstrap);
        write(&to.join("precious.txt"), b"keep me");
        begin_move(&bootstrap, &bootstrap, &to, 1).expect("begin");
        let report = run_startup(&bootstrap, &Ops::real(), &mut real_ops_progress());
        assert!(report.moved.is_none());
        assert!(report.error.as_deref().is_some_and(|e| e.contains("already contains")), "{report:?}");
        assert!(to.join("precious.txt").is_file());
        assert_source_intact(&bootstrap);
        assert_eq!(load(&bootstrap).expect("load").pending_move, None);
    }

    #[test]
    fn marker_is_written_before_the_first_copy_and_removed_after_a_failure_cleanup() {
        let t = Temp::new("marker");
        let bootstrap = t.path("appdata/WaClient");
        let to = t.path("dst/WaClient");
        populate(&bootstrap);
        begin_move(&bootstrap, &bootstrap, &to, 1).expect("begin");
        let seen = Cell::new(false);
        let probing = |s: &Path, d: &Path| -> io::Result<u64> {
            seen.set(to.join(MOVE_MARKER).is_file());
            if seen.get() { Err(io::Error::other("stop here")) } else { fs::copy(s, d) }
        };
        let ops = Ops { copy_file: &probing, check_db: &sqlite_integrity };
        let report = run_startup(&bootstrap, &ops, &mut real_ops_progress());
        assert!(seen.get(), "marker existed when the first file was copied");
        assert!(report.error.is_some());
        assert!(!to.exists());
        assert_source_intact(&bootstrap);
    }

    #[test]
    fn a_changed_last_byte_is_caught_even_when_the_size_matches() {
        let t = Temp::new("edges");
        let bootstrap = t.path("appdata/WaClient");
        let to = t.path("dst/WaClient");
        populate(&bootstrap);
        write(&bootstrap.join("profiles/aaaa1111/media/big.bin"), &vec![9u8; 200_000]);
        begin_move(&bootstrap, &bootstrap, &to, 1).expect("begin");
        let flipping = |s: &Path, d: &Path| -> io::Result<u64> {
            let n = fs::copy(s, d)?;
            if s.file_name().is_some_and(|f| f == "big.bin") {
                let mut bytes = fs::read(d)?;
                if let Some(last) = bytes.last_mut() {
                    *last ^= 0xff;
                }
                fs::write(d, bytes)?;
            }
            Ok(n)
        };
        let ops = Ops { copy_file: &flipping, check_db: &sqlite_integrity };
        let report = run_startup(&bootstrap, &ops, &mut real_ops_progress());
        assert!(report.error.as_deref().is_some_and(|e| e.contains("differs")), "{report:?}");
        assert!(!to.exists());
        assert_source_intact(&bootstrap);
    }

    #[test]
    fn move_away_and_back_to_the_default_folder() {
        let t = Temp::new("moveback");
        let bootstrap = t.path("appdata/WaClient");
        let away = t.path("bigdisk/WaClient");
        populate(&bootstrap);
        begin_move(&bootstrap, &bootstrap, &away, 1).expect("away");
        let report = run_startup(&bootstrap, &Ops::real(), &mut real_ops_progress());
        assert_eq!(report.moved, Some(away.clone()), "{report:?}");
        assert!(!bootstrap.join("profiles").exists());

        // Moving back: the bootstrap files already in the target do not count.
        let picked = t.path("appdata");
        let plan = check_target(&bootstrap, &away, &picked).expect("default folder is a valid target");
        assert_eq!(plan.target, bootstrap);
        begin_move(&bootstrap, &away, &bootstrap, 2).expect("back");
        let report = run_startup(&bootstrap, &Ops::real(), &mut real_ops_progress());
        assert_eq!(report, MoveReport { moved: Some(bootstrap.clone()), ..MoveReport::default() });
        assert_source_intact(&bootstrap);
        assert!(!away.exists(), "the old custom folder is removed");
        assert!(bootstrap.join("waclient.log").is_file() && bootstrap.join("waclient-icon.png").is_file());
        assert!(!bootstrap.join(MOVE_MARKER).exists());
        let loc = load(&bootstrap).expect("load");
        assert_eq!(loc.root(&bootstrap), bootstrap);
        assert_eq!(loc.old_copy, None);
        // And away again works from there.
        begin_move(&bootstrap, &bootstrap, &away, 3).expect("away again");
        let report = run_startup(&bootstrap, &Ops::real(), &mut real_ops_progress());
        assert_eq!(report.moved, Some(away.clone()), "{report:?}");
        assert_source_intact(&away);
        assert!(bootstrap.join(LOCATION_FILE).is_file());
    }

    #[test]
    fn failed_move_back_removes_only_what_it_copied_into_the_bootstrap_folder() {
        let t = Temp::new("backfail");
        let bootstrap = t.path("appdata/WaClient");
        let away = t.path("bigdisk/WaClient");
        populate(&away);
        write(&bootstrap.join("waclient.log"), b"log");
        save(&bootstrap, &Location { data_root: Some(away.clone()), ..Location::default() }).expect("save");
        begin_move(&bootstrap, &away, &bootstrap, 1).expect("back");
        let calls = Cell::new(0);
        let failing = |s: &Path, d: &Path| -> io::Result<u64> {
            calls.set(calls.get() + 1);
            if calls.get() == 3 { Err(io::Error::other("disk full")) } else { fs::copy(s, d) }
        };
        let ops = Ops { copy_file: &failing, check_db: &sqlite_integrity };
        let report = run_startup(&bootstrap, &ops, &mut real_ops_progress());
        assert!(report.error.is_some());
        assert_source_intact(&away);
        assert!(bootstrap.join("waclient.log").is_file());
        assert!(bootstrap.join(LOCATION_FILE).is_file());
        assert!(!bootstrap.join("profiles").exists());
        assert!(!bootstrap.join(MOVE_MARKER).exists());
        assert_eq!(load(&bootstrap).expect("load").root(&bootstrap), away);
    }

    #[test]
    fn safe_to_delete_rejects_everything_that_is_not_a_plain_waclient_folder() {
        let t = Temp::new("safe");
        let boot = t.path("appdata/WaClient");
        let cur = t.path("cur/WaClient");
        fs::create_dir_all(&boot).expect("boot");
        fs::create_dir_all(&cur).expect("cur");
        let ctx = DeleteCtx { bootstrap: &boot, current_root: &cur };

        let docs = t.path("Users/me/Documents");
        fs::create_dir_all(&docs).expect("docs");
        assert_eq!(safe_to_delete(&docs, &ctx), Err(Reason::NotWaClient));
        assert_eq!(safe_to_delete(Path::new("relative/WaClient"), &ctx), Err(Reason::Relative));
        let drive = std::env::temp_dir().ancestors().last().expect("root").to_path_buf();
        assert_eq!(safe_to_delete(&drive, &ctx), Err(Reason::DriveRoot));
        assert_eq!(safe_to_delete(&boot, &ctx), Err(Reason::Bootstrap));
        assert_eq!(safe_to_delete(&cur, &ctx), Err(Reason::OverlapsCurrentRoot));
        assert_eq!(safe_to_delete(&cur.join("sub").join("WaClient"), &ctx), Err(Reason::OverlapsCurrentRoot));
        assert_eq!(safe_to_delete(&t.path("cur"), &ctx), Err(Reason::NotWaClient));

        let fine = t.path("old/waclient");
        fs::create_dir_all(&fine).expect("fine");
        assert_eq!(safe_to_delete(&fine, &ctx), Ok(()));
        assert_eq!(safe_to_delete(&t.path("missing/WaClient"), &ctx), Ok(()));
        // The bootstrap folder can be cleaned item by item, never as a tree.
        assert_eq!(check_deletable(&boot, &DeleteCtx { bootstrap: &boot, current_root: &fine }), Ok(DeleteMode::ItemsOnly));
        assert_eq!(check_deletable(&fine, &ctx), Ok(DeleteMode::Tree));
    }

    /// cmd reads a `/` inside a path as the start of a switch.
    #[cfg(windows)]
    fn backslashed(p: &Path) -> String {
        p.to_string_lossy().replace('/', "\\")
    }

    #[cfg(windows)]
    #[test]
    fn a_junction_is_never_deleted_and_its_target_survives() {
        let t = Temp::new("junction");
        let real = t.path("real/WaClient");
        write(&real.join("precious.txt"), b"keep");
        let parent = t.path("links");
        fs::create_dir_all(&parent).expect("links");
        let link = parent.join("WaClient");
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(backslashed(&link))
            .arg(backslashed(&real))
            .output()
            .expect("mklink");
        assert!(status.status.success(), "mklink failed: {} {}", String::from_utf8_lossy(&status.stdout), String::from_utf8_lossy(&status.stderr));
        let cur = t.path("cur/WaClient");
        fs::create_dir_all(&cur).expect("cur");
        let boot = t.path("appdata/WaClient");
        let ctx = DeleteCtx { bootstrap: &boot, current_root: &cur };
        assert_eq!(safe_to_delete(&link, &ctx), Err(Reason::Reparse));

        // A recorded old copy that is a junction is dropped, not followed.
        let root = t.path("new/WaClient");
        populate(&root);
        save(&boot, &Location { data_root: Some(root.clone()), old_copy: Some(link.clone()), ..Location::default() }).expect("save");
        let report = run_startup(&boot, &Ops::real(), &mut real_ops_progress());
        assert_eq!(report.notices.len(), 1, "{report:?}");
        assert!(real.join("precious.txt").is_file());
        assert_eq!(load(&boot).expect("load").old_copy, None);
    }

    #[test]
    fn old_copy_is_kept_while_the_new_root_is_missing_or_blank() {
        let t = Temp::new("rootgone");
        let boot = t.path("appdata/WaClient");
        let old = t.path("old/WaClient");
        populate(&old);
        let missing = t.path("gone/WaClient");
        let mut loc = Location { data_root: Some(missing.clone()), old_copy: Some(old.clone()), ..Location::default() };
        save(&boot, &loc).expect("save");
        // Through the start-up job: refuses outright and creates nothing.
        let report = run_startup(&boot, &Ops::real(), &mut real_ops_progress());
        assert!(report.error.is_some());
        assert!(!missing.exists());
        assert_source_intact(&old);
        // Directly: even if it were called, a missing or blank root deletes nothing.
        let mut notices = Vec::new();
        assert_eq!(sweep_old_copy(&boot, &mut loc, &mut |_| {}, &mut notices), None);
        assert_eq!(notices.len(), 1);
        assert_source_intact(&old);
        fs::create_dir_all(&missing).expect("blank root");
        assert_eq!(sweep_old_copy(&boot, &mut loc, &mut |_| {}, &mut notices), None);
        assert_eq!(notices.len(), 2);
        assert_source_intact(&old);
        assert_eq!(load(&boot).expect("load").old_copy, Some(old));
    }

    #[test]
    fn a_pending_move_does_not_run_while_the_root_is_missing() {
        let t = Temp::new("pendinggone");
        let boot = t.path("appdata/WaClient");
        let to = t.path("dst/WaClient");
        let missing = t.path("gone/WaClient");
        save(&boot, &Location {
            data_root: Some(missing.clone()),
            pending_move: Some(PendingMove { from: missing.clone(), to: to.clone(), requested_at: 1 }),
            old_copy: None,
        })
        .expect("save");
        let report = run_startup(&boot, &Ops::real(), &mut real_ops_progress());
        assert!(report.error.as_deref().is_some_and(|e| e.contains("unavailable")), "{report:?}");
        assert!(!to.exists() && !missing.exists());
        assert!(peek_pending(&boot), "the move stays pending");
        assert!(check_root_present(&load(&boot).expect("load")).is_err());
    }

    #[test]
    fn deletion_needs_location_json_on_disk_to_confirm_the_new_root() {
        let t = Temp::new("reread");
        let boot = t.path("appdata/WaClient");
        let old = t.path("old/WaClient");
        let new = t.path("new/WaClient");
        let other = t.path("other/WaClient");
        populate(&old);
        populate(&new);
        populate(&other);
        // Disk says the data lives in `other`; memory believes `new`.
        save(&boot, &Location { data_root: Some(other.clone()), old_copy: Some(old.clone()), ..Location::default() })
            .expect("save");
        let mut stale = Location { data_root: Some(new.clone()), old_copy: Some(old.clone()), ..Location::default() };
        let mut notices = Vec::new();
        assert_eq!(sweep_old_copy(&boot, &mut stale, &mut |_| {}, &mut notices), Some(old.clone()));
        assert_source_intact(&old);
        assert_eq!(notices.len(), 1);
        // A corrupt file on disk is no confirmation either.
        write(&boot.join(LOCATION_FILE), b"{ broken");
        assert_eq!(sweep_old_copy(&boot, &mut stale, &mut |_| {}, &mut notices), Some(old.clone()));
        assert_source_intact(&old);
        // Once the disk agrees, the old copy goes.
        save(&boot, &stale).expect("save");
        assert_eq!(sweep_old_copy(&boot, &mut stale, &mut |_| {}, &mut notices), None);
        assert!(!old.exists());
        assert_source_intact(&new);
    }

    #[test]
    fn begin_move_never_overwrites_an_unreadable_file_or_a_pending_cleanup() {
        let t = Temp::new("begin");
        let boot = t.path("appdata/WaClient");
        let to = t.path("dst/WaClient");
        write(&boot.join(LOCATION_FILE), b"{ not json");
        let err = begin_move(&boot, &boot, &to, 1).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read(boot.join(LOCATION_FILE)).expect("file"), b"{ not json");

        let old = t.path("old/WaClient");
        save(&boot, &Location { data_root: Some(t.path("cur/WaClient")), old_copy: Some(old.clone()), ..Location::default() })
            .expect("save");
        let err = begin_move(&boot, &t.path("cur/WaClient"), &to, 1).unwrap_err();
        assert!(err.to_string().contains("Previous move cleanup pending"), "{err}");
        assert!(err.to_string().contains(&old.display().to_string()));
        assert_eq!(pending_cleanup(&boot), Some(old));
        assert!(!peek_pending(&boot));

        // A missing file is a normal first move.
        let fresh = t.path("fresh/WaClient");
        begin_move(&fresh, &fresh, &to, 1).expect("fresh begin");
        assert!(peek_pending(&fresh));
    }

    #[test]
    fn crash_after_the_switch_still_removes_the_old_copy() {
        let t = Temp::new("oldcopy");
        let bootstrap = t.path("appdata/WaClient");
        let to = t.path("dst/WaClient");
        populate(&bootstrap);
        // State after a crash between recording the new root and deleting.
        let mut seed = Location::default();
        copy_and_verify(&bootstrap, &to, &Ops::real(), &mut real_ops_progress()).expect("copy");
        seed.data_root = Some(to.clone());
        seed.old_copy = Some(bootstrap.clone());
        save(&bootstrap, &seed).expect("save");

        let report = run_startup(&bootstrap, &Ops::real(), &mut real_ops_progress());
        assert_eq!(report, MoveReport::default());
        assert!(!bootstrap.join("profiles").exists());
        assert_eq!(load(&bootstrap).expect("load").old_copy, None);
        assert_source_intact(&to);
    }

    #[test]
    fn overlapping_old_copy_record_is_never_deleted() {
        let t = Temp::new("overlap");
        // A WaClient-named folder that contains the live root.
        let root = t.path("WaClient/root");
        populate(&root);
        let seed = Location { data_root: Some(root.clone()), old_copy: Some(t.path("WaClient")), ..Location::default() };
        save(&t.path("boot"), &seed).expect("save");
        let report = run_startup(&t.path("boot"), &Ops::real(), &mut real_ops_progress());
        assert!(report.notices.iter().any(|n| n.contains("overlaps")), "{report:?}");
        assert_source_intact(&root);
        // Any other path (an arbitrary folder) is refused as well.
        let seed = Location { data_root: Some(root.clone()), old_copy: Some(t.0.clone()), ..Location::default() };
        save(&t.path("boot"), &seed).expect("save");
        let report = run_startup(&t.path("boot"), &Ops::real(), &mut real_ops_progress());
        assert!(report.notices.iter().any(|n| n.contains("not a WaClient folder")), "{report:?}");
        assert_source_intact(&root);
        assert_eq!(load(&t.path("boot")).expect("load").old_copy, None);
    }

    #[test]
    fn inconsistent_or_unsafe_pending_moves_are_dropped_without_deleting() {
        let t = Temp::new("unsafe");
        let bootstrap = t.path("src");
        populate(&bootstrap);
        let victim = t.path("victim");
        write(&victim.join("precious.txt"), b"keep me");
        // Target is not a WaClient folder: must not be wiped.
        begin_move(&bootstrap, &bootstrap, &victim, 1).expect("begin");
        let report = run_startup(&bootstrap, &Ops::real(), &mut real_ops_progress());
        assert!(report.error.is_some());
        assert!(victim.join("precious.txt").is_file());
        assert_eq!(load(&bootstrap).expect("load").pending_move, None);
        // Source that is not the current root.
        begin_move(&bootstrap, &t.path("elsewhere"), &t.path("x/WaClient"), 1).expect("begin");
        let report = run_startup(&bootstrap, &Ops::real(), &mut real_ops_progress());
        assert!(report.error.is_some());
        assert_source_intact(&bootstrap);
    }

    #[test]
    fn progress_fraction_is_monotonic_across_phases() {
        let p = |phase, done, total| Progress { phase, done, total }.fraction();
        assert_eq!(p(Phase::Copying, 0, 10), 0.0);
        assert!(p(Phase::Copying, 10, 10) <= p(Phase::Verifying, 0, 2));
        assert!(p(Phase::Verifying, 2, 2) <= p(Phase::Cleaning, 0, 1));
        assert_eq!(p(Phase::Cleaning, 1, 1), 1.0);
        assert_eq!(p(Phase::Copying, 0, 0), 0.85);
    }

    #[test]
    fn short_path_hides_the_middle() {
        assert_eq!(short(Path::new(r"C:\Users\someone\AppData\Roaming\WaClient")), r"C:\…\WaClient");
        assert_eq!(short(Path::new(r"E:\WaClient")), r"E:\WaClient");
    }
}
