//! Keysym mapping: winit logical keys -> X keysyms.
//!
//! The wire carries keysyms (layout-independent intent), so what the user's
//! laptop keyboard looks like does not have to match the remote layout.

/// X keysyms beyond Latin-1 (which maps to its code points).
pub const XK_RETURN: u32 = 0xff0d;
pub const XK_TAB: u32 = 0xff09;
pub const XK_ESCAPE: u32 = 0xff1b;
pub const XK_BACKSPACE: u32 = 0xff08;
pub const XK_DELETE: u32 = 0xffff;
pub const XK_LEFT: u32 = 0xff51;
pub const XK_UP: u32 = 0xff52;
pub const XK_RIGHT: u32 = 0xff53;
pub const XK_DOWN: u32 = 0xff54;
pub const XK_HOME: u32 = 0xff50;
pub const XK_END: u32 = 0xff57;
pub const XK_PAGE_UP: u32 = 0xff55;
pub const XK_PAGE_DOWN: u32 = 0xff56;
pub const XK_SHIFT_L: u32 = 0xffe1;
pub const XK_CONTROL_L: u32 = 0xffe3;
pub const XK_ALT_L: u32 = 0xffe9;
pub const XK_SUPER_L: u32 = 0xffeb;
pub const XK_ISO_LEVEL3_SHIFT: u32 = 0xfe03;
pub const XK_F1: u32 = 0xffbe;
pub const XK_CAPS_LOCK: u32 = 0xffe5;
pub const XK_INSERT: u32 = 0xff63;
pub const XK_PRINT: u32 = 0xff61;
pub const XK_KP_ADD: u32 = 0xffab;
pub const XK_KP_SUBTRACT: u32 = 0xffad;
pub const XK_KP_MULTIPLY: u32 = 0xffaa;
pub const XK_KP_DIVIDE: u32 = 0xffaf;
pub const XK_KP_DECIMAL: u32 = 0xffae;
pub const XK_KP_ENTER: u32 = 0xff8d;
pub const XK_KP_EQUAL: u32 = 0xffbd;
pub const XK_KP_0: u32 = 0xffb0;

use winit::keyboard::{Key, KeyCode, NamedKey};

/// Numpad keys are only exposed as physical codes in winit 0.30.
pub fn keysym_for_physical(code: KeyCode) -> Option<u32> {
    Some(match code {
        KeyCode::Numpad0 => XK_KP_0,
        KeyCode::Numpad1 => XK_KP_0 + 1,
        KeyCode::Numpad2 => XK_KP_0 + 2,
        KeyCode::Numpad3 => XK_KP_0 + 3,
        KeyCode::Numpad4 => XK_KP_0 + 4,
        KeyCode::Numpad5 => XK_KP_0 + 5,
        KeyCode::Numpad6 => XK_KP_0 + 6,
        KeyCode::Numpad7 => XK_KP_0 + 7,
        KeyCode::Numpad8 => XK_KP_0 + 8,
        KeyCode::Numpad9 => XK_KP_0 + 9,
        KeyCode::NumpadAdd => XK_KP_ADD,
        KeyCode::NumpadSubtract => XK_KP_SUBTRACT,
        KeyCode::NumpadMultiply => XK_KP_MULTIPLY,
        KeyCode::NumpadDivide => XK_KP_DIVIDE,
        KeyCode::NumpadDecimal => XK_KP_DECIMAL,
        KeyCode::NumpadEnter => XK_KP_ENTER,
        KeyCode::NumpadEqual => XK_KP_EQUAL,
        _ => return None,
    })
}

/// Translate a winit logical key to an X keysym, if it is representable.
pub fn keysym_for(key: &Key) -> Option<u32> {
    match key {
        Key::Character(s) => {
            let c = s.chars().next()?;
            let cp = c as u32;
            match cp {
                0x20..=0xff => Some(cp),
                // X11 Unicode keysym; the server remaps a spare keycode.
                0x100..=0x10ffff => Some(0x0100_0000 + cp),
                _ => None,
            }
        }
        Key::Named(named) => Some(match named {
            NamedKey::Enter => XK_RETURN,
            NamedKey::Tab => XK_TAB,
            NamedKey::Escape => XK_ESCAPE,
            NamedKey::Backspace => XK_BACKSPACE,
            NamedKey::Delete => XK_DELETE,
            NamedKey::ArrowLeft => XK_LEFT,
            NamedKey::ArrowUp => XK_UP,
            NamedKey::ArrowRight => XK_RIGHT,
            NamedKey::ArrowDown => XK_DOWN,
            NamedKey::Home => XK_HOME,
            NamedKey::End => XK_END,
            NamedKey::PageUp => XK_PAGE_UP,
            NamedKey::PageDown => XK_PAGE_DOWN,
            NamedKey::Space => 0x20,
            NamedKey::Shift => XK_SHIFT_L,
            NamedKey::Control => XK_CONTROL_L,
            NamedKey::Alt => XK_ALT_L,
            NamedKey::Super => XK_SUPER_L,
            NamedKey::AltGraph => XK_ISO_LEVEL3_SHIFT,
            NamedKey::CapsLock => XK_CAPS_LOCK,
            NamedKey::Insert => XK_INSERT,
            NamedKey::PrintScreen => XK_PRINT,
            NamedKey::NumLock => 0xff7f,
            NamedKey::ScrollLock => 0xff14,
            NamedKey::Pause => 0xff13,
            NamedKey::ContextMenu => 0xff67,
            NamedKey::F1 => XK_F1,
            NamedKey::F2 => XK_F1 + 1,
            NamedKey::F3 => XK_F1 + 2,
            NamedKey::F4 => XK_F1 + 3,
            NamedKey::F5 => XK_F1 + 4,
            NamedKey::F6 => XK_F1 + 5,
            NamedKey::F7 => XK_F1 + 6,
            NamedKey::F8 => XK_F1 + 7,
            NamedKey::F9 => XK_F1 + 8,
            NamedKey::F10 => XK_F1 + 9,
            NamedKey::F11 => XK_F1 + 10,
            NamedKey::F12 => XK_F1 + 11,
            NamedKey::F13 => XK_F1 + 12,
            NamedKey::F14 => XK_F1 + 13,
            NamedKey::F15 => XK_F1 + 14,
            NamedKey::F16 => XK_F1 + 15,
            NamedKey::F17 => XK_F1 + 16,
            NamedKey::F18 => XK_F1 + 17,
            NamedKey::F19 => XK_F1 + 18,
            NamedKey::F20 => XK_F1 + 19,
            NamedKey::F21 => XK_F1 + 20,
            NamedKey::F22 => XK_F1 + 21,
            NamedKey::F23 => XK_F1 + 22,
            NamedKey::F24 => XK_F1 + 23,
            _ => return None,
        }),
        _ => None,
    }
}

/// Keysym for the text the OS produced.
///
/// This is the most reliable signal for *characters*: Wine can mis-detect the
/// keyboard layout and report nonsense named keys (q → MediaTrackPrevious,
/// w → F17) while the text is correct. Control codes from Ctrl+letter are
/// mapped back to the letter, because the physical Ctrl is already held on the
/// remote. Ambiguous C0 codes (Tab, Enter, Backspace, Escape, …) return None so
/// the named-key mapping handles them.
pub fn text_keysym(text: Option<&str>) -> Option<u32> {
    let mut chars = text?.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None; // multi-char IME/compose commits have no single keysym
    }
    let cp = c as u32;
    match cp {
        0x01..=0x1a if !matches!(cp, 0x08 | 0x09 | 0x0a | 0x0d) => Some('a' as u32 + cp - 1),
        0x20..=0x7e | 0xa0..=0xff => Some(cp),
        0x100..=0x10ffff => Some(0x0100_0000 + cp),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::keyboard::{Key, KeyCode, NamedKey};

    #[test]
    fn text_preferred_over_named_key() {
        // Wine's mis-detected session: named key is garbage, text is right.
        assert_eq!(text_keysym(Some("q")), Some(0x71));
        assert_eq!(text_keysym(Some("1")), Some(0x31));
        assert_eq!(text_keysym(Some(" ")), Some(0x20));
        assert_eq!(text_keysym(Some("é")), Some(0xe9));
        assert_eq!(text_keysym(Some("λ")), Some(0x0100_0000 + 0x3bb));
        // Ctrl+letter control codes become the letter (Ctrl is held remotely).
        assert_eq!(text_keysym(Some("\u{3}")), Some(0x63)); // Ctrl+C
        // Ambiguous control codes are left to the named-key mapping.
        assert_eq!(text_keysym(Some("\u{1b}")), None);
        assert_eq!(text_keysym(Some("\t")), None);
        assert_eq!(text_keysym(Some("\r")), None);
        assert_eq!(text_keysym(Some("\u{8}")), None);
        // Multi-char / DEL are not a single keysym.
        assert_eq!(text_keysym(Some("ab")), None);
        assert_eq!(text_keysym(Some("\u{7f}")), None);
        assert_eq!(text_keysym(None), None);
    }

    #[test]
    fn character_keys() {
        assert_eq!(keysym_for(&Key::Character("e".into())), Some(0x65));
        assert_eq!(keysym_for(&Key::Character("E".into())), Some(0x45));
        assert_eq!(keysym_for(&Key::Character("1".into())), Some(0x31));
        assert_eq!(keysym_for(&Key::Character(" ".into())), Some(0x20));
        assert_eq!(keysym_for(&Key::Character("é".into())), Some(0xe9));
    }

    #[test]
    fn unicode_and_physical() {
        // Non-Latin-1 goes to the X11 Unicode keysym.
        assert_eq!(
            keysym_for(&Key::Character("λ".into())),
            Some(0x0100_0000 + 0x3bb)
        );
        assert_eq!(keysym_for(&Key::Character("😀".into())), Some(0x0100_0000 + 0x1f600));
        // Numpad keys are physical-only in winit 0.30.
        assert_eq!(keysym_for_physical(KeyCode::Numpad7), Some(XK_KP_0 + 7));
        assert_eq!(keysym_for_physical(KeyCode::NumpadEnter), Some(XK_KP_ENTER));
        assert_eq!(keysym_for_physical(KeyCode::KeyA), None);
    }

    #[test]
    fn named_keys() {
        assert_eq!(keysym_for(&Key::Named(NamedKey::Enter)), Some(XK_RETURN));
        assert_eq!(keysym_for(&Key::Named(NamedKey::Escape)), Some(XK_ESCAPE));
        assert_eq!(keysym_for(&Key::Named(NamedKey::ArrowLeft)), Some(XK_LEFT));
        assert_eq!(keysym_for(&Key::Named(NamedKey::Space)), Some(0x20));
        assert_eq!(keysym_for(&Key::Named(NamedKey::Shift)), Some(XK_SHIFT_L));
        assert_eq!(keysym_for(&Key::Named(NamedKey::F5)), Some(XK_F1 + 4));
        assert_eq!(
            keysym_for(&Key::Named(NamedKey::CapsLock)),
            Some(XK_CAPS_LOCK)
        );
        assert_eq!(
            keysym_for(&Key::Named(NamedKey::PrintScreen)),
            Some(XK_PRINT)
        );
    }
}
