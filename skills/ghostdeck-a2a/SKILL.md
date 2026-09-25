---
name: ghostdeck-a2a
description: Use inside GhostDeck to run a human-visible process, launch a peer agent, inspect or control a terminal surface, or notify the human.
---

# GhostDeck

Use inherited `GHOSTDECK_*` identity. Do not discover or guess targets unless an ID is missing.

## Visible process

```bash
created="$(ghostdeck --json add-surface --cwd apps/web --cmd 'npm run dev')"
surface="$(printf '%s\n' "$created" | jq -r '.surface_id')"
```

Relative `--cwd` paths use your current directory. `--cmd` is sent verbatim to the configured shell. GhostDeck stacks at most three added surfaces to the right in the caller's tab.

Run another command in an added surface after its shell prompt returns:

```bash
ghostdeck run --surface "$surface" --cmd 'npm test'
```

For durable output:

```bash
mkdir -p apps/web/.ghostdeck/logs
ghostdeck --json add-surface --cwd apps/web \
  --cmd 'npm run dev 2>&1 | tee -a .ghostdeck/logs/dev.log'
```

## Control

```bash
ghostdeck read-screen --surface "$surface"
ghostdeck send-key --surface "$surface" '<Ctrl>c'
ghostdeck notify --body "short status" "Input needed"
ghostdeck close-surface --surface "$surface"
```

`close-surface` closes a surface created by this agent in its current tab and shuts down its terminal session. It cannot close the caller surface.

## Peer agent

```bash
peer="$(ghostdeck --json add-surface --cmd 'codex "Task prompt"')"
peer_surface="$(printf '%s\n' "$peer" | jq -r '.surface_id')"
ghostdeck send --surface "$peer_surface" "message"
```

Capture returned surface IDs immediately. Use `read-screen` for observation and `send` only when the peer needs a message.
