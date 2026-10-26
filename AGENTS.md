# Project instructions

`xcom` forwards X11 sessions without carrying pixels over SSH: one `xcomd`
server owns an Xvfb display, the `xcom` client renders each remote window as a
native window. Read the user's global OpenCode preferences separately; this
file is repository-wide project knowledge only.

If `AGENTS.local.md` exists in the repository root, read it before running
tests. It is gitignored and contains dev-box-specific paths and commands.

## Quick commands

```sh
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
scripts/ci.sh                 # clippy, release tests, Windows pure-Rust check, e2e-direct
scripts/testvm.sh up          # idempotent test VM + dev xcomd
scripts/testvm.sh status
scripts/testvm.sh down
scripts/e2e-direct.sh 3
scripts/e2e-ssh.sh 3
scripts/e2e-kitty.sh          # fast idle full-window-frame check
```

`scripts/ci.sh` and `e2e-direct` use release binaries (`target/release/xcom`,
`target/release/xcomd`); build them if missing. CI does not run the e2e
scripts; they are local checks.

## Repository map

- `crates/xcom-proto` — wire messages, framing, build id, logging.
- `crates/xcom-transport` — TLS 1.3 control/bulk channels, tokens, roles.
- `crates/xcom-codec` — rect coalescing and LZ4/raw compression.
- `crates/xcom-x11` — Composite/DAMAGE/MIT-SHM capture, XTEST input, keymap,
  clipboard, window helpers, Xvfb spawning.
- `crates/xcom-server` — `xcomd` CLI, session owner, attach/control, serve loop,
  metrics.
- `crates/xcom-client` — CLI/bootstrap, link threads, winit/softbuffer UI, key
  mapping, clipboard, HUD/metrics.
- `scripts/testvm.sh` — idempotent test VM; builds/refreshes the static dev
  `xcomd` and polls readiness.
- `scripts/e2e-lib.sh` — shared private-Xvfb policy, SSH helper, x11_input
  preparation, bounded pid waits.
- `deploy/testvm/Dockerfile` — test VM packages.
- `docs/m2.md`, `docs/m3.md`, `docs/windows-client.md`,
  `docs/session-security.md`.

## Testing conventions

- Automated e2e clients use a private Xvfb. To see/interact with a window, set
  `CLIENT_DISPLAY=:0`:
  ```sh
  CLIENT_DISPLAY=:0 scripts/e2e-ssh.sh 3
  CLIENT_DISPLAY=:0 APP=gtk3-demo scripts/e2e-ssh.sh 60
  ```
- `scripts/e2e-kitty.sh --interactive` is the manual kitty mode: desktop
  display, no assertion, runs until close. `e2e-ssh.sh` does not accept
  `--interactive`.
- `e2e-kitty.sh` is the fast idle check: it forces cursor blinking, waits for
  the first frame and a quiet gap, then samples 1.5 s. It fails if post-startup
  frames still contain full-window rects; a blink should be a small cursor tile
  (typically one 32×32 tile).
- Use `timeout` around e2e runs. Clean up leaked processes:
  ```sh
  pgrep -a Xvfb
  podman exec xcom-testvm sh -c 'pgrep -a xcomd; pgrep -a Xvfb; pgrep -a kitty'
  ```
- `e2e-ssh.sh` uses `--replace` on the `default` session; other stateful tests
  kill their sessions at start/end.
- `e2e-persist` has a known flaky synthetic close-dialog stage. It uses bounded
  waits, so a missed click fails instead of hanging; retry if it fails there.
- `scripts/check-frames.py` is the shared frame-geometry checker;
  `scripts/check-ppm.py` validates PPM/HUD output. Do not embed new parsers in
  individual tests.
- A window at the full display size plus the +32 placement can break screen
  readback; use a smaller window or a larger client display.

## Test VM

- Podman container `xcom-testvm`, host networking, SSH on port 2222.
- `testvm.sh up` is idempotent: a build-state label skips `podman build` when
  the Dockerfile/key are unchanged, the container is recreated only when its
  image changes, and readiness is polled. Do not hold a flock across
  `podman run`: conmon inherits the FD and keeps the lock.
- The container runs the **static dev** `xcomd` mounted read-only from
  `dist/xcomd-dev-linux-x86_64`. `testvm.sh up` rebuilds it when sources are
  newer and restarts the container to refresh the mount.
- It installs kitty, Mesa llvmpipe, `glxinfo`, xterm and gtk3 for tests.
- The client warns when its build id differs from the remote `xcomd`; rebuild
  `target/release/xcom` after client changes.

## Build and environment facts

- The server is static musl; the Linux client is glibc because winit/softbuffer
  `dlopen` X11/Wayland. The Windows client is built in CI with
  `scripts/build-windows.sh` / the release workflow.
- `--backend auto` prefers X11/XWayland; `--backend wayland` is opt-in.
- `XCOM_DEBUG=1` enables client and remote protocol tracing; it propagates over
  SSH. `XCOM_DEBUG_KEYS=1` adds key mapping traces.
- Session bootstrap tokens are single-use and must not be placed on command
  lines in normal use; prefer the SSH bootstrap.

## Project gotchas

- Do not run `cargo fmt --all`: the tree has pre-existing rustfmt diffs. Format
  only the files you touch.
- Keep capture/input changes measurable: run the kitty idle check, the
  full-window animation e2e, and the relevant session tests. Capture design
  details live in `docs/m2.md`.

## Docs

- `docs/m2.md` — window management, popup placement, clipboard, cursor, HUD.
- `docs/m3.md` — persistent sessions, reattach, close dialog.
- `docs/windows-client.md` — Wine/real-Windows findings and caveats.
- `docs/session-security.md` — session trust boundaries and hardening.
