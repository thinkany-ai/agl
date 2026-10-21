# agl — AgentLink

English | [简体中文](README.zh-CN.md)

agl is a single Rust binary (about 4 MB) that drives the coding agents installed on a machine: Claude Code, Codex, Gemini, Kimi, OpenCode, Grok, Cursor and more.

- **Run agents well:** persistent warm sessions, concurrency and queueing, approvals, mid-turn steering, interrupts, model lists and automatic garbage collection.
- **Reach them from the web:** one `curl … | sh` links any machine to your web app, so the web page and the agents on that machine talk both ways.

The design draws on [botiverse/oar](https://github.com/botiverse/oar) and on the local runtimes of two existing products, douchat and Fleet. For how the integration options compare, with measured numbers, see [docs/comparison.md](docs/comparison.md) (in Chinese).

```
 browser ──> your backend / agl hub <──wss (machine dials out)── agl daemon ─┬─ claude   (stream-json + control protocol)
 terminal / desktop app / runtime ──ws://127.0.0.1───────────────────────────┤─ codex    (app-server JSON-RPC)
 Rust app ──in-process agl::session::Manager─────────────────────────────────┤─ gemini · kimi · opencode · grok · … (ACP)
                                                                             └─ cursor · pi (one process per turn, native resume)
```

## Features

| | |
|---|---|
| **One protocol, 11+ agents** | Each agent is driven through its best first-party interface. Any ACP-speaking CLI can be added through config. |
| **Warm sessions, reused context** | The agent process stays alive between turns. After it is suspended, or after a daemon restart, native resume continues the same conversation. Measured: Claude's time to first token drops from about 6s to about 1s per turn. |
| **Reusable sessions** | Sessions can be reused by name (`-s name`). The CLI, desktop apps and the web share the same sessions, so one can pick up where another left off. History is persisted, with a resume cursor. |
| **Concurrency, queueing, steering** | Different sessions run in parallel. Within a session, prompts either queue or are injected into the running turn (claude, codex). |
| **Approvals** | Four access levels: `full`, `edit`, `ask`, `read_only`. Permission requests from claude, codex and ACP agents all arrive as `permission_request` events, and are denied automatically if nobody answers in time. |
| **Automatic GC** | A hard cap on live processes (least recently used idle sessions are evicted first). Idle processes are suspended and expired sessions removed. Processes orphaned by a crash are reaped on the next start. |
| **Reliability** | Heartbeats and stall detection. A turn that hasn't stopped 10s after an interrupt is killed. Claude falls back to its own login when an exported API key is rejected. PATH and API keys are imported from the login shell. |
| **Remote linking** | One-time pairing codes and a per-machine token that can be revoked. The installer is HTTPS-only, verifies a sha256 checksum, and sets up launchd/systemd autostart. |
| **Integration options** | A local WebSocket API, a zero-dependency TypeScript client, a Rust crate, and an integration skill for coding agents. |

## Install

```bash
# From source (needs a recent stable Rust)
cargo install --git https://github.com/thinkany-ai/agl

# Or clone and build
git clone https://github.com/thinkany-ai/agl.git && cd agl
cargo build --release          # binary: target/release/agl
```

Supported on macOS (Apple silicon and Intel) and Linux (x64 and arm64). Windows is not supported yet.

## CLI

```bash
agl agents                                   # detect installed agents and what each supports
agl models codex                             # list models (does not create a conversation)
agl run claude "What does this repo do?"     # one turn (the daemon starts in the background)
agl run claude -s proj --access ask "Fix the failing test"   # named session; ask mode prompts in the terminal
agl send proj "Run the tests again"          # continue a session
agl send proj --steer "Also fix lint"        # inject into the running turn
agl chat kimi -s k                           # interactive chat (ACP agent)
agl configure proj -m opus --access edit     # change model or access, keep the conversation
agl approve proj <request_id> allow          # answer an approval from another terminal
agl sessions | history proj | attach         # list / transcript / follow all sessions live
agl abort proj | close proj | gc --idle
agl status | start | stop
```

## Integrating agl into your product

There is a skill for coding agents in [skills/agl-integration](skills/agl-integration/SKILL.md). Copy that directory into `~/.claude/skills/` (or your agent's skills directory), then ask your agent to "integrate local agents with agl".

How each kind of product uses agl:

| Product | How | Guide |
|---|---|---|
| Desktop app (Electron, native), agents only needed while it's open | Bundle agl as a sidecar: `agl daemon --port 0 --parent-pid <pid>` with its own `AGL_HOME` | [desktop-app.md](skills/agl-integration/references/desktop-app.md) |
| Rust desktop app (Tauri, GPUI) | Link the `agl` crate and use `Manager` in-process | [desktop-app.md#rust-in-process](skills/agl-integration/references/desktop-app.md#rust-in-process) |
| Web product | The user runs `curl …/install.sh?code=… \| sh`; the machine dials out to your backend | [web-product.md](skills/agl-integration/references/web-product.md) |
| Existing runtime or daemon | Keep your own pairing and transport; replace only the execution layer with agl | [existing-runtime.md](skills/agl-integration/references/existing-runtime.md) |

The full wire protocol is in [protocol.md](skills/agl-integration/references/protocol.md). The client is [clients/typescript/agl.ts](clients/typescript/agl.ts).

### Reference hub

```bash
agl hub --listen 0.0.0.0:8787 --public-url https://hub.example.com
```

Open `https://hub.example.com/?token=<admin>` and click "连接电脑" (Connect computer; the hub UI is in Chinese). Each time, the hub generates an install command carrying a one-time pairing code. You can also copy setup instructions for an agent to run.

The hub web UI supports:
- creating sessions with a chosen agent, model and access level;
- streaming chat;
- approving permission requests in the browser;
- steering and stopping a running turn;
- showing time to first token and total time;
- revoking machines.

The hub serves `agl-<os>-<arch>` binaries and their `.sha256` files from `--dist <dir>`. The binary for the hub's own platform is served without any setup.

## Configuration

Settings live in `~/.agl/config.json`. All fields are listed in [protocol.md](skills/agl-integration/references/protocol.md#configuration-aglhomeconfigjson).

```json
{ "default_access": "full", "max_live": 8, "idle_timeout_secs": 600, "approval_timeout_secs": 600,
  "custom_agents": [{ "id": "goose", "command": "goose", "args": ["acp"], "protocol": "acp" }] }
```

Other files in `~/.agl/`:

| File | Contents |
|---|---|
| `daemon.json` | Port and local token (mode 0600) |
| `sessions.json` | Session list |
| `history/` | Per-session event history |
| `daemon.log` | Daemon log |

## Repository layout

| Path | Contents |
|---|---|
| `src/agents/` | Agent drivers: `claude.rs`, `codex.rs`, `acp.rs`, `oneshot.rs` |
| `src/session.rs` | Session manager: warm sessions, queueing, steering, approvals, GC, process budget |
| `src/daemon.rs`, `src/link.rs` | Local daemon and the machine link to a hub |
| `src/hub.rs`, `assets/` | Reference hub, web UI, install script |
| `clients/typescript/` | Zero-dependency TypeScript client |
| `skills/agl-integration/` | Integration skill for coding agents |
| `docs/` | Comparison and benchmarks |

## Security

- `full` access lets the agent run any command as you. For remote use, default to `ask` or `edit` and provide approvals in your UI.
- The local daemon listens on 127.0.0.1 only, and every connection must present the token from `daemon.json`.
- The hub stores only hashes of machine tokens, and each machine can be revoked individually. Pairing codes are single-use and expire after 10 minutes.

## License

[MIT](LICENSE) © 2026 ThinkAny, LLC
