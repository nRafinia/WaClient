//! Keyboard shortcuts that work on every keyboard layout.
//!
//! Slint matches Ctrl shortcuts (Ctrl+A/C/V/X/Z/Y in text fields, and every
//! `e.text == "f"` style check in the .slint files) on the text the key
//! produces. With a non-Latin layout active (Persian, Arabic, Russian, Hebrew)
//! Windows produces that layout's letter, so none of them matched. Slint's
//! KeyEvent exposes no scan code, but winit's raw event does: this filter turns
//! Ctrl + a physical letter / digit key into the same key press with the Latin
//! character the key has on a US layout, before Slint sees it.

use std::cell::Cell;
use std::rc::Rc;

use slint::winit_030::winit::event::{ElementState, KeyEvent, WindowEvent};
use slint::winit_030::winit::keyboard::{Key, KeyCode, ModifiersState, PhysicalKey};

/// The US-layout character of a physical letter, digit or period key.
fn latin_of(code: KeyCode) -> Option<char> {
    use KeyCode::*;
    Some(match code {
        KeyA => 'a', KeyB => 'b', KeyC => 'c', KeyD => 'd', KeyE => 'e', KeyF => 'f',
        KeyG => 'g', KeyH => 'h', KeyI => 'i', KeyJ => 'j', KeyK => 'k', KeyL => 'l',
        KeyM => 'm', KeyN => 'n', KeyO => 'o', KeyP => 'p', KeyQ => 'q', KeyR => 'r',
        KeyS => 's', KeyT => 't', KeyU => 'u', KeyV => 'v', KeyW => 'w', KeyX => 'x',
        KeyY => 'y', KeyZ => 'z',
        Digit0 => '0', Digit1 => '1', Digit2 => '2', Digit3 => '3', Digit4 => '4',
        Digit5 => '5', Digit6 => '6', Digit7 => '7', Digit8 => '8', Digit9 => '9',
        Period => '.',
        _ => return None,
    })
}

/// The text to feed Slint instead of `logical` for Ctrl + `code`, or `None`
/// when the key already produces the Latin character (or is no shortcut).
/// `logical` is what the active layout produced for the key.
fn shortcut_text(ctrl: bool, alt: bool, shift: bool, code: KeyCode, logical: Option<&str>) -> Option<String> {
    // Ctrl+Alt is AltGr on Windows: that is typing, not a shortcut.
    if !ctrl || alt {
        return None;
    }
    let latin = latin_of(code)?;
    // Only non-Latin layouts (Persian, Arabic, Cyrillic, Hebrew, ...) need the
    // remap. Any layout whose key produces ASCII (QWERTY, QWERTZ, AZERTY,
    // Dvorak, ...) already gives Slint the right shortcut letter, which may sit
    // on a different physical key, so it is left alone.
    if logical.is_some_and(|l| l.chars().all(|c| c.is_ascii())) {
        return None;
    }
    Some(match (latin, shift) {
        ('.', true) => ">".to_string(),
        (c, true) => c.to_ascii_uppercase().to_string(),
        (c, false) => c.to_string(),
    })
}

/// Returns the filter state to give to [`remap`]: tracks the modifier keys.
pub fn new_state() -> Rc<Cell<ModifiersState>> {
    Rc::new(Cell::new(ModifiersState::empty()))
}

/// Looks at a raw window event. For a Ctrl shortcut typed on a non-Latin layout it
/// sends Slint the Latin key press and returns `true` (Slint must not see the
/// original); otherwise `false`.
pub fn remap(window: &slint::Window, event: &WindowEvent, mods: &Cell<ModifiersState>) -> bool {
    match event {
        WindowEvent::ModifiersChanged(m) => {
            mods.set(m.state());
            false
        }
        WindowEvent::KeyboardInput { event: KeyEvent { physical_key: PhysicalKey::Code(code), logical_key, state, repeat, .. }, is_synthetic: false, .. } => {
            let m = mods.get();
            let logical = match logical_key {
                Key::Character(s) => Some(s.as_str()),
                _ => None,
            };
            let Some(text) = shortcut_text(m.control_key(), m.alt_key(), m.shift_key(), *code, logical) else {
                return false;
            };
            let text: slint::SharedString = text.as_str().into();
            use slint::platform::WindowEvent as Ev;
            window.dispatch_event(match (state, repeat) {
                (ElementState::Released, _) => Ev::KeyReleased { text },
                (ElementState::Pressed, true) => Ev::KeyPressRepeated { text },
                (ElementState::Pressed, false) => Ev::KeyPressed { text },
            });
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persian_ctrl_letters_become_latin() {
        // Ctrl+C on the Persian layout produces "ز".
        assert_eq!(shortcut_text(true, false, false, KeyCode::KeyC, Some("ز")).as_deref(), Some("c"));
        assert_eq!(shortcut_text(true, false, false, KeyCode::KeyX, Some("ط")).as_deref(), Some("x"));
        assert_eq!(shortcut_text(true, false, false, KeyCode::KeyZ, None).as_deref(), Some("z"));
        assert_eq!(shortcut_text(true, false, true, KeyCode::KeyZ, Some("ظ")).as_deref(), Some("Z"));
    }

    #[test]
    fn latin_layout_and_plain_typing_are_untouched() {
        assert_eq!(shortcut_text(true, false, false, KeyCode::KeyC, Some("c")), None);
        assert_eq!(shortcut_text(true, false, false, KeyCode::KeyC, Some("C")), None);
        assert_eq!(shortcut_text(false, false, false, KeyCode::KeyC, Some("ز")), None);
        // AltGr (Ctrl+Alt) types characters.
        assert_eq!(shortcut_text(true, true, false, KeyCode::KeyC, Some("ز")), None);
        // Not a letter/digit key.
        assert_eq!(shortcut_text(true, false, false, KeyCode::Enter, None), None);
    }

    #[test]
    fn non_qwerty_latin_layouts_are_untouched() {
        // QWERTZ: the Z key is physical KeyY.
        assert_eq!(shortcut_text(true, false, false, KeyCode::KeyY, Some("z")), None);
        // AZERTY: A on physical KeyQ, Z on physical KeyW.
        assert_eq!(shortcut_text(true, false, false, KeyCode::KeyQ, Some("a")), None);
        assert_eq!(shortcut_text(true, false, false, KeyCode::KeyW, Some("z")), None);
        // Dvorak: C on physical KeyI.
        assert_eq!(shortcut_text(true, false, false, KeyCode::KeyI, Some("c")), None);
        // Russian / Hebrew still remap.
        assert_eq!(shortcut_text(true, false, false, KeyCode::KeyC, Some("с")).as_deref(), Some("c"));
        assert_eq!(shortcut_text(true, false, false, KeyCode::KeyC, Some("ב")).as_deref(), Some("c"));
    }

    #[test]
    fn digits_and_period() {
        assert_eq!(shortcut_text(true, false, false, KeyCode::Digit1, Some("۱")).as_deref(), Some("1"));
        assert_eq!(shortcut_text(true, false, true, KeyCode::Period, Some("ظ")).as_deref(), Some(">"));
        assert_eq!(shortcut_text(true, false, false, KeyCode::Digit2, Some("2")), None);
    }
}
