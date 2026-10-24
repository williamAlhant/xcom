# xcom

X11 forwarding without carrying pixels over SSH: SSH is used for the initial
handshake only, then the session runs over a dedicated TLS channel. The server
owns a virtual Xvfb display plus a small window manager; damage is streamed
with LZ4 and each remote window is rendered as a native window on the client,
with input, resize and reflow. Requires the `xvfb` package on the server
machine; the server ships as a static binary, while the client uses the
desktop's own X11/Wayland libraries.

## AI assistance

This project was developed with extensive use of AI coding assistants. A large
share of the code, tests and documentation was generated with AI and then
validated through the test suite and end-to-end runs against real X11 sessions.
It has not been independently reviewed or security-audited; the limitations
below are the ones known today.

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
- **Session** — the remote owner keeps Xvfb, the app and capture state alive
  across connections. Each client is an *attachment*; reattaching by name
  resyncs windows, frames, cursor and clipboard. The app defines the session's
  life: when it exits, the session ends.

## Quick start

Server side: install `xvfb`, put `xcomd` in `PATH`. Client side: the `xcom`
binary (it loads the desktop's X11 or Wayland libraries at runtime) plus the
system `ssh` for the bootstrap.

```sh
# ssh mode: the client launches the daemon on the remote host
xcom connect --ssh user@host --exec 'xterm'

# sessions outlive the client; reattach, inspect or end one by name
xcom connect --ssh user@host --session work --exec 'xterm'
xcom sessions --ssh user@host
xcom kill --ssh user@host --session work

# direct mode: start the daemon yourself and pass the handshake through
xcomd serve --json --exec 'xterm' > handshake.json
xcom connect --direct "$(jq -r .connect handshake.json)" \
    --token-control "$(jq -r .token_control handshake.json)" \
    --token-bulk "$(jq -r .token_bulk handshake.json)" \
    --fingerprint "$(jq -r .cert_sha256 handshake.json)"
```

Closing the last window asks whether to close the app (ending the session) or
disconnect and leave it running; Esc or a click outside cancels.

Prebuilt binaries (static Linux `xcomd`, glibc Linux `xcom`, Windows
`xcom.exe`) are attached to GitHub releases. CI builds the static Linux server
in the same Alpine Rust container as `scripts/build-static.sh`, the Linux
client on a glibc runner, and the Windows client natively on a pinned
`windows-2022` runner with a pinned Rust version.

Useful flags: `--connect HOST:PORT` overrides the address to dial (NAT,
containers); `--seconds N` exits after N seconds (0 = until the windows close);
`--spawn WxH` requests the session display size (default: this machine's
desktop; it cannot grow later, so windows are capped at it);
`xcomd serve --spawn 1600x1200` sets the virtual display size directly;
`--session NAME` attaches to (or creates) a named session (`default`;
`--replace` rebuilds it); `--idle-timeout N` ends a session after N seconds
without a client (0 = never, the default); a dropped `--ssh` link reconnects
in the background (`--no-reconnect` opts out); `--hud` shows
the metrics overlay (fps, bandwidth, compression ratio, RTT, capture/encode
time), toggled in-session with Ctrl+Shift+M; `--no-clipboard-sync` turns off
clipboard mirroring; `--close-action ask|close|disconnect` overrides the
last-window dialog for scripts; `--backend auto|x11|wayland` picks the Linux
client backend (`auto` prefers X11/XWayland, see below; ignored on Windows).

Builds:

```sh
cargo build --release                       # dev binaries (target/release)
scripts/build-static.sh xcomd               # static musl server (dist/)
cargo build --profile dist -p xcom-client   # glibc client (target/dist)
scripts/build-static.sh --dev xcomd         # + M0 harness, for the test container
scripts/build-windows.sh                    # Windows client    (target/x86_64-pc-windows-msvc)
```

The client cannot be built fully static: winit and softbuffer load
`libX11`/`libwayland` at runtime with `dlopen`, which static musl does not
support.

Tests: `cargo test --workspace`, plus `scripts/e2e-direct.sh`,
`scripts/e2e-ssh.sh`, `scripts/e2e-persist.sh`, `scripts/e2e-reconnect.sh`
and `scripts/e2e-clipboard.sh` (short end-to-end runs).

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
- **One attachment per session.** A second client is rejected as "session
  busy"; there is no multi-client sharing.
- **Reattach needs ssh.** A dropped `--ssh` link reconnects in the background
  (`--no-reconnect` opts out); `--direct` clients exit instead, because their
  single-use tokens only arrive over ssh.
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
  regenerated for every attachment.

## Untested / unverified

The Windows client has been validated on real Windows (window rendering,
menus/popup placement, clipboard in both directions, keyboard), but not at
display scaling above 100%, with client multi-monitor setups, on long sessions,
or on macOS.

## Docs

- `docs/m2.md` — window management, clipboard, cursor.
- `docs/m3.md` — persistent sessions, reattach, the close dialog.
- `docs/windows-client.md` — Windows build and Wine findings.
