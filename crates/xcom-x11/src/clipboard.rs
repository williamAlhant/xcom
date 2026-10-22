//! X11 selection (clipboard) access for the session.
//!
//! A worker thread owns its own X connection and a small unmapped window. It
//! can read the current selection (classic `convert_selection` round trip) and
//! take ownership of a selection so the remote applications can paste text we
//! pushed from the client.
//!
//! X11 selections are pull-based: the owner answers `SelectionRequest` events.
//! The worker therefore has to stay alive for as long as the session runs, and
//! the text is only available while this daemon owns the selection.

use std::collections::HashMap;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt, CreateWindowAux, EventMask, PropMode, SelectionNotifyEvent,
    SelectionRequestEvent, Timestamp, Window, WindowClass, SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::Event;
use xcom_proto::logln;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Clipboard,
    Primary,
}

impl Target {
    pub fn from_u8(v: u8) -> Option<Target> {
        match v {
            0 => Some(Target::Clipboard),
            1 => Some(Target::Primary),
            _ => None,
        }
    }

    pub fn as_u8(self) -> u8 {
        match self {
            Target::Clipboard => 0,
            Target::Primary => 1,
        }
    }
}

enum Cmd {
    Get {
        target: Target,
        reply: mpsc::Sender<Option<String>>,
    },
    Set {
        target: Target,
        text: String,
        reply: mpsc::Sender<bool>,
    },
}

/// Handle to the selection worker.
#[derive(Clone)]
pub struct Clipboard {
    tx: mpsc::Sender<Cmd>,
}

impl Clipboard {
    /// Start the worker on `display` (e.g. ":3"). Fails if the display cannot
    /// be opened.
    pub fn spawn(display: &str) -> Result<Clipboard> {
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let display = display.to_string();
        std::thread::spawn(move || match x11rb::connect(Some(&display)) {
            Ok((conn, screen)) => {
                ready_tx.send(Ok(())).ok();
                if let Err(e) = clipboard_loop(conn, screen, rx) {
                    logln!("[clipboard] stopped: {e:#}");
                }
            }
            Err(e) => {
                ready_tx
                    .send(Err(format!("connecting to {display}: {e}")))
                    .ok();
            }
        });
        match ready_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(Ok(())) => Ok(Clipboard { tx }),
            Ok(Err(e)) => Err(anyhow!(e)),
            Err(_) => Err(anyhow!("clipboard worker did not start")),
        }
    }

    /// Read the current selection, waiting up to `timeout` for the owner.
    pub fn get(&self, target: Target, timeout: Duration) -> Option<String> {
        let (tx, rx) = mpsc::channel();
        self.tx.send(Cmd::Get { target, reply: tx }).ok()?;
        rx.recv_timeout(timeout).ok().flatten()
    }

    /// Take ownership of `target` and serve `text` to remote apps.
    pub fn set(&self, target: Target, text: String) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Cmd::Set {
                target,
                text,
                reply: tx,
            })
            .map_err(|_| anyhow!("clipboard worker is gone"))?;
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(true) => Ok(()),
            Ok(false) => Err(anyhow!("could not own the selection")),
            Err(_) => Err(anyhow!("clipboard worker did not answer")),
        }
    }

    /// Async request: the result is delivered on `reply` by the worker.
    pub fn request(&self, target: Target, reply: mpsc::Sender<Option<String>>) {
        let _ = self.tx.send(Cmd::Get { target, reply });
    }
}

struct Atoms {
    clipboard: Atom,
    primary: Atom,
    utf8: Atom,
    string: Atom,
    text: Atom,
    targets: Atom,
    property: Atom,
}

impl Atoms {
    fn intern(conn: &impl Connection) -> Result<Atoms> {
        let atom = |name: &[u8]| -> Result<Atom> {
            Ok(conn.intern_atom(false, name)?.reply()?.atom)
        };
        Ok(Atoms {
            clipboard: atom(b"CLIPBOARD")?,
            primary: atom(b"PRIMARY")?,
            utf8: atom(b"UTF8_STRING")?,
            string: atom(b"STRING")?,
            text: atom(b"TEXT")?,
            targets: atom(b"TARGETS")?,
            property: atom(b"_XCOM_CLIPBOARD")?,
        })
    }

    fn selection(&self, target: Target) -> Atom {
        match target {
            Target::Clipboard => self.clipboard,
            Target::Primary => self.primary,
        }
    }
}

fn clipboard_loop(
    conn: x11rb::rust_connection::RustConnection,
    screen: usize,
    rx: mpsc::Receiver<Cmd>,
) -> Result<()> {
    let root = conn.setup().roots[screen].root;
    let atoms = Atoms::intern(&conn)?;
    let win: Window = conn.generate_id()?;
    // Selection owner windows do not need to be mapped; it just has to exist.
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
    )?;
    conn.flush()?;

    let mut owned: HashMap<Atom, String> = HashMap::new();
    loop {
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                Cmd::Get { target, reply } => {
                    // While waiting for the owner we may also have to answer
                    // selection requests for the other selection.
                    match get_selection(&conn, win, &atoms, target, &mut owned) {
                        Ok(text) => {
                            let _ = reply.send(text);
                        }
                        Err(e) => {
                            logln!("[clipboard] get {:?} failed: {e:#}", target);
                            let _ = reply.send(None);
                        }
                    }
                }
                Cmd::Set {
                    target,
                    text,
                    reply,
                } => {
                    let sel = atoms.selection(target);
                    let ok = match conn.set_selection_owner(win, sel, Timestamp::from(0u32)) {
                        Ok(cookie) => cookie.check().is_ok(),
                        Err(_) => false,
                    };
                    if ok {
                        owned.insert(sel, text);
                    }
                    let _ = reply.send(ok);
                }
            }
        }
        while let Some(ev) = conn.poll_for_event()? {
            handle_event(&conn, &atoms, &mut owned, ev)?;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn handle_event(
    conn: &impl Connection,
    atoms: &Atoms,
    owned: &mut HashMap<Atom, String>,
    ev: Event,
) -> Result<()> {
    match ev {
        Event::SelectionRequest(e) => handle_selection_request(conn, atoms, owned, e)?,
        Event::SelectionClear(e) => {
            owned.remove(&e.selection);
        }
        _ => {}
    }
    Ok(())
}

fn handle_selection_request(
    conn: &impl Connection,
    atoms: &Atoms,
    owned: &HashMap<Atom, String>,
    e: SelectionRequestEvent,
) -> Result<()> {
    let Some(text) = owned.get(&e.selection) else {
        return notify(conn, &e, x11rb::NONE);
    };
    let mut property = e.property;
    if property == x11rb::NONE {
        // Ancient requestors put the data on the target atom.
        property = e.target;
    }
    let supported = if e.target == atoms.targets {
        let targets = [atoms.utf8, atoms.string, atoms.text, atoms.targets];
        let data: Vec<u8> = targets.iter().flat_map(|a| a.to_le_bytes()).collect();
        conn.change_property(
            PropMode::REPLACE,
            e.requestor,
            property,
            AtomEnum::ATOM,
            32,
            targets.len() as u32,
            &data,
        )?;
        true
    } else if e.target == atoms.utf8 || e.target == atoms.string || e.target == atoms.text {
        conn.change_property(
            PropMode::REPLACE,
            e.requestor,
            property,
            e.target,
            8,
            text.len() as u32,
            text.as_bytes(),
        )?;
        true
    } else {
        false
    };
    conn.flush()?;
    notify(conn, &e, if supported { property } else { x11rb::NONE })
}

fn notify(conn: &impl Connection, e: &SelectionRequestEvent, property: Atom) -> Result<()> {
    let ev = SelectionNotifyEvent {
        response_type: SELECTION_NOTIFY_EVENT,
        sequence: 0,
        time: e.time,
        requestor: e.requestor,
        selection: e.selection,
        target: e.target,
        property,
    };
    conn.send_event(false, e.requestor, EventMask::NO_EVENT, ev)?;
    conn.flush()?;
    Ok(())
}

fn get_selection(
    conn: &impl Connection,
    win: Window,
    atoms: &Atoms,
    target: Target,
    owned: &mut HashMap<Atom, String>,
) -> Result<Option<String>> {
    let sel = atoms.selection(target);
    for candidate in [atoms.utf8, atoms.string] {
        conn.convert_selection(win, sel, candidate, atoms.property, Timestamp::from(0u32))?;
        conn.flush()?;
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            match conn.poll_for_event()? {
                Some(Event::SelectionNotify(e))
                    if e.requestor == win && e.selection == sel =>
                {
                    if e.property == x11rb::NONE {
                        break; // refused for this target
                    }
                    let reply = conn
                        .get_property(true, win, e.property, AtomEnum::ANY, 0, 0x1FFF_FFFF)?
                        .reply()?;
                    if reply.format == 8 {
                        let text = String::from_utf8_lossy(&reply.value)
                            .trim_end_matches('\0')
                            .to_string();
                        return Ok(Some(text));
                    }
                    break;
                }
                Some(ev) => handle_event(conn, atoms, owned, ev)?,
                None => {
                    if Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }
    Ok(None)
}
