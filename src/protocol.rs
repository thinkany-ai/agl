//! Wire protocol shared by the local daemon socket, the hub link and the hub.
//!
//! Every message is one JSON object discriminated by `type`:
//! - `request`  {id, method, params}       caller -> daemon
//! - `response` {id, result?, error?}      daemon -> caller
//! - `event`    {session_id, seq, ts, event} daemon -> subscribers
//! - `hello`    {machine}                  daemon -> hub, first frame of a link

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireMsg {
    Request {
        id: String,
        method: String,
        #[serde(default)]
        params: Value,
    },
    Response {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    Event(EventEnvelope),
    Hello {
        machine: MachineInfo,
    },
    /// Hub -> machine after a pairing-code connect: the machine's own token.
    Paired {
        token: String,
    },
}

impl WireMsg {
    pub fn reply(id: String, res: anyhow::Result<Value>) -> Self {
        match res {
            Ok(v) => WireMsg::Response { id, result: Some(v), error: None },
            Err(e) => WireMsg::Response { id, result: None, error: Some(format!("{e:#}")) },
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("wire message serializes")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub session_id: String,
    pub seq: u64,
    pub ts: u64,
    pub event: AgentEvent,
}

/// Runtime-independent facts about a session. Every agent maps its native
/// stream onto these.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentEvent {
    /// Lifecycle: idle | running | suspended | closed | starting
    Status { status: String },
    /// A prompt was accepted (echoed so every viewer sees every input).
    UserMessage { turn_id: String, text: String },
    /// Prompt waits behind the running turn.
    Queued { turn_id: String, position: usize },
    TurnStarted { turn_id: String },
    TextDelta { text: String },
    Reasoning { text: String },
    ToolStart {
        call_id: String,
        name: String,
        #[serde(default)]
        input: Value,
    },
    ToolEnd {
        call_id: String,
        ok: bool,
        #[serde(default)]
        output: String,
    },
    TurnEnded {
        turn_id: String,
        /// completed | failed | aborted
        outcome: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Value>,
        /// launch_ms, first_event_ms, first_text_ms, total_ms, warm
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timing: Option<Value>,
    },
    /// The agent asks before running a tool. Answer with `session.approve`.
    PermissionRequest {
        request_id: String,
        tool: String,
        #[serde(default)]
        title: String,
        #[serde(default)]
        input: Value,
        /// Decisions this request accepts: allow, allow_session, deny.
        options: Vec<String>,
        /// Unix ms after which agl denies it.
        expires_at: u64,
    },
    /// decision: allow | allow_session | deny; by: client | policy | timeout | cancelled
    PermissionResolved { request_id: String, decision: String, by: String },
    /// Every 15s while a turn runs. `stalled` after 60s without agent output.
    Heartbeat { turn_id: String, elapsed_ms: u64, idle_ms: u64, stalled: bool },
    /// The runtime is retrying a failed model request.
    Retrying {
        #[serde(default)]
        message: String,
    },
    /// A prompt was injected into the running turn instead of queued.
    Steered { turn_id: String, into_turn_id: String },
    /// The agent's plan / todo list (ACP `plan`, claude TodoWrite).
    Plan { entries: Value },
    /// Native identity / model reported by the runtime.
    Info {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        native_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    Error { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MachineInfo {
    pub machine_id: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub version: String,
    pub agents: Vec<AgentInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInfo {
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// claude | codex | acp | oneshot
    #[serde(default)]
    pub protocol: String,
    pub available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// What agl can do with it: warm (persistent process), approvals, steer, models.
    #[serde(default)]
    pub features: Vec<String>,
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn short_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
}
