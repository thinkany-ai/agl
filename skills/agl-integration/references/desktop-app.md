# Desktop apps

## Sidecar (Electron, native, any language)

Ship the `agl` binary inside the app. Use one binary per platform, or `lipo` a
universal macOS binary. Start it from the main process and stop it with the
app.

```ts
// Electron main process
import { spawn } from "node:child_process";
import path from "node:path";
import { app } from "electron";
import { AglClient } from "./agl"; // clients/typescript/agl.ts

const aglBin = app.isPackaged
  ? path.join(process.resourcesPath, "bin", "agl")
  : path.join(__dirname, "../../bin/agl");
const aglHome = path.join(app.getPath("userData"), "agl");   // isolated from the user's ~/.agl

const daemon = spawn(aglBin, ["daemon", "--port", "0", "--parent-pid", String(process.pid)], {
  env: { ...process.env, AGL_HOME: aglHome },
  stdio: ["ignore", "pipe", "pipe"],
});
// The first stdout line is {"pid","port","token","version"}.
const info = await new Promise<any>((resolve, reject) => {
  daemon.stdout.once("data", (d) => resolve(JSON.parse(String(d).split("\n")[0])));
  daemon.once("exit", (c) => reject(new Error(`agl exited ${c}`)));
});
const agl = new AglClient(`ws://127.0.0.1:${info.port}/ws?token=${info.token}`);
await agl.connect();
app.on("before-quit", () => daemon.kill("SIGTERM"));  // agl stops every agent process tree
```

Why these flags:

| flag / setting | reason |
|---|---|
| `--port 0` | No collision with the user's own agl (7799) or with another app. |
| `--parent-pid` | If the app crashes, the daemon exits within about 2s and kills its agents. |
| own `AGL_HOME` | The app's sessions, history and config stay separate. |

Things you do **not** need to do:
- Resolve PATH. agl reads the login shell's PATH and API keys, plus ~25 common
  install dirs and nvm.
- Clean up orphans. agl kills agent process trees left by a crashed predecessor
  at the next start, after checking a process fingerprint first.

### Renderer access

Keep the token in the main process. Expose a narrow IPC surface instead:
`createSession`, `prompt`, `approve`, `abort`, and a subscription that forwards
events. If you must connect from the renderer, pass the token through
`contextBridge`. The daemon accepts Origin-bearing connections only with a
valid token.

### Mapping your threads

| Your concept | agl |
|---|---|
| thread / conversation id | `session.create {name: "<app>:<threadId>"}`. The same name reuses the session. |
| thread working folder | `cwd` |
| per-thread model / permission mode | `model`, `effort`, `access`. Change them later with `session.configure`; the conversation is kept. |
| "agent is typing" | `status` events and `heartbeat` (`stalled` after 60s without output) |
| stop button | `session.abort` |
| send while running | `session.prompt {mode: "steer"}` if the session has the `steer` feature, else `"queue"` |
| tool approval dialog | `permission_request` → `session.approve` |

### Packaging

| platform | what to do |
|---|---|
| macOS | Sign the sidecar with the app's identity and hardened runtime, or notarization fails. Tauri `externalBin` does this for you. In Electron, add it to `extraResources` and to the signing config. |
| Windows | Not supported yet: no process groups or service. |
| Linux | Ship `agl-linux-x64` / `agl-linux-arm64`. |

## Rust in-process

For Tauri, GPUI or any tokio app, skip the process entirely.

```toml
[dependencies]
agl = { git = "https://github.com/thinkany-ai/agl" }   # or a path dependency
```

```rust
use agl::{config::Config, session::{Manager, CreateParams}, protocol::AgentEvent};

let mut cfg = Config::load()?;                 // honours AGL_HOME
cfg.default_access = "ask".into();
let mgr = Manager::new(cfg);
mgr.load();                                    // restore saved sessions (suspended)
let mut events = mgr.bus.subscribe();          // broadcast::Receiver<EventEnvelope>

let s = mgr.create(CreateParams { agent: "claude".into(), cwd: Some(project), name: Some(thread_id), warm: true, ..Default::default() })?;
let turn = mgr.prompt(&s.id, text, Some("queue"), None)?;
let turn_id = turn["turn_id"].as_str().unwrap_or_default().to_string();
while let Ok(env) = events.recv().await {
    if env.session_id != s.id { continue }
    match env.event {
        AgentEvent::TextDelta { text } => ui.append(text),
        AgentEvent::PermissionRequest { request_id, .. } => { /* ask user */ mgr.approve(&s.id, &request_id, "allow").await?; }
        AgentEvent::TurnEnded { turn_id: t, .. } if t == turn_id => break,
        _ => {}
    }
}
```

A complete runnable version is `examples/embed.rs` in the agl repo
(`cargo run --example embed -- codex "hi"`).

Add a GC tick (`mgr.gc(false).await` every 30s) and call `mgr.shutdown_all()`
on exit. Both are part of the daemon's normal loop; when you embed the manager,
you run them yourself.

### GPUI / std-thread apps (e.g. dscode)

Run a tokio runtime on a background thread that owns the `Manager`. Bridge
each turn with a `futures::channel::mpsc` sender. Keep the existing
`start(...) -> (Run, Receiver<AgentEvent>)` signature and translate events:

| agl event | your event |
|---|---|
| `text_delta` | `TextDelta` |
| `reasoning` | `ThinkingDelta` |
| `tool_start` | `ToolStart` (derive title and path from `name` and `input` as before) |
| `tool_end` | `ToolEnd { failed: !ok }` |
| `turn_ended` | `Error` if not completed, then `Done` |
| `info.native_id` | `Session(id)` |

`Run::stop()` becomes `mgr.abort(id)`.

## Shared daemon

Use this when your app belongs to the same ecosystem as the user's terminal
`agl`, or when the machine must stay reachable from the web while the app is
closed.

1. Connect with `AglClient.local()`, which reads `~/.agl/daemon.json`.
2. If the daemon is not running, start the bundled binary with `agl start` (or
   `agl service install` for autostart).
3. Prefix session names with your app id so they don't collide with terminal
   sessions.

When the app also links the machine to your web product (the same link the
`curl … | sh` install makes, see [web-product.md](web-product.md)), let both
paths share one install so the machine shows up once:

- Copy the bundled binary to `~/.agl/bin/agl`, where the install script puts
  it, unless a newer one is already there (`agl --version`), and run
  `agl service install` from that path. The service then outlives app updates
  and keeps running when the app is closed or removed.
- Read `agl status --json` (works without a daemon): `machine_id`, whether the
  daemon runs and the service is installed, and each linked hub with its
  status. Your backend learns the same `machine_id` when the machine pairs, so
  the app can find "this computer" among the ones the backend lists.
- Link with `agl connect <hub url> --code <code>`, minting the code with the
  signed-in user's session. A hub URL is one link: when one product has several
  tenants (workspaces) that each pair the machine, give each tenant its own
  hub URL, e.g. `https://app.example.com/agl/w/<workspace>`.
- agl updates on its own release cycle, not only with the app:
  `agl update --check --json` says whether a newer release exists, and
  `agl update --if-idle --json` installs it unless a session is running a
  turn (`busy`), so an app can update it unattended. An app never puts back an
  older bundled binary over a newer one.
- Apps that run code outside a sandbox can do this; a Mac App Store app
  cannot, since every process it starts inherits its sandbox.

Sessions are then visible in `agl sessions`, `agl attach` and on the hub. The
process budget (`max_live`) is shared across every client.
