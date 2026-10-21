//! Reference hub: what a web app needs to reach agents on linked machines.
//!
//! - `GET /`                          web UI (connect-a-machine dialog + chat)
//! - `GET /install.sh?code=…`         installer for a one-time pairing code
//! - `GET /download/<os>-<arch>`      agl binary (`.sha256` for its checksum)
//! - `WS  /ws/machine?code=…|token=…` machines dial in here
//! - `WS  /ws/ui?token=…`             browsers: `request` with `machine_id`
//!
//! Pairing: the UI asks for a one-time code (10 min); the install command
//! carries it; the machine connects with `?code=`, receives its own token
//! (`paired`), and reconnects with that. Only token hashes are stored, and a
//! machine can be revoked on its own. The admin token guards the UI.
//!
//! A real web app implements the same two WebSocket contracts.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

use crate::config;
use crate::protocol::{now_ms, WireMsg};

const UI_HTML: &str = include_str!("../assets/hub.html");
const INSTALL_SH: &str = include_str!("../assets/install.sh");
const PAIRING_TTL_MS: u64 = 10 * 60 * 1000;

struct MachineConn {
    conn: u64,
    info: Value,
    tx: mpsc::UnboundedSender<String>,
}

struct Pending {
    ui: u64,
    id: String,
    machine_id: String,
}

#[derive(Default, Serialize, Deserialize)]
struct Persisted {
    token: String,
    /// machine_id -> sha256(machine token)
    #[serde(default)]
    machines: HashMap<String, MachineRecord>,
}

#[derive(Clone, Serialize, Deserialize)]
struct MachineRecord {
    token_sha256: String,
    #[serde(default)]
    hostname: String,
    paired_at: u64,
}

struct Hub {
    token: String,
    public_url: Option<String>,
    dist: Option<PathBuf>,
    /// Where binaries not in `dist` come from (GitHub Releases by default).
    release_url: Option<String>,
    store: Mutex<Persisted>,
    /// pairing code -> expiry (unix ms)
    codes: Mutex<HashMap<String, u64>>,
    machines: Mutex<HashMap<String, MachineConn>>,
    uis: Mutex<HashMap<u64, mpsc::UnboundedSender<String>>>,
    pending: Mutex<HashMap<String, Pending>>,
    counter: AtomicU64,
}

#[derive(Deserialize)]
struct AuthQuery {
    token: Option<String>,
    code: Option<String>,
    machine_id: Option<String>,
}

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

fn random_token(prefix: &str) -> String {
    format!("{prefix}{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple())
}

fn store_path() -> PathBuf {
    config::home().join("hub.json")
}

pub const DEFAULT_RELEASE_URL: &str = "https://github.com/thinkany-ai/agl/releases/latest/download";

pub async fn run(listen: String, public_url: Option<String>, token: Option<String>, dist: Option<PathBuf>, release_url: Option<String>) -> Result<()> {
    std::fs::create_dir_all(config::home())?;
    let mut store: Persisted = std::fs::read_to_string(store_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    if let Some(t) = token {
        store.token = t;
    }
    if store.token.is_empty() {
        store.token = uuid::Uuid::new_v4().simple().to_string();
    }
    config::write_private(&store_path(), serde_json::to_string_pretty(&store)?.as_bytes())?;
    let hub = Arc::new(Hub {
        token: store.token.clone(),
        public_url: public_url.map(|u| u.trim_end_matches('/').to_string()),
        dist,
        release_url: release_url.filter(|u| !u.is_empty()).map(|u| u.trim_end_matches('/').to_string()),
        store: Mutex::new(store),
        codes: Mutex::default(),
        machines: Mutex::default(),
        uis: Mutex::default(),
        pending: Mutex::default(),
        counter: AtomicU64::new(1),
    });
    let app = Router::new()
        .route("/", get(ui_page))
        .route("/install.sh", get(install_script))
        .route("/download/{target}", get(download))
        .route("/ws/machine", get(machine_ws))
        .route("/ws/ui", get(ui_ws))
        .with_state(hub.clone());
    let listener = tokio::net::TcpListener::bind(&listen).await.with_context(|| format!("bind {listen}"))?;
    let shown = hub.public_url.clone().unwrap_or_else(|| {
        let port = listener.local_addr().map(|a| a.port()).unwrap_or(8787);
        format!("http://localhost:{port}")
    });
    println!("agl hub listening on {listen}");
    println!("  web UI: {shown}/?token={}", hub.token);
    println!("  open the UI and click “连接电脑” to get a one-time install command");
    axum::serve(listener, app).await?;
    Ok(())
}

impl Hub {
    fn base_url(&self, headers: &HeaderMap) -> String {
        if let Some(u) = &self.public_url {
            return u.clone();
        }
        let host = headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("localhost:8787");
        let proto = headers.get("x-forwarded-proto").and_then(|h| h.to_str().ok()).unwrap_or("http");
        format!("{proto}://{host}")
    }

    fn save(&self) {
        let data = serde_json::to_string_pretty(&*self.store.lock().unwrap()).unwrap_or_default();
        let _ = config::write_private(&store_path(), data.as_bytes());
    }

    fn new_code(&self) -> (String, u64) {
        let code = uuid::Uuid::new_v4().simple().to_string()[..20].to_string();
        let exp = now_ms() + PAIRING_TTL_MS;
        let mut codes = self.codes.lock().unwrap();
        codes.retain(|_, e| *e > now_ms());
        codes.insert(code.clone(), exp);
        (code, exp)
    }

    fn code_valid(&self, code: &str) -> bool {
        self.codes.lock().unwrap().get(code).map(|e| *e > now_ms()).unwrap_or(false)
    }

    /// Consume a pairing code once.
    fn take_code(&self, code: &str) -> bool {
        matches!(self.codes.lock().unwrap().remove(code), Some(e) if e > now_ms())
    }

    fn machine_token_ok(&self, machine_id: &str, token: &str) -> bool {
        let store = self.store.lock().unwrap();
        store.machines.get(machine_id).map(|r| r.token_sha256 == sha256_hex(token.as_bytes())).unwrap_or(false)
    }

    fn machines_json(&self) -> Value {
        let online = self.machines.lock().unwrap();
        let store = self.store.lock().unwrap();
        let mut list: Vec<Value> = online
            .values()
            .map(|m| {
                let mut v = m.info.clone();
                v["online"] = json!(true);
                v
            })
            .collect();
        for (id, rec) in &store.machines {
            if !online.contains_key(id) {
                list.push(json!({"machine_id": id, "hostname": rec.hostname, "online": false, "agents": []}));
            }
        }
        list.sort_by(|a, b| b["online"].as_bool().cmp(&a["online"].as_bool()).then(a["hostname"].as_str().cmp(&b["hostname"].as_str())));
        Value::Array(list)
    }

    fn broadcast(&self, msg: &Value) {
        let s = msg.to_string();
        self.uis.lock().unwrap().retain(|_, tx| tx.send(s.clone()).is_ok());
    }

    fn broadcast_machines(&self) {
        self.broadcast(&json!({"type": "machines", "machines": self.machines_json()}));
    }

    fn send_ui(&self, ui: u64, msg: Value) {
        if let Some(tx) = self.uis.lock().unwrap().get(&ui) {
            let _ = tx.send(msg.to_string());
        }
    }
}

async fn ui_page(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Html<String> {
    Html(UI_HTML.replace("__HUB_URL__", &hub.base_url(&headers)))
}

async fn install_script(State(hub): State<Arc<Hub>>, headers: HeaderMap, Query(q): Query<AuthQuery>) -> Response {
    let secret = match (&q.code, &q.token) {
        (Some(c), _) if hub.code_valid(c) => format!("--code {c}"),
        (_, Some(t)) if *t == hub.token => format!("--token {t}"),
        _ => return (StatusCode::UNAUTHORIZED, "echo 'agl: this install link is invalid or has expired; copy a new one from the web page' >&2; exit 1\n").into_response(),
    };
    let script = INSTALL_SH.replace("__HUB_URL__", &hub.base_url(&headers)).replace("__CONNECT_ARGS__", &secret);
    ([(header::CONTENT_TYPE, "text/x-shellscript; charset=utf-8")], script).into_response()
}

pub fn host_target() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        other => other,
    };
    format!("{os}-{arch}")
}

async fn download(State(hub): State<Arc<Hub>>, Path(target): Path<String>) -> Response {
    let (target, want_sum) = match target.strip_suffix(".sha256") {
        Some(t) => (t.to_string(), true),
        None => (target, false),
    };
    if !target.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let mut candidates = Vec::new();
    if let Some(dir) = &hub.dist {
        candidates.push(dir.join(format!("agl-{target}")));
    }
    if target == host_target() {
        if let Ok(exe) = std::env::current_exe() {
            candidates.push(exe);
        }
    }
    for p in candidates {
        if let Ok(bytes) = tokio::fs::read(&p).await {
            if want_sum {
                return ([(header::CONTENT_TYPE, "text/plain")], format!("{}\n", sha256_hex(&bytes))).into_response();
            }
            return ([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response();
        }
    }
    if let Some(base) = &hub.release_url {
        let suffix = if want_sum { ".sha256" } else { "" };
        return axum::response::Redirect::temporary(&format!("{base}/agl-{target}{suffix}")).into_response();
    }
    (StatusCode::NOT_FOUND, format!("no agl build for {target} on this hub (put agl-{target} in --dist)")).into_response()
}

enum MachineAuth {
    Token,
    Pairing,
}

async fn machine_ws(State(hub): State<Arc<Hub>>, Query(q): Query<AuthQuery>, ws: WebSocketUpgrade) -> Response {
    let machine_id = q.machine_id.clone().unwrap_or_default();
    if machine_id.is_empty() || machine_id.len() > 64 {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let auth = match (&q.code, &q.token) {
        (Some(code), _) if hub.take_code(code) => MachineAuth::Pairing,
        // The admin token also works, for scripted setups.
        (_, Some(t)) if hub.machine_token_ok(&machine_id, t) || *t == hub.token => MachineAuth::Token,
        _ => return StatusCode::UNAUTHORIZED.into_response(),
    };
    ws.on_upgrade(move |s| async move {
        match auth {
            MachineAuth::Pairing => pair(hub, s, machine_id).await,
            MachineAuth::Token => machine_conn(hub, s).await,
        }
    })
}

/// Issue the machine its own token; it reconnects with it.
async fn pair(hub: Arc<Hub>, mut socket: WebSocket, machine_id: String) {
    let token = random_token("aglm_");
    hub.store.lock().unwrap().machines.insert(
        machine_id.clone(),
        MachineRecord { token_sha256: sha256_hex(token.as_bytes()), hostname: String::new(), paired_at: now_ms() },
    );
    hub.save();
    let _ = socket.send(Message::Text(WireMsg::Paired { token }.to_json().into())).await;
    let _ = socket.close().await;
    println!("machine paired: {machine_id}");
}

async fn machine_conn(hub: Arc<Hub>, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let hello = tokio::time::timeout(Duration::from_secs(10), stream.next()).await;
    let Ok(Some(Ok(Message::Text(text)))) = hello else { return };
    let Ok(WireMsg::Hello { machine }) = serde_json::from_str::<WireMsg>(&text) else { return };
    let machine_id = machine.machine_id.clone();
    let conn = hub.counter.fetch_add(1, Ordering::SeqCst);
    let mut info = serde_json::to_value(&machine).unwrap_or_default();
    info["connected_at"] = json!(now_ms());
    if let Some(rec) = hub.store.lock().unwrap().machines.get_mut(&machine_id) {
        rec.hostname = machine.hostname.clone();
    }
    hub.save();

    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    hub.machines.lock().unwrap().insert(machine_id.clone(), MachineConn { conn, info, tx });
    hub.broadcast_machines();
    println!("machine connected: {} ({machine_id}, agl {}, protocol {})", machine.hostname, machine.version, machine.protocol);

    let writer = tokio::spawn(async move {
        let mut ping = tokio::time::interval(Duration::from_secs(25));
        loop {
            tokio::select! {
                m = rx.recv() => match m {
                    Some(s) if s == "\u{0}close" => { let _ = sink.send(Message::Close(None)).await; break }
                    Some(s) => if sink.send(Message::Text(s.into())).await.is_err() { break },
                    None => break,
                },
                _ = ping.tick() => if sink.send(Message::Ping(Vec::new().into())).await.is_err() { break },
            }
        }
    });

    while let Some(Ok(msg)) = stream.next().await {
        let Message::Text(text) = msg else { continue };
        match serde_json::from_str::<WireMsg>(&text) {
            Ok(WireMsg::Response { id, result, error }) => {
                if let Some(p) = hub.pending.lock().unwrap().remove(&id) {
                    hub.send_ui(p.ui, json!({"type": "response", "id": p.id, "machine_id": p.machine_id, "result": result, "error": error}));
                }
            }
            Ok(WireMsg::Event(env)) => {
                let mut v = serde_json::to_value(&env).unwrap_or_default();
                v["type"] = json!("event");
                v["machine_id"] = json!(machine_id);
                hub.broadcast(&v);
            }
            Ok(WireMsg::Hello { machine }) => {
                if let Some(m) = hub.machines.lock().unwrap().get_mut(&machine_id) {
                    m.info["agents"] = serde_json::to_value(&machine.agents).unwrap_or_default();
                }
                hub.broadcast_machines();
            }
            _ => {}
        }
    }

    writer.abort();
    {
        let mut machines = hub.machines.lock().unwrap();
        if machines.get(&machine_id).map(|m| m.conn) == Some(conn) {
            machines.remove(&machine_id);
        }
    }
    let failed: Vec<Pending> = {
        let mut pending = hub.pending.lock().unwrap();
        let ids: Vec<String> = pending.iter().filter(|(_, p)| p.machine_id == machine_id).map(|(k, _)| k.clone()).collect();
        ids.into_iter().filter_map(|k| pending.remove(&k)).collect()
    };
    for p in failed {
        hub.send_ui(p.ui, json!({"type": "response", "id": p.id, "machine_id": p.machine_id, "error": "machine disconnected"}));
    }
    hub.broadcast_machines();
    println!("machine disconnected: {} ({machine_id})", machine.hostname);
}

async fn ui_ws(State(hub): State<Arc<Hub>>, Query(q): Query<AuthQuery>, ws: WebSocketUpgrade) -> Response {
    if q.token.as_deref() != Some(hub.token.as_str()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    ws.on_upgrade(move |s| ui_conn(hub, s))
}

async fn ui_conn(hub: Arc<Hub>, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let ui = hub.counter.fetch_add(1, Ordering::SeqCst);
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    hub.uis.lock().unwrap().insert(ui, tx);
    hub.send_ui(ui, json!({"type": "machines", "machines": hub.machines_json()}));

    let writer = tokio::spawn(async move {
        while let Some(s) = rx.recv().await {
            if sink.send(Message::Text(s.into())).await.is_err() {
                break;
            }
        }
    });

    while let Some(Ok(msg)) = stream.next().await {
        let Message::Text(text) = msg else { continue };
        let Ok(req) = serde_json::from_str::<Value>(&text) else { continue };
        if req["type"] != "request" {
            continue;
        }
        let id = req["id"].as_str().map(String::from).unwrap_or_else(|| req["id"].to_string());
        let method = req["method"].as_str().unwrap_or_default().to_string();
        // Hub-level methods.
        let local = match method.as_str() {
            "machines.list" => Some(Ok(hub.machines_json())),
            "pairing.create" => {
                let (code, exp) = hub.new_code();
                Some(Ok(json!({"code": code, "expires_at": exp})))
            }
            "machines.revoke" => {
                let mid = req["params"]["machine_id"].as_str().unwrap_or_default().to_string();
                let existed = hub.store.lock().unwrap().machines.remove(&mid).is_some();
                hub.save();
                if let Some(m) = hub.machines.lock().unwrap().get(&mid) {
                    let _ = m.tx.send("\u{0}close".into());
                }
                hub.broadcast_machines();
                Some(if existed { Ok(json!({"ok": true})) } else { Err("no such machine") })
            }
            _ => None,
        };
        if let Some(res) = local {
            let msg = match res {
                Ok(v) => json!({"type": "response", "id": id, "result": v}),
                Err(e) => json!({"type": "response", "id": id, "error": e}),
            };
            hub.send_ui(ui, msg);
            continue;
        }
        let machine_id = req["machine_id"].as_str().unwrap_or_default().to_string();
        let target = hub.machines.lock().unwrap().get(&machine_id).map(|m| m.tx.clone());
        let Some(mtx) = target else {
            hub.send_ui(ui, json!({"type": "response", "id": id, "machine_id": machine_id, "error": "machine is offline"}));
            continue;
        };
        let hub_id = format!("h{}", hub.counter.fetch_add(1, Ordering::SeqCst));
        hub.pending.lock().unwrap().insert(hub_id.clone(), Pending { ui, id, machine_id });
        let fwd = WireMsg::Request { id: hub_id, method, params: req["params"].clone() };
        let _ = mtx.send(fwd.to_json());
    }

    writer.abort();
    hub.uis.lock().unwrap().remove(&ui);
    hub.pending.lock().unwrap().retain(|_, p| p.ui != ui);
}
