//! Keysym -> keycode mapping and modifier bookkeeping.
//!
//! The client sends *keysyms* (layout-independent intent: "the user typed é"),
//! and this module resolves them against the remote server's actual keymap:
//! which keycode produces the keysym, and which modifiers must be held. This
//! keeps typing correct even when the laptop's layout differs from the VM's.

use std::collections::HashMap;

use anyhow::Result;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::ConnectionExt;

/// Core modifier masks (X protocol `KeyButMask` bits).
pub const MASK_SHIFT: u16 = 1 << 0;
pub const MASK_LOCK: u16 = 1 << 1;
pub const MASK_CONTROL: u16 = 1 << 2;
pub const MASK_MOD1: u16 = 1 << 3; // usually Alt
pub const MASK_MOD5: u16 = 1 << 7; // usually AltGr / ISO level 3

// Keysyms we care about beyond ASCII/Latin-1 (which map to their code points).
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
pub const XK_SHIFT_R: u32 = 0xffe2;
pub const XK_CONTROL_L: u32 = 0xffe3;
pub const XK_CONTROL_R: u32 = 0xffe4;
pub const XK_ALT_L: u32 = 0xffe9;
pub const XK_ALT_R: u32 = 0xffea;
pub const XK_ISO_LEVEL3_SHIFT: u32 = 0xfe03;
pub const XK_MODE_SWITCH: u32 = 0xff7e;

/// Which column of the keysym array applies to a modifier combination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    pub keycode: u8,
    pub mods: u16,
}

#[derive(Debug, Clone)]
pub struct Keymap {
    /// keycode -> keysyms by column (0: plain, 1: shift, 2: level3, 3: level3+shift)
    pub columns: HashMap<u8, Vec<u32>>,
    /// keysym -> how to produce it
    pub by_sym: HashMap<u32, Binding>,
    /// Mask that selects level 3 (AltGr); discovered from the modifier map.
    pub level3_mask: u16,
    pub keysyms_per_keycode: u8,
}

impl Keymap {
    /// Read the whole keymap (core protocol keycodes 8..=255).
    pub fn query<C: Connection>(conn: &C) -> Result<Keymap> {
        let reply = conn.get_keyboard_mapping(8u8.into(), 248)?.reply()?;
        let per = reply.keysyms_per_keycode;
        let mut columns: HashMap<u8, Vec<u32>> = HashMap::new();
        for (i, sym) in reply.keysyms.iter().enumerate() {
            let keycode = 8 + (i / per.max(1) as usize) as u8;
            columns.entry(keycode).or_default().push((*sym).into());
        }

        // Find the modifier that acts as level-3 select (AltGr / Mode_switch).
        let mut level3_mask = MASK_MOD5;
        if let Ok(modmap) = conn.get_modifier_mapping()?.reply() {
            let kpm = modmap.keycodes_per_modifier().max(1) as usize;
            'outer: for (index, chunk) in modmap.keycodes.chunks(kpm).enumerate() {
                for kc in chunk {
                    let syms = columns.get(&u8::from(*kc)).cloned().unwrap_or_default();
                    for s in syms {
                        if s == XK_ISO_LEVEL3_SHIFT || s == XK_MODE_SWITCH || s == XK_ALT_R {
                            level3_mask = 1 << index;
                            break 'outer;
                        }
                    }
                }
            }
        }

        let mut by_sym: HashMap<u32, Binding> = HashMap::new();
        for (&keycode, syms) in &columns {
            for (col, &sym) in syms.iter().enumerate() {
                if sym == 0 {
                    continue;
                }
                let mods = match col {
                    0 => 0,
                    1 => MASK_SHIFT,
                    2 => level3_mask,
                    3 => level3_mask | MASK_SHIFT,
                    _ => continue,
                };
                // Prefer the binding with fewest modifiers.
                by_sym
                    .entry(sym)
                    .and_modify(|b| {
                        if b.mods.count_ones() > mods.count_ones() {
                            *b = Binding { keycode, mods };
                        }
                    })
                    .or_insert(Binding { keycode, mods });
            }
        }

        Ok(Keymap {
            columns,
            by_sym,
            level3_mask,
            keysyms_per_keycode: per,
        })
    }

    /// How to produce `sym`, if this keymap can.
    pub fn resolve(&self, sym: u32) -> Option<Binding> {
        self.by_sym.get(&sym).copied()
    }

    /// Keysym produced by `keycode` under `mods` (used to verify injections).
    pub fn keysym_for(&self, keycode: u8, mods: u16) -> Option<u32> {
        let syms = self.columns.get(&keycode)?;
        let col = if mods & MASK_SHIFT != 0 {
            if mods & self.level3_mask != 0 {
                3
            } else {
                1
            }
        } else if mods & self.level3_mask != 0 {
            2
        } else {
            0
        };
        syms.get(col).copied().filter(|s| *s != 0).or_else(|| {
            if col > 0 {
                syms.first().copied().filter(|s| *s != 0)
            } else {
                None
            }
        })
    }

    /// Keysym for a char (Latin-1 direct mapping, ASCII included).
    pub fn sym_for_char(c: char) -> Option<u32> {
        let c = c as u32;
        if (0x20..0x100).contains(&c) {
            Some(c)
        } else {
            None
        }
    }
}
