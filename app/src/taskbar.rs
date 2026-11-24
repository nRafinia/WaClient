//! Unread-count badge on the Windows taskbar button (ITaskbarList3 overlay icon).

use std::cell::RefCell;
use std::ffi::c_void;
use std::time::Duration;

use windows::Win32::Foundation::{HWND, RPC_E_CHANGED_MODE};
use windows::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject, HGDIOBJ};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::Win32::UI::Shell::{ITaskbarList3, TaskbarList};
use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, DestroyIcon, HICON, ICONINFO};
use windows::core::{HSTRING, PCWSTR};

/// The overlay is drawn at twice its 16 px logical size; Windows scales it down.
const ICON_SIZE: u32 = 32;
/// The taskbar button may not exist yet at startup; re-apply once shortly after.
const REAPPLY_DELAY: Duration = Duration::from_secs(3);

const RED: [u8; 3] = [0xE5, 0x39, 0x35];

#[derive(Default)]
struct State {
    taskbar: Option<ITaskbarList3>,
    /// COM setup failed; stop trying (already logged once).
    broken: bool,
    icon: Option<HICON>,
    wanted: u32,
    /// Window and count the shell currently shows.
    applied: Option<(isize, u32)>,
    reapply_scheduled: bool,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

/// Shows `count` on the taskbar button of `hwnd` (0 clears it). UI thread only.
/// Without a window handle yet, the count is remembered and applied on the next call.
pub fn set_unread(hwnd: Option<isize>, count: u32) {
    let schedule = STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.wanted = count;
        match hwnd {
            Some(h) => {
                apply(&mut s, h, false);
                !std::mem::replace(&mut s.reapply_scheduled, true)
            }
            None => false,
        }
    });
    if let (true, Some(h)) = (schedule, hwnd) {
        slint::Timer::single_shot(REAPPLY_DELAY, move || {
            STATE.with(|s| apply(&mut s.borrow_mut(), h, true));
        });
    }
}

fn apply(s: &mut State, hwnd: isize, force: bool) {
    if s.broken || (!force && s.applied == Some((hwnd, s.wanted))) {
        return;
    }
    if s.taskbar.is_none() {
        match init_taskbar() {
            Ok(t) => s.taskbar = Some(t),
            Err(e) => {
                log::warn!("taskbar badge unavailable: {e}");
                s.broken = true;
                return;
            }
        }
    }
    let Some(tb) = s.taskbar.clone() else { return };
    let count = s.wanted;
    let new_icon = if count == 0 {
        None
    } else {
        match make_hicon(count) {
            Ok(i) => Some(i),
            Err(e) => {
                log::warn!("taskbar badge icon failed: {e}");
                return;
            }
        }
    };
    let description = if count == 0 { HSTRING::new() } else { HSTRING::from(badge_description(count)) };
    let hicon = new_icon.unwrap_or_default();
    // SAFETY: `hwnd` is our own top-level window, `hicon` is null or a live icon we own, and
    // `description` outlives the call.
    let res = unsafe { tb.SetOverlayIcon(HWND(hwnd as *mut c_void), hicon, PCWSTR(description.as_ptr())) };
    match res {
        Ok(()) => {
            s.applied = Some((hwnd, count));
            if let Some(old) = std::mem::replace(&mut s.icon, new_icon) {
                // SAFETY: `old` was created by us and the shell no longer shows it.
                unsafe {
                    let _ = DestroyIcon(old);
                }
            }
        }
        Err(e) => {
            log::warn!("taskbar overlay icon failed: {e}");
            s.applied = None;
            if let Some(i) = new_icon {
                // SAFETY: the shell did not take the icon, so only we own it.
                unsafe {
                    let _ = DestroyIcon(i);
                }
            }
        }
    }
}

fn init_taskbar() -> windows::core::Result<ITaskbarList3> {
    // SAFETY: plain COM initialisation on the UI thread; an already-initialised
    // apartment of another mode is fine for our purposes.
    unsafe {
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        if hr.is_err() && hr != RPC_E_CHANGED_MODE {
            return Err(hr.into());
        }
        let tb: ITaskbarList3 = CoCreateInstance(&TaskbarList, None, CLSCTX_INPROC_SERVER)?;
        tb.HrInit()?;
        Ok(tb)
    }
}

fn make_hicon(count: u32) -> windows::core::Result<HICON> {
    let rgba = badge_rgba(count, ICON_SIZE);
    // GDI wants premultiplied BGRA for 32 bpp icons with alpha.
    let mut bgra = Vec::with_capacity(rgba.len());
    for p in rgba.chunks_exact(4) {
        let a = p[3] as u32;
        let pm = |c: u8| ((c as u32 * a + 127) / 255) as u8;
        bgra.extend_from_slice(&[pm(p[2]), pm(p[1]), pm(p[0]), p[3]]);
    }
    let mask = vec![0u8; (ICON_SIZE * ICON_SIZE / 8) as usize];
    let size = ICON_SIZE as i32;
    // SAFETY: both buffers are at least as large as the bitmaps they initialise (the
    // 1 bpp mask rows are 32 bits wide, so already word aligned).
    let (color, mask) = unsafe {
        (
            CreateBitmap(size, size, 1, 32, Some(bgra.as_ptr() as *const c_void)),
            CreateBitmap(size, size, 1, 1, Some(mask.as_ptr() as *const c_void)),
        )
    };
    let info = ICONINFO { fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
    // SAFETY: `info` holds the two bitmaps created above; the icon copies them.
    let icon = unsafe { CreateIconIndirect(&info) };
    // SAFETY: the bitmaps are ours and no longer needed after the icon copied them.
    unsafe {
        let _ = DeleteObject(HGDIOBJ(color.0));
        let _ = DeleteObject(HGDIOBJ(mask.0));
    }
    icon
}

/// Text shown inside the badge: the count, or "99+" above 99.
fn badge_label(count: u32) -> String {
    if count > 99 { "99+".to_string() } else { count.to_string() }
}

fn badge_description(count: u32) -> String {
    if count == 1 { "1 unread message".to_string() } else { format!("{count} unread messages") }
}

/// 5x7 glyphs, one byte per row, bit 4 = leftmost column.
fn glyph(c: char) -> [u8; 7] {
    match c {
        '0' => [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110],
        '1' => [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        '2' => [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111],
        '3' => [0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110],
        '4' => [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
        '5' => [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110],
        '6' => [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
        '7' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000],
        '8' => [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
        '9' => [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100],
        '+' => [0b00000, 0b00100, 0b00100, 0b11111, 0b00100, 0b00100, 0b00000],
        _ => [0; 7],
    }
}

/// Renders the badge: red disc with the white label. Returns `size * size * 4` RGBA bytes.
fn badge_rgba(count: u32, size: u32) -> Vec<u8> {
    let s = size as f32;
    let c = s / 2.0;
    let label: Vec<char> = badge_label(count).chars().collect();
    // Glyph cell width, height and gap as fractions of the icon size: one digit is
    // large, two are narrower, "99+" narrowest.
    let (gw, gh, gap) = match label.len() {
        1 => (0.50, 0.75, 0.0),
        2 => (0.41, 0.69, 0.06),
        _ => (0.28, 0.50, 0.03),
    };
    let (gw, gh, gap) = (gw * s, gh * s, gap * s);
    let total = label.len() as f32 * gw + (label.len() as f32 - 1.0) * gap;
    let (x0, y0) = ((s - total) / 2.0, (s - gh) / 2.0);

    let mut px = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let d = ((fx - c).powi(2) + (fy - c).powi(2)).sqrt();
            let alpha = (c - d + 0.5).clamp(0.0, 1.0);
            if alpha <= 0.0 {
                continue;
            }
            let mut color = RED;
            let (rx, ry) = (fx - x0, fy - y0);
            if ry >= 0.0 && ry < gh && rx >= 0.0 {
                let cell = gw + gap;
                let idx = (rx / cell) as usize;
                let within = rx - idx as f32 * cell;
                if idx < label.len() && within < gw {
                    let col = ((within / gw) * 5.0) as u32;
                    let row = ((ry / gh) * 7.0) as usize;
                    if glyph(label[idx])[row.min(6)] >> (4 - col.min(4)) & 1 == 1 {
                        color = [0xFF; 3];
                    }
                }
            }
            let i = ((y * size + x) * 4) as usize;
            px[i..i + 4].copy_from_slice(&[color[0], color[1], color[2], (alpha * 255.0).round() as u8]);
        }
    }
    px
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        for (n, want) in [(0, "0"), (1, "1"), (9, "9"), (10, "10"), (99, "99"), (100, "99+"), (1000, "99+")] {
            assert_eq!(badge_label(n), want);
        }
    }

    #[test]
    fn descriptions() {
        assert_eq!(badge_description(1), "1 unread message");
        assert_eq!(badge_description(5), "5 unread messages");
    }

    #[test]
    fn buffer_shape() {
        for n in [1, 8, 42, 150] {
            assert_eq!(badge_rgba(n, 32).len(), 32 * 32 * 4);
        }
        assert_eq!(badge_rgba(3, 16).len(), 16 * 16 * 4);
    }

    #[test]
    fn corners_transparent_centre_opaque() {
        let px = badge_rgba(7, 32);
        for (x, y) in [(0u32, 0u32), (31, 0), (0, 31), (31, 31)] {
            assert_eq!(px[((y * 32 + x) * 4 + 3) as usize], 0);
        }
        assert_ne!(px[((16 * 32 + 16) * 4 + 3) as usize], 0);
    }

    #[test]
    fn digits_differ_and_have_white_text() {
        let one = badge_rgba(1, 32);
        let eight = badge_rgba(8, 32);
        assert_ne!(one, eight);
        for buf in [&one, &eight, &badge_rgba(99, 32), &badge_rgba(100, 32)] {
            assert!(buf.chunks_exact(4).any(|p| p[..3] == [0xFF; 3] && p[3] == 0xFF));
        }
    }
}
