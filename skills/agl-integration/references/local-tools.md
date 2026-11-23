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

Computer tools have their own rules, see [Computer use](#computer-use-macos-agl-0317).

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

## Computer use (macOS, agl 0.3.17+)

Ten `computer_*` tools let the cloud model operate the Mac's apps the way
Codex's Computer Use does, with no coding agent installed: it reads a
window's accessibility tree and screenshot, then presses, types and scrolls
**in the background**, without taking over the person's pointer or focus.

**Off per link until the person turns it on (0.3.17).** A hub link lists and
may call the `computer_*` tools only when computer use is on for it:
`agl computer-use on [hub]`, or `agl connect … --computer-use` (the link's
`computer_use` in the config; `agl computer-use off` turns it off). A hub
cannot turn it on; while it is off, `hello` `tools` and `tools.list` leave
the tools out and a call answers `computer use is off for this link`. A
product embedding agl may default its own links to on
(`Brand::computer_use_default`). Local clients are not affected. Then the
access rule below applies.

| Tool | Arguments (besides `app`) | Does |
|---|---|---|
| `computer_list_apps` | – (no `app`) | `{apps: [{bundle_id, name, pid, windows: [{id, title, focused}], blocked}]}` |
| `computer_get_state` | `window?, screenshot?, max_elements?` | The state: `state_id`, `tree` (text, elements numbered `[n]`), screenshot in `images` |
| `computer_click` | `element` or `x, y`; `button?, count?, foreground?` | Presses / selects through accessibility; a point is hit-tested to its element |
| `computer_type` | `text, element?, via?, foreground?` | Inserts at the cursor (accessibility), or key events with `via: "keys"` |
| `computer_key` | `keys` (`"cmd+l"`, `"shift+tab return"`), `foreground?` | Shortcuts go through the app's menu when it is in the background |
| `computer_scroll` | `direction, amount?, element? / x, y` | Scroll wheel at that place |
| `computer_drag` | `from, to` (`{element}` or `{x, y}`) | Always in the foreground (moves the real pointer) |
| `computer_set_value` | `element, value` | Sets AXValue: text fields, sliders, pop-ups |
| `computer_action` | `element, action` | Any other AX action (`AXShowMenu`, `AXIncrement`, …) |
| `computer_open_app` | – | Starts or activates an app (also not running), returns its state |

`app` is a `bundle_id` (preferred) or name, localized or not ("Notes",
"备忘录"). Actions take `state_id` (the state their element numbers come
from) and return the new state with `done` saying what happened; pass
`return_state: false` to skip it and `screenshot: false` for tree only.
Coordinates are pixels of the last screenshot. When the app changed
since `state_id`, nothing is done and the result has `error: "stale_state"`
with the current state. When a background action has no effect,
`foreground: true` brings the app to the front and uses the real pointer
and keyboard. The tool descriptions carry guidance for the model.

```ts
const tools = await machine.tools({ approvals: true });   // includes computer_* on macOS
let r = await machine.callTool("computer_get_state", { app: "com.apple.Notes" },
  { approvals: true, session: chatId, images: true });
if (isApprovalRequired(r)) r = await machine.approveTool(r.approval_id, await ask(r.title));
// Anthropic: tool_result content; OpenAI: toModelContent(r, "openai")
const content = toModelContent(r);
```

**Images** come back only with `images: true` on `tools.call` (JPEG, long
edge at most 1280 px, quality 70, from 0.3.17; 1568 px before); otherwise
they are `{mime_type, omitted: true}`, so older hubs don't paste base64 into
the model's context.

From 0.3.17:

- **`image_upload: {url}`** (with `images: true`): the daemon PUTs each
  screenshot's JPEG bytes to that signed URL (`Content-Type: image/jpeg`,
  30 s, no redirects; HTTPS, or HTTP to this machine) and the image becomes
  `{mime_type, uploaded: true, bytes}` (`bytes`: the base64 length `data`
  would have had). Links with a `max_body` move a screenshot in dozens of
  parts otherwise. A failed upload keeps `data` inline and adds
  `image_upload_failed: <reason>` to `warnings`. The URL never appears in
  errors or logs.
- **Unchanged screenshots**: when a window's screenshot is the same as the
  last one sent for it in that `session`, the image is
  `{mime_type, unchanged: true}`, with no data and no upload.
  `computer.reset {session}` forgets what was sent.
- **`app_name`**: the product the helper runs under when the link has no
  app name of its own (links paired by a web flow often have none), so
  macOS asks for and keeps the permissions under that product. The link's
  own app name wins. `computer.permissions` takes it too.

`toModelContent` leaves out images without `data`.

**Who may use it.** Local clients and `full` links: everything. `ask` and
`edit` links: only with `approvals: true`, and the first use of **each app**
waits for the person (`title: "Use Notes (com.apple.Notes)"`);
`allow_session` keeps that app for the `session` for an hour, other apps
still ask. `read_only`: not offered. Terminals, password managers, System
Settings and agl's own launchers are never operated (`blocked: true`,
error `app_blocked`); the person can change that in `config.json`
(`computer.blocked_apps`, `computer.allowed_blocked_apps`, bundle ids or
prefixes ending in `*`), a hub cannot.

**Stop.** When the person presses stop, call `computer.stop {session}`:
later calls in that session fail with `stopped_by_user` (the model is told
to wait). Call `computer.reset {session}` with their next message.

**macOS permissions.** The work happens in a helper process (`agl
__computer`) started through the link's product launcher (see
[App name](protocol.md#app-name-macos-privacy-prompts)), so macOS asks for
**Accessibility** and **Screen Recording** in the product's name and keeps
the grants per product. Until both are granted, tools fail with
`permission_missing: accessibility`, or return the tree with a
`permission_missing: screen_recording` warning and no screenshot. Check
with `computer.permissions {}` and guide the person with
`computer.permissions {request: true}`, which shows macOS's prompt and
opens System Settings. Linux and Windows: not offered.

## Rust embedding

Keep one `agl::machine::Runtime::default()` for the lifetime of your host,
and call its `dispatch(&cfg, &origin, method, params).await`. Use the same
`Origin` for every request from one application. Call
`runtime.executions.shutdown().await` before the host exits. The older
stateless `agl::machine::dispatch` remains available for file operations
and one-shot blocking `exec`; use `Runtime` for discovery, persistent
execution IDs, output polling and cancellation.
