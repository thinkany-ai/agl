# Discovering local agents

agl knows where the coding agents on a machine live, which version is
installed, how each one was installed, and the command that installs or
updates it. A product can use this for an "installed agents" screen without
writing any detection of its own, and whether or not it also uses agl to
run the agents.

Detection only reads: it looks up executables on the login shell's PATH,
runs `<agent> --version`, and checks for desktop apps. It never installs,
updates or starts an agent.

## Getting the list

Pick one of the following. All of them return the same `AgentInfo[]`.

### CLI, no daemon (any language)

```bash
agl agents --json                    # local only, 1–2s
agl agents --json --check-updates    # also the newest published versions (network, a few seconds)
```

Run it from a privileged process (an Electron main process, a backend, a
Swift `Process`) and parse stdout. The exit code is 0 even when no agent is
installed. It needs no `daemon.json` or token, so it is the simplest option
when the product only needs the list. `AGL_HOME` (default `~/.agl`) must be
writable: agl creates it on first use.

A bundled binary works too: `<app>/Contents/Resources/agl agents --json`.
Set `AGL_HOME` if the product keeps its own agl config, where
`custom_agents` are added to the list.

### Daemon method `agents.list`

```json
{"type": "request", "id": "1", "method": "agents.list", "params": {"check_updates": false}}
```

Use this when the product already talks to a daemon, whether a sidecar, the
shared one, or one reached through a hub. `machine.info`, and the `hello`
frame a machine sends its hub, carry the same list without update checks.

### TypeScript client

```ts
import { AglClient } from "./agl";
const agl = await AglClient.local();                 // Node/Electron main: reads ~/.agl/daemon.json
const agents = await agl.agents();                   // local only
const withUpdates = await agl.agents({ check_updates: true });
```

### Rust crate

```rust
let cfg = agl::config::Config::load()?;               // honours AGL_HOME
let agents = agl::agents::detect(&cfg, /* check_updates */ false).await;
```

## `AgentInfo`

```json
{
  "id": "codex",
  "name": "Codex",
  "protocol": "codex",
  "available": true,
  "path": "/Users/you/code/node/npm_global/bin/codex",
  "version": "codex-cli 0.164.0",
  "features": ["warm", "approvals", "steer", "interrupt", "models", "effort"],
  "status": "ready",
  "install": {
    "source": "npm",
    "package": "@openai/codex",
    "prefix": "/Users/you/code/node/npm_global",
    "update_command": "npm install --global --prefix '/Users/you/code/node/npm_global' @openai/codex@latest",
    "guide": "https://github.com/openai/codex"
  },
  "latest_version": "0.164.0",
  "update": "current"
}
```

A missing agent:

```json
{
  "id": "droid", "name": "Factory Droid", "protocol": "acp", "available": false,
  "features": ["warm", "approvals", "interrupt", "models"],
  "status": "not-found",
  "install": {
    "install_command": "curl -fsSL https://app.factory.ai/cli | sh",
    "guide": "https://docs.factory.ai/cli/getting-started/quickstart"
  }
}
```

| field | meaning |
|---|---|
| `id` | Stable key, e.g. `claude`, `codex`, `gemini`, `opencode`, `kimi`, `grok`, `droid`, `openclaw`, `hermes`, `omp`, `cursor`, `pi`, plus `custom_agents` ids. Pass it as `agent` to `session.create` and `agents.models`. |
| `name` | Display name |
| `protocol` | How agl drives it: `claude`, `codex`, `acp`, `oneshot` |
| `available` | agl found a runnable executable. Only these can be used in `session.create`. |
| `path` | That executable (`AGL_<ID>_BIN` overrides the lookup) |
| `version` | First line of `--version`, as printed (`codex-cli 0.164.0`, `2.1.310 (Claude Code)`). Absent if the CLI does not answer within 8s. |
| `features` | What agl can do with it: `warm`, `approvals`, `steer`, `interrupt`, `models`, `effort`, `resume`. Base your UI on these. |
| `status` | `ready`, `desktop-only` (no CLI, but its macOS desktop app is installed, so suggest installing the CLI), or `not-found`. Absent from agl 0.2.0 and earlier; derive it from `available` there. |
| `desktop_app` | The desktop app that ships the agent, e.g. `/Applications/Cursor.app` |
| `install.source` | How the found binary was installed. `npm` comes with `package` and `prefix` (the global prefix it lives in, which may not be npm's default). The others are `bun`, `brew`, `native` (a standalone install that updates itself), `app-bundle` (the Codex inside Codex.app or ChatGPT.app, which updates with the app) and `unknown`. |
| `install.install_command` | For a missing agent: the official installer or `npm install -g …` |
| `install.update_command` | For an installed agent: the command that updates this installation in place, e.g. `'<brew>' upgrade gemini-cli` or `'<path>' update`. Absent for `app-bundle`. |
| `install.guide` | The agent's install docs |
| `latest_version` | Only with `check_updates`: the newest published version, from npm, GitHub releases or the vendor's channel. Absent when agl knows no source (kimi, droid, custom agents) or the lookup failed. |
| `update` | Only with `check_updates`, for installed agents: `available`, `current` or `unknown`. A version agl cannot parse or order is `unknown`, never `current`. |

Custom agents have no `install`, `desktop_app` or update fields. Every new
field is optional: ignore the ones you do not know, as for the rest of the
protocol.

## Building the "installed agents" screen

- List `status == "ready"` agents with `version` and `path`.
- Show an **Update** button when `update == "available"`, with
  `latest_version` in its tooltip. When `update == "unknown"`, say
  "version unconfirmed" instead of claiming the agent is current.
- For a `desktop-only` or `not-found` agent, show **Install** with
  `install.install_command` and link `install.guide`.
- **Show the command and let the user confirm it.** Then run it in a
  terminal, or in a shell that has the user's PATH. agl never runs it. After
  it finishes, list the agents again.
- Check for updates on demand (a "Detect" button) or at most every few
  hours, not on every render: each check makes one HTTPS request per agent,
  through the user's proxy (`HTTPS_PROXY` from the login shell).

## Sandboxed apps (Mac App Store)

A sandboxed process cannot see `~/.local/bin`, npm prefixes or `~/.agl`, and
any helper it launches inherits the sandbox. Running a bundled
`agl agents --json` from a sandboxed app therefore finds nothing useful, and
agents it starts cannot reach the user's files. Use one of these instead:
- ship the app outside the sandbox (Developer ID) and use agl as a sidecar;
- talk to an agl daemon the user installed (`curl …/install.sh | sh`) over
  `ws://127.0.0.1`. The app has to obtain that daemon's port and token
  through a channel the sandbox allows, since it cannot read
  `~/.agl/daemon.json`;
- go through your server: link the machine to your hub
  ([web-product.md](web-product.md)) and read `machine.info` there.
