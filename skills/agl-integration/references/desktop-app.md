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
  install dirs and nvm. If your app installs agent CLIs somewhere of its own,
  add that directory to `extra_paths` in `$AGL_HOME/config.json`, and after
  installing one call `env.reload` so `agents.list` shows it without a
  daemon restart (this also picks up PATH changes the installer made to the
  user's shell profile).
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
| "done" notification, thread preview, the answer you store | `turn_ended.text`: the final answer without interim commentary (agl 0.3.12+) |
| per-thread system prompt, CLI flags, "don't keep it in the agent's history" | `session.create {instructions, extra_args, ephemeral}` (agl 0.3.12+, local connections only; check the agent's `features`) |
| thread that must not be written to disk by agl | `session.create {history: false}` |

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

## Embedding and branding (agl 0.3.17+)

A product can ship agl under its own name and host the whole daemon in its
own process — hub links, sessions, built-in tools — instead of a sidecar or
the shared `~/.agl` daemon. Set a brand once, first thing in `main`, then use
the same APIs agl's own binary uses.

```rust
use agl::brand::{Brand, Product};

agl::brand::set(Brand {
    cli_name: "acme-link".into(),            // usage text and messages
    display_name: "Acme Link".into(),
    home: Some(dirs::home_dir().unwrap().join(".acme/link")), // AGL_HOME still wins
    service_label: "com.acme.link".into(),   // launchd label
    systemd_unit: "acme-link.service".into(),
    releases: None,                          // updated with the app: `update` / daemon.update refuse
    allowed_hubs: Some(vec!["https://app.acme.com".into()]), // prefixes; others are refused / not linked
    computer_helper: Some(helper_path),      // see below
    blocked_apps: vec!["com.acme.desktop".into()], // computer use never operates these
    product: Some(Product { name: "acme-desktop".into(), version: env!("CARGO_PKG_VERSION").into() }),
})?;
```

`Brand::default()` is agl itself; leave a field at its default to keep agl's
behaviour for it. `agl::brand::get()` reads it; `agl::brand::version()` is the
product's version (or agl's).

**Own command line.** A branded binary is agl's CLI under its name:

```rust
fn main() {
    agl::brand::set(brand()).unwrap();
    if let Some(code) = agl::app::hold_from_args() { std::process::exit(code) }
    std::process::exit(agl::cli::run());
}
```

`agl::cli::command()` is the clap definition (help pages, completions).
`service install` registers the brand's label/unit; `update` downloads
`<releases>/download/v<version>/<cli_name>-<os>-<arch>` (plus `.sha256`).

**Daemon in-process.** In a tokio app (Tauri):

```rust
let cfg = agl::config::Config::load()?;      // the brand's home
let d = agl::daemon::spawn_embedded(cfg, agl::daemon::DaemonOptions { port: Some(0), ..Default::default() }).await?;
// d.port / d.token: the local API, as for a separate daemon (also in daemon.json)
agl::link::connect(hub_url, agl::link::ConnectOptions {
    code: Some(code), access: Some("full".into()), private: true,
    transport: Some("http".into()), wait: Some(Duration::from_secs(15)), ..Default::default()
}).await?;
// on quit: check d.busy() (turns and executions it would end), then
d.shutdown().await;
```

`link::connect` / `link::disconnect` do what the `connect` / `disconnect`
commands do: validate, save the home's config, and make a daemon running there
pick it up. One daemon per home: `spawn_embedded` fails while another process,
or this one, serves the same home. The machine `hello` carries
`product: {name, version}` so a hub can tell which product a machine runs.

**Computer use with your own helper app.** By default the helper is agl
itself, started through a per-product launcher (`app_name`). With
`computer_helper`, agl starts that executable instead — for example
`Acme.app/Contents/Helpers/Acme Computer Use.app/Contents/MacOS/acme-computer-use`,
signed with your Developer ID — as `<helper> __computer --responsible`. Its
`main` is just:

```rust
fn main() { std::process::exit(agl::computer::helper_main()) }
```

`helper_main` re-executes itself responsible for itself, so macOS lists
Accessibility and Screen Recording under that helper app's name and keeps the
grants across updates (they follow its signature and bundle id). One helper
serves every hub; `app_name` is not used for computer use then, and
`computer.permissions` reports the helper app's name.

### Host-registered tools (agl 0.3.17+)

An embedding application can offer its own tools to hubs next to agl's
built-ins: implement `agl::machine::ToolProvider` and register it before the
daemon starts.

```rust
use agl::machine::{register_provider, CallContext, ToolDef, ToolProvider};
use agl::machine::provider::BoxFuture;

struct Acme;
impl ToolProvider for Acme {
    fn tools(&self) -> Vec<ToolDef> {
        vec![ToolDef { name: "acme_snapshot".into(), description: "…".into(), input_schema: serde_json::json!({"type": "object"}) }]
    }
    fn call<'a>(&'a self, name: &'a str, args: serde_json::Value, ctx: CallContext) -> BoxFuture<'a, anyhow::Result<serde_json::Value>> {
        Box::pin(async move { /* ctx.session, ctx.origin_key, ctx.images */ Ok(serde_json::json!({})) })
    }
}
register_provider(std::sync::Arc::new(Acme))?;
```

- **Listing.** The tools appear in `tools.list` after the built-ins and in the
  machine `hello` `tools`. `tools()` is called for every listing.
- **Who may call.** The computer-use rule: a `full` link or a local client
  may; an `ask` / `edit` link only with `approvals: true`, each tool waiting
  for the person once per `session` (`allow_session` keeps it); `read_only`
  never.
- **Calls.** `arguments` reach the provider as sent (keys the hub adds
  included; `_agl` is reserved). `CallContext` carries `session`, the link
  (`origin_key`, the hub URL, empty for a local client) and `images`. An error
  becomes the `tools.call` error, its message as is.
- **Images.** A result's `images: [{mime_type, data}]` (base64) is handled
  like computer-use screenshots: returned only with `images: true`, PUT to
  `image_upload` when given (`{uploaded: true, bytes}`; inline with an
  `image_upload_failed` warning when that fails), and `{unchanged: true}`
  when the same as the last image sent for that session and place. The place
  is the result's `image_key` (removed from the result), else the tool name.
- **Names** must not repeat a built-in tool (also computer use's, on any
  system) or another provider's tool: `register_provider` fails.

## Shared daemon

Use this when your app belongs to the same ecosystem as the user's terminal
`agl`, or when the machine must stay reachable from the web while the app is
closed.

1. Connect with `AglClient.local()`, which reads `~/.agl/daemon.json` (again
   before every reconnect: the daemon's port and token change when it
   restarts, e.g. after an update). Listen with `agl.onConnection(...)`: on
   `reconnected` with `restarted: true`, every turn you were waiting for is
   gone; fail it in your UI (`runTurn` already resolves `aborted` with
   `restarted: true`). See [Reconnects and daemon restarts](protocol.md#reconnects-and-daemon-restarts).
2. If the daemon is not running, start the bundled binary with `agl start` (or
   `agl service install` for autostart).
3. Prefix session names with your app id so they don't collide with terminal
   sessions.
4. Create sessions that must stay on this computer (private chats, anything
   the user did not mean to share with a linked web product) with
   `visibility: "local"`: no hub link sees them, even when the machine was
   linked with `share_local`.

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
