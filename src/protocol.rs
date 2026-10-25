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

/// Wire protocol version, sent in `hello` and `ping`. Bumped only for
/// incompatible changes; new fields and events are additive.
pub const PROTOCOL: u32 = 1;

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
    /// One piece of a message too large for the hub's request size limit
    /// (HTTP link): `data` is base64 of bytes `index` of `count` of the
    /// message's JSON, which the hub joins and reads once it has them all.
    Part {
        id: String,
        index: u32,
        count: u32,
        data: String,
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
    /// Text for the user. `phase: "commentary"` marks an interim update the
    /// agent sends while it keeps working (Codex), not part of its answer.
    TextDelta {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        phase: Option<String>,
    },
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
        /// The tool's text output, capped at `tool_output_limit`.
        #[serde(default)]
        output: String,
        /// Images the tool returned, e.g. computer-use screenshots.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ToolImage>,
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

/// An image in a tool result, base64-encoded.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolImage {
    pub mime_type: String,
    /// Base64 data; empty when `omitted`.
    #[serde(default)]
    pub data: String,
    /// The data was dropped: too large, or aged out of the history.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub omitted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MachineInfo {
    pub machine_id: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub version: String,
    /// Wire protocol version (`PROTOCOL`); 0 from agl ≤0.1.
    #[serde(default)]
    pub protocol: u32,
    pub agents: Vec<AgentInfo>,
    /// The user's home directory, for hubs that address files under it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub home: String,
    /// When this daemon started (unix ms). A new value means it restarted, so
    /// no turn the hub saw running on the old one is running any more.
    #[serde(default)]
    pub started_at: u64,
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
    /// ready | desktop-only (only its desktop app is installed) | not-found.
    /// Empty from agl 0.2.0 and earlier.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub status: String,
    /// The desktop app that ships this agent (macOS).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desktop_app: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install: Option<InstallInfo>,
    /// Only when updates were checked: the newest published version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_version: Option<String>,
    /// Only when updates were checked: available | current | unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<String>,
}

/// How an agent is installed or updated. Commands are shell commands for the
/// user to confirm; agl never runs them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallInfo {
    /// For an installed agent: npm | bun | brew | native | app-bundle | unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// npm package.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    /// npm global prefix the installed CLI lives in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// For a missing agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_command: Option<String>,
    /// For an installed agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guide: Option<String>,
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
