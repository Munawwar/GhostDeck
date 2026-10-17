#!/usr/bin/env python3
"""Exercise layout, focus and exact input routing against the smoke GTK host."""
import json
import os
from pathlib import Path
import subprocess
import sys
import time

cli, demo = sys.argv[1], Path(sys.argv[2])
workspace = "00000000-0000-4000-8000-000000000001"
source = "1:terminal-0:leaf-0"
env = dict(os.environ, GHOSTDECK_WORKSPACE_ID=workspace,
           GHOSTDECK_TAB_ID="terminal-0", GHOSTDECK_SURFACE_ID=source)
session = Path(env["XDG_DATA_HOME"]) / "ghostdeck/session.json"


def command(*args, success=True):
    result = subprocess.run([cli, "--json", "--id-format", "both", *args],
                            env=env, capture_output=True, text=True)
    assert (result.returncode == 0) == success, (args, result.stdout, result.stderr)
    return json.loads(result.stdout) if success else None


def wait_for(check):
    deadline = time.monotonic() + 15
    last_error = None
    while time.monotonic() < deadline:
        try:
            result = check()
            if result:
                return result
        except (AssertionError, FileNotFoundError, json.JSONDecodeError) as error:
            last_error = error
        time.sleep(0.1)
    print("Active terminal on failure:", command("read-screen", "--surface", active), flush=True)
    raise AssertionError(f"Timed out: {check}; last failure: {last_error}")


def rectangles(node, x=0, y=0, width=1, height=1):
    if node["kind"] == "leaf":
        return [(node["leaf_id"], x, y, width, height)]
    ratio = node["ratio"]
    if node["orientation"] == "horizontal":
        return (rectangles(node["start"], x, y, width * ratio, height)
                + rectangles(node["end"], x + width * ratio, y, width * (1-ratio), height))
    return (rectangles(node["start"], x, y, width, height * ratio)
            + rectangles(node["end"], x, y + height * ratio, width, height * (1-ratio)))


def layout_matches(surfaces, active):
    state = json.loads(session.read_text())
    tab = next(w for w in state["workspaces"] if w["id"] == workspace)["layout"]["tabs"][0]
    actual = rectangles(tab["tree"])
    assert [r[0] for r in actual] == [s.rsplit(":", 1)[1] for s in surfaces], actual
    assert tab["active_leaf_id"] == active.rsplit(":", 1)[1], tab
    columns = 1 + (len(surfaces) - 1 + 2) // 3
    for index, (_, x, y, width, height) in enumerate(actual):
        column = 0 if index == 0 else 1 + (index - 1) // 3
        rows = 1 if index == 0 else min(3, len(surfaces) - 1 - (column - 1) * 3)
        row = 0 if index == 0 else (index - 1) % 3
        expected = (column / columns, row / rows, 1 / columns, 1 / rows)
        assert all(abs(a-b) < 0.035 for a, b in zip((x, y, width, height), expected)), (actual, expected)
    return True


surfaces = [source]
active = source
for index in range(7):
    created = command("add-surface")
    surfaces.append(created["surface_id"])
    wait_for(lambda: layout_matches(surfaces, active))

# Wait for GTK realization as well as the persisted layout before clicking.
wait_for(lambda: all(row["healthy"] for row in command("surface-health")["surfaces"]))
# Reproduce typing in the rightmost surface while an agent targets earlier ones.
env["DISPLAY"] = (demo / "display").read_text()
window = subprocess.check_output(["xdotool", "search", "--onlyvisible", "--class", "ghostdeck"],
                                 env=env, text=True).splitlines()[-1]
geometry = dict(line.split("=", 1) for line in subprocess.check_output(
    ["xdotool", "getwindowgeometry", "--shell", window], env=env, text=True).splitlines())
subprocess.run(["xdotool", "windowfocus", "--sync", window, "mousemove", "--window", window,
                str(int(geometry["WIDTH"]) - 80), str(int(geometry["HEIGHT"]) // 2),
                "sleep", "0.2", "click", "1"], env=env, check=True)
active = surfaces[-1]
wait_for(lambda: layout_matches(surfaces, active))
assert [row["surface_id"] for row in command("list-panels")["surfaces"]] == surfaces

# Shell expansion proves the receiving process's identity, not just execution.
for index, target in enumerate(surfaces):
    proof = demo / f"target-{index}"
    command("send", "--surface", target,
            f"printf '%s' \"$GHOSTDECK_SURFACE_ID\" > '{proof}'")
    command("send-key", "--surface", target, "Return")
    wait_for(lambda: proof.exists() and proof.read_text() == target)

missing = "1:terminal-0:missing-leaf"
for operation, value in (("send", "SHOULD_NOT_BE_TYPED"), ("send-key", "Return")):
    command(operation, "--surface", missing, value, success=False)
command("read-screen", "--surface", missing, success=False)
command("close-surface", "--surface", source, success=False)

# Leave the user's command half-entered while background layout changes occur.
keyboard_proof = demo / "keyboard-focus"
subprocess.run(["xdotool", "type", "--clearmodifiers",
                f"printf '%s' \"$GHOSTDECK_SURFACE_ID\" > '{keyboard_proof}'"], env=env, check=True)

# Remove a middle row, then exit a shell in a different column. Gaps must close.
removed = surfaces.pop(2)
command("close-surface", "--surface", removed)
wait_for(lambda: layout_matches(surfaces, active))
command("send", "--surface", removed, "SHOULD_NOT_BE_TYPED", success=False)
removed = surfaces.pop(4)
command("run", "--surface", removed, "--cmd", "exit")
wait_for(lambda: layout_matches(surfaces, active))

# Creation after compaction appends at the bottom of the final column.
proof = demo / "add-command-target"
created = command("add-surface", "--cmd",
                  f"printf '%s' \"$GHOSTDECK_SURFACE_ID\" > '{proof}'")
surfaces.append(created["surface_id"])
wait_for(lambda: proof.exists() and proof.read_text() == surfaces[-1])
wait_for(lambda: layout_matches(surfaces, active))
subprocess.run(["xdotool", "key", "Return"], env=env, check=True)
wait_for(lambda: keyboard_proof.exists() and keyboard_proof.read_text() == active)
for target in surfaces[:0:-1]:
    command("close-surface", "--surface", target)
    surfaces.remove(target)
    if target == active:
        active = source
    wait_for(lambda: layout_matches(surfaces, active))
print("surface lifecycle: ordered equal columns, exact routing, invalid IDs, close and shell exit OK")
