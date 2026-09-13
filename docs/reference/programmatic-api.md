# Programmatic Metadata API

opensessions exposes HTTP endpoints that let agents, scripts, and CI pipelines push custom metadata to the TUI sidebar. This is the programmatic surface — your tools can self-report status, progress, and structured logs without opensessions needing a dedicated watcher.

## Endpoints

All endpoints accept `POST` with `Content-Type: application/json` on the local server. The port is derived from the tmux socket (`22000 + hash(socket path)`), so it differs per tmux server; `7391` is only the fallback used outside tmux. The examples below read it into `OPENSESSIONS_URL` first:

```sh
OPENSESSIONS_URL="$(sh ~/.tmux/plugins/opensessions/integrations/tmux-plugin/scripts/port.sh)"
```

### `POST /set-status`

Set a status pill on a session. Shows in both the session card and the detail panel.

```sh
# Set status
curl -sS -X POST "$OPENSESSIONS_URL"/set-status \
  -H 'content-type: application/json' \
  -d '{"session":"api","text":"Indexing","tone":"info"}'

# Clear status
curl -sS -X POST "$OPENSESSIONS_URL"/set-status \
  -H 'content-type: application/json' \
  -d '{"session":"api","text":null}'
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `session` | `string` | yes | Mux session name |
| `text` | `string \| null` | yes | Status text, or `null` to clear |
| `tone` | `string` | no | One of `neutral`, `info`, `success`, `warn`, `error` |

### `POST /set-progress`

Set a progress indicator on a session. Shows as a compact summary (e.g. `3/10` or `75%`).

```sh
# Set progress with current/total
curl -sS -X POST "$OPENSESSIONS_URL"/set-progress \
  -H 'content-type: application/json' \
  -d '{"session":"api","current":3,"total":10,"label":"files"}'

# Set progress with percent
curl -sS -X POST "$OPENSESSIONS_URL"/set-progress \
  -H 'content-type: application/json' \
  -d '{"session":"api","percent":0.75,"label":"deploying"}'

# Clear progress
curl -sS -X POST "$OPENSESSIONS_URL"/set-progress \
  -H 'content-type: application/json' \
  -d '{"session":"api","clear":true}'
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `session` | `string` | yes | Mux session name |
| `current` | `number` | no | Current step |
| `total` | `number` | no | Total steps |
| `percent` | `number` | no | Progress as 0.0–1.0 (alternative to current/total) |
| `label` | `string` | no | Short label shown next to the number |
| `clear` | `boolean` | no | Set to `true` to clear progress |

### `POST /log`

Append a structured log entry to a session. Last 8 entries are visible in the detail panel.

```sh
curl -sS -X POST "$OPENSESSIONS_URL"/log \
  -H 'content-type: application/json' \
  -d '{"session":"api","message":"Build started","source":"ci","tone":"info"}'
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `session` | `string` | yes | Mux session name |
| `message` | `string` | yes | Log message (max 500 chars, truncated) |
| `tone` | `string` | no | One of `neutral`, `info`, `success`, `warn`, `error` |
| `source` | `string` | no | Source label (e.g. `ci`, `build`, `agent`) |

### `POST /clear-log`

Clear all log entries for a session.

```sh
curl -sS -X POST "$OPENSESSIONS_URL"/clear-log \
  -H 'content-type: application/json' \
  -d '{"session":"api"}'
```

### `POST /notify`

Send a notification (currently appends to logs with highlighting). Same fields as `/log`.

```sh
curl -sS -X POST "$OPENSESSIONS_URL"/notify \
  -H 'content-type: application/json' \
  -d '{"session":"api","message":"Deploy complete","tone":"success","source":"cd"}'
```

## How It Renders

**Session card** — A compact summary line showing status text + progress:
```
  Indexing · 3/10 files
```

**Detail panel** — Full status, progress, and recent log entries with tone-colored icons:
```
ℹ Indexing
  · 3/10 files
ℹ [ci] Build started
✓ [ci] Tests passed
✗ [ci] Lint failed
```

## Tones

| Tone | Icon | Color |
|------|------|-------|
| `neutral` | `·` | gray |
| `info` | `ℹ` | blue |
| `success` | `✓` | green |
| `warn` | `⚠` | yellow |
| `error` | `✗` | red |

## Retention

- Status and progress are in-memory only — cleared on server restart.
- Logs are capped at 50 entries per session (oldest are dropped).
- Metadata for deleted sessions is auto-pruned.
- Messages are truncated: status at 100 chars, logs at 500 chars.

## Example: Build Script

```bash
#!/bin/bash
SESSION=$(tmux display-message -p '#{session_name}')
URL="$(sh ~/.tmux/plugins/opensessions/integrations/tmux-plugin/scripts/port.sh)"

curl -sS -X POST "$URL/set-status" \
  -H 'content-type: application/json' \
  -d "{\"session\":\"$SESSION\",\"text\":\"Building\",\"tone\":\"info\"}"

curl -sS -X POST "$URL/set-progress" \
  -H 'content-type: application/json' \
  -d "{\"session\":\"$SESSION\",\"current\":0,\"total\":3,\"label\":\"steps\"}"

npm run build 2>&1 && {
  curl -sS -X POST "$URL/log" \
    -H 'content-type: application/json' \
    -d "{\"session\":\"$SESSION\",\"message\":\"Build succeeded\",\"tone\":\"success\",\"source\":\"build\"}"
} || {
  curl -sS -X POST "$URL/log" \
    -H 'content-type: application/json' \
    -d "{\"session\":\"$SESSION\",\"message\":\"Build failed\",\"tone\":\"error\",\"source\":\"build\"}"
}

# Clear when done
curl -sS -X POST "$URL/set-status" \
  -H 'content-type: application/json' \
  -d "{\"session\":\"$SESSION\",\"text\":null}"

curl -sS -X POST "$URL/set-progress" \
  -H 'content-type: application/json' \
  -d "{\"session\":\"$SESSION\",\"clear\":true}"
```
