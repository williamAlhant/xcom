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

use std::collections::HashMap;

use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};
use xcom_proto::logln;

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

/// True for physical keys that normally produce a character (letters, digits,
/// punctuation, space) — i.e. keys where a Ctrl/Alt shortcut uses the plain
/// character. Named keys (Enter, arrows, F-keys) are not included.
pub fn is_text_key(code: KeyCode) -> bool {
    matches!(
        code,
        KeyCode::KeyA
            | KeyCode::KeyB
            | KeyCode::KeyC
            | KeyCode::KeyD
            | KeyCode::KeyE
            | KeyCode::KeyF
            | KeyCode::KeyG
            | KeyCode::KeyH
            | KeyCode::KeyI
            | KeyCode::KeyJ
            | KeyCode::KeyK
            | KeyCode::KeyL
            | KeyCode::KeyM
            | KeyCode::KeyN
            | KeyCode::KeyO
            | KeyCode::KeyP
            | KeyCode::KeyQ
            | KeyCode::KeyR
            | KeyCode::KeyS
            | KeyCode::KeyT
            | KeyCode::KeyU
            | KeyCode::KeyV
            | KeyCode::KeyW
            | KeyCode::KeyX
            | KeyCode::KeyY
            | KeyCode::KeyZ
            | KeyCode::Digit0
            | KeyCode::Digit1
            | KeyCode::Digit2
            | KeyCode::Digit3
            | KeyCode::Digit4
            | KeyCode::Digit5
            | KeyCode::Digit6
            | KeyCode::Digit7
            | KeyCode::Digit8
            | KeyCode::Digit9
            | KeyCode::Space
            | KeyCode::Minus
            | KeyCode::Equal
            | KeyCode::BracketLeft
            | KeyCode::BracketRight
            | KeyCode::Backslash
            | KeyCode::Semicolon
            | KeyCode::Quote
            | KeyCode::Comma
            | KeyCode::Period
            | KeyCode::Slash
            | KeyCode::Backquote
            | KeyCode::IntlBackslash
    )
}


// --- key state machine -------------------------------------------------

/// What the UI should do with a key event after local bindings are applied.
#[derive(Debug, PartialEq, Eq)]
pub enum KeyAction {
    None,
    /// Forward this keysym as a press.
    Press(u32),
    /// Forward this keysym as a release.
    Release(u32),
    /// Local shortcut: show/hide the metrics HUD.
    ToggleHud,
}

/// Per-window keyboard state: which keysym each held physical key sent, and the
/// plain (unmodified) character learned for each key.
#[derive(Default)]
pub struct KeyState {
    held: HashMap<PhysicalKey, u32>,
    plain_chars: HashMap<PhysicalKey, u32>,
}

impl KeyState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Release every key we believe is down. A focus loss must never leave a
    /// remote key held: the X server would auto-repeat it forever.
    pub fn release_all(&mut self) -> Vec<u32> {
        self.held.drain().map(|(_, sym)| sym).collect()
    }

    /// Handle one key event.
    ///
    /// `mods` is winit's authoritative modifier state (Wayland reports
    /// modifiers already held when a window gains focus only through
    /// `ModifiersChanged`); the keysym map is kept as a fallback for Wine's
    /// bogus virtual keys.
    pub fn on_key(
        &mut self,
        physical: &PhysicalKey,
        logical: &Key,
        text: Option<&str>,
        pressed: bool,
        repeat: bool,
        mods: ModifiersState,
    ) -> KeyAction {
        // The remote X server auto-repeats a held XTEST key by itself;
        // forwarding local repeats too would double the rate.
        if repeat {
            return KeyAction::None;
        }
        if !pressed {
            // Release exactly what the press sent: a release has no text and
            // Wine may report a bogus named key. Keys whose press sent nothing
            // are not in the map, so they stay silent.
            return match self.held.remove(physical) {
                Some(sym) => KeyAction::Release(sym),
                None => KeyAction::None,
            };
        }

        const CTRL: [u32; 2] = [XK_CONTROL_L, 0xffe4];
        const ALT: [u32; 2] = [XK_ALT_L, 0xffea];
        const SUPER: [u32; 2] = [XK_SUPER_L, 0xffec];
        const SHIFT: [u32; 2] = [XK_SHIFT_L, 0xffe2];
        let held_any = |syms: [u32; 2]| self.held.values().any(|s| syms.contains(s));
        let ctrl = mods.control_key() || held_any(CTRL);
        let shift = mods.shift_key() || held_any(SHIFT);
        let alt_super = mods.alt_key() || mods.super_key() || held_any(ALT) || held_any(SUPER);
        let ctrl_alt = ctrl || alt_super;

        // Alt+Tab, Super/Win+Tab and their Shift/Ctrl variants belong to the
        // local desktop's window switcher. Windows delivers the Tab to the
        // focused window before it performs the switch, so forwarding it would
        // type a Tab in the remote app. Ctrl+Tab and Shift+Tab stay remote
        // input.
        if is_window_switch(logical, physical, alt_super) {
            if xcom_proto::debug_keys() {
                logln!("[ui] Tab with Alt/Super held is a local window switch; not forwarded");
            }
            return KeyAction::None;
        }

        // Local shortcut (never forwarded).
        if is_hud_toggle(logical, physical, ctrl, shift, alt_super) {
            return KeyAction::ToggleHud;
        }

        let text_sym = text_keysym(text);
        // Learn the layout's plain character. ASCII only and with no
        // modifiers, so dead-key accents and shifted symbols cannot pollute the
        // map.
        if !ctrl_alt && !shift {
            if let Some(sym) = text_sym {
                if (0x20..=0x7e).contains(&sym) {
                    self.plain_chars.insert(*physical, sym);
                }
            }
        }

        let logical_sym = keysym_for(logical);
        let sym = if ctrl_alt && text_sym.is_none() {
            if matches!(logical, Key::Character(_)) {
                // Real Windows: the logical key is the layout character even
                // with Ctrl held.
                logical_sym
            } else {
                match physical {
                    PhysicalKey::Code(code) if is_text_key(*code) => {
                        // Wine: bogus named VK. Use the character this key
                        // normally produces; send nothing if the user has not
                        // typed it yet (no wrong guesses).
                        self.plain_chars.get(physical).copied()
                    }
                    _ => logical_sym.or_else(|| match physical {
                        PhysicalKey::Code(code) => keysym_for_physical(*code),
                        _ => None,
                    }),
                }
            }
        } else {
            text_sym.or(logical_sym).or_else(|| match physical {
                PhysicalKey::Code(code) => keysym_for_physical(*code),
                _ => None,
            })
        };

        if xcom_proto::debug_keys() {
            logln!(
                "[ui] key {:?} -> sym={} pressed={} text={:?}",
                logical,
                sym.map(|s| format!("{s:#x}"))
                    .unwrap_or_else(|| "NONE".into()),
                pressed,
                text
            );
        }
        match sym {
            Some(sym) => {
                self.held.insert(*physical, sym);
                KeyAction::Press(sym)
            }
            None => KeyAction::None,
        }
    }
}

/// True when a key event should toggle the metrics HUD (Ctrl+Shift+M).
///
/// Matching the physical key covers the common case; the logical character
/// covers layouts where `m` is not at `KeyCode::KeyM` and Wine's bogus keys.
pub fn is_hud_toggle(
    logical: &Key,
    physical: &PhysicalKey,
    ctrl: bool,
    shift: bool,
    alt_super: bool,
) -> bool {
    if !ctrl || !shift || alt_super {
        return false;
    }
    is_shortcut(physical, logical, KeyCode::KeyM, "m")
}

/// Match a shortcut by physical key or by the character it types.
fn is_shortcut(physical: &PhysicalKey, logical: &Key, code: KeyCode, ch: &str) -> bool {
    matches!(physical, PhysicalKey::Code(c) if *c == code)
        || matches!(logical, Key::Character(c) if c.eq_ignore_ascii_case(ch))
}

/// True for the Tab key, by physical code or logical name: layouts and Wine
/// report one or the other reliably.
fn is_tab(physical: &PhysicalKey, logical: &Key) -> bool {
    matches!(physical, PhysicalKey::Code(KeyCode::Tab))
        || matches!(logical, Key::Named(NamedKey::Tab))
}

/// True when Tab is part of the local desktop's window-switcher gesture
/// (Alt+Tab, Super/Win+Tab, and their Shift/Ctrl variants) rather than input
/// for the remote app.
pub fn is_window_switch(logical: &Key, physical: &PhysicalKey, alt_super: bool) -> bool {
    alt_super && is_tab(physical, logical)
}

/// True for a press winit synthesized for a key that was already down when the
/// window gained focus.
///
/// Winit synthesizes presses for every held key on focus gain (X11 and
/// Windows), before `Focused(true)` and, on Windows, *before* the modifier
/// presses. Such a key was pressed while another window had focus: after
/// Alt+Tab it is the Tab that landed the switch, which therefore arrives with
/// no Alt in the modifier state and slips past `is_window_switch`. Modifiers
/// are kept, because losing OS focus released them remotely and the remote
/// needs their state re-sent.
pub fn is_stale_focus_press(
    logical: &Key,
    physical: &PhysicalKey,
    pressed: bool,
    synthetic: bool,
) -> bool {
    synthetic && pressed && !is_modifier(logical, physical)
}

/// Modifier keys, by logical name or physical code: layouts and Wine report
/// one or the other reliably.
fn is_modifier(logical: &Key, physical: &PhysicalKey) -> bool {
    matches!(
        logical,
        Key::Named(
            NamedKey::Shift
                | NamedKey::Control
                | NamedKey::Alt
                | NamedKey::AltGraph
                | NamedKey::Super
                | NamedKey::Meta
                | NamedKey::Hyper
                | NamedKey::Symbol
        )
    ) || matches!(
        physical,
        PhysicalKey::Code(
            KeyCode::ShiftLeft
                | KeyCode::ShiftRight
                | KeyCode::ControlLeft
                | KeyCode::ControlRight
                | KeyCode::AltLeft
                | KeyCode::AltRight
                | KeyCode::SuperLeft
                | KeyCode::SuperRight
        )
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_keys_are_letters_digits_and_punctuation() {
        assert!(is_text_key(KeyCode::KeyC));
        assert!(is_text_key(KeyCode::Digit3));
        assert!(is_text_key(KeyCode::Comma));
        assert!(is_text_key(KeyCode::Space));
        assert!(!is_text_key(KeyCode::Enter));
        assert!(!is_text_key(KeyCode::F4));
        assert!(!is_text_key(KeyCode::ArrowLeft));
    }

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

    fn code(c: KeyCode) -> PhysicalKey {
        PhysicalKey::Code(c)
    }

    fn ch(s: &str) -> Key {
        Key::Character(s.into())
    }

    #[test]
    fn press_and_release_pair_up() {
        let mut ks = KeyState::new();
        let phys = code(KeyCode::KeyA);
        assert_eq!(
            ks.on_key(&phys, &ch("a"), Some("a"), true, false, ModifiersState::empty()),
            KeyAction::Press(0x61)
        );
        assert_eq!(
            ks.on_key(&phys, &ch("a"), None, false, false, ModifiersState::empty()),
            KeyAction::Release(0x61)
        );
        assert_eq!(
            ks.on_key(&phys, &ch("a"), None, false, false, ModifiersState::empty()),
            KeyAction::None,
            "a release without a matching press sends nothing"
        );
    }

    #[test]
    fn repeats_are_ignored() {
        let mut ks = KeyState::new();
        assert_eq!(
            ks.on_key(&code(KeyCode::KeyA), &ch("a"), Some("a"), true, true, ModifiersState::empty()),
            KeyAction::None
        );
    }

    #[test]
    fn tab_is_recognized_by_name_or_physical_code() {
        assert!(is_tab(&code(KeyCode::Tab), &Key::Named(NamedKey::Tab)));
        assert!(is_tab(&code(KeyCode::KeyQ), &Key::Named(NamedKey::Tab)));
        assert!(is_tab(&code(KeyCode::Tab), &Key::Named(NamedKey::F17)));
        assert!(!is_tab(&code(KeyCode::KeyQ), &ch("q")));
        assert!(is_window_switch(
            &Key::Named(NamedKey::Tab),
            &code(KeyCode::Tab),
            true
        ));
        assert!(!is_window_switch(
            &Key::Named(NamedKey::Tab),
            &code(KeyCode::Tab),
            false
        ));
        assert!(!is_window_switch(&ch("m"), &code(KeyCode::KeyM), true));
    }

    #[test]
    fn window_switch_tab_is_not_forwarded() {
        let mut ks = KeyState::new();
        let phys = code(KeyCode::Tab);
        let tab = Key::Named(NamedKey::Tab);
        for mods in [
            ModifiersState::ALT,
            ModifiersState::SUPER,
            ModifiersState::ALT | ModifiersState::SHIFT,
            ModifiersState::CONTROL | ModifiersState::ALT,
        ] {
            assert_eq!(
                ks.on_key(&phys, &tab, Some("\t"), true, false, mods),
                KeyAction::None,
                "Tab with {mods:?} must stay local"
            );
            assert_eq!(
                ks.on_key(&phys, &tab, None, false, false, mods),
                KeyAction::None,
                "the release after a filtered press sends nothing"
            );
        }
    }

    #[test]
    fn tab_is_local_when_alt_is_held_through_the_keysym_map() {
        // Wine may report the held modifier only through the keysym map.
        let mut ks = KeyState::new();
        assert_eq!(
            ks.on_key(
                &code(KeyCode::AltLeft),
                &Key::Named(NamedKey::Alt),
                None,
                true,
                false,
                ModifiersState::empty()
            ),
            KeyAction::Press(XK_ALT_L)
        );
        assert_eq!(
            ks.on_key(
                &code(KeyCode::Tab),
                &Key::Named(NamedKey::Tab),
                Some("\t"),
                true,
                false,
                ModifiersState::empty()
            ),
            KeyAction::None
        );
    }

    #[test]
    fn focus_gain_presses_keep_modifiers_and_drop_other_held_keys() {
        // Winit synthesizes a press for every key already down when a window
        // gains focus. Only modifiers must reach the remote (focus loss
        // released them there); the rest were pressed elsewhere.
        for logical in [
            Key::Named(NamedKey::Tab),
            Key::Named(NamedKey::CapsLock),
            Key::Named(NamedKey::NumLock),
            ch("a"),
        ] {
            assert!(is_stale_focus_press(
                &logical,
                &code(KeyCode::KeyA),
                true,
                true
            ));
        }
        for (logical, physical) in [
            (Key::Named(NamedKey::Shift), code(KeyCode::ShiftLeft)),
            (Key::Named(NamedKey::Control), code(KeyCode::ControlLeft)),
            (Key::Named(NamedKey::Alt), code(KeyCode::AltLeft)),
            (Key::Named(NamedKey::Super), code(KeyCode::SuperLeft)),
        ] {
            assert!(!is_stale_focus_press(&logical, &physical, true, true));
        }
        // Wine may report a bogus logical key for the held key.
        assert!(is_stale_focus_press(
            &Key::Named(NamedKey::F17),
            &code(KeyCode::Tab),
            true,
            true
        ));
        assert!(!is_stale_focus_press(
            &Key::Named(NamedKey::F17),
            &code(KeyCode::ShiftLeft),
            true,
            true
        ));
        // Releases and real presses are unaffected.
        assert!(!is_stale_focus_press(
            &Key::Named(NamedKey::Tab),
            &code(KeyCode::Tab),
            false,
            true
        ));
        assert!(!is_stale_focus_press(
            &Key::Named(NamedKey::Tab),
            &code(KeyCode::Tab),
            true,
            false
        ));
    }

    #[test]
    fn plain_ctrl_and_shift_tab_are_forwarded() {
        let mut ks = KeyState::new();
        let phys = code(KeyCode::Tab);
        let tab = Key::Named(NamedKey::Tab);
        assert_eq!(
            ks.on_key(&phys, &tab, None, true, false, ModifiersState::CONTROL),
            KeyAction::Press(XK_TAB)
        );
        assert_eq!(
            ks.on_key(&phys, &tab, None, false, false, ModifiersState::CONTROL),
            KeyAction::Release(XK_TAB)
        );
        assert_eq!(
            ks.on_key(&phys, &tab, Some("\t"), true, false, ModifiersState::SHIFT),
            KeyAction::Press(XK_TAB)
        );
        assert_eq!(
            ks.on_key(&phys, &tab, None, false, false, ModifiersState::SHIFT),
            KeyAction::Release(XK_TAB)
        );
        assert_eq!(
            ks.on_key(
                &phys,
                &tab,
                Some("\t"),
                true,
                false,
                ModifiersState::empty()
            ),
            KeyAction::Press(XK_TAB)
        );
    }

    #[test]
    fn ctrl_shift_c_and_v_are_forwarded() {
        // The clipboard is mirrored in the background, so these must reach the
        // application (xterm's own copy/paste bindings) instead of being eaten.
        let mut ks = KeyState::new();
        let mods = ModifiersState::CONTROL | ModifiersState::SHIFT;
        assert_eq!(
            ks.on_key(&code(KeyCode::KeyC), &ch("C"), None, true, false, mods),
            KeyAction::Press(0x43)
        );
        assert_eq!(
            ks.on_key(&code(KeyCode::KeyV), &ch("V"), None, true, false, mods),
            KeyAction::Press(0x56)
        );
    }

    #[test]
    fn hud_toggle_uses_winit_modifier_state() {
        // Wayland reports modifiers held before focus only as ModifiersChanged;
        // no modifier press is ever seen by the keysym map.
        let mut ks = KeyState::new();
        let mods = ModifiersState::CONTROL | ModifiersState::SHIFT;
        assert_eq!(
            ks.on_key(&code(KeyCode::KeyM), &ch("M"), None, true, false, mods),
            KeyAction::ToggleHud
        );
        // Same shortcut on a layout where 'm' is elsewhere (logical only).
        assert_eq!(
            ks.on_key(&code(KeyCode::KeyN), &ch("m"), None, true, false, mods),
            KeyAction::ToggleHud
        );
        // Without the modifiers it is a normal key.
        assert_eq!(
            ks.on_key(&code(KeyCode::KeyM), &ch("m"), Some("m"), true, false, ModifiersState::empty()),
            KeyAction::Press(0x6d)
        );
        // Alt+Ctrl+Shift+M must not toggle.
        assert_ne!(
            ks.on_key(
                &code(KeyCode::KeyM),
                &ch("M"),
                None,
                true,
                false,
                mods | ModifiersState::ALT
            ),
            KeyAction::ToggleHud
        );
    }

    #[test]
    fn wine_ctrl_shortcut_uses_the_learned_plain_char() {
        let mut ks = KeyState::new();
        // Type 'k' once so the layout character is known.
        assert_eq!(
            ks.on_key(&code(KeyCode::KeyK), &ch("k"), Some("k"), true, false, ModifiersState::empty()),
            KeyAction::Press(0x6b)
        );
        // Wine reports a bogus named key with Ctrl+Alt; no text is delivered.
        let mods = ModifiersState::CONTROL | ModifiersState::ALT;
        assert_eq!(
            ks.on_key(&code(KeyCode::KeyK), &Key::Named(NamedKey::F5), None, true, false, mods),
            KeyAction::Press(0x6b)
        );
    }

    #[test]
    fn release_all_drains_held_keys() {
        let mut ks = KeyState::new();
        ks.on_key(&code(KeyCode::ControlLeft), &Key::Named(NamedKey::Control), None, true, false, ModifiersState::empty());
        ks.on_key(&code(KeyCode::KeyA), &ch("a"), Some("a"), true, false, ModifiersState::empty());
        let mut syms = ks.release_all();
        syms.sort_unstable();
        assert_eq!(syms, vec![0x61, 0xffe3]);
        assert!(ks.release_all().is_empty());
    }

    #[test]
    fn toggle_binding_needs_ctrl_shift_without_alt() {
        let physical = code(KeyCode::KeyM);
        assert!(is_hud_toggle(&ch("m"), &physical, true, true, false));
        assert!(!is_hud_toggle(&ch("m"), &physical, false, true, false));
        assert!(!is_hud_toggle(&ch("m"), &physical, true, false, false));
        assert!(!is_hud_toggle(&ch("m"), &physical, true, true, true));
        assert!(!is_hud_toggle(&Key::Named(NamedKey::F5), &code(KeyCode::F5), true, true, false));
    }
}
