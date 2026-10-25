---
name: agl-integration
description: Integrate agl (AgentLink) into a product so it can drive local coding agents (Claude Code, Codex, Gemini, Kimi, OpenCode, Grok, Cursor, …) with persistent warm sessions, approvals, steering, model lists and GC. Use when wiring a desktop app (Electron, Tauri, GPUI/native), a web product that reaches users' machines, or an existing agent runtime/daemon to local agents through agl, or when replacing hand-written `claude -p` / `codex exec` / ACP plumbing with agl.
---

# Integrating agl

agl is a single binary daemon. It owns one long-lived process per agent
session and exposes a single JSON protocol for all of them. The protocol
carries sessions, turns, streamed events, permission requests, steering,
interrupts, model lists and history. It speaks each agent's first-party
interface:
- Claude Code: stream-json plus its control protocol
- Codex: `app-server` JSON-RPC
- Gemini, Kimi, OpenCode, Grok, Droid and others: ACP
- cursor-agent and pi: one-shot JSON

Your product never parses agent output itself.

## 1. Pick the integration shape

| Product | Where agl runs | Who talks to it | Read |
|---|---|---|---|
| Desktop app (Electron, native, Swift, …), agents only used while the app is open | **Sidecar**: bundled binary, `agl daemon --port 0 --parent-pid <app pid>` with the app's own `AGL_HOME` | App main process over `ws://127.0.0.1` | [desktop-app.md](references/desktop-app.md) |
| Rust desktop app (Tauri, GPUI) | **In-process**: depend on the `agl` crate and use `agl::session::Manager` | Rust code directly; no port, no token | [desktop-app.md](references/desktop-app.md#rust-in-process) |
| Desktop app that must stay reachable when closed, or that shares sessions with the user's terminal | **Shared daemon** in `~/.agl` with autostart (`agl service install`) | App over localhost; the web through a hub | [desktop-app.md](references/desktop-app.md#shared-daemon) |
| Web product that uses agents on users' machines | **Installed daemon**: `curl …/install.sh?code=… \| sh`; the machine dials out to your server | Your backend over the machine link; browsers through your backend | [web-product.md](references/web-product.md) |
| Product that only needs the list of installed agents (an "agents" settings screen) | Nowhere persistent: run `agl agents --json` | Any process that can spawn a binary | [discovery.md](references/discovery.md) |
| Product that already has its own runtime or daemon (pairing, queue, transport) | **Execution engine** under your runtime: keep your pairing and transport, replace the per-CLI runners with agl calls | Your runtime process over localhost | [existing-runtime.md](references/existing-runtime.md) |

When unsure, ask one question: *must the agent be reachable when the user's
app is closed?*
- Yes: use an installed or shared daemon.
- No: use a sidecar.

## 2. Universal workflow

1. **Get a client.**
   - TypeScript: copy `clients/typescript/agl.ts` from the agl repo. It has zero
     dependencies and works in Node, Electron main and browsers.
   - Any other language: the protocol is plain JSON over WebSocket. See
     [protocol.md](references/protocol.md).
2. **Discover.** `agents.list` (or `agl agents --json`, no daemon needed)
   returns each agent's availability, version, `features`, install source and
   install/update command, and with `check_updates` the latest version. See
   [discovery.md](references/discovery.md). `agents.models {agent}` returns
   model lists. Base your UI on `features` (`steer`, `approvals`, …). Do not
   hardcode per-agent behaviour or write your own detection.
3. **Create or reuse a session.**
   - Call `session.create {agent, cwd, model?, access, name?, warm: true}`.
   - Use a stable `name` such as `"<your-thread-id>"` so the same thread maps to
     the same agl session, and survives restarts.
   - `warm: true` hides process startup.
4. **Run turns.**
   - Call `session.prompt {session_id, text, mode}`.
   - Render events from `turn_started` to `turn_ended`.
   - Use `mode: "steer"` when the user sends while a turn runs (if `features`
     includes `steer`).
   - Use `session.abort` for Stop.
5. **Approvals.**
   - With `access: "ask"` or `"edit"`, show `permission_request` events and
     answer them with `session.approve`.
   - Unanswered requests are denied after `approval_timeout_secs`, so a turn
     never hangs.
6. **Resume UI state.** On reconnect or reload, call
   `session.history {after_seq}`, then keep consuming live events.
7. **Settings.** `session.configure {model|effort|access}` keeps the
   conversation: agl restarts the process with a native resume.

## 3. Rules that prevent the classic bugs

- **Do not parse agent CLIs yourself, and do not spawn `claude -p` per turn.**
  Cold starts cost seconds per turn (see `docs/comparison.md` in the agl repo),
  and you lose approvals and steering.
- **Pick `access` deliberately.**
  - `full` runs tools without asking. That is fine for a local single-user tool
    and dangerous for anything remote.
  - Use `ask` or `edit` when a human can answer, and `read_only` for planners.
- **One session = one conversation.** Never share a session across users. Never
  run two products' threads in one session.
- **The local token is a credential.** Read it from `daemon.json` in a
  privileged process (Electron main, a Rust backend). Do not ship it to remote
  pages.
- **Use the event `seq`, not timestamps,** for ordering and dedup.
- **Treat `text_delta` as provisional** until `turn_ended`. On `aborted`, the
  text is partial.
- **Do not retry failed turns blindly**: tools may already have changed files.
  agl itself retries only before any activity (credential fallback, lost
  resume id).
- **GUI-launched processes have a bare PATH.** agl imports the login shell's
  PATH and API-key variables itself. Do not "fix" PATH in your app; pass
  `AGL_<ID>_BIN` only for unusual installs.

## 4. Verify before you ship

Each step has a pass condition:

- [ ] `agents.list` shows the user's agents as `available`.
- [ ] Two prompts in one session: the second turn's `timing.warm` is `true` and
      the agent remembers the first.
- [ ] Stop during a long turn gives `turn_ended.outcome == "aborted"` within a
      few seconds.
- [ ] With `access: "ask"`: a write-tool prompt produces `permission_request`.
      Allow creates the file; deny does not.
- [ ] Kill your app or the daemon mid-session, start again, send a prompt: the
      conversation continues (native resume) and `session.history` still has
      the transcript.
- [ ] Web shape only: the install command works once (pairing code reuse is
      rejected), and revoking a machine stops it reconnecting.
