# Session security

This describes the trust boundaries around persistent sessions (M3/M4). It is
not an independent audit; see the README's security note.

## Who talks to whom

```
sshd ──> xcomd attach (remote user) ──> Unix socket ──> xcomd session (owner)
                                                    │
client <──── TLS (per attachment, pinned cert) ─────┘
```

- `xcomd attach` runs as the remote user under sshd. It reads `SSH_CONNECTION`
  and forwards the client's IP in the attach request.
- The owner listens on `$XDG_RUNTIME_DIR/xcomd/<name>.sock` (directory 0700,
  socket 0600; `/tmp/xcomd-<uid>/` fallback). Only processes of the same user
  can reach it.
- For each attachment the owner generates a fresh self-signed certificate and
  one single-use token per role. The handshake (address, tokens, fingerprint)
  travels over the Unix socket; the client pins the fingerprint and
  authenticates both TLS channels with their tokens.

## Boundaries and guarantees

- **The Unix socket is a same-user boundary, not a privilege boundary.** Any
  process running as the remote user can ask to attach, list or kill a
  session. That is equivalent to running the `xcomd`/`xcom` CLI, which that
  user could already do.
- **Per-attachment allowlist.** The owner allowlists the peer IP from the
  attach request, so only the ssh client can complete the TLS handshake. The
  request is built by the local `xcomd attach` process; a same-user process
  could forge it, but could also just run the CLI.
- **`--allow-any-peer` disables the allowlist** and widens the pre-auth
  exposure. Use it only when the TLS source address legitimately differs from
  the ssh source (jump host, NAT).
- **No ssh peer.** A local `xcomd attach` has no `SSH_CONNECTION`. That is
  allowed only when the listener is loopback; a non-loopback `--listen`
  address is refused unless `--allow-any-peer` is given explicitly.
- **Tokens and certificates are per attachment.** They are single-use and
  regenerated on every attach; a reconnect takes a new handshake over the
  socket. Nothing is stored on disk.
- **`attach` is attach-only on reconnect.** A reconnect never creates a new
  session, so a vanished session is reported as gone instead of silently
  launching a different app.
- **Stale sockets.** If the socket path exists and answers, the name is in
  use. If it does not answer (owner crashed), the next owner replaces it. A
  same-user attacker could pre-create the directory to cause a denial of
  service, not to gain access.

## What is deliberately not protected

- Same-user processes can inspect the owner's command line, environment and
  logs, and can attach to or kill its sessions.
- The TLS listener is reachable on the network before authentication. TLS,
  the pinned certificate, single-use tokens and the peer allowlist gate the
  handshake, but an unauthenticated peer can still consume connection slots
  until `--accept-timeout`.
- The app runs as the remote user and sees the session display, as it would on
  a normal desktop.
- `XCOM_DEBUG=1` traces protocol/input details into `<name>.log` (directory
  0700). Do not enable it on a shared machine unless that output is acceptable.

## Operational knobs

| flag | effect |
|---|---|
| `--listen ADDR` | TLS bind address; `auto` is loopback without a peer, and the peer's family with one |
| `--allow-any-peer` | disable the per-attachment IP allowlist |
| `--accept-timeout N` | how long the owner waits for the first TLS role; the second role gets at most 10 s |
| `--idle-timeout N` | end a detached session after N seconds |
| `--session NAME` / `--replace` | choose or rebuild a session; names are restricted to `[A-Za-z0-9._-]` |
| `--no-reconnect` | do not reattach automatically after a link loss |
