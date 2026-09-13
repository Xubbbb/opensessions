#!/usr/bin/env sh
# Print the opensessions server URL for the current tmux server.
# The port is derived from the tmux socket path (22000 + hash), so scripts
# should ask rather than assume; run this from inside tmux:
#   curl -X POST "$(sh ~/.tmux/plugins/opensessions/integrations/tmux-plugin/scripts/port.sh)/set-status" ...

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
. "$SCRIPT_DIR/server-common.sh"
printf 'http://%s:%s\n' "$HOST" "$PORT"
