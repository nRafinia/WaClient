//! App passcode lock: an optional PIN/password that covers the whole window.
//!
//! This is a screen lock for a shared PC, not encryption: engines keep running
//! and nothing on disk is encrypted. The passcode itself is never stored, only
//! a salted PBKDF2-HMAC-SHA256 hash, in `applock.json` in the data folder
//! (deleting that file removes the lock). The decisions (validation, hashing,
//! idle timing, back-off) are pure functions; `App` only carries them out.

use std::cell::{Cell, RefCell};
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use slint::{ComponentHandle, Timer, TimerMode};

use super::{App, with_app};
use crate::{LockState, MainWindow, PopupState};

const FILE: &str = "applock.json";
pub const MIN_LEN: usize = 4;
pub const MAX_LEN: usize = 32;
/// PBKDF2 rounds for new hashes; the count is stored with the hash so it can
/// be raised later without invalidating existing passcodes.
const ITERATIONS: u32 = 250_000;
const SALT_LEN: usize = 16;
const HASH_LEN: usize = 32;
/// A stored record outside this range is damaged or tampered with (a tiny
/// count would make guessing cheap, a huge one would hang the check).
const MIN_ITERATIONS: u32 = 100_000;
const MAX_ITERATIONS: u32 = 10_000_000;
/// Shown on the lock screen when `applock.json` exists but cannot be used.
pub const DAMAGED_TEXT: &str = "Lock settings are damaged. Delete %APPDATA%\\WaClient\\applock.json (or the same file in your moved data folder) to reset the lock.";
/// Wrong attempts allowed before the back-off starts.
const FREE_TRIES: u32 = 5;
/// Waits after the 5th, 6th, ... wrong attempt; the last one repeats.
const BACKOFF_SECS: [u64; 6] = [5, 10, 30, 60, 120, 300];
/// Seconds of inactivity per auto-lock choice: off, 1 min, 5 min, 1 h, 5 h.
pub const AUTO_LOCK_SECS: [u64; 5] = [0, 60, 300, 3600, 5 * 3600];
/// How often the idle / minimised checks and the back-off countdown run.
const TICK: Duration = Duration::from_secs(1);

// ---- pure functions --------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasscodeError {
    TooShort,
    TooLong,
    Mismatch,
}

impl PasscodeError {
    pub fn text(self) -> &'static str {
        match self {
            PasscodeError::TooShort => "The passcode must have at least 4 characters.",
            PasscodeError::TooLong => "The passcode can have at most 32 characters.",
            PasscodeError::Mismatch => "The two entries do not match.",
        }
    }
}

/// A new passcode must be 4 to 32 characters and typed twice the same.
pub fn validate_new(first: &str, second: &str) -> Result<(), PasscodeError> {
    let n = first.chars().count();
    if n < MIN_LEN {
        Err(PasscodeError::TooShort)
    } else if n > MAX_LEN {
        Err(PasscodeError::TooLong)
    } else if first != second {
        Err(PasscodeError::Mismatch)
    } else {
        Ok(())
    }
}

/// The stored form of a passcode.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct PasscodeRecord {
    pub iterations: u32,
    /// Hex.
    pub salt: String,
    /// Hex.
    pub hash: String,
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn from_hex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 || !text.is_ascii() {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok()).collect()
}

/// PBKDF2-HMAC-SHA256 for one 32-byte output block (RFC 8018).
fn pbkdf2_sha256(password: &[u8], salt: &[u8], iterations: u32) -> Option<[u8; 32]> {
    let base = Hmac::<Sha256>::new_from_slice(password).ok()?;
    let mut first = base.clone();
    first.update(salt);
    first.update(&1u32.to_be_bytes());
    let mut u = [0u8; 32];
    u.copy_from_slice(&first.finalize().into_bytes()[..]);
    let mut out = u;
    for _ in 1..iterations.max(1) {
        let mut mac = base.clone();
        mac.update(&u);
        u.copy_from_slice(&mac.finalize().into_bytes()[..]);
        for (o, x) in out.iter_mut().zip(u.iter()) {
            *o ^= x;
        }
    }
    Some(out)
}

/// Equality that does not stop at the first differing byte.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// Whether a record read from disk is plausible: iteration count in range, a
/// 16-byte salt and a 32-byte hash.
pub fn record_is_valid(record: &PasscodeRecord) -> bool {
    (MIN_ITERATIONS..=MAX_ITERATIONS).contains(&record.iterations)
        && from_hex(&record.salt).is_some_and(|s| s.len() == SALT_LEN)
        && from_hex(&record.hash).is_some_and(|h| h.len() == HASH_LEN)
}

/// Hashes `passcode` with the given salt.
pub fn hash_with(passcode: &str, salt: &[u8], iterations: u32) -> Option<PasscodeRecord> {
    let hash = pbkdf2_sha256(passcode.as_bytes(), salt, iterations)?;
    Some(PasscodeRecord { iterations, salt: to_hex(salt), hash: to_hex(&hash) })
}

/// Hashes `passcode` with a fresh random salt.
pub fn make_record(passcode: &str) -> Result<PasscodeRecord, String> {
    let mut salt = [0u8; SALT_LEN];
    if !fill_random(&mut salt) {
        return Err("The system random number generator is not available.".to_string());
    }
    hash_with(passcode, &salt, ITERATIONS).ok_or_else(|| "Could not hash the passcode.".to_string())
}

/// Whether `passcode` matches the stored record.
pub fn verify(record: &PasscodeRecord, passcode: &str) -> bool {
    let (Some(salt), Some(want)) = (from_hex(&record.salt), from_hex(&record.hash)) else { return false };
    pbkdf2_sha256(passcode.as_bytes(), &salt, record.iterations).is_some_and(|got| ct_eq(&got, &want))
}

#[cfg(windows)]
fn fill_random(buf: &mut [u8]) -> bool {
    #[link(name = "bcrypt")]
    unsafe extern "system" {
        fn BCryptGenRandom(alg: *mut core::ffi::c_void, buf: *mut u8, len: u32, flags: u32) -> i32;
    }
    const USE_SYSTEM_PREFERRED_RNG: u32 = 2;
    let Ok(len) = u32::try_from(buf.len()) else { return false };
    // SAFETY: `buf` is valid for `len` bytes; a null algorithm handle together
    // with the system-preferred-RNG flag is the documented way to call this.
    unsafe { BCryptGenRandom(std::ptr::null_mut(), buf.as_mut_ptr(), len, USE_SYSTEM_PREFERRED_RNG) == 0 }
}

#[cfg(not(windows))]
fn fill_random(buf: &mut [u8]) -> bool {
    use std::io::Read;
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(buf)).is_ok()
}

/// True when `idle` has reached the auto-lock choice (index into
/// [`AUTO_LOCK_SECS`]); "Off" and unknown choices never lock.
pub fn idle_should_lock(idle: Duration, choice: usize) -> bool {
    AUTO_LOCK_SECS.get(choice).is_some_and(|&secs| secs > 0 && idle.as_secs() >= secs)
}

/// How long the unlock is refused after `failures` wrong attempts in a row.
pub fn backoff_secs(failures: u32) -> u64 {
    if failures < FREE_TRIES {
        return 0;
    }
    let step = usize::try_from(failures - FREE_TRIES).unwrap_or(usize::MAX);
    BACKOFF_SECS[step.min(BACKOFF_SECS.len() - 1)]
}

/// Seconds left of a back-off that ends at `blocked_until` (unix seconds). A
/// stored end further away than the longest wait is a clock change: clamped.
pub fn wait_left(blocked_until: i64, now: i64) -> u64 {
    let longest = BACKOFF_SECS[BACKOFF_SECS.len() - 1];
    u64::try_from(blocked_until.saturating_sub(now)).unwrap_or(0).min(longest)
}

/// Input idle time of the app: while its window is in front it is the system
/// idle time; otherwise the time since the last input that happened in front.
pub fn app_idle(window_active: bool, system_idle: Duration, since_last_input: Duration) -> Duration {
    if window_active { system_idle } else { since_last_input }
}

// ---- stored settings --------------------------------------------------------

/// `applock.json`. Absent file = no lock.
#[derive(Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LockConfig {
    pub passcode: Option<PasscodeRecord>,
    /// Index into [`AUTO_LOCK_SECS`].
    pub auto_lock: usize,
    pub lock_on_tray: bool,
    /// Wrong attempts since the last success; kept across restarts.
    pub failures: u32,
    /// Unix seconds until which unlocking is refused.
    pub blocked_until: i64,
}

/// Reads the stored settings. `Err` means the file exists but cannot be
/// trusted: the app then starts locked with no way to unlock.
fn parse_config(bytes: &[u8]) -> Result<LockConfig, &'static str> {
    // Only fixed reasons are logged: serde messages can quote file content.
    let cfg: LockConfig = serde_json::from_slice(bytes).map_err(|_| "not valid JSON")?;
    match &cfg.passcode {
        Some(record) if !record_is_valid(record) => Err("the passcode record is not valid"),
        _ => Ok(cfg),
    }
}

fn load(path: &std::path::Path) -> Result<LockConfig, &'static str> {
    match std::fs::read(path) {
        Ok(bytes) => parse_config(&bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(LockConfig::default()),
        Err(e) => {
            log::error!("applock.json could not be opened ({:?})", e.kind());
            Err("could not be opened")
        }
    }
}

fn save(path: &std::path::Path, cfg: &LockConfig) -> std::io::Result<()> {
    let json = serde_json::to_vec_pretty(cfg).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(&json)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)
}

// ---- controller state -------------------------------------------------------

pub struct LockCtl {
    /// None in the demo, which never writes into the data folder.
    path: Option<PathBuf>,
    cfg: RefCell<LockConfig>,
    locked: Cell<bool>,
    /// `applock.json` exists but is unreadable or corrupt: locked for good.
    damaged: bool,
    /// A passcode check is running on a worker thread.
    verifying: Cell<bool>,
    /// When the last keyboard / mouse input in the app window happened.
    last_input: Cell<Instant>,
    tick: Timer,
}

impl LockCtl {
    pub fn new(data_dir: Option<PathBuf>) -> Self {
        let path = data_dir.map(|d| d.join(FILE));
        let (cfg, damaged) = match path.as_deref().map(load) {
            Some(Err(why)) => {
                log::error!("applock.json is damaged ({why}); starting locked");
                (LockConfig::default(), true)
            }
            Some(Ok(cfg)) => (cfg, false),
            None => (LockConfig::default(), false),
        };
        // Locked from the start whenever a passcode is set, and always when
        // the file cannot be trusted (a damaged file must not turn the lock off).
        let locked = damaged || cfg.passcode.is_some();
        Self {
            path,
            cfg: RefCell::new(cfg),
            locked: Cell::new(locked),
            damaged,
            verifying: Cell::new(false),
            last_input: Cell::new(Instant::now()),
            tick: Timer::default(),
        }
    }

    pub fn is_locked(&self) -> bool {
        self.locked.get()
    }
}

/// Time since the last keyboard or mouse input anywhere in Windows.
#[cfg(windows)]
fn system_idle() -> Option<Duration> {
    #[repr(C)]
    struct LastInputInfo {
        size: u32,
        time: u32,
    }
    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetLastInputInfo(info: *mut LastInputInfo) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetTickCount() -> u32;
    }
    let mut info = LastInputInfo { size: size_of::<LastInputInfo>() as u32, time: 0 };
    // SAFETY: `info` is a valid LASTINPUTINFO with `cbSize` set.
    if unsafe { GetLastInputInfo(&mut info) } == 0 {
        return None;
    }
    // SAFETY: GetTickCount has no preconditions.
    let now = unsafe { GetTickCount() };
    Some(Duration::from_millis(u64::from(now.wrapping_sub(info.time))))
}

#[cfg(not(windows))]
fn system_idle() -> Option<Duration> {
    None
}

fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

pub(super) fn wire(win: &MainWindow) {
    let g = win.global::<LockState>();
    g.on_unlock(|code| with_app(|a| a.lock_try_unlock(code.as_str())));
    g.on_lock_now(|| with_app(App::lock_now));
    g.on_save_passcode(|cur, new, again| with_app(|a| a.lock_save_passcode(cur.as_str(), new.as_str(), again.as_str())));
    g.on_turn_off(|cur| with_app(|a| a.lock_turn_off(cur.as_str())));
    g.on_set_auto_lock(|i| with_app(|a| a.lock_set_auto(i)));
    g.on_set_lock_on_tray(|on| with_app(|a| a.lock_set_tray(on)));
}

/// Popup menus draw above everything, including the lock overlay, so any that
/// is open when the lock engages is closed with Esc (Slint closes the topmost
/// popup per press; the window-level handler ignores Esc while locked).
fn close_open_popups(win: &MainWindow) {
    let open = win.global::<PopupState>().get_open();
    let esc = slint::SharedString::from(slint::platform::Key::Escape);
    for _ in 0..open.clamp(0, 16) {
        win.window().dispatch_event(slint::platform::WindowEvent::KeyPressed { text: esc.clone() });
        win.window().dispatch_event(slint::platform::WindowEvent::KeyReleased { text: esc.clone() });
    }
}

impl App {
    /// Whether the lock screen is up (masks notifications and call banners).
    pub(super) fn is_locked(&self) -> bool {
        self.lock.is_locked()
    }

    /// Shows the stored state and starts the idle watcher. Called once.
    pub(super) fn lock_start(&self) {
        self.lock_push();
        self.lock_refresh_wait();
        self.lock.tick.start(TimerMode::Repeated, TICK, || with_app(App::lock_tick));
    }

    fn lock_push(&self) {
        let Some(win) = self.win() else { return };
        let g = win.global::<LockState>();
        let cfg = self.lock.cfg.borrow();
        g.set_has_passcode(cfg.passcode.is_some());
        g.set_auto_lock(i32::try_from(cfg.auto_lock).unwrap_or(0));
        g.set_lock_on_tray(cfg.lock_on_tray);
        g.set_damaged(self.lock.damaged);
        if self.lock.damaged {
            g.set_error(DAMAGED_TEXT.into());
        }
        g.set_locked(self.lock.locked.get());
    }

    /// Writes `next` and only then makes it the current state.
    fn lock_commit(&self, next: LockConfig) -> Result<(), String> {
        if let Some(path) = &self.lock.path {
            if let Err(e) = save(path, &next) {
                log::error!("could not save applock.json: {e}");
                return Err(format!("Could not save the lock settings: {e}"));
            }
        }
        *self.lock.cfg.borrow_mut() = next;
        self.lock_push();
        Ok(())
    }

    /// Persists the failure counter; a write error is logged, not fatal.
    fn lock_store_attempts(&self) {
        let cfg = self.lock.cfg.borrow().clone();
        if let Some(path) = &self.lock.path {
            if let Err(e) = save(path, &cfg) {
                log::error!("could not save the unlock attempts: {e}");
            }
        }
    }

    pub(super) fn lock_now(&self) {
        if self.lock.cfg.borrow().passcode.is_none() || self.lock.locked.replace(true) {
            return;
        }
        // Pop-out chat windows would keep showing messages: they close.
        self.popouts_close_all();
        if let Some(win) = self.win() {
            close_open_popups(&win);
            let g = win.global::<LockState>();
            g.set_error("".into());
            g.set_locked(true);
        }
        self.refresh_title();
        self.lock_refresh_wait();
        self.calls_render();
    }

    /// Checks the passcode on a worker thread (hashing takes long, especially in
    /// debug builds); the result comes back through `lock_unlock_done`.
    fn lock_try_unlock(&self, code: &str) {
        if !self.lock.locked.get() || self.lock.damaged || self.lock.verifying.get() {
            return;
        }
        if wait_left(self.lock.cfg.borrow().blocked_until, now_secs()) > 0 {
            self.lock_refresh_wait();
            return;
        }
        let Some(record) = self.lock.cfg.borrow().passcode.clone() else { return };
        let code = code.to_string();
        self.lock_set_verifying(true);
        let spawned = std::thread::Builder::new().name("lock-verify".into()).spawn(move || {
            let ok = verify(&record, &code);
            let queued = slint::invoke_from_event_loop(move || with_app(|a| a.lock_unlock_done(ok)));
            if let Err(e) = queued {
                log::error!("could not deliver the passcode check: {e}");
            }
        });
        if let Err(e) = spawned {
            log::error!("could not start the passcode check: {e}");
            self.lock_set_verifying(false);
            if let Some(win) = self.win() {
                win.global::<LockState>().set_error("Could not check the passcode. Try again.".into());
            }
        }
    }

    fn lock_set_verifying(&self, on: bool) {
        self.lock.verifying.set(on);
        if let Some(win) = self.win() {
            win.global::<LockState>().set_verifying(on);
        }
    }

    fn lock_unlock_done(&self, ok: bool) {
        self.lock_set_verifying(false);
        if !self.lock.locked.get() {
            return;
        }
        let now = now_secs();
        if ok {
            {
                let mut cfg = self.lock.cfg.borrow_mut();
                cfg.failures = 0;
                cfg.blocked_until = 0;
            }
            self.lock_store_attempts();
            self.lock.locked.set(false);
            self.lock.last_input.set(Instant::now());
            if let Some(win) = self.win() {
                let g = win.global::<LockState>();
                g.set_error("".into());
                g.set_blocked(false);
                g.set_locked(false);
            }
            self.refresh_title();
            self.calls_render();
            return;
        }
        {
            let mut cfg = self.lock.cfg.borrow_mut();
            cfg.failures = cfg.failures.saturating_add(1);
            let wait = backoff_secs(cfg.failures);
            cfg.blocked_until = if wait > 0 { now.saturating_add(i64::try_from(wait).unwrap_or(0)) } else { 0 };
        }
        self.lock_store_attempts();
        log::warn!("wrong app passcode ({} in a row)", self.lock.cfg.borrow().failures);
        if let Some(win) = self.win() {
            win.global::<LockState>().set_error("Wrong passcode".into());
        }
        self.lock_refresh_wait();
    }

    /// Error line of the lock screen while the back-off runs: the countdown,
    /// cleared when it ends.
    fn lock_refresh_wait(&self) {
        let Some(win) = self.win() else { return };
        let g = win.global::<LockState>();
        let left = wait_left(self.lock.cfg.borrow().blocked_until, now_secs());
        if left > 0 {
            g.set_blocked(true);
            g.set_error(format!("Wrong passcode. Try again in {left} s.").into());
        } else if g.get_blocked() {
            g.set_blocked(false);
            g.set_error("".into());
        }
    }

    /// Once a second: counts the back-off down and applies the auto-lock rules.
    fn lock_tick(&self) {
        if self.lock.cfg.borrow().passcode.is_none() {
            return;
        }
        if self.lock.locked.get() {
            if self.win().is_some_and(|w| w.global::<LockState>().get_blocked()) {
                self.lock_refresh_wait();
            }
            return;
        }
        let Some(win) = self.win() else { return };
        let (on_tray, choice) = {
            let cfg = self.lock.cfg.borrow();
            (cfg.lock_on_tray, cfg.auto_lock)
        };
        let w = win.window();
        if on_tray && (!w.is_visible() || w.is_minimized()) {
            self.lock_now();
            return;
        }
        let active = self.window_active();
        let now = Instant::now();
        let system = system_idle().unwrap_or_default();
        if active {
            // Remember when the last input happened while the window is in front.
            self.lock.last_input.set(now.checked_sub(system).unwrap_or(now));
        }
        let idle = app_idle(active, system, now.saturating_duration_since(self.lock.last_input.get()));
        if idle_should_lock(idle, choice) {
            self.lock_now();
        }
    }

    fn lock_set_status(&self, error: &str, status: &str) {
        if let Some(win) = self.win() {
            let g = win.global::<LockState>();
            g.set_form_error(error.into());
            g.set_status(status.into());
        }
    }

    fn lock_close_form(&self, status: &str) {
        if let Some(win) = self.win() {
            let g = win.global::<LockState>();
            g.set_form_error("".into());
            g.set_status(status.into());
            g.set_form_mode(0);
        }
    }

    fn lock_save_passcode(&self, current: &str, new: &str, again: &str) {
        let had = self.lock.cfg.borrow().passcode.clone();
        if let Some(record) = &had {
            if !verify(record, current) {
                self.lock_set_status("The current passcode is wrong.", "");
                return;
            }
        }
        if let Err(e) = validate_new(new, again) {
            self.lock_set_status(e.text(), "");
            return;
        }
        let record = match make_record(new) {
            Ok(r) => r,
            Err(e) => {
                self.lock_set_status(&e, "");
                return;
            }
        };
        let mut next = self.lock.cfg.borrow().clone();
        next.passcode = Some(record);
        next.failures = 0;
        next.blocked_until = 0;
        match self.lock_commit(next) {
            Ok(()) => self.lock_close_form(if had.is_some() { "Passcode changed." } else { "Passcode set." }),
            Err(e) => self.lock_set_status(&e, ""),
        }
    }

    fn lock_turn_off(&self, current: &str) {
        let ok = self.lock.cfg.borrow().passcode.as_ref().is_some_and(|r| verify(r, current));
        if !ok {
            self.lock_set_status("The current passcode is wrong.", "");
            return;
        }
        match self.lock_commit(LockConfig::default()) {
            Ok(()) => self.lock_close_form("App lock is off."),
            Err(e) => self.lock_set_status(&e, ""),
        }
    }

    fn lock_set_auto(&self, index: i32) {
        let Ok(index) = usize::try_from(index) else { return };
        if index >= AUTO_LOCK_SECS.len() {
            return;
        }
        let mut next = self.lock.cfg.borrow().clone();
        next.auto_lock = index;
        if let Err(e) = self.lock_commit(next) {
            self.lock_set_status(&e, "");
        }
    }

    fn lock_set_tray(&self, on: bool) {
        let mut next = self.lock.cfg.borrow().clone();
        next.lock_on_tray = on;
        if let Err(e) = self.lock_commit(next) {
            self.lock_set_status(&e, "");
        }
    }
}

#[cfg(debug_assertions)]
impl App {
    /// Demo (`WACLIENT_DEMO_UI=locked`): sets a throw-away in-memory passcode
    /// ("1234") and shows the lock screen.
    pub(super) fn demo_lock_damaged(&self) {
        if let Some(win) = self.win() {
            let g = win.global::<LockState>();
            g.set_damaged(true);
            g.set_error(DAMAGED_TEXT.into());
            g.set_locked(true);
        }
    }

    pub(super) fn demo_lock(&self) {
        if let Ok(record) = make_record("1234") {
            self.lock.cfg.borrow_mut().passcode = Some(record);
        }
        self.lock_push();
        self.lock_now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passcode_length_is_four_to_thirty_two_characters() {
        assert_eq!(validate_new("abc", "abc"), Err(PasscodeError::TooShort));
        assert_eq!(validate_new("", ""), Err(PasscodeError::TooShort));
        assert_eq!(validate_new("abcd", "abcd"), Ok(()));
        let long = "x".repeat(32);
        assert_eq!(validate_new(&long, &long), Ok(()));
        let too_long = "x".repeat(33);
        assert_eq!(validate_new(&too_long, &too_long), Err(PasscodeError::TooLong));
    }

    #[test]
    fn length_counts_characters_not_bytes() {
        assert_eq!(validate_new("éééé", "éééé"), Ok(()));
        assert_eq!(validate_new("ééé", "ééé"), Err(PasscodeError::TooShort));
    }

    #[test]
    fn both_entries_must_match() {
        assert_eq!(validate_new("abcd", "abce"), Err(PasscodeError::Mismatch));
        assert_eq!(validate_new("abcd", ""), Err(PasscodeError::Mismatch));
    }

    #[test]
    fn pbkdf2_matches_the_known_vector() {
        // RFC 6070 style vector for HMAC-SHA256: "password" / "salt", 1 round.
        let out = pbkdf2_sha256(b"password", b"salt", 1).expect("hash");
        assert_eq!(to_hex(&out), "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b");
        let out = pbkdf2_sha256(b"password", b"salt", 2).expect("hash");
        assert_eq!(to_hex(&out), "ae4d0c95af6b46d32d0adff928f06dd02a303f8ef3c251dfd6e2d85a95474c43");
    }

    #[test]
    fn hash_and_verify_round_trip() {
        let record = hash_with("correct horse", &[7u8; SALT_LEN], 1000).expect("hash");
        assert!(verify(&record, "correct horse"));
        assert!(!verify(&record, "correct horse "));
        assert!(!verify(&record, "Correct horse"));
        assert!(!verify(&record, ""));
    }

    #[test]
    fn record_does_not_contain_the_passcode_and_salts_differ() {
        let a = make_record("secret-pass").expect("record");
        let b = make_record("secret-pass").expect("record");
        assert!(a.salt != b.salt);
        assert!(a.hash != b.hash);
        assert!(a.iterations >= 200_000);
        let json = serde_json::to_string(&a).expect("json");
        assert!(!json.contains("secret-pass"));
        assert!(verify(&a, "secret-pass"));
        assert!(verify(&b, "secret-pass"));
    }

    #[test]
    fn damaged_record_never_verifies() {
        let mut record = hash_with("abcd", &[1u8; SALT_LEN], 10).expect("hash");
        record.hash = "zz".to_string();
        assert!(!verify(&record, "abcd"));
        record.hash = "abc".to_string();
        assert!(!verify(&record, "abcd"));
    }

    #[test]
    fn constant_time_compare() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
    }

    #[test]
    fn idle_decision() {
        let min = Duration::from_secs(60);
        assert!(!idle_should_lock(Duration::from_secs(10_000), 0), "off never locks");
        assert!(!idle_should_lock(Duration::from_secs(59), 1));
        assert!(idle_should_lock(min, 1));
        assert!(!idle_should_lock(Duration::from_secs(299), 2));
        assert!(idle_should_lock(Duration::from_secs(300), 2));
        assert!(!idle_should_lock(Duration::from_secs(3599), 3));
        assert!(idle_should_lock(Duration::from_secs(3600), 3));
        assert!(!idle_should_lock(Duration::from_secs(5 * 3600 - 1), 4));
        assert!(idle_should_lock(Duration::from_secs(5 * 3600), 4));
        assert!(!idle_should_lock(Duration::from_secs(1_000_000), 99), "unknown choice never locks");
    }

    #[test]
    fn idle_uses_the_last_input_while_the_window_is_not_in_front() {
        let sys = Duration::from_secs(3);
        let since = Duration::from_secs(400);
        assert_eq!(app_idle(true, sys, since), sys);
        assert_eq!(app_idle(false, sys, since), since);
    }

    #[test]
    fn backoff_schedule() {
        for n in 0..5 {
            assert_eq!(backoff_secs(n), 0, "{n} failures");
        }
        let want = [(5, 5), (6, 10), (7, 30), (8, 60), (9, 120), (10, 300), (11, 300), (500, 300), (u32::MAX, 300)];
        for (failures, secs) in want {
            assert_eq!(backoff_secs(failures), secs, "{failures} failures");
        }
    }

    #[test]
    fn wait_left_counts_down_and_is_capped() {
        assert_eq!(wait_left(110, 100), 10);
        assert_eq!(wait_left(100, 100), 0);
        assert_eq!(wait_left(90, 100), 0);
        assert_eq!(wait_left(100 + 86_400, 100), 300, "a far-future end is a clock change");
        assert_eq!(wait_left(0, 100), 0);
    }

    /// A well-formed record without paying for the hashing.
    fn record(iterations: u32, salt_len: usize, hash_len: usize) -> PasscodeRecord {
        PasscodeRecord { iterations, salt: "ab".repeat(salt_len), hash: "cd".repeat(hash_len) }
    }

    #[test]
    fn record_validation_checks_iterations_and_lengths() {
        assert!(record_is_valid(&record(100_000, 16, 32)));
        assert!(record_is_valid(&record(250_000, 16, 32)));
        assert!(record_is_valid(&record(10_000_000, 16, 32)));
        assert!(!record_is_valid(&record(99_999, 16, 32)), "too few rounds");
        assert!(!record_is_valid(&record(0, 16, 32)));
        assert!(!record_is_valid(&record(10_000_001, 16, 32)), "too many rounds");
        assert!(!record_is_valid(&record(u32::MAX, 16, 32)));
        assert!(!record_is_valid(&record(250_000, 15, 32)), "short salt");
        assert!(!record_is_valid(&record(250_000, 17, 32)), "long salt");
        assert!(!record_is_valid(&record(250_000, 16, 31)), "short hash");
        assert!(!record_is_valid(&record(250_000, 16, 33)), "long hash");
        let mut bad_hex = record(250_000, 16, 32);
        bad_hex.salt = "zz".repeat(16);
        assert!(!record_is_valid(&bad_hex));
    }

    #[test]
    fn a_fresh_record_is_valid() {
        assert!(record_is_valid(&make_record("abcd").expect("record")));
    }

    #[test]
    fn parse_accepts_good_files_and_rejects_damaged_ones() {
        let good = LockConfig { passcode: Some(record(250_000, 16, 32)), auto_lock: 2, lock_on_tray: true, failures: 3, blocked_until: 9 };
        let bytes = serde_json::to_vec(&good).expect("json");
        assert!(parse_config(&bytes).is_ok_and(|c| c == good));
        assert!(parse_config(b"{\"auto_lock\":1}").is_ok_and(|c| c.passcode.is_none() && c.auto_lock == 1));
        assert!(parse_config(b"{ not json").is_err());
        assert!(parse_config(b"").is_err());
        assert!(parse_config(b"{\"passcode\":\"x\"}").is_err(), "wrong type");
        let weak = LockConfig { passcode: Some(record(10, 16, 32)), ..LockConfig::default() };
        assert!(parse_config(&serde_json::to_vec(&weak).expect("json")).is_err(), "iterations out of range");
        let short = LockConfig { passcode: Some(record(250_000, 8, 32)), ..LockConfig::default() };
        assert!(parse_config(&serde_json::to_vec(&short).expect("json")).is_err(), "salt length");
    }

    #[test]
    fn config_roundtrips_and_a_damaged_file_is_an_error() {
        let dir = std::env::temp_dir().join(format!("waclient-lock-test-{}-{}", std::process::id(), now_secs()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join(FILE);
        assert!(load(&path).is_ok_and(|c| c == LockConfig::default()), "no file = no lock");
        let cfg = LockConfig { passcode: Some(record(250_000, 16, 32)), auto_lock: 2, lock_on_tray: true, failures: 6, blocked_until: 1234 };
        save(&path, &cfg).expect("save");
        assert!(load(&path).is_ok_and(|c| c == cfg));
        std::fs::write(&path, b"{\"auto_lock\":1}").expect("write");
        let partial = load(&path).expect("partial");
        assert!(partial.passcode.is_none() && partial.auto_lock == 1 && partial.failures == 0);
        std::fs::write(&path, b"{ not json").expect("write");
        assert!(load(&path).is_err(), "corrupt = damaged, not 'lock off'");
        // A directory in place of the file: an I/O error other than NotFound.
        let as_dir = dir.join("sub");
        std::fs::create_dir_all(&as_dir).expect("sub");
        assert!(load(&as_dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
