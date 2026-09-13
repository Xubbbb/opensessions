# opensessions — AI Agent Instructions

You are working on **opensessions**, an agent-agnostic terminal session manager and parallel-agent control plane. This repository is the maintained fork at `Xubbbb/opensessions`; upstream `Ataraxy-Labs/opensessions` is dormant.

## North Star

**opensessions is becoming the parallel-agent operating system.** It should make many CLI agents, panes, tmux sessions, and git worktrees feel like one coherent control plane instead of a pile of terminals.

The product direction is:

- **Observe everything**: track tmux sessions, windows, panes, focused pane, layouts, worktrees, git state, agent status, approval state, unread/done state, and session activity as first-class runtime state.
- **One worktree === one session by default** for opensessions-created work: a new isolated task should get a worktree-backed tmux session with predictable naming, pane layout, agent launch, and cleanup.
- **Existing work stays valid**: users and agents must also be able to launch agents inside existing worktrees, existing sessions, and existing panes when that is the right workflow.
- **Server as control plane**: the server owns durable state, synchronization, launch jobs, tmux/worktree mappings, and agent-facing APIs. The sidebar is one UI over that control plane, not the control plane itself.
- **CLI/API for agents**: opensessions should expose commands and eventually an API/MCP surface that lets agents create sessions, launch sibling agents, inspect panes, read useful context, send prompts, wait for state changes, and report handoffs safely.
- **Tmux-native, not tmux-hostile**: tmux remains the supported substrate. opensessions should repair and manage only what it owns, preserve user layouts by default, and reserve fully managed layouts for explicit modes.
- **Review and merge are part of the OS**: parallel execution is only useful when the user can compare outputs, detect conflicts, review diffs, merge winning work, and clean up sessions/worktrees without ceremony.

## Project Structure

```
opensessions/
├── apps/
│   ├── server-rs/          # opensessions-server — Rust WebSocket/HTTP server and control plane
│   ├── tui-rs/             # opensessions-sidebar — Rust ratatui sidebar client
│   └── tui/scripts/        # tmux sidebar launcher and sessionizer shell scripts
├── integrations/
│   ├── tmux-plugin/        # tmux-facing scripts and host integration glue
│   ├── amp/                # Amp helper integration
│   └── pi-extension/       # Pi runtime helper integration
├── packages/
│   ├── runtime-rs/         # Shared Rust runtime: config, protocol, tracker, tmux provider, watchers
│   └── sidebar-core-rs/    # Sidebar app state, input handling, rendering, layout, hit testing
├── CONTRACTS.md            # Supported agent event and runtime integration contracts
├── opensessions.tmux       # Root TPM entrypoint
├── Cargo.toml              # Rust workspace root
└── package.json            # Single version source: bumped by CI, read by the TPM download helper and apps/server-rs/build.rs
```

## Key Architecture Decisions

1. **Rust-first runtime**: the supported server and TUI are Rust crates in `apps/*-rs` and `packages/*-rs`.
2. **Ratatui sidebar**: rendering is immediate-mode Ratatui/Crossterm. Shared UI logic lives in `packages/sidebar-core-rs` so renderer, input, tests, and E2E flows use one source of truth.
3. **Built-in agent watchers**: the Rust server scans Amp, Claude Code, Codex, OpenCode, Pi, and Droid state directly and converts it into `AgentEvent`s. For Claude Code the source of truth is its session registry (`<config>/sessions/<pid>.json`: session id, pane, status, name — see `packages/runtime-rs/src/claude_registry.rs`); transcripts only enrich registry rows and are the fallback for sessions without a record.
4. **External agent events via HTTP**: third-party agents should POST to `/api/agent-event` or use the metadata endpoints. TypeScript plugin loading is not a supported runtime path right now.
5. **Tmux is the supported mux**: abstractions remain mux-shaped, but tmux is the only documented supported provider. Older zellij helper code is not part of the support promise.
6. **Release binaries, not local builds**: TPM users get prebuilt `opensessions-sidebar`, `opensessions-server`, and `lazydiff` binaries in `bin/`. `cargo build --release` is for development or unsupported platforms.
7. **Version flow**: every push to `main` runs `auto-version.yml`, which bumps `package.json`, tags `vX`, and dispatches `release.yml` for that tag (a `GITHUB_TOKEN` tag push cannot trigger it on its own). `SERVER_VERSION` is derived from `package.json` by `apps/server-rs/build.rs`; never hardcode it.
8. **tmux hooks live in array slot `90210`** (`tmux_scripting::HOOK_SLOT`) so other plugins' hooks on the same events survive; `uninstall.sh` mirrors the constant. The sidebar pane command is a fixed `sh -c '...'` string with per-pane values passed via `split-window -e`, so it works under fish and other non-POSIX default shells.
9. **Debug logging is opt-in**: `opensessions_runtime::debug_log` writes only when `OPENSESSIONS_DEBUG_LOG` is set, capped at 16 MB. Do not reintroduce an always-on default path.

## Contracts

### AgentEvent

```typescript
{
  agent: string,
  session: string,
  status: "idle" | "running" | "tool-running" | "done" | "error" | "waiting" | "interrupted" | "stale",
  ts: number,
  threadId?: string,
  threadName?: string,
  lastUserPrompt?: string,
  unseen?: boolean,
  paneId?: string,
  liveness?: "alive" | "exited" | "unknown",
  detail?: string
}
```

External tools can send events with:

```bash
curl -X POST http://127.0.0.1:<port>/api/agent-event \
  -H 'content-type: application/json' \
  -d '{"agent":"my-agent","status":"running","tmuxSession":"work","threadId":"task-1"}'
```

The server can resolve the session from `tmuxSession` or `projectDir`.

### MuxProvider

The Rust trait lives in `packages/runtime-rs/src/mux.rs`. Keep methods synchronous because the tmux provider is command-driven and the server uses it as a simple control surface.

## Stack

- **Runtime**: Rust 2024 edition
- **TUI**: Ratatui 0.30 + Crossterm 0.29
- **Async/networking**: Tokio + tokio-websockets
- **Tests**: `cargo test`, with tmux E2E coverage in `apps/tui-rs/tests/tmux_e2e.rs`
- **Release**: GitHub Actions builds `opensessions-sidebar`, `opensessions-server`, and bundled `lazydiff` for release artifacts

## Development Guidelines

- **TDD**: Red-green-refactor, vertical slices, one test at a time. Tests verify behavior through public interfaces.
- **Sync tmux calls**: keep mux provider methods synchronous unless the architecture changes deliberately.
- **Preserve optimizations**: batched tmux calls, git cache with HEAD watchers, lightweight focus-only broadcasts, fixed-width sidebar repair, and per-client focus state.
- **Sidebar resize work**: before changing sidebar spawning, width sync, tmux resize handling, or `sidebar-coordinator`, read `docs/explanation/sidebar-behavior.md` and preserve those invariants unless you update the doc in the same change.
- **Built-in watchers in Rust runtime/server**: Amp, Claude Code, Codex, OpenCode, Pi, and Droid watcher parsing lives in `packages/runtime-rs/src/agent_watchers.rs` and server scanning lives in `apps/server-rs/src/lib.rs`.
- **Do not reintroduce pane-derived agent status**: panes can bind rows (explicitly through a registry record or `paneId`, heuristically only on an unambiguous 1:1 match) and drive focus/kill/seen, but the registry, watcher and API events are the source of agent status. Never guess Claude Code state from a transcript when a registry record exists.
- **Agent sync runs in `snapshot_json`**: `ReadOnlyMuxStateSource::sync_agents` lists every pane and every client's current pane once, reconciles liveness, bindings and the seen rule (`AgentTracker::sync_panes`), then reaps gone or abandoned rows (`AgentTracker::prune`; TTL constants in `tracker.rs`). Keep it on every snapshot and keep it to those two tmux calls.
- **Transcripts are read incrementally**: `transcript_tail::TailCache` parses only appended lines; a parser that needs the whole file again must reset its state rather than re-read from disk.

## Common Commands

```bash
cargo test --workspace                         # Run Rust tests
cargo test -p opensessions-sidebar-core        # Focused sidebar core tests
cargo test -p opensessions-sidebar --test tmux_e2e -- --nocapture
cargo build --release                          # Build local dev binaries
cargo run -p opensessions-server               # Start server directly
cargo run -p opensessions-sidebar              # Start sidebar directly
cargo clippy --workspace --all-targets         # Lint
```

The tmux E2E suite starts private tmux servers (`tmux -L opensessions-e2e-*`) and needs `tmux`, `git`, and `python3`; it never touches the user's own tmux server.

## Adding A New Built-In Mux Provider

1. Implement the Rust `MuxProvider` trait in `packages/runtime-rs/src/mux.rs` or a new Rust module/package.
2. Register it in the server bootstrap if it should be built in.
3. Add focused command-runner tests and E2E coverage at the highest useful layer.
4. Document whether it is supported or experimental. Do not document a provider as supported until install/setup and sidebar behavior are stable.

## Adding Agent Support

1. Prefer an external HTTP integration first: POST `/api/agent-event` with stable `agent`, `threadId`, `projectDir` or `tmuxSession`, `status`, and `paneId` whenever the integration runs inside tmux (`$TMUX_PANE`), so the row is bound to its pane explicitly.
2. For built-in support, add parser/scanner logic in Rust and tests in `packages/runtime-rs` or `apps/server-rs`.
3. Preserve per-thread unseen semantics and pane focus clearing.
4. See `CONTRACTS.md` for integration examples.
