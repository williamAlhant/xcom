//! Clipboard worker integration test against a private Xvfb.
//!
//! Covers both directions of the X11 selection protocol:
//!   1. an external owner → `Clipboard::get`
//!   2. `Clipboard::set` → an external reader

use std::sync::mpsc;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt, CreateWindowAux, EventMask, PropMode, SelectionNotifyEvent,
    SelectionRequestEvent, Timestamp, Window, WindowClass, SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

use xcom_x11::clipboard::{Clipboard, Target};
use xcom_x11::xvfb::Xvfb;

struct Atoms {
    clipboard: Atom,
    utf8: Atom,
    property: Atom,
}

fn atoms(conn: &RustConnection) -> Atoms {
    let a = |name: &[u8]| -> Atom { conn.intern_atom(false, name).unwrap().reply().unwrap().atom };
    Atoms {
        clipboard: a(b"CLIPBOARD"),
        utf8: a(b"UTF8_STRING"),
        property: a(b"_XCOM_TEST_CLIP"),
    }
}

fn create_window(conn: &RustConnection, screen: usize) -> Window {
    let root = conn.setup().roots[screen].root;
    let win: Window = conn.generate_id().unwrap();
    conn.create_window(
        0,
        win,
        root,
        -1,
        -1,
        1,
        1,
        0,
        WindowClass::INPUT_OUTPUT,
        x11rb::COPY_FROM_PARENT,
        &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )
    .unwrap();
    conn.flush().unwrap();
    win
}

fn answer(conn: &RustConnection, e: &SelectionRequestEvent, text: &str) {
    let property = if e.property == x11rb::NONE {
        e.target
    } else {
        e.property
    };
    conn.change_property(
        PropMode::REPLACE,
        e.requestor,
        property,
        e.target,
        8,
        text.len() as u32,
        text.as_bytes(),
    )
    .unwrap();
    conn.send_event(
        false,
        e.requestor,
        EventMask::NO_EVENT,
        SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: e.time,
            requestor: e.requestor,
            selection: e.selection,
            target: e.target,
            property,
        },
    )
    .unwrap();
    conn.flush().unwrap();
}

fn read_selection(
    conn: &RustConnection,
    win: Window,
    atoms: &Atoms,
    timeout: Duration,
) -> Option<String> {
    conn.convert_selection(
        win,
        atoms.clipboard,
        atoms.utf8,
        atoms.property,
        Timestamp::from(0u32),
    )
    .unwrap();
    conn.flush().unwrap();
    let deadline = Instant::now() + timeout;
    loop {
        while let Some(ev) = conn.poll_for_event().unwrap() {
            if let Event::SelectionNotify(e) = ev {
                if e.requestor != win || e.selection != atoms.clipboard {
                    continue;
                }
                if e.property == x11rb::NONE {
                    return None;
                }
                let reply = conn
                    .get_property(true, win, e.property, AtomEnum::ANY, 0, 0x1FFF_FFFF)
                    .unwrap()
                    .reply()
                    .unwrap();
                return Some(
                    String::from_utf8_lossy(&reply.value)
                        .trim_end_matches('\0')
                        .to_string(),
                );
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn clipboard_roundtrip() {
    let Ok(server) = Xvfb::spawn(320, 240) else {
        eprintln!("skipping clipboard test: Xvfb not available");
        return;
    };
    let display = server.display.clone();

    let clipboard = Clipboard::spawn(&display).expect("clipboard worker");

    // --- 1. external owner -> worker.get ---------------------------------
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let owner_display = display.clone();
    let owner = std::thread::spawn(move || {
        let (conn, screen) = x11rb::connect(Some(&owner_display)).unwrap();
        let atoms = atoms(&conn);
        let win = create_window(&conn, screen);
        conn.set_selection_owner(win, atoms.clipboard, Timestamp::from(0u32))
            .unwrap()
            .check()
            .unwrap();
        conn.flush().unwrap();
        let text = "hello from helper";
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && stop_rx.try_recv().is_err() {
            while let Some(ev) = conn.poll_for_event().unwrap() {
                if let Event::SelectionRequest(e) = ev {
                    answer(&conn, &e, text);
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    });

    let got = clipboard.get(Target::Clipboard, Duration::from_secs(2));
    assert_eq!(got.as_deref(), Some("hello from helper"));
    let _ = stop_tx.send(());
    owner.join().unwrap();

    // --- 2. worker.set -> external reader --------------------------------
    clipboard
        .set(Target::Clipboard, "from worker".to_string())
        .expect("set selection");
    let (conn, screen) = x11rb::connect(Some(&display)).unwrap();
    let atoms = atoms(&conn);
    let win = create_window(&conn, screen);
    let got = read_selection(&conn, win, &atoms, Duration::from_secs(2));
    assert_eq!(got.as_deref(), Some("from worker"));
}
