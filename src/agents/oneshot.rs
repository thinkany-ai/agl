//! CLIs without a persistent protocol: one process per turn, JSON lines on
//! stdout, conversation continuity through the CLI's own resume flag.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Child;

use super::{base_command, clip, kill_tree, split_content, spawn_frame_reader, spawn_stderr_tail, track, DriverMsg, DriverTx, Protocol, SpawnSpec};
use crate::protocol::AgentEvent;

pub struct OneShotDriver {
    bin: PathBuf,
    spec: SpawnSpec,
    tx: DriverTx,
    session: Arc<Mutex<Option<String>>>,
    child: Arc<tokio::sync::Mutex<Option<Child>>>,
}

#[derive(Default)]
struct Turn {
    text_seen: bool,
    final_text: Option<String>,
    error: Option<String>,
    usage: Option<Value>,
}

impl OneShotDriver {
    pub fn new(bin: PathBuf, spec: &SpawnSpec, tx: DriverTx) -> Self {
        OneShotDriver { bin, spec: spec.clone(), tx, session: Arc::new(Mutex::new(spec.resume.clone())), child: Arc::default() }
    }

    pub async fn prompt(&mut self, text: &str) -> Result<()> {
        let mut cmd = base_command(&self.bin, &self.spec);
        let resume = self.session.lock().unwrap().clone();
        let mut stdin_text = None;
        match self.spec.def.protocol {
            Protocol::Cursor => {
                cmd.args(["--print", "--output-format", "stream-json", "--stream-partial-output", "--trust", "--approve-mcps"]);
                match self.spec.access.as_str() {
                    "full" => {
                        cmd.arg("--force");
                    }
                    "read_only" => {
                        cmd.args(["--mode", "ask"]);
                    }
                    _ => {}
                }
                if let Some(id) = &resume {
                    cmd.args(["--resume", id]);
                }
                if let Some(m) = &self.spec.model {
                    cmd.args(["--model", m]);
                }
                cmd.arg("--").arg(text);
            }
            _ => {
                cmd.args(["--mode", "json"]);
                if let Some(id) = &resume {
                    cmd.args(["--session", id]);
                }
                if let Some(m) = &self.spec.model {
                    cmd.args(["--model", m]);
                }
                stdin_text = Some(text.to_string());
            }
        }
        let mut child = cmd.spawn().with_context(|| format!("spawn {}", self.bin.display()))?;
        if let Some(pid) = child.id() {
            track(pid);
        }
        let mut stdin = child.stdin.take().unwrap();
        if let Some(t) = stdin_text {
            stdin.write_all(t.as_bytes()).await?;
        }
        drop(stdin);
        let stdout = child.stdout.take().unwrap();
        let tail = spawn_stderr_tail(child.stderr.take().unwrap());
        *self.child.lock().await = Some(child);

        let (tx, gen, session, slot, limit, proto) =
            (self.tx.clone(), self.spec.generation, self.session.clone(), self.child.clone(), self.spec.tool_output_limit, self.spec.def.protocol);
        let turn = Arc::new(Mutex::new(Turn::default()));
        let t = turn.clone();
        let t2 = tx.clone();
        let reader = spawn_frame_reader(stdout, move |f| {
            let mut turn = t.lock().unwrap();
            for msg in map(proto, &f, &mut turn, limit) {
                if let DriverMsg::NativeId(id) = &msg {
                    *session.lock().unwrap() = Some(id.clone());
                }
                let _ = t2.send((gen, msg));
            }
        });
        tokio::spawn(async move {
            let _ = reader.await;
            let status = match slot.lock().await.take() {
                Some(mut c) => {
                    if let Some(pid) = c.id() {
                        super::untrack(pid);
                    }
                    c.wait().await.ok()
                }
                None => None, // interrupted: the child was taken and killed
            };
            let mut turn = std::mem::take(&mut *turn.lock().unwrap());
            if !turn.text_seen {
                if let Some(text) = turn.final_text.take().filter(|s| !s.is_empty()) {
                    let _ = tx.send((gen, DriverMsg::Event(AgentEvent::TextDelta { text, phase: None })));
                }
            }
            let msg = match status {
                None => DriverMsg::TurnDone { outcome: "aborted".into(), error: None, usage: turn.usage },
                Some(s) if s.success() && turn.error.is_none() => DriverMsg::TurnDone { outcome: "completed".into(), error: None, usage: turn.usage },
                Some(s) => {
                    let err = turn.error.unwrap_or_else(|| {
                        let tail = tail.lock().unwrap().join("\n");
                        if tail.is_empty() { format!("exited with {s}") } else { tail }
                    });
                    DriverMsg::TurnDone { outcome: "failed".into(), error: Some(err), usage: turn.usage }
                }
            };
            let _ = tx.send((gen, msg));
        });
        Ok(())
    }

    pub async fn interrupt(&mut self) -> Result<()> {
        if let Some(mut c) = self.child.lock().await.take() {
            kill_tree(&mut c).await;
        }
        Ok(())
    }

    pub async fn shutdown(mut self) {
        let _ = self.interrupt().await;
    }
}

fn map(proto: Protocol, f: &Value, turn: &mut Turn, limit: usize) -> Vec<DriverMsg> {
    let ev = |e: AgentEvent| DriverMsg::Event(e);
    let mut out = Vec::new();
    match proto {
        Protocol::Cursor => {
            if let Some(id) = f["session_id"].as_str() {
                if f["type"] == "system" {
                    out.push(DriverMsg::NativeId(id.into()));
                    out.push(ev(AgentEvent::Info { native_id: Some(id.into()), model: f["model"].as_str().map(String::from) }));
                }
            }
            match (f["type"].as_str().unwrap_or(""), f["subtype"].as_str()) {
                ("thinking", Some("delta")) => out.push(ev(AgentEvent::Reasoning { text: f["text"].as_str().unwrap_or_default().into() })),
                ("assistant", _) => {
                    // With --stream-partial-output each event is a delta; the
                    // final full message carries timestamp-less duplicate text.
                    let text: String = f["message"]["content"].as_array().into_iter().flatten().filter_map(|c| c["text"].as_str()).collect();
                    if !text.is_empty() && f.get("timestamp_ms").is_some() {
                        turn.text_seen = true;
                        out.push(ev(AgentEvent::TextDelta { text, phase: None }));
                    } else if !text.is_empty() && !turn.text_seen {
                        turn.final_text = Some(text);
                    }
                }
                ("tool_call", Some(sub @ ("started" | "completed"))) => {
                    let call = f["tool_call"].as_object();
                    let (kind, body) = call.and_then(|o| o.iter().next()).map(|(k, v)| (k.clone(), v.clone())).unwrap_or_default();
                    let name = kind.trim_end_matches("ToolCall").to_string();
                    let id = f["call_id"].as_str().unwrap_or_default().to_string();
                    if sub == "started" {
                        out.push(ev(AgentEvent::ToolStart { call_id: id, name, input: body["args"].clone() }));
                    } else {
                        let res = &body["result"];
                        let failed = res.get("error").is_some() || res.get("failure").is_some();
                        out.push(ev(AgentEvent::ToolEnd { call_id: id, ok: !failed, output: clip(&res.to_string(), limit), images: Vec::new() }));
                    }
                }
                ("result", _) => {
                    turn.usage = Some(f["usage"].clone());
                    if f["is_error"].as_bool().unwrap_or(false) {
                        turn.error = Some(f["result"].as_str().unwrap_or("error").to_string());
                    } else if !turn.text_seen {
                        turn.final_text = f["result"].as_str().map(String::from);
                    }
                }
                _ => {}
            }
        }
        _ => match f["type"].as_str().unwrap_or("") {
            "session" => {
                if let Some(id) = f["id"].as_str() {
                    out.push(DriverMsg::NativeId(id.into()));
                    out.push(ev(AgentEvent::Info { native_id: Some(id.into()), model: None }));
                }
            }
            "message_update" => {
                let e = &f["assistantMessageEvent"];
                match e["type"].as_str() {
                    Some("text_delta") => {
                        turn.text_seen = true;
                        out.push(ev(AgentEvent::TextDelta { text: e["delta"].as_str().unwrap_or_default().into(), phase: None }));
                    }
                    Some("thinking_delta") => out.push(ev(AgentEvent::Reasoning { text: e["delta"].as_str().unwrap_or_default().into() })),
                    _ => {}
                }
            }
            "message_end" if f["message"]["role"] == "assistant" => {
                let m = &f["message"];
                let text: String = m["content"].as_array().into_iter().flatten().filter_map(|c| c["text"].as_str()).collect();
                if !text.is_empty() {
                    turn.final_text = Some(text);
                }
                turn.usage = Some(m["usage"].clone());
            }
            "turn_end" => {
                let m = &f["message"];
                if m["stopReason"] == "error" {
                    turn.error = Some(m["errorMessage"].as_str().unwrap_or("model request failed").to_string());
                }
            }
            "tool_execution_start" => out.push(ev(AgentEvent::ToolStart {
                call_id: f["toolCallId"].as_str().unwrap_or_default().into(),
                name: f["toolName"].as_str().unwrap_or("tool").into(),
                input: f["args"].clone(),
            })),
            "tool_execution_end" => {
                let (output, images) = split_content(&f["result"], limit);
                out.push(ev(AgentEvent::ToolEnd {
                    call_id: f["toolCallId"].as_str().unwrap_or_default().into(),
                    ok: !f["isError"].as_bool().unwrap_or(false),
                    output,
                    images,
                }));
            }
            _ => {}
        },
    }
    out
}
