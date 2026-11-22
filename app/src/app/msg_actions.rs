//! Chat-view actions that need the controller: the context menu's copy and
//! delete, opening links, the "Copied" toast, and profile-picture prefetching
//! for the chat list.

use std::collections::VecDeque;
use std::time::Duration;

use slint::{Model, Timer, TimerMode};
use wa_engine::{Command, ConnState};
use wa_store::ChatKind;

use super::{App, ProfileState, State, with_app};
use crate::{links, models};

const TOAST_TTL: Duration = Duration::from_millis(1600);
/// Longest URL (in characters) quoted in the "Opening" toast.
const TOAST_URL_CHARS: usize = 60;
/// Rows requested right away when the list is (re)built, before the viewport
/// reports what is actually in view.
const AVATAR_FIRST_WINDOW: usize = 14;
/// The rest of the list is requested in batches of this size ...
const AVATAR_BATCH: usize = 20;
/// ... this far apart. The engine paces the lookups itself; this only keeps
/// the queue it has to hold short.
const AVATAR_BATCH_EVERY: Duration = Duration::from_secs(2);
/// Most rows one viewport report may ask for.
const AVATAR_MAX_VISIBLE: usize = 40;

thread_local! {
    static TOAST: Timer = Timer::default();
    static AVATAR_BATCHES: Timer = Timer::default();
}

fn avatar_wanted(st: &State, jid: &str) -> bool {
    ChatKind::from_jid(jid) != ChatKind::Other
        && !st.avatar_requested.contains(jid)
        && !matches!(st.avatar_paths.get(jid), Some(Some(_)))
}

fn copy_to_clipboard(text: &str) -> Result<(), arboard::Error> {
    arboard::Clipboard::new()?.set_text(text.to_owned())
}

/// `https://very.long/...` -> first characters plus an ellipsis.
fn shorten(url: &str) -> String {
    if url.chars().count() <= TOAST_URL_CHARS {
        return url.to_string();
    }
    let mut s: String = url.chars().take(TOAST_URL_CHARS).collect();
    s.push('…');
    s
}

#[cfg(windows)]
fn launch_url(url: &str) -> std::io::Result<()> {
    // ShellExecuteW needs COM on the calling thread for some handlers.
    // SAFETY: plain COM init/uninit on this thread; uninit only after a
    // successful init (S_OK or S_FALSE), as COM requires.
    let com = unsafe { super::CoInitializeEx(std::ptr::null(), super::COINIT_APARTMENTTHREADED | super::COINIT_DISABLE_OLE1DDE) } >= 0;
    let result = shell_open(url);
    if com {
        // SAFETY: balances the successful CoInitializeEx above.
        unsafe { super::CoUninitialize() };
    }
    result
}

#[cfg(windows)]
fn shell_open(url: &str) -> std::io::Result<()> {
    let wide: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
    let verb: Vec<u16> = "open".encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `verb` and `wide` are valid NUL-terminated UTF-16 strings that
    // outlive the call; the remaining pointer arguments are allowed to be null.
    let code = unsafe {
        super::ShellExecuteW(0, verb.as_ptr(), wide.as_ptr(), std::ptr::null(), std::ptr::null(), 1)
    };
    // Values above 32 mean success.
    if code > 32 {
        Ok(())
    } else {
        Err(std::io::Error::other(format!("ShellExecuteW failed with code {code}")))
    }
}

#[cfg(not(windows))]
fn launch_url(url: &str) -> std::io::Result<()> {
    std::process::Command::new("xdg-open").arg(url).spawn().map(drop)
}

impl App {
    // ---- toast -----------------------------------------------------------------

    pub(super) fn show_toast(&self, text: &str) {
        self.show_toast_action(text, "", TOAST_TTL);
    }

    /// A toast with a clickable `action` after the text (empty: none).
    pub(super) fn show_toast_action(&self, text: &str, action: &str, ttl: Duration) {
        let Some(win) = self.win() else { return };
        win.set_toast_text(text.into());
        win.set_toast_action(action.into());
        TOAST.with(|t| {
            t.start(TimerMode::SingleShot, ttl, || {
                with_app(|a| {
                    if let Some(w) = a.win() {
                        w.set_toast_text(Default::default());
                        w.set_toast_action(Default::default());
                    }
                });
            });
        });
    }

    // ---- context menu actions ----------------------------------------------------

    pub(super) fn copy_message(&self, id: &str) {
        let text = {
            let p = self.p();
            let st = p.st.borrow();
            st.msgs.iter().find(|m| m.id == id && !m.revoked).and_then(|m| m.text.clone())
        };
        if let Some(text) = text.filter(|t| !t.is_empty()) {
            self.copy_text(&text);
        }
    }

    /// Opens the direct chat with a shared contact, creating it when needed.
    pub(super) fn message_contact(&self, digits: &str) {
        // A number still starting with 0 is a local one whose country is unknown.
        let Some(jid) = models::contact_chat_jid(digits).filter(|_| !digits.starts_with('0')) else {
            self.show_banner(1, "This contact has no usable phone number");
            return;
        };
        let p = self.p();
        let exists = matches!(p.store.chat(&jid), Ok(Some(_)));
        if !exists {
            let name = format!("+{digits}");
            if self.ok("create chat", p.store.upsert_chat(&jid, ChatKind::Dm, Some(&name))).is_none() {
                return;
            }
        }
        if let Some(win) = self.win() {
            win.set_status_mode(false);
        }
        self.select_chat(&jid);
        self.refresh_chats();
    }

    /// Puts `text` on the clipboard and confirms with a toast.
    /// The write runs off the UI thread: the clipboard can be held by another
    /// program for a while.
    pub(super) fn copy_text(&self, text: &str) {
        let text = text.to_owned();
        let spawned = std::thread::Builder::new().name("clipboard".into()).spawn(move || {
            let result = copy_to_clipboard(&text);
            let queued = slint::invoke_from_event_loop(move || {
                with_app(|a| match result {
                    Ok(()) => a.show_toast("Copied"),
                    Err(e) => {
                        log::error!("clipboard: {e}");
                        a.show_banner(1, &format!("Could not copy to the clipboard: {e}"));
                    }
                });
            });
            if let Err(e) = queued {
                log::debug!("clipboard result not delivered: {e}");
            }
        });
        if let Err(e) = spawned {
            log::error!("could not start the clipboard thread: {e}");
            self.show_banner(1, "Could not copy to the clipboard");
        }
    }

    /// Sends the delete command; the row changes when the engine reports
    /// `MessagesChanged`, not before.
    pub(super) fn delete_message(&self, id: &str, for_everyone: bool) {
        let (jid, allowed) = {
            let p = self.p();
            let st = p.st.borrow();
            let Some(jid) = st.selected.clone() else { return };
            let now = chrono::Utc::now().timestamp();
            let allowed = st
                .msgs
                .iter()
                .find(|m| m.id == id)
                .map(|m| !for_everyone || models::can_delete_for_everyone(m, now));
            (jid, allowed)
        };
        match allowed {
            None => {}
            Some(false) => self.show_banner(1, "This message can no longer be deleted for everyone"),
            Some(true) => self.send(Command::DeleteMessage {
                chat_jid: jid,
                id: id.to_string(),
                for_everyone,
            }),
        }
    }

    pub(super) fn open_link(&self, url: &str) {
        if super::group_new_ui::is_invite_link(url) {
            self.preview_invite(url);
            return;
        }
        if !links::is_safe_link(url) {
            log::warn!("refused to open a link with an unsupported scheme");
            self.show_banner(1, "Only web and e-mail links can be opened");
            return;
        }
        self.show_toast(&format!("Opening {}", shorten(url)));
        let url = url.to_string();
        let spawned = std::thread::Builder::new().name("open-link".into()).spawn(move || {
            if let Err(e) = launch_url(&url) {
                log::error!("could not open a link: {e}");
                let queued = slint::invoke_from_event_loop(move || {
                    with_app(|a| a.show_banner(1, &format!("Could not open the link: {e}")));
                });
                if let Err(e) = queued {
                    log::debug!("link error not delivered: {e}");
                }
            }
        });
        if let Err(e) = spawned {
            log::error!("could not start the link opener thread: {e}");
        }
    }

    // ---- profile pictures of the chat list -------------------------------------------

    fn chat_jids(&self, from: usize, to: usize) -> Vec<String> {
        let to = to.min(self.chats_model.row_count());
        (from..to)
            .filter_map(|i| self.chats_model.row_data(i))
            .map(|c| c.jid.to_string())
            .collect()
    }

    /// Asks for the pictures of `jids` that were not asked for in this session.
    pub(super) fn request_avatars(&self, p: &ProfileState, jids: &[String]) {
        let fresh: Vec<String> = {
            let mut st = p.st.borrow_mut();
            if st.conn != ConnState::Connected {
                return;
            }
            let mut fresh = Vec::new();
            for jid in jids {
                if avatar_wanted(&st, jid) {
                    st.avatar_requested.insert(jid.clone());
                    fresh.push(jid.clone());
                }
            }
            fresh
        };
        for jid in fresh {
            (p.send)(Command::FetchAvatar { jid });
        }
    }

    /// The chat list scrolled: the rows in view go first.
    pub(super) fn chats_visible(&self, first: i32, count: i32) {
        let from = usize::try_from(first).unwrap_or(0);
        let n = usize::try_from(count).unwrap_or(0).min(AVATAR_MAX_VISIBLE);
        self.request_avatars(&self.p(), &self.chat_jids(from, from.saturating_add(n)));
    }

    /// Called whenever the list is rebuilt or the connection comes up: the top
    /// of the list now, the rest in the background.
    pub(super) fn prefetch_avatars(&self) {
        let p = self.p();
        if p.st.borrow().conn != ConnState::Connected {
            return;
        }
        let all = self.chat_jids(0, usize::MAX);
        self.request_avatars(&p, &all[..all.len().min(AVATAR_FIRST_WINDOW)]);
        let more = {
            let mut st = p.st.borrow_mut();
            let queue: VecDeque<String> = all.into_iter().filter(|j| avatar_wanted(&st, j)).collect();
            let more = !queue.is_empty();
            st.avatar_queue = queue;
            more
        };
        if more {
            AVATAR_BATCHES.with(|t| {
                if !t.running() {
                    t.start(TimerMode::Repeated, AVATAR_BATCH_EVERY, || with_app(App::avatar_batch));
                }
            });
        }
    }

    fn avatar_batch(&self) {
        let p = self.p();
        let next: Vec<String> = {
            let mut st = p.st.borrow_mut();
            // `prefetch_avatars` restarts the timer once the connection is back.
            if st.conn != ConnState::Connected {
                AVATAR_BATCHES.with(Timer::stop);
                return;
            }
            let mut next = Vec::new();
            while next.len() < AVATAR_BATCH {
                let Some(jid) = st.avatar_queue.pop_front() else { break };
                if avatar_wanted(&st, &jid) {
                    next.push(jid);
                }
            }
            next
        };
        if next.is_empty() {
            AVATAR_BATCHES.with(Timer::stop);
            return;
        }
        self.request_avatars(&p, &next);
    }

    /// Refreshes the avatars shown in the chat list without rebuilding it:
    /// the row of `only`, or every row that has none yet.
    pub(super) fn sync_chat_avatars(&self, only: Option<&str>) {
        let p = self.p();
        let mut st = p.st.borrow_mut();
        for i in 0..self.chats_model.row_count() {
            let Some(mut row) = self.chats_model.row_data(i) else { continue };
            let targeted = only.is_some_and(|j| row.jid.as_str() == j);
            if only.is_some() && !targeted {
                continue;
            }
            if !targeted && row.has_avatar {
                continue;
            }
            let img = self.avatar(&p, &mut st, row.jid.as_str());
            if !targeted && img.is_none() {
                continue;
            }
            row.has_avatar = img.is_some();
            row.avatar = img.unwrap_or_default();
            self.chats_model.set_row_data(i, row);
        }
    }
}

/// Debug aids for the demo (`WACLIENT_DEMO_UI`): they only talk to this
/// program's own window.
#[cfg(all(debug_assertions, windows))]
mod demo {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use slint::ComponentHandle;

    use super::App;

    #[link(name = "user32")]
    unsafe extern "system" {
        fn PostMessageW(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> i32;
    }

    const WM_MOUSEMOVE: u32 = 0x0200;
    const WM_RBUTTONDOWN: u32 = 0x0204;
    const WM_RBUTTONUP: u32 = 0x0205;
    const WM_LBUTTONDOWN: u32 = 0x0201;
    const WM_LBUTTONUP: u32 = 0x0202;
    const WM_KEYDOWN: u32 = 0x0100;
    const WM_KEYUP: u32 = 0x0101;
    const VK_ESCAPE: usize = 0x1B;
    const MK_RBUTTON: usize = 0x0002;
    const MK_LBUTTON: usize = 0x0001;

    impl App {
        /// Posts a mouse move and a right click at client pixel (x, y) to the
        /// window itself, exercising the real pointer path of the bubbles.
        pub(in crate::app) fn demo_right_click(&self, x: i32, y: i32) {
            self.demo_click(x, y, true);
        }

        /// Presses and releases Esc as real keyboard messages.
        pub(in crate::app) fn demo_esc(&self) {
            let Some(win) = self.win() else { return };
            let handle = win.window().window_handle();
            let Some(hwnd) = handle.window_handle().ok().and_then(|h| match h.as_raw() {
                RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
                _ => None,
            }) else {
                log::warn!("demo: no window handle");
                return;
            };
            // SAFETY: `hwnd` is this process's own window; the messages carry no pointers.
            unsafe {
                PostMessageW(hwnd, WM_KEYDOWN, VK_ESCAPE, 0x0001_0001);
                PostMessageW(hwnd, WM_KEYUP, VK_ESCAPE, 0xC001_0001u32 as isize);
            }
        }

        /// Left or right click at client pixel (x, y), as real input messages.
        pub(in crate::app) fn demo_click(&self, x: i32, y: i32, right: bool) {
            let Some(win) = self.win() else { return };
            let handle = win.window().window_handle();
            let Some(hwnd) = handle.window_handle().ok().and_then(|h| match h.as_raw() {
                RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
                _ => None,
            }) else {
                log::warn!("demo: no window handle");
                return;
            };
            let lparam = ((y as isize & 0xffff) << 16) | (x as isize & 0xffff);
            // SAFETY: `hwnd` is this process's own window; the messages carry no pointers.
            unsafe {
                PostMessageW(hwnd, WM_MOUSEMOVE, 0, lparam);
                if right {
                    PostMessageW(hwnd, WM_RBUTTONDOWN, MK_RBUTTON, lparam);
                    PostMessageW(hwnd, WM_RBUTTONUP, 0, lparam);
                } else {
                    PostMessageW(hwnd, WM_LBUTTONDOWN, MK_LBUTTON, lparam);
                    PostMessageW(hwnd, WM_LBUTTONUP, 0, lparam);
                }
            }
        }
    }
}
