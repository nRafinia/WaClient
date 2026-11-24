//! App-wide proxy for all profile engines: Settings › Network › Proxy.
//!
//! Stored in `proxy.json` in the data folder (not per profile). The password
//! is never written in plain text: it is encrypted with Windows DPAPI for the
//! current user, so the file is useless to another Windows account (the
//! password is then treated as empty and the UI says so). The engines read the
//! proxy once when they start, so a change is applied by relaunching the app.
//! Parsing, validation and the file mapping are pure functions; `App` carries
//! them out. Nothing here logs the host, the credentials or a link.

use std::cell::{Cell, RefCell};
use std::io::Write;
use std::net::Ipv6Addr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use slint::ComponentHandle;
use wa_engine::{ConnState, ProxyConfig, ProxyKind};

use super::{App, with_app};
use crate::{MainWindow, ProxyState};

const FILE: &str = "proxy.json";
/// Index of "Network" in the settings page's category list.
const NETWORK_CATEGORY: i32 = 6;
const DEFAULT_SOCKS_PORT: u16 = 1080;
const DEFAULT_HTTP_PORT: u16 = 8080;
/// SOCKS5 username/password fields are length-prefixed with one byte.
const MAX_CREDENTIAL_BYTES: usize = 255;
/// Mixed into DPAPI so a blob from another application is not accepted.
const ENTROPY: &[u8] = b"WaClient proxy password v1";

/// Whether the running engines use a proxy (decided once, at startup).
static IN_USE: AtomicBool = AtomicBool::new(false);

/// True while the engines run through a proxy; the connection text says so.
pub fn in_use() -> bool {
    IN_USE.load(Ordering::Relaxed)
}

/// How long the login screen waits for a QR code before it suggests a proxy.
pub const HINT_AFTER: Duration = Duration::from_secs(15);

/// Whether the login screen should suggest a proxy: still no QR code, not
/// paired, and no proxy in use (with one, the problem is something else).
pub fn hint_due(has_qr: bool, logged_in: bool, proxy_in_use: bool) -> bool {
    !has_qr && !logged_in && !proxy_in_use
}

/// What happens after a successful save of the proxy form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyPath {
    /// The engines already run with this proxy.
    Nothing,
    /// Show "Restart now" and let the user choose when.
    Prompt,
    /// Relaunch at once.
    Relaunch,
}

/// The engines read the proxy once at start and there is no way to restart a
/// single profile's engine in this process, so applying always means a
/// relaunch. On the login screen it happens at once (nothing is unsaved before
/// pairing); in Settings the user chooses the moment.
pub fn apply_path(on_login_screen: bool, can_relaunch: bool, changed: bool) -> ApplyPath {
    match (changed, on_login_screen && can_relaunch) {
        (false, _) => ApplyPath::Nothing,
        (true, true) => ApplyPath::Relaunch,
        (true, false) => ApplyPath::Prompt,
    }
}

// ---- pure functions: status badge -------------------------------------------

/// The proxy shield at the bottom of the chat list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyBadge {
    /// No proxy in use: nothing is shown.
    Hidden,
    Connected,
    Connecting,
    Failed,
    /// Saved and switched on, but the engines only pick it up after a restart.
    Pending,
}

/// `in_use`: a complete, switched-on proxy is saved. `pending_restart`: what is
/// saved differs from what the engines run with, so their state says nothing
/// about it. `conn` is the active profile's connection.
pub fn proxy_badge(in_use: bool, pending_restart: bool, conn: &ConnState) -> ProxyBadge {
    if !in_use {
        return ProxyBadge::Hidden;
    }
    if pending_restart {
        return ProxyBadge::Pending;
    }
    match conn {
        ConnState::Connected => ProxyBadge::Connected,
        ConnState::Connecting => ProxyBadge::Connecting,
        ConnState::Disconnected { .. } => ProxyBadge::Failed,
    }
}

impl ProxyBadge {
    /// The value of `MainWindow.proxy-badge`.
    fn slint_value(self) -> i32 {
        match self {
            ProxyBadge::Hidden => 0,
            ProxyBadge::Connected => 1,
            ProxyBadge::Connecting => 2,
            ProxyBadge::Failed => 3,
            ProxyBadge::Pending => 4,
        }
    }
}

/// The tooltip: state and proxy type, never the host, port or login.
fn badge_tip(badge: ProxyBadge, kind: ProxyKind) -> String {
    let state = match badge {
        ProxyBadge::Hidden => return String::new(),
        ProxyBadge::Connected => "connected",
        ProxyBadge::Connecting => "connecting\u{2026}",
        ProxyBadge::Failed => "not connected",
        ProxyBadge::Pending => "restart to apply",
    };
    let kind = match kind {
        ProxyKind::Socks5 => "SOCKS5",
        ProxyKind::Http => "HTTP",
    };
    format!("Proxy: {state} ({kind})")
}

// ---- pure functions: links --------------------------------------------------

/// What a pasted proxy link carries. Empty strings mean "not given".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkParts {
    pub kind: ProxyKind,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
}

const LINK_HELP: &str = "Unsupported link. Use socks5://user:pass@host:port, http://host:port or a Telegram proxy link.";

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Decodes `%XX` escapes (and `+` as a space when `plus_is_space`). A stray `%`
/// is kept as it is.
fn percent_decode(text: &str, plus_is_space: bool) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() && hex_val(bytes[i + 1]).is_some() && hex_val(bytes[i + 2]).is_some() => {
                out.push(hex_val(bytes[i + 1]).unwrap_or(0) << 4 | hex_val(bytes[i + 2]).unwrap_or(0));
                i += 3;
            }
            b'+' if plus_is_space => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_port(text: &str) -> Result<u16, String> {
    match text.trim().parse::<u16>() {
        Ok(p) if p > 0 => Ok(p),
        _ => Err("The port must be a number from 1 to 65535.".to_string()),
    }
}

/// `host`, `host:port`, `[v6]` or `[v6]:port`; the port falls back to `default`.
fn split_host_port(text: &str, default: u16) -> Result<(String, u16), String> {
    if let Some(rest) = text.strip_prefix('[') {
        let (host, after) = rest.split_once(']').ok_or("The IPv6 address is missing its closing bracket.")?;
        let port = match after.strip_prefix(':') {
            Some(p) => parse_port(p)?,
            None if after.is_empty() => default,
            None => return Err("Unexpected text after the IPv6 address.".to_string()),
        };
        return Ok((host.to_string(), port));
    }
    match text.matches(':').count() {
        0 => Ok((text.to_string(), default)),
        1 => {
            let (host, port) = text.rsplit_once(':').unwrap_or((text, ""));
            Ok((host.to_string(), parse_port(port)?))
        }
        _ => Err("Put an IPv6 address in [brackets].".to_string()),
    }
}

/// `[user[:pass]@]host[:port]` of a `scheme://` link (path and query ignored).
fn parse_authority(kind: ProxyKind, rest: &str, default_port: u16) -> Result<LinkParts, String> {
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    let authority = &rest[..end];
    let (userinfo, hostport) = match authority.rsplit_once('@') {
        Some((u, h)) => (Some(u), h),
        None => (None, authority),
    };
    let (host, port) = split_host_port(hostport, default_port)?;
    if host.is_empty() {
        return Err("The link has no host.".to_string());
    }
    let (username, password) = match userinfo {
        Some(u) => match u.split_once(':') {
            Some((name, pass)) => (percent_decode(name, false), percent_decode(pass, false)),
            None => (percent_decode(u, false), String::new()),
        },
        None => (String::new(), String::new()),
    };
    Ok(LinkParts { kind, host: percent_decode(&host, false), port, username, password })
}

/// `server=…&port=…&user=…&pass=…` of a Telegram SOCKS link.
fn parse_telegram_query(query: &str) -> Result<LinkParts, String> {
    let (mut server, mut port, mut user, mut pass) = (String::new(), None, String::new(), String::new());
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let value = percent_decode(value, true);
        match key {
            "server" => server = value,
            "port" => port = Some(value),
            "user" => user = value,
            "pass" => pass = value,
            _ => {}
        }
    }
    if server.is_empty() {
        return Err("The link has no server.".to_string());
    }
    let port = match port {
        Some(p) => parse_port(&p)?,
        None => DEFAULT_SOCKS_PORT,
    };
    let host = server.trim().trim_start_matches('[').trim_end_matches(']').to_string();
    Ok(LinkParts { kind: ProxyKind::Socks5, host, port, username: user, password: pass })
}

/// Reads `socks5://`, `socks5h://`, `socks://`, `http://` proxy links and the
/// Telegram SOCKS links (`tg://socks?…`, `https://t.me/socks?…`). Missing ports
/// default to 1080 (SOCKS) / 8080 (HTTP).
pub fn parse_link(text: &str) -> Result<LinkParts, String> {
    let text = text.trim();
    let text = text.split('#').next().unwrap_or(text);
    let (scheme, rest) = text.split_once("://").ok_or(LINK_HELP)?;
    match scheme.to_ascii_lowercase().as_str() {
        "socks5" | "socks5h" | "socks" => parse_authority(ProxyKind::Socks5, rest, DEFAULT_SOCKS_PORT),
        "http" => parse_authority(ProxyKind::Http, rest, DEFAULT_HTTP_PORT),
        "tg" => {
            let (path, query) = rest.split_once('?').ok_or(LINK_HELP)?;
            if path.trim_end_matches('/').eq_ignore_ascii_case("socks") {
                parse_telegram_query(query)
            } else {
                Err(LINK_HELP.to_string())
            }
        }
        "https" => {
            let (target, query) = rest.split_once('?').ok_or(LINK_HELP)?;
            let (host, path) = target.split_once('/').ok_or(LINK_HELP)?;
            let host = host.to_ascii_lowercase();
            if (host == "t.me" || host == "telegram.me") && path.trim_end_matches('/').eq_ignore_ascii_case("socks") {
                parse_telegram_query(query)
            } else {
                Err(LINK_HELP.to_string())
            }
        }
        _ => Err(LINK_HELP.to_string()),
    }
}

// ---- pure functions: validation and the stored form -------------------------

fn validate_host(text: &str) -> Result<String, String> {
    let host = text.trim();
    let host = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
    if host.is_empty() {
        return Err("Enter the proxy host.".to_string());
    }
    if host.contains("://") || host.contains(['/', '\\', '?', '#', '@']) || host.chars().any(char::is_whitespace) {
        return Err("Enter only the host name or address, without a scheme, path or spaces.".to_string());
    }
    if host.contains(':') {
        return host
            .parse::<Ipv6Addr>()
            .map(|_| host.to_string())
            .map_err(|_| "That is not a valid IPv6 address. Enter the port in the port field.".to_string());
    }
    if !host.chars().all(|c| c.is_alphanumeric() || matches!(c, '-' | '.' | '_')) {
        return Err("The host contains characters that are not allowed.".to_string());
    }
    Ok(host.to_string())
}

/// Checks the form fields; empty credentials become `None`.
pub fn validate(kind: ProxyKind, host: &str, port: &str, username: &str, password: &str) -> Result<ProxyConfig, String> {
    let host = validate_host(host)?;
    let port = parse_port(port)?;
    if username.len() > MAX_CREDENTIAL_BYTES || password.len() > MAX_CREDENTIAL_BYTES {
        return Err("The user name and the password can have at most 255 bytes each.".to_string());
    }
    Ok(ProxyConfig {
        kind,
        host,
        port,
        username: (!username.is_empty()).then(|| username.to_string()),
        password: (!password.is_empty()).then(|| password.to_string()),
    })
}

/// The saved proxy: the settings plus whether it is switched on.
#[derive(Clone, PartialEq, Eq)]
pub struct ProxySettings {
    pub enabled: bool,
    pub config: ProxyConfig,
}

impl Default for ProxySettings {
    fn default() -> Self {
        Self {
            enabled: false,
            config: ProxyConfig { kind: ProxyKind::Socks5, host: String::new(), port: DEFAULT_SOCKS_PORT, username: None, password: None },
        }
    }
}

impl ProxySettings {
    /// What the engines are given: only a switched-on, complete proxy.
    pub fn to_engine(&self) -> Option<ProxyConfig> {
        (self.enabled && !self.config.host.is_empty() && self.config.port != 0).then(|| self.config.clone())
    }
}

/// `proxy.json`. The password is the hex of the DPAPI-encrypted bytes.
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct ProxyFile {
    enabled: bool,
    /// "socks5" or "http".
    kind: String,
    host: String,
    port: u16,
    username: String,
    password: String,
}

fn kind_name(kind: ProxyKind) -> &'static str {
    match kind {
        ProxyKind::Socks5 => "socks5",
        ProxyKind::Http => "http",
    }
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

type Crypt<'a> = &'a dyn Fn(&[u8]) -> Option<Vec<u8>>;

fn to_file(settings: &ProxySettings, encrypt: Crypt) -> Result<ProxyFile, String> {
    let c = &settings.config;
    let password = match c.password.as_deref() {
        Some(p) if !p.is_empty() => to_hex(&encrypt(p.as_bytes()).ok_or("The password could not be encrypted, so nothing was saved.")?),
        _ => String::new(),
    };
    Ok(ProxyFile {
        enabled: settings.enabled,
        kind: kind_name(c.kind).to_string(),
        host: c.host.clone(),
        port: c.port,
        username: c.username.clone().unwrap_or_default(),
        password,
    })
}

/// The settings of a file, and whether its password could not be decrypted
/// (then it is treated as empty).
fn from_file(file: &ProxyFile, decrypt: Crypt) -> (ProxySettings, bool) {
    let (password, lost) = if file.password.is_empty() {
        (None, false)
    } else {
        match from_hex(&file.password).and_then(|b| decrypt(&b)).and_then(|b| String::from_utf8(b).ok()) {
            Some(p) => (Some(p), false),
            None => (None, true),
        }
    };
    let settings = ProxySettings {
        enabled: file.enabled,
        config: ProxyConfig {
            kind: if file.kind.eq_ignore_ascii_case("http") { ProxyKind::Http } else { ProxyKind::Socks5 },
            host: file.host.clone(),
            port: if file.port == 0 { DEFAULT_SOCKS_PORT } else { file.port },
            username: (!file.username.is_empty()).then(|| file.username.clone()),
            password,
        },
    };
    (settings, lost)
}

const PASSWORD_LOST: &str =
    "The saved proxy password could not be decrypted (it was saved by another Windows user). Enter it again and save.";
const FILE_UNREADABLE: &str = "proxy.json could not be read. Enter the proxy again and save to replace it, then restart WaClient.";

/// Why a start without a connection was chosen (shown under the message).
const BLOCK_UNREADABLE: &str = "proxy.json could not be read or is damaged.";
const BLOCK_INVALID: &str = "The saved proxy has an invalid host, port or login.";
const BLOCK_PASSWORD: &str = "The saved proxy password could not be decrypted (it was saved by another Windows user).";
/// Shown in place of every profile while the engines are held back.
pub const HELD_TEXT: &str = "Not connected: the proxy settings could not be read.";

/// What `proxy.json` looks like on disk, before any decision about it.
#[derive(Debug, PartialEq, Eq)]
pub enum FileState {
    Missing,
    /// Present but not readable (an I/O error other than "not found") or not valid JSON.
    Unreadable,
    Read { settings: ProxySettings, password_lost: bool },
}

impl std::fmt::Debug for ProxySettings {
    // Never prints the host or credentials.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxySettings").field("enabled", &self.enabled).finish_non_exhaustive()
    }
}

/// How the engines are started this run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupProxy {
    NoProxy,
    Proxy(ProxyConfig),
    /// A proxy was meant but cannot be used: never fall back to a direct
    /// connection, which would expose the user's IP address.
    Blocked(&'static str),
}

/// Missing file = no proxy. A file that exists but cannot be used never means
/// "connect directly": the start is blocked until the user decides.
pub fn startup_proxy_decision(state: &FileState) -> StartupProxy {
    match state {
        FileState::Missing => StartupProxy::NoProxy,
        FileState::Unreadable => StartupProxy::Blocked(BLOCK_UNREADABLE),
        FileState::Read { settings, password_lost } => {
            if !settings.enabled {
                return StartupProxy::NoProxy;
            }
            let c = &settings.config;
            if *password_lost && c.username.is_some() {
                return StartupProxy::Blocked(BLOCK_PASSWORD);
            }
            let checked = validate(c.kind, &c.host, &c.port.to_string(), c.username.as_deref().unwrap_or(""), c.password.as_deref().unwrap_or(""));
            match checked {
                Ok(config) => StartupProxy::Proxy(config),
                Err(_) => StartupProxy::Blocked(BLOCK_INVALID),
            }
        }
    }
}

fn read_state(path: &Path, decrypt: Crypt) -> FileState {
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<ProxyFile>(&bytes) {
            Ok(file) => {
                let (settings, password_lost) = from_file(&file, decrypt);
                if password_lost {
                    log::warn!("the saved proxy password could not be decrypted");
                }
                FileState::Read { settings, password_lost }
            }
            Err(e) => {
                // Only the error class: the message can quote file content.
                log::error!("proxy.json is not valid ({:?}, line {})", e.classify(), e.line());
                FileState::Unreadable
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => FileState::Missing,
        Err(e) => {
            log::error!("proxy.json could not be read ({:?})", e.kind());
            FileState::Unreadable
        }
    }
}

/// Loaded settings and a warning for the page, if the file was damaged.
struct Loaded {
    settings: ProxySettings,
    warning: Option<&'static str>,
}

fn loaded_from(state: FileState) -> Loaded {
    match state {
        FileState::Missing => Loaded { settings: ProxySettings::default(), warning: None },
        FileState::Unreadable => Loaded { settings: ProxySettings::default(), warning: Some(FILE_UNREADABLE) },
        FileState::Read { settings, password_lost } => Loaded { settings, warning: password_lost.then_some(PASSWORD_LOST) },
    }
}


fn save_with(path: &Path, settings: &ProxySettings, encrypt: Crypt) -> Result<(), String> {
    let file = to_file(settings, encrypt)?;
    let json = serde_json::to_vec_pretty(&file).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    let write = || -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&json)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    };
    write().map_err(|e| {
        log::error!("could not save proxy.json: {e}");
        format!("Could not save the proxy settings: {e}")
    })
}

/// How the engines of this run may be started (the demo never calls this).
/// Call before starting any engine.
pub fn startup_proxy(data_dir: &Path) -> StartupProxy {
    startup_proxy_decision(&read_state(&data_dir.join(FILE), &dpapi_unprotect))
}

/// Set once the user chose "Connect without proxy": engines may then start
/// without one for the rest of this run (proxy.json is not changed).
static DIRECT_ALLOWED: AtomicBool = AtomicBool::new(false);

/// The proxy to start an engine with, or why it must not be started yet.
pub fn engine_proxy(startup: &StartupProxy) -> Result<Option<ProxyConfig>, &'static str> {
    match startup {
        StartupProxy::NoProxy => Ok(None),
        StartupProxy::Proxy(c) => Ok(Some(c.clone())),
        StartupProxy::Blocked(_) if DIRECT_ALLOWED.load(Ordering::Relaxed) => Ok(None),
        StartupProxy::Blocked(reason) => Err(reason),
    }
}

// ---- Windows DPAPI ----------------------------------------------------------

#[cfg(windows)]
mod dpapi {
    use std::ffi::c_void;

    #[repr(C)]
    struct DataBlob {
        len: u32,
        data: *mut u8,
    }

    #[link(name = "crypt32")]
    unsafe extern "system" {
        fn CryptProtectData(
            input: *const DataBlob,
            description: *const u16,
            entropy: *const DataBlob,
            reserved: *mut c_void,
            prompt: *mut c_void,
            flags: u32,
            output: *mut DataBlob,
        ) -> i32;
        fn CryptUnprotectData(
            input: *const DataBlob,
            description: *mut *mut u16,
            entropy: *const DataBlob,
            reserved: *mut c_void,
            prompt: *mut c_void,
            flags: u32,
            output: *mut DataBlob,
        ) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LocalFree(mem: *mut c_void) -> *mut c_void;
    }
    const UI_FORBIDDEN: u32 = 1;

    pub fn run(data: &[u8], entropy: &[u8], protect: bool) -> Option<Vec<u8>> {
        let blob = |b: &[u8]| Some(DataBlob { len: u32::try_from(b.len()).ok()?, data: b.as_ptr().cast_mut() });
        let input = blob(data)?;
        let ent = blob(entropy)?;
        let mut out = DataBlob { len: 0, data: std::ptr::null_mut() };
        // SAFETY: `input` and `ent` point at live slices for the whole call and
        // DPAPI only reads them; `out` is a valid DATA_BLOB that DPAPI fills
        // with a LocalAlloc'ed buffer, which is copied and then freed below.
        let ok = unsafe {
            if protect {
                CryptProtectData(&input, std::ptr::null(), &ent, std::ptr::null_mut(), std::ptr::null_mut(), UI_FORBIDDEN, &mut out)
            } else {
                CryptUnprotectData(&input, std::ptr::null_mut(), &ent, std::ptr::null_mut(), std::ptr::null_mut(), UI_FORBIDDEN, &mut out)
            }
        };
        if ok == 0 || out.data.is_null() {
            return None;
        }
        // SAFETY: on success `out.data` holds `out.len` initialised bytes.
        let bytes = unsafe { std::slice::from_raw_parts(out.data, out.len as usize) }.to_vec();
        // SAFETY: the buffer was allocated by DPAPI with LocalAlloc.
        unsafe { LocalFree(out.data.cast()) };
        Some(bytes)
    }
}

/// Encrypts for the current Windows user. None where DPAPI does not exist.
fn dpapi_protect(data: &[u8]) -> Option<Vec<u8>> {
    #[cfg(windows)]
    {
        dpapi::run(data, ENTROPY, true)
    }
    #[cfg(not(windows))]
    {
        let _ = data;
        None
    }
}

fn dpapi_unprotect(data: &[u8]) -> Option<Vec<u8>> {
    #[cfg(windows)]
    {
        dpapi::run(data, ENTROPY, false)
    }
    #[cfg(not(windows))]
    {
        let _ = data;
        None
    }
}

// ---- controller -------------------------------------------------------------

pub struct ProxyCtl {
    /// None in the demo, which never writes into the data folder.
    path: Option<PathBuf>,
    saved: RefCell<ProxySettings>,
    /// What the engines of this run were started with.
    applied: Option<ProxyConfig>,
    /// Warning from loading the file; cleared by a successful save.
    warning: RefCell<Option<&'static str>>,
    testing: Cell<bool>,
    /// Why the engines are held back (nothing connects) until the user decides.
    blocked: Cell<Option<&'static str>>,
}

impl ProxyCtl {
    pub fn new(data_dir: Option<PathBuf>) -> Self {
        let path = data_dir.map(|d| d.join(FILE));
        let state = match &path {
            Some(p) => read_state(p, &dpapi_unprotect),
            None => FileState::Missing,
        };
        let demo = demo_settings(path.is_none());
        let decision = startup_proxy_decision(&state);
        let (applied, blocked) = match (&demo, decision) {
            (Some(d), _) => (d.to_engine(), None),
            (None, StartupProxy::NoProxy) => (None, None),
            (None, StartupProxy::Proxy(c)) => (Some(c), None),
            (None, StartupProxy::Blocked(reason)) => (None, Some(reason)),
        };
        IN_USE.store(applied.is_some(), Ordering::Relaxed);
        let loaded = loaded_from(state);
        Self {
            path,
            saved: RefCell::new(demo.unwrap_or(loaded.settings)),
            applied,
            warning: RefCell::new(loaded.warning),
            testing: Cell::new(false),
            blocked: Cell::new(blocked),
        }
    }

    /// Why nothing is connecting, while the user has not yet chosen.
    pub fn blocked(&self) -> Option<&'static str> {
        self.blocked.get()
    }
}

/// Demo only (`WACLIENT_DEMO_PROXY=connected|connecting|failed`): a made-up
/// proxy that is saved, switched on and applied. Never in a real run.
#[cfg(debug_assertions)]
fn demo_settings(is_demo: bool) -> Option<ProxySettings> {
    (is_demo && std::env::var("WACLIENT_DEMO_PROXY").is_ok()).then(|| ProxySettings {
        enabled: true,
        config: ProxyConfig { kind: ProxyKind::Socks5, host: "127.0.0.1".into(), port: 10808, username: None, password: None },
    })
}

#[cfg(not(debug_assertions))]
fn demo_settings(_is_demo: bool) -> Option<ProxySettings> {
    None
}

pub(super) fn wire(win: &MainWindow) {
    let g = win.global::<ProxyState>();
    g.on_save(|| with_app(App::proxy_save));
    g.on_cancel(|| with_app(App::proxy_cancel));
    g.on_test(|| with_app(App::proxy_test));
    g.on_fill_link(|text| with_app(|a| a.proxy_fill_link(text.as_str())));
    g.on_restart(|| with_app(App::proxy_restart));
    g.on_connect_direct(|| with_app(App::proxy_connect_direct));
    win.on_proxy_settings_requested(|| with_app(App::proxy_open_settings));
}

fn kind_index(kind: ProxyKind) -> i32 {
    match kind {
        ProxyKind::Socks5 => 0,
        ProxyKind::Http => 1,
    }
}

fn index_kind(index: i32) -> ProxyKind {
    if index == 1 { ProxyKind::Http } else { ProxyKind::Socks5 }
}

/// "Connected in 230 ms".
fn connected_text(elapsed: Duration) -> String {
    format!("Connected in {} ms", elapsed.as_millis())
}

impl App {
    /// Shows the stored proxy in the form. Called once at startup.
    pub(super) fn proxy_start(&self) {
        self.proxy_push();
        if let (Some(win), Some(reason)) = (self.win(), self.proxy.blocked()) {
            let g = win.global::<ProxyState>();
            g.set_blocked_reason(reason.into());
            g.set_blocked(true);
        }
    }

    /// "Connect without proxy": starts the held engines with no proxy for this
    /// run only. proxy.json is left as it is.
    pub(super) fn proxy_connect_direct(&self) {
        if self.proxy.blocked.take().is_none() {
            return;
        }
        DIRECT_ALLOWED.store(true, Ordering::Relaxed);
        log::warn!("connecting without a proxy for this run (the user chose it)");
        let profiles = self.registry.borrow().profiles.clone();
        for profile in &profiles {
            if self.find(&profile.id).is_none() {
                continue;
            }
            match super::start_runtime(&self.root, self.persist, &self.starter, &self.win, profile) {
                Ok(state) => {
                    if let Some(slot) = self.profs.borrow_mut().iter_mut().find(|p| p.id == state.id) {
                        *slot = state.clone();
                    }
                    if self.active.borrow().id == state.id {
                        *self.active.borrow_mut() = state;
                    }
                }
                Err(e) => log::error!("could not start profile {}: {e:#}", profile.id),
            }
        }
        if let Some(win) = self.win() {
            win.global::<ProxyState>().set_blocked(false);
        }
        self.render_active();
    }

    fn proxy_push(&self) {
        let Some(win) = self.win() else { return };
        let g = win.global::<ProxyState>();
        let saved = self.proxy.saved.borrow();
        let c = &saved.config;
        g.set_enabled(saved.enabled);
        g.set_kind(kind_index(c.kind));
        g.set_host(c.host.as_str().into());
        g.set_port(c.port.to_string().into());
        g.set_username(c.username.clone().unwrap_or_default().into());
        g.set_password(c.password.clone().unwrap_or_default().into());
        g.set_show_password(false);
        g.set_error("".into());
        g.set_status("".into());
        g.set_notice(self.proxy.warning.borrow().unwrap_or_default().into());
        g.set_needs_restart(saved.to_engine() != self.proxy.applied);
        drop(saved);
        self.proxy_badge_push();
    }

    /// Shows the shield for the active profile's connection (call on every
    /// connection change, profile switch and proxy change).
    pub(super) fn proxy_badge_push(&self) {
        let Some(win) = self.win() else { return };
        let active = self.p();
        // Called from many places: if one of them holds a mutable borrow, try
        // again once it has returned instead of panicking.
        let (Ok(saved), Ok(st)) = (self.proxy.saved.try_borrow(), active.st.try_borrow()) else {
            slint::Timer::single_shot(Duration::ZERO, || with_app(App::proxy_badge_push));
            return;
        };
        let engine = saved.to_engine();
        let conn = st.conn.clone();
        drop(st);
        let badge = proxy_badge(engine.is_some(), engine != self.proxy.applied, &conn);
        win.set_proxy_badge(badge.slint_value());
        win.set_proxy_tip(badge_tip(badge, saved.config.kind).into());
    }

    /// The shield was clicked: Settings, on the Network page where the proxy form is.
    fn proxy_open_settings(&self) {
        let Some(win) = self.win() else { return };
        self.open_settings();
        win.set_settings_start_category(NETWORK_CATEGORY);
        win.set_show_settings(true);
    }

    /// The form as a validated proxy config.
    fn proxy_form(&self) -> Result<ProxyConfig, String> {
        let win = self.win().ok_or("The window is closed.")?;
        let g = win.global::<ProxyState>();
        validate(
            index_kind(g.get_kind()),
            g.get_host().as_str(),
            g.get_port().as_str(),
            g.get_username().as_str(),
            g.get_password().as_str(),
        )
    }

    fn proxy_set_error(&self, error: &str) {
        if let Some(win) = self.win() {
            let g = win.global::<ProxyState>();
            g.set_error(error.into());
            g.set_notice("".into());
        }
    }

    pub(super) fn proxy_save(&self) {
        let Some(win) = self.win() else { return };
        let g = win.global::<ProxyState>();
        let enabled = g.get_enabled();
        let config = match self.proxy_form() {
            Ok(c) => c,
            // Switching the proxy off must not need a valid form.
            Err(_) if !enabled => self.proxy.saved.borrow().config.clone(),
            Err(e) => {
                self.proxy_set_error(&e);
                return;
            }
        };
        let next = ProxySettings { enabled, config };
        if let Some(path) = &self.proxy.path {
            if let Err(e) = save_with(path, &next, &dpapi_protect) {
                self.proxy_set_error(&e);
                return;
            }
        }
        let changed = next.to_engine() != self.proxy.applied;
        *self.proxy.saved.borrow_mut() = next;
        *self.proxy.warning.borrow_mut() = None;
        self.proxy_push();
        let path = apply_path(g.get_login_open(), self.proxy.path.is_some(), changed);
        if path == ApplyPath::Relaunch {
            g.set_login_open(false);
            self.proxy_restart();
        }
    }

    pub(super) fn proxy_cancel(&self) {
        self.proxy_push();
    }

    pub(super) fn proxy_fill_link(&self, text: &str) {
        let Some(win) = self.win() else { return };
        let g = win.global::<ProxyState>();
        match parse_link(text) {
            Ok(link) => {
                g.set_kind(kind_index(link.kind));
                g.set_host(link.host.into());
                g.set_port(link.port.to_string().into());
                g.set_username(link.username.into());
                g.set_password(link.password.into());
                g.set_error("".into());
                g.set_notice("Link read. Check the fields, then test or save.".into());
            }
            Err(e) => self.proxy_set_error(&e),
        }
    }

    /// Tries the form's proxy on a worker thread so the window stays responsive.
    pub(super) fn proxy_test(&self) {
        if self.proxy.testing.get() {
            return;
        }
        let config = match self.proxy_form() {
            Ok(c) => c,
            Err(e) => {
                self.proxy_set_error(&e);
                return;
            }
        };
        let Some(win) = self.win() else { return };
        let g = win.global::<ProxyState>();
        g.set_error("".into());
        g.set_status("".into());
        g.set_testing(true);
        self.proxy.testing.set(true);
        let spawned = std::thread::Builder::new().name("proxy-test".into()).spawn(move || {
            let result = wa_engine::test_proxy_blocking(&config);
            let queued = slint::invoke_from_event_loop(move || with_app(|a| a.proxy_test_done(result)));
            if let Err(e) = queued {
                log::error!("could not deliver the proxy test result: {e}");
            }
        });
        if let Err(e) = spawned {
            log::error!("could not start the proxy test: {e}");
            self.proxy_test_done(Err(format!("Could not start the test: {e}")));
        }
    }

    fn proxy_test_done(&self, result: Result<Duration, String>) {
        self.proxy.testing.set(false);
        let Some(win) = self.win() else { return };
        let g = win.global::<ProxyState>();
        g.set_testing(false);
        match result {
            Ok(elapsed) => {
                g.set_status_ok(true);
                g.set_status(connected_text(elapsed).into());
            }
            Err(e) => {
                g.set_status_ok(false);
                g.set_status(e.into());
            }
        }
    }

    /// Relaunches through the normal shutdown path: `main` starts the successor
    /// once the engines are down, and it waits for this process to exit.
    pub(super) fn proxy_restart(&self) {
        if self.proxy.path.is_none() {
            return;
        }
        // The profile on screen (e.g. the QR of one being added) is reopened.
        crate::location::request_relaunch_to_profile(&self.p().id);
        self.quit();
    }
}

#[cfg(debug_assertions)]
impl App {
    /// Demo (`WACLIENT_DEMO_UI=settings-proxy`): sample values, nothing saved.
    pub(super) fn demo_proxy(&self) {
        let Some(win) = self.win() else { return };
        let g = win.global::<ProxyState>();
        g.set_enabled(true);
        g.set_kind(0);
        g.set_host("127.0.0.1".into());
        g.set_port("10808".into());
        g.set_username("demo".into());
        g.set_password("secret".into());
        g.set_status_ok(true);
        g.set_status("Connected in 230 ms".into());
        g.set_needs_restart(true);
    }

    /// Demo (`WACLIENT_DEMO_UI=login-proxy|login-proxy-hint`): the active
    /// profile's login screen, with the proxy modal open or the hint shown.
    pub(super) fn demo_login_proxy(&self, hint: bool) {
        let p = self.p();
        self.leave_main(&p, "Connecting…");
        if hint {
            p.login.borrow_mut().show_hint = true;
            self.render_login(&p);
        } else {
            self.demo_proxy();
            if let Some(win) = self.win() {
                win.global::<ProxyState>().set_login_open(true);
            }
        }
    }

    /// Demo (`WACLIENT_DEMO_UI=proxy-badge` with `WACLIENT_DEMO_PROXY=connected|connecting|failed`,
    /// optionally `WACLIENT_DEMO_THEME=light`): puts the active profile into that connection state.
    pub(super) fn demo_proxy_badge(&self) {
        let conn = match std::env::var("WACLIENT_DEMO_PROXY").as_deref() {
            Ok("connecting") => ConnState::Connecting,
            Ok("failed") => ConnState::Disconnected { reason: String::new() },
            _ => ConnState::Connected,
        };
        self.p().st.borrow_mut().conn = conn;
        if let (Some(win), Ok("light")) = (self.win(), std::env::var("WACLIENT_DEMO_THEME").as_deref()) {
            win.global::<crate::Theme>().set_dark(false);
        }
        self.refresh_rail();
    }

    /// Demo (`WACLIENT_DEMO_UI=proxy-blocked`): the "nothing connected" screen.
    pub(super) fn demo_proxy_blocked(&self) {
        let Some(win) = self.win() else { return };
        let g = win.global::<ProxyState>();
        g.set_blocked_reason(BLOCK_UNREADABLE.into());
        g.set_blocked(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_with(path: &Path, decrypt: Crypt) -> Loaded {
        loaded_from(read_state(path, decrypt))
    }

    fn link(text: &str) -> LinkParts {
        parse_link(text).unwrap_or_else(|e| panic!("{text}: {e}"))
    }

    fn parts(kind: ProxyKind, host: &str, port: u16, user: &str, pass: &str) -> LinkParts {
        LinkParts { kind, host: host.into(), port, username: user.into(), password: pass.into() }
    }

    #[test]
    fn socks_and_http_links() {
        assert_eq!(link("socks5://user:pass@example.com:1081"), parts(ProxyKind::Socks5, "example.com", 1081, "user", "pass"));
        assert_eq!(link("socks5h://10.0.0.2:9050"), parts(ProxyKind::Socks5, "10.0.0.2", 9050, "", ""));
        assert_eq!(link("SOCKS5://host"), parts(ProxyKind::Socks5, "host", 1080, "", ""));
        assert_eq!(link("http://u:p@proxy.local:3128/"), parts(ProxyKind::Http, "proxy.local", 3128, "u", "p"));
        assert_eq!(link("http://proxy.local"), parts(ProxyKind::Http, "proxy.local", 8080, "", ""));
        assert_eq!(link("  socks5://only@host:1  "), parts(ProxyKind::Socks5, "host", 1, "only", ""));
    }

    #[test]
    fn credentials_are_percent_decoded() {
        let l = link("socks5://us%40er:p%3Ass%2Fw%C3%A9@host:1080");
        assert_eq!((l.username.as_str(), l.password.as_str()), ("us@er", "p:ss/wé"));
        let l = link("socks5://user:p%zz@host:1080");
        assert_eq!(l.password, "p%zz", "a bad escape stays as typed");
        assert_eq!(link("socks5://u:p%@host").password, "p%");
    }

    #[test]
    fn ipv6_hosts() {
        assert_eq!(link("socks5://[::1]:1080"), parts(ProxyKind::Socks5, "::1", 1080, "", ""));
        assert_eq!(link("socks5://u:p@[2001:db8::2]:9999"), parts(ProxyKind::Socks5, "2001:db8::2", 9999, "u", "p"));
        assert_eq!(link("http://[::1]"), parts(ProxyKind::Http, "::1", 8080, "", ""));
        assert!(parse_link("socks5://::1:1080").is_err());
        assert!(parse_link("socks5://[::1:1080").is_err());
    }

    #[test]
    fn telegram_links() {
        let want = parts(ProxyKind::Socks5, "1.2.3.4", 443, "me", "p w");
        assert_eq!(link("tg://socks?server=1.2.3.4&port=443&user=me&pass=p%20w"), want);
        assert_eq!(link("https://t.me/socks?server=1.2.3.4&port=443&user=me&pass=p+w"), want);
        assert_eq!(link("https://telegram.me/socks?server=1.2.3.4&port=443&user=me&pass=p%20w"), want);
        assert_eq!(link("tg://socks?server=example.org"), parts(ProxyKind::Socks5, "example.org", 1080, "", ""));
        assert_eq!(link("tg://socks?server=%5B%3A%3A1%5D&port=1"), parts(ProxyKind::Socks5, "::1", 1, "", ""));
        assert!(parse_link("tg://socks?port=1").is_err(), "no server");
        assert!(parse_link("tg://socks?server=h&port=x").is_err());
        assert!(parse_link("tg://proxy?server=h&port=1").is_err(), "MTProto links are not SOCKS");
        assert!(parse_link("https://t.me/proxy?server=h&port=1").is_err());
        assert!(parse_link("https://example.com/socks?server=h&port=1").is_err());
    }

    #[test]
    fn bad_links_are_refused() {
        for bad in ["", "host:1080", "ftp://host:1", "socks5://", "socks5://:1080", "socks5://host:0", "socks5://host:70000", "socks5://host:x", "https://host:8080"] {
            assert!(parse_link(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn validation_of_the_form() {
        let ok = validate(ProxyKind::Http, " proxy.example.com ", " 8080 ", "", "").expect("valid");
        assert_eq!((ok.host.as_str(), ok.port, ok.username, ok.password), ("proxy.example.com", 8080, None, None));
        let ok = validate(ProxyKind::Socks5, "[::1]", "1080", "u", "p").expect("valid");
        assert_eq!((ok.host.as_str(), ok.username.as_deref(), ok.password.as_deref()), ("::1", Some("u"), Some("p")));
        assert!(validate(ProxyKind::Socks5, "2001:db8::1", "1", "", "").is_ok());
        assert!(validate(ProxyKind::Socks5, "127.0.0.1", "65535", "", "").is_ok());
    }

    #[test]
    fn invalid_hosts_and_ports_are_refused() {
        for host in ["", "  ", "socks5://host", "host/path", "host:1080", "us@host", "a b", "host?x=1", "ho$t", "[::zz]"] {
            assert!(validate(ProxyKind::Socks5, host, "1080", "", "").is_err(), "{host:?}");
        }
        for port in ["", "0", "65536", "-1", "abc", "10 80"] {
            assert!(validate(ProxyKind::Socks5, "host", port, "", "").is_err(), "{port:?}");
        }
        let long = "x".repeat(256);
        assert!(validate(ProxyKind::Socks5, "host", "1", &long, "").is_err());
        assert!(validate(ProxyKind::Socks5, "host", "1", "", &long).is_err());
    }

    /// Stand-ins for DPAPI: reversible and not the plain text.
    fn fake_encrypt(b: &[u8]) -> Option<Vec<u8>> {
        Some(b.iter().map(|x| x ^ 0x5a).collect())
    }

    fn fake_decrypt(b: &[u8]) -> Option<Vec<u8>> {
        fake_encrypt(b)
    }

    fn refuse(_: &[u8]) -> Option<Vec<u8>> {
        None
    }

    fn sample() -> ProxySettings {
        ProxySettings {
            enabled: true,
            config: ProxyConfig {
                kind: ProxyKind::Http,
                host: "::1".into(),
                port: 3128,
                username: Some("user".into()),
                password: Some("hunter2 \u{e9}".into()),
            },
        }
    }

    #[test]
    fn file_round_trip_never_stores_the_password_in_plain_text() {
        let dir = std::env::temp_dir().join(format!("waclient-proxy-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join(FILE);
        assert!(load_with(&path, &fake_decrypt).settings == ProxySettings::default(), "no file = no proxy");
        save_with(&path, &sample(), &fake_encrypt).expect("save");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(!text.contains("hunter2"), "{text}");
        assert!(text.contains("\"kind\": \"http\"") && text.contains("\"user\""));
        let loaded = load_with(&path, &fake_decrypt);
        assert!(loaded.settings == sample() && loaded.warning.is_none());
        assert_eq!(loaded.settings.to_engine(), Some(sample().config));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn undecryptable_password_is_empty_and_reported() {
        let file = to_file(&sample(), &fake_encrypt).expect("file");
        let (settings, lost) = from_file(&file, &refuse);
        assert!(lost);
        assert_eq!(settings.config.password, None);
        assert_eq!(settings.config.username.as_deref(), Some("user"));
        let mut damaged = to_file(&sample(), &fake_encrypt).expect("file");
        damaged.password = "zz".into();
        assert!(from_file(&damaged, &fake_decrypt).1);
    }

    #[test]
    fn a_failed_encryption_saves_nothing() {
        assert!(to_file(&sample(), &refuse).is_err());
        let mut no_password = sample();
        no_password.config.password = None;
        assert!(to_file(&no_password, &refuse).is_ok(), "nothing to encrypt");
    }

    #[test]
    fn damaged_or_partial_files_are_handled() {
        let dir = std::env::temp_dir().join(format!("waclient-proxy-test2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join(FILE);
        std::fs::write(&path, b"{ not json").expect("write");
        let loaded = load_with(&path, &fake_decrypt);
        assert!(loaded.warning.is_some() && loaded.settings.to_engine().is_none());
        std::fs::write(&path, b"{\"enabled\":true,\"host\":\"h\"}").expect("write");
        let cfg = load_with(&path, &fake_decrypt).settings.to_engine().expect("proxy");
        assert_eq!((cfg.kind, cfg.port), (ProxyKind::Socks5, 1080));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn engines_only_get_a_complete_enabled_proxy() {
        let mut s = sample();
        assert!(s.to_engine().is_some());
        s.enabled = false;
        assert!(s.to_engine().is_none());
        s.enabled = true;
        s.config.host.clear();
        assert!(s.to_engine().is_none());
        assert!(ProxySettings::default().to_engine().is_none());
    }

    fn read(settings: ProxySettings, password_lost: bool) -> FileState {
        FileState::Read { settings, password_lost }
    }

    #[test]
    fn a_missing_file_is_no_proxy_and_an_unreadable_one_blocks() {
        assert_eq!(startup_proxy_decision(&FileState::Missing), StartupProxy::NoProxy);
        assert!(matches!(startup_proxy_decision(&FileState::Unreadable), StartupProxy::Blocked(_)));
    }

    #[test]
    fn a_disabled_proxy_is_no_proxy_whatever_else_is_in_the_file() {
        let mut s = sample();
        s.enabled = false;
        s.config.host = "bad host".into();
        assert_eq!(startup_proxy_decision(&read(s, true)), StartupProxy::NoProxy);
        assert_eq!(startup_proxy_decision(&read(ProxySettings::default(), false)), StartupProxy::NoProxy);
    }

    #[test]
    fn an_enabled_valid_proxy_is_used() {
        assert_eq!(startup_proxy_decision(&read(sample(), false)), StartupProxy::Proxy(sample().config));
        let mut padded = sample();
        padded.config.host = " proxy.example.com ".into();
        let StartupProxy::Proxy(c) = startup_proxy_decision(&read(padded, false)) else { panic!("blocked") };
        assert_eq!(c.host, "proxy.example.com");
    }

    #[test]
    fn an_enabled_proxy_with_a_bad_host_or_port_blocks() {
        for host in ["", "  ", "bad host", "host/path", "socks5://h"] {
            let mut s = sample();
            s.config.host = host.into();
            assert!(matches!(startup_proxy_decision(&read(s, false)), StartupProxy::Blocked(_)), "{host:?}");
        }
        let mut s = sample();
        s.config.port = 0;
        assert!(matches!(startup_proxy_decision(&read(s, false)), StartupProxy::Blocked(_)));
        let mut s = sample();
        s.config.username = Some("u".repeat(256));
        assert!(matches!(startup_proxy_decision(&read(s, false)), StartupProxy::Blocked(_)));
    }

    #[test]
    fn a_lost_password_with_a_user_name_blocks() {
        let mut s = sample();
        s.config.password = None;
        assert!(matches!(startup_proxy_decision(&read(s.clone(), true)), StartupProxy::Blocked(_)));
        s.config.username = None;
        assert!(matches!(startup_proxy_decision(&read(s, true)), StartupProxy::Proxy(_)), "no login to fail");
    }

    #[test]
    fn files_on_disk_map_to_the_right_state() {
        let dir = std::env::temp_dir().join(format!("waclient-proxy-test3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join(FILE);
        assert_eq!(read_state(&path, &fake_decrypt), FileState::Missing);
        std::fs::write(&path, b"{ not json").expect("write");
        assert_eq!(read_state(&path, &fake_decrypt), FileState::Unreadable);
        // A directory in place of the file is an I/O error other than NotFound.
        let as_dir = dir.join("sub");
        std::fs::create_dir_all(&as_dir).expect("sub");
        assert_eq!(read_state(&as_dir, &fake_decrypt), FileState::Unreadable);
        save_with(&path, &sample(), &fake_encrypt).expect("save");
        let state = read_state(&path, &fake_decrypt);
        assert_eq!(startup_proxy_decision(&state), StartupProxy::Proxy(sample().config));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn engines_are_only_held_back_while_blocked() {
        assert_eq!(engine_proxy(&StartupProxy::NoProxy), Ok(None));
        assert_eq!(engine_proxy(&StartupProxy::Proxy(sample().config)), Ok(Some(sample().config)));
        assert!(engine_proxy(&StartupProxy::Blocked("x")).is_err());
    }

    #[test]
    fn the_hint_waits_for_a_missing_qr_and_no_proxy() {
        assert!(hint_due(false, false, false));
        assert!(!hint_due(true, false, false), "a QR code is showing");
        assert!(!hint_due(false, true, false), "already paired");
        assert!(!hint_due(false, false, true), "a proxy is set: not a blocking problem");
        assert_eq!(HINT_AFTER, Duration::from_secs(15));
    }

    #[test]
    fn a_changed_proxy_relaunches_on_the_login_screen_only() {
        assert_eq!(apply_path(true, true, true), ApplyPath::Relaunch);
        assert_eq!(apply_path(false, true, true), ApplyPath::Prompt);
        assert_eq!(apply_path(true, false, true), ApplyPath::Prompt, "the demo never relaunches");
        assert_eq!(apply_path(true, true, false), ApplyPath::Nothing);
        assert_eq!(apply_path(false, false, false), ApplyPath::Nothing);
    }

    fn badge(in_use: bool, pending: bool, conn: ConnState) -> ProxyBadge {
        proxy_badge(in_use, pending, &conn)
    }

    fn down() -> ConnState {
        ConnState::Disconnected { reason: "network".into() }
    }

    #[test]
    fn no_proxy_means_no_badge_in_any_state() {
        for conn in [ConnState::Connected, ConnState::Connecting, down()] {
            assert_eq!(badge(false, false, conn.clone()), ProxyBadge::Hidden);
            assert_eq!(badge(false, true, conn), ProxyBadge::Hidden, "switched off, restart still due");
        }
    }

    #[test]
    fn the_badge_follows_the_connection_of_the_applied_proxy() {
        assert_eq!(badge(true, false, ConnState::Connected), ProxyBadge::Connected);
        assert_eq!(badge(true, false, ConnState::Connecting), ProxyBadge::Connecting);
        assert_eq!(badge(true, false, down()), ProxyBadge::Failed);
        assert_eq!(badge(true, false, ConnState::Disconnected { reason: String::new() }), ProxyBadge::Failed);
    }

    #[test]
    fn a_proxy_the_engines_do_not_use_yet_is_pending_whatever_the_connection() {
        for conn in [ConnState::Connected, ConnState::Connecting, down()] {
            assert_eq!(badge(true, true, conn), ProxyBadge::Pending);
        }
    }

    #[test]
    fn badge_values_for_the_ui_are_distinct() {
        let all = [ProxyBadge::Hidden, ProxyBadge::Connected, ProxyBadge::Connecting, ProxyBadge::Failed, ProxyBadge::Pending];
        let values: Vec<i32> = all.iter().map(|b| b.slint_value()).collect();
        assert_eq!(values, [0, 1, 2, 3, 4]);
    }

    #[test]
    fn the_tooltip_names_state_and_type_but_never_the_address() {
        assert_eq!(badge_tip(ProxyBadge::Connected, ProxyKind::Socks5), "Proxy: connected (SOCKS5)");
        assert_eq!(badge_tip(ProxyBadge::Connecting, ProxyKind::Http), "Proxy: connecting\u{2026} (HTTP)");
        assert_eq!(badge_tip(ProxyBadge::Failed, ProxyKind::Socks5), "Proxy: not connected (SOCKS5)");
        assert_eq!(badge_tip(ProxyBadge::Pending, ProxyKind::Http), "Proxy: restart to apply (HTTP)");
        assert_eq!(badge_tip(ProxyBadge::Hidden, ProxyKind::Http), "");
    }

    #[test]
    fn test_result_text() {
        assert_eq!(connected_text(Duration::from_millis(230)), "Connected in 230 ms");
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_round_trip() {
        let blob = dpapi_protect(b"secret").expect("protect");
        assert!(!blob.windows(6).any(|w| w == b"secret"));
        assert_eq!(dpapi_unprotect(&blob).as_deref(), Some(&b"secret"[..]));
        assert!(dpapi_unprotect(b"not a blob").is_none());
    }
}
