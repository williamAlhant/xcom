# Windows client: Wine smoke test

Date: 2026-10-21
Purpose: first run of the Windows build (M3 groundwork) without a Windows
machine, to catch Win32-backend and key-mapping problems early.

## Build

```sh
scripts/build-windows.sh    # cargo-xwin container -> target/x86_64-pc-windows-msvc/release/xcom.exe
```

Static CRT (`+crt-static`): the exe imports only system DLLs, no
`vcruntime140`/UCRT redistributable.

## Run under Wine (no ssh bootstrap in the prefix)

A Wine prefix has no `ssh.exe`, so the client's ssh bootstrap cannot run there.
Use direct mode against a daemon started separately:

```sh
# daemon in the test container
podman exec -e XCOM_DEBUG_KEYS=1 xcom-testvm xcomd serve --json --listen 127.0.0.1:0 \
    --exec "xterm -geometry 80x24 -e sh -c 'stty raw -echo; cat > /tmp/keys.txt'" \
    > /tmp/hs.json

# private X server, so XTEST does not need the desktop's input-monitoring grant
Xvfb :99 -screen 0 1280x1024x24 -nolisten tcp &
DISPLAY=:99 wine target/x86_64-pc-windows-msvc/release/xcom.exe connect \
  --direct "$(jq -r .connect /tmp/hs.json)" \
  --token-control "$(jq -r .token_control /tmp/hs.json)" \
  --token-bulk "$(jq -r .token_bulk /tmp/hs.json)" \
  --fingerprint "$(jq -r .cert_sha256 /tmp/hs.json)"
```

Input was injected with a small XTEST helper (no xdotool/libXtst installed):

```sh
cargo run -p xcom-x11 --example x11_input -- focus xcom
cargo run -p xcom-x11 --example x11_input -- keys 24 25 26      # q w e on a US keymap
cargo run -p xcom-x11 --example x11_input -- down 50
cargo run -p xcom-x11 --example x11_input -- keys 38 38         # A A while Shift held
cargo run -p xcom-x11 --example x11_input -- up 50
```

## Findings

1. **Wine mis-detects the keyboard layout.** `WINEDEBUG=+key` showed
   `detect_keyboard_layout mismatch` and Wine assigned *spare* VKs to keys
   whose text was correct: keycode 24 → `VK_MEDIA_PREV_TRACK`, 25 → `VK_F17`,
   19 → `VK_F18`, 30 → `VK_F22`, … Winit reported those as `Named(...)`, so the
   client sent the wrong keysyms (or none). This matched the user-visible
   "typing aze echoes ~e".
   **Fix:** the client now prefers the OS text (`event.text`) over the logical
   key for single printable characters (and maps Ctrl+letter control codes back
   to the letter). Named keys still handle Enter/Tab/Escape/arrows.
2. **Held modifiers were released by synthesized ones.** Typing `A` with Shift
   held pressed Shift again and released it on key-up, flipping the remote
   modifier state. **Fix:** the injector tracks down keycodes and only releases
   modifiers it pressed itself.
3. **Releases did not match presses.** A release event has no text, so it fell
   back to `logical_key` — Wine's bogus named key. The press sent `q`, the
   release sent `F17` (or nothing), so the remote key stayed down and the X
   server auto-repeated it forever ("another key repeats" after a while).
   **Fix:** the client remembers the keysym per physical key on press and uses
   it for the release; losing OS focus releases everything still held.
4. **Auto-repeat was forwarded on top of the server's own repeat.** Verified
   on the remote Xvfb: a single XTEST press held for 1.5 s produced 25
   characters. Local winit repeats are now ignored: the remote server repeats
   a held key at its own rate.
5. **Ctrl shortcuts did not work.** Winit deliberately ignores the character
   when Ctrl is held and falls back to the VK — which Wine mangles into a
   spare named key (`Ctrl+C` arrived as `Named(AudioVolumeDown)`, text none).
   **Fix:** the client caches the plain (unmodified, ASCII) character per
   physical key from text events and uses it when a Ctrl/Alt/Super press has
   no text and the physical key is a character key. Genuine named keys (Enter,
   arrows, F-keys) are unchanged. Cold start: pressing a shortcut before the
   key has ever been typed sends nothing (deliberately no wrong-layout guess).
3. **Flaky display startup.** Killing Xvfb with SIGKILL left
   `/tmp/.X11-unix/X<n>` behind; `-displayfd` selects by lock file, so the next
   server picked the dead socket and failed to bind. The readiness connect could
   also succeed against the dying old server.
   **Fix:** SIGTERM + wait on shutdown, remove stale sockets (no lock, connect
   refused, owned by us), reject a socket older than the spawn, keep the
   `-displayfd` pipe open (Xvfb writes again when retrying a display; a closed
   read end delivers SIGPIPE), and retry the spawn.

## Verified under Wine

| Check | Result |
|---|---|
| Window opens, xterm renders, pixel streaming | yes |
| US keycodes 24/25/26, 10–13, 38 | remote received `qwe1234a`, unchanged 1.5 s later (no stuck key) |
| Shift held across two `a` taps, then released | remote received `AAa` |
| `a` held 1.2 s | ~17 repeats from the remote X server, then stable |
| type `c`, then Ctrl+C | bytes `63 03` |
| type `d`, then Ctrl+D | bytes `64 04` |
| Ctrl+C before typing `c` | nothing (cold-start caveat, no wrong guess) |
| Client log | `logical_key` still shows Wine's bogus named keys; `sym` is correct |

## Not verified / caveats

- The ssh bootstrap (needs `ssh.exe`; works on real Windows, not in a prefix).
- The French-layout path end-to-end: the host's KDE layout is French but its
  XWayland keymap reports `us`, and the locally extracted Xvfb ignored
  `setxkbmap -layout fr`, so a French X keymap could not be exercised here.
  With text-first mapping, aligning the X keymap with the compositor layout is
  what makes the produced characters match the physical keyboard.
- DPI/scaling, real Windows window manager quirks, signing/Defender: real
  Windows (M3).

## Validated on real Windows

Date: 2026-10-23. The static `xcom.exe` was run on real Windows against the
Linux VM (the Wine notes above remain the way to test builds locally).

What was checked:

- windows render at the remote size and present correctly; resize/reflow works;
- **menus/popups**: an override-redirect context menu appeared one title bar
  too high. The client anchored content-relative remote coordinates to the
  parent window's *frame* (`outer_position`, which includes the title bar and
  the DWM border). Undecorated children now anchor to the parent's client area
  (`inner_position`); menus land at the pointer. A menu whose window is not
  focused also stays dismissable: Escape is forwarded when focus leaves the
  session (see `docs/m2.md`);
- **clipboard mirrors both ways** (text, 60 KB cap). One failure looked like a
  one-way clipboard bug but was a stale daemon on the VM; the client now warns
  when the handshake's build id differs (or is absent);
- `--backend` is Linux-only: on Windows the flag is ignored (it previously
  printed a misleading XWayland line when `DISPLAY` happened to be set).
- **Display size**: an external monitor exposed a window that could be seen but
  not clicked on its right side — a maximized window was wider than the
  session's 1600x1200 virtual display, and the X pointer cannot leave the
  screen. The client now requests a display sized to the local desktop and caps
  windows at the session size (`docs/m2.md`).

Still untested on Windows: display scaling above 100% (remote pixels map 1:1 to
physical pixels, so text may look small), multi-monitor setups, and long
sessions.
