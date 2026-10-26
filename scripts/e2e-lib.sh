#!/usr/bin/env bash
# Shared display plumbing for the end-to-end scripts.
#
# Source this file; do not execute it. It provides one policy for the local
# client display:
#
#   interactive:  CLIENT_DISPLAY (default :0), no private Xvfb;
#   automated:    CLIENT_DISPLAY when set, otherwise a private Xvfb.
#
# Call e2e_choose_display <interactive> <geometry> <log>; it sets E2E_DISPLAY
# and E2E_XVFB_PID (empty when an existing display was chosen). The caller owns
# the EXIT trap and must call e2e_stop_xvfb from it.
#
# The private display is picked from :99 upwards and readiness is the X socket,
# not a fixed sleep.

E2E_REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
E2E_KEY=${E2E_KEY:-/tmp/opencode/e2e/id_ed25519}
E2E_SSH_ARGS="-p 2222 -i $E2E_KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null"

# Run a command in the test VM over ssh.
e2e_ssh() {
    ssh -p 2222 -i "$E2E_KEY" -o StrictHostKeyChecking=no \
        -o UserKnownHostsFile=/dev/null xcom@127.0.0.1 "$@"
}

# Ensure the test VM is current and running.
e2e_ensure_testvm() {
    "$E2E_REPO/scripts/testvm.sh" up
}

# Build the XTEST helper and install it in the test VM.
e2e_prepare_x11_input() {
    e2e_ensure_testvm >/dev/null
    (cd "$E2E_REPO" && cargo build -q -p xcom-x11 --example x11_input)
    podman cp "$E2E_REPO/target/debug/examples/x11_input" \
        xcom-testvm:/usr/local/bin/x11_input
}

# Wait for a pid with a bound, so a missed dialog/click fails the test instead
# of hanging it. Kills and reaps the process on timeout.
e2e_wait_pid() {
    local pid=$1
    local timeout=$2
    local label=${3:-process}
    local deadline=$((SECONDS + timeout))
    while kill -0 "$pid" 2>/dev/null; do
        if [ "$SECONDS" -ge "$deadline" ]; then
            echo "error: $label still running after ${timeout}s" >&2
            kill "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
            return 1
        fi
        sleep 0.1
    done
    wait "$pid" 2>/dev/null || true
}

# Find Xvfb without assuming it is on PATH (the dev box has it under
# ~/.local/xvfb). XCOM_XVFB overrides the lookup.
e2e_find_xvfb() {
    if [ -n "${XCOM_XVFB:-}" ] && [ -x "$XCOM_XVFB" ]; then
        printf '%s\n' "$XCOM_XVFB"
    elif command -v Xvfb >/dev/null 2>&1; then
        command -v Xvfb
    elif [ -x "$HOME/.local/xvfb/usr/bin/Xvfb" ]; then
        printf '%s\n' "$HOME/.local/xvfb/usr/bin/Xvfb"
    else
        return 1
    fi
}

# Start a private Xvfb. Sets E2E_DISPLAY and E2E_XVFB_PID. The screen spec is
# WxH or WxHxD; a missing depth defaults to 24 bit.
e2e_start_xvfb() {
    local geometry=$1
    local log=$2
    local xvfb display
    case "$geometry" in
        *x*x*) ;;
        *) geometry="${geometry}x24" ;;
    esac
    xvfb=$(e2e_find_xvfb) || {
        echo "error: Xvfb not found; set XCOM_XVFB to its path" >&2
        return 1
    }
    for display in $(seq 99 109); do
        [ -e "/tmp/.X11-unix/X$display" ] && continue
        "$xvfb" ":$display" -screen 0 "$geometry" -nolisten tcp +extension XTEST \
            > "$log" 2>&1 &
        E2E_XVFB_PID=$!
        for _ in $(seq 1 100); do
            [ -S "/tmp/.X11-unix/X$display" ] && break
            kill -0 "$E2E_XVFB_PID" 2>/dev/null || break
            sleep 0.05
        done
        if [ -S "/tmp/.X11-unix/X$display" ]; then
            sleep 0.1   # X server finished its post-socket setup
            E2E_DISPLAY=":$display"
            return 0
        fi
        kill "$E2E_XVFB_PID" 2>/dev/null || true
        wait "$E2E_XVFB_PID" 2>/dev/null || true
        E2E_XVFB_PID=""
    done
    echo "error: could not start a private Xvfb (tried :99-:109)" >&2
    [ -s "$log" ] && tail -10 "$log" >&2
    return 1
}

e2e_stop_xvfb() {
    if [ -n "${E2E_XVFB_PID:-}" ]; then
        kill "$E2E_XVFB_PID" 2>/dev/null || true
        wait "$E2E_XVFB_PID" 2>/dev/null || true
        E2E_XVFB_PID=""
    fi
}

# Choose the local client display. `interactive=1` keeps the window on the
# desktop; automated runs are isolated unless CLIENT_DISPLAY says otherwise.
e2e_choose_display() {
    local interactive=$1
    local geometry=$2
    local log=$3
    E2E_XVFB_PID=""
    if [ "$interactive" = 1 ]; then
        E2E_DISPLAY=${CLIENT_DISPLAY:-:0}
    elif [ -n "${CLIENT_DISPLAY:-}" ]; then
        E2E_DISPLAY=$CLIENT_DISPLAY
    else
        e2e_start_xvfb "$geometry" "$log" || return 1
    fi
    CLIENT_DISPLAY=$E2E_DISPLAY
}
