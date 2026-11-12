# Built-in local tools (agl 0.3.14+)

Use this path when your application owns the model calls and agent loop and
needs to operate on a user's machine. Installing agl is sufficient: no coding
agent, local model, or local model API key is required. Individual commands
still depend on the programs installed on that machine.

The application owns prompts, context, planning, tool selection and retries.
agl provides four tools: `read`, `write`, `edit`, `exec`. Use shell commands
for listing, searching, moving, archiving or Git operations. agl never calls
a model on this path. Existing `session.*` APIs still drive installed agents.

## Discover and call

Connect over the local socket or either hub transport as usual. A machine's
`tools` field advertises the built-in tool names even if `agents` has no
available entries. Call `tools.list {}` for schemas filtered by the link's
current permissions:

```ts
const machine = hub.forMachine(machineId); // or await AglClient.local()
const definitions = await machine.tools();
// Map each {name, description, input_schema} to your model provider's tool
// definition format. Validate/parse its tool arguments, then execute:
const result = await machine.callTool("read", {
  path: "/absolute/path/to/file.txt", offset: 1, limit: 100,
});
// Return JSON.stringify(result) as the result for that model tool call ID.
```

`tools.call {name, arguments}` returns the tool result directly. Wire errors
are normal agl response `error` strings: return them as failed tool results
to the model. Unknown names and invalid parameters fail without a model call.

| Tool | Arguments | Result |
|---|---|---|
| `read` | `path, offset?, limit?` | UTF-8 `content`, file `size` in bytes, `offset`, `lines`, `next_offset`, `truncated` |
| `write` | `path, content` | `{size}` in bytes |
| `edit` | `path, old_text, new_text` | `{size, replacements: 1}` |
| `exec` | `action?: start/read/cancel` and the corresponding execution parameters below | Execution snapshot on start/read; `{execution_id, ok}` on cancel |

Paths are absolute or begin with `~/`. Read offsets are **1-based lines**;
default offset 1, limit 2000, maximum limit 10000. Text output is capped at
64 KiB on complete line boundaries. Continue at `next_offset` until it is
null. A single line over 64 KiB fails; use binary `fs.read` or a command to
extract the needed bytes. Invalid UTF-8 fails; binary data remains available
through `fs.read` with its default base64 encoding.

Writes accept up to 100 MiB, create missing parents, replace atomically and
preserve an existing file's mode. Edits accept UTF-8 files up to 100 MiB and
require a nonempty exact match occurring **once**, including overlapping
matches. Missing or ambiguous matches leave the file untouched. Edits follow symlinks while preserving the link itself. Mutations
through one runtime are serialized; external editors and commands can still
change files concurrently. These APIs do not provide a filesystem sandbox.

## Commands and incremental output

The model-facing `exec` tool defaults to `action: "start"`. Supply exactly
one of `argv` (runs directly, no shell) or `command` (runs `/bin/sh -c`).
Optional launch parameters: `cwd`, `env`, `stdin`, `timeout_ms`, `app_name`,
`execution_id`. The environment uses agl's login-shell PATH, including
`extra_paths`. `app_name` defaults to the hub link's product identity.

Start returns a snapshot after waiting up to `wait_ms` (default 1000,
maximum 30000) for output or completion. A running status is a normal result;
the cloud model or application should read again. Subsequent model calls use
`action: "read"` or `"cancel"` with `execution_id`.

Applications can use the corresponding direct APIs and typed client methods:

```ts
// Choose and retain the ID BEFORE sending. If the response is lost, retry
// startExec with the same ID and exactly the same launch arguments.
const input = {
  execution_id: crypto.randomUUID(),
  command: "find . -maxdepth 2 -type f",
  cwd: "/absolute/path/to/project",
};
const { execution_id } = await machine.startExec(input);
let stdout_offset = 0, stderr_offset = 0;
for (;;) {
  const r = await machine.readExec(execution_id, {
    stdout_offset, stderr_offset, wait_ms: 1000,
  });
  console.log(r.stdout);
  console.error(r.stderr);
  stdout_offset = r.stdout_offset;
  stderr_offset = r.stderr_offset;
  if (r.status === "completed" && !r.has_more) break;
}
// To stop: await machine.cancelExec(execution_id);
```

- `exec.start` takes the launch parameters above and immediately returns
  `{execution_id}`. It requires no event subscription.
- `exec.read` takes `execution_id, stdout_offset?, stderr_offset?, limit?,
  wait_ms?`. Default cursors 0, limit 65536 **bytes per stream** (4..65536),
  default wait 0, maximum wait 30000 ms. It returns `execution_id, status,
  stdout, stderr, stdout_offset, stderr_offset, has_more, exit_code,
  timed_out, cancelled, truncated, error`. Cursors address the retained,
  UTF-8-decoded output, and always end at character boundaries. A completed
  command can still have unread output (`has_more`). `exit_code` is null
  while running, or when no exit status exists (timeout/cancel/wait error).
  Signal exits report -1. Inspect `error` for output/wait errors.
- `exec.cancel` acknowledges a stop request. Poll until completed to confirm
  cleanup. Cancellation and timeout kill the process tree/group, and cleanup
  is bounded even if a descendant retains an output pipe.
- The original blocking `exec` remains compatible and now also accepts
  `command` and an optional `execution_id`; it returns aggregate output.

Output is retained up to 16 MiB per stream, with `truncated` when more is
discarded; the pipes keep draining. At most four commands run at once per
runtime. Completed results are retained for up to ten minutes, with earlier
output eviction as newer commands start (at most eight buffered executions,
including running commands). Eviction returns an explicit expired-result
error; it never silently reruns the command. IDs remain as deduplication
tombstones until ten minutes after completion, at most 1024 tracked IDs.
Results and IDs live in daemon memory and **do not survive a daemon restart**.
Never retry a mutating start blindly across a restart or after ID expiry.
File write/edit requests have no execution-ID deduplication.

Execution IDs are scoped to the calling hub. Hubs cannot read or cancel
each other's or local executions, even with `share_local`. A local
administrator may inspect/cancel a hub's execution when its ID is unique.
Commands survive transport reconnects to the same daemon; keep the ID and
cursors and continue reading. Running commands count toward daemon busy and
update checks, and are cancelled on orderly daemon shutdown. Crash leftovers
use the existing process reaper when their tracked root is still alive.

## Permissions

Without approvals (below), the link policy applies to both `tools.call` and
the direct APIs:

| Caller | Tools exposed by `tools.list` |
|---|---|
| Local client | All four |
| Hub `ask` / `read_only` | `read` |
| Hub `edit` | `read`, `write`, `edit` |
| Hub `full` | All four |

Built-in tools do not emit `session.permission_request` events or create
agent sessions. A denied call fails immediately, unless the hub opted in to
approvals. Configure a hub's access
locally with `agl connect … --access full` when command execution is wanted;
the cloud cannot elevate its own link. Permissions are connection-wide,
without per-directory scoping. Arbitrary command access also permits file
access through those commands. macOS system privacy consent still applies;
file APIs run in the daemon, while named command launches use `app_name`.

## Approvals

From agl 0.3.15, a hub may pass `approvals: true` to `tools.list` and
`tools.call`. Then a call the link doesn't allow outright, but that the
person could allow, is held instead of refused:

| Access | Runs at once | Waits for the person |
|---|---|---|
| `read_only` | `read` | – (nothing else is offered) |
| `ask` | `read`, read-only commands | `write`, `edit`, other commands |
| `edit` | `read`, `write`, `edit`, read-only commands | other commands |
| `full` | all four | – |

Read-only commands are decided by agl, never by the hub or the model: a
plain command or `|` pipeline of `ls`, `cat`, `head`, `tail`, `wc`, `du`,
`df`, `stat`, `find` (no `-exec`/`-delete`/`-fprint…`), `grep`/`rg`, `sort`
(no `-o`), `mdfind`, `git status/log/show/diff/…` and a few more, with no
`env`, no redirection except `2>/dev/null` / `2>&1`, and no `;`, `&&`, `$`,
backticks or subshells. Anything else waits.

A held call answers at once:

```json
{"status": "approval_required", "approval_id": "…", "tool": "exec",
 "title": "Run: rm ~/Downloads/old.dmg", "options": ["allow", "allow_session", "deny"],
 "expires_at": 1762930000000}
```

Show `title` to the person, then answer with
`tools.approve {approval_id, decision}`: `allow` and `allow_session` run the
call and return its normal result; `deny` fails it with an error to hand to
the model. `allow_session` also lets later calls of the same tool with the
same `session` (any string the hub passes, e.g. its conversation id) run
without asking, for an hour; `session` empty means no such grants. Held calls
expire after ten minutes. Only the hub that made the call may answer it, and
the link's access is checked again when it runs, so a link lowered meanwhile
still refuses.

The answer travels through the hub, as `session.approve` does for agent
sessions: agl trusts the hub to relay the person's decision. What the hub
can never do is go above the link's access; `read_only` stays read-only.

```ts
const tools = await machine.tools({ approvals: true });
let r = await machine.callTool("exec", { command: "rm old.dmg", cwd: "~/Downloads" }, { approvals: true, session: chatId });
if (isApprovalRequired(r)) {
  const ok = await askThePerson(r.title);
  r = await machine.approveTool(r.approval_id, ok ? "allow" : "deny");
}
```

## Rust embedding

Keep one `agl::machine::Runtime::default()` for the lifetime of your host,
and call its `dispatch(&cfg, &origin, method, params).await`. Use the same
`Origin` for every request from one application. Call
`runtime.executions.shutdown().await` before the host exits. The older
stateless `agl::machine::dispatch` remains available for file operations
and one-shot blocking `exec`; use `Runtime` for discovery, persistent
execution IDs, output polling and cancellation.
