# GhostDeck

A GPU-accelerated terminal workspace manager for Linux, powered by Ghostty's rendering engine. A special thanks to the cmux contributors who inspired this build. 

If you are on Mac, please visit https://github.com/manaflow-ai/cmux to download the original. 

https://github.com/user-attachments/assets/6f3047c2-e2b6-49f2-b536-570a1570d0f8

## Features

- **GPU-rendered terminals** via embedded Ghostty (OpenGL)
- **Workspaces** with folder-based naming, persistence across restarts, and sidebar management
- **Ghostty surface splits** (horizontal/vertical) with keyboard navigation
- **Tabbed terminals** within each workspace
- **Right-click context menu** with copy, paste, split, clear
- **Drag-and-drop** workspace reordering with favorites/pinning
- **Animated sidebar** collapse/expand

## Install

Download the latest release from [GitHub Releases](https://github.com/Munawwar/GhostDeck/releases).

**Debian/Ubuntu (.deb)** — recommended:
```bash
sudo dpkg -i ./ghostdeck_0.1.19_amd64.deb
```

**AppImage** — portable across Ubuntu 24.04-era desktops and newer, no install needed:
```bash
chmod +x GhostDeck-0.1.19-x86_64.AppImage
./GhostDeck-0.1.19-x86_64.AppImage
```

Release AppImages are built and checked on the Ubuntu 24.04 `GLIBC_2.39`
floor. GhostDeck still uses the host GTK4 and libadwaita runtime
libraries, so older distributions may need the `.deb`, tarball, or a source
build with matching system packages instead.

**Tarball** — manual install:
```bash
tar xzf ghostdeck-*-linux-x86_64.tar.gz
cd ghostdeck-*-linux-x86_64
sudo ./install.sh
```

**Arch Linux (unofficial AUR package)** — community-maintained by [antonbarchukov](https://github.com/antonbarchukov):
```bash
yay -S ghostdeck-bin
```

The AUR package is available at [`ghostdeck-bin`](https://aur.archlinux.org/packages/ghostdeck-bin). Thanks to [antonbarchukov](https://github.com/antonbarchukov) for packaging GhostDeck for Arch users. Arch packaging is not currently maintained by upstream; please report AUR packaging issues to the package maintainer first. See [issue #5](https://github.com/Munawwar/GhostDeck/issues/5).

To uninstall:
```bash
# deb
sudo apt remove ghostdeck

# tarball
sudo ./install.sh --uninstall
```

### System dependencies

```bash
# Ubuntu/Debian
sudo apt install libgtk-4-1 libadwaita-1-0
```

## Build from source

### Prerequisites

- Rust toolchain (stable)
- Zig
- GTK4 and libadwaita dev packages
- Initialized Ghostty submodule

```bash
# Install dev dependencies (Ubuntu/Debian)
sudo apt install libgtk-4-dev libadwaita-1-dev pkg-config build-essential

# Initialize Ghostty and build the embedded library with GhostDeck's Linux patch
git submodule update --init --recursive
./scripts/build-ghostty.sh -Dapp-runtime=none -Doptimize=ReleaseFast

# Build ghostdeck
cargo build --release

# Run (point to libghostty.so location)
LD_LIBRARY_PATH=ghostty/zig-out/lib:$LD_LIBRARY_PATH ./target/release/ghostdeck
```

### Package a release tarball

```bash
./scripts/package.sh
```

This builds the binary, bundles `libghostty.so`, icons, and an install script into a tarball.
`package.sh` also rebuilds `libghostty.so` with `ReleaseFast` and `-Dcpu=baseline`, applying GhostDeck's Linux embedded patch in a temporary worktree.

## Development

Run the canonical local quality gate before committing:

```bash
./scripts/check.sh
```

Repository maintainability rules live in [`docs/maintainability.md`](docs/maintainability.md).

## Agent integrations

GhostDeck ships first-class hooks for coding agents (Codex, Claude Code, and
Gemini CLI). Every terminal ghostdeck spawns auto-exports
`GHOSTDECK_WORKSPACE_ID` / `GHOSTDECK_SURFACE_ID` / `GHOSTDECK_PANE_ID` /
`GHOSTDECK_TAB_ID` / `GHOSTDECK_SOCKET`, so the CLI auto-targets the right place
with no flags needed from inside the agent's own terminal.

```bash
# Fire a libadwaita toast + sidebar unread badge from any agent
ghostdeck notify --subtitle "needs review" --body "blocked on auth choice" "Input needed"

# Install GhostDeck session-restore hooks for supported agents
ghostdeck hooks setup

# Install GhostDeck's compact agent skill for Codex
ghostdeck skill setup codex

# Drop-in hook handlers translate hook JSON on stdin into notify/session state
echo '{"event":"stop"}' | ghostdeck claude-hook --event stop
echo '{"event":"finished"}' | ghostdeck gemini-hook --event finished

# Spin up a multi-agent collaboration team in the current tab,
# launches each agent's CLI, and writes AGENTS.md describing the
# <agent-msg> XML protocol so peers can talk to each other:
ghostdeck agent-team --agents codex,claude --cwd "$PWD"
# → Codex and Claude can now do:
#   ghostdeck send --workspace claude $'<agent-msg from="codex" to="claude" id="…" ts="…">…</agent-msg>\n'

# Or launch another terminal agent beside the caller:
ghostdeck add-surface --cmd claude

# Add a visible process to the agent's current terminal tab. GhostDeck owns the
# layout: the agent starts on the left and up to three added surfaces stack on
# the right. No direction or target flags are accepted.
created="$(ghostdeck --json add-surface --cwd apps/web)"
surface="$(printf '%s\n' "$created" | jq -r '.surface_id')"
ghostdeck run --surface "$surface" --cmd 'npm run dev'
# Stop the foreground process with Ctrl+C, then remove the added surface:
ghostdeck close-surface --surface "$surface"

# Keep both agents in the same workspace on separate surfaces:
ghostdeck identify --json
ghostdeck list-panels --workspace "$GHOSTDECK_WORKSPACE_ID"
ghostdeck send --workspace "$GHOSTDECK_WORKSPACE_ID" --surface "<peer-surface-id>" \
  $'<agent-msg from="codex" to="claude" id="…" ts="…">…</agent-msg>\n'
```

See the auto-generated `AGENTS.md` (written into the shared cwd) for
the full protocol spec, peer table, and editable Policies section.

Checked-in hook templates live in [`hooks/`](hooks/). They mirror
`ghostdeck hooks setup` for Codex, Claude Code, and Gemini CLI; OpenCode is
omitted until its hook integration is ready.

Coding agents working on **ghostdeck itself** should read [`AGENTS.md`](AGENTS.md)
and [`CLAUDE.md`](CLAUDE.md) in the repo root — those cover the build
loop, crate map, and the `feat/cmux-parity` roadmap tracked in
[`docs/cmux-parity-plan.md`](docs/cmux-parity-plan.md).

## Keyboard shortcuts

Most default shortcuts use `Ctrl`. Fullscreen defaults to `F11`. Custom remaps may also use `Cmd`, which GhostDeck maps to either the Linux `Meta` or `Super` modifier. `Opt` maps to `Alt`.

### App

| Shortcut | Action |
|---|---|
| `Ctrl+Q` | Quit GhostDeck |
| `Ctrl+Alt+N` | Open a new GhostDeck instance |
| `F11` | Toggle fullscreen |

### Find

| Shortcut | Action |
|---|---|
| `Ctrl+F` | Open find on the focused terminal |
| `Ctrl+G` | Find next |
| `Ctrl+Shift+G` | Find previous |
| `Ctrl+Shift+F` | Hide find |
| `Ctrl+E` | Use selection for find |

### Terminal

| Shortcut | Action |
|---|---|
| `Ctrl+K` | Clear scrollback |
| `Ctrl+Shift+C` | Copy selection |
| `Ctrl+Shift+V` | Paste |
| `Ctrl++` | Increase font size |
| `Ctrl+-` | Decrease font size |
| `Ctrl+Shift+0` | Reset font size |

### Workspace And Terminal Surfaces

| Shortcut | Action |
|---|---|
| `Ctrl+Shift+N` | New workspace (folder picker) |
| `Ctrl+Shift+W` | Close workspace |
| `Ctrl+Shift+Left/Right` | Cycle terminal tabs |
| `Ctrl+Shift+D` | Split terminal down |
| `Ctrl+Shift+T` | New terminal tab |
| `Ctrl+D` | Split terminal right |
| `Ctrl+Shift+S` | Swap terminal surfaces in the focused tab |
| `Ctrl+M` | Toggle sidebar |
| `Ctrl+Shift+M` | Toggle top bar |
| `Ctrl+T` | New terminal tab |
| `Ctrl+Shift+,/.` | Focus surface left or right |
| `Ctrl+PageDown/Up` | Next or previous workspace |
| `Ctrl+1-9` | Switch to workspace by number |

## Architecture

```
rust/
  ghostdeck-host-linux/    # GTK4/Adwaita UI (window, sidebar, panes, tabs)
  ghostdeck-ghostty-sys/   # FFI bindings to libghostty
  ghostdeck-core/          # Command dispatcher and state engine
  ghostdeck-protocol/      # Socket wire format types
  ghostdeck-control/       # Unix socket server
  ghostdeck-cli/           # CLI client
```

The terminal rendering is handled by Ghostty 1.3.1's embedded library (`libghostty.so`) with GhostDeck's Linux patch. The UI layer is native GTK4 with libadwaita.

## License

MIT
