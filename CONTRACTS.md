# Contracts And Supported Integration Interfaces

This document describes the integration surface that exists in the current Rust runtime. The supported extension path today is HTTP: external tools can push agent state with `/api/agent-event` and session metadata with the metadata endpoints.

TypeScript plugin loading, `PluginAPI`, and package-based mux/agent plugins are not supported by the Rust server right now. The `plugins` config field is still parsed for compatibility, but it is not executed.

For end-user setup, start with [README.md](./README.md).

## Built-In Watchers

The Rust server scans these agent data sources directly:

### Amp

- Reads `~/.local/share/amp/threads/T-*.json`.
- Resolves project directories from `env.initial.trees[0].uri`.

### Claude Code

- Reads JSONL transcripts in `~/.claude/projects/<encoded-path>/*.jsonl`, plus `$CLAUDE_CONFIG_DIR/projects/` and every `~/.claude*/projects/` sibling directory (multi-account setups).
- Decodes project directories from folder names such as `-Users-me-project`.
- Treats recent tool-use silence as `waiting` and long silence as `stale`.

### Codex

- Reads transcript JSONL files in `~/.codex/sessions/**/*.jsonl` or `$CODEX_HOME/sessions/**/*.jsonl`.
- Reads `$CODEX_HOME/session_index.jsonl` for recent thread titles when available.
- Resolves sessions from transcript `turn_context.cwd`.

### OpenCode

- Polls `~/.local/share/opencode/opencode.db` or `$OPENCODE_DB_PATH`.
- Resolves sessions from the OpenCode session row's `directory` field.

### Pi and Droid

- The Rust server includes scanner/parser support for Pi and Droid runtime/session state.
- Pi integrations can also use the Pi runtime API exposed by the server.

## Agent Event HTTP API

External agents should POST JSON to:

```text
POST /api/agent-event
```

Example:

```bash
curl -sS -X POST "$(sh ~/.tmux/plugins/opensessions/integrations/tmux-plugin/scripts/port.sh)/api/agent-event" \
  -H 'content-type: application/json' \
  -d '{
    "agent": "my-agent",
    "status": "running",
    "tmuxSession": "work",
    "threadId": "task-123",
    "threadName": "Implement search",
    "lastUserPrompt": "Add search to the sidebar",
    "paneId": "%7"
  }'
```

### Session Resolution

The server resolves the target session from either:

| Input field | Meaning |
| --- | --- |
| `tmuxSession` | Exact tmux session name |
| `projectDir` | Project/worktree directory; exact session-dir match wins, then parent/child prefix matching |

If neither field resolves to a session known to this server, the event is ignored with `202 Accepted` (not an error), so an integration can broadcast one event to every opensessions server on the machine and let each decide ownership. Malformed events get `400 Bad Request`.

## Agent Model

### `AgentStatus`

```ts
type AgentStatus =
  | "idle"
  | "running"
  | "tool-running"
  | "done"
  | "error"
  | "waiting"
  | "interrupted"
  | "stale";
```

Terminal states are `done`, `error`, `interrupted`, and `stale`; the tracker marks a session unseen when an instance enters any of them, so a run that went silent mid tool-call gets the same attention marker as a finished one. `tool-running` is a running subtype used when the agent is actively using tools. `stale` means the last known running/waiting state has aged past the runtime threshold (15 s of silence for the built-in transcript watchers).

### `AgentEvent`

```ts
interface AgentEvent {
  agent: string;
  session: string;
  status: AgentStatus;
  ts: number;
  threadId?: string;
  threadName?: string;
  lastUserPrompt?: string;
  unseen?: boolean;
  paneId?: string;
  liveness?: "alive" | "exited" | "unknown";
}
```

External `/api/agent-event` callers send the same shape except they use `tmuxSession` or `projectDir` for session resolution. The serialized server state always contains the resolved `session` field.

| Field | Type | Required for HTTP | Notes |
| --- | --- | --- | --- |
| `agent` | `string` | yes | Stable agent identifier such as `amp`, `claude-code`, `codex`, `opencode`, `pi`, `droid`, or your integration name |
| `status` | `AgentStatus` | yes | Current agent state |
| `tmuxSession` | `string` | one of `tmuxSession` / `projectDir` | Exact tmux session name |
| `projectDir` | `string` | one of `tmuxSession` / `projectDir` | Project directory used for session resolution |
| `ts` | `number` | no | Millisecond timestamp; server time is used when omitted |
| `threadId` | `string` | no | Stable instance key for multiple threads in one session |
| `threadName` | `string` | no | Human-readable label shown in the detail panel |
| `lastUserPrompt` | `string` | no | Latest user prompt/intent, shown in agent detail UI |
| `paneId` | `string` | no | tmux pane id used for focus/kill routing when available |

### Tracker Semantics

- Instances are keyed by `agent:threadId` when `threadId` exists, otherwise by `agent`.
- A session can have multiple active agent instances.
- Unseen state is tracked per instance, then derived to the session level.
- Non-terminal updates clear unseen state for that instance.
- Terminal instances become seen when the user focuses the associated pane/session according to the server's tmux focus tracking.
- Terminal instances are pruned automatically: about 10 seconds after their pane is observed gone, otherwise after 5 minutes once seen or 30 minutes while unseen. Running instances with no live pane are dropped after 30 minutes of silence. The websocket `dismiss-agent` command removes one immediately; a Pi runtime `delete` also removes that Pi thread.

## Metadata HTTP API

Scripts can also attach status, progress, and logs to sessions:

```text
POST /set-status
POST /set-progress
POST /log
POST /clear-log
POST /notify
```

See [docs/reference/programmatic-api.md](./docs/reference/programmatic-api.md) for examples.

## Rust Mux Contract

The supported mux implementation is tmux. The abstraction still lives in Rust so future providers can be added deliberately.

The trait is defined in `packages/runtime-rs/src/mux.rs`:

```rust
pub trait MuxProvider: Send + Sync {
    fn name(&self) -> &str;
    fn list_sessions(&self) -> Vec<MuxSessionInfo>;
    fn switch_session(&self, name: &str, client_tty: Option<&str>);
    fn get_current_session(&self) -> Option<String>;
    fn get_session_dir(&self, name: &str) -> String;
    fn get_pane_count(&self, name: &str) -> u32;
    fn get_client_tty(&self) -> String;
    fn create_session(&self, name: Option<&str>, dir: Option<&str>);
    fn kill_session(&self, name: &str);
    fn setup_hooks(&self, server_host: &str, server_port: u16);
    fn cleanup_hooks(&self);
    // optional capability methods omitted here; see source for full trait
}
```

Provider methods are synchronous because tmux operations are command-driven and the server treats the provider as a simple control surface.

## Built-In Runtime Behaviors To Know About

- The server computes `ServerState` from tmux sessions, git/cache state, metadata, ports, and tracked agent events.
- Session ordering is persisted separately from tmux ordering.
- Toggling the sidebar off kills its panes; only `prefix o → e` uses the `_os_stash` session, briefly, while re-laying out a window.
- tmux is the only supported built-in mux today.
- The sidebar and helper scripts resolve the server port from the tmux socket via `OPENSESSIONS_SERVER_KEY`, defaulting to derived per-socket ports.
- TPM installs use prebuilt binaries in `bin/`; local builds use `target/release` or `target/debug` as fallback paths.

## Where To Start

- Integrate an agent: POST `/api/agent-event` with stable `agent`, `threadId`, and `projectDir` or `tmuxSession`.
- Push build/deploy metadata: use [docs/reference/programmatic-api.md](./docs/reference/programmatic-api.md).
- Understand runtime behavior: read [docs/explanation/architecture.md](./docs/explanation/architecture.md).
