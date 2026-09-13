# Handoff: agent identification redesign

Written 2026-09-13 at the end of the fork's first maintenance session. Read this before touching anything under "agent" in the codebase. The companion file `audit-findings.md` lists every audit finding with its status.

## 1. Where the repository stands

- Fork: `Xubbbb/opensessions` of the dormant `Ataraxy-Labs/opensessions` (last upstream release v0.2.0-alpha.12). The user still runs the upstream TPM install locally and will switch once the fork ships a release.
- The working tree carries two large **uncommitted** passes (nothing has been committed yet — the user decides how to split commits):
  1. the initial bug-fix pass (release retargeting, `build.rs` version, fish-shell spawn via `split-window -e`, hook array slot 90210, opt-in capped debug log, agent pruning, `/pane-died`, `CLAUDE_CONFIG_DIR`, lsof, uninstall, graceful SIGTERM, tmux utempter SIGCHLD nudge);
  2. 36 fixes from the audit (see `audit-findings.md`, status `fixed`), including: exact tmux session targets (`=name:`), width-repair loop guard, session ids in hook bodies, HTTP body cap/timeouts, server exits when its tmux server dies, config `theme`/`sidebarPosition`/`sessionFilter` applied and persisted, session order persisted, proxy-proof loopback curls, `focus.sh` no longer toggling every sidebar off, theme picker/scroll fixes, refocus to the previously active pane.
- Verified state: `cargo test --workspace` green (74 unit tests); tmux E2E 23/23 in most runs, with residual flakiness in the agent seen-marking tests that the redesign will make obsolete. `cargo clippy` shows only the 12 warnings upstream already had.
- `Xubbbb/lazydiff` was re-forked with all tags, so `release.yml`'s `LAZYDIFF_REF: v0.1.0-alpha.18` resolves. `Xubbbb/opensessions` has no tags; that is fine (do **not** push the 53 upstream tags — each tag push would trigger a release build).
- No `LICENSE` file exists upstream or here despite the MIT badge; the user has been told.

## 2. How agents are identified today (the thing being redesigned)

Everything is inference; there is no ground-truth link between a tmux pane and an agent conversation.

1. **Discovery.** Two inputs feed `AgentTracker` (`packages/runtime-rs/src/tracker.rs`):
   - Built-in watchers: `apps/server-rs/src/lib.rs` `scan_amp_threads` / `scan_claude_code_projects` / `scan_codex_sessions` / `scan_opencode_sessions` / `scan_pi_sessions` / `scan_droid_sessions`, driven by `run_agent_watcher_loop` every 2 s. Each re-reads every transcript modified in the last 5 min **in full** and parses it (`packages/runtime-rs/src/agent_watchers.rs`, `agent_parsers.rs`) into an `AgentWatcherSnapshot { agent, thread_id, thread_name, last_user_prompt, project_dir, status, ts = mtime }`. A fingerprint (status, thread_name, prompt, project_dir) suppresses re-applying unchanged snapshots. A 4 MB Claude Code transcript costs ~75 ms CPU and ~145 MB of transient allocation per tick on the user's machine (audit F015/F020/F010).
   - HTTP `POST /api/agent-event` (`apply_agent_event`), used by `integrations/amp/opensessions.ts`, `integrations/pi-extension/opensessions-runtime.ts` and anything custom; carries `tmuxSession` or `projectDir`, optional `paneId`, `threadId`, `status`, `ts`.
2. **Identity.** Instance key = `agent:threadId` (`tracker::instance_key`), or `agent` alone. Claude Code's `threadId` is the transcript file stem (the session UUID). Status for Claude Code is guessed from the transcript tail (`agent_parsers::determine_claude_code_status`) plus timing: tool-call silence > 3 s → `waiting`, > 15 s → `stale`.
3. **Session attribution.** Claude Code's project dir is decoded from the folder name `~/.claude/projects/<dashed-path>` (`decode_claude_project_dir`, lossy for paths containing `-`), then matched to tmux sessions by directory (`project_dir_session::resolve_session_for_project_dir`: exact match, else parent/child prefix; ties → nobody). Session directory = tmux `session_path` or the active pane's cwd (`TmuxProvider::list_sessions`).
4. **Pane binding.** On every snapshot `sync_agent_pane_presence` lists each session's panes; `tmux_provider::agent_from_pane` calls a pane an agent pane when `pane_current_command`/title contains an alias token (`AGENT_ALIASES`; now whole-word). tmux never knows which conversation a pane runs, so `tracker::apply_pane_presence` binds by thread id (never available from tmux) → thread name (only Amp puts it in the title) → "if exactly one entry of that agent exists in the session, stamp the last listed pane" (F036). That binding drives `Enter` (focus pane), `x` (kill pane), seen-marking when the pane is focused (`focused_pane_by_session`, F013), and prune protection (`liveness == Alive` is never pruned).
5. **Pruning.** `prune_agents` runs inside every `snapshot_json`: terminal entries go ~10 s after their pane is observed gone (`exited_at`), else after 5 min seen / 30 min unseen; running entries with no live pane after 30 min (`tracker.rs` constants).

Verified fact for the redesign: a Claude Code process launched inside tmux inherits `TMUX_PANE` (checked from a live session: `TMUX_PANE=%136`, `tmux display-message -t %136 '#{session_name} #{pane_current_command}'` → `opensessions claude`). Claude Code hook events (`SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Notification`, `Stop`, `SubagentStart`/`SubagentStop`, `SessionEnd`) receive `session_id`, `cwd`, `transcript_path`, `hook_event_name` (plus `notification_type`, `tool_name`, `agent_id`… per event) on stdin, and run with the process environment. The user's `~/.claude/settings.json` currently has no hooks configured.

## 3. Open defects in this pipeline (deferred to the redesign)

From `audit-findings.md`, status `agent redesign` (ids refer to that file):

- F013 (high) background-session panes stay in `focused_pane_by_session` forever → finished agents there are marked seen instantly; no unseen dot.
- F036 (high) single-entry pane attachment stamps the last listed pane and is never re-evaluated → kill/focus/seen route to the wrong Claude/Codex pane.
- F039 project-dir decoding trusts the dash→slash guess; F016 ancestor-directory sessions make nested agents unresolvable; F118 two sessions in one directory resolve to nobody.
- F035/F110 custom HTTP agents lose `paneId` on the first snapshot and get reaped ~10 s later; F038 waiting/idle entries are never pruned; F037 fresh events clear `exited_at`; F042 `mark_seen` leaves `event.unseen`; F043 tracker bookkeeping for dead sessions never released.
- F040 Claude Code `isMeta`/compact-summary user entries become thread names; F041 Codex injected user items become titles; F046 Droid per-line ids fork instances.
- F015/F020/F010 full transcript re-read + repeated `list_sessions` per unresolved snapshot every 2 s.
- F116 `/api/agent-event` trusts client `ts` (seconds instead of ms → pruned in the same request).
- F044 was decided: `stale` stays an attention-worthy (unseen) state; docs updated.

## 4. Redesign direction (proposed, not yet designed)

**Hooks as the primary source for Claude Code; transcript polling as fallback.** A small hook (shell or a subcommand shipped with the binaries) posts `{agent:"claude-code", threadId: session_id, paneId: $TMUX_PANE, tmuxSession: <from tmux display-message -t $TMUX_PANE>, status}` on each event: `UserPromptSubmit` → running, `PreToolUse` → tool-running, `Notification/permission_prompt` → waiting, `Stop` → done, `SessionEnd` → gone; `SubagentStart/Stop` for Task-tool subagents. Then:

- `(agent, threadId)` is the only identity; pane binding is explicit when an integration supplies it and heuristic only as fallback.
- The watcher path stays for hook-less agents but becomes incremental (per-file byte offsets, parse only appended lines) and uses the hook's `cwd`/`transcript_path` when available instead of decoding folder names.
- Installation: a `hooks` block in `~/.claude/settings.json`; needs an installer that merges JSON safely and an uninstall path.
- Everything in `docs/explanation/sidebar-behavior.md` about sidebar width/focus is unaffected; `CONTEXT.md` ("Agent Thread State", "Agent Pane Presence") already states the intended ownership rules.

## 5. Product questions the maintainer still has to answer

Decide these during the design (they were raised by the audit and left open on purpose):

1. Two tmux sessions rooted at the same directory: attribute transcript agents to the session with a live agent pane, show under all, or keep unresolved? (F118)
2. `/api/agent-event` with `ts` in seconds: auto-convert, reject with 400, or ignore client `ts`? (F116)
3. Should the header spinner count use the same definition as group badges and the `running` filter? (F067)
4. `d` on the attached session is a silent no-op — add feedback? (F102)
5. Make the unconditional `prefix C-s / C-t / M-1..9` bindings opt-in? They shadow tmux's `M-1..M-5` layout keys and tmux-resurrect's `C-s`. (F092)
6. Delete the orphaned migration modules `watch_plan.rs`, `lifecycle_operation.rs`, `portless.rs`? (F105)

## 6. Working rules that matter here

- Run `. ~/.cargo/env` first (rustup lives in the home dir). Build **all bins** before the E2E suite: `cargo build --workspace --bins && cargo test -p opensessions-sidebar --test tmux_e2e` — the suite runs `target/debug/opensessions-server` and does not rebuild it.
- Never run `tmux` without `-L <private socket>` in experiments; the user's live tmux (default socket) and the live upstream server on `127.0.0.1:24500` are off limits. `target/debug/opensessions-server` has no CLI flags — it binds a port and blocks; run it in the background with `OPENSESSIONS_PORT`, `OPENSESSIONS_PID_FILE`, `HOME`, `TMUX` pointed at scratch values.
- tmux ≤ 3.5a on Debian/Ubuntu loses `SIGCHLD` for exiting pane processes (tmux #4559); `nudge_dead_panes` in the poll loop works around it. tmux also expands `$name` inside hook bodies unless escaped — build hook bodies only through `tmux_scripting::run_shell_command`.
- Keep `AGENTS.md` rules: no pane-derived agent status as a source of truth, sync `MuxProvider` methods, HTTP-first integrations, `SERVER_VERSION` from `package.json`.
- Debug logging is opt-in: `OPENSESSIONS_DEBUG_LOG=<path>` (capped at 16 MB) — the E2E lab sets it per run.

## 7. Init prompt for the next session

```
You are continuing maintenance of the opensessions fork (Xubbbb/opensessions), a Rust tmux sidebar. Start in plan mode; do not write code yet.

First read, in this order: docs/handoff/agent-redesign-handoff.md, docs/handoff/audit-findings.md (only the rows marked "agent redesign"), AGENTS.md, CONTEXT.md, CONTRACTS.md, and the code they point to for the agent pipeline (apps/server-rs/src/lib.rs scan_*/apply_agent_event/sync_agent_pane_presence/prune_agents, packages/runtime-rs/src/tracker.rs, agent_watchers.rs, agent_parsers.rs, project_dir_session.rs, packages/runtime-rs/src/tmux_provider.rs agent_from_pane).

Goal: redesign and then implement agent identification, focusing on Claude Code first. The handoff proposes hooks-as-primary-source (Claude Code hooks posting to /api/agent-event with session_id + $TMUX_PANE), explicit pane binding, and an incremental transcript fallback.

Before producing a plan, interview me: confirm or challenge the proposed direction, resolve the six open product questions in section 5 of the handoff, and ask about anything the design depends on (which agents besides Claude Code matter to me, how hooks should be installed, what "done"/"waiting"/"unseen" should mean in the sidebar, whether subagents should appear). Then present a design with: the identity model, the event/HTTP contract changes, tracker state machine and pruning rules, the fallback watcher, migration and install steps, and the test plan (unit + tmux E2E). Only after I approve the plan, implement it in small verified steps.
```
