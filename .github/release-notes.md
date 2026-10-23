xcom renders individual X11 windows from a Linux host as native windows on your
machine. SSH is used only to bootstrap and authenticate; pixels and input travel
over a dedicated TLS 1.3 channel with LZ4-compressed, damage-driven updates.

## Artifacts

| file | runs on | notes |
|---|---|---|
| `xcomd-linux-x86_64` | the remote Linux host | static (musl); owns an Xvfb display and a mini window manager |
| `xcom-linux-x86_64` | a Linux client | static (musl) |
| `xcom-windows-x86_64.exe` | a Windows x86_64 client | static CRT, no DLL dependencies |

`SHA256SUMS` covers all three.

## Requirements

- **Remote host**: `sshd`, the `xvfb` package (`Xvfb` on `PATH`), and `xcomd`.
  No desktop session is needed — the daemon starts its own virtual display.
- **Client**: Linux x86_64 or Windows x86_64, plus a system `ssh` client.

## Quick start

```sh
# on the remote
install -m755 xcomd-linux-x86_64 /usr/local/bin/xcomd

# on the client
xcom connect --ssh user@host --exec 'xterm'
```

On Windows, run `xcom.exe` from PowerShell or cmd. The executable is unsigned,
so SmartScreen shows a warning the first time.

## Notes

- The client warns when the remote `xcomd` reports a different build id (or none
  at all, which means it predates build reporting). Replace the remote binary
  when it does: a stale daemon behaves like an old version, which is easy to
  mistake for a client bug.
- SSH is used only for the handshake; the session's traffic does not go through
  it.
- `--hud` shows fps, bandwidth, compression ratio, RTT and capture/encode time
  (toggle in-session with Ctrl+Shift+M). Clipboard text is mirrored in both
  directions by default (`--no-clipboard-sync` turns it off).

## Limitations

- X11 capture only: apps that speak Wayland exclusively are not captured.
- One client per session, and the session ends when the client disconnects;
  detachable sessions are planned.
- The virtual display defaults to 1600x1200 (`xcomd serve --spawn`).
- Windows display scaling above 100%, client multi-monitor setups, and long
  sessions are not validated yet.
- No audio.
