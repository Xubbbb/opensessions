#!/usr/bin/env sh
# opensessions uninstall — clean up all tmux hooks, keybindings, sidebar panes, and env vars
# Run this BEFORE removing the plugin files.
#
# Usage:
#   sh /path/to/opensessions/integrations/tmux-plugin/scripts/uninstall.sh

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
. "$SCRIPT_DIR/server-common.sh"

echo "opensessions: uninstalling..."

# --- Stop the server first so it does not reinstall hooks behind us ---
# /quit also makes the server broadcast quit to every sidebar client and
# remove its own hooks; everything below is a belt-and-braces sweep for a
# server that was not running or died uncleanly.
curl -s -o /dev/null -m 1 --noproxy '*' -X POST "http://${HOST}:${PORT}/quit" 2>/dev/null || true
sleep 0.3
echo "  ✓ stopped server (if running)"

# --- Remove global hooks ---
# opensessions writes each hook into its own array slot (HOOK_SLOT, kept in
# sync with packages/runtime-rs/src/tmux_scripting.rs) so it can coexist with
# other plugins. Remove only entries opensessions wrote: our slot, plus any
# index-0 entry left by releases that predate the slot scheme. Never unset the
# whole array, which would wipe other plugins' hooks.
HOOK_SLOT=90210
owned_hook_indices() {
  tmux show-hooks -g "$1" 2>/dev/null | awk -v hook="$1" -v slot="$HOOK_SLOT" '
    index($0, hook "[") == 1 {
      rest = substr($0, length(hook) + 2)
      close_bracket = index(rest, "]")
      if (close_bracket == 0) next
      idx = substr(rest, 1, close_bracket - 1)
      cmd = substr(rest, close_bracket + 1)
      if (idx == slot || cmd ~ /opensessions-sidebar/ || cmd ~ /--connect-timeout 0\.1 -X POST http:\/\/[^ ]*\/(focus|refresh|session-renamed|ensure-sidebar|pane-exited|client-resized|pane-layout-changed)([ ?"]|$)/) {
        print idx
      }
    }'
}
for hook in \
  client-session-changed \
  after-select-pane \
  session-created \
  session-closed \
  session-renamed \
  after-select-window \
  after-new-window \
  client-resized \
  after-kill-pane \
  pane-exited \
  pane-died \
  after-resize-pane \
  after-resize-window; do
  for idx in $(owned_hook_indices "$hook"); do
    tmux set-hook -gu "${hook}[${idx}]" 2>/dev/null || true
  done
done
echo "  ✓ removed global hooks"

# --- Kill sidebar panes ---
# Find all panes titled "opensessions-sidebar" and kill them. Windows that
# hosted a sidebar had remain-on-exit switched on; switch it back off first
# so killing the pane does not leave a dead placeholder behind.
sidebar_rows=$(tmux list-panes -a -F '#{pane_id} #{window_id} #{pane_title}' 2>/dev/null | awk '$3 == "opensessions-sidebar" { print $1, $2 }') || true
if [ -n "$sidebar_rows" ]; then
  printf '%s\n' "$sidebar_rows" | while read -r pane window; do
    tmux set-window-option -t "$window" remain-on-exit off 2>/dev/null || true
    tmux kill-pane -t "$pane" 2>/dev/null || true
  done
  echo "  ✓ killed sidebar panes"
fi

# --- Kill stash session ---
tmux kill-session -t "_os_stash" 2>/dev/null || true
echo "  ✓ removed stash session"

# --- Remove the pid file the server writes ---
rm -f "$PID_FILE" 2>/dev/null || true

# --- Remove keybindings ---
# Command table bindings (opensessions key table)
PREFIX_KEY=$(tmux show-option -gqv "@opensessions-prefix-key" 2>/dev/null)
PREFIX_KEY="${PREFIX_KEY:-o}"
tmux unbind-key "$PREFIX_KEY" 2>/dev/null || true

# Unbind all keys in the opensessions command table
tmux unbind-key -T opensessions s 2>/dev/null || true
tmux unbind-key -T opensessions t 2>/dev/null || true
tmux unbind-key -T opensessions e 2>/dev/null || true
for i in 1 2 3 4 5 6 7 8 9; do
  tmux unbind-key -T opensessions "$i" 2>/dev/null || true
done
tmux unbind-key -T opensessions Any 2>/dev/null || true

# Direct prefix bindings, then tmux's own defaults for the keys they shadowed
# (prefix M-1..M-5 select layouts) so they work again without a tmux restart.
tmux unbind-key C-s 2>/dev/null || true
tmux unbind-key C-t 2>/dev/null || true
for i in 1 2 3 4 5 6 7 8 9; do
  tmux unbind-key "M-$i" 2>/dev/null || true
done
tmux bind-key M-1 select-layout even-horizontal 2>/dev/null || true
tmux bind-key M-2 select-layout even-vertical 2>/dev/null || true
tmux bind-key M-3 select-layout main-horizontal 2>/dev/null || true
tmux bind-key M-4 select-layout main-vertical 2>/dev/null || true
tmux bind-key M-5 select-layout tiled 2>/dev/null || true

# Global keys (if configured)
FOCUS_GLOBAL_KEY=$(tmux show-option -gqv "@opensessions-focus-global-key" 2>/dev/null)
if [ -n "$FOCUS_GLOBAL_KEY" ]; then
  tmux unbind-key -n "$FOCUS_GLOBAL_KEY" 2>/dev/null || true
fi
INDEX_KEYS=$(tmux show-option -gqv "@opensessions-index-keys" 2>/dev/null)
for key in $INDEX_KEYS; do
  tmux unbind-key -n "$key" 2>/dev/null || true
done
echo "  ✓ removed keybindings"

# --- Remove environment variables and options ---
tmux set-environment -gu OPENSESSIONS_DIR 2>/dev/null || true
tmux set-environment -gu OPENSESSIONS_WIDTH 2>/dev/null || true
tmux set-environment -gu OPENSESSIONS_HOST 2>/dev/null || true
tmux set-environment -gu OPENSESSIONS_PORT 2>/dev/null || true
tmux set-environment -gu OPENSESSIONS_PID_FILE 2>/dev/null || true
tmux set-option -gu @opensessions_width 2>/dev/null || true
echo "  ✓ removed environment variables"

echo "opensessions: uninstall complete. You can now remove the plugin files."
echo "  If using TPM: remove the line from .tmux.conf and run prefix + alt + u"
