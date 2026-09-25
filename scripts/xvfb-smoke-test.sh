#!/usr/bin/env bash
# scripts/xvfb-smoke-test.sh - Headless end-to-end smoke test for the
# ghostdeck agent-integrations stack. Runs a real ghostdeck GTK host under Xvfb,
# exercises ghostdeck-cli against the live Unix socket, asserts expected
# behavior, then tears down. Zero display hardware required.
#
# Usage:
#   ./scripts/xvfb-smoke-test.sh                # release build
#   GHOSTDECK_SMOKE_PROFILE=debug ./scripts/xvfb-smoke-test.sh
set -euo pipefail

PROFILE="${GHOSTDECK_SMOKE_PROFILE:-release}"
ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT_DIR"

DEMO_DIR="$(mktemp -d -t ghostdeck-smoke-XXXXXX)"
LOG_DIR="$DEMO_DIR/logs"
mkdir -p "$LOG_DIR"

echo "== ghostdeck agent-integrations smoke test =="
echo "profile:   $PROFILE"
echo "demo dir:  $DEMO_DIR"
echo "log dir:   $LOG_DIR"

# --- 1. Deps --------------------------------------------------------------
command -v xvfb-run >/dev/null || {
  echo "FAIL: xvfb-run not installed (sudo pacman -S xorg-server-xvfb)"
  exit 2
}
command -v cargo >/dev/null || { echo "FAIL: cargo missing"; exit 2; }
command -v sed >/dev/null || { echo "FAIL: sed missing"; exit 2; }

# --- 2. Build -------------------------------------------------------------
if [ "$PROFILE" = "release" ]; then
  CARGO_FLAGS="--release"
  BIN_DIR="target/release"
else
  CARGO_FLAGS=""
  BIN_DIR="target/debug"
fi

echo "-- building ghostdeck-cli ($PROFILE)..."
cargo build $CARGO_FLAGS -p ghostdeck-cli --bin ghostdeck-cli 2>&1 | tail -3

echo "-- building ghostdeck-host-linux ($PROFILE)..."
cargo build $CARGO_FLAGS -p ghostdeck-host-linux 2>&1 | tail -3

GHOSTDECK_HOST="$ROOT_DIR/$BIN_DIR/ghostdeck"
GHOSTDECK_CLI="$ROOT_DIR/$BIN_DIR/ghostdeck-cli"
[ -x "$GHOSTDECK_HOST" ] || { echo "FAIL: host binary missing at $GHOSTDECK_HOST"; exit 2; }
[ -x "$GHOSTDECK_CLI" ]  || { echo "FAIL: cli binary missing at $GHOSTDECK_CLI"; exit 2; }

# Both host profiles load the locally built libghostty.so.
LIBGHOSTTY_DIR="$ROOT_DIR/ghostty/zig-out/lib"
if [ -d "$LIBGHOSTTY_DIR" ]; then
  export LD_LIBRARY_PATH="$LIBGHOSTTY_DIR:${LD_LIBRARY_PATH:-}"
fi

# --- 3. Stage 0: dry-run agent-team (no host) ----------------------------
# Fast sanity pass — if this fails nothing else will work.
echo
echo "== stage 0: agent-team --dry-run (no host) =="
"$GHOSTDECK_CLI" agent-team --dry-run \
  --agents codex,claude,opencode \
  --cwd "$DEMO_DIR" \
  2>&1 | tee "$LOG_DIR/stage0.txt"

grep -q "peers=\[codex, claude, opencode\]" \
  "$LOG_DIR/stage0.txt" \
  || { echo "FAIL: stage 0 dry-run did not report expected peers"; exit 1; }
echo "stage 0: OK"

# --- 4. Launch the live host under Xvfb ----------------------------------
# Each smoke run gets its own socket path so we don't collide with the
# user's real ghostdeck session.
SOCKET="$DEMO_DIR/ghostdeck.sock"
export GHOSTDECK_SOCKET="$SOCKET"
export GHOSTDECK_SOCKET_PATH="$SOCKET"
export GHOSTDECK_SOCKET_MODE="runtime"
export XDG_DATA_HOME="$DEMO_DIR/data"
export XDG_STATE_HOME="$DEMO_DIR/state"
export XDG_CONFIG_HOME="$DEMO_DIR/config"
export XDG_RUNTIME_DIR="$DEMO_DIR/runtime"
mkdir -p "$XDG_DATA_HOME/ghostdeck" "$XDG_STATE_HOME" "$XDG_CONFIG_HOME" "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"
cat > "$XDG_DATA_HOME/ghostdeck/session.json" <<SMOKE_SESSION
{
  "version": 1,
  "active_workspace_index": 0,
  "top_bar_visible": true,
  "sidebar": { "visible": true, "width": 220 },
  "workspaces": [
    {
      "id": "00000000-0000-4000-8000-000000000001",
      "name": "ghostdeck",
      "favorite": false,
      "cwd": "$DEMO_DIR",
      "folder_path": "$DEMO_DIR",
      "layout": {
        "kind": "pane",
        "pane_id": 1,
        "active_tab_id": "terminal-0",
        "tabs": [
          {
            "id": "terminal-0",
            "custom_name": null,
            "pinned": false,
            "tab_kind": "terminal",
            "cwd": "$DEMO_DIR"
          }
        ]
      }
    }
  ]
}
SMOKE_SESSION

echo
echo "== stage 1: boot ghostdeck host under xvfb-run =="
# Under Xvfb there is no GPU, so Mesa would fall back to llvmpipe, which
# has historically crashed on Ghostty's shader variants. Force softpipe
# (slower but stable), and pin GL version to avoid newer-feature probes.
export LIBGL_ALWAYS_SOFTWARE=1
export GALLIUM_DRIVER=softpipe
export LP_NUM_THREADS=1
export MESA_GL_VERSION_OVERRIDE="${MESA_GL_VERSION_OVERRIDE:-3.3}"
xvfb-run -a -s "-screen 0 1280x800x24 +extension GLX +render" \
  "$GHOSTDECK_HOST" >"$LOG_DIR/host.stdout" 2>"$LOG_DIR/host.stderr" &
HOST_PID=$!
echo "host PID: $HOST_PID (socket=$SOCKET)"

cleanup() {
  local rc=$?
  echo
  echo "-- cleanup (rc=$rc) --"
  if kill -0 "$HOST_PID" 2>/dev/null; then
    kill "$HOST_PID" 2>/dev/null || true
    sleep 1
    kill -9 "$HOST_PID" 2>/dev/null || true
  fi
  # Tail the host log on failure to aid debugging.
  if [ "$rc" -ne 0 ]; then
    echo "-- host.stdout (tail) --"
    tail -n 40 "$LOG_DIR/host.stdout" 2>/dev/null || true
    echo "-- host.stderr (tail) --"
    tail -n 40 "$LOG_DIR/host.stderr" 2>/dev/null || true
    echo "artifacts retained at: $DEMO_DIR"
  else
    # Clean slate on success.
    rm -rf "$DEMO_DIR"
  fi
}
trap cleanup EXIT INT TERM

# Poll for the socket (up to 30s)
for i in $(seq 1 60); do
  if [ -S "$SOCKET" ]; then
    echo "socket up after ${i}*500ms"
    break
  fi
  if ! kill -0 "$HOST_PID" 2>/dev/null; then
    echo "FAIL: host process died before opening the socket"
    exit 1
  fi
  sleep 0.5
done

[ -S "$SOCKET" ] || { echo "FAIL: socket $SOCKET never appeared"; exit 1; }
for _ in $(seq 1 40); do
  "$GHOSTDECK_CLI" --json surface-health --workspace 00000000-0000-4000-8000-000000000001 \
    > "$LOG_DIR/initial-surface-health.json" 2>/dev/null || true
  grep -Fq '"healthy":true' "$LOG_DIR/initial-surface-health.json" && break
  sleep 0.25
done
grep -Fq '"healthy":true' "$LOG_DIR/initial-surface-health.json" \
  || { echo "FAIL: initial Ghostty surface did not become healthy under Xvfb"; exit 1; }

# --- 5. Stage 1b: terminal cwd persistence -------------------------------
echo
echo "== stage 1b: terminal cwd reaches the persisted session =="
CWD_TARGET="$DEMO_DIR/terminal-cwd"
mkdir -p "$CWD_TARGET"
env GHOSTDECK_WORKSPACE_ID=00000000-0000-4000-8000-000000000001 \
  GHOSTDECK_TAB_ID=terminal-0 GHOSTDECK_SURFACE_ID=1:terminal-0:leaf-0 \
  "$GHOSTDECK_CLI" --json --id-format both add-surface --cwd "$CWD_TARGET" > "$LOG_DIR/stage1b-add.json"
CWD_SURFACE_ID="$(sed -n 's/.*"surface_id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$LOG_DIR/stage1b-add.json" | head -1)"
sleep 0.25
"$GHOSTDECK_CLI" rename-workspace \
  --workspace 00000000-0000-4000-8000-000000000001 \
  ghostdeck-cwd-test >/dev/null

for _ in $(seq 1 20); do
  grep -Fq "\"cwd\": \"$CWD_TARGET\"" "$XDG_DATA_HOME/ghostdeck/session.json" && break
  sleep 0.25
done

grep -Fq "\"cwd\": \"$CWD_TARGET\"" "$XDG_DATA_HOME/ghostdeck/session.json" \
  || { echo "FAIL: terminal cwd was not persisted"; exit 1; }
env GHOSTDECK_WORKSPACE_ID=00000000-0000-4000-8000-000000000001 \
  GHOSTDECK_TAB_ID=terminal-0 GHOSTDECK_SURFACE_ID=1:terminal-0:leaf-0 \
  "$GHOSTDECK_CLI" close-surface --surface "$CWD_SURFACE_ID" >/dev/null
echo "stage 1b: OK"

# --- 5. Stage 1c: caller-owned surface lifecycle --------------------------
echo
echo "== stage 1c: add, run in, and close a caller-owned surface =="
CALLER_WORKSPACE_ID="00000000-0000-4000-8000-000000000001"
CALLER_TAB_ID="terminal-0"
CALLER_SURFACE_ID="1:terminal-0:leaf-0"
CALLER_ENV=("GHOSTDECK_WORKSPACE_ID=$CALLER_WORKSPACE_ID" "GHOSTDECK_TAB_ID=$CALLER_TAB_ID" "GHOSTDECK_SURFACE_ID=$CALLER_SURFACE_ID")
env "${CALLER_ENV[@]}" "$GHOSTDECK_CLI" --json --id-format both add-surface > "$LOG_DIR/stage1c-add.json"
CHILD_SURFACE_ID="$(sed -n 's/.*"surface_id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$LOG_DIR/stage1c-add.json" | head -1)"
[ -n "$CHILD_SURFACE_ID" ] || { echo "FAIL: add-surface response missing surface_id"; exit 1; }

RUN_PROOF="$DEMO_DIR/run-surface-proof"
env "${CALLER_ENV[@]}" "$GHOSTDECK_CLI" run --surface "$CHILD_SURFACE_ID" --cmd "printf run-ok > '$RUN_PROOF'"
for _ in $(seq 1 50); do
  [ -f "$RUN_PROOF" ] && break
  sleep 0.1
done
[ -f "$RUN_PROOF" ] && [ "$(cat "$RUN_PROOF")" = "run-ok" ] \
  || { echo "FAIL: run did not execute in the added surface"; exit 1; }

if env "${CALLER_ENV[@]}" "$GHOSTDECK_CLI" close-surface --surface "$CALLER_SURFACE_ID" >/dev/null 2>&1; then
  echo "FAIL: close-surface allowed closing the caller source"; exit 1
fi

env "${CALLER_ENV[@]}" "$GHOSTDECK_CLI" close-surface --surface "$CHILD_SURFACE_ID"
env "${CALLER_ENV[@]}" "$GHOSTDECK_CLI" --json list-panels --workspace "$CALLER_WORKSPACE_ID" \
  > "$LOG_DIR/stage1c-panels.json"
if grep -Fq "$CHILD_SURFACE_ID" "$LOG_DIR/stage1c-panels.json"; then
  echo "FAIL: closed surface remains in the caller tab"; exit 1
fi
echo "stage 1c: OK (add, run, source protection, close)"

# --- 5. Stage 2: live agent-team ------------------------------------------
echo
echo "== stage 2: agent-team against live host (--no-launch) =="
# --no-launch keeps the workspace commands from actually spawning codex/
# claude binaries (which may not be installed in CI); the bridge + AGENTS.md
# + allow_name=true path are still fully exercised.
env "${CALLER_ENV[@]}" "$GHOSTDECK_CLI" --id-format both agent-team \
  --agents codex,claude \
  --cwd "$DEMO_DIR" \
  --no-launch \
  2>&1 | tee "$LOG_DIR/stage2.txt"

grep -q "peers=\[codex, claude\]" "$LOG_DIR/stage2.txt" \
  || { echo "FAIL: live agent-team did not create peers"; exit 1; }
[ -f "$DEMO_DIR/AGENTS.md" ] \
  || { echo "FAIL: AGENTS.md not written to $DEMO_DIR"; exit 1; }

# Assert the runtime AGENTS.md has the protocol envelope + both peers.
grep -q "<agent-msg"  "$DEMO_DIR/AGENTS.md" || { echo "FAIL: AGENTS.md missing <agent-msg>"; exit 1; }
grep -q "\bcodex\b"   "$DEMO_DIR/AGENTS.md" || { echo "FAIL: AGENTS.md missing codex peer"; exit 1; }
grep -q "\bclaude\b"  "$DEMO_DIR/AGENTS.md" || { echo "FAIL: AGENTS.md missing claude peer"; exit 1; }
echo "stage 2: OK (AGENTS.md + 2 peer surfaces)"

for name in codex claude; do
  created="$("$GHOSTDECK_CLI" --json --id-format both new-workspace --cwd "$DEMO_DIR")"
  workspace_id="$(printf '%s\n' "$created" | sed -n 's/.*"workspace_id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -1)"
  "$GHOSTDECK_CLI" rename-workspace --workspace "$workspace_id" "$name" >/dev/null
done

# --- 6. Stage 3: list-workspaces sanity -----------------------------------
echo
echo "== stage 3: list-workspaces sees both peers =="
"$GHOSTDECK_CLI" list-workspaces 2>&1 | tee "$LOG_DIR/stage3.txt"
grep -q codex  "$LOG_DIR/stage3.txt" || { echo "FAIL: list-workspaces missing codex"; exit 1; }
grep -q claude "$LOG_DIR/stage3.txt" || { echo "FAIL: list-workspaces missing claude"; exit 1; }
echo "stage 3: OK"

# --- 7. Stage 4: by-name send (the phase-5 allow_name=true unlock) --------
# This is the single most important assertion in the whole harness —
# it proves that `ghostdeck send --workspace <name>` resolves to the right
# workspace via the bridge. Without allow_name=true this errors out.
echo
echo "== stage 4: surface.send_text by workspace name =="
ENVELOPE=$'<agent-msg from="codex" to="claude" id="smoke-1" ts="2026-04-19T23:59:00Z"><request>smoke test ping</request></agent-msg>\n'
if "$GHOSTDECK_CLI" send --workspace claude "$ENVELOPE" 2>&1 | tee "$LOG_DIR/stage4.txt"; then
  echo "stage 4: OK (by-name send accepted)"
else
  echo "FAIL: by-name send to 'claude' failed — allow_name=true may be regressed"
  exit 1
fi

# --- 8. Stage 5: by-name notify -------------------------------------------
echo
echo "== stage 5: notification.create by workspace name =="
if "$GHOSTDECK_CLI" notify --workspace claude --subtitle "smoke" --body "all good" "Smoke test" \
     2>&1 | tee "$LOG_DIR/stage5.txt"; then
  echo "stage 5: OK (by-name notify accepted)"
else
  echo "FAIL: by-name notify failed — allow_name=true on notification.create may be regressed"
  exit 1
fi
