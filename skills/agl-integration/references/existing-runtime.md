# Products that already have a runtime

Many products already ship a local runtime: a daemon or desktop process that
pairs with a server, polls for work, and runs agent CLIs. Do **not** replace
the parts that are product-specific:
- pairing
- server transport (long-poll, relay)
- task leases
- result delivery

Replace only the **execution layer**: the code that spawns `claude`, `codex`,
`gemini` … and parses their output.

## Why replace the runner

| Typical hand-written runner | With agl |
|---|---|
| One CLI process per turn (`claude -p --resume`, `codex exec resume`): cold start, MCP and hook startup on every turn | One warm process per session; later turns skip startup |
| Final text only, or ad-hoc progress parsing per CLI | One event stream: deltas, tools, plan, reasoning, heartbeat, timing |
| No approvals (bypass or deny-all) | `permission_request` / `session.approve` for claude, codex and ACP agents |
| Stop = kill the process | Native interrupt, with a 10s fallback to killing the process tree |
| Per-CLI quirks in your code | Handled once in agl: login-shell PATH, API-key/login conflict, codex handshake serialization, orphan reaping, `active writer` retries |

## Migration recipe

1. **Run agl next to your runtime.** Bundle the binary. Start it as the
   runtime's sidecar: `agl daemon --port 0 --parent-pid <runtime pid>` with
   `AGL_HOME=<runtime home>/agl`. Read `{port, token}` from the first stdout
   line.
2. **Keep the native session ids you already store.** On the first turn after
   migration, call `session.create {agent, cwd, name: "<your session key>",
   resume: <stored native id>}`. agl continues the existing conversation
   (claude `--resume`, codex `thread/resume`, ACP `session/resume|load`).
3. **Turn handler.**

   ```ts
   const s = await agl.createSession({ agent: driver, cwd: workspace, model, name: sessionKey, resume: storedNativeId, access });
   const r = await agl.runTurn(s.id, prompt, {
     onEvent: (e) => progress.emit(mapEvent(e)),      // your existing progress channel
     onPermission: (p) => askOwnerOrPolicy(p),        // or omit with access "full"/"read_only"
     signal: cancelSignal,                            // your cancellation → session.abort
   });
   if (r.outcome !== "completed") throw new Error(r.error ?? r.outcome);
   return { content: r.text, sessionId: (await agl.session(s.id)).native_id, usage: r.usage };
   ```

4. **Map events onto your vocabulary**, for example:

   | agl event | your event |
   |---|---|
   | `text_delta` | `message_delta` |
   | `reasoning` | `thinking_delta` |
   | `tool_start` | `tool_start {id: call_id, name, args: input}` |
   | `tool_end` | `tool_end {id, output, isError: !ok}` |
   | `heartbeat` | `status {stage: stalled ? "stalled" : "working"}` |
   | `turn_ended.timing` | your timing metrics |

5. **Keep your other drivers.** Route the agents agl supports through agl and
   leave the rest on your existing runners. Check `agents.list` /
   `features`. You can also register extra ACP CLIs through `custom_agents` in
   agl's config instead of writing a runner.
6. **Remove** your own: process-group killing, PATH probing, per-CLI parsers,
   resume-id bookkeeping, idle eviction. agl does them (`max_live`,
   `idle_timeout_secs`).

## Things to keep in your runtime

- **Turn leases and the "never replay a turn that may have run tools"
  journal.** agl does not replay failed turns, but it also does not know your
  server's lease semantics.
- **Server-side session serialization.** agl serializes turns per session, but
  your server decides which session a message goes to.
- **Workspace layout and attachment handling.**
