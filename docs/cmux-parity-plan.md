# cmux-parity plan (revised after architectural discovery)

## Architecture discovery

GhostDeck has **two control servers**:

1. **Standalone `ghostdeck-control-server` binary** — uses `ghostdeck_core::Dispatcher`
   + `ControlState` and supports the **full** command vocabulary. Used for
   tests and for CLI calls when the GUI isn't running.

2. **Embedded bridge inside `ghostdeck-host-linux`** — `control_bridge.rs` only
   routes a narrow subset of methods to the GTK main loop. Supports
   `system.ping`, `system.identify`, `workspace.{current,list,create,
   select,rename,close}`, `pane.list`, `pane.surfaces`, `surface.list`,
   `surface.send_text`,
   `surface.add` for fixed-layout agent terminals, `surface.run` for commands
   in caller-created surfaces, `surface.close` for removing those surfaces,
   `surface.send_key`,
   `surface.read_text`, `surface.health`, and
   `notification.create`.

When the GUI is running, the CLI targets the bridge via the runtime
socket. `list-panes` / `list-panels`, `add-surface --cmd ...`,
text injection, key-level injection, `surface-health`, and terminal
`read-screen` now work against the running host.

## Delivery strategy (revised)

### Phase 1 — Env auto-wiring ✅ (shipped in 1295d12)

### Phase 2 — Make the bridge a full proxy (🚧 PARTIAL)

Bridge should route unknown methods to a local `Dispatcher` instance
seeded with live GTK state, OR to dedicated per-method `ControlCommand`
variants that interrogate the live state. The cleanest path:

- Maintain a `Arc<Mutex<ControlState>>` owned by the GTK app, kept in
  sync with live workspace/pane/surface state.
- Bridge falls through unknown methods to `Dispatcher::dispatch` on that
  shared state.
- Specific methods that need GTK side-effects (send_text, create_surface,
  notification.create) remain as `ControlCommand` variants.

The terminal introspection path is now bridged directly against live GTK state.
Remaining proxy work is for broader dispatcher parity.

**Shipped so far (in 6b8eb1a and follow-up bridge work):**

- `surface.send_text` and `notification.create` now pass `allow_name=true`
  to `parse_optional_workspace_target`, so peers can address each other
  by workspace name (`--workspace claude`) without juggling runtime
  UUIDs. This is what made phase 5 practical.
- `pane.list`, `pane.surfaces`, and `surface.list` now route on the live
  GTK bridge, so agents can discover peer panes/surfaces in a running
  GhostDeck window.
- `surface.send_key` now routes to the exact terminal surface when provided,
  so agents can send deterministic key-level control such as Ctrl-C.
- `surface.health` and `surface.read_text` now route on the live GTK bridge,
  so agents can inspect peer terminal health and visible screen text.
- `surface.add` adds up to three terminal surfaces to the caller's exact tab.
  GhostDeck keeps the caller on the left and owns the fixed vertical stack on the
  right; the caller cannot provide a direction or target.

### Phase 3 — `ghostdeck notify` + GUI toast/sidebar integration ✅
`ControlCommand::CreateNotification` wired through the bridge into
`mark_workspace_unread_with_message` + libadwaita toast.
CLI: `ghostdeck notify [--workspace <id|name>] [--subtitle <…>] [--body <…>] <title>`.

### Phase 4 — `ghostdeck claude-hook` / `opencode-hook` / `gemini-hook` ✅
Reads hook JSON from stdin, translates the agent-specific event vocabulary
into a `notify` (and, where useful, an inline `send`). Drop-in for
`~/.claude/settings.json` hooks blocks.

### Phase 5 — `ghostdeck agent-team` + `AGENTS.md` template ✅
`ghostdeck agent-team [--agents codex,claude[,opencode,gemini]] [--cwd <path>]
[--no-launch] [--dry-run]`:

- Calls `surface.add` for each peer (up to three) in the caller's tab and
  launches the agent through the terminal's configured shell.
- Peers use their surface IDs for direct messages.
- Writes `AGENTS.md` in the shared cwd documenting:
    - the peers table (agent → surface ID → launch command),
    - the `<agent-msg from="…" to="…" id="…" reply-to="…" ts="…">` envelope,
    - the exact `ghostdeck send` invocation for sending and replying,
    - the `ghostdeck notify` escalation path for human input,
    - the `GHOSTDECK_*` env contract every spawned terminal inherits,
    - editable Policies section (timeouts, size limits, destructive-action gating).

### Phase 6 — (deferred) `ghostdeck progress`, `ghostdeck log`, `ghostdeck markdown`
Nice polish, not blockers.

## Why phase 2 first

Without a real bridge, every subsequent feature ends up routing around
the same hole: the GUI owns the ground truth about surfaces/panes but
the CLI can't query it. Fixing this once, properly, makes phases 3–5
small.
