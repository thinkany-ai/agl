//! Claude Code over `claude -p --input-format stream-json`: one long-lived
//! process per session, one user frame per turn, `result` ends the turn.
//! The control channel carries interrupts and, with
//! `--permission-prompt-tool stdio`, tool permission requests.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin};
use tokio::sync::Mutex as AsyncMutex;

use super::{base_command, kill_tree, split_content, spawn_frame_reader, spawn_stderr_tail, track, Decision, DriverMsg, DriverTx, PermissionAsk, SpawnSpec};
use crate::protocol::AgentEvent;

/// Read-only tools allowed in `read_only` access (everything else is denied
/// without asking, `--permission-mode dontAsk`).
const READ_ONLY_TOOLS: &str = "Read,Grep,Glob,LS,WebSearch,WebFetch,TodoWrite,NotebookRead";

pub struct ClaudeDriver {
    child: Child,
    stdin: Arc<AsyncMutex<ChildStdin>>,
    req_seq: u64,
    /// Pending can_use_tool requests: request_id -> original input.
    pending: Arc<Mutex<HashMap<String, Value>>>,
    /// The instructions file, removed with the driver.
    _instructions: Option<PrivateFile>,
}

/// A 0600 file under `$AGL_HOME/run`, deleted when dropped. A hub's
/// instructions go to Claude Code through one instead of the command line,
/// which every local user can read (`ps`).
struct PrivateFile(std::path::PathBuf);

impl PrivateFile {
    fn write(name: &str, text: &str) -> Result<Self> {
        let dir = crate::config::home().join("run");
        std::fs::create_dir_all(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let path = dir.join(name);
        crate::config::write_private(&path, text.as_bytes())?;
        Ok(Self(path))
    }
}

/// Whether this Claude Code takes `--append-system-prompt-file` (2.1 on; an
/// older one, or one whose version cannot be read, gets the text itself).
/// Read once per executable and modification time.
async fn takes_prompt_file(bin: &Path) -> bool {
    type Seen = Mutex<HashMap<(std::path::PathBuf, Option<std::time::SystemTime>), bool>>;
    static SEEN: std::sync::OnceLock<Seen> = std::sync::OnceLock::new();
    let key = (bin.to_path_buf(), std::fs::metadata(bin).and_then(|m| m.modified()).ok());
    if let Some(&known) = SEEN.get_or_init(Default::default).lock().unwrap().get(&key) {
        return known;
    }
    let ok = super::version_of(bin).await.is_some_and(|v| at_least_2_1(&v));
    SEEN.get_or_init(Default::default).lock().unwrap().insert(key, ok);
    ok
}

fn at_least_2_1(version: &str) -> bool {
    let mut parts = version.split(|c: char| !c.is_ascii_digit()).filter(|p| !p.is_empty()).map(|p| p.parse::<u64>().unwrap_or(0));
    let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    (major, minor) >= (2, 1)
}

impl Drop for PrivateFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

impl ClaudeDriver {
    pub async fn spawn(bin: &Path, spec: &SpawnSpec, tx: DriverTx) -> Result<Self> {
        let mut cmd = base_command(bin, spec);
        cmd.args(["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose", "--include-partial-messages"]);
        match spec.access.as_str() {
            "full" => {
                cmd.arg("--dangerously-skip-permissions");
            }
            "read_only" => {
                cmd.args(["--permission-mode", "dontAsk", "--allowedTools", READ_ONLY_TOOLS]);
            }
            "edit" => {
                cmd.args(["--permission-mode", "acceptEdits", "--permission-prompt-tool", "stdio"]);
            }
            _ => {
                cmd.args(["--permission-mode", "default", "--permission-prompt-tool", "stdio"]);
            }
        }
        if let Some(m) = &spec.model {
            cmd.args(["--model", m]);
        }
        if let Some(e) = &spec.effort {
            cmd.args(["--effort", e]);
        }
        let native_id = match &spec.resume {
            Some(id) => {
                cmd.args(["--resume", id]);
                id.clone()
            }
            None => {
                // Pre-assigned, so the id is known before any output.
                let id = uuid::Uuid::new_v4().to_string();
                cmd.args(["--session-id", &id]);
                id
            }
        };
        let instructions = match &spec.instructions {
            Some(text) if takes_prompt_file(bin).await => {
                let file = PrivateFile::write(&format!("claude-{native_id}.md"), text).context("write the session's instructions")?;
                cmd.arg("--append-system-prompt-file").arg(&file.0);
                Some(file)
            }
            Some(text) => {
                cmd.args(["--append-system-prompt", text]);
                None
            }
            None => None,
        };
        if spec.ephemeral {
            cmd.arg("--no-session-persistence");
        }
        // Last, as separate argv entries (no shell): they can override ours.
        cmd.args(&spec.extra_args);
        let mut child = cmd.spawn().with_context(|| format!("spawn {}", bin.display()))?;
        if let Some(pid) = child.id() {
            track(pid);
        }
        let stdin = Arc::new(AsyncMutex::new(child.stdin.take().unwrap()));
        let stdout = child.stdout.take().unwrap();
        let tail = spawn_stderr_tail(child.stderr.take().unwrap());
        let pending: Arc<Mutex<HashMap<String, Value>>> = Arc::default();

        let gen = spec.generation;
        let _ = tx.send((gen, DriverMsg::NativeId(native_id)));
        let mut state = State { wrote_text: false, limit: spec.tool_output_limit };
        let (p, w, t2) = (pending.clone(), stdin.clone(), tx.clone());
        let reader = spawn_frame_reader(stdout, move |frame| {
            if frame["type"] == "control_request" {
                let id = frame["request_id"].as_str().unwrap_or_default().to_string();
                let req = &frame["request"];
                if req["subtype"] == "can_use_tool" {
                    let input = req["input"].clone();
                    p.lock().unwrap().insert(id.clone(), input.clone());
                    let tool = req["tool_name"].as_str().unwrap_or("tool").to_string();
                    let title = req["description"].as_str().or(req["display_name"].as_str()).unwrap_or(&tool).to_string();
                    let _ = t2.send((
                        gen,
                        DriverMsg::Permission(PermissionAsk {
                            key: id,
                            tool,
                            title,
                            input,
                            options: vec!["allow".into(), "allow_session".into(), "deny".into()],
                        }),
                    ));
                } else {
                    // Never leave the CLI waiting on a request nobody can answer.
                    let w = w.clone();
                    let reply = json!({"type": "control_response", "response": {"subtype": "error", "request_id": id, "error": "not supported by agl"}});
                    tokio::spawn(async move {
                        let _ = w.lock().await.write_all(format!("{reply}\n").as_bytes()).await;
                    });
                }
                return;
            }
            if frame["type"] == "control_cancel_request" {
                if let Some(id) = frame["request_id"].as_str() {
                    p.lock().unwrap().remove(id);
                    let _ = t2.send((gen, DriverMsg::PermissionCancelled(id.to_string())));
                }
                return;
            }
            for msg in state.map(&frame) {
                let _ = t2.send((gen, msg));
            }
        });
        tokio::spawn(async move {
            let _ = reader.await;
            let reason = tail.lock().unwrap().join("\n");
            let _ = tx.send((gen, DriverMsg::Exited(reason)));
        });
        let d = ClaudeDriver { child, stdin, req_seq: 0, pending, _instructions: instructions };
        // Claude initializes (MCP servers, hooks, settings) lazily on the
        // first input; ask now so a warmed session is really warm.
        d.write(json!({"type": "control_request", "request_id": "agl-init", "request": {"subtype": "initialize", "hooks": null}})).await?;
        Ok(d)
    }

    async fn write(&self, v: Value) -> Result<()> {
        let mut w = self.stdin.lock().await;
        w.write_all(format!("{v}\n").as_bytes()).await.context("write to claude")?;
        w.flush().await?;
        Ok(())
    }

    /// A user frame. While a turn runs, claude absorbs it at the next model
    /// step, which is how steering works.
    pub async fn prompt(&mut self, text: &str) -> Result<()> {
        self.write(json!({
            "type": "user",
            "uuid": uuid::Uuid::new_v4().to_string(),
            "message": {"role": "user", "content": [{"type": "text", "text": text}]}
        }))
        .await
    }

    pub async fn interrupt(&mut self) -> Result<()> {
        self.req_seq += 1;
        let id = format!("agl-int-{}", self.req_seq);
        self.write(json!({"type": "control_request", "request_id": id, "request": {"subtype": "interrupt"}})).await
    }

    pub async fn respond(&mut self, key: &str, decision: Decision) -> Result<()> {
        let Some(input) = self.pending.lock().unwrap().remove(key) else { return Ok(()) };
        let response = match decision {
            Decision::Allow => json!({"behavior": "allow", "updatedInput": input}),
            Decision::AllowSession => json!({"behavior": "allow", "updatedInput": input}),
            Decision::Deny => json!({"behavior": "deny", "message": "The user denied this action."}),
        };
        self.write(json!({"type": "control_response", "response": {"subtype": "success", "request_id": key, "response": response}}))
            .await
    }

    pub async fn shutdown(mut self) {
        kill_tree(&mut self.child).await;
    }
}

struct State {
    wrote_text: bool,
    limit: usize,
}

impl State {
    fn map(&mut self, f: &Value) -> Vec<DriverMsg> {
        let mut out = Vec::new();
        let ev = |e: AgentEvent| DriverMsg::Event(e);
        // Text from subagents is not part of the main transcript.
        let is_root = f.get("parent_tool_use_id").map(|v| v.is_null()).unwrap_or(true);
        match f["type"].as_str().unwrap_or("") {
            "system" => match f["subtype"].as_str() {
                Some("init") => out.push(ev(AgentEvent::Info {
                    native_id: f["session_id"].as_str().map(String::from),
                    model: f["model"].as_str().map(String::from),
                })),
                Some("api_retry") => out.push(ev(AgentEvent::Retrying {
                    message: f["error"].as_str().or(f["message"].as_str()).unwrap_or("retrying").to_string(),
                })),
                _ => {}
            },
            "stream_event" if is_root => {
                let e = &f["event"];
                if e["type"] == "content_block_delta" {
                    let d = &e["delta"];
                    match d["type"].as_str() {
                        Some("text_delta") => {
                            if let Some(t) = d["text"].as_str() {
                                self.wrote_text = true;
                                out.push(ev(AgentEvent::TextDelta { text: t.into(), phase: None }));
                            }
                        }
                        Some("thinking_delta") => {
                            if let Some(t) = d["thinking"].as_str() {
                                out.push(ev(AgentEvent::Reasoning { text: t.into() }));
                            }
                        }
                        _ => {}
                    }
                } else if e["type"] == "content_block_start" && e["content_block"]["type"] == "text" && self.wrote_text {
                    out.push(ev(AgentEvent::TextDelta { text: "\n\n".into(), phase: None }));
                }
            }
            "assistant" => {
                for b in f["message"]["content"].as_array().into_iter().flatten().filter(|b| b["type"] == "tool_use") {
                    if is_root && b["name"] == "TodoWrite" {
                        out.push(ev(AgentEvent::Plan { entries: b["input"]["todos"].clone() }));
                    }
                    out.push(ev(AgentEvent::ToolStart {
                        call_id: b["id"].as_str().unwrap_or_default().into(),
                        name: b["name"].as_str().unwrap_or("tool").into(),
                        input: b["input"].clone(),
                    }));
                }
            }
            "user" => {
                for b in f["message"]["content"].as_array().into_iter().flatten().filter(|b| b["type"] == "tool_result") {
                    let (output, images) = split_content(&b["content"], self.limit);
                    out.push(ev(AgentEvent::ToolEnd {
                        call_id: b["tool_use_id"].as_str().unwrap_or_default().into(),
                        ok: !b["is_error"].as_bool().unwrap_or(false),
                        output,
                        images,
                    }));
                }
            }
            "result" => {
                self.wrote_text = false;
                // Claude Code also runs turns of its own: a background task's
                // notification is answered like a message, and its `result`
                // names where it came from. After `--resume`, the tasks the
                // last process left behind are reported that way before the
                // person's message is read, so that `result` is not their
                // turn's end; theirs has no origin.
                if f["origin"]["kind"] == "task-notification" {
                    return out;
                }
                let failed = f["is_error"].as_bool().unwrap_or(false);
                let error = failed.then(|| {
                    f["result"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .or_else(|| {
                            f["errors"].as_array().map(|e| e.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join("; "))
                        })
                        .unwrap_or_else(|| f["subtype"].as_str().unwrap_or("error").to_string())
                });
                // `result` is the final assistant message's text.
                let text = (!failed).then(|| f["result"].as_str().map(String::from)).flatten();
                out.push(DriverMsg::TurnDone {
                    outcome: if failed { "failed" } else { "completed" }.into(),
                    error,
                    text,
                    usage: Some(json!({
                        "usage": f["usage"],
                        "model_usage": f["modelUsage"],
                        "cost_usd": f["total_cost_usd"],
                        "duration_ms": f["duration_ms"],
                        "num_turns": f["num_turns"],
                    })),
                });
            }
            _ => {}
        }
        out
    }
}

/// Models via the control protocol's `initialize` answer (no model call).
pub async fn list_models(bin: &Path) -> Vec<Value> {
    let fallback = || {
        ["default", "opus", "sonnet", "haiku"].iter().map(|m| json!({"id": m, "name": m})).collect::<Vec<_>>()
    };
    let spec = super::probe_spec("claude", super::Protocol::Claude);
    let mut cmd = base_command(bin, &spec);
    cmd.args(["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose", "--no-session-persistence"]);
    let Ok(mut child) = cmd.spawn() else { return fallback() };
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let req = json!({"type": "control_request", "request_id": "agl-init", "request": {"subtype": "initialize", "hooks": null}});
    let _ = stdin.write_all(format!("{req}\n").as_bytes()).await;
    let (tx, rx) = tokio::sync::oneshot::channel::<Value>();
    let tx = Mutex::new(Some(tx));
    let reader = spawn_frame_reader(stdout, move |f| {
        if f["type"] == "control_response" && f["response"]["request_id"] == "agl-init" {
            if let Some(tx) = tx.lock().unwrap().take() {
                let _ = tx.send(f["response"]["response"].clone());
            }
        }
    });
    let res = tokio::time::timeout(Duration::from_secs(20), rx).await;
    reader.abort();
    kill_tree(&mut child).await;
    match res {
        Ok(Ok(v)) => {
            let models: Vec<Value> = v["models"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|m| {
                    let id = m["value"].as_str()?;
                    Some(json!({
                        "id": id,
                        "name": m["displayName"].as_str().unwrap_or(id),
                        "description": m["description"],
                        "efforts": m["supportedEffortLevels"],
                    }))
                })
                .collect();
            if models.is_empty() {
                fallback()
            } else {
                models
            }
        }
        _ => fallback(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_file_from_2_1() {
        assert!(at_least_2_1("2.1.332 (Claude Code)"));
        assert!(at_least_2_1("3.0.0"));
        assert!(!at_least_2_1("2.0.76 (Claude Code)"));
        assert!(!at_least_2_1("1.0.128"));
        assert!(!at_least_2_1("garbage"));
    }

    #[test]
    fn maps_stream_json() {
        let mut s = State { wrote_text: false, limit: 1024 };
        let delta = json!({"type": "stream_event", "parent_tool_use_id": null, "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": "hi"}}});
        assert!(matches!(&s.map(&delta)[0], DriverMsg::Event(AgentEvent::TextDelta { text, .. }) if text == "hi"));

        let child = json!({"type": "stream_event", "parent_tool_use_id": "tu9", "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": "sub"}}});
        assert!(s.map(&child).is_empty(), "subagent text is not the main transcript");

        let tool = json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "tu1", "name": "TodoWrite", "input": {"todos": [{"content": "a"}]}}]}});
        let out = s.map(&tool);
        assert!(matches!(&out[0], DriverMsg::Event(AgentEvent::Plan { .. })));
        assert!(matches!(&out[1], DriverMsg::Event(AgentEvent::ToolStart { name, .. }) if name == "TodoWrite"));

        let result = json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "tu1", "is_error": true, "content": "nope"}]}});
        assert!(matches!(&s.map(&result)[0], DriverMsg::Event(AgentEvent::ToolEnd { ok: false, output, .. }) if output == "nope"));

        let retry = json!({"type": "system", "subtype": "api_retry", "error": "overloaded"});
        assert!(matches!(&s.map(&retry)[0], DriverMsg::Event(AgentEvent::Retrying { .. })));

        let failed = json!({"type": "result", "is_error": true, "result": "Credit balance is too low"});
        assert!(matches!(&s.map(&failed)[0], DriverMsg::TurnDone { outcome, error: Some(e), text: None, .. } if outcome == "failed" && e.contains("Credit")));

        // As Claude Code 2.1 sends it after `--resume`, for a background
        // command the last process left running.
        let notified = json!({"type": "result", "subtype": "success", "is_error": false, "num_turns": 0, "result": "", "origin": {"kind": "task-notification"}});
        assert!(s.map(&notified).is_empty(), "a task notification's result does not end the person's turn");

        let done = json!({"type": "result", "is_error": false, "result": "All tests pass."});
        assert!(matches!(&s.map(&done)[0], DriverMsg::TurnDone { text: Some(t), .. } if t == "All tests pass."), "the result text is the final answer");
    }
}
