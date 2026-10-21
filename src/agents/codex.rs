//! Codex over `codex app-server` (JSON-RPC, newline-delimited, stdio):
//! `initialize` -> `thread/start|resume` -> `turn/start` per prompt,
//! `turn/steer` mid-turn, `turn/completed` ends the turn. Approval server
//! requests become permission requests.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::process::Child;

use super::rpc::Peer;
use super::{base_command, clip, kill_tree, split_content, spawn_frame_reader, spawn_stderr_tail, track, Decision, DriverMsg, DriverTx, PermissionAsk, SpawnSpec};
use crate::protocol::AgentEvent;

const ACK: Duration = Duration::from_secs(60);

/// Concurrent first `initialize` on one CODEX_HOME races on its SQLite state
/// (openai/codex#50290), so handshakes are serialized.
static HANDSHAKE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Default)]
struct TurnState {
    thread_id: Option<String>,
    turn_id: Option<String>,
    last_error: Option<String>,
    usage: Option<Value>,
    wrote_text: bool,
    limit: usize,
}

enum Approval {
    /// v2: {decision: accept|acceptForSession|decline}
    V2(Value),
    /// legacy v1: {decision: approved|approved_for_session|denied}
    V1(Value),
    /// MCP elicitation without a form: {action: accept|decline}, plus the
    /// `_meta.persist` scopes the request offers (e.g. "session", "always").
    Elicitation(Value, Vec<String>),
}

type Approvals = Arc<Mutex<HashMap<String, Approval>>>;

pub struct CodexDriver {
    child: Child,
    peer: Peer,
    state: Arc<Mutex<TurnState>>,
    approvals: Approvals,
    thread_id: String,
}

const TOOL_ITEMS: &[&str] = &["commandExecution", "fileChange", "mcpToolCall", "webSearch", "dynamicToolCall", "imageView"];

/// The text output of a built-in (non-MCP) tool item.
fn tool_text(ty: &str, item: &Value, status: &str) -> String {
    match ty {
        "commandExecution" => item["aggregatedOutput"].as_str().unwrap_or_default().to_string(),
        "fileChange" => item["changes"]
            .as_array()
            .map(|c| c.iter().filter_map(|x| x["path"].as_str()).collect::<Vec<_>>().join("\n"))
            .unwrap_or_default(),
        _ => status.to_string(),
    }
}

fn thread_params(spec: &SpawnSpec) -> Value {
    let (sandbox, approval) = match spec.access.as_str() {
        "full" => ("danger-full-access", "never"),
        "read_only" => ("read-only", "never"),
        "edit" => ("workspace-write", "on-request"),
        _ => ("workspace-write", "untrusted"),
    };
    let mut config = json!({});
    if sandbox == "workspace-write" {
        config["sandbox_workspace_write.network_access"] = json!(true);
    }
    if let Some(e) = &spec.effort {
        config["model_reasoning_effort"] = json!(e);
    }
    let mut p = json!({"cwd": spec.cwd.display().to_string(), "sandbox": sandbox, "approvalPolicy": approval, "config": config});
    // Always repeat the model: thread/resume otherwise reverts to config.toml's.
    if let Some(m) = &spec.model {
        p["model"] = json!(m);
    }
    p
}

fn app_server(bin: &Path, spec: &SpawnSpec) -> tokio::process::Command {
    let mut cmd = base_command(bin, spec);
    // notify=[] keeps the user's desktop notification hook from firing per turn.
    cmd.args(["app-server", "-c", "notify=[]", "--listen", "stdio://"]);
    cmd
}

async fn handshake(peer: &Peer) -> Result<()> {
    let _guard = HANDSHAKE.lock().await;
    peer.request(
        "initialize",
        json!({"clientInfo": {"name": "agl", "title": "agl", "version": crate::protocol::VERSION}, "capabilities": {"experimentalApi": true}}),
        ACK,
    )
    .await?;
    peer.notify("initialized", json!({})).await
}

impl CodexDriver {
    pub async fn spawn(bin: &Path, spec: &SpawnSpec, tx: DriverTx) -> Result<Self> {
        let mut child = app_server(bin, spec).spawn().with_context(|| format!("spawn {}", bin.display()))?;
        if let Some(pid) = child.id() {
            track(pid);
        }
        let peer = Peer::new(child.stdin.take().unwrap(), "codex");
        let stdout = child.stdout.take().unwrap();
        let tail = spawn_stderr_tail(child.stderr.take().unwrap());
        let state = Arc::new(Mutex::new(TurnState { limit: spec.tool_output_limit, ..Default::default() }));
        let approvals: Approvals = Arc::default();

        let gen = spec.generation;
        let (p, st, ap, t2) = (peer.clone(), state.clone(), approvals.clone(), tx.clone());
        let reader = spawn_frame_reader(stdout, move |msg| {
            if p.route_reply(&msg) {
                return;
            }
            let method = msg["method"].as_str().unwrap_or_default().to_string();
            let params = &msg["params"];
            if let Some(id) = msg.get("id").cloned() {
                server_request(&p, &ap, &t2, gen, id, &method, params);
            } else if method == "serverRequest/resolved" {
                let key = params["requestId"].to_string();
                if ap.lock().unwrap().remove(&key).is_some() {
                    let _ = t2.send((gen, DriverMsg::PermissionCancelled(key)));
                }
            } else {
                for out in map_notification(&method, params, &st) {
                    let _ = t2.send((gen, out));
                }
            }
        });
        {
            let (peer, tx) = (peer.clone(), tx.clone());
            tokio::spawn(async move {
                let _ = reader.await;
                peer.fail_all();
                let reason = tail.lock().unwrap().join("\n");
                let _ = tx.send((gen, DriverMsg::Exited(reason)));
            });
        }

        let mut d = CodexDriver { child, peer, state, approvals, thread_id: String::new() };
        let res = match d.open(spec).await {
            Ok(r) => r,
            Err(e) => {
                kill_tree(&mut d.child).await;
                return Err(e);
            }
        };
        let thread_id = res["thread"]["id"].as_str().unwrap_or_default().to_string();
        d.state.lock().unwrap().thread_id = Some(thread_id.clone());
        d.thread_id = thread_id.clone();
        let _ = tx.send((gen, DriverMsg::NativeId(thread_id.clone())));
        let _ = tx.send((gen, DriverMsg::Event(AgentEvent::Info { native_id: Some(thread_id), model: res["model"].as_str().map(String::from) })));
        Ok(d)
    }

    async fn open(&mut self, spec: &SpawnSpec) -> Result<Value> {
        handshake(&self.peer).await?;
        let mut params = thread_params(spec);
        if let Some(id) = &spec.resume {
            params["threadId"] = json!(id);
            params["excludeTurns"] = json!(true);
            // A just-killed predecessor may still hold the thread briefly.
            let mut attempt = 0;
            loop {
                match self.peer.request("thread/resume", params.clone(), ACK).await {
                    Ok(r) => {
                        if r["thread"]["id"].as_str() != Some(id) {
                            bail!("codex resumed a different thread than {id}");
                        }
                        return Ok(r);
                    }
                    Err(e) if e.to_string().contains("active writer") && attempt < 3 => {
                        attempt += 1;
                        tokio::time::sleep(Duration::from_millis(500 * attempt)).await;
                    }
                    Err(e) => return Err(e.context(format!("resume codex thread {id}"))),
                }
            }
        }
        let r = self.peer.request("thread/start", params, ACK).await?;
        if r["thread"]["id"].as_str().is_none() {
            bail!("codex returned no thread id");
        }
        Ok(r)
    }

    pub async fn prompt(&mut self, text: &str) -> Result<()> {
        {
            let mut s = self.state.lock().unwrap();
            s.last_error = None;
            s.wrote_text = false;
        }
        let res = self
            .peer
            .request("turn/start", json!({"threadId": self.thread_id, "input": [{"type": "text", "text": text, "text_elements": []}]}), ACK)
            .await?;
        if let Some(id) = res["turn"]["id"].as_str() {
            self.state.lock().unwrap().turn_id = Some(id.to_string());
        }
        Ok(())
    }

    pub async fn steer(&mut self, text: &str) -> Result<()> {
        let turn = self.state.lock().unwrap().turn_id.clone();
        let Some(turn_id) = turn else { bail!("no active turn") };
        self.peer
            .request(
                "turn/steer",
                json!({"threadId": self.thread_id, "expectedTurnId": turn_id, "input": [{"type": "text", "text": text, "text_elements": []}]}),
                ACK,
            )
            .await?;
        Ok(())
    }

    pub async fn interrupt(&mut self) -> Result<()> {
        let turn = self.state.lock().unwrap().turn_id.clone();
        let Some(turn_id) = turn else { bail!("no active turn") };
        self.peer.request("turn/interrupt", json!({"threadId": self.thread_id, "turnId": turn_id}), ACK).await?;
        Ok(())
    }

    pub async fn respond(&mut self, key: &str, decision: Decision) -> Result<()> {
        let Some(kind) = self.approvals.lock().unwrap().remove(key) else { return Ok(()) };
        let (id, result) = match kind {
            Approval::V2(id) => (
                id,
                json!({"decision": match decision { Decision::Allow => "accept", Decision::AllowSession => "acceptForSession", Decision::Deny => "decline" }}),
            ),
            Approval::V1(id) => (
                id,
                json!({"decision": match decision { Decision::Allow => "approved", Decision::AllowSession => "approved_for_session", Decision::Deny => "denied" }}),
            ),
            Approval::Elicitation(id, persist) => (
                id,
                match decision {
                    Decision::Deny => json!({"action": "decline", "content": null}),
                    // Computer use asks once per app; remember the answer for the session.
                    Decision::AllowSession if persist.iter().any(|p| p == "session") => {
                        json!({"action": "accept", "content": {}, "_meta": {"persist": "session"}})
                    }
                    _ => json!({"action": "accept", "content": {}}),
                },
            ),
        };
        self.peer.reply(&id, result).await
    }

    pub async fn shutdown(mut self) {
        kill_tree(&mut self.child).await;
    }
}

fn server_request(p: &Peer, ap: &Approvals, tx: &DriverTx, gen: u64, id: Value, method: &str, params: &Value) {
    let key = id.to_string();
    let ask = |tool: &str, title: String, kind: Approval| {
        ap.lock().unwrap().insert(key.clone(), kind);
        let _ = tx.send((
            gen,
            DriverMsg::Permission(PermissionAsk {
                key: key.clone(),
                tool: tool.into(),
                title,
                input: params.clone(),
                options: vec!["allow".into(), "allow_session".into(), "deny".into()],
            }),
        ));
    };
    match method {
        "item/commandExecution/requestApproval" => {
            let title = params["command"].as_str().or(params["reason"].as_str()).unwrap_or("run a command").to_string();
            ask("shell", title, Approval::V2(id));
        }
        "item/fileChange/requestApproval" => {
            let title = params["reason"].as_str().unwrap_or("edit files").to_string();
            ask("edit", title, Approval::V2(id));
        }
        "execCommandApproval" => {
            let cmd = params["command"].as_array().map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" ")).unwrap_or_default();
            ask("shell", cmd, Approval::V1(id));
        }
        "applyPatchApproval" => ask("edit", "apply a patch".into(), Approval::V1(id)),
        "mcpServer/elicitation/request"
            // Only plain confirmations can be answered with allow/deny.
            if params["requestedSchema"]["properties"].as_object().map(|o| o.is_empty()).unwrap_or(true) =>
        {
            let server = params["serverName"].as_str().unwrap_or("mcp");
            let msg = params["message"].as_str().unwrap_or("confirm").to_string();
            let persist = params["_meta"]["persist"]
                .as_array()
                .map(|p| p.iter().filter_map(|s| s.as_str().map(String::from)).collect())
                .unwrap_or_default();
            ask(&format!("mcp:{server}"), msg, Approval::Elicitation(id, persist));
        }
        _ => {
            let p = p.clone();
            let result = match method {
                "mcpServer/elicitation/request" => Some(json!({"action": "decline", "content": null})),
                "item/tool/requestUserInput" => Some(json!({"answers": {}})),
                "currentTime/read" => Some(json!({"currentTimeAt": crate::protocol::now_ms() / 1000})),
                _ => None,
            };
            // Never leave the server waiting on a request nobody can answer.
            tokio::spawn(async move {
                let _ = match result {
                    Some(r) => p.reply(&id, r).await,
                    None => p.reply_error(&id, -32601, "unsupported headless request").await,
                };
            });
        }
    }
}

fn map_notification(method: &str, p: &Value, state: &Arc<Mutex<TurnState>>) -> Vec<DriverMsg> {
    let mut s = state.lock().unwrap();
    // Only the root thread; child threads (subagents) carry their own ids.
    if let (Some(root), Some(tid)) = (&s.thread_id, p["threadId"].as_str()) {
        if root != tid {
            return vec![];
        }
    }
    let ev = |e: AgentEvent| vec![DriverMsg::Event(e)];
    match method {
        "item/agentMessage/delta" => {
            s.wrote_text = true;
            ev(AgentEvent::TextDelta { text: p["delta"].as_str().unwrap_or_default().into() })
        }
        m if m.starts_with("item/reasoning/") && m.ends_with("Delta") => {
            ev(AgentEvent::Reasoning { text: p["delta"].as_str().unwrap_or_default().into() })
        }
        "item/started" => {
            let item = &p["item"];
            let ty = item["type"].as_str().unwrap_or_default();
            if ty == "agentMessage" && s.wrote_text {
                return ev(AgentEvent::TextDelta { text: "\n\n".into() });
            }
            if !TOOL_ITEMS.contains(&ty) {
                return vec![];
            }
            let (name, input) = match ty {
                "commandExecution" => ("shell".to_string(), json!({"command": item["command"], "cwd": item["cwd"]})),
                "fileChange" => ("edit".to_string(), json!({"changes": item["changes"]})),
                "mcpToolCall" => (
                    format!("{}.{}", item["server"].as_str().unwrap_or("mcp"), item["tool"].as_str().unwrap_or("tool")),
                    item["arguments"].clone(),
                ),
                "dynamicToolCall" => (
                    format!("{}.{}", item["namespace"].as_str().unwrap_or("tool"), item["tool"].as_str().unwrap_or("call")),
                    item["arguments"].clone(),
                ),
                _ => (ty.to_string(), item.clone()),
            };
            ev(AgentEvent::ToolStart { call_id: item["id"].as_str().unwrap_or_default().into(), name, input })
        }
        "item/completed" => {
            let item = &p["item"];
            let ty = item["type"].as_str().unwrap_or_default();
            if !TOOL_ITEMS.contains(&ty) {
                return vec![];
            }
            let status = item["status"].as_str().unwrap_or("completed");
            let exit_ok = item["exitCode"].as_i64().map(|c| c == 0).unwrap_or(true);
            let (output, images) = match ty {
                // MCP results carry text and images (computer-use screenshots).
                "mcpToolCall" | "dynamicToolCall" => {
                    let result = if item["result"].is_null() { &item["contentItems"] } else { &item["result"] };
                    let (text, images) = split_content(result, s.limit);
                    let text = match (text.is_empty(), item["error"]["message"].as_str()) {
                        (true, Some(err)) => err.to_string(),
                        _ => text,
                    };
                    (text, images)
                }
                _ => (tool_text(ty, item, status), Vec::new()),
            };
            ev(AgentEvent::ToolEnd {
                call_id: item["id"].as_str().unwrap_or_default().into(),
                ok: status == "completed" && exit_ok,
                output: clip(&output, s.limit),
                images,
            })
        }
        "turn/started" => {
            s.turn_id = p["turn"]["id"].as_str().map(String::from);
            vec![]
        }
        "thread/tokenUsage/updated" => {
            s.usage = Some(p["tokenUsage"].clone());
            vec![]
        }
        "error" => {
            let msg = p["error"]["message"].as_str().or(p["message"].as_str()).unwrap_or("error").to_string();
            if p["willRetry"].as_bool().unwrap_or(false) {
                return ev(AgentEvent::Retrying { message: msg });
            }
            s.last_error = Some(msg);
            vec![]
        }
        "turn/completed" => {
            s.turn_id = None;
            let turn = &p["turn"];
            let (outcome, error) = match turn["status"].as_str() {
                Some("completed") => ("completed", None),
                Some("interrupted") => ("aborted", None),
                _ => ("failed", turn["error"]["message"].as_str().map(String::from).or(s.last_error.take())),
            };
            vec![DriverMsg::TurnDone { outcome: outcome.into(), error, usage: s.usage.take() }]
        }
        _ => vec![],
    }
}

/// `model/list` from a short-lived app-server (no thread is created).
pub async fn list_models(bin: &Path) -> Result<Vec<Value>> {
    let spec = super::probe_spec("codex", super::Protocol::Codex);
    let mut child = app_server(bin, &spec).spawn()?;
    let peer = Peer::new(child.stdin.take().unwrap(), "codex");
    let p = peer.clone();
    let reader = spawn_frame_reader(child.stdout.take().unwrap(), move |m| {
        p.route_reply(&m);
    });
    let result = async {
        handshake(&peer).await?;
        let mut models = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..20 {
            let r = peer.request("model/list", json!({"limit": 100, "cursor": cursor}), Duration::from_secs(20)).await?;
            for m in r["data"].as_array().into_iter().flatten() {
                if m["hidden"].as_bool().unwrap_or(false) {
                    continue;
                }
                let efforts: Vec<Value> = m["supportedReasoningEfforts"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|e| e.get("reasoningEffort").cloned().unwrap_or(e.clone()))
                    .collect();
                models.push(json!({
                    "id": m["model"].as_str().or(m["id"].as_str()),
                    "name": m["displayName"],
                    "description": m["description"],
                    "default": m["isDefault"],
                    "efforts": efforts,
                    "default_effort": m["defaultReasoningEffort"],
                }));
            }
            let next = r["nextCursor"].as_str().map(String::from);
            if next.is_none() || next == cursor {
                break;
            }
            cursor = next;
        }
        anyhow::Ok(models)
    }
    .await;
    reader.abort();
    kill_tree(&mut child).await;
    result
}
