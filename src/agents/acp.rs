//! Agent Client Protocol (JSON-RPC 2.0 over stdio): one process and one ACP
//! session per agl session. `session/prompt` is answered when the turn ends,
//! `session/update` streams the turn, `session/request_permission` becomes a
//! permission request, `session/cancel` interrupts.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::process::Child;

use super::rpc::Peer;
use super::{base_command, kill_tree, split_content, spawn_frame_reader, spawn_stderr_tail, track, Decision, DriverMsg, DriverTx, PermissionAsk, SpawnSpec};
use crate::protocol::AgentEvent;

const ACK: Duration = Duration::from_secs(60);
/// `session/prompt` resolves at turn end; this only bounds a lost reply.
const TURN: Duration = Duration::from_secs(24 * 3600);

#[derive(Default)]
struct State {
    /// Updates are ignored while `session/load` replays history.
    live: bool,
    wrote_text: bool,
    started_tools: std::collections::HashSet<String>,
    usage: Option<Value>,
    limit: usize,
}

/// Pending permission requests: key -> (rpc id, offered options).
type Approvals = Arc<Mutex<HashMap<String, (Value, Vec<Value>)>>>;

pub struct AcpDriver {
    child: Child,
    peer: Peer,
    session_id: String,
    state: Arc<Mutex<State>>,
    approvals: Approvals,
    tx: DriverTx,
    generation: u64,
    name: String,
}

/// Mode ids agents use for each access level, best first.
fn modes_for(access: &str) -> &'static [&'static str] {
    match access {
        "full" => &["yolo", "bypassPermissions", "bypass", "full-access", "auto"],
        "edit" => &["autoEdit", "acceptEdits", "auto-edit", "auto"],
        "read_only" => &["plan", "read-only", "readonly"],
        _ => &["default"],
    }
}

impl AcpDriver {
    pub async fn spawn(bin: &Path, spec: &SpawnSpec, tx: DriverTx) -> Result<Self> {
        let mut cmd = base_command(bin, spec);
        cmd.args(&spec.def.args);
        let mut child = cmd.spawn().with_context(|| format!("spawn {}", bin.display()))?;
        if let Some(pid) = child.id() {
            track(pid);
        }
        let peer = Peer::new(child.stdin.take().unwrap(), "acp");
        let stdout = child.stdout.take().unwrap();
        let tail = spawn_stderr_tail(child.stderr.take().unwrap());
        let state = Arc::new(Mutex::new(State { limit: spec.tool_output_limit, ..Default::default() }));
        let approvals: Approvals = Arc::default();

        let gen = spec.generation;
        let (p, st, ap, t2) = (peer.clone(), state.clone(), approvals.clone(), tx.clone());
        let reader = spawn_frame_reader(stdout, move |msg| {
            if p.route_reply(&msg) {
                return;
            }
            let method = msg["method"].as_str().unwrap_or_default();
            let params = &msg["params"];
            match (msg.get("id").cloned(), method) {
                (Some(id), "session/request_permission") => {
                    let key = id.to_string();
                    let tc = &params["toolCall"];
                    let options = params["options"].as_array().cloned().unwrap_or_default();
                    let mut ours = Vec::new();
                    for (kinds, name) in [(&["allow_once"][..], "allow"), (&["allow_always"][..], "allow_session"), (&["reject_once", "reject_always"][..], "deny")] {
                        if options.iter().any(|o| kinds.contains(&o["kind"].as_str().unwrap_or(""))) {
                            ours.push(name.to_string());
                        }
                    }
                    ap.lock().unwrap().insert(key.clone(), (id, options));
                    let _ = t2.send((
                        gen,
                        DriverMsg::Permission(PermissionAsk {
                            key,
                            tool: tc["kind"].as_str().or(tc["title"].as_str()).unwrap_or("tool").to_string(),
                            title: tc["title"].as_str().unwrap_or_default().to_string(),
                            input: tc.get("rawInput").cloned().unwrap_or(tc.clone()),
                            options: ours,
                        }),
                    ));
                }
                (Some(id), _) => {
                    // fs/* and terminal/* are not offered; refuse anything else.
                    let p = p.clone();
                    tokio::spawn(async move {
                        let _ = p.reply_error(&id, -32601, "not supported by agl").await;
                    });
                }
                (None, "session/update") => {
                    for out in map_update(&params["update"], &st) {
                        let _ = t2.send((gen, out));
                    }
                }
                _ => {}
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

        let mut d = AcpDriver {
            child,
            peer,
            session_id: String::new(),
            state,
            approvals,
            tx,
            generation: gen,
            name: spec.def.id.clone(),
        };
        if let Err(e) = d.open(spec).await {
            kill_tree(&mut d.child).await;
            return Err(e);
        }
        Ok(d)
    }

    async fn open(&mut self, spec: &SpawnSpec) -> Result<()> {
        let init = self
            .peer
            .request(
                "initialize",
                json!({
                    "protocolVersion": 1,
                    "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false},
                    "clientInfo": {"name": "agl", "version": crate::protocol::VERSION},
                }),
                ACK,
            )
            .await
            .with_context(|| format!("{} ACP initialize", self.name))?;
        let caps = &init["agentCapabilities"];
        let cwd = spec.cwd.display().to_string();

        let mut opened: Option<Value> = None;
        if let Some(id) = &spec.resume {
            let params = json!({"sessionId": id, "cwd": cwd, "mcpServers": []});
            // resume does not replay history; load does (and we skip it).
            let res = if caps["sessionCapabilities"].get("resume").is_some() {
                self.peer.request("session/resume", params, ACK).await
            } else if caps["loadSession"].as_bool().unwrap_or(false) {
                self.peer.request("session/load", params, ACK).await
            } else {
                Err(anyhow::anyhow!("agent cannot resume sessions"))
            };
            match res {
                Ok(r) => {
                    self.session_id = id.clone();
                    opened = Some(r);
                }
                Err(e) => {
                    let _ = self.tx.send((
                        self.generation,
                        DriverMsg::Event(AgentEvent::Error { message: format!("could not resume {id} ({e:#}); started a new conversation") }),
                    ));
                }
            }
        }
        let opened = match opened {
            Some(r) => r,
            None => {
                let r = self.peer.request("session/new", json!({"cwd": cwd, "mcpServers": []}), ACK).await?;
                let Some(sid) = r["sessionId"].as_str() else { bail!("{} returned no sessionId", self.name) };
                self.session_id = sid.to_string();
                r
            }
        };
        self.state.lock().unwrap().live = true;
        let _ = self.tx.send((self.generation, DriverMsg::NativeId(self.session_id.clone())));

        // Access level -> the agent's own mode, where it has one.
        if let Some(modes) = opened["modes"]["availableModes"].as_array() {
            let ids: Vec<&str> = modes.iter().filter_map(|m| m["id"].as_str()).collect();
            if let Some(mode) = modes_for(&spec.access).iter().find(|m| ids.contains(m)) {
                if opened["modes"]["currentModeId"].as_str() != Some(mode) {
                    let _ = self.peer.request("session/set_mode", json!({"sessionId": self.session_id, "modeId": mode}), ACK).await;
                }
            }
        }
        let mut model = current_model(&opened);
        if let Some(m) = &spec.model {
            model = Some(m.clone());
            let config_id = opened["configOptions"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|o| o["category"] == "model")
                .and_then(|o| o["id"].as_str().map(String::from));
            // A resume reply may omit configOptions: try the standard config
            // option first, then the older set_model.
            let cid = config_id.unwrap_or_else(|| "model".into());
            let mut res = self.peer.request("session/set_config_option", json!({"sessionId": self.session_id, "configId": cid, "value": m}), ACK).await;
            if res.is_err() && (opened["models"].is_object() || spec.resume.is_some()) {
                res = self.peer.request("session/set_model", json!({"sessionId": self.session_id, "modelId": m}), ACK).await;
            }
            if let Err(e) = res {
                bail!("{} rejected model `{m}`: {e:#}", self.name);
            }
        }
        let _ = self.tx.send((self.generation, DriverMsg::Event(AgentEvent::Info { native_id: Some(self.session_id.clone()), model })));
        Ok(())
    }

    pub async fn prompt(&mut self, text: &str) -> Result<()> {
        {
            let mut s = self.state.lock().unwrap();
            s.wrote_text = false;
            s.usage = None;
        }
        let (peer, tx, gen, sid, state) = (self.peer.clone(), self.tx.clone(), self.generation, self.session_id.clone(), self.state.clone());
        let params = json!({"sessionId": sid, "prompt": [{"type": "text", "text": text}]});
        // The reply is the end of the turn.
        tokio::spawn(async move {
            let res = peer.request("session/prompt", params, TURN).await;
            let usage = state.lock().unwrap().usage.take();
            let msg = match res {
                Ok(r) => match r["stopReason"].as_str().unwrap_or("end_turn") {
                    "end_turn" => DriverMsg::TurnDone { outcome: "completed".into(), error: None, usage: usage.or(r.get("usage").cloned()) },
                    "cancelled" => DriverMsg::TurnDone { outcome: "aborted".into(), error: None, usage },
                    other => DriverMsg::TurnDone { outcome: "failed".into(), error: Some(format!("stopped: {other}")), usage },
                },
                Err(e) if e.to_string().contains("process exited") => return,
                Err(e) => DriverMsg::TurnDone { outcome: "failed".into(), error: Some(clean_error(&e.to_string())), usage },
            };
            let _ = tx.send((gen, msg));
        });
        Ok(())
    }

    pub async fn interrupt(&mut self) -> Result<()> {
        // Outstanding permission requests must be answered `cancelled`.
        let pending: Vec<(String, Value)> = self.approvals.lock().unwrap().drain().map(|(k, (id, _))| (k, id)).collect();
        for (key, id) in pending {
            let _ = self.peer.reply(&id, json!({"outcome": {"outcome": "cancelled"}})).await;
            let _ = self.tx.send((self.generation, DriverMsg::PermissionCancelled(key)));
        }
        self.peer.notify("session/cancel", json!({"sessionId": self.session_id})).await
    }

    pub async fn respond(&mut self, key: &str, decision: Decision) -> Result<()> {
        let Some((id, options)) = self.approvals.lock().unwrap().remove(key) else { return Ok(()) };
        let kinds: &[&str] = match decision {
            Decision::Allow => &["allow_once", "allow_always"],
            Decision::AllowSession => &["allow_always", "allow_once"],
            Decision::Deny => &["reject_once", "reject_always"],
        };
        let pick = kinds.iter().find_map(|k| options.iter().find(|o| o["kind"] == *k));
        let outcome = match pick.and_then(|o| o["optionId"].as_str()) {
            Some(opt) => json!({"outcome": {"outcome": "selected", "optionId": opt}}),
            None => json!({"outcome": {"outcome": "cancelled"}}),
        };
        self.peer.reply(&id, outcome).await
    }

    pub async fn shutdown(mut self) {
        kill_tree(&mut self.child).await;
    }
}

fn current_model(opened: &Value) -> Option<String> {
    opened["configOptions"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|o| o["category"] == "model")
        .and_then(|o| o["currentValue"].as_str().map(String::from))
        .or_else(|| opened["models"]["currentModelId"].as_str().map(String::from))
}

/// Models offered by the agent, from a session's open reply.
pub(crate) fn models_of(opened: &Value) -> Vec<Value> {
    if let Some(opt) = opened["configOptions"].as_array().into_iter().flatten().find(|o| o["category"] == "model") {
        let mut out = Vec::new();
        for o in opt["options"].as_array().into_iter().flatten() {
            // Options may be grouped.
            if let Some(group) = o["options"].as_array() {
                for g in group {
                    out.push(json!({"id": g["value"], "name": g["name"]}));
                }
            } else {
                out.push(json!({"id": o["value"], "name": o["name"]}));
            }
        }
        return out;
    }
    opened["models"]["availableModels"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|m| json!({"id": m["modelId"], "name": m["name"], "description": m["description"]}))
        .collect()
}

/// ACP errors often wrap the provider's JSON; keep the readable part.
fn clean_error(s: &str) -> String {
    let s = s.replace("\\n", " ").replace("\\\"", "\"");
    if let Some(i) = s.find("\"message\":") {
        let rest = &s[i + 10..];
        let rest = rest.trim_start_matches([' ', '"', '{', '\\']);
        let end = rest.find(['"', '\\']).unwrap_or(rest.len().min(300));
        let msg = &rest[..end];
        if !msg.is_empty() && !msg.starts_with('{') {
            return msg.to_string();
        }
    }
    s.chars().take(600).collect()
}

fn content_text(v: &Value) -> String {
    match v {
        Value::Array(items) => items.iter().map(content_text).filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n"),
        Value::Object(o) => {
            if let Some(t) = o.get("text").and_then(|t| t.as_str()) {
                return t.to_string();
            }
            if let Some(c) = o.get("content") {
                return content_text(c);
            }
            if o.get("type").and_then(|t| t.as_str()) == Some("diff") {
                return o.get("path").and_then(|p| p.as_str()).unwrap_or("diff").to_string();
            }
            String::new()
        }
        Value::String(s) => s.clone(),
        _ => String::new(),
    }
}

fn map_update(u: &Value, state: &Arc<Mutex<State>>) -> Vec<DriverMsg> {
    let mut s = state.lock().unwrap();
    if !s.live {
        return vec![];
    }
    let ev = |e: AgentEvent| vec![DriverMsg::Event(e)];
    match u["sessionUpdate"].as_str().unwrap_or("") {
        "agent_message_chunk" => {
            let t = content_text(&u["content"]);
            if t.is_empty() {
                return vec![];
            }
            s.wrote_text = true;
            ev(AgentEvent::TextDelta { text: t, phase: None })
        }
        "agent_thought_chunk" => ev(AgentEvent::Reasoning { text: content_text(&u["content"]) }),
        "tool_call" => {
            let id = u["toolCallId"].as_str().unwrap_or_default().to_string();
            if !s.started_tools.insert(id.clone()) {
                return vec![];
            }
            let mut out = Vec::new();
            if s.wrote_text {
                s.wrote_text = false;
                out.push(DriverMsg::Event(AgentEvent::TextDelta { text: "\n\n".into(), phase: None }));
            }
            let name = u["title"].as_str().or(u["kind"].as_str()).unwrap_or("tool").to_string();
            let input = u.get("rawInput").cloned().unwrap_or_else(|| json!({"kind": u["kind"], "locations": u["locations"]}));
            out.push(DriverMsg::Event(AgentEvent::ToolStart { call_id: id, name, input }));
            out
        }
        "tool_call_update" => {
            let id = u["toolCallId"].as_str().unwrap_or_default().to_string();
            match u["status"].as_str() {
                Some(st @ ("completed" | "failed")) => {
                    let mut out = Vec::new();
                    if s.started_tools.insert(id.clone()) {
                        out.push(DriverMsg::Event(AgentEvent::ToolStart {
                            call_id: id.clone(),
                            name: u["title"].as_str().unwrap_or("tool").to_string(),
                            input: u.get("rawInput").cloned().unwrap_or(Value::Null),
                        }));
                    }
                    let (mut output, mut images) = split_content(&u["content"], s.limit);
                    if output.is_empty() && images.is_empty() {
                        (output, images) = split_content(&u["rawOutput"], s.limit);
                    }
                    out.push(DriverMsg::Event(AgentEvent::ToolEnd { call_id: id, ok: st == "completed", output, images }));
                    out
                }
                _ => vec![],
            }
        }
        "plan" => ev(AgentEvent::Plan { entries: u["entries"].clone() }),
        "usage_update" => {
            s.usage = Some(u.clone());
            vec![]
        }
        _ => vec![],
    }
}

/// Models offered by an ACP agent: open a throwaway session, read them,
/// and close/delete it where the agent supports that.
pub async fn list_models(bin: &Path, def: &super::AgentDef) -> Result<Vec<Value>> {
    let mut spec = super::probe_spec(&def.id, super::Protocol::Acp);
    spec.def = def.clone();
    let mut cmd = base_command(bin, &spec);
    cmd.args(&def.args);
    let mut child = cmd.spawn()?;
    let peer = Peer::new(child.stdin.take().unwrap(), "acp");
    let p = peer.clone();
    let reader = spawn_frame_reader(child.stdout.take().unwrap(), move |m| {
        if !p.route_reply(&m) {
            if let Some(id) = m.get("id").cloned() {
                let p = p.clone();
                tokio::spawn(async move {
                    let _ = p.reply_error(&id, -32601, "not supported").await;
                });
            }
        }
    });
    let result = async {
        let t = Duration::from_secs(30);
        let init = peer
            .request("initialize", json!({"protocolVersion": 1, "clientCapabilities": {}, "clientInfo": {"name": "agl", "version": crate::protocol::VERSION}}), t)
            .await?;
        let r = peer.request("session/new", json!({"cwd": std::env::temp_dir(), "mcpServers": []}), t).await?;
        let caps = &init["agentCapabilities"]["sessionCapabilities"];
        if let Some(sid) = r["sessionId"].as_str() {
            for m in ["session/delete", "session/close"] {
                let key = m.trim_start_matches("session/");
                if caps.get(key).is_some() {
                    let _ = peer.request(m, json!({"sessionId": sid}), Duration::from_secs(5)).await;
                    break;
                }
            }
        }
        anyhow::Ok(models_of(&r))
    }
    .await;
    reader.abort();
    kill_tree(&mut child).await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_session_updates() {
        let s = Arc::new(Mutex::new(State { live: true, limit: 1024, ..Default::default() }));
        let text = map_update(&json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "hi"}}), &s);
        assert!(matches!(&text[0], DriverMsg::Event(AgentEvent::TextDelta { text, .. }) if text == "hi"));

        let start = map_update(&json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Bash", "rawInput": {"command": "ls"}}), &s);
        assert!(start.iter().any(|m| matches!(m, DriverMsg::Event(AgentEvent::ToolStart { call_id, .. }) if call_id == "t1")));
        assert!(map_update(&json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "in_progress"}), &s).is_empty());
        let end = map_update(&json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "failed",
            "content": [{"type": "content", "content": {"type": "text", "text": "denied"}}]}), &s);
        assert!(matches!(&end[0], DriverMsg::Event(AgentEvent::ToolEnd { ok: false, output, .. }) if output == "denied"));

        s.lock().unwrap().live = false;
        assert!(map_update(&json!({"sessionUpdate": "agent_message_chunk", "content": {"text": "replayed"}}), &s).is_empty(), "history replay is skipped");
    }

    #[test]
    fn reads_models_and_modes() {
        let opened = json!({"configOptions": [{"id": "model", "category": "model", "currentValue": "k3",
            "options": [{"value": "k3", "name": "K3"}, {"name": "group", "options": [{"value": "k2", "name": "K2"}]}]}]});
        let models = models_of(&opened);
        assert_eq!(models.len(), 2);
        assert_eq!(current_model(&opened).as_deref(), Some("k3"));
        let gemini = json!({"models": {"currentModelId": "auto", "availableModels": [{"modelId": "auto", "name": "Auto"}]}});
        assert_eq!(models_of(&gemini)[0]["id"], "auto");
        assert_eq!(modes_for("full")[0], "yolo");
        assert_eq!(modes_for("read_only")[0], "plan");
    }

    #[test]
    fn cleans_provider_errors() {
        let raw = r#"Internal error: {"error":{"message":"Permission denied: project suspended","code":403}}"#;
        assert_eq!(clean_error(raw), "Permission denied: project suspended");
    }
}
