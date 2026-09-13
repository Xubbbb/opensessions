# opensessions plugin for Amp

Real-time agent status for Amp threads, driven by the Amp plugin API instead
of by polling Amp's cloud API.

## Why

The cloud API approach (built-in `AmpAgentWatcher`) works, but:

- It polls `/api/threads` every 10s for discovery.
- Local threads (`usesDtw: false`) require detail fetches on every version bump.
- Amp's DTW WebSocket protocol is "in flux" per the Amp team and may change.

When this plugin is installed, opensessions uses its events as the source of
truth and automatically suppresses cloud API calls for any thread the plugin
has reported on. If the plugin goes silent for 5 minutes, the watcher falls
back to polling.

## Install

Copy (or symlink) `opensessions.ts` into `~/.config/amp/plugins/`:

```sh
mkdir -p ~/.config/amp/plugins
cp integrations/amp/opensessions.ts ~/.config/amp/plugins/opensessions.ts
```

Restart Amp. A recent Amp build that exposes `ctx.thread.id` on `session.start`
is required.

## Environment

| Variable | Default | Purpose |
| --- | --- | --- |
| `OPENSESSIONS_URL` | `http://127.0.0.1:7391` (only correct outside tmux; a plugin-launched server listens on `22000 + hash(tmux socket)` — print it with `integrations/tmux-plugin/scripts/port.sh` and export it as `OPENSESSIONS_URL`) | opensessions server base URL |

## Event mapping

| Amp event | opensessions status |
| --- | --- |
| `session.start` | `idle` |
| `agent.start` | `running` |
| `agent.end` (`status=done`) | `done` |
| `agent.end` (`status=error`) | `error` |
| `agent.end` (other) | `interrupted` |
| `tool.call` | `tool-running` |
| `tool.result` (`status=error`) | `error` |
| `tool.result` (`status=cancelled`) | `interrupted` |
| `tool.result` (success) | `running` (agent streaming the reply) |

## Session resolution

Every event payload carries:

1. `tmuxSession` — from `tmux display-message -p '#S'`, if Amp is running
   inside a tmux pane.
2. `projectDir` — from `process.cwd()`.

The server prefers a known tmux session name; otherwise it resolves
`projectDir` against its session→directory map.
