---
name: limux-a2a
description: Use inside Limux to run a human-visible process, launch a peer agent, inspect or control a terminal surface, or notify the human.
---

# Limux

Use inherited `LIMUX_*` identity. Do not discover or guess targets unless an ID is missing.

## Visible process

```bash
created="$(limux --json add-surface -- npm run dev)"
surface="$(printf '%s\n' "$created" | jq -r '.surface_id')"
```

`add-surface` accepts no target or direction. Limux stacks at most three added surfaces to the right in the caller's tab.

For durable output:

```bash
mkdir -p .limux/logs
limux --json add-surface -- bash -lc \
  'set -o pipefail; npm run dev 2>&1 | tee -a .limux/logs/dev.log'
```

## Control

```bash
limux read-screen --surface "$surface"
limux send-key --surface "$surface" '<Ctrl>c'
limux notify --body "short status" "Input needed"
```

## Peer agent

```bash
peer="$(limux --json new-pane --command 'codex "Task prompt"')"
peer_surface="$(printf '%s\n' "$peer" | jq -r '.surface_id')"
limux send --surface "$peer_surface" "message"
```

Capture returned surface IDs immediately. Use `read-screen` for observation and `send` only when the peer needs a message.
