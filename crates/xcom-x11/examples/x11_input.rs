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
        "watch" => {
            use x11rb::protocol::xproto::{CreateWindowAux, EventMask, WindowClass};
            let win: Window = conn.generate_id()?;
            conn.create_window(
                0,
                win,
                root,
                100,
                100,
                400,
                300,
                0,
                WindowClass::INPUT_OUTPUT,
                x11rb::COPY_FROM_PARENT,
                &CreateWindowAux::new()
                    .background_pixel(0x00ff_ffff)
                    .event_mask(
                        EventMask::BUTTON_PRESS
                            | EventMask::BUTTON_RELEASE
                            | EventMask::POINTER_MOTION
                            | EventMask::KEY_PRESS
                            | EventMask::KEY_RELEASE,
                    ),
            )?;
            conn.map_window(win)?;
            conn.flush()?;
            std::thread::sleep(std::time::Duration::from_millis(300));
            conn.set_input_focus(InputFocus::PARENT, win, Timestamp::from(0u32))?;
            conn.flush()?;
            eprintln!("watching {win:#x} at 100,100 400x300 for 6s");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(6);
            while std::time::Instant::now() < deadline {
                while let Some(ev) = conn.poll_for_event()? {
                    match ev {
                        x11rb::protocol::Event::ButtonPress(e) => {
                            println!("ButtonPress detail={} at {}x{}", e.detail, e.event_x, e.event_y)
                        }
                        x11rb::protocol::Event::ButtonRelease(e) => {
                            println!("ButtonRelease detail={}", e.detail)
                        }
                        x11rb::protocol::Event::MotionNotify(e) => {
                            println!("Motion {}x{}", e.event_x, e.event_y)
                        }
                        x11rb::protocol::Event::KeyPress(e) => {
                            println!("KeyPress detail={}", e.detail)
                        }
                        x11rb::protocol::Event::KeyRelease(e) => {
                            println!("KeyRelease detail={}", e.detail)
                        }
                        _ => {}
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        "clipboard-get" => {
            let target = if args.next().as_deref() == Some("primary") {
                xcom_x11::clipboard::Target::Primary
            } else {
                xcom_x11::clipboard::Target::Clipboard
            };
            let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into());
            let (tx, _rx) = std::sync::mpsc::channel();
            let cb = xcom_x11::clipboard::Clipboard::spawn(&display, tx)?;
            match cb.get(target, std::time::Duration::from_secs(2)) {
                Some(text) => println!("{text}"),
                None => bail!("selection is empty"),
            }
        }
        "clipboard-set" => {
            let text = args.next().context("clipboard-set needs text")?;
            // Keep owning the selection for a while: the worker dies (and the
            // selection is lost) when the process exits.
            let hold: u64 = args
                .next()
                .map(|s| s.parse())
                .transpose()?
                .unwrap_or(0);
            let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into());
            let (tx, _rx) = std::sync::mpsc::channel();
            let cb = xcom_x11::clipboard::Clipboard::spawn(&display, tx)?;
            cb.set(xcom_x11::clipboard::Target::Clipboard, text)?;
            if hold > 0 {
                std::thread::sleep(std::time::Duration::from_secs(hold));
            }
        }
        "focus-get" => {
            let r = conn.get_input_focus()?.reply()?;
            println!("focus={:#x} revert={:?}", r.focus, r.revert_to);
        }
        "move" => {
            let x: i16 = args.next().context("move needs x")?.parse()?;
            let y: i16 = args.next().context("move needs y")?.parse()?;
            conn.xtest_fake_input(6, 0, Timestamp::from(0u32), root, x, y, 0)?
                .check()?;
            conn.flush()?;
        }
        "pointer" => {
            let r = conn.query_pointer(root)?.reply()?;
            println!("pointer root=({},{}) child={:#x}", r.root_x, r.root_y, r.child);
        }
        "list" => {
            let tree = conn.query_tree(root)?.reply()?;
            for win in tree.children {
                let name = conn
                    .get_property(false, win, AtomEnum::WM_NAME, AtomEnum::ANY, 0, 1024)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .map(|r| String::from_utf8_lossy(&r.value).into_owned())
                    .unwrap_or_default();
                let (g, class, mapped, or) = match conn.get_geometry(win) {
                    Ok(c) => match c.reply() {
                        Ok(g) => {
                            let a = conn.get_window_attributes(win).ok().and_then(|c| c.reply().ok());
                            (
                                format!("{}+{}+{}x{}", g.x, g.y, g.width, g.height),
                                a.map(|a| format!("{:?}", a.class)).unwrap_or_default(),
                                a.map(|a| format!("{:?}", a.map_state)).unwrap_or_default(),
                                a.map(|a| a.override_redirect).unwrap_or(false),
                            )
                        }
                        Err(_) => (String::new(), String::new(), String::new(), false),
                    },
                    Err(_) => (String::new(), String::new(), String::new(), false),
                };
                println!("{win:#x} {g} {class} {mapped} override={or} {name:?}");
            }
        }
        "clickat" => {
            let x: i16 = args.next().context("clickat needs x")?.parse()?;
            let y: i16 = args.next().context("clickat needs y")?.parse()?;
            let button: u8 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(1);
            conn.xtest_fake_input(6, 0, Timestamp::from(0u32), root, x, y, 0)?
                .check()?;
            conn.flush()?;
            // Let the client process the motion (its button handler uses the
            // last CursorMoved position).
            std::thread::sleep(std::time::Duration::from_millis(120));
            conn.xtest_fake_input(4, button, Timestamp::from(0u32), 0u32, 0, 0, 0)?
                .check()?;
            conn.xtest_fake_input(5, button, Timestamp::from(0u32), 0u32, 0, 0, 0)?
                .check()?;
            conn.flush()?;
            println!("clicked button {button} at ({x},{y})");
        }
        "focus" => {
            let needle = args.next().context("focus needs a title substring")?;
            let win = find_window(&conn, root, &needle)?.context("no window matched")?;
            // No window manager under Xvfb: focus has to be set explicitly.
            conn.set_input_focus(InputFocus::PARENT, win, Timestamp::from(0u32))?
                .check()?;
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
