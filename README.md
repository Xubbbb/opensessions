# opensessions

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://opensource.org/licenses/MIT)
[![Stars](https://img.shields.io/github/stars/Xubbbb/opensessions?style=social)](https://github.com/Xubbbb/opensessions)

> This is a maintained fork of [Ataraxy-Labs/opensessions](https://github.com/Ataraxy-Labs/opensessions), which stopped receiving updates in mid-2026. It keeps the same install path and configuration, fixes the outstanding bugs, and carries over community fixes from the upstream pull-request queue. The original work is by Ataraxy Labs and contributors.

tmux is all you need. make tmux great again :)

<img width="4180" height="2416" alt="amp-img-e686694168e21738-aesthetic" src="https://github.com/user-attachments/assets/2caaee1a-b3f5-4041-aa3c-5b3668aa1912" />

`opensessions` is a sidebar for `tmux` when your sessions, agents, and localhost tabs start multiplying.

It lives inside your existing tmux workflow instead of replacing it: one small pane for session switching, agent state, repo breadcrumbs, and quick jumps back into the right terminal.

Built with Rust and [ratatui](https://ratatui.rs) for native performance and zero runtime dependencies.

tmux is the only supported mux today. There is older zellij integration code in the repo, but it is not stable enough to document as supported; we are looking for maintainers who want to help bring it back to that bar.

## Install With TPM

Requirements:

- `tmux` 3.0 or newer
- [TPM](https://github.com/tmux-plugins/tpm)
- `curl` or `wget` for downloading prebuilt binaries on first load

Add this to `~/.tmux.conf`:

```tmux
set -g @plugin 'Xubbbb/opensessions'
```

Then reload tmux and install plugins:

```bash
tmux source-file ~/.tmux.conf
~/.tmux/plugins/tpm/bin/install_plugins
```

Open the sidebar with `prefix o → s`.

TPM clones the repo into `~/.tmux/plugins/opensessions`. On first load, opensessions downloads the matching prebuilt release bundle into `bin/`; that bundle includes `opensessions-sidebar`, `opensessions-server`, and `lazydiff`.

If your platform is unsupported or you are developing locally, you can still build from source:

```bash
cd ~/.tmux/plugins/opensessions
cargo build --release
```

If you want the same setup as a single shell command:

```bash
grep -q "Xubbbb/opensessions" ~/.tmux.conf 2>/dev/null || printf '\nset -g @plugin '\''Xubbbb/opensessions'\''\n' >> ~/.tmux.conf && tmux source-file ~/.tmux.conf && ~/.tmux/plugins/tpm/bin/install_plugins
```

Switching from the upstream plugin: run the uninstall script from the old checkout first (see below), then replace `Ataraxy-Labs/opensessions` with `Xubbbb/opensessions` in `~/.tmux.conf` and install as above. Your `~/.config/opensessions/` configuration carries over unchanged.

## Update

Use TPM's built-in update (`prefix + U`) or run:

```bash
~/.tmux/plugins/tpm/bin/update_plugins opensessions
```

No rebuild step:

No local rebuild is needed for normal installs. Reload tmux after TPM updates the plugin; opensessions will download the matching release bundle if `bin/` is missing or incomplete.

The plugin automatically restarts the server on update so it picks up the new binary. Toggle the sidebar back on with `prefix o → s` if it was open.

## Uninstall

Run the uninstall script **before** removing the plugin files — it cleans up tmux hooks, keybindings, sidebar panes, and environment variables that would otherwise persist and cause glitching:

```bash
sh ~/.tmux/plugins/opensessions/integrations/tmux-plugin/scripts/uninstall.sh
```

Then remove the `set -g @plugin 'Xubbbb/opensessions'` line from `~/.tmux.conf` and run `prefix + alt + u` (TPM uninstall).

## Today

- Live agent state across sessions for Amp, Claude Code, Codex, and OpenCode.
- Per-thread unseen markers for `done`, `error`, and `interrupted` states.
- Session context in the UI: branch in the list, working directory in the detail panel, thread names, and detected localhost ports.
- Programmatic metadata API: agents and scripts push status, progress, and logs to the sidebar via HTTP.
- Fast switching with `j`/`k`, arrows, `Tab`, `1`-`9`, session reordering, hide/restore, creation, and kill actions.
- `prefix o → s` and `prefix o → t` for sidebar focus and toggle, `prefix o → e` for sidebar-safe `even-horizontal` layout in the current window, `prefix o → 1` through `9` for quick switching, optional no-prefix shortcuts, and in-app theme switching.
- Finished agents leave the sidebar on their own: shortly after their pane closes, or after a timeout otherwise (30 minutes while still unseen, 5 minutes once seen). `d` in the agents panel dismisses one immediately.
- Works with any tmux `default-shell`, including fish; installs its tmux hooks in a dedicated array slot so it coexists with other plugins' hooks.
- Native Rust sidebar built with ratatui 0.30 and crossterm 0.29, with a local Rust WebSocket/HTTP server.

## Programmatic API

Scripts and agents can push custom metadata to the sidebar over HTTP — no binary needed. The server's port is derived from the tmux socket (`22000 + hash(socket path)`), so it differs per tmux server and is not `7391` (that is only the fallback outside tmux). Ask for it from inside tmux:

```sh
OPENSESSIONS_URL="$(sh ~/.tmux/plugins/opensessions/integrations/tmux-plugin/scripts/port.sh)"
```

```sh
# Set a status pill on a session
curl -X POST "$OPENSESSIONS_URL/set-status" \
  -H 'content-type: application/json' \
  -d '{"session":"my-app","text":"Deploying","tone":"warn"}'

# Set progress
curl -X POST "$OPENSESSIONS_URL/set-progress" \
  -H 'content-type: application/json' \
  -d '{"session":"my-app","current":3,"total":10,"label":"services"}'

# Push a log entry
curl -X POST "$OPENSESSIONS_URL/log" \
  -H 'content-type: application/json' \
  -d '{"session":"my-app","message":"Tests passed","source":"ci","tone":"success"}'
```

Endpoints: `/set-status`, `/set-progress`, `/log`, `/clear-log`, `/notify`

Tones: `neutral`, `info`, `success`, `warn`, `error` — each with a distinct icon and color.

Full reference: [docs/reference/programmatic-api.md](./docs/reference/programmatic-api.md)

## Local Development

Build and run from a local clone:

```bash
git clone https://github.com/Xubbbb/opensessions.git
cd opensessions
cargo build --release
cargo test --workspace
```

`cargo test --workspace` includes the tmux end-to-end suite, which starts private tmux servers on their own sockets and needs `tmux`, `git`, and `python3` on `PATH`.

To try a build without touching the tmux server you work in, start an isolated one — its own socket, sessions, hooks, and opensessions server on its own port:

```bash
cargo build --release && scripts/isolated-tmux.sh      # attach; prefix o → s opens the sidebar
scripts/isolated-tmux.sh --stop                         # tear it down
```

It loads your `~/.tmux.conf` minus plugin lines plus this checkout's `opensessions.tmux`, shares your `~/.config/opensessions/` settings and agent transcripts so real state shows up, and logs to `/tmp/opensessions-ostest-debug.log`. Run it from a terminal outside your normal tmux; started from inside, the inner prefix becomes `C-a`.

To point your own tmux at the checkout instead of the TPM copy, add `run '/absolute/path/to/opensessions/opensessions.tmux'` to `~/.tmux.conf` (or use `scripts/toggle-dev.sh` to flip between the two). Set `OPENSESSIONS_SKIP_BINARY_DOWNLOAD=1` in the environment tmux starts from so the checkout uses `target/` builds rather than downloading a release bundle.

Debug logging is off by default. To capture it, set `OPENSESSIONS_DEBUG_LOG` to a file path in tmux's global environment before the server starts (`tmux set-environment -g OPENSESSIONS_DEBUG_LOG /tmp/opensessions-debug.log`, then `q` and reopen the sidebar). The file is capped at 16 MB.

Start the sidebar manually (outside tmux, for testing):

```bash
cargo run -p opensessions-sidebar
```

Start the server:

```bash
cargo run -p opensessions-server
```

For the full tmux workflow with keybindings, troubleshooting, and configuration options, follow the guide below.

## Docs

- [Get started in tmux](./docs/tutorials/get-started-in-tmux.md)
- [Set up Ghostty shortcuts](./docs/how-to/set-up-ghostty-shortcuts.md)
- [Configuration reference](./docs/reference/configuration.md)
- [Features and keybindings reference](./docs/reference/features-and-keybindings.md)
- [Programmatic API reference](./docs/reference/programmatic-api.md)
- [Architecture explanation](./docs/explanation/architecture.md)
- [Contracts and supported integration interfaces](./CONTRACTS.md)

## A Few Concrete Bits

- Session ordering is persisted in `~/.config/opensessions/session-order.json`.
- Amp watcher reads `~/.local/share/amp/threads/*.json`.
- Claude Code watcher reads JSONL transcripts in `~/.claude/projects/`, plus `$CLAUDE_CONFIG_DIR/projects/` and any sibling `~/.claude*/projects/` directory, so multiple Claude Code accounts all show up.
- Codex watcher reads transcript JSONL files in `~/.codex/sessions/` or `$CODEX_HOME/sessions/` and resolves sessions from `turn_context.cwd`.
- OpenCode watcher polls the SQLite database in `~/.local/share/opencode/opencode.db`.
- Toggling the sidebar off closes its panes; `prefix o → e` briefly parks the sidebar pane in a tmux session named `_os_stash` while it re-lays out the window.
- Clicking a detected port opens `http://localhost:<port>`.

## Repo Layout

### Apps

- `apps/tui-rs/` — Rust ratatui sidebar client (connects to server over WebSocket)
- `apps/server-rs/` — Rust server that assembles state from mux providers and agent watchers
- `apps/tui/scripts/` — Shell scripts for tmux sidebar launch and session switching

### Packages

- `packages/runtime-rs/` — Shared Rust runtime: tmux provider, agent watchers, config, tracker, protocol
- `packages/sidebar-core-rs/` — Core sidebar state, input, and rendering logic

### Integrations

- `opensessions.tmux` — Root TPM entrypoint for users
- `integrations/tmux-plugin/` — tmux-facing scripts and host integration glue
- `integrations/amp/` — Amp agent integration
- `integrations/pi-extension/` — Pi extension integration

## Current Caveats

- The app is local-only; the default host is `127.0.0.1`, and ports are derived per tmux socket unless explicitly overridden.
- `theme`, `sidebarWidth`, `sidebarPosition`, `detailPanelHeight`, `sessionFilter`, and `mux` are wired through the runtime. `plugins`, `port`, and `keybinding` are parsed for compatibility but are not active runtime extension hooks today.
- Inline theme objects exist in core, but the running server persists and broadcasts theme names.
- tmux is the only supported mux today.

## License

MIT. Originally developed by Ataraxy Labs and contributors; maintained here as a fork.
