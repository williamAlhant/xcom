//! Input injection through the XTEST extension.
//!
//! Events are injected at the server (not sent to a specific client), so they
//! behave exactly like real hardware input: grabs, focus and toolkit event
//! handling all work unchanged.

use std::cell::Cell;

use anyhow::{anyhow, bail, Result};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt, InputFocus, Keycode, Timestamp, Window};
use x11rb::protocol::xtest::ConnectionExt as _;

use crate::keymap::{Binding, Keymap, MASK_CONTROL, MASK_MOD1, MASK_SHIFT, XK_ALT_L, XK_CONTROL_L, XK_SHIFT_L};

// X core event codes as used by XTestFakeInput's `type` field.
const FAKE_KEY_PRESS: u8 = 2;
const FAKE_KEY_RELEASE: u8 = 3;
const FAKE_BUTTON_PRESS: u8 = 4;
const FAKE_BUTTON_RELEASE: u8 = 5;
const FAKE_MOTION: u8 = 6;

pub struct Injector<'c, C: Connection> {
    conn: &'c C,
    root: Window,
    pub keymap: Keymap,
    /// A keycode with no keysyms, used to type Unicode the layout lacks.
    scratch: Option<u8>,
    /// (keysym, keycode) currently installed on the scratch keycode.
    scratch_active: Cell<Option<(u32, u8)>>,
}

impl<'c, C: Connection> Injector<'c, C> {
    pub fn new(conn: &'c C, root: Window, keymap: Keymap) -> Self {
        // Highest unused keycode: layouts usually leave the top codes free.
        let scratch = (8u8..=255).rev().find(|kc| {
            keymap
                .columns
                .get(kc)
                .map(|syms| syms.iter().all(|&s| s == 0))
                .unwrap_or(true)
        });
        Self {
            conn,
            root,
            keymap,
            scratch,
            scratch_active: Cell::new(None),
        }
    }

    fn fake(&self, type_: u8, detail: u8, x: i16, y: i16) -> Result<()> {
        self.conn
            .xtest_fake_input(type_, detail, Timestamp::from(0u32), self.root, x, y, 0)
            .map_err(|e| anyhow::anyhow!(e))?;
        // x11rb buffers requests: without an explicit flush the XTest event
        // sits in userspace until unrelated traffic happens to flush it, so
        // keystrokes arrive late and in bursts. Flush per event.
        self.conn.flush().map_err(|e| anyhow::anyhow!(e))?;
        Ok(())
    }

    fn modifier_keycode(&self, mask: u16) -> Option<Keycode> {
        let sym = if mask & MASK_SHIFT != 0 {
            XK_SHIFT_L
        } else if mask & MASK_CONTROL != 0 {
            XK_CONTROL_L
        } else if mask & MASK_MOD1 != 0 {
            XK_ALT_L
        } else {
            return None;
        };
        self.keymap.resolve(sym).map(|b| Keycode::from(b.keycode))
    }

    fn set_modifier(&self, mask: u16, press: bool) -> Result<()> {
        if let Some(kc) = self.modifier_keycode(mask) {
            let t = if press {
                FAKE_KEY_PRESS
            } else {
                FAKE_KEY_RELEASE
            };
            self.fake(t, kc, 0, 0)?;
        }
        Ok(())
    }

    /// Press (or release) a keysym, synthesizing whatever modifiers it needs.
    ///
    /// Keysyms the remote layout does not contain (most non-Latin-1 Unicode)
    /// are typed by temporarily binding a spare keycode to the Unicode keysym,
    /// the same technique xdotool uses.
    pub fn key(&self, sym: u32, press: bool) -> Result<()> {
        // Release of a key that went through the scratch keycode.
        if !press {
            if let Some((active, kc)) = self.scratch_active.get() {
                if active == sym {
                    self.fake(FAKE_KEY_RELEASE, kc, 0, 0)?;
                    self.set_scratch(kc, None)?;
                    self.scratch_active.set(None);
                    return Ok(());
                }
            }
        }

        if let Some(binding) = self.keymap.resolve(sym) {
            return self.key_binding(binding, press);
        }

        if press {
            if let Some(kc) = self.scratch {
                self.set_scratch(kc, Some(sym))?;
                self.fake(FAKE_KEY_PRESS, kc, 0, 0)?;
                self.scratch_active.set(Some((sym, kc)));
                return Ok(());
            }
        }
        bail!("keysym {sym:#x} not in the remote keymap and no free keycode to type it")
    }

    fn key_binding(&self, binding: Binding, press: bool) -> Result<()> {
        let Binding { keycode, mods } = binding;

        // Press modifiers in a stable order, release in reverse.
        const ORDER: [u16; 3] = [MASK_SHIFT, MASK_CONTROL, MASK_MOD1];
        if press {
            for m in ORDER {
                if mods & m != 0 {
                    self.set_modifier(m, true)?;
                }
            }
        }
        let t = if press {
            FAKE_KEY_PRESS
        } else {
            FAKE_KEY_RELEASE
        };
        self.fake(t, keycode, 0, 0)?;
        if !press {
            for m in ORDER.iter().rev() {
                if mods & m != 0 {
                    self.set_modifier(*m, false)?;
                }
            }
        }
        Ok(())
    }

    /// Bind the scratch keycode to `sym` (or clear it with `None`).
    fn set_scratch(&self, kc: u8, sym: Option<u32>) -> Result<()> {
        let n = self.keymap.keysyms_per_keycode.max(1);
        let mut syms = vec![0u32; n as usize];
        if let Some(sym) = sym {
            syms[0] = sym;
        }
        self.conn
            .change_keyboard_mapping(1, kc, n, &syms)?
            .check()?;
        self.conn.flush().map_err(|e| anyhow!("{e}"))?;
        Ok(())
    }

    /// Full press+release.
    pub fn tap(&self, sym: u32) -> Result<()> {
        self.key(sym, true)?;
        self.key(sym, false)
    }

    /// Type a string. Latin-1 goes through the layout; everything else uses
    /// Unicode keysyms via the scratch keycode.
    pub fn type_str(&self, s: &str) -> Result<()> {
        for c in s.chars() {
            let sym = Keymap::sym_for_char(c)
                .ok_or_else(|| anyhow::anyhow!("char {c:?} is not a printable code point"))?;
            self.tap(sym)?;
        }
        Ok(())
    }

    pub fn button(&self, button: u8, press: bool) -> Result<()> {
        let t = if press {
            FAKE_BUTTON_PRESS
        } else {
            FAKE_BUTTON_RELEASE
        };
        self.fake(t, button, 0, 0)
    }

    pub fn click(&self, button: u8) -> Result<()> {
        self.button(button, true)?;
        self.button(button, false)
    }

    /// Wheel: X buttons 4/5 (up/down).
    pub fn scroll(&self, down: bool) -> Result<()> {
        self.click(if down { 5 } else { 4 })
    }

    pub fn motion(&self, x: i16, y: i16) -> Result<()> {
        self.fake(FAKE_MOTION, 0, x, y)
    }

    /// Give a window keyboard focus (what a window manager would do on click).
    pub fn focus(&self, win: Window) -> Result<()> {
        self.conn
            .set_input_focus(InputFocus::PARENT, win, Timestamp::from(0u32))
            .map_err(|e| anyhow::anyhow!(e))?;
        self.conn.flush().map_err(|e| anyhow::anyhow!(e))?;
        Ok(())
    }
}
