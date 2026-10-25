# Web products

The user runs one command on their machine. agl installs, gets autostart, and
**dials out** to your server over WebSocket, so it works behind NAT with no
inbound port. Your backend then drives that machine's agents and relays events
to browsers.

```
browser ──(your API / SSE / WS)──> your backend <──wss /ws/machine── agl daemon ──> claude / codex / gemini / …
```

`agl hub` in the agl repo (`src/hub.rs`, about 450 lines, plus `assets/hub.html`)
is a complete reference implementation. Run it to see the flow, then port its
two endpoints into your stack.

## 1. Pairing (connect-a-computer dialog)

1. The user clicks "Connect computer". Your backend creates a **one-time
   pairing code**: random, 10 minutes, single use. Store only a hash if it is
   persisted.
2. Show two options:
   - Terminal: `curl -fL --progress-bar "https://you/agl/install.sh?code=<code>" | sh`
   - "Hand it to an agent": a paragraph that tells the user's Claude Code or
     Codex to run that command with normal user permissions (not inside a
     sandbox, not with sudo), then check `~/.agl/bin/agl status`.
3. `GET /install.sh?code=` serves `assets/install.sh` with two placeholders
   replaced:
   - `__HUB_URL__` becomes your base URL.
   - `__CONNECT_ARGS__` becomes `--code <code>`.
   - If the code is invalid, return a script that only prints an error and
     exits 1.
4. The script:
   - refuses root, and accepts only HTTPS (except localhost);
   - downloads `<base>/download/<os>-<arch>` and verifies it against
     `<base>/download/<os>-<arch>.sha256`;
   - keeps an agl already installed when it is newer than the download, since
     other web apps on the machine share it (`AGL_FORCE=1` replaces it anyway);
   - installs autostart (launchd, or systemd --user plus linger);
   - runs `agl connect <base> --code <code>`.
5. The machine connects to `/ws/machine?code=<code>&machine_id=<uuid>`.
   Validate and **consume** the code, upgrade the connection, and send
   `{"type":"paired","token":"<new machine token>"}`. Store
   `sha256(token)` against `(user, machine_id)` and close the connection.
6. The machine reconnects with `?token=<machine token>&machine_id=…` and sends
   `hello` (hostname, os, arch, version, agents). Mark it online.

Binaries:
- Every agl release on GitHub publishes `agl-<target>` and `agl-<target>.sha256`
  for `darwin-arm64`, `darwin-x64`, `linux-x64` and `linux-arm64` (Linux
  builds are static musl binaries).
- Easiest: make `GET <base>/download/<target>[.sha256]` redirect (302) to
  `https://github.com/thinkany-ai/agl/releases/latest/download/agl-<target>[.sha256]`.
  This is what `agl hub` does for anything not in its `--dist` directory.
- To pin a version or use your own CDN, serve the files yourself, or set
  `AGL_DOWNLOAD_URL` in the script.

## 2. The machine link

Check `hello.machine.protocol`: `1` today, `0` from agl ≤0.1. Within a
version everything new is additive, so ignore unknown fields and event kinds.

After `hello`, your backend:
- sends `{"type":"request","id","method","params"}` and receives
  `{"type":"response","id","result"|"error"}`;
- receives `{"type":"event","session_id","seq","ts","event"}` for the sessions
  **your hub may see**: the ones it created, plus the user's local ones
  (terminal, desktop apps) unless the machine was linked with
  `agl connect --private`. Sessions created by other web apps the same
  machine is linked to are never visible to you.

Over HTTP (`agl connect --transport http`), the same messages travel as
`POST /machine/send` and long-polled `POST /machine/poll` calls; see
[HTTP link](protocol.md#http-link) for the delivery rules.

What the daemon does on its side:
- sends a WebSocket ping every 20s, and reconnects if it hears nothing for 60s
  (over HTTP: polls continuously; a poll is held at most 25s);
- reconnects with exponential backoff up to 30s;
- on HTTP 401 or 403 at upgrade, stops retrying and reports
  `unauthorized … run agl connect again`. That is how revocation works:
  delete the token hash and close the socket.

Methods you will use most:
- `machine.info`, `agents.list`, `agents.models`
- `session.create`, `session.prompt`, `session.approve`, `session.abort`
- `session.history`, `session.configure`

See [protocol.md](protocol.md) for parameters and events.

## 3. Relaying to browsers

- Forward events to the browser that owns the session (SSE or WS). Keep `seq`
  so the browser can resume.
- On reload, the browser calls `session.history {session_id, after_seq}`
  through you, then resumes live events.
- Approvals: render `permission_request` as a card with the `options`. POST the
  decision, and your backend calls `session.approve`. agl auto-denies after
  `approval_timeout_secs`, so a closed tab cannot hang the agent.
- Steering: while a turn runs, send new input with `mode: "steer"` if the
  session has the `steer` feature (claude, codex); otherwise use `"queue"`.

## 4. Multi-instance backends

Simplest: link machines over HTTP (`--transport http` in the install
command's connect arguments). Every call is an ordinary request that any
instance can answer, with the request queue and cursors in your database
([HTTP link](protocol.md#http-link)).

Over WebSocket, a machine socket is held by **one** backend instance. With
several pods:
- **Sticky gateway**: a small stateful service holds the machine sockets, for
  example one Durable Object per machine on Cloudflare, or a dedicated
  WS-gateway deployment. API pods publish requests to it through
  Redis/NATS/HTTP and subscribe to its events.
- Do not route a machine's requests to a random pod. Responses come back on
  the socket, not on the pod that asked.

## 5. Security checklist

- Sessions your hub creates without `access` default to `ask`: every risky
  tool call becomes a `permission_request`, so build the approval UI first.
  Pass `access: "full"` only when the user opted into it, and say so in your
  UI. The machine owner can change the link's default with
  `agl connect <url> --code … --access edit|full|read_only`.
- Machine tokens:
  - one per machine, hashed at rest, revocable;
  - check ownership on every request (user ↔ machine_id);
  - never accept a machine token for UI routes.
- Use HTTPS/WSS only. Do not log URLs that carry tokens or codes.
- Treat everything coming from a machine (`hello`, events, tool output) as
  untrusted input: escape it when rendering, and cap the sizes you store.
