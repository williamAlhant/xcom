//! Input injection through the XTEST extension.
//!
//! Events are injected at the server (not sent to a specific client), so they
//! behave exactly like real hardware input: grabs, focus and toolkit event
//! handling all work unchanged.

use anyhow::Result;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt, InputFocus, Keycode, Timestamp, Window};
use x11rb::protocol::xtest::ConnectionExt as _;

use crate::keymap::{Keymap, Binding, MASK_CONTROL, MASK_MOD1, MASK_SHIFT, XK_CONTROL_L, XK_ALT_L, XK_SHIFT_L};

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
}

impl<'c, C: Connection> Injector<'c, C> {
    pub fn new(conn: &'c C, root: Window, keymap: Keymap) -> Self {
        Self { conn, root, keymap }
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
            self.fake(t, u8::from(kc), 0, 0)?;
        }
        Ok(())
    }

    /// Press (or release) a keysym, synthesizing whatever modifiers it needs.
    pub fn key(&self, sym: u32, press: bool) -> Result<()> {
        let Binding { keycode, mods } = self
            .keymap
            .resolve(sym)
            .ok_or_else(|| anyhow::anyhow!("keysym {sym:#x} not in remote keymap"))?;

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

    /// Full press+release.
    pub fn tap(&self, sym: u32) -> Result<()> {
        self.key(sym, true)?;
        self.key(sym, false)
    }

    /// Type a string via Latin-1 keysyms.
    pub fn type_str(&self, s: &str) -> Result<()> {
        for c in s.chars() {
            let sym = Keymap::sym_for_char(c)
                .ok_or_else(|| anyhow::anyhow!("char {c:?} not representable (M0 is Latin-1)"))?;
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
