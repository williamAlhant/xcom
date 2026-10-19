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

use winit::keyboard::{Key, NamedKey};

/// Translate a winit logical key to an X keysym, if it is representable.
pub fn keysym_for(key: &Key) -> Option<u32> {
    match key {
        Key::Character(s) => {
            let c = s.chars().next()?;
            let c = c as u32;
            (0x20..0x100).contains(&c).then_some(c)
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
            _ => return None,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::keyboard::{Key, NamedKey};

    #[test]
    fn character_keys() {
        assert_eq!(keysym_for(&Key::Character("e".into())), Some(0x65));
        assert_eq!(keysym_for(&Key::Character("E".into())), Some(0x45));
        assert_eq!(keysym_for(&Key::Character("1".into())), Some(0x31));
        assert_eq!(keysym_for(&Key::Character(" ".into())), Some(0x20));
        assert_eq!(keysym_for(&Key::Character("é".into())), Some(0xe9));
    }

    #[test]
    fn named_keys() {
        assert_eq!(keysym_for(&Key::Named(NamedKey::Enter)), Some(XK_RETURN));
        assert_eq!(keysym_for(&Key::Named(NamedKey::Escape)), Some(XK_ESCAPE));
        assert_eq!(keysym_for(&Key::Named(NamedKey::ArrowLeft)), Some(XK_LEFT));
        assert_eq!(keysym_for(&Key::Named(NamedKey::Space)), Some(0x20));
        assert_eq!(keysym_for(&Key::Named(NamedKey::Shift)), Some(XK_SHIFT_L));
        assert_eq!(keysym_for(&Key::Named(NamedKey::F5)), Some(XK_F1 + 4));
        assert_eq!(keysym_for(&Key::Named(NamedKey::CapsLock)), None);
    }
}
