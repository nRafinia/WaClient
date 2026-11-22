//! Incoming-call banners. Calls cannot be answered here: the banner shows who
//! is calling, offers Decline and Open chat, and tells the user to answer on
//! the phone. Several profiles can ring at once, so banners are keyed by
//! (profile id, call id) and stacked, newest on top.

use std::cell::RefCell;
use std::time::Duration;

use slint::{ComponentHandle, ModelRc, Timer, VecModel};
use wa_engine::{CallEndReason, Command};

use super::{App, ProfileState, with_app};
use crate::{CallBannerItem, CallState, MainWindow, models};

/// Most banners on screen; the oldest is dropped for a newer call.
const MAX_BANNERS: usize = 3;
/// How long "Missed call" stays before the banner goes away.
const MISSED_SHOWN: Duration = Duration::from_secs(2);
/// Avatars decode in the background; the banners are redrawn after these delays.
const AVATAR_RETRIES: [Duration; 2] = [Duration::from_millis(400), Duration::from_millis(1500)];

#[derive(Debug, Clone, PartialEq, Eq)]
struct CallEntry {
    profile: String,
    profile_name: String,
    call_id: String,
    chat_jid: String,
    caller: String,
    group_name: Option<String>,
    is_video: bool,
    is_group: bool,
    declining: bool,
    missed: bool,
}

impl CallEntry {
    fn key(&self) -> String {
        format!("{}\u{1f}{}", self.profile, self.call_id)
    }

    fn is(&self, profile: &str, call_id: &str) -> bool {
        self.profile == profile && self.call_id == call_id
    }
}

thread_local! {
    /// Banners, newest first.
    static CALLS: RefCell<Vec<CallEntry>> = const { RefCell::new(Vec::new()) };
}

/// "Caller" or, for a group call whose group name is known, "Caller · Group".
fn banner_title(caller: &str, is_group: bool, group_name: Option<&str>) -> String {
    match group_name.map(str::trim).filter(|g| is_group && !g.is_empty()) {
        Some(group) => format!("{caller} · {group}"),
        None => caller.to_string(),
    }
}

/// "Incoming voice call" / "Incoming video call", plus the profile's name when
/// the call is for a profile other than the active one.
fn banner_subtitle(is_video: bool, missed: bool, other_profile: Option<&str>) -> String {
    let kind = if is_video { "video" } else { "voice" };
    let mut text = if missed { format!("Missed {kind} call") } else { format!("Incoming {kind} call") };
    if let Some(name) = other_profile {
        text.push_str(" · ");
        text.push_str(name);
    }
    text
}

/// What a banner shows while the app is locked: no name, avatar or profile.
fn masked_banner(e: &CallEntry) -> CallBannerItem {
    CallBannerItem {
        key: e.key().into(),
        title: "Incoming call".into(),
        subtitle: if e.missed { "Missed call".into() } else { "".into() },
        has_avatar: false,
        avatar: Default::default(),
        initials: "".into(),
        tint: models::tint_for(""),
        is_video: e.is_video,
        declining: e.declining,
        missed: e.missed,
    }
}

/// Puts `entry` on top of the stack. A call that is already shown is updated
/// in place; beyond `cap` the oldest banners are dropped.
fn stack_insert(list: &mut Vec<CallEntry>, entry: CallEntry, cap: usize) {
    if let Some(existing) = list.iter_mut().find(|e| e.is(&entry.profile, &entry.call_id)) {
        *existing = entry;
        return;
    }
    list.insert(0, entry);
    list.truncate(cap);
}

/// Re-enables Decline on a call's banner; true when it was there.
fn stack_decline_failed(list: &mut [CallEntry], profile: &str, call_id: &str) -> bool {
    match list.iter_mut().find(|e| e.is(profile, call_id)) {
        Some(e) => {
            e.declining = false;
            true
        }
        None => false,
    }
}

/// Removes a call's banner; true when it was there.
fn stack_remove(list: &mut Vec<CallEntry>, profile: &str, call_id: &str) -> bool {
    let before = list.len();
    list.retain(|e| !e.is(profile, call_id));
    list.len() != before
}

pub(super) fn has_banners() -> bool {
    CALLS.with(|c| !c.borrow().is_empty())
}

fn entry_by_key(key: &str) -> Option<CallEntry> {
    CALLS.with(|c| c.borrow().iter().find(|e| e.key() == key).cloned())
}

fn remove_call(profile: &str, call_id: &str) {
    CALLS.with(|c| stack_remove(&mut c.borrow_mut(), profile, call_id));
}

pub(super) fn wire(win: &MainWindow) {
    let g = win.global::<CallState>();
    g.on_decline(|key| with_app(|a| a.call_decline(key.as_str())));
    g.on_open_chat(|key| with_app(|a| a.call_open_chat(key.as_str())));
    g.on_dismiss(|key| {
        with_app(|a| {
            if let Some(e) = entry_by_key(key.as_str()) {
                remove_call(&e.profile, &e.call_id);
                a.calls_render();
            }
        });
    });
}

impl App {
    pub(super) fn on_incoming_call(
        &self,
        p: &ProfileState,
        call_id: String,
        chat_jid: String,
        caller_name: String,
        is_video: bool,
        is_group: bool,
    ) {
        log::info!("incoming call (video={is_video}, group={is_group})");
        let group_name = if is_group {
            p.store.chat(&chat_jid).ok().flatten().map(|r| r.display_name).filter(|n| !n.trim().is_empty())
        } else {
            None
        };
        let entry = CallEntry {
            profile: p.id.clone(),
            profile_name: p.name(),
            call_id,
            chat_jid,
            caller: caller_name,
            group_name,
            is_video,
            is_group,
            declining: false,
            missed: false,
        };
        CALLS.with(|c| stack_insert(&mut c.borrow_mut(), entry, MAX_BANNERS));
        self.calls_render();
        for delay in AVATAR_RETRIES {
            Timer::single_shot(delay, || with_app(App::calls_render));
        }
        if !self.window_active() {
            if let Some(win) = self.win() {
                flash_taskbar(&win);
            }
        }
    }

    pub(super) fn on_call_ended(&self, p: &ProfileState, call_id: &str, reason: CallEndReason) {
        let shown_missed = reason == CallEndReason::Missed
            && CALLS.with(|c| match c.borrow_mut().iter_mut().find(|e| e.is(&p.id, call_id)) {
                Some(e) => {
                    e.missed = true;
                    true
                }
                None => false,
            });
        if shown_missed {
            let (profile, call_id) = (p.id.clone(), call_id.to_string());
            Timer::single_shot(MISSED_SHOWN, move || {
                with_app(|a| {
                    remove_call(&profile, &call_id);
                    a.calls_render();
                });
            });
        } else {
            remove_call(&p.id, call_id);
        }
        self.calls_render();
    }

    pub(super) fn on_call_decline_failed(&self, p: &ProfileState, call_id: &str) {
        CALLS.with(|c| stack_decline_failed(&mut c.borrow_mut(), &p.id, call_id));
        self.calls_render();
    }

    /// Esc: hides the topmost banner (the call keeps ringing on the phone).
    pub(super) fn call_dismiss_top(&self) {
        CALLS.with(|c| {
            let mut list = c.borrow_mut();
            if !list.is_empty() {
                list.remove(0);
            }
        });
        self.calls_render();
    }

    fn call_decline(&self, key: &str) {
        let Some(entry) = entry_by_key(key) else { return };
        let Some(p) = self.find(&entry.profile) else { return };
        CALLS.with(|c| {
            if let Some(e) = c.borrow_mut().iter_mut().find(|e| e.is(&entry.profile, &entry.call_id)) {
                e.declining = true;
            }
        });
        (p.send)(Command::RejectCall { call_id: entry.call_id });
        self.calls_render();
    }

    fn call_open_chat(&self, key: &str) {
        let Some(entry) = entry_by_key(key) else { return };
        self.show_window();
        if self.is_locked() {
            return;
        }
        if self.find(&entry.profile).is_none() || self.profile_switch_refused() {
            return;
        }
        self.activate(&entry.profile);
        self.select_chat(&entry.chat_jid);
    }

    /// Redraws the banners from the stack (also after a profile switch, which
    /// changes which calls are labelled with their profile).
    pub(super) fn calls_render(&self) {
        let Some(win) = self.win() else { return };
        let active_id = self.p().id.clone();
        let entries = CALLS.with(|c| c.borrow().clone());
        let locked = self.is_locked();
        let items: Vec<CallBannerItem> = entries
            .iter()
            .map(|e| {
                if locked {
                    return masked_banner(e);
                }
                let avatar = self.find(&e.profile).and_then(|p| {
                    let mut st = p.st.borrow_mut();
                    self.avatar(&p, &mut st, &e.chat_jid)
                });
                let other = (e.profile != active_id).then_some(e.profile_name.as_str());
                CallBannerItem {
                    key: e.key().into(),
                    title: banner_title(&e.caller, e.is_group, e.group_name.as_deref()).into(),
                    subtitle: banner_subtitle(e.is_video, e.missed, other).into(),
                    has_avatar: avatar.is_some(),
                    avatar: avatar.unwrap_or_default(),
                    initials: models::initials(&e.caller).into(),
                    tint: models::tint_for(&e.chat_jid),
                    is_video: e.is_video,
                    declining: e.declining,
                    missed: e.missed,
                }
            })
            .collect();
        win.global::<CallState>().set_items(ModelRc::new(VecModel::from(items)));
    }
}

/// Flashes the taskbar button until the window is brought to the front.
#[cfg(windows)]
fn flash_taskbar(win: &MainWindow) {
    #[repr(C)]
    struct FlashInfo {
        size: u32,
        hwnd: isize,
        flags: u32,
        count: u32,
        timeout: u32,
    }
    #[link(name = "user32")]
    unsafe extern "system" {
        fn FlashWindowEx(info: *const FlashInfo) -> i32;
    }
    const FLASHW_TRAY: u32 = 0x2;
    const FLASHW_TIMERNOFG: u32 = 0xC;

    let Some(hwnd) = crate::shell::hwnd(win) else {
        return;
    };
    let info = FlashInfo {
        size: size_of::<FlashInfo>() as u32,
        hwnd,
        flags: FLASHW_TRAY | FLASHW_TIMERNOFG,
        count: 0,
        timeout: 0,
    };
    // SAFETY: `info` is a valid FLASHWINFO for our own window and outlives the call.
    unsafe {
        FlashWindowEx(&info);
    }
}

#[cfg(not(windows))]
fn flash_taskbar(_win: &MainWindow) {}

#[cfg(debug_assertions)]
impl App {
    /// Demo (`WACLIENT_DEMO_UI=call-incoming|call-incoming-video-group`):
    /// injects a fake ringing call into the active profile.
    pub(super) fn demo_call(&self, what: &str) {
        let id = self.p().id.clone();
        let ev = if what == "call-incoming-video-group" {
            wa_engine::UiEvent::IncomingCall {
                call_id: "demo-call-2".into(),
                chat_jid: "999@g.us".into(),
                caller_name: "Alice".into(),
                is_video: true,
                is_group: true,
            }
        } else {
            wa_engine::UiEvent::IncomingCall {
                call_id: "demo-call-1".into(),
                chat_jid: "111@s.whatsapp.net".into(),
                caller_name: "Alice".into(),
                is_video: false,
                is_group: false,
            }
        };
        self.on_event(&id, ev);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(profile: &str, call_id: &str) -> CallEntry {
        CallEntry {
            profile: profile.into(),
            profile_name: format!("{profile} name"),
            call_id: call_id.into(),
            chat_jid: "1@s.whatsapp.net".into(),
            caller: "Alice".into(),
            group_name: None,
            is_video: false,
            is_group: false,
            declining: false,
            missed: false,
        }
    }

    #[test]
    fn title_for_direct_and_group_calls() {
        assert_eq!(banner_title("Alice", false, None), "Alice");
        assert_eq!(banner_title("Alice", true, Some("Team")), "Alice · Team");
        assert_eq!(banner_title("Alice", true, None), "Alice");
        assert_eq!(banner_title("Alice", true, Some("  ")), "Alice");
        assert_eq!(banner_title("Alice", false, Some("Team")), "Alice");
    }

    #[test]
    fn subtitle_kind_and_profile() {
        assert_eq!(banner_subtitle(false, false, None), "Incoming voice call");
        assert_eq!(banner_subtitle(true, false, None), "Incoming video call");
        assert_eq!(banner_subtitle(true, false, Some("Work")), "Incoming video call · Work");
        assert_eq!(banner_subtitle(false, true, None), "Missed voice call");
    }

    #[test]
    fn newest_call_goes_on_top() {
        let mut l = Vec::new();
        stack_insert(&mut l, entry("a", "1"), 3);
        stack_insert(&mut l, entry("b", "2"), 3);
        assert_eq!(l[0].call_id, "2");
        assert_eq!(l[1].call_id, "1");
    }

    #[test]
    fn stack_is_capped_and_drops_the_oldest() {
        let mut l = Vec::new();
        for i in 0..5 {
            stack_insert(&mut l, entry("a", &i.to_string()), 3);
        }
        let ids: Vec<_> = l.iter().map(|e| e.call_id.as_str()).collect();
        assert_eq!(ids, ["4", "3", "2"]);
    }

    #[test]
    fn same_call_id_in_two_profiles_is_two_banners() {
        let mut l = Vec::new();
        stack_insert(&mut l, entry("a", "1"), 3);
        stack_insert(&mut l, entry("b", "1"), 3);
        assert_eq!(l.len(), 2);
        assert_ne!(l[0].key(), l[1].key());
    }

    #[test]
    fn repeated_call_updates_in_place() {
        let mut l = Vec::new();
        stack_insert(&mut l, entry("a", "1"), 3);
        stack_insert(&mut l, entry("a", "2"), 3);
        let mut again = entry("a", "1");
        again.caller = "Bob".into();
        stack_insert(&mut l, again, 3);
        assert_eq!(l.len(), 2);
        assert_eq!(l[1].caller, "Bob");
    }

    #[test]
    fn decline_failure_re_enables_the_button() {
        let mut l = Vec::new();
        let mut a = entry("a", "1");
        a.declining = true;
        let mut b = entry("b", "1");
        b.declining = true;
        stack_insert(&mut l, a, 3);
        stack_insert(&mut l, b, 3);
        assert!(stack_decline_failed(&mut l, "a", "1"));
        assert!(!stack_decline_failed(&mut l, "a", "9"));
        assert!(!l.iter().find(|e| e.profile == "a").expect("a").declining);
        assert!(l.iter().find(|e| e.profile == "b").expect("b").declining);
    }

    #[test]
    fn remove_matches_profile_and_call() {
        let mut l = Vec::new();
        stack_insert(&mut l, entry("a", "1"), 3);
        stack_insert(&mut l, entry("b", "1"), 3);
        assert!(!stack_remove(&mut l, "c", "1"));
        assert!(stack_remove(&mut l, "a", "1"));
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].profile, "b");
    }
}
