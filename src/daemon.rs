//! The local daemon: owns the session manager, serves the local WebSocket API
//! on 127.0.0.1 for the CLI and embedding apps, and keeps the outbound link
//! to a hub.
//!
//! Local clients authenticate with the token in `$AGL_HOME/daemon.json`
//! (0600, same user only) via `?token=` or `Authorization: Bearer`. A request
//! carrying a valid token may come from a browser/webview (Origin allowed);
//! without one, browsers are always refused.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, Notify};

use crate::agents;
use crate::config::{self, Config};
use crate::protocol::{MachineInfo, WireMsg, PROTOCOL, VERSION};
use crate::session::{ConfigureParams, CreateParams, Manager, Origin};

pub struct Daemon {
    pub mgr: Arc<Manager>,
    /// Link status per hub URL.
    pub hub_status: Mutex<std::collections::HashMap<String, String>>,
    /// Bumped whenever the hub configuration changes.
    pub reload: tokio::sync::watch::Sender<u64>,
    pub shutdown: Notify,
    pub token: String,
    pub port: Mutex<u16>,
}

#[derive(Default)]
pub struct DaemonOptions {
    /// 0 = pick a free port (written to daemon.json).
    pub port: Option<u16>,
    /// Exit when this process dies (sidecar mode).
    pub parent_pid: Option<u32>,
    /// Use this token instead of a random one.
    pub token: Option<String>,
}

pub async fn run(cfg: Config, opts: DaemonOptions) -> Result<()> {
    STARTED_AT.get_or_init(crate::protocol::now_ms);
    let explicit_port = opts.port.is_some();
    let port = opts.port.unwrap_or(cfg.port);
    std::fs::create_dir_all(config::home())?;
    // One daemon per AGL_HOME.
    if let Some(pid) = std::fs::read_to_string(config::daemon_file())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v["pid"].as_u64())
    {
        if pid as u32 != std::process::id() && pid_alive(pid as u32) {
            anyhow::bail!("an agl daemon (pid {pid}) is already running for {}", config::home().display());
        }
    }
    let reaped = agents::reap_orphans();
    if reaped > 0 {
        eprintln!("agl: stopped {reaped} agent process(es) left by a previous daemon");
    }
    // Resolve the login-shell environment off the request path.
    tokio::task::spawn_blocking(crate::shellenv::warm_up);

    let mgr = Manager::new(cfg);
    mgr.load();
    let token = opts
        .token
        .or_else(|| std::env::var("AGL_LOCAL_TOKEN").ok())
        .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
    let daemon = Arc::new(Daemon {
        mgr: mgr.clone(),
        hub_status: Mutex::default(),
        reload: tokio::sync::watch::Sender::new(0),
        shutdown: Notify::new(),
        token,
        port: Mutex::new(port),
    });

    // Garbage collector.
    {
        let mgr = mgr.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            loop {
                tick.tick().await;
                mgr.gc(false).await;
            }
        });
    }
    if let Some(ppid) = opts.parent_pid {
        let d = daemon.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                if !pid_alive(ppid) {
                    eprintln!("agl: parent process {ppid} exited; shutting down");
                    d.shutdown.notify_one();
                    break;
                }
            }
        });
    }
    tokio::spawn(crate::link::supervise(daemon.clone()));

    let app = Router::new()
        .route("/health", get(|| async { Json(json!({"ok": true, "version": VERSION})) }))
        .route("/ws", get(ws_handler))
        .with_state(daemon.clone());
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(l) => l,
        // The default port is taken (another AGL_HOME, another app): any free
        // port works, since clients read it from daemon.json.
        Err(e) if !explicit_port && e.kind() == std::io::ErrorKind::AddrInUse => {
            eprintln!("agl: port {port} is in use; picking a free one");
            tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?
        }
        Err(e) => return Err(e).with_context(|| format!("bind 127.0.0.1:{port}")),
    };
    let port = listener.local_addr()?.port();
    *daemon.port.lock().unwrap() = port;
    let info = json!({"pid": std::process::id(), "port": port, "token": daemon.token, "version": VERSION});
    config::write_private(&config::daemon_file(), info.to_string().as_bytes())?;
    eprintln!("agl daemon {VERSION} listening on 127.0.0.1:{port} (home {})", config::home().display());
    // Machine-readable line for sidecar hosts that read our stdout.
    println!("{info}");

    let d = daemon.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = d.shutdown.notified() => {}
                _ = terminate() => {}
            }
        })
        .await?;
    eprintln!("agl daemon shutting down");
    mgr.shutdown_all().await;
    // Only remove the file if it is still ours.
    if let Ok(s) = std::fs::read_to_string(config::daemon_file()) {
        if serde_json::from_str::<Value>(&s).ok().and_then(|v| v["pid"].as_u64()) == Some(std::process::id() as u64) {
            let _ = std::fs::remove_file(config::daemon_file());
        }
    }
    Ok(())
}

fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: signal 0 only checks for existence.
        unsafe { libc::kill(pid as i32, 0) == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

#[cfg(unix)]
async fn terminate() {
    use tokio::signal::unix::{signal, SignalKind};
    if let Ok(mut s) = signal(SignalKind::terminate()) {
        s.recv().await;
    }
}

#[cfg(not(unix))]
async fn terminate() {
    std::future::pending::<()>().await
}

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

async fn ws_handler(State(d): State<Arc<Daemon>>, headers: HeaderMap, Query(q): Query<TokenQuery>, ws: WebSocketUpgrade) -> Response {
    let bearer = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(String::from);
    let supplied = q.token.or(bearer);
    if supplied.as_deref() != Some(d.token.as_str()) {
        return (StatusCode::UNAUTHORIZED, "missing or invalid token (see $AGL_HOME/daemon.json)").into_response();
    }
    ws.on_upgrade(move |socket| local_conn(d, socket))
}

#[derive(Default)]
struct Subs {
    all: bool,
    ids: HashSet<String>,
}

async fn local_conn(d: Arc<Daemon>, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    let subs = Arc::new(Mutex::new(Subs::default()));

    let writer = tokio::spawn(async move {
        while let Some(s) = out_rx.recv().await {
            if sink.send(Message::Text(s.into())).await.is_err() {
                break;
            }
        }
    });
    let forwarder = {
        let (subs, out_tx) = (subs.clone(), out_tx.clone());
        let mut bus = d.mgr.bus.subscribe();
        tokio::spawn(async move {
            loop {
                match bus.recv().await {
                    Ok(env) => {
                        let wanted = {
                            let s = subs.lock().unwrap();
                            s.all || s.ids.contains(&env.session_id)
                        };
                        if wanted && out_tx.send(WireMsg::Event(env).to_json()).is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        })
    };

    while let Some(Ok(msg)) = stream.next().await {
        let Message::Text(text) = msg else { continue };
        let Ok(WireMsg::Request { id, method, params }) = serde_json::from_str::<WireMsg>(&text) else { continue };
        match method.as_str() {
            "subscribe" | "unsubscribe" => {
                let res = (|| -> Result<Value> {
                    let mut s = subs.lock().unwrap();
                    let on = method == "subscribe";
                    if params["all"].as_bool().unwrap_or(false) {
                        s.all = on;
                        return Ok(json!({"all": on}));
                    }
                    let key = params["session_id"].as_str().ok_or_else(|| anyhow!("session_id required"))?;
                    let sid = d.mgr.resolve(key).ok_or_else(|| anyhow!("no session `{key}`"))?;
                    if on {
                        s.ids.insert(sid.clone());
                    } else {
                        s.ids.remove(&sid);
                    }
                    Ok(json!({"session_id": sid}))
                })();
                let _ = out_tx.send(WireMsg::reply(id, res).to_json());
            }
            _ => {
                // Prompting a session subscribes to it, before its first event.
                if method == "session.prompt" {
                    if let Some(sid) = params["session_id"].as_str().and_then(|k| d.mgr.resolve(k)) {
                        subs.lock().unwrap().ids.insert(sid);
                    }
                }
                let (d, out_tx) = (d.clone(), out_tx.clone());
                tokio::spawn(async move {
                    let res = dispatch(&d, &method, params, &Origin::Local).await;
                    let _ = out_tx.send(WireMsg::reply(id, res).to_json());
                });
            }
        }
    }
    forwarder.abort();
    drop(out_tx);
    let _ = writer.await;
}

/// When this process started serving, for `MachineInfo::started_at`.
static STARTED_AT: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

pub async fn machine_info(cfg: &Config) -> MachineInfo {
    MachineInfo {
        home: dirs::home_dir().map(|h| h.display().to_string()).unwrap_or_default(),
        started_at: *STARTED_AT.get_or_init(crate::protocol::now_ms),
        machine_id: cfg.machine_id.clone(),
        hostname: cfg.machine_name.clone().unwrap_or_else(config::hostname),
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        version: VERSION.into(),
        protocol: PROTOCOL,
        agents: agents::detect(cfg, false).await,
    }
}

fn str_param<'a>(params: &'a Value, key: &str) -> Result<&'a str> {
    params[key].as_str().ok_or_else(|| anyhow!("missing parameter `{key}`"))
}

/// One request, from a local client or from a linked hub. A hub only ever
/// sees the sessions its `origin` may see.
pub async fn dispatch(d: &Arc<Daemon>, method: &str, params: Value, origin: &Origin) -> Result<Value> {
    let mgr = &d.mgr;
    let local = *origin == Origin::Local;
    // The session a request names, checked against the caller's visibility.
    let sid = || -> Result<String> { mgr.resolve_for(str_param(&params, "session_id")?, origin) };
    Ok(match method {
        "ping" => json!({"pong": true, "version": VERSION, "protocol": PROTOCOL}),
        "machine.info" => serde_json::to_value(machine_info(&mgr.cfg()).await)?,
        "agents.list" => serde_json::to_value(agents::detect(&mgr.cfg(), params["check_updates"].as_bool().unwrap_or(false)).await)?,
        "agents.models" => agents::list_models(&mgr.cfg(), str_param(&params, "agent")?).await?,
        "daemon.status" => {
            let cfg = mgr.cfg();
            let statuses = d.hub_status.lock().unwrap().clone();
            let hubs: Vec<Value> = cfg
                .hubs
                .iter()
                // A hub learns nothing about the machine's other hubs.
                .filter(|h| local || Origin::Hub { url: h.url.clone(), share_local: false }.tag() == origin.tag())
                .map(|h| json!({"url": h.url, "status": statuses.get(&h.url).cloned().unwrap_or_else(|| "starting".into()), "access": h.access, "share_local": h.share_local, "transport": h.transport}))
                .collect();
            json!({
                "version": VERSION,
                "protocol": PROTOCOL,
                "pid": std::process::id(),
                "port": *d.port.lock().unwrap(),
                "home": if local { json!(config::home()) } else { Value::Null },
                "machine_id": cfg.machine_id,
                "hubs": hubs,
                "sessions": mgr.list().iter().filter(|m| origin.sees(&m.origin)).count(),
                "live": mgr.live_count(),
                "default_access": cfg.default_access,
                "idle_timeout_secs": cfg.idle_timeout_secs,
                "session_ttl_secs": cfg.session_ttl_secs,
                "max_live": cfg.max_live,
            })
        }
        "daemon.shutdown" if local => {
            d.shutdown.notify_one();
            json!({"ok": true})
        }
        "hub.reload" if local => {
            let fresh = Config::load()?;
            *mgr.cfg.write().unwrap() = fresh;
            d.reload.send_modify(|g| *g += 1);
            json!({"ok": true})
        }
        "session.create" => {
            let mut p: CreateParams = serde_json::from_value(params).context("session.create params")?;
            if p.access.is_none() {
                if let Origin::Hub { url, .. } = origin {
                    // Remote sessions ask before acting unless the link says otherwise.
                    p.access = Some(mgr.cfg().hub(url).map(|h| h.access.clone()).unwrap_or_else(|| "ask".into()));
                }
            }
            p.origin = Some(origin.clone());
            serde_json::to_value(mgr.create(p)?)?
        }
        "session.prompt" => {
            let text = str_param(&params, "text")?.to_string();
            let turn_id = params["turn_id"].as_str().map(String::from);
            mgr.prompt(&sid()?, text, params["mode"].as_str(), turn_id)?
        }
        "session.approve" => {
            mgr.approve(&sid()?, str_param(&params, "request_id")?, str_param(&params, "decision")?).await?;
            json!({"ok": true})
        }
        "session.abort" => {
            mgr.abort(&sid()?)?;
            json!({"ok": true})
        }
        "session.warm" => {
            mgr.warm(&sid()?)?;
            json!({"ok": true})
        }
        "session.configure" => {
            let key = sid()?;
            let p: ConfigureParams = serde_json::from_value(params).context("session.configure params")?;
            serde_json::to_value(mgr.configure(&key, p)?)?
        }
        "session.close" => {
            mgr.close(&sid()?).await?;
            json!({"ok": true})
        }
        "session.get" => serde_json::to_value(mgr.get(&sid()?)?)?,
        "session.list" => serde_json::to_value(mgr.list().into_iter().filter(|m| origin.sees(&m.origin)).collect::<Vec<_>>())?,
        "session.history" => {
            let after = params["after_seq"].as_u64().unwrap_or(0);
            let limit = params["limit"].as_u64().map(|n| n as usize);
            serde_json::to_value(mgr.history(&sid()?, after, limit)?)?
        }
        "gc.run" if local => mgr.gc(params["idle"].as_bool().unwrap_or(false)).await,
        "fs.read" | "fs.write" | "exec" => crate::machine::dispatch(&mgr.cfg(), origin, method, params).await?,
        other => return Err(anyhow!("unknown method `{other}`")),
    })
}
