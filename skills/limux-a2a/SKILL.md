---
name: limux-a2a
description: Use inside Limux to run a human-visible process, launch a peer agent, inspect or control a terminal surface, or notify the human.
---

# Limux

Use inherited `LIMUX_*` identity. Do not discover or guess targets unless an ID is missing.

## Visible process

```bash
created="$(limux --json add-surface --cwd apps/web --cmd 'npm run dev')"
surface="$(printf '%s\n' "$created" | jq -r '.surface_id')"
```

Relative `--cwd` paths use your current directory. `--cmd` is sent verbatim to the configured shell. Limux stacks at most three added surfaces to the right in the caller's tab.

Run another command in an added surface after its shell prompt returns:

```bash
limux run --surface "$surface" --cmd 'npm test'
```

For durable output:

```bash
mkdir -p apps/web/.limux/logs
limux --json add-surface --cwd apps/web \
  --cmd 'npm run dev 2>&1 | tee -a .limux/logs/dev.log'
```

## Control

```bash
limux read-screen --surface "$surface"
limux send-key --surface "$surface" '<Ctrl>c'
limux notify --body "short status" "Input needed"
limux close-surface --surface "$surface"
```

`close-surface` closes a surface created by this agent in its current tab and shuts down its terminal session. It cannot close the caller surface.

## Peer agent

```bash
peer="$(limux --json new-pane --command 'codex "Task prompt"')"
peer_surface="$(printf '%s\n' "$peer" | jq -r '.surface_id')"
limux send --surface "$peer_surface" "message"
```

Capture returned surface IDs immediately. Use `read-screen` for observation and `send` only when the peer needs a message.
