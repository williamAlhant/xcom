# xcom

X11 forwarding without carrying pixels over SSH: SSH is used for the initial
handshake only, then the session runs over a dedicated TLS channel. The server
owns a virtual Xvfb display plus a small window manager; damage is streamed
with LZ4 and each remote window is rendered as a native window on the client,
with input, resize and reflow. Requires the `xvfb` package on the server
machine; everything else is a static binary.

## How it works

- **SSH bootstrap** — the client runs `xcomd serve --json` over SSH and reads
  one handshake line: channel address, per-role single-use tokens, and the
  SHA-256 fingerprint of an ephemeral self-signed certificate.
- **Dedicated channel** — two TLS 1.3 connections (`control`, `bulk`) pin that
  fingerprint. When bootstrapped over SSH, the server only accepts the SSH
  client's address.
- **Server** — spawns Xvfb, runs a mini-WM (classification, dialog placement,
  override-redirect popups), injects input with XTEST, and streams
  damage-driven rectangles compressed with LZ4.
- **Client** — applies the pixel stream to native windows (winit + softbuffer),
  forwards keyboard/mouse, and mirrors the remote pointer shape.

## Quick start

Server side: install `xvfb`, put `xcomd` in `PATH`. Client side: a static
`xcom` binary; it uses the system `ssh` for the bootstrap.

```sh
# ssh mode: the client launches the daemon on the remote host
xcom connect --ssh user@host --exec 'xterm'

# direct mode: start the daemon yourself and pass the handshake through
xcomd serve --json --exec 'xterm' > handshake.json
xcom connect --direct "$(jq -r .connect handshake.json)" \
    --token-control "$(jq -r .token_control handshake.json)" \
    --token-bulk "$(jq -r .token_bulk handshake.json)" \
    --fingerprint "$(jq -r .cert_sha256 handshake.json)"
```

Prebuilt binaries (static Linux `xcomd`/`xcom`, Windows `xcom.exe`) are attached
to GitHub releases. CI builds the Linux artifacts in the same Alpine Rust
container as `scripts/build-static.sh` and the Windows client natively on a
pinned `windows-2022` runner with a pinned Rust version.

Useful flags: `--connect HOST:PORT` overrides the address to dial (NAT,
containers); `--seconds N` exits after N seconds (0 = until the windows close);
`--spawn WxH` requests the session display size (default: this machine's
desktop; it cannot grow later, so windows are capped at it);
`xcomd serve --spawn 1600x1200` sets the virtual display size directly; `--hud`
shows
the metrics overlay (fps, bandwidth, compression ratio, RTT, capture/encode
time), toggled in-session with Ctrl+Shift+M; `--no-clipboard-sync` turns off
clipboard mirroring; `--backend auto|x11|wayland` picks the Linux client
backend (`auto` prefers X11/XWayland, see below; ignored on Windows).

Builds:

```sh
cargo build --release                 # dev binaries (target/release)
scripts/build-static.sh xcomd xcom    # static musl       (dist/)
scripts/build-static.sh --dev xcomd   # + M0 harness, for the test container
scripts/build-windows.sh              # Windows client    (target/x86_64-pc-windows-msvc)
```

Tests: `cargo test --workspace`, plus `scripts/e2e-direct.sh`,
`scripts/e2e-ssh.sh` and `scripts/e2e-clipboard.sh` (short end-to-end runs).

The client warns when the remote daemon reports a different build id: a stale
`xcomd` silently behaves like an old version, which is easy to mistake for a
client bug.

## Limitations

- **X11 capture only.** The server renders into Xvfb (or XWayland); apps that
  only speak Wayland are not captured.
- **No scaling (deferred).** A remote window larger than the local work area is
  shown oversized; resize the remote window to fit. In the other direction,
  windows are capped at the session display: the X pointer cannot leave that
  screen, so a larger window would have a region that is visible but
  unclickable. The client sizes the session from this machine's desktop when it
  connects (`--spawn WxH` overrides it); a monitor attached afterwards needs a
  new session to be fully usable.
- **No persistent session.** The daemon and its Xvfb exit when the client
  disconnects; there is no reconnect or resume.
- **SSH is bootstrap-only.** After authentication the client reaches the
  daemon's advertised address directly; there is no `ssh -L` fallback yet.
- **Clipboard is text only**, capped at 60 KB, and mirrored in the background
  (CLIPBOARD only; no PRIMARY/middle-click). Disable with
  `--no-clipboard-sync`, since mirroring lets the remote read your local
  selection.
- **Wayland client defaults to X11/XWayland.** Wayland has no global window
  positions (`xdg_popup` is not available in winit 0.30), so menus and tooltips
  land wherever the compositor puts them, and the clipboard can only be
  observed through XWayland. `--backend auto` therefore uses XWayland when
  `DISPLAY` is set; `--backend wayland` keeps native Wayland (useful for
  fractional scaling and input methods) with those limitations.
- **Single remote screen**: no multi-monitor, no audio, no file transfer.
- **Not security-audited.** Tokens and the ephemeral certificate are
  regenerated on every run.

## Untested / unverified

The Windows client has been validated on real Windows (window rendering,
menus/popup placement, clipboard in both directions, keyboard), but not at
display scaling above 100%, with client multi-monitor setups, on long sessions,
or on macOS.

## Docs

- `docs/m2.md` — window management, clipboard, cursor.
- `docs/windows-client.md` — Windows build and Wine findings.
