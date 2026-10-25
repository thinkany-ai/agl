# agl protocol reference

This is the one place the wire protocol is defined. The local daemon socket
and the hub link both use it. The hub UI socket uses it too, with one extra
field, `machine_id`, on each request.

## Transport

| Endpoint | Who connects | Auth |
|---|---|---|
| `ws://127.0.0.1:<port>/ws` | CLI, desktop apps, local runtimes | `?token=` or `Authorization: Bearer`. The token comes from `$AGL_HOME/daemon.json` (`{pid, port, token, version}`, mode 0600). |
| `<hub>/ws/machine?machine_id=…&code=…\|token=…` | the agl daemon, dialing out | One-time pairing code, then the machine's own token |
| `POST <hub>/machine/{pair,send,poll}` | the agl daemon, dialing out (`agl connect --transport http`) | The same, over HTTP: see [HTTP link](#http-link) |
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

**Versioning.** `hello.machine.protocol` and `ping` carry the wire protocol
version (currently `1`; agl ≤0.1 sends none, read it as `0`). New fields,
methods and event kinds are additive within a version, so clients must
ignore what they do not know. Only an incompatible change bumps the version.

## HTTP link

From agl 0.3.0, the machine link can also run over plain HTTPS requests, for
backends that cannot hold one WebSocket per machine (several stateless instances, a WAF, a
serverless host). The machine chooses it with `agl connect <hub> --transport
http`; the hub's install command can pass that flag in `__CONNECT_ARGS__`.
The messages are exactly the WebSocket link's; only how they travel differs.

Every call is a `POST` with a JSON body. After pairing, each carries
`Authorization: Bearer <machine token>` and `X-Agl-Machine-Id: <machine_id>`.

| call | body | answer |
|---|---|---|
| `<hub>/machine/pair` | `{machine_id, code}` | `{token, max_body?}`: the machine's own token, and the largest request body you accept. 401 for an invalid or used code. |
| `<hub>/machine/send` | `{messages: [...]}` | 2xx. `hello` first after every (re)connect, then `response` and `event` messages in the order they happened. |
| `<hub>/machine/poll` | `{ack, wait_ms}` | `{messages: [request…], cursor, max_body?}`. Held open up to `wait_ms` (25 000) until there is a request; empty `messages` on timeout. |

Delivery:
- **Requests, hub → machine.** Give each queued request a cursor that
  increases per machine. `poll` returns the oldest unacknowledged ones and
  `cursor`, the last cursor returned (or `ack` when there are none). The
  machine sends that back as `ack` on its next poll; drop everything up to
  it. Anything not yet acknowledged is delivered again, and the machine
  answers a request id it has already run from a cache instead of running it
  twice, so redelivering is always safe.
- **Size limit.** If something in front of you refuses large bodies (an AWS
  WAF stops at 8 KiB), say so with `max_body` (bytes, 1024 to 262144) when
  pairing and on polls. The machine keeps every request under it and sends a
  message too large for it as `part {id, index, count, data}` messages:
  `data` is base64 of the message JSON's bytes from `index` of `count` equal
  slices. Keep the parts of an `id` (they can reach different instances),
  join them in `index` order once all are there, and read the result as the
  message it is. Responses to the machine are not limited.
- **Messages, machine → hub.** Batches of up to 500 messages or 256 KiB; a
  batch that failed is sent again. Drop a `response` whose `id` you no longer
  wait for, and an `event` whose `seq` is not above the last one you have for
  its `session_id`.
- **Presence.** A machine is online from its `hello` while it keeps polling.
  It polls again as soon as a poll returns, so mark it offline when no poll
  has been open for about 60 s.
- **Errors.** 401 or 403 on any call means the token is revoked: the machine
  stops retrying and reports `unauthorized … run agl connect again`. 409 on
  `poll` means you have no `hello` for it (for instance after a restart): the
  machine reconnects and sends `hello` again. Anything else is retried with
  backoff up to 30 s.

The state this needs (the request queue with cursors, the last `seq` per
session, the last poll time) fits in a database or Redis, so any instance
can answer any call: a request created on one instance can be handed out by
the poll another instance is holding.

## Visibility: one machine, several hubs

A machine can link to several hubs (`agl connect` once per hub). Every
session records its `origin`: `local` (CLI, apps on the machine), or the URL
of the hub that created it.

| caller | sees and can drive |
|---|---|
| local clients | every session |
| a hub | the sessions it created, plus `local` ones if it was linked with `share_local` (the default; `agl connect --private` turns it off) |

Sessions a hub may not see do not exist for it: methods answer `no session`,
`session.list` leaves them out, and their events are never sent on that
hub's link. Sessions a hub creates without naming `access` get the link's
access (default `ask`).

## Local-only methods

| method | params | result |
|---|---|---|
| `subscribe` / `unsubscribe` | `{session_id}` or `{all: true}` | Start or stop receiving that session's events. `session.prompt` subscribes you automatically. |
| `daemon.shutdown` | – | Stops the daemon and every agent process. |
| `hub.reload` | – | Re-reads `config.json` and reconciles the hub links. |
| `gc.run` | `{idle?}` | `{suspended[], removed[]}` |

## Methods

| method | params | result / notes |
|---|---|---|
| `ping` | – | `{pong, version, protocol}` |
| `machine.info` | – | `{machine_id, hostname, os, arch, version, protocol, agents[], home, started_at}`. `home` is the user's home directory; `started_at` (unix ms) changes when the daemon restarts, which ends every running turn: a hub that sees a new value in `hello` can fail the turns it was waiting for. Both are absent before agl 0.3.0. |
| `agents.list` | `{check_updates?}` | `AgentInfo[]`: `{id, name, protocol, available, path, version, features[], status, desktop_app?, install?, latest_version?, update?}`. See [Agent discovery](#agent-discovery). |
| `agents.models` | `{agent}` | `{agent, models: [{id, name, description?, default?, efforts?[]}]}`. Never starts a conversation the user would see. |
| `daemon.status` | – | Version, protocol, pid, port, `hubs: [{url, status, access, share_local}]` (a hub sees only its own entry), visible sessions, live processes, defaults |
| `session.create` | `{agent, cwd?, model?, effort?, access?, name?, resume?, warm?}` | `SessionMeta`. Details: |
| | | `name`: if a session with that name exists (and is visible to the caller), it is reused. |
| | | `access`: when omitted, the config's `default_access` for local callers, the link's `access` (default `ask`) for a hub. |
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
| `fs.read` | `{path}` | `{content_base64, size}`. `path` is absolute or `~/…`; up to 100 MB. |
| `fs.write` | `{path, content_base64}` | `{size}`. Creates parent directories and replaces the file atomically. |
| `exec` | `{argv, cwd?, timeout_ms?, env?, stdin?}` | `{stdout, stderr, exit_code, timed_out, truncated}`. Runs `argv` without a shell, with the agents' environment (login-shell PATH), in its own process group; on timeout (default 10 min, max 30) the whole tree is killed. Output is kept up to 16 MiB per stream. |

`fs.*` and `exec` exist from agl 0.3.0. A hub gets them only as far as its
link's access allows, since nobody is asked first: any link may `fs.read`,
`edit` and `full` links may `fs.write`, and only `full` links may `exec`.

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
  "model": "haiku", "current_model": "claude-haiku-5-5", "effort": null, "access": "ask",
  "native_id": "6f0c…", "origin": "local",
  "status": "idle", "created_at": 1792559001027, "last_active_at": 1792559009456,
  "turns": 3, "queued": 0, "live": true, "pending_approvals": 0,
  "features": ["warm", "approvals", "steer", "interrupt", "models", "effort"]
}
```

`model` is what was asked for (null = the agent's default); `current_model`
is what the runtime reported running. `origin` is `local` or a hub URL.

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
| `text_delta` | `text`, `phase?` | Append in order. `"\n\n"` separates the agent's messages. `phase: "commentary"` (Codex, from agl 0.3.1) marks an interim update the agent gives while it keeps working, e.g. what it will do first; show it as progress, not as part of the answer. |
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

## Agent discovery

How to use this in a product: [discovery.md](discovery.md).

`agents.list` (or `agl agents --json`, which needs no daemon) reports every built-in and custom agent:

| field | meaning |
|---|---|
| `available`, `path`, `version` | The executable agl runs (login-shell PATH; `AGL_<ID>_BIN` overrides) and the first line of `--version` |
| `status` | `ready`, `desktop-only` (no CLI, but its macOS desktop app is installed), `not-found` |
| `desktop_app` | The desktop app that ships the agent, e.g. `/Applications/Cursor.app` |
| `install.source` | How the found binary was installed: `npm` (with `package`, `prefix`), `bun`, `brew`, `native` (self-updating install), `app-bundle` (the Codex inside Codex.app/ChatGPT.app, updated with the app), `unknown` |
| `install.install_command` / `update_command` | A shell command for a missing / installed agent, e.g. `npm install --global --prefix '<prefix>' @openai/codex@latest`. Show it to the user to confirm; agl never runs it. |
| `install.guide` | The agent's install docs |
| `latest_version`, `update` | Only with `check_updates: true` (`agl agents --check-updates`): the newest published version (npm, GitHub releases, the vendor's channel) and `available`, `current` or `unknown`. Unparseable versions are `unknown`, never `current`. Takes a few seconds and goes through the user's proxy. |

Custom agents carry no `install` or update fields. Detection only reads; nothing is installed or launched other than `<agent> --version`.

## Configuration (`$AGL_HOME/config.json`)

```json
{
  "port": 7799, "default_access": "full", "max_live": 8,
  "idle_timeout_secs": 600, "session_ttl_secs": 604800,
  "approval_timeout_secs": 600, "stall_timeout_secs": 120, "turn_timeout_secs": 0,
  "tool_output_limit": 16384, "history_limit": 2000, "history_image_limit": 16,
  "custom_agents": [{"id": "goose", "command": "goose", "args": ["acp"], "protocol": "acp"}],
  "hubs": [{"url": "https://app.example.com/agl", "token": "aglm_…", "access": "ask", "share_local": true}]
}
```

`hubs` is written by `agl connect` / `agl disconnect`; the single
`hub_url` / `hub_token` fields of agl ≤0.1 are migrated into it on load.

Environment variables:

| variable | effect |
|---|---|
| `AGL_HOME` | Data directory (default `~/.agl`) |
| `AGL_PORT` | Daemon port |
| `AGL_<ID>_BIN` | Executable for agent `<ID>` |
| `AGL_LOCAL_TOKEN` | Fixed local token |
| `AGL_NO_LOGIN_SHELL` | Do not import the login shell's environment |
| `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY`, `NO_PROXY` | The hub link tunnels through an `http://[user:pass@]host:port` proxy (CONNECT). These are read from the process or the login shell, so a launchd/systemd daemon uses the user's proxy too. localhost is never proxied. |
