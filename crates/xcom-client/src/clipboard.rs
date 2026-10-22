//! Local clipboard mirroring.
//!
//! The daemon pushes remote selection changes as unsolicited `ClipboardData`;
//! this module pushes local changes the other way, so no custom shortcut is
//! needed and every application pastes with its own native binding.
//!
//! Change detection is **event-driven**, never content polling:
//! - X11 (including XWayland): XFixes `SetSelectionOwnerNotify` on CLIPBOARD;
//! - Windows: the clipboard sequence number (a counter, no clipboard access).
//!
//! The text is read only after a change is signalled. Writes we perform
//! ourselves are suppressed for a short window so the two sides cannot echo
//! each other; the daemon has an exact owner-window guard on its side.

use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use xcom_proto::{logln, Message, CLIPBOARD_CLIPBOARD};

/// Ignore watcher events this long after we wrote the clipboard ourselves
/// (our own write looks like a change; arboard's owner window is internal).
const SUPPRESS: Duration = Duration::from_millis(500);

/// UI-side handle: applies remote text and suppresses the resulting echo.
pub struct LocalClipboard {
    inner: Option<arboard::Clipboard>,
    suppress_until: Arc<Mutex<Instant>>,
}

impl LocalClipboard {
    /// Write text received from the remote.
    pub fn apply(&mut self, text: &str) {
        let Some(cb) = self.inner.as_mut() else {
            return;
        };
        // Suppress before writing: the watcher may see the change immediately.
        *self.suppress_until.lock().unwrap() = Instant::now() + SUPPRESS;
        if let Err(e) = cb.set_text(text) {
            logln!("[ui] clipboard write failed: {e}");
        }
    }
}

/// Start the watcher; the returned handle belongs to the UI thread.
///
/// With `enabled == false` the local clipboard is never read or written.
pub fn spawn(input_tx: Sender<(Message, Instant)>, enabled: bool) -> LocalClipboard {
    if !enabled {
        logln!("[ui] clipboard mirroring disabled");
        return LocalClipboard {
            inner: None,
            suppress_until: Arc::new(Mutex::new(Instant::now())),
        };
    }
    let inner = match arboard::Clipboard::new() {
        Ok(c) => Some(c),
        Err(e) => {
            logln!("[ui] clipboard unavailable: {e}");
            None
        }
    };
    let suppress_until = Arc::new(Mutex::new(Instant::now()));
    if inner.is_some() {
        let tx = input_tx;
        let suppress = Arc::clone(&suppress_until);
        std::thread::spawn(move || watch(tx, suppress));
    }
    LocalClipboard {
        inner,
        suppress_until,
    }
}

/// Read the local clipboard and push it to the remote if it really changed.
fn read_and_push(
    cb: &mut arboard::Clipboard,
    tx: &Sender<(Message, Instant)>,
    suppress_until: &Mutex<Instant>,
) {
    if Instant::now() < *suppress_until.lock().unwrap() {
        return; // our own write echoing back
    }
    // Fails for non-text content and while another process holds the clipboard.
    let Ok(text) = cb.get_text() else {
        return;
    };
    if text.is_empty() {
        return;
    }
    if text.len() > xcom_proto::MAX_CLIPBOARD_TEXT {
        if xcom_proto::debug() {
            logln!(
                "[ui] clipboard: {}-byte local selection not mirrored (cap {})",
                text.len(),
                xcom_proto::MAX_CLIPBOARD_TEXT
            );
        }
        return;
    }
    if xcom_proto::debug() {
        logln!(
            "[ui] clipboard: mirroring {} bytes to the remote",
            text.len()
        );
    }
    tx.send((
        Message::ClipboardSet {
            target: CLIPBOARD_CLIPBOARD,
            text,
        },
        Instant::now(),
    ))
    .ok();
}

#[cfg(target_os = "linux")]
fn watch(tx: Sender<(Message, Instant)>, suppress: Arc<Mutex<Instant>>) {
    use x11rb::connection::Connection;
    use x11rb::protocol::xfixes::{self, ConnectionExt as _};
    use x11rb::protocol::xproto::ConnectionExt as _;
    use x11rb::protocol::Event;

    let (conn, screen) = match x11rb::connect(None) {
        Ok(c) => c,
        Err(e) => {
            logln!("[ui] clipboard sync disabled: no X display ({e})");
            return;
        }
    };
    let root = conn.setup().roots[screen].root;
    let clipboard = match conn.intern_atom(false, b"CLIPBOARD") {
        Ok(cookie) => match cookie.reply() {
            Ok(r) => r.atom,
            Err(e) => {
                logln!("[ui] clipboard sync disabled: intern CLIPBOARD: {e}");
                return;
            }
        },
        Err(e) => {
            logln!("[ui] clipboard sync disabled: intern CLIPBOARD: {e}");
            return;
        }
    };
    // The extension requires a QueryVersion before other requests.
    let versioned = match conn.xfixes_query_version(5, 0) {
        Ok(cookie) => cookie.reply().is_ok(),
        Err(_) => false,
    };
    let watched = versioned
        && match conn.xfixes_select_selection_input(
            root,
            clipboard,
            xfixes::SelectionEventMask::SET_SELECTION_OWNER,
        ) {
            Ok(cookie) => cookie.check().is_ok(),
            Err(_) => false,
        };
    if !watched {
        logln!("[ui] clipboard sync disabled: XFixes selection events unavailable");
        return;
    }
    let _ = conn.flush();
    let mut cb = match arboard::Clipboard::new() {
        Ok(c) => c,
        Err(e) => {
            logln!("[ui] clipboard sync disabled: {e}");
            return;
        }
    };
    loop {
        match conn.wait_for_event() {
            Ok(Event::XfixesSelectionNotify(e))
                if e.subtype == xfixes::SelectionEvent::SET_SELECTION_OWNER
                    && e.selection == clipboard
                    && e.owner != x11rb::NONE =>
            {
                read_and_push(&mut cb, &tx, &suppress);
            }
            Ok(_) => {}
            Err(e) => {
                logln!("[ui] clipboard sync stopped: {e}");
                return;
            }
        }
    }
}

#[cfg(windows)]
fn watch(tx: Sender<(Message, Instant)>, suppress: Arc<Mutex<Instant>>) {
    use windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber;

    let mut cb = match arboard::Clipboard::new() {
        Ok(c) => c,
        Err(e) => {
            logln!("[ui] clipboard sync disabled: {e}");
            return;
        }
    };
    // The sequence number changes on every clipboard write; checking it does
    // not touch the clipboard itself.
    let mut last = unsafe { GetClipboardSequenceNumber() };
    loop {
        std::thread::sleep(Duration::from_millis(250));
        let seq = unsafe { GetClipboardSequenceNumber() };
        if seq == last {
            continue;
        }
        last = seq;
        read_and_push(&mut cb, &tx, &suppress);
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
fn watch(_tx: Sender<(Message, Instant)>, _suppress: Arc<Mutex<Instant>>) {
    logln!("[ui] clipboard sync not implemented on this platform");
}
