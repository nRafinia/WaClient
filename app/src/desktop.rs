//! Windows desktop integration: toast notifications and a system-tray icon.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use tauri_winrt_notification::{Sound, Toast};
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

const AUMID: &str = "WaClient.Desktop";
const DISPLAY_NAME: &str = "WaClient";
const ICON_FILE: &str = "waclient-icon.png";
const TRAY_SIZE: u32 = 32;
const FILE_ICON_SIZE: u32 = 128;
const MAX_BODY_CHARS: usize = 200;

const GREEN: [u8; 3] = [0x25, 0xD3, 0x66];
const WHITE: [u8; 3] = [0xFF, 0xFF, 0xFF];
const RED: [u8; 3] = [0xE5, 0x39, 0x35];

/// Events produced by the tray icon and by toast activation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DesktopEvent {
    OpenWindow,
    OpenChat { profile: String, chat_jid: String },
    Quit,
}

type Callback = Arc<dyn Fn(DesktopEvent) + Send + Sync>;
type SharedCallback = Arc<Mutex<Option<Callback>>>;

fn dispatch(cb: &SharedCallback, ev: DesktopEvent) {
    let f = cb.lock().ok().and_then(|g| g.clone());
    if let Some(f) = f {
        f(ev);
    }
}

pub struct Desktop {
    tray: TrayIcon,
    callback: SharedCallback,
    icon_plain: Icon,
    icon_badge: Icon,
    icon_path: PathBuf,
}

impl Desktop {
    /// Registers the AppUserModelID, creates the tray icon and menu.
    /// Must be called on the main (UI) thread, which has to pump Win32 messages
    /// (Slint's winit event loop does).
    pub fn init(data_dir: &Path) -> anyhow::Result<Desktop> {
        std::fs::create_dir_all(data_dir).context("creating data dir")?;
        let icon_path = data_dir.join(ICON_FILE);
        write_icon_file(&icon_path).context("writing app icon")?;
        register_aumid(&icon_path).context("registering AppUserModelID")?;
        set_process_aumid().context("setting process AppUserModelID")?;

        let icon_plain = make_icon(TRAY_SIZE, false)?;
        let icon_badge = make_icon(TRAY_SIZE, true)?;

        let open = MenuItem::new("Open WaClient", true, None);
        let quit = MenuItem::new("Quit", true, None);
        let menu = Menu::new();
        menu.append(&open).context("menu append")?;
        menu.append(&quit).context("menu append")?;
        let open_id = open.id().clone();
        let quit_id = quit.id().clone();

        let callback: SharedCallback = Arc::new(Mutex::new(None));

        let cb = callback.clone();
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            if e.id == open_id {
                dispatch(&cb, DesktopEvent::OpenWindow);
            } else if e.id == quit_id {
                dispatch(&cb, DesktopEvent::Quit);
            }
        }));

        // A double click also produces two single clicks, so reacting to the
        // left-button release alone covers both.
        let cb = callback.clone();
        TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = e
            {
                dispatch(&cb, DesktopEvent::OpenWindow);
            }
        }));

        let tray = TrayIconBuilder::new()
            .with_icon(icon_plain.clone())
            .with_tooltip(tooltip_text(0))
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build()
            .context("creating tray icon")?;

        Ok(Desktop {
            tray,
            callback,
            icon_plain,
            icon_badge,
            icon_path,
        })
    }

    /// Registers the event callback (replaces any previous one).
    ///
    /// The callback may be invoked from non-UI threads (tray handlers and the
    /// WinRT toast thread pool). The consumer must hop to the UI thread with
    /// `slint::invoke_from_event_loop` before touching UI state.
    pub fn on_event(&self, f: impl Fn(DesktopEvent) + Send + Sync + 'static) {
        if let Ok(mut g) = self.callback.lock() {
            *g = Some(Arc::new(f));
        }
    }

    /// Shows a toast; clicking it emits `DesktopEvent::OpenChat` for the chat
    /// of the given profile. Non-blocking; failures are logged only. `sound`
    /// off makes the toast silent.
    pub fn notify(&self, title: &str, body: &str, profile_id: &str, chat_jid: &str, sound: bool) {
        let body = truncate_body(body, MAX_BODY_CHARS);
        let jid = chat_jid.to_string();
        let profile = profile_id.to_string();
        let cb = self.callback.clone();
        let toast = Toast::new(AUMID)
            .title(title)
            .text1(&body)
            .sound(sound.then_some(Sound::Default))
            .icon(
                &self.icon_path,
                tauri_winrt_notification::IconCrop::Circular,
                DISPLAY_NAME,
            )
            .on_activated(move |_| {
                dispatch(&cb, DesktopEvent::OpenChat { profile: profile.clone(), chat_jid: jid.clone() });
                Ok(())
            });
        if let Err(e) = toast.show() {
            log::warn!("toast failed: {e}");
        }
    }

    /// Updates the tray tooltip and swaps the icon to the red-dot variant when unread > 0.
    pub fn set_unread_badge(&self, total_unread: u32) {
        if let Err(e) = self.tray.set_tooltip(Some(tooltip_text(total_unread))) {
            log::warn!("tray tooltip failed: {e}");
        }
        let icon = if total_unread > 0 {
            &self.icon_badge
        } else {
            &self.icon_plain
        };
        if let Err(e) = self.tray.set_icon(Some(icon.clone())) {
            log::warn!("tray icon failed: {e}");
        }
    }
}

fn truncate_body(body: &str, max_chars: usize) -> String {
    if body.chars().count() <= max_chars {
        return body.to_string();
    }
    let mut s: String = body.chars().take(max_chars.saturating_sub(1)).collect();
    s.push('\u{2026}');
    s
}

fn tooltip_text(unread: u32) -> String {
    if unread == 0 {
        DISPLAY_NAME.to_string()
    } else {
        format!("{DISPLAY_NAME} \u{2013} {unread} unread")
    }
}

/// Renders the app icon: green disc, white speech bubble with a tail, and an
/// optional red dot in the top-right corner. Returns `size * size * 4` RGBA bytes.
fn icon_rgba(size: u32, badge: bool) -> Vec<u8> {
    let s = size as f32;
    let c = s / 2.0;
    let mut px = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            // Sample at the pixel centre.
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let mut color: Option<[u8; 3]> = None;
            if dist(fx, fy, c, c) <= c {
                color = Some(GREEN);
                let in_bubble = dist(fx, fy, c, c * 0.92) <= s * 0.30
                    || in_triangle(
                        (fx, fy),
                        (s * 0.28, s * 0.62),
                        (s * 0.30, s * 0.82),
                        (s * 0.48, s * 0.72),
                    );
                if in_bubble {
                    color = Some(WHITE);
                }
            }
            if badge && dist(fx, fy, s * 0.76, s * 0.24) <= s * 0.22 {
                color = Some(RED);
            }
            if let Some([r, g, b]) = color {
                let i = ((y * size + x) * 4) as usize;
                px[i..i + 4].copy_from_slice(&[r, g, b, 0xFF]);
            }
        }
    }
    px
}

fn dist(x: f32, y: f32, cx: f32, cy: f32) -> f32 {
    ((x - cx).powi(2) + (y - cy).powi(2)).sqrt()
}

fn in_triangle(p: (f32, f32), a: (f32, f32), b: (f32, f32), c: (f32, f32)) -> bool {
    let sign = |p1: (f32, f32), p2: (f32, f32), p3: (f32, f32)| {
        (p1.0 - p3.0) * (p2.1 - p3.1) - (p2.0 - p3.0) * (p1.1 - p3.1)
    };
    let d1 = sign(p, a, b);
    let d2 = sign(p, b, c);
    let d3 = sign(p, c, a);
    let neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
    !(neg && pos)
}

fn make_icon(size: u32, badge: bool) -> anyhow::Result<Icon> {
    Icon::from_rgba(icon_rgba(size, badge), size, size).context("building tray icon")
}

fn write_icon_file(path: &Path) -> anyhow::Result<()> {
    let img = image::RgbaImage::from_raw(
        FILE_ICON_SIZE,
        FILE_ICON_SIZE,
        icon_rgba(FILE_ICON_SIZE, false),
    )
    .context("icon buffer size mismatch")?;
    img.save_with_format(path, image::ImageFormat::Png)?;
    Ok(())
}

/// Writes HKCU\Software\Classes\AppUserModelId\WaClient.Desktop so toasts from
/// this unpackaged exe are attributed to "WaClient". Idempotent.
fn register_aumid(icon_path: &Path) -> anyhow::Result<()> {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu.create_subkey(format!(r"Software\Classes\AppUserModelId\{AUMID}"))?;
    key.set_value("DisplayName", &DISPLAY_NAME)?;
    key.set_value("IconUri", &icon_path.to_string_lossy().into_owned())?;
    key.set_value("IconBackgroundColor", &"FF25D366")?;
    Ok(())
}

#[link(name = "shell32")]
unsafe extern "system" {
    fn SetCurrentProcessExplicitAppUserModelID(app_id: *const u16) -> i32;
}

fn set_process_aumid() -> anyhow::Result<()> {
    let wide: Vec<u16> = AUMID.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` is a valid NUL-terminated UTF-16 string that outlives the call.
    let hr = unsafe { SetCurrentProcessExplicitAppUserModelID(wide.as_ptr()) };
    if hr < 0 {
        anyhow::bail!("SetCurrentProcessExplicitAppUserModelID failed: HRESULT {hr:#010x}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_unchanged() {
        assert_eq!(truncate_body("hello", 200), "hello");
    }

    #[test]
    fn truncate_is_char_safe() {
        let persian = "سلام ".repeat(100);
        let out = truncate_body(&persian, 200);
        assert_eq!(out.chars().count(), 200);
        assert!(out.ends_with('\u{2026}'));

        let emoji = "😀".repeat(300);
        let out = truncate_body(&emoji, 200);
        assert_eq!(out.chars().count(), 200);
    }

    #[test]
    fn truncate_exact_boundary() {
        let s = "a".repeat(200);
        assert_eq!(truncate_body(&s, 200), s);
    }

    #[test]
    fn tooltip_variants() {
        assert_eq!(tooltip_text(0), "WaClient");
        assert_eq!(tooltip_text(3), "WaClient \u{2013} 3 unread");
    }

    #[test]
    fn icon_dimensions_and_badge() {
        let plain = icon_rgba(32, false);
        let badge = icon_rgba(32, true);
        assert_eq!(plain.len(), 32 * 32 * 4);
        assert_eq!(badge.len(), plain.len());
        assert_ne!(plain, badge);
        // Corner is transparent, centre-ish bubble pixel is opaque.
        assert_eq!(plain[3], 0);
        let mid = ((16 * 32 + 16) * 4) as usize;
        assert_eq!(plain[mid + 3], 0xFF);
    }

    #[test]
    #[ignore]
    fn manual_toast() {
        let dir = std::env::temp_dir().join("waclient-desktop-test");
        let d = Desktop::init(&dir).expect("init");
        d.set_unread_badge(2);
        d.notify("Test", "Hello from WaClient سلام", "p1", "123@s.whatsapp.net", true);
    }
}
