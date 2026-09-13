# Handoff: agent identification (registry-first)

Written 2026-09-13 at the end of the agent-redesign session, replacing the pre-redesign handoff. Read this before touching anything under "agent" in the codebase. `audit-findings.md` lists every audit finding with its status; the ones closed here are marked `fixed 2026-09-13 (agent redesign)`.

## 1. Where the repository stands

- Fork: `Xubbbb/opensessions` of the dormant `Ataraxy-Labs/opensessions`. `main` is the working branch; every push to `main` publishes a release (auto-version → tag → release build), so work lands on a branch and is fast-forwarded when ready. The redesign was done on branch `agent-redesign` (9 commits on top of v0.2.0-alpha.13's `2e82f85`).
- Verified state at the end of the session: 124 unit tests green (`cargo test --workspace --exclude opensessions-sidebar` + sidebar lib), tmux E2E 28/28 (`cargo build --workspace --bins && cargo test -p opensessions-sidebar --test tmux_e2e`), `cargo clippy` shows only the warnings upstream already had (one `collapsible_if` in `handle_connection`, seven in sidebar-core).
- Manually verified with a real `claude` (2.1.270) inside an isolated tmux server (`scripts/isolated-tmux.sh`-style, socket `ostest-verify`): an idle session appears as `✓ <name> / idle · claude-code` within a second of starting; a prompt shows the spinner within 500 ms and `●` when the turn ends in a pane the client is not looking at, `✓` when the pane is the client's active pane; `/exit` removes the row ≈10 s later and the transcript does not bring it back; the user's own Claude sessions in the default tmux server (same directory) never appear in the isolated server.

## 2. How agents are identified now

Three sources, in decreasing authority, feed `AgentTracker` (`packages/runtime-rs/src/tracker.rs`):

1. **Claude Code session registry** (`packages/runtime-rs/src/claude_registry.rs`). Claude Code ≥ 2.1.x writes `<config>/sessions/<pid>.json` for every interactive process and rewrites it on each status transition. Fields used: `sessionId` (= transcript stem = tracker `threadId`), `tmux` (`session:@window.%pane`, captured at startup), `status ∈ {busy, idle, waiting, shell}`, `waitingFor`, `statusUpdatedAt` (moves only on a real transition), `startedAt`, `name`/`nameSource` (`/rename`), `agent` (teammates), `procStart` (Linux `/proc/<pid>/stat` starttime), `kind` (only `interactive` counts). Config dirs: `~/.claude`, `$CLAUDE_CONFIG_DIR`, `~/.claude*` siblings holding `projects/` or `sessions/`.
   - `RegistryResolver` decides liveness (pid exists and start token matches) and ownership: a record is ours iff one of our panes' `pane_pid` is an ancestor of the record's pid (cached per pid incarnation; `/proc` on Linux, `ps -o ppid=` elsewhere). The pane id in the record is a hint only — every tmux server numbers panes from `%0`.
   - Server side (`apps/server-rs/src/lib.rs` `apply_claude_registry`): polled every 500 ms by `run_agent_watcher_loop`; live+ours records become `RegistryInput`s; records that are alive but not ours go into `foreign_claude_sessions`; sessions that just left the live set go into `claude_retired_sessions` (kept for `AGENT_WATCHER_RECENT_MS`). Both sets stop the transcript fallback from creating rows for those session ids.
2. **`/api/agent-event`** (`apply_agent_event`): unchanged shape plus `ts` normalisation (seconds → ms, future → now); `paneId` is an explicit binding; `unseen`/`liveness`/`detail` from clients are ignored.
3. **Transcript watchers** (`packages/runtime-rs/src/agent_watchers.rs`, `transcript_tail.rs`): Claude Code and Codex transcripts are parsed incrementally (`TailCache` keeps offset + parse state per file; a shrunk file is re-parsed). For Claude sessions with a registry record the transcript only produces a `TranscriptHint` (custom title, last prompt from Claude's `last-prompt` records, classification of the last entry) delivered through `enrich_claude_registry_rows`; for sessions without a record the old inference remains (`ClaudeTranscriptState::snapshot`: status from the last entry, tool-use silence ≥ 3 s → `waiting`, ≥ 15 s → `stale`, project dir from the entries' `cwd`). Amp files are parsed once per (len, mtime); OpenCode/Pi/Droid are unchanged.

### Tracker rules (`tracker.rs`)
- Key `agent:threadId` per session; a row relocates (keeping its unseen marker) when its bound pane moves to another session.
- Bindings: `Explicit(pane)` from registry/`paneId`; `Heuristic(pane)` only for non-registry rows, only when exactly one free agent-looking pane and one candidate row of that agent exist in the session (Amp titles bind by name first), never restamped while the pane exists, dropped when it disappears.
- Registry transitions: `busy` → running (refined to `tool-running` when the transcript's last assistant entry is an unanswered tool call, or `running (delegating)` when the assistant's own turn ended); `waiting` → waiting with `detail = waitingFor`; `busy/waiting → idle` → done + unseen (also when `statusUpdatedAt` advanced while status stayed idle: the transition happened between polls); `→ shell` → done (shell running) + unseen; `shell → idle` clears the detail only. Done is refined to interrupted/error by the transcript hint. First observation of an idle session is a seen `idle` row. A record with the same pid but a new session id (`/clear`) replaces the old row immediately.
- Seen rule: `sync_panes` marks seen every row bound to the current pane of an attached client (`tmux list-clients`), on every sync; the `/focus` hook does the same for its pane when `pane_active`. Nothing about focus is remembered (F013). Serialized `unseen` derives only from the unseen set (F042).
- Liveness: an explicit/heuristic pane that is missing from `list-panes -a` marks the row gone; a registry row is also gone when its record is absent/dead, and a surviving pane does **not** revive it (only a live record does). Rows of sessions that no longer exist are gone too (F043).
- Prune: gone for > `EXITED_PRUNE_MS` (10 s) → removed whatever the status; never-bound rows: terminal after 5 min seen / 30 min unseen, others after 30 min silence; rows bound to a live pane never.
- Server sync (`sync_agents`): one `list-panes -a` + one `list-clients` per snapshot (was `list-sessions` + one `list-panes` per session); `prune` and the Pi runtime registry prune run there too. The watcher loop reuses the tick's pane listing for the sync, lists sessions once per transcript tick, and broadcasts one snapshot per tick when anything changed.

### Contract changes
- `AgentEvent.detail?: string` (protocol 1, additive). Sidebar renders it as `label (detail) · agent`.
- Session resolution: bound pane > `projectDir` (deepest ancestor wins, same-dir tie settled by a live agent pane, F016/F118) > `tmuxSession`.
- Documented in `CONTRACTS.md`, `AGENTS.md`, `README.md`, `docs/reference/features-and-keybindings.md`, `docs/explanation/architecture.md`.

## 3. Product decisions taken (with the maintainer)

Registry over hooks (zero install, exact UI state incl. dialogs and interrupts; hooks stay possible as a push layer — a hook payload's `session_id` + `$TMUX_PANE` maps onto `RegistryInput`/`apply_event` with no remap). Claude Code and Codex in scope; live idle Claude sessions are rows; in-process subagents/workflows never are; teammate processes with their own pane are ordinary rows named after their agent; "delegating" label when busy after the own turn ended; seen = active pane of an attached client; gone rows removed after 10 s whatever the status; `ts` seconds auto-converted; header spinner count unified with the running predicate over visible sessions (F067); `d` on the attached session shows a footer notice (F102); `@opensessions-direct-bindings` opt-out and uninstall restores tmux layout keys (F092); `watch_plan.rs`/`lifecycle_operation.rs` deleted, `portless.rs` kept because `server_state` uses it (F105).

## 4. Open items

- F046 (Droid per-line ids) is out of scope and still open; Pi/Droid/OpenCode scanners still re-read whole files (small).
- Registry format is Claude Code-internal. If a future version changes it, `parse_registry_record` returns `None` (unknown status reads as idle, missing `sessionId` skips) and Claude sessions fall back to transcript inference; re-check the vocabulary in the binary (`status:` / `waitingFor` strings) when upgrading.
- macOS: `ProcessInspector` falls back to `ps -o ppid=` per pid (one spawn per record per tick) and cannot verify `procStart`; if that shows up in profiles, batch it with one `ps -axo pid=,ppid=` per tick.
- The registry poll costs two tmux spawns per 500 ms (`list-panes -a`, `list-clients`). If it ever matters, throttle the pane listing to the tmux poll cadence and keep only the file reads at 500 ms.
- `isolated-tmux.sh --stop` then an immediate restart on the same socket races the old opensessions server's shutdown cleanup (it notices the tmux server change ≈4 s later and hides sidebar panes on the new server); wait ~6 s between stop and start.
- Possible next steps: an opt-in Claude Code hooks layer for sub-poll latency; a "session not ours" indicator for Claude sessions running outside tmux; Codex pane binding beyond the alias heuristic if Codex ever publishes a registry.

## 5. Working rules that matter here

- Run `. ~/.cargo/env` first. Build **all bins** before the E2E suite: `cargo build --workspace --bins && cargo test -p opensessions-sidebar --test tmux_e2e`.
- Never run `tmux` without `-L <private socket>` in experiments; the user's live tmux and the live server on `127.0.0.1:24500` are off limits. To try a build: `cargo build --release && scripts/isolated-tmux.sh`; a headless variant is `tmux -L ostest -f <generated conf> new-session -d -s dev -x 160 -y 40`, `tmux -L ostest run-shell "sh integrations/tmux-plugin/scripts/focus.sh"`, and a python `pty.fork` client with a real window size (a size-less `script` pty makes tmux drop the window).
- Registry records of your own Claude sessions are readable test data; the `.key` files next to them are secrets — never read them.
- Debug logging is opt-in: `OPENSESSIONS_DEBUG_LOG=<path>`; the registry path logs `claude-registry applied changes`, the watcher `watcher-snapshot skipped foreign or exited claude session`.
- Keep `AGENTS.md` rules: registry/watcher/API events author status, panes only bind; sync stays at two tmux calls; transcripts are read incrementally; `SERVER_VERSION` from `package.json`.

## 6. Init prompt for the next session

```
You are continuing maintenance of the opensessions fork (Xubbbb/opensessions), a Rust tmux sidebar. Start in plan mode; do not write code yet.

Read docs/handoff/agent-redesign-handoff.md (registry-first agent identification, implemented 2026-09-13), then docs/handoff/audit-findings.md for what is still open, AGENTS.md and CONTRACTS.md. The agent pipeline lives in packages/runtime-rs/src/{claude_registry,tracker,transcript_tail,agent_watchers,project_dir_session}.rs and apps/server-rs/src/lib.rs (apply_claude_registry, sync_agents, run_agent_watcher_loop).

Ask me what to work on next; candidates are listed in section 4 of the handoff.
```
