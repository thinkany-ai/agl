# agl protocol reference

This is the one place the wire protocol is defined. The local daemon socket
and the hub link both use it. The hub UI socket uses it too, with one extra
field, `machine_id`, on each request.

## Transport

| Endpoint | Who connects | Auth |
|---|---|---|
| `ws://127.0.0.1:<port>/ws` | CLI, desktop apps, local runtimes | `?token=` or `Authorization: Bearer`. The token comes from `$AGL_HOME/daemon.json` (`{pid, port, token, version}`, mode 0600). |
| `<hub>/ws/machine?machine_id=…&code=…\|token=…` | the agl daemon, dialing out | One-time pairing code, then the machine's own token |
| `<hub>/ws/ui?token=…` (reference hub) | browsers | Hub admin token. In your product, use your own auth. |

Framing:
- One JSON object per WebSocket text frame.
- `type` discriminates the message:
  - `request {id, method, params}`: client → daemon.
  - `response {id, result?, error?}`: daemon → client. `error` is a string.
  - `event {session_id, seq, ts, event}`: daemon → subscribers.
  - `hello {machine}`: machine → hub, the first frame of a link.
  - `paired {token}`: hub → machine, sent after a pairing-code connect.
- `id` is a string you choose. Responses can arrive in any order.

## Local-only methods

| method | params | result |
|---|---|---|
| `subscribe` / `unsubscribe` | `{session_id}` or `{all: true}` | Start or stop receiving that session's events. `session.prompt` subscribes you automatically. |
| `daemon.shutdown` | – | Stops the daemon and every agent process. |
| `hub.reload` | – | Re-reads `config.json` and reconnects to the hub. |

## Methods

| method | params | result / notes |
|---|---|---|
| `ping` | – | `{pong, version}` |
| `machine.info` | – | `{machine_id, hostname, os, arch, version, agents[]}` |
| `agents.list` | – | `AgentInfo[]`: `{id, name, protocol, available, path, version, features[]}` |
| `agents.models` | `{agent}` | `{agent, models: [{id, name, description?, default?, efforts?[]}]}`. Never starts a conversation the user would see. |
| `daemon.status` | – | Version, pid, port, hub_status, sessions, live, defaults |
| `session.create` | `{agent, cwd?, model?, effort?, access?, name?, resume?, warm?}` | `SessionMeta`. Details: |
| | | `name`: if a session with that name exists, it is reused. |
| | | `resume`: attaches a native id. |
| | | `warm`: starts the process now (only takes a free slot). |
| | | `cwd` defaults to the home directory, and `~/` is expanded. |
| `session.prompt` | `{session_id, text, mode?, turn_id?}` | `{session_id, turn_id}`, returned as soon as the prompt is accepted. `mode`: |
| | | `queue` (default): run after the current turn. |
| | | `steer`: inject into the running turn (claude, codex); otherwise falls back to queue. |
| | | `interrupt`: stop the current turn, then run this one. |
| | | `turn_id`: your own id, max 64 chars. |
| `session.approve` | `{session_id, request_id, decision}` | `decision` is `allow`, `allow_session` or `deny` |
| `session.abort` | `{session_id}` | Interrupts the running turn and cancels queued prompts. If the runtime has not ended the turn after 10s, the process tree is killed. |
| `session.warm` | `{session_id}` | Pre-starts the agent process. |
| `session.configure` | `{session_id, model?, effort?, access?, name?}` | `SessionMeta`. Changing model, effort or access restarts the process before the next turn and resumes the conversation natively, so context is kept. |
| `session.get` / `session.list` | `{session_id}` / – | `SessionMeta` / `SessionMeta[]`, most recent first |
| `session.history` | `{session_id, after_seq?, limit?}` | `EventEnvelope[]`. Consecutive `text_delta` events are merged. History is persisted, so it survives a daemon restart. |
| `session.close` | `{session_id}` | Kills the process and deletes the session. |
| `gc.run` | `{idle?}` | `{suspended[], removed[]}` |

`session_id` accepts any of: the id, the session name, or an id prefix of at
least 4 characters.

### Reference hub UI methods

Hub-level methods take no `machine_id`:

| method | result |
|---|---|
| `machines.list` | Machine list, including offline paired machines |
| `pairing.create` | `{code, expires_at}`: a one-time code, valid 10 minutes |
| `machines.revoke {machine_id}` | Deletes the machine's token and disconnects it |

## SessionMeta

```json
{
  "id": "8b6d5cf21266", "name": "web-demo", "agent": "claude", "cwd": "/Users/me/proj",
  "model": "haiku", "effort": null, "access": "ask", "native_id": "6f0c…",
  "status": "idle", "created_at": 1792559001027, "last_active_at": 1792559009456,
  "turns": 3, "queued": 0, "live": true, "pending_approvals": 0,
  "features": ["warm", "approvals", "steer", "interrupt", "models", "effort"]
}
```

`status` values:

| status | meaning |
|---|---|
| `idle` | Process alive, no turn running |
| `starting` | Launching the process |
| `running` | A turn is in progress |
| `waiting` | Blocked on a permission request, or waiting for a process slot |
| `suspended` | No process. The next prompt resumes the conversation natively. |
| `closed` | The session was closed |

## Events (`event.kind`)

| kind | fields | notes |
|---|---|---|
| `user_message` | `turn_id, text` | Every accepted input, from any client |
| `queued` | `turn_id, position` | |
| `steered` | `turn_id, into_turn_id` | This input was injected into a running turn; it gets no turn of its own |
| `turn_started` | `turn_id` | |
| `text_delta` | `text` | Append in order. `"\n\n"` separates the agent's messages. |
| `reasoning` | `text` | Thinking, where the runtime exposes it |
| `tool_start` | `call_id, name, input` | |
| `tool_end` | `call_id, ok, output, images?` | `output` is the tool's text, capped at `tool_output_limit` (16KB). `images` are the images it returned, e.g. Codex computer-use screenshots: `[{mime_type, data (base64), omitted?}]`. Images over 4MB, and images beyond the newest `history_image_limit` in `session.history`, arrive with `omitted: true` and no data. |
| `plan` | `entries` | ACP plan, or claude TodoWrite `[{content, status}]` |
| `permission_request` | `request_id, tool, title, input, options[], expires_at` | Answer with `session.approve` |
| `permission_resolved` | `request_id, decision, by` | `by` is one of `client`, `policy`, `timeout`, `cancelled` |
| `heartbeat` | `turn_id, elapsed_ms, idle_ms, stalled` | Every 15s while running. `stalled` means no output for 60s. Heartbeats are not stored in history. |
| `retrying` | `message` | The runtime is retrying a model call, or agl switched credentials |
| `turn_ended` | `turn_id, outcome, error?, usage?, timing?` | `outcome` is `completed`, `failed` or `aborted`. `timing` is `{warm, launch_ms, first_event_ms, first_text_ms, total_ms}`. |
| `status` | `status` | |
| `info` | `native_id?, model?` | |
| `error` | `message` | Non-fatal |

A typical turn emits events in this order:

```
user_message → [queued] → [status starting] → turn_started → status running
  → text_delta… / tool_start → tool_end … / permission_request → permission_resolved
  → turn_ended → status idle
```

Rules for clients:
- Group events by `session_id`. Only one turn runs per session, so every event
  between `turn_started` and `turn_ended` belongs to that turn.
- `seq` increases monotonically per session and is persisted with history. To
  reconnect without losing events, call `session.history {after_seq: lastSeen}`
  and then resume handling live events.
- Do not treat `text_delta` as final until `turn_ended`. On `aborted`, the text
  streamed so far is partial.

## Access levels

| access | claude | codex app-server | ACP agents | cursor |
|---|---|---|---|---|
| `full` | `--dangerously-skip-permissions` | `danger-full-access`, approvals `never` | mode `yolo` / `bypassPermissions`; any request is auto-allowed | `--force` |
| `edit` | `acceptEdits` + approvals forwarded | `workspace-write`, `on-request` | mode `autoEdit` / `acceptEdits` + forwarded | prompts are not interactive |
| `ask` | `default` + approvals forwarded | `workspace-write`, `untrusted` | mode `default` + forwarded | – |
| `read_only` | `dontAsk` + read-only tool allowlist | `read-only`, approvals `never` | mode `plan`; requests are auto-denied | `--mode ask` |

How approval requests are handled:
- Forwarded requests time out after `approval_timeout_secs` (default 600s) and
  are then denied.
- Under `full` they are auto-allowed, and under `read_only` auto-denied. In
  both cases `permission_resolved` reports `by: "policy"`.

## Agents

| id | interface | warm process | approvals | steer | models |
|---|---|---|---|---|---|
| claude | `claude -p` stream-json + control protocol | ✓ | ✓ | ✓ | ✓ (via `initialize`) |
| codex | `codex app-server` JSON-RPC | ✓ | ✓ | ✓ (`turn/steer`) | ✓ (`model/list`) |
| gemini, opencode, kimi, grok, droid, openclaw, hermes, omp | ACP (first-party) | ✓ | ✓ | – (queued) | ✓ (session config) |
| cursor, pi | one process per turn, native resume | – | – | – | cursor ✓ |
| custom | `custom_agents` in config, any ACP CLI | ✓ | ✓ | – | ✓ |

## Configuration (`$AGL_HOME/config.json`)

```json
{
  "port": 7799, "default_access": "full", "max_live": 8,
  "idle_timeout_secs": 600, "session_ttl_secs": 604800,
  "approval_timeout_secs": 600, "stall_timeout_secs": 120, "turn_timeout_secs": 0,
  "tool_output_limit": 16384, "history_limit": 2000, "history_image_limit": 16,
  "custom_agents": [{"id": "goose", "command": "goose", "args": ["acp"], "protocol": "acp"}]
}
```

Environment variables:

| variable | effect |
|---|---|
| `AGL_HOME` | Data directory (default `~/.agl`) |
| `AGL_PORT` | Daemon port |
| `AGL_<ID>_BIN` | Executable for agent `<ID>` |
| `AGL_LOCAL_TOKEN` | Fixed local token |
| `AGL_NO_LOGIN_SHELL` | Do not import the login shell's environment |
