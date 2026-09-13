#!/usr/bin/env bash
# Exercise tab rename through real pointer input, using prebuilt host/CLI binaries.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/../.." && pwd)"
for dependency in xvfb-run xdotool dbus-run-session jq setsid; do
  command -v "$dependency" >/dev/null || { echo "Missing dependency: $dependency"; exit 2; }
done
if [ "${1:-}" != --inside ]; then
  LIMUX_RENAME_TEST_DIR="$(mktemp -d -t limux-tab-rename-XXXXXX)"
  export LIMUX_RENAME_TEST_DIR
  export XDG_DATA_HOME="$LIMUX_RENAME_TEST_DIR/data" XDG_STATE_HOME="$LIMUX_RENAME_TEST_DIR/state"
  export XDG_CONFIG_HOME="$LIMUX_RENAME_TEST_DIR/config" XDG_RUNTIME_DIR="$LIMUX_RENAME_TEST_DIR/runtime"
  export XDG_CACHE_HOME="$LIMUX_RENAME_TEST_DIR/cache"
  mkdir -p "$XDG_DATA_HOME/limux" "$XDG_STATE_HOME" "$XDG_CONFIG_HOME/ghostty" "$XDG_RUNTIME_DIR" "$XDG_CACHE_HOME"
  chmod 700 "$XDG_RUNTIME_DIR"
  exec xvfb-run -a -s '-screen 0 1440x1000x24 -nolisten tcp' \
    dbus-run-session -- bash "$0" --inside
fi

HOST="${LIMUX_TEST_HOST:-$ROOT_DIR/target/debug/limux}"
CLI="${LIMUX_TEST_CLI:-$ROOT_DIR/target/debug/limux-cli}"
if [ ! -x "$HOST" ] || [ ! -x "$CLI" ]; then echo "Build the host and CLI first"; exit 2; fi
RUN_DIR="${LIMUX_RENAME_TEST_DIR:?}"
echo "Test artifacts: $RUN_DIR"
export LIMUX_SOCKET="$RUN_DIR/limux.sock" LIMUX_SOCKET_PATH="$RUN_DIR/limux.sock"
export LIMUX_SOCKET_MODE=runtime GDK_BACKEND=x11 GDK_SCALE=1 GTK_THEME=Adwaita
export LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe LP_NUM_THREADS=1 SHELL=/bin/sh
export GTK_USE_PORTAL=0 GTK_A11Y=none GIO_USE_VFS=local
export LD_LIBRARY_PATH="$ROOT_DIR/ghostty/zig-out/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
export GHOSTTY_RESOURCES_DIR="$ROOT_DIR/ghostty/zig-out/share/ghostty"
export TERMINFO="$ROOT_DIR/ghostty/zig-out/share/terminfo"
unset LIMUX_WORKSPACE_ID LIMUX_PANE_ID LIMUX_SURFACE_ID LIMUX_TAB_ID WAYLAND_DISPLAY
unset LD_PRELOAD GDK_DPI_SCALE
printf 'command = /bin/sh\nshell-integration = none\nfont-size = 12\n' \
  >"$XDG_CONFIG_HOME/ghostty/config"
jq -n --arg cwd "$RUN_DIR" '{
  version: 1, active_workspace_index: 0, top_bar_visible: true,
  sidebar: {visible: true, width: 220},
  workspaces: [{id: "rename", name: "rename", cwd: $cwd,
    layout: {kind: "pane", pane_id: 1, active_tab_id: "terminal", tabs: [
      {id: "terminal", tab_kind: "terminal", cwd: $cwd, custom_name: "Terminal"},
      {id: "browser", tab_kind: "browser", uri: "about:blank", custom_name: "Browser", pinned: true}
    ]}}]
}' >"$XDG_DATA_HOME/limux/session.json"

HOST_PID=""
# shellcheck disable=SC2317 # Invoked by the EXIT trap.
cleanup() {
  local result=$?
  if [ "$result" -ne 0 ]; then
    if command -v import >/dev/null; then import -window root "$RUN_DIR/window.png" || true; fi
    tail -40 "$RUN_DIR/host.stderr"
  fi
  if [ -n "$HOST_PID" ]; then
    kill -TERM -- "-$HOST_PID" 2>/dev/null || true
    wait "$HOST_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT
setsid "$HOST" >"$RUN_DIR/host.stdout" 2>"$RUN_DIR/host.stderr" &
HOST_PID=$!
WINDOW=""
for _ in $(seq 1 450); do
  kill -0 "$HOST_PID" || { echo "Host exited during startup"; exit 1; }
  WINDOW="$(xdotool search --onlyvisible --pid "$HOST_PID" 2>/dev/null | head -1 || true)"
  [ -n "$WINDOW" ] && [ -S "$LIMUX_SOCKET" ] && break
  sleep 0.1
done
if [ -z "$WINDOW" ] || [ ! -S "$LIMUX_SOCKET" ]; then echo "Host startup timed out"; exit 1; fi
xdotool windowsize --sync "$WINDOW" 1200 760 windowfocus --sync "$WINDOW"
sleep 1

assert_title() {
  local tab=$1 expected=$2
  for _ in $(seq 1 50); do
    if "$CLI" --json --id-format both list-panels --workspace rename \
      | jq -e --arg id "1:$tab" --arg title "$expected" \
        '.surfaces[] | select(.surface_id == $id and .title == $title)' >/dev/null; then return; fi
    sleep 0.1
  done
  echo "FAIL: $tab title did not become $expected"
  exit 1
}
click_tab() {
  sleep 0.5
  xdotool mousemove --window "$WINDOW" "$1" 65 click --repeat "${2:-1}" --delay 100 1
}
type_name() {
  xdotool key --clearmodifiers ctrl+a
  xdotool type --clearmodifiers --delay 20 "$1"
}

# A single click must activate the browser without creating a rename editor.
click_tab 335
"$CLI" --json --id-format both list-panels --workspace rename \
  | jq -e '.surfaces[] | select(.surface_id == "1:browser" and .selected)' >/dev/null \
  || { echo 'FAIL: single click did not activate the browser tab'; exit 1; }
type_name 'Not a tab name'
xdotool key Return
assert_title browser Browser

click_tab 335 2
type_name 'Browser renamed'
xdotool key Return
assert_title browser 'Browser renamed'
echo 'PASS: browser double-click rename and Enter commit'

click_tab 335 2
xdotool key --clearmodifiers ctrl+a BackSpace Return
assert_title browser 'Browser renamed'
echo 'PASS: empty rename preserves the current name'

click_tab 245
xdotool type --clearmodifiers --delay 20 ': # Not a tab name'
xdotool key Return
assert_title terminal Terminal

click_tab 245 2
type_name 'Terminal renamed'
xdotool mousemove --window "$WINDOW" 500 350 click 1
assert_title terminal 'Terminal renamed'
echo 'PASS: terminal double-click rename and click-away commit'

for _ in $(seq 1 50); do
  if jq -e '.workspaces[0].layout.tabs |
    any(.id == "terminal" and .custom_name == "Terminal renamed") and
    any(.id == "browser" and .custom_name == "Browser renamed" and .pinned)' \
    "$XDG_DATA_HOME/limux/session.json" >/dev/null; then
    echo 'PASS: both custom names persisted'
    if command -v import >/dev/null; then import -window root "$RUN_DIR/renamed-tabs.png" || true; fi
    exit 0
  fi
  sleep 0.1
done
echo 'FAIL: renamed tabs were not persisted'
exit 1
