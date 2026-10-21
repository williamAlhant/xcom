//! Test aid: inject X11 input through XTEST.
//!
//! The host has no xdotool/libXtst, and driving the Wine client needs real
//! X11 key events (the client's `--type-text` bypasses the OS input path).
//!
//!   cargo run -p xcom-x11 --example x11_input -- click xcom
//!   cargo run -p xcom-x11 --example x11_input -- keys 24 25 26
//!
//! Keycodes are X keycodes (evdev keycode + 8): 24/25/26 are the physical
//! Q/W/E positions, i.e. A/Z/E on a French layout.

use anyhow::{bail, Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, InputFocus, Timestamp, Window};
use x11rb::protocol::xtest::ConnectionExt as _;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let cmd = args
        .next()
        .context("usage: x11_input click <title-substr> | keys <keycode>...")?;
    let (conn, screen_num) = x11rb::connect(None)?;
    let root = conn.setup().roots[screen_num].root;
    match cmd.as_str() {
        "keymap" => {
            let first: u8 = args
                .next()
                .map(|s| s.parse())
                .transpose()?
                .unwrap_or(8);
            let count: u8 = args
                .next()
                .map(|s| s.parse())
                .transpose()?
                .unwrap_or(32);
            let reply = conn.get_keyboard_mapping(first, count)?.reply()?;
            let per = (reply.keysyms_per_keycode as usize).max(1);
            for (i, chunk) in reply.keysyms.chunks(per).enumerate() {
                let kc = first as usize + i;
                let syms: Vec<String> = chunk
                    .iter()
                    .map(|s| format!("{:#x}", (*s)))
                    .collect();
                println!("keycode {kc}: {}", syms.join(" "));
            }
        }
        "focus" => {
            let needle = args.next().context("focus needs a title substring")?;
            let win = find_window(&conn, root, &needle)?.context("no window matched")?;
            // No window manager under Xvfb: focus has to be set explicitly.
            conn.set_input_focus(InputFocus::PARENT, win, Timestamp::from(0u32))?;
            conn.flush()?;
            println!("focused {win:#x}");
        }
        "click" => {
            let needle = args.next().context("click needs a title substring")?;
            let win = find_window(&conn, root, &needle)?.context("no window matched")?;
            let geo = conn.get_geometry(win)?.reply()?;
            let t = conn.translate_coordinates(win, root, 0, 0)?.reply()?;
            let x = t.dst_x + (geo.width / 2) as i16;
            let y = t.dst_y + (geo.height / 2) as i16;
            conn.xtest_fake_input(6, 0, Timestamp::from(0u32), root, x, y, 0)?;
            conn.xtest_fake_input(4, 1, Timestamp::from(0u32), root, 0, 0, 0)?;
            conn.xtest_fake_input(5, 1, Timestamp::from(0u32), root, 0, 0, 0)?;
            conn.flush()?;
            println!("clicked {win:#x} at root ({x},{y})");
        }
        "keys" => {
            for code in args {
                let code: u8 = code.parse().context("keycode must be 0..=255")?;
                conn.xtest_fake_input(2, code, Timestamp::from(0u32), 0u32, 0, 0, 0)?;
                conn.xtest_fake_input(3, code, Timestamp::from(0u32), 0u32, 0, 0, 0)?;
                conn.flush()?;
                std::thread::sleep(std::time::Duration::from_millis(150));
            }
        }
        "down" => {
            for code in args {
                let code: u8 = code.parse().context("keycode must be 0..=255")?;
                conn.xtest_fake_input(2, code, Timestamp::from(0u32), 0u32, 0, 0, 0)?;
                conn.flush()?;
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
        "up" => {
            for code in args {
                let code: u8 = code.parse().context("keycode must be 0..=255")?;
                conn.xtest_fake_input(3, code, Timestamp::from(0u32), 0u32, 0, 0, 0)?;
                conn.flush()?;
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
        other => bail!("unknown command {other:?}"),
    }
    Ok(())
}

fn find_window<C: Connection>(conn: &C, root: Window, needle: &str) -> Result<Option<Window>> {
    let tree = conn.query_tree(root)?.reply()?;
    let needle = needle.to_lowercase();
    for win in tree.children {
        let name = conn
            .get_property(false, win, AtomEnum::WM_NAME, AtomEnum::ANY, 0, 1024)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| String::from_utf8_lossy(&r.value).into_owned())
            .unwrap_or_default();
        if name.to_lowercase().contains(&needle) {
            return Ok(Some(win));
        }
    }
    Ok(None)
}
