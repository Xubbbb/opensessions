# Features And Keybindings Reference

This page lists the user-visible features currently implemented in the sidebar.

## Session List

Each session row can show:

- Session index for number-key switching
- Session name
- Branch name, truncated when needed
- Running spinner or terminal-state marker
- Focused state highlight
- Current client session emphasis
- Unseen accent for `done`, `error`, `interrupted`, or `stale` states

## Detail Panel

The focused session detail panel can show:

- Truncated working directory
- Detected localhost ports
- Agent instances in that session
- Thread names when watchers provide them

Clicking a detected port opens `http://localhost:<port>`.

## Agent Features

- Multiple agent instances per session when a watcher emits `threadId`
- Per-instance unseen tracking: a finished agent keeps its `●` until its pane is the active pane of an attached tmux client, until you `Enter` it, or until the session is marked seen
- The selected row is yours: it moves when you move it, when you pick a session (Enter, click, `1`–`9`, the index keys, or opening an agent: the sidebar you arrive in then shows that session selected), or when its row disappears. A plain tmux switch back to a session leaves its sidebar's selection where you put it. The current session is marked with `▌`, the selected row with `›`.
- Renaming a tmux session keeps its sidebar working: the sidebar re-identifies itself and follows the new name (its `▌` marker and agents panel stay). Other sidebars that had the old name selected fall back to their own row, since rows are identified by name.
- Status values: `idle`, `running`, `tool-running`, `done`, `error`, `waiting`, `interrupted`, `stale`; a qualifier in parentheses adds the reason when there is one (`working (delegating)` while background subagents run, `blocked (dialog open)`, `done (shell running)`)
- Live Claude Code sessions are rows even while idle (`✓ idle`), named after their `/rename` name
- Agents are removed automatically about 10 seconds after their tmux pane or process disappears, whatever their status. Agents that never had a pane are dropped after 5 minutes once seen or 30 minutes while still unseen when finished, and after 30 minutes of silence otherwise. `d` in the agents panel removes one immediately.

## Session Metadata Features

- Git branch lookup from the session directory
- Dirty-worktree detection
- Git worktree detection
- Pane count and window count per session
- Session uptime display
- Session ordering persisted across restarts

## Sidebar Management Features

### tmux-specific

- Global hooks for session changes, pane/window selection, window creation, pane exit, and resize. Each hook is installed in array slot `90210` (for example `after-select-window[90210]`) so other plugins' hooks on the same events are left intact
- Sidebar panes are launched through `sh -c`, so any tmux `default-shell` works, including fish
- `prefix o → e` parks the sidebar pane in a session named `_os_stash` while re-laying out the window, then joins it back at its fixed width

## Keyboard Shortcuts

### Inside the sidebar

Sessions panel (the default focus):

| Key | Action |
| --- | --- |
| `j`, `Down` | Move focus down |
| `k`, `Up` | Move focus up |
| `Enter` | Switch to the focused session, or expand/collapse a focused worktree group |
| `Tab` | Switch to the next session immediately |
| `Shift+Tab` | Switch to the previous session immediately |
| `1`-`9` | Switch to session by visible index |
| `Alt+Up` | Move focused session (or worktree group) up in persisted order |
| `Alt+Down` | Move focused session (or worktree group) down in persisted order |
| `Right` | Focus the agents panel if the focused session has agents; otherwise widen the detail panel |
| `Left` | Shrink the detail panel |
| `Ctrl+J` | Focus the agents panel |
| `n`, `c` | Create a new tmux session (detached; it appears in the list) |
| `d` | Hide the focused session from the list |
| `u` | Show all hidden sessions again |
| `x` | Open the kill confirmation for the focused session or worktree group (`y` confirms, any other key cancels) |
| `f` | Cycle the session filter: all → active → running |
| `a` | Toggle the agents panel between the current session and all sessions |
| `l` | Open lazydiff for the current session in a tmux popup |
| `L` | Open lazydiff for the current session in a new terminal window |
| `w` | Open the sidebar width slider (`Left`/`Right`/`h`/`l` step by 1, `H`/`L` by 5; every step applies live and both `Enter` and `Esc` close the slider keeping the current value) |
| `t` | Open the theme picker (type to filter, `Up`/`Down`, `Enter` keeps, `Esc` reverts) |
| `r` | Refresh state |
| `q` | Quit the server and every sidebar pane in this tmux server |
| `Ctrl+C` | Exit only this sidebar pane |

Agents panel (`Right`/`Ctrl+J` to enter):

| Key | Action |
| --- | --- |
| `j`/`k`, `Down`/`Up` | Move between agents |
| `Enter` | Switch to the agent's session and focus its pane |
| `d` | Dismiss the focused agent from the sidebar |

In the sessions list, `d` on the session your client is attached to does nothing except show a short footer notice: the attached session is always listed.
| `x` | Kill the agent's tmux pane (falls back to the session kill confirmation when no pane is known) |
| `Esc`, `Left`, `Ctrl+K` | Back to the sessions panel |

Mouse: click a session row to switch, click a group header to collapse/expand, click an agent to focus its pane, click a port to open it in the browser, scroll the lists, and drag the separator to resize the detail panel.

### tmux plugin shortcuts

| Key | Action |
| --- | --- |
| `prefix o → s` | Reveal and focus the sidebar pane |
| `prefix o → t` | Toggle the sidebar |
| `prefix o → e` | Spread non-sidebar panes in the current window using `even-horizontal` |
| `prefix o → 1` through `prefix o → 9` | Switch directly to the visible session indices |
| Configurable `@opensessions-focus-global-key` such as `Alt-s` | Reveal and focus the sidebar pane from any tmux pane |
| Configurable `@opensessions-index-keys` such as `Alt-1` through `Alt-9` | Switch directly to the visible session indices from any tmux pane |

## Session Creation Behavior

- `n`/`c` asks the server to run `tmux new-session -d`; the new session appears in the list and can be switched to like any other.
- For a directory picker, the repository ships a standalone `fzf` sessionizer at `apps/tui/scripts/sessionizer.sh`. It is not bound by default; bind it yourself, for example `bind-key C-f display-popup -E "~/.tmux/plugins/opensessions/apps/tui/scripts/sessionizer.sh"`.
- The sessionizer searches directories listed in `SESSIONIZER_DIR` (colon-separated, e.g. `$HOME/Code:$HOME/.config`) or `$HOME/Documents` if unset. The variable is also read from the tmux global environment (`tmux set-environment -g`) as a fallback.
- Search depth is controlled by `SESSIONIZER_MAXDEPTH` (defaults to `3`).
- If `fzf` is unavailable, the sessionizer exits with a prompt explaining that dependency.

## Session Switching Behavior

- Switching routes through the server so the server can use authoritative client TTY information.
- tmux is the only supported mux today.

## Refresh And Discovery Behavior

- Git info is cached for 5 seconds.
- Listening localhost ports are re-polled every 10 seconds (via `lsof`).
- The Claude Code session registry is polled every 500 ms (tmux is only asked when a record or process changed). Transcript watchers (Amp, Codex, OpenCode, Pi, Droid) poll every 2 seconds and only look at files modified in the last 5 minutes; the transcripts of live Claude Code sessions are read incrementally on the same cadence, for their title and last prompt only.
- tmux state is re-polled every 2 seconds as a backstop for anything the hooks missed.

## Files And Paths The UI Depends On

- `~/.local/share/amp/threads/`
- `~/.claude/sessions/` and `~/.claude/projects/` (also under `$CLAUDE_CONFIG_DIR` and any `~/.claude*/`)
- `~/.codex/sessions/` (or `$CODEX_HOME/sessions/`)
- `~/.local/share/opencode/opencode.db` (or `$OPENCODE_DB_PATH`)
- `~/.pi/agent/sessions/` and `~/.factory/projects/`
- `~/.config/opensessions/config.json`

## Related Docs

- [Configuration reference](./configuration.md)
- [Architecture explanation](../explanation/architecture.md)
- [Contracts and extension interfaces](../../CONTRACTS.md)
