//! Per-attachment client plumbing and the shared input queue.
//!
//! A *link* is one TLS control+bulk pair and its three threads. The supervisor
//! in `main.rs` owns the sequence of links; the UI and the clipboard worker
//! share one [`InputSink`] and never care which link is current.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use winit::event_loop::EventLoopProxy;

use xcom_proto::{logln, Message};
use xcom_transport::Role;

use crate::ui::UserEvent;

/// The current link's input queue.
///
/// Replaced by the supervisor on reconnect. A send while disconnected goes to
/// the old (dead) channel and is dropped: input must never be queued for a
/// link that may come back minutes later.
#[derive(Clone)]
pub struct InputSink(Arc<Mutex<Sender<(Message, Instant)>>>);

impl InputSink {
    pub fn new(tx: Sender<(Message, Instant)>) -> Self {
        Self(Arc::new(Mutex::new(tx)))
    }

    pub fn send(&self, msg: Message) {
        self.send_at(msg, Instant::now());
    }

    pub fn send_at(&self, msg: Message, at: Instant) {
        if let Ok(tx) = self.0.lock() {
            let _ = tx.send((msg, at));
        }
    }

    fn replace(&self, tx: Sender<(Message, Instant)>) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = tx;
        }
    }
}

/// Where to dial one attachment.
pub struct Endpoint {
    pub addr: String,
    pub control_token: [u8; 32],
    pub bulk_token: [u8; 32],
    pub fingerprint: String,
}

/// One live link. Dropping it does not stop the threads; call [`Link::stop`].
pub struct Link {
    down: mpsc::Receiver<()>,
    stop: Arc<AtomicBool>,
}

impl Link {
    /// Block until one of the link threads reports that the connection is gone.
    pub fn wait_down(&self) {
        let _ = self.down.recv();
    }

    /// Tell the remaining threads to finish (the reader may already be gone).
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Connect both roles and start the reader/writer/bulk threads.
pub fn start(
    endpoint: &Endpoint,
    sink: &InputSink,
    proxy: &EventLoopProxy<UserEvent>,
    queue_us: Arc<AtomicU32>,
) -> Result<Link> {
    let timeout = Duration::from_secs(15);
    let control = xcom_transport::connect(
        &endpoint.addr,
        Role::Control,
        &endpoint.fingerprint,
        &endpoint.control_token,
        timeout,
    )
    .context("control channel")?;
    let bulk = xcom_transport::connect(
        &endpoint.addr,
        Role::Bulk,
        &endpoint.fingerprint,
        &endpoint.bulk_token,
        timeout,
    )
    .context("bulk channel")?;
    logln!("[xcom] secure channel established");

    let (input_tx, input_rx) = mpsc::channel::<(Message, Instant)>();
    sink.replace(input_tx);

    let (ctrl_reader, ctrl_writer) = control.split().context("splitting the control channel")?;
    let pending: Arc<Mutex<Option<(u64, Instant)>>> = Arc::new(Mutex::new(None));
    let stop = Arc::new(AtomicBool::new(false));
    let (down_tx, down_rx) = mpsc::channel::<()>();

    let reader_proxy = proxy.clone();
    let reader_pending = Arc::clone(&pending);
    let reader_stop = Arc::clone(&stop);
    let reader_down = down_tx.clone();
    std::thread::spawn(move || {
        crate::control_reader_loop(ctrl_reader, reader_proxy, reader_pending, reader_stop);
        let _ = reader_down.send(());
    });

    let writer_stop = Arc::clone(&stop);
    let writer_down = down_tx.clone();
    std::thread::spawn(move || {
        crate::control_writer_loop(ctrl_writer, input_rx, queue_us, pending, writer_stop);
        let _ = writer_down.send(());
    });

    let bulk_proxy = proxy.clone();
    let bulk_stop = Arc::clone(&stop);
    std::thread::spawn(move || {
        if let Err(e) = crate::bulk_loop(bulk, bulk_proxy) {
            logln!("[xcom] bulk channel: {e}");
        }
        bulk_stop.store(true, Ordering::SeqCst);
        let _ = down_tx.send(());
    });

    Ok(Link {
        down: down_rx,
        stop,
    })
}
