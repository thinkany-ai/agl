//! Session manager.
//!
//! Each session is an actor task that owns at most one agent process:
//! - prompts run one turn at a time per session (queued FIFO, or steered into
//!   the running turn), while different sessions run concurrently;
//! - the process stays alive between turns, so the conversation context is
//!   reused in-process; a suspended session respawns with the runtime's
//!   native resume id, so context survives suspension and daemon restarts;
//! - live processes are bounded by `max_live` (a hard budget: the least
//!   recently used idle session is evicted, otherwise the turn waits);
//! - permission requests go to clients and are denied on timeout;
//! - the garbage collector suspends idle processes and forgets sessions past
//!   their TTL.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, oneshot, OwnedSemaphorePermit, Semaphore};

use crate::agents::{self, Decision, Driver, DriverMsg, Protocol, SpawnSpec, API_KEY_VARS};
use crate::config::{self, Config};
use crate::protocol::{now_ms, short_id, AgentEvent, EventEnvelope};
use crate::shellenv;

pub const ACCESS_LEVELS: &[&str] = &["full", "edit", "ask", "read_only"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    pub agent: String,
    pub cwd: PathBuf,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    /// full | edit | ask | read_only
    #[serde(default = "default_access")]
    pub access: String,
    /// The runtime's own conversation id (claude session id / codex thread id / ACP session id).
    #[serde(default)]
    pub native_id: Option<String>,
    #[serde(default)]
    pub status: String,
    pub created_at: u64,
    pub last_active_at: u64,
    #[serde(default)]
    pub turns: u64,
    #[serde(default)]
    pub queued: usize,
    /// Whether an agent process is currently running for this session.
    #[serde(default)]
    pub live: bool,
    /// Permission requests waiting for an answer.
    #[serde(default)]
    pub pending_approvals: usize,
    /// warm, approvals, steer, interrupt, models, …
    #[serde(default)]
    pub features: Vec<String>,
    /// Claude rejected the API key; run it with its own login instead.
    #[serde(default)]
    pub use_login: bool,
}

fn default_access() -> String {
    "full".into()
}

enum Cmd {
    Prompt { turn_id: String, text: String, mode: String },
    Abort,
    Suspend,
    Warm,
    Approve { request_id: String, decision: Decision, done: oneshot::Sender<Result<()>> },
    Reconfigure,
    Close(oneshot::Sender<()>),
}

struct Shared {
    meta: Mutex<SessionMeta>,
    history: Mutex<VecDeque<EventEnvelope>>,
    seq: AtomicU64,
}

struct Handle {
    shared: Arc<Shared>,
    tx: mpsc::UnboundedSender<Cmd>,
}

pub struct Manager {
    pub cfg: RwLock<Config>,
    /// Claude rejected an exported API key on this machine: start new claude
    /// sessions with the CLI's own login (remembered in state.json).
    claude_use_login: std::sync::atomic::AtomicBool,
    sessions: Mutex<HashMap<String, Handle>>,
    pub bus: broadcast::Sender<EventEnvelope>,
    generation: AtomicU64,
    budget: Arc<Semaphore>,
}

#[derive(Debug, Default, Deserialize)]
pub struct CreateParams {
    pub agent: String,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    /// full | edit | ask | read_only (default from config).
    #[serde(default)]
    pub access: Option<String>,
    /// Reuse the session with this name if it exists.
    #[serde(default)]
    pub name: Option<String>,
    /// Attach to an existing native conversation id.
    #[serde(default)]
    pub resume: Option<String>,
    /// Start the agent process now so the first prompt is warm.
    #[serde(default)]
    pub warm: bool,
}

#[derive(Debug, Default, Deserialize)]
pub struct ConfigureParams {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub access: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

impl Manager {
    pub fn new(cfg: Config) -> Arc<Manager> {
        let (bus, _) = broadcast::channel(16384);
        let budget = Arc::new(Semaphore::new(cfg.max_live.max(1)));
        let use_login = std::fs::read_to_string(config::home().join("state.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|v| v["claude_use_login"].as_bool())
            .unwrap_or(false);
        Arc::new(Manager {
            cfg: RwLock::new(cfg),
            claude_use_login: std::sync::atomic::AtomicBool::new(use_login),
            sessions: Mutex::default(),
            bus,
            generation: AtomicU64::new(1),
            budget,
        })
    }

    pub fn cfg(&self) -> Config {
        self.cfg.read().unwrap().clone()
    }

    /// Restore sessions saved by a previous daemon; they start suspended.
    pub fn load(self: &Arc<Self>) {
        let Ok(s) = std::fs::read_to_string(config::sessions_path()) else { return };
        let Ok(metas) = serde_json::from_str::<Vec<SessionMeta>>(&s) else { return };
        for mut m in metas {
            m.status = "suspended".into();
            m.live = false;
            m.queued = 0;
            m.pending_approvals = 0;
            let history = load_history(&m.id);
            let shared = self.start_actor(m);
            let last = history.back().map(|e| e.seq).unwrap_or(0);
            shared.seq.store(last, Ordering::SeqCst);
            *shared.history.lock().unwrap() = history;
        }
    }

    fn persist(&self) {
        let metas: Vec<SessionMeta> = self.sessions.lock().unwrap().values().map(|h| h.shared.meta.lock().unwrap().clone()).collect();
        let _ = std::fs::create_dir_all(config::home());
        let _ = config::write_private(&config::sessions_path(), serde_json::to_string_pretty(&metas).unwrap_or_default().as_bytes());
    }

    fn start_actor(self: &Arc<Self>, meta: SessionMeta) -> Arc<Shared> {
        let id = meta.id.clone();
        let shared = Arc::new(Shared { meta: Mutex::new(meta), history: Mutex::default(), seq: AtomicU64::new(0) });
        let (tx, rx) = mpsc::unbounded_channel();
        self.sessions.lock().unwrap().insert(id, Handle { shared: shared.clone(), tx });
        tokio::spawn(actor(self.clone(), shared.clone(), rx));
        shared
    }

    /// Resolve a session by id, name or unique id prefix (≥4 chars).
    pub fn resolve(&self, key: &str) -> Option<String> {
        let sessions = self.sessions.lock().unwrap();
        if sessions.contains_key(key) {
            return Some(key.to_string());
        }
        let by_name = sessions
            .values()
            .map(|h| h.shared.meta.lock().unwrap())
            .find(|m| m.name.as_deref() == Some(key))
            .map(|m| m.id.clone());
        by_name.or_else(|| {
            let mut hits = sessions.keys().filter(|k| k.starts_with(key));
            match (hits.next(), hits.next()) {
                (Some(k), None) if key.len() >= 4 => Some(k.clone()),
                _ => None,
            }
        })
    }

    fn handle(&self, key: &str) -> Result<(String, Arc<Shared>, mpsc::UnboundedSender<Cmd>)> {
        let id = self.resolve(key).ok_or_else(|| anyhow!("no session `{key}`"))?;
        let sessions = self.sessions.lock().unwrap();
        let h = sessions.get(&id).ok_or_else(|| anyhow!("no session `{key}`"))?;
        Ok((id, h.shared.clone(), h.tx.clone()))
    }

    pub fn create(self: &Arc<Self>, p: CreateParams) -> Result<SessionMeta> {
        let cfg = self.cfg();
        let Some(def) = agents::find_def(&cfg, &p.agent) else {
            let ids: Vec<String> = agents::registry(&cfg).into_iter().map(|d| d.id).collect();
            bail!("unknown agent `{}` (available: {})", p.agent, ids.join(", "));
        };
        let access = p.access.unwrap_or_else(|| cfg.default_access.clone());
        if !ACCESS_LEVELS.contains(&access.as_str()) {
            bail!("access must be one of {}", ACCESS_LEVELS.join(", "));
        }
        if let Some(name) = &p.name {
            if let Some(id) = self.resolve(name) {
                let (_, shared, tx) = self.handle(&id)?;
                let meta = shared.meta.lock().unwrap().clone();
                if meta.agent != p.agent {
                    bail!("session `{name}` exists with agent `{}`", meta.agent);
                }
                if p.warm {
                    let _ = tx.send(Cmd::Warm);
                }
                return Ok(meta);
            }
        }
        let cwd = match p.cwd {
            Some(c) => expand_home(c),
            None => dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")),
        };
        if !cwd.is_dir() {
            bail!("cwd `{}` is not a directory", cwd.display());
        }
        let now = now_ms();
        let meta = SessionMeta {
            id: short_id(),
            name: p.name,
            agent: p.agent,
            cwd,
            model: p.model,
            effort: p.effort,
            access,
            native_id: p.resume,
            status: "idle".into(),
            created_at: now,
            last_active_at: now,
            turns: 0,
            queued: 0,
            live: false,
            pending_approvals: 0,
            features: def.protocol.features(),
            use_login: false,
        };
        let shared = self.start_actor(meta.clone());
        if p.warm {
            let _ = self.sessions.lock().unwrap().get(&meta.id).map(|h| h.tx.send(Cmd::Warm));
        }
        drop(shared);
        self.persist();
        Ok(meta)
    }

    /// mode: queue (default) | steer (into the running turn, else queue) |
    /// interrupt (abort the running turn, then run this).
    pub fn prompt(&self, key: &str, text: String, mode: Option<&str>, turn_id: Option<String>) -> Result<Value> {
        if text.trim().is_empty() {
            bail!("empty prompt");
        }
        let mode = mode.unwrap_or("queue");
        if !["queue", "steer", "interrupt"].contains(&mode) {
            bail!("mode must be queue, steer or interrupt");
        }
        let (id, shared, tx) = self.handle(key)?;
        let turn_id = turn_id.filter(|t| !t.is_empty() && t.len() <= 64).unwrap_or_else(short_id);
        shared.meta.lock().unwrap().last_active_at = now_ms();
        self.emit(&shared, AgentEvent::UserMessage { turn_id: turn_id.clone(), text: text.clone() });
        tx.send(Cmd::Prompt { turn_id: turn_id.clone(), text, mode: mode.into() }).map_err(|_| anyhow!("session is closed"))?;
        Ok(json!({"session_id": id, "turn_id": turn_id}))
    }

    pub fn abort(&self, key: &str) -> Result<()> {
        let (_, _, tx) = self.handle(key)?;
        tx.send(Cmd::Abort).map_err(|_| anyhow!("session is closed"))
    }

    pub fn warm(&self, key: &str) -> Result<()> {
        let (_, _, tx) = self.handle(key)?;
        tx.send(Cmd::Warm).map_err(|_| anyhow!("session is closed"))
    }

    pub async fn approve(&self, key: &str, request_id: &str, decision: &str) -> Result<()> {
        let decision = Decision::parse(decision).ok_or_else(|| anyhow!("decision must be allow, allow_session or deny"))?;
        let (_, _, tx) = self.handle(key)?;
        let (done, wait) = oneshot::channel();
        tx.send(Cmd::Approve { request_id: request_id.into(), decision, done }).map_err(|_| anyhow!("session is closed"))?;
        wait.await.map_err(|_| anyhow!("session is closed"))?
    }

    /// Change model / effort / access. A live process is restarted with
    /// native resume at the next turn, so the conversation is kept.
    pub fn configure(&self, key: &str, p: ConfigureParams) -> Result<SessionMeta> {
        let (_, shared, tx) = self.handle(key)?;
        if let Some(a) = &p.access {
            if !ACCESS_LEVELS.contains(&a.as_str()) {
                bail!("access must be one of {}", ACCESS_LEVELS.join(", "));
            }
        }
        if let Some(n) = &p.name {
            if let Some(other) = self.resolve(n) {
                if other != shared.meta.lock().unwrap().id {
                    bail!("name `{n}` is taken");
                }
            }
        }
        let meta = {
            let mut m = shared.meta.lock().unwrap();
            let relaunch = p.model.is_some() || p.effort.is_some() || p.access.is_some();
            if let Some(v) = p.model {
                m.model = Some(v).filter(|s| !s.is_empty());
            }
            if let Some(v) = p.effort {
                m.effort = Some(v).filter(|s| !s.is_empty());
            }
            if let Some(v) = p.access {
                m.access = v;
            }
            if let Some(v) = p.name {
                m.name = Some(v).filter(|s| !s.is_empty());
            }
            if relaunch {
                let _ = tx.send(Cmd::Reconfigure);
            }
            m.clone()
        };
        self.persist();
        Ok(meta)
    }

    pub async fn close(&self, key: &str) -> Result<()> {
        let (id, _, tx) = self.handle(key)?;
        let (done, wait) = oneshot::channel();
        if tx.send(Cmd::Close(done)).is_ok() {
            let _ = wait.await;
        }
        self.sessions.lock().unwrap().remove(&id);
        let _ = std::fs::remove_file(history_path(&id));
        self.persist();
        Ok(())
    }

    pub fn get(&self, key: &str) -> Result<SessionMeta> {
        let (_, shared, _) = self.handle(key)?;
        let m = shared.meta.lock().unwrap().clone();
        Ok(m)
    }

    pub fn list(&self) -> Vec<SessionMeta> {
        let mut v: Vec<SessionMeta> = self.sessions.lock().unwrap().values().map(|h| h.shared.meta.lock().unwrap().clone()).collect();
        v.sort_by_key(|m| std::cmp::Reverse(m.last_active_at));
        v
    }

    pub fn history(&self, key: &str, after_seq: u64, limit: Option<usize>) -> Result<Vec<EventEnvelope>> {
        let (_, shared, _) = self.handle(key)?;
        let h = shared.history.lock().unwrap();
        let v: Vec<EventEnvelope> = h.iter().filter(|e| e.seq > after_seq).cloned().collect();
        Ok(match limit {
            Some(n) if v.len() > n => v[v.len() - n..].to_vec(),
            _ => v,
        })
    }

    /// Suspend idle processes and forget expired sessions.
    pub async fn gc(&self, force_idle: bool) -> Value {
        let cfg = self.cfg();
        let now = now_ms();
        let metas = self.list();
        let mut suspended = Vec::new();
        let mut removed = Vec::new();
        for m in &metas {
            let idle = is_idle(m);
            if idle && now.saturating_sub(m.last_active_at) > cfg.session_ttl_secs * 1000 {
                let _ = self.close(&m.id).await;
                removed.push(m.id.clone());
            } else if idle && m.live && (force_idle || now.saturating_sub(m.last_active_at) > cfg.idle_timeout_secs * 1000) {
                self.suspend(&m.id);
                suspended.push(m.id.clone());
            }
        }
        json!({"suspended": suspended, "removed": removed})
    }

    /// Suspend the least recently used idle live session (to free a slot).
    fn evict_one(&self, except: &str) -> bool {
        let victim = self
            .list()
            .into_iter()
            .filter(|m| m.live && m.id != except && is_idle(m))
            .min_by_key(|m| m.last_active_at);
        match victim {
            Some(m) => {
                self.suspend(&m.id);
                true
            }
            None => false,
        }
    }

    fn suspend(&self, id: &str) {
        if let Ok((_, _, tx)) = self.handle(id) {
            let _ = tx.send(Cmd::Suspend);
        }
    }

    pub async fn shutdown_all(&self) {
        let txs: Vec<mpsc::UnboundedSender<Cmd>> = self.sessions.lock().unwrap().values().map(|h| h.tx.clone()).collect();
        let waits = txs.into_iter().map(|tx| async move {
            let (done, wait) = oneshot::channel();
            if tx.send(Cmd::Close(done)).is_ok() {
                let _ = wait.await;
            }
        });
        futures_util::future::join_all(waits).await;
    }

    pub fn live_count(&self) -> usize {
        self.sessions.lock().unwrap().values().filter(|h| h.shared.meta.lock().unwrap().live).count()
    }

    fn emit(&self, shared: &Shared, event: AgentEvent) {
        let session_id = shared.meta.lock().unwrap().id.clone();
        let env = EventEnvelope { session_id, seq: shared.seq.fetch_add(1, Ordering::SeqCst) + 1, ts: now_ms(), event };
        {
            let limit = self.cfg.read().unwrap().history_limit;
            let mut h = shared.history.lock().unwrap();
            // Merge consecutive text deltas so history stays compact.
            let merged = match (h.back_mut(), &env.event) {
                (Some(last), AgentEvent::TextDelta { text }) => match &mut last.event {
                    AgentEvent::TextDelta { text: prev } => {
                        prev.push_str(text);
                        last.seq = env.seq;
                        last.ts = env.ts;
                        true
                    }
                    _ => false,
                },
                (_, AgentEvent::Heartbeat { .. }) => true, // live-only
                _ => false,
            };
            if !merged {
                h.push_back(env.clone());
            }
            while h.len() > limit {
                h.pop_front();
            }
        }
        let _ = self.bus.send(env);
    }

    fn set_status(&self, shared: &Shared, status: &str) {
        let changed = {
            let mut m = shared.meta.lock().unwrap();
            if m.status == status {
                false
            } else {
                m.status = status.to_string();
                true
            }
        };
        if changed {
            self.emit(shared, AgentEvent::Status { status: status.to_string() });
        }
    }
}

fn is_idle(m: &SessionMeta) -> bool {
    !matches!(m.status.as_str(), "running" | "starting" | "waiting") && m.queued == 0
}

fn expand_home(p: PathBuf) -> PathBuf {
    match p.strip_prefix("~") {
        Ok(rest) => dirs::home_dir().map(|h| h.join(rest)).unwrap_or(p),
        Err(_) => p,
    }
}

fn history_path(id: &str) -> PathBuf {
    config::history_dir().join(format!("{id}.json"))
}

fn save_history(shared: &Shared) {
    let id = shared.meta.lock().unwrap().id.clone();
    let data = {
        let h = shared.history.lock().unwrap();
        serde_json::to_vec(&*h).unwrap_or_default()
    };
    let _ = std::fs::create_dir_all(config::history_dir());
    let _ = config::write_private(&history_path(&id), &data);
}

fn load_history(id: &str) -> VecDeque<EventEnvelope> {
    std::fs::read(history_path(id)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// Claude refusing an API key that the user's own login would not need.
fn is_credential_conflict(err: &str) -> bool {
    let e = err.to_lowercase();
    e.contains("credit balance is too low")
        || (e.contains("connectors are disabled because") && e.contains("auth source"))
        || e.contains("invalid api key")
        || e.contains("invalid x-api-key")
}

struct Pending {
    key: String,
    expires: Instant,
}

struct Running {
    turn_id: String,
    started: Instant,
    last_output: Instant,
    first_event_ms: Option<u64>,
    first_text_ms: Option<u64>,
    launch_ms: Option<u64>,
    warm: bool,
    retrying: bool,
    activity: bool,
    aborting: Option<Instant>,
    /// Kept so a turn that failed before doing anything can be re-run
    /// (credential fallback, lost resume id).
    prompt: String,
}

#[allow(unused_assignments)]
async fn actor(mgr: Arc<Manager>, shared: Arc<Shared>, mut rx: mpsc::UnboundedReceiver<Cmd>) {
    let (dtx, mut drx) = mpsc::unbounded_channel::<(u64, DriverMsg)>();
    let mut driver: Option<Driver> = None;
    let mut permit: Option<OwnedSemaphorePermit> = None;
    let mut generation = 0u64;
    let mut queue: VecDeque<(String, String)> = VecDeque::new();
    let mut current: Option<Running> = None;
    let mut approvals: HashMap<String, Pending> = HashMap::new();
    let mut resume_retried = false;
    let mut launch_ms: Option<u64> = None;
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    let mut ticks: u64 = 0;
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    macro_rules! emit {
        ($e:expr) => {
            mgr.emit(&shared, $e)
        };
    }
    macro_rules! stop_driver {
        () => {
            if let Some(d) = driver.take() {
                d.shutdown().await;
            }
            permit = None;
            shared.meta.lock().unwrap().live = false;
        };
    }

    loop {
        let mut warm_requested = false;
        tokio::select! {
            cmd = rx.recv() => {
                let Some(cmd) = cmd else { break };
                match cmd {
                    Cmd::Prompt { turn_id, text, mode } => {
                        let running = current.as_ref().map(|r| r.turn_id.clone());
                        match (mode.as_str(), running, driver.as_mut()) {
                            ("steer", Some(into), Some(d)) => match d.steer(&text).await {
                                Ok(true) => emit!(AgentEvent::Steered { turn_id, into_turn_id: into }),
                                _ => {
                                    queue.push_back((turn_id.clone(), text));
                                    emit!(AgentEvent::Queued { turn_id, position: queue.len() });
                                }
                            },
                            ("interrupt", Some(_), Some(d)) => {
                                queue.push_front((turn_id, text));
                                if let Some(r) = current.as_mut() {
                                    if r.aborting.is_none() {
                                        r.aborting = Some(Instant::now());
                                        let _ = d.interrupt().await;
                                    }
                                }
                            }
                            (_, Some(_), _) => {
                                queue.push_back((turn_id.clone(), text));
                                emit!(AgentEvent::Queued { turn_id, position: queue.len() });
                            }
                            _ => queue.push_back((turn_id, text)),
                        }
                    }
                    Cmd::Abort => {
                        for (turn_id, _) in queue.drain(..) {
                            emit!(AgentEvent::TurnEnded { turn_id, outcome: "aborted".into(), error: Some("cancelled before start".into()), usage: None, timing: None });
                        }
                        if let Some(r) = current.as_mut() {
                            if r.aborting.is_none() {
                                r.aborting = Some(Instant::now());
                                if let Some(d) = driver.as_mut() {
                                    if let Err(e) = d.interrupt().await {
                                        emit!(AgentEvent::Error { message: format!("interrupt failed: {e:#}") });
                                    }
                                }
                            }
                        }
                    }
                    Cmd::Suspend => {
                        if current.is_none() && queue.is_empty() && driver.is_some() {
                            stop_driver!();
                            mgr.set_status(&shared, "suspended");
                        }
                    }
                    Cmd::Reconfigure => {
                        if current.is_none() && driver.is_some() {
                            stop_driver!();
                            mgr.set_status(&shared, "suspended");
                        }
                    }
                    Cmd::Warm => warm_requested = true,
                    Cmd::Approve { request_id, decision, done } => {
                        let res = match (approvals.remove(&request_id), driver.as_mut()) {
                            (Some(p), Some(d)) => {
                                let r = d.respond(&p.key, decision).await;
                                emit!(AgentEvent::PermissionResolved { request_id, decision: decision.as_str().into(), by: "client".into() });
                                r
                            }
                            _ => Err(anyhow!("no pending permission request `{request_id}`")),
                        };
                        shared.meta.lock().unwrap().pending_approvals = approvals.len();
                        let _ = done.send(res);
                    }
                    Cmd::Close(done) => {
                        stop_driver!();
                        mgr.set_status(&shared, "closed");
                        let _ = done.send(());
                        return;
                    }
                }
            }
            Some((g, msg)) = drx.recv() => {
                if g != generation {
                    continue;
                }
                if let Some(r) = current.as_mut() {
                    r.last_output = Instant::now();
                    if matches!(msg, DriverMsg::Event(AgentEvent::TextDelta { .. } | AgentEvent::Reasoning { .. } | AgentEvent::ToolStart { .. }) | DriverMsg::Permission(_)) {
                        let ms = r.started.elapsed().as_millis() as u64;
                        r.first_event_ms.get_or_insert(ms);
                    }
                }
                match msg {
                    DriverMsg::Event(e) => {
                        if let Some(r) = current.as_mut() {
                            match &e {
                                AgentEvent::TextDelta { text } => {
                                    r.activity = true;
                                    r.retrying = false;
                                    if !text.is_empty() {
                                        r.first_text_ms.get_or_insert(r.started.elapsed().as_millis() as u64);
                                    }
                                }
                                AgentEvent::ToolStart { .. } | AgentEvent::Reasoning { .. } => {
                                    r.activity = true;
                                    r.retrying = false;
                                }
                                AgentEvent::Retrying { .. } => r.retrying = true,
                                _ => {}
                            }
                        }
                        if let AgentEvent::Info { model: Some(m), .. } = &e {
                            shared.meta.lock().unwrap().model.get_or_insert(m.clone());
                        }
                        emit!(e);
                    }
                    DriverMsg::NativeId(id) => {
                        shared.meta.lock().unwrap().native_id = Some(id);
                        mgr.persist();
                    }
                    DriverMsg::Permission(ask) => {
                        let access = shared.meta.lock().unwrap().access.clone();
                        let request_id = short_id();
                        let timeout = mgr.cfg.read().unwrap().approval_timeout_secs;
                        let policy = match access.as_str() {
                            "read_only" => Some(Decision::Deny),
                            "full" => Some(Decision::Allow),
                            _ => None,
                        };
                        emit!(AgentEvent::PermissionRequest {
                            request_id: request_id.clone(),
                            tool: ask.tool,
                            title: ask.title,
                            input: ask.input,
                            options: ask.options,
                            expires_at: now_ms() + timeout * 1000,
                        });
                        match (policy, driver.as_mut()) {
                            (Some(dec), Some(d)) => {
                                let _ = d.respond(&ask.key, dec).await;
                                emit!(AgentEvent::PermissionResolved { request_id, decision: dec.as_str().into(), by: "policy".into() });
                            }
                            _ => {
                                approvals.insert(request_id, Pending { key: ask.key, expires: Instant::now() + Duration::from_secs(timeout) });
                                mgr.set_status(&shared, "waiting");
                            }
                        }
                        shared.meta.lock().unwrap().pending_approvals = approvals.len();
                    }
                    DriverMsg::PermissionCancelled(key) => {
                        let ids: Vec<String> = approvals.iter().filter(|(_, p)| p.key == key).map(|(k, _)| k.clone()).collect();
                        for id in ids {
                            approvals.remove(&id);
                            emit!(AgentEvent::PermissionResolved { request_id: id, decision: "deny".into(), by: "cancelled".into() });
                        }
                        shared.meta.lock().unwrap().pending_approvals = approvals.len();
                    }
                    DriverMsg::TurnDone { outcome, error, usage } => {
                        let Some(r) = current.take() else { continue };
                        cancel_approvals(&mgr, &shared, &mut approvals);
                        let (agent, use_login, native) = {
                            let m = shared.meta.lock().unwrap();
                            (m.agent.clone(), m.use_login, m.native_id.clone())
                        };
                        let is_claude = agents::find_def(&mgr.cfg(), &agent).map(|d| d.protocol == Protocol::Claude).unwrap_or(false);
                        let err = error.clone().unwrap_or_default();
                        // Retry once with the CLI's own login when an exported
                        // API key was rejected before anything happened.
                        if outcome == "failed" && is_claude && !use_login && !r.activity && is_credential_conflict(&err)
                            && API_KEY_VARS.iter().any(|k| shellenv::has_var(k))
                        {
                            shared.meta.lock().unwrap().use_login = true;
                            mgr.claude_use_login.store(true, Ordering::SeqCst);
                            let _ = config::write_private(&config::home().join("state.json"), json!({"claude_use_login": true}).to_string().as_bytes());
                            emit!(AgentEvent::Retrying { message: "API key rejected; retrying with the CLI's own login".into() });
                            stop_driver!();
                            queue.push_front((r.turn_id, r.prompt));
                            continue;
                        }
                        // A resume id the runtime no longer knows: start over once.
                        if outcome == "failed" && !r.activity && !resume_retried && native.is_some()
                            && err.contains("No conversation found with session ID")
                        {
                            resume_retried = true;
                            shared.meta.lock().unwrap().native_id = None;
                            emit!(AgentEvent::Error { message: "the previous conversation was not found; starting a new one".into() });
                            stop_driver!();
                            queue.push_front((r.turn_id, r.prompt));
                            continue;
                        }
                        let (outcome, error) = if r.aborting.is_some() { ("aborted".to_string(), None) } else { (outcome, error) };
                        let error = match (&outcome[..], error) {
                            ("completed", _) => None,
                            (_, e) => e,
                        };
                        {
                            let mut m = shared.meta.lock().unwrap();
                            m.turns += 1;
                            m.last_active_at = now_ms();
                        }
                        let t = timing(&r);
                        emit!(AgentEvent::TurnEnded { turn_id: r.turn_id, outcome, error, usage, timing: Some(t) });
                        save_history(&shared);
                        mgr.persist();
                    }
                    DriverMsg::Exited(reason) => {
                        driver = None;
                        permit = None;
                        shared.meta.lock().unwrap().live = false;
                        cancel_approvals(&mgr, &shared, &mut approvals);
                        if let Some(r) = current.take() {
                            let (outcome, error) = if r.aborting.is_some() {
                                ("aborted".to_string(), None)
                            } else {
                                let reason = reason.trim();
                                ("failed".to_string(), Some(if reason.is_empty() { "agent process exited".to_string() } else { format!("agent process exited: {reason}") }))
                            };
                            let t = timing(&r);
                        emit!(AgentEvent::TurnEnded { turn_id: r.turn_id, outcome, error, usage: None, timing: Some(t) });
                            save_history(&shared);
                        }
                        mgr.set_status(&shared, "suspended");
                    }
                }
            }
            _ = tick.tick() => {
                ticks += 1;
                let cfg = mgr.cfg();
                // Unanswered permission requests are denied, so a missing
                // client can never hang a turn forever.
                let expired: Vec<String> = approvals.iter().filter(|(_, p)| p.expires <= Instant::now()).map(|(k, _)| k.clone()).collect();
                for id in expired {
                    if let (Some(p), Some(d)) = (approvals.remove(&id), driver.as_mut()) {
                        let _ = d.respond(&p.key, Decision::Deny).await;
                    }
                    emit!(AgentEvent::PermissionResolved { request_id: id, decision: "deny".into(), by: "timeout".into() });
                }
                shared.meta.lock().unwrap().pending_approvals = approvals.len();
                if approvals.is_empty() && shared.meta.lock().unwrap().status == "waiting" {
                    mgr.set_status(&shared, "running");
                }
                let mut kill_reason: Option<String> = None;
                if let Some(r) = current.as_ref() {
                    let idle = r.last_output.elapsed();
                    if ticks % 3 == 0 {
                        emit!(AgentEvent::Heartbeat {
                            turn_id: r.turn_id.clone(),
                            elapsed_ms: r.started.elapsed().as_millis() as u64,
                            idle_ms: idle.as_millis() as u64,
                            stalled: idle > Duration::from_secs(60) && approvals.is_empty(),
                        });
                    }
                    if r.aborting.map(|t| t.elapsed() > Duration::from_secs(10)).unwrap_or(false) {
                        kill_reason = Some(String::new());
                    } else if r.retrying && cfg.stall_timeout_secs > 0 && idle > Duration::from_secs(cfg.stall_timeout_secs) && approvals.is_empty() {
                        kill_reason = Some(format!("stalled: no output for {}s after a model retry", idle.as_secs()));
                    } else if cfg.turn_timeout_secs > 0 && r.started.elapsed() > Duration::from_secs(cfg.turn_timeout_secs) {
                        kill_reason = Some(format!("turn exceeded {}s", cfg.turn_timeout_secs));
                    }
                }
                if let Some(reason) = kill_reason {
                    // The runtime did not end the turn itself: stop the tree.
                    stop_driver!();
                    cancel_approvals(&mgr, &shared, &mut approvals);
                    if let Some(r) = current.take() {
                        let (outcome, error) = if reason.is_empty() { ("aborted".to_string(), None) } else { ("failed".to_string(), Some(reason)) };
                        let t = timing(&r);
                        emit!(AgentEvent::TurnEnded { turn_id: r.turn_id, outcome, error, usage: None, timing: Some(t) });
                        save_history(&shared);
                    }
                    mgr.set_status(&shared, "suspended");
                }
            }
        }

        // Start the agent (for a queued turn, or a warm-up) and run turns.
        if driver.is_none() && (!queue.is_empty() && current.is_none() || warm_requested) {
            let for_turn = !queue.is_empty();
            // Budget: a free slot, else evict an idle session; warm-ups never evict.
            if permit.is_none() {
                permit = mgr.budget.clone().try_acquire_owned().ok();
                if permit.is_none() && for_turn {
                    let my_id = shared.meta.lock().unwrap().id.clone();
                    mgr.set_status(&shared, "waiting");
                    mgr.evict_one(&my_id);
                    permit = tokio::time::timeout(Duration::from_secs(30), mgr.budget.clone().acquire_owned()).await.ok().and_then(|p| p.ok());
                    if permit.is_none() {
                        // Over budget with nothing idle: run anyway rather than starve.
                        emit!(AgentEvent::Error { message: "process budget exhausted; starting anyway".into() });
                    }
                }
            }
            if permit.is_some() || for_turn {
                generation = mgr.generation.fetch_add(1, Ordering::SeqCst);
                let cfg = mgr.cfg();
                let spec = {
                    let m = shared.meta.lock().unwrap();
                    agents::find_def(&cfg, &m.agent).map(|def| SpawnSpec {
                        strip_api_key: m.use_login || (def.protocol == Protocol::Claude && mgr.claude_use_login.load(Ordering::SeqCst)),
                        def,
                        cwd: m.cwd.clone(),
                        model: m.model.clone(),
                        effort: m.effort.clone(),
                        resume: m.native_id.clone(),
                        access: m.access.clone(),
                        tool_output_limit: cfg.tool_output_limit,
                        generation,
                    })
                };
                mgr.set_status(&shared, "starting");
                let t0 = Instant::now();
                let res = match spec {
                    Some(spec) => Driver::spawn(&spec, dtx.clone()).await,
                    None => Err(anyhow!("agent is no longer configured")),
                };
                match res {
                    Ok(d) => {
                        driver = Some(d);
                        shared.meta.lock().unwrap().live = true;
                        // Only a launch on the turn's path makes the turn cold.
                        launch_ms = for_turn.then(|| t0.elapsed().as_millis() as u64);
                        mgr.set_status(&shared, "idle");
                    }
                    Err(e) => {
                        permit = None;
                        if let Some((turn_id, _)) = queue.pop_front() {
                            emit!(AgentEvent::TurnEnded { turn_id, outcome: "failed".into(), error: Some(format!("{e:#}")), usage: None, timing: None });
                        } else {
                            emit!(AgentEvent::Error { message: format!("warm-up failed: {e:#}") });
                        }
                        mgr.set_status(&shared, "suspended");
                    }
                }
            }
        }
        while current.is_none() && driver.is_some() {
            let Some((turn_id, text)) = queue.pop_front() else { break };
            let d = driver.as_mut().expect("driver present");
            emit!(AgentEvent::TurnStarted { turn_id: turn_id.clone() });
            mgr.set_status(&shared, "running");
            let now = Instant::now();
            let cold = launch_ms.take();
            match d.prompt(&text).await {
                Ok(()) => {
                    let text = text;
                    current = Some(Running {
                        turn_id,
                        started: now,
                        last_output: now,
                        first_event_ms: None,
                        first_text_ms: None,
                        launch_ms: cold,
                        warm: cold.is_none(),
                        retrying: false,
                        activity: false,
                        aborting: None,
                        prompt: text,
                    })
                }
                Err(e) => emit!(AgentEvent::TurnEnded { turn_id, outcome: "failed".into(), error: Some(format!("{e:#}")), usage: None, timing: None }),
            }
        }
        if current.is_none() && driver.is_some() && approvals.is_empty() {
            mgr.set_status(&shared, "idle");
        }
        shared.meta.lock().unwrap().queued = queue.len();
    }
}

fn cancel_approvals(mgr: &Manager, shared: &Shared, approvals: &mut HashMap<String, Pending>) {
    for (id, _) in approvals.drain() {
        mgr.emit(shared, AgentEvent::PermissionResolved { request_id: id, decision: "deny".into(), by: "cancelled".into() });
    }
    shared.meta.lock().unwrap().pending_approvals = 0;
}

fn timing(r: &Running) -> Value {
    json!({
        "warm": r.warm,
        "launch_ms": r.launch_ms,
        "first_event_ms": r.first_event_ms,
        "first_text_ms": r.first_text_ms,
        "total_ms": r.started.elapsed().as_millis() as u64,
    })
}
