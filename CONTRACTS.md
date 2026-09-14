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

- Primary source: the session registry Claude Code (2.1.x and later) keeps at `<config>/sessions/<pid>.json` for every interactive process, polled every 500 ms. A record names the session id, the tmux pane the process started in, the `/rename` name, and a status that mirrors Claude's own status line: `busy` (turn in flight, or background subagents/workflows/teammates still running), `idle`, `waiting` (a permission prompt, question, plan approval or elicitation is open; the reason is shown as the row's `detail`), or `shell` (at the prompt with a background shell command running, shown as `done (shell running)`).
- A record belongs to this server only when one of its panes' processes is an ancestor of the record's pid; pane ids alone are never trusted because every tmux server numbers panes from `%0`. Records of sessions outside this tmux server are ignored, and their transcripts are never attributed by directory either.
- Transcripts (`<config>/projects/<encoded-path>/<session id>.jsonl`) are read incrementally and only enrich registry rows: custom title and last prompt for display, `tool-running` while the last assistant entry is an unanswered tool call, `delegating` while the registry is busy after the assistant's own turn ended, and `interrupted`/`error` for a turn that ended that way. A transcript without a live registry record (an exited or `/clear`ed session, a `claude -p` run, a Claude Code older than 2.1) never becomes a row.
- Config directories: `~/.claude`, `$CLAUDE_CONFIG_DIR`, and every `~/.claude*` sibling holding Claude state (multi-account setups).

### Codex

- Reads transcript JSONL files in `~/.codex/sessions/**/*.jsonl` or `$CODEX_HOME/sessions/**/*.jsonl`, incrementally.
- Reads `$CODEX_HOME/session_index.jsonl` for recent thread titles when available; otherwise the title is the user's request after the last `## My request for Codex:` marker (injected IDE context is never a title).
- Resolves sessions from transcript `turn_context.cwd`; rollouts of Codex's own subagents are ignored.

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

The server resolves the target session, in this order:

| Input | Meaning |
| --- | --- |
| a bound pane | Once a row is bound to a pane (`paneId`, or a Claude Code registry record), it lives in that pane's session and follows the pane if it is moved |
| `projectDir` | Project/worktree directory: an exact session-dir match wins, then sessions rooted below it together with the sessions of the deepest ancestor directory. A tie between several sessions is settled by the one session that shows a live pane running that agent; otherwise the event is not attributed |
| `tmuxSession` | Exact tmux session name |

If nothing resolves to a session known to this server, the event is ignored with `202 Accepted` (not an error), so an integration can broadcast one event to every opensessions server on the machine and let each decide ownership. Malformed events get `400 Bad Request`.

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
  detail?: string;
}
```

External `/api/agent-event` callers send the same shape except they use `tmuxSession` or `projectDir` for session resolution. The serialized server state always contains the resolved `session` field. `unseen`, `liveness` and `detail` are derived by the server and ignored when a client sends them.

| Field | Type | Required for HTTP | Notes |
| --- | --- | --- | --- |
| `agent` | `string` | yes | Stable agent identifier such as `amp`, `claude-code`, `codex`, `opencode`, `pi`, `droid`, or your integration name |
| `status` | `AgentStatus` | yes | Current agent state |
| `tmuxSession` | `string` | one of `tmuxSession` / `projectDir` | Exact tmux session name |
| `projectDir` | `string` | one of `tmuxSession` / `projectDir` | Project directory used for session resolution |
| `ts` | `number` | no | Millisecond timestamp; server time is used when omitted. A value below 10^12 is treated as seconds and scaled; a value more than an hour in the future is replaced by server time |
| `threadId` | `string` | no | Stable instance key for multiple threads in one session |
| `threadName` | `string` | no | Human-readable label shown in the detail panel |
| `lastUserPrompt` | `string` | no | Latest user prompt/intent, shown in agent detail UI |
| `paneId` | `string` | no | tmux pane the agent runs in. Binds the row to that pane: focus/kill go there, the row stays alive exactly as long as the pane exists, and it is reaped about 10 seconds after the pane disappears |

### Tracker Semantics

- Instances are keyed by `agent:threadId` when `threadId` exists, otherwise by `agent`. For Claude Code the `threadId` is the session id.
- A session can have multiple active agent instances. A live Claude Code session is a row even while idle (`✓ idle`), so the sidebar is a map of agent panes: `Enter` focuses the pane, `x` kills it.
- Pane bindings are explicit (a registry record or `paneId`) or, for agents without either, heuristic: an agent-looking pane (process name or title) is bound to a pane-less row of that agent only when the match is unambiguous — one free pane and one candidate in the session — and a binding is never moved while its pane still exists. Panes bind rows; they never author status.
- Unseen state is tracked per instance, then derived to the session level. A row becomes unseen when it enters a terminal state (`done`, `error`, `interrupted`, `stale`). Non-terminal updates clear it.
- Seen rule: a terminal row is seen as soon as its pane is the active pane of an attached tmux client — computed from tmux on every sync, never remembered — or when the user opens it with `Enter`, or when the session is marked seen. Finishing in a pane that is not on screen keeps the marker until the user looks.
- Pruning: a row whose pane or process is gone is removed about 10 seconds later whatever its status. Rows that never had a pane keep the timeouts: terminal rows after 5 minutes once seen or 30 minutes while unseen, other rows after 30 minutes of silence. Rows bound to a live pane are never pruned. The websocket `dismiss-agent` command removes one immediately; a Pi runtime `delete` also removes that Pi thread.

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
