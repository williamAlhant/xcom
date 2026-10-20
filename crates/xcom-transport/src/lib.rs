//! The secure channel.
//!
//! Security model (why this is MITM-safe without any PKI):
//!
//! 1. The client runs `ssh user@host 'xcomd serve --json'` over an
//!    authenticated SSH channel and receives one handshake line containing a
//!    single-use token and the SHA-256 fingerprint of an ephemeral TLS
//!    certificate this server just generated.
//! 2. The client connects directly (TCP, outside SSH) and completes a TLS
//!    handshake with a verifier that accepts exactly the pinned certificate.
//!    An attacker cannot present that certificate without its private key, and
//!    cannot substitute their own because the fingerprint arrived over SSH.
//! 3. The client proves itself by sending the token inside the TLS tunnel.
//!
//! The channel carries two TLS connections to the same endpoint:
//! - **control**: input events, window lifecycle, pings — small, latency
//!   sensitive, never queued behind pixel data;
//! - **bulk**: compressed damage frames — backpressure falls out naturally
//!   because the capture thread blocks on writes while damage keeps coalescing
//!   in the X server.

use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use sha2::{Digest, Sha256};

pub const PROTO_VERSION: u16 = 2;

/// Per-attempt budget for TLS + hello. A stalled peer costs at most this much
/// before the accept loop moves on; peers not on the allowlist never get here.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Which half of the channel a connection carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Control,
    Bulk,
}

impl Role {
    pub fn as_u8(self) -> u8 {
        match self {
            Role::Control => 0,
            Role::Bulk => 1,
        }
    }

    pub fn from_u8(v: u8) -> Option<Role> {
        match v {
            0 => Some(Role::Control),
            1 => Some(Role::Bulk),
            _ => None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("tls: {0}")]
    Tls(#[from] rustls::Error),
    #[error("rcgen: {0}")]
    Rcgen(#[from] rcgen::Error),
    #[error("certificate fingerprint mismatch: expected {expected}, got {got}")]
    PinMismatch { expected: String, got: String },
    #[error("authentication rejected")]
    AuthRejected,
    #[error("protocol error: {0}")]
    Proto(String),
    #[error("framing: {0}")]
    Framing(#[from] xcom_proto::ProtoError),
    #[error("connection timed out")]
    Timeout,
}

/// An ephemeral server identity: one self-signed certificate per daemon run.
pub struct Identity {
    cert_der: CertificateDer<'static>,
    key_der: PrivateKeyDer<'static>,
    fingerprint: String,
}

impl Identity {
    pub fn generate() -> Result<Identity, TransportError> {
        let cert = rcgen::generate_simple_self_signed(vec!["xcom".to_string()])?;
        let cert_der = cert.cert.der().clone();
        let key_der = PrivateKeyDer::Pkcs8(cert.key_pair.serialize_der().into());
        let fingerprint = fingerprint_of(&cert_der);
        Ok(Identity {
            cert_der,
            key_der,
            fingerprint,
        })
    }

    /// SHA-256 of the DER certificate, hex — this is what travels over SSH.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    fn server_config(&self) -> Result<Arc<ServerConfig>, TransportError> {
        let cfg = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![self.cert_der.clone()], self.key_der.clone_key())?;
        Ok(Arc::new(cfg))
    }
}

/// SHA-256 hex fingerprint of a DER certificate.
pub fn fingerprint_of(cert_der: &CertificateDer<'_>) -> String {
    let digest = Sha256::digest(cert_der.as_ref());
    hex(&digest)
}

pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

pub fn random_token() -> [u8; 32] {
    let mut token = [0u8; 32];
    getrandom::fill(&mut token).expect("no OS entropy source");
    token
}

/// Verifies the peer certificate is exactly the one pinned over SSH.
///
/// Signature verification of the handshake (proof that the peer holds the
/// pinned certificate's private key) is delegated to rustls' default checks.
#[derive(Debug)]
struct PinnedVerifier {
    expected: String,
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let got = fingerprint_of(end_entity);
        if got == self.expected {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::Other(rustls::OtherError(std::sync::Arc::new(
                    std::io::Error::other(format!("pinned certificate mismatch: {got}")),
                ))),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

enum TlsStream {
    Client(StreamOwned<ClientConnection, TcpStream>),
    Server(StreamOwned<ServerConnection, TcpStream>),
}

impl Read for TlsStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            TlsStream::Client(s) => s.read(buf),
            TlsStream::Server(s) => s.read(buf),
        }
    }
}

impl Write for TlsStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            TlsStream::Client(s) => s.write(buf),
            TlsStream::Server(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            TlsStream::Client(s) => s.flush(),
            TlsStream::Server(s) => s.flush(),
        }
    }
}

/// One authenticated TLS channel (either role), carrying framed messages.
pub struct Channel {
    stream: TlsStream,
    state: xcom_proto::FrameState,
    role: Role,
}

impl Channel {
    pub fn role(&self) -> Role {
        self.role
    }

    /// Send one framed message. Blocking: on the bulk channel this is the
    /// backpressure signal the capture loop wants.
    pub fn send(&mut self, msg_type: u8, payload: &[u8]) -> Result<(), TransportError> {
        xcom_proto::write_frame(&mut self.stream, msg_type, payload)?;
        Ok(())
    }

    /// Receive one framed message; `None` on clean EOF.
    ///
    /// Resumable across read timeouts: partial frames are kept in the channel's
    /// read state, so polling with `set_read_timeout` cannot corrupt framing.
    pub fn recv(&mut self) -> Result<Option<(u8, Vec<u8>)>, TransportError> {
        Ok(self.state.read_from(&mut self.stream)?)
    }

    /// Graceful TLS shutdown (close_notify) — polite servers log less.
    pub fn shutdown(&mut self) {
        match &mut self.stream {
            TlsStream::Client(s) => {
                s.conn.send_close_notify();
                let _ = s.flush();
            }
            TlsStream::Server(s) => {
                s.conn.send_close_notify();
                let _ = s.flush();
            }
        }
    }

    /// Set a read timeout on the underlying socket (0 = none).
    pub fn set_read_timeout(&self, d: Option<Duration>) -> Result<(), TransportError> {
        let tcp = match &self.stream {
            TlsStream::Client(s) => &s.sock,
            TlsStream::Server(s) => &s.sock,
        };
        tcp.set_read_timeout(d)?;
        Ok(())
    }

    pub fn set_write_timeout(&self, d: Option<Duration>) -> Result<(), TransportError> {
        let tcp = match &self.stream {
            TlsStream::Client(s) => &s.sock,
            TlsStream::Server(s) => &s.sock,
        };
        tcp.set_write_timeout(d)?;
        Ok(())
    }

    pub fn set_nodelay(&self, on: bool) -> Result<(), TransportError> {
        let tcp = match &self.stream {
            TlsStream::Client(s) => &s.sock,
            TlsStream::Server(s) => &s.sock,
        };
        tcp.set_nodelay(on)?;
        Ok(())
    }
}

/// Server side of the channel.
///
/// Holds one single-use token per role. A connection that fails TLS or auth is
/// dropped and the loop continues: an unauthenticated peer must never be able
/// to end the session (a connect+close used to propagate out of `accept` and
/// kill the daemon before the real client arrived).
pub struct ChannelAcceptor {
    listener: std::net::TcpListener,
    tls: Arc<ServerConfig>,
    tokens: [[u8; 32]; 2],
    used: std::sync::Mutex<[bool; 2]>,
    allow_peer: Option<IpAddr>,
}

impl ChannelAcceptor {
    pub fn bind(
        addr: &str,
        identity: &Identity,
        control_token: [u8; 32],
        bulk_token: [u8; 32],
        allow_peer: Option<IpAddr>,
    ) -> Result<ChannelAcceptor, TransportError> {
        let listener = std::net::TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        Ok(ChannelAcceptor {
            listener,
            tls: identity.server_config()?,
            tokens: [control_token, bulk_token],
            used: std::sync::Mutex::new([false, false]),
            allow_peer,
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr, TransportError> {
        Ok(self.listener.local_addr()?)
    }

    pub fn token(&self, role: Role) -> [u8; 32] {
        self.tokens[role_index(role)]
    }

    pub fn token_used(&self, role: Role) -> bool {
        self.used.lock().unwrap()[role_index(role)]
    }

    /// Accept one authenticated connection for `role`.
    ///
    /// Unauthenticated peers (wrong token, replay, bad TLS, not on the
    /// allowlist) are dropped; the loop keeps waiting for the real client
    /// until the overall deadline.
    pub fn accept(&self, role: Role, timeout: Duration) -> Result<Channel, TransportError> {
        let deadline = Instant::now() + timeout;
        let mut last_log = Instant::now() - Duration::from_secs(10);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(TransportError::Timeout);
            }
            let (tcp, peer) = match self.listener.accept() {
                Ok(conn) => conn,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            };
            if let Some(expected) = self.allow_peer {
                if peer.ip() != expected {
                    if last_log.elapsed() >= Duration::from_secs(1) {
                        xcom_proto::logln!(
                            "[transport] ignoring {peer} (expected {expected}); --allow-any-peer disables this"
                        );
                        last_log = Instant::now();
                    }
                    continue;
                }
            }
            let budget = remaining.min(HANDSHAKE_TIMEOUT);
            match self.authenticate(tcp, role, budget) {
                Ok(channel) => return Ok(channel),
                Err(e) => {
                    if last_log.elapsed() >= Duration::from_secs(1) {
                        xcom_proto::logln!("[transport] {role:?} connection rejected: {e}");
                        last_log = Instant::now();
                    }
                    continue;
                }
            }
        }
    }

    fn authenticate(
        &self,
        tcp: TcpStream,
        role: Role,
        budget: Duration,
    ) -> Result<Channel, TransportError> {
        tcp.set_nonblocking(false)?;
        tcp.set_nodelay(true)?;
        tcp.set_read_timeout(Some(budget))?;
        tcp.set_write_timeout(Some(budget))?;
        let conn = ServerConnection::new(self.tls.clone())?;
        let stream = StreamOwned::new(conn, tcp);
        let mut channel = Channel {
            stream: TlsStream::Server(stream),
            // Pre-auth frames are tiny: the 512 MiB allocation a peer could
            // trigger by advertising a huge length is gone.
            state: xcom_proto::FrameState::with_max_len(xcom_proto::MAX_HELLO_LEN),
            role,
        };
        let token = self.tokens[role_index(role)];
        if let Err(e) = channel.auth_server(&token, role) {
            let _ = channel.send(xcom_proto::MSG_AUTH_REJECTED, &[]);
            return Err(e);
        }
        {
            let mut used = self.used.lock().unwrap();
            let slot = &mut used[role_index(role)];
            if *slot {
                let _ = channel.send(xcom_proto::MSG_AUTH_REJECTED, &[]);
                return Err(TransportError::AuthRejected);
            }
            *slot = true;
        }
        // Idle timeouts here would look like EOF to the bulk reader (it blocks
        // between frames). Liveness is the keepalive's job, not the socket's.
        channel.set_read_timeout(None)?;
        channel.set_write_timeout(None)?;
        channel.state.set_max_len(role_max_frame(role));
        Ok(channel)
    }
}

fn role_index(role: Role) -> usize {
    match role {
        Role::Control => 0,
        Role::Bulk => 1,
    }
}

fn role_max_frame(role: Role) -> usize {
    match role {
        Role::Control => xcom_proto::CONTROL_MAX_FRAME,
        Role::Bulk => xcom_proto::BULK_MAX_FRAME,
    }
}

impl Channel {
    /// Server side of the auth exchange: expect `MSG_HELLO` with the token.
    fn auth_server(&mut self, token: &[u8; 32], role: Role) -> Result<(), TransportError> {
        match self.recv()? {
            Some((xcom_proto::MSG_HELLO, payload)) => {
                if payload.len() != 2 + 1 + 32 {
                    return Err(TransportError::Proto("bad hello".into()));
                }
                let proto = u16::from_le_bytes(payload[..2].try_into().unwrap());
                let got_role = Role::from_u8(payload[2]).ok_or_else(|| {
                    TransportError::Proto("bad role".into())
                })?;
                if proto != PROTO_VERSION {
                    return Err(TransportError::Proto(format!(
                        "client speaks proto {proto}, server speaks {PROTO_VERSION}"
                    )));
                }
                if got_role != role {
                    return Err(TransportError::Proto("role mismatch".into()));
                }
                if !constant_time_eq(&payload[3..], token) {
                    return Err(TransportError::AuthRejected);
                }
                self.send(xcom_proto::MSG_AUTH_OK, &PROTO_VERSION.to_le_bytes())?;
                Ok(())
            }
            _ => Err(TransportError::Proto("expected hello".into())),
        }
    }
}

/// Client side: connect, pin the certificate, present the token.
pub fn connect(
    addr: &str,
    role: Role,
    fingerprint: &str,
    token: &[u8; 32],
    timeout: Duration,
) -> Result<Channel, TransportError> {
    let addr: SocketAddr = addr
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| TransportError::Proto(format!("cannot resolve {addr}")))?;

    let tcp = connect_with_timeout(addr, timeout)?;
    tcp.set_nodelay(true)?;

    let verifier = Arc::new(PinnedVerifier {
        expected: fingerprint.to_string(),
    });
    let mut cfg = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    cfg.enable_sni = false; // we pin the certificate; names are irrelevant
    let cfg = Arc::new(cfg);

    let server_name = ServerName::try_from("xcom").unwrap();
    let conn = ClientConnection::new(cfg, server_name)?;
    let stream = StreamOwned::new(conn, tcp);
    stream.sock.set_read_timeout(Some(timeout))?;
    stream.sock.set_write_timeout(Some(timeout))?;

    let mut channel = Channel {
        stream: TlsStream::Client(stream),
        state: xcom_proto::FrameState::with_max_len(xcom_proto::MAX_HELLO_LEN),
        role,
    };

    // Hello: version + role + token, inside the freshly established TLS tunnel.
    let mut payload = Vec::with_capacity(35);
    payload.extend_from_slice(&PROTO_VERSION.to_le_bytes());
    payload.push(role.as_u8());
    payload.extend_from_slice(token);
    channel.send(xcom_proto::MSG_HELLO, &payload)?;

    match channel.recv()? {
        Some((xcom_proto::MSG_AUTH_OK, p)) if p.len() >= 2 => {
            // Blocking reads from here on: the bulk channel idles between
            // frames and a socket timeout must not look like a dead peer.
            channel.set_read_timeout(None)?;
            channel.set_write_timeout(None)?;
            channel.state.set_max_len(role_max_frame(role));
            Ok(channel)
        }
        Some((xcom_proto::MSG_AUTH_REJECTED, _)) => Err(TransportError::AuthRejected),
        _ => Err(TransportError::Proto("expected auth-ok".into())),
    }
}

fn connect_with_timeout(addr: SocketAddr, timeout: Duration) -> Result<TcpStream, TransportError> {
    match TcpStream::connect_timeout(&addr, timeout) {
        Ok(s) => Ok(s),
        Err(e) => Err(e.into()),
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}
