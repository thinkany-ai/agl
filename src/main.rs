use agl::{agents, client, config, daemon, hub, link, protocol, service, update};

use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};
use tokio::io::AsyncBufReadExt;

use client::{ensure_daemon, Client};
use config::Config;
use protocol::{now_ms, AgentEvent, EventEnvelope};

#[derive(Parser)]
#[command(name = "agl", version, about = "AgentLink: talk to local coding agents from the CLI or a remote web app")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Send one prompt to an agent and stream the reply (creates or reuses a session)
    Run {
        /// claude | codex
        agent: String,
        /// Prompt text; `-` or nothing reads stdin
        prompt: Vec<String>,
        /// Session name or id to reuse (created with this name if missing)
        #[arg(short, long)]
        session: Option<String>,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(short, long)]
        model: Option<String>,
        /// Reasoning effort, e.g. low | medium | high
        #[arg(long)]
        effort: Option<String>,
        /// full | edit | ask | read_only (ask/edit prompt you for approvals)
        #[arg(long)]
        access: Option<String>,
        /// Attach to an existing native conversation id
        #[arg(long)]
        resume: Option<String>,
        /// Print raw events as JSON lines
        #[arg(long)]
        json: bool,
    },
    /// Send a prompt to an existing session
    Send {
        session: String,
        prompt: Vec<String>,
        /// Inject into the running turn instead of queueing (claude, codex)
        #[arg(long)]
        steer: bool,
        #[arg(long)]
        json: bool,
    },
    /// Answer a permission request: allow | allow_session | deny
    Approve { session: String, request_id: String, decision: String },
    /// Change a session's model / effort / access / name (keeps the conversation)
    Configure {
        session: String,
        #[arg(short, long)]
        model: Option<String>,
        #[arg(long)]
        effort: Option<String>,
        #[arg(long)]
        access: Option<String>,
        #[arg(long)]
        name: Option<String>,
    },
    /// List the models an agent can run
    Models {
        agent: String,
        #[arg(long)]
        json: bool,
    },
    /// Interactive chat with an agent
    Chat {
        agent: String,
        #[arg(short, long)]
        session: Option<String>,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(short, long)]
        model: Option<String>,
        #[arg(long)]
        effort: Option<String>,
        #[arg(long)]
        access: Option<String>,
    },
    /// List sessions
    #[command(alias = "ls")]
    Sessions {
        #[arg(long)]
        json: bool,
    },
    /// Show a session's transcript
    History {
        session: String,
        #[arg(long)]
        json: bool,
    },
    /// Follow a session's events live (all sessions when omitted)
    Attach { session: Option<String> },
    /// Interrupt the running turn and drop queued prompts
    Abort { session: String },
    /// Close a session and stop its agent process
    Close { session: String },
    /// Run garbage collection now
    Gc {
        /// Also suspend every idle agent process, regardless of idle timeout
        #[arg(long)]
        idle: bool,
    },
    /// Detect installed agents (no daemon needed)
    Agents {
        /// Print the detection as JSON (`AgentInfo[]`, as `agents.list` returns it)
        #[arg(long)]
        json: bool,
        /// Also look up each agent's newest published version (network)
        #[arg(long)]
        check_updates: bool,
    },
    /// Run the daemon in the foreground
    Daemon {
        /// Port on 127.0.0.1 (0 = any free port; see daemon.json)
        #[arg(long)]
        port: Option<u16>,
        /// Exit when this process exits (for apps bundling agl as a sidecar)
        #[arg(long)]
        parent_pid: Option<u32>,
        /// Local API token (default: random, written to daemon.json)
        #[arg(long)]
        token: Option<String>,
    },
    /// Start the daemon in the background
    Start,
    /// Stop the background daemon
    Stop,
    /// Show daemon and hub link status
    Status {
        /// Print it as JSON, for apps that manage agl (works without a daemon)
        #[arg(long)]
        json: bool,
    },
    /// Link this machine to a hub (web app); several hubs can be linked
    Connect {
        /// Hub URL, e.g. https://hub.example.com
        url: String,
        /// One-time pairing code from the hub (exchanged for a machine token)
        #[arg(long, conflicts_with = "token")]
        code: Option<String>,
        /// Long-lived machine (or hub) token
        #[arg(long)]
        token: Option<String>,
        /// Display name on the hub (defaults to hostname)
        #[arg(long)]
        name: Option<String>,
        /// Access for sessions this hub creates without naming one (default: ask)
        #[arg(long)]
        access: Option<String>,
        /// Do not let this hub see sessions started locally
        #[arg(long)]
        private: bool,
        /// How to reach the hub: `ws` (one WebSocket, default) or `http` (long polling)
        #[arg(long)]
        transport: Option<String>,
    },
    /// Unlink this machine from a hub (the only one, or the named one)
    Disconnect {
        url: Option<String>,
        #[arg(long)]
        all: bool,
    },
    /// Update agl to the newest release and restart the daemon on it
    Update {
        /// Only say whether a newer release exists
        #[arg(long)]
        check: bool,
        /// Print the result as JSON
        #[arg(long)]
        json: bool,
        /// Leave it for later while a session runs a turn (for unattended updates)
        #[arg(long)]
        if_idle: bool,
        /// Install this version instead of the newest (also to go back)
        #[arg(long)]
        to: Option<String>,
    },
    /// Manage autostart (launchd on macOS, systemd --user on Linux)
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Run a reference hub: web UI + install script + relay between browsers and machines
    Hub {
        #[arg(long, default_value = "0.0.0.0:8787")]
        listen: String,
        /// Public URL used in install commands (defaults to the request's Host)
        #[arg(long)]
        public_url: Option<String>,
        /// Access token (generated and saved when omitted)
        #[arg(long)]
        token: Option<String>,
        /// Directory with prebuilt binaries named agl-<os>-<arch> (e.g. agl-linux-x64)
        #[arg(long)]
        dist: Option<PathBuf>,
        /// Redirect downloads not found in --dist here ("" to disable)
        #[arg(long, default_value = hub::DEFAULT_RELEASE_URL)]
        release_url: String,
        /// Refuse request bodies over this many bytes and tell HTTP machines so
        /// (to try a hub that sits behind a WAF)
        #[arg(long)]
        max_body: Option<usize>,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    Install,
    Uninstall,
    Status,
}

#[tokio::main]
async fn main() {
    if let Err(e) = real_main().await {
        eprintln!("agl: {e:#}");
        std::process::exit(1);
    }
}

async fn real_main() -> Result<()> {
    let cli = Cli::parse();
    let mut cfg = Config::load()?;
    match cli.cmd {
        Cmd::Daemon { port, parent_pid, token } => daemon::run(cfg, daemon::DaemonOptions { port, parent_pid, token }).await,
        Cmd::Hub { listen, public_url, token, dist, release_url, max_body } => hub::run(listen, public_url, token, dist, Some(release_url), max_body).await,
        Cmd::Start => {
            let c = ensure_daemon(&cfg).await?;
            let st = c.call("daemon.status", json!({})).await?;
            println!("daemon running on 127.0.0.1:{} (pid {})", st["port"], st["pid"]);
            Ok(())
        }
        Cmd::Stop => {
            match Client::connect_local().await {
                Ok(c) => {
                    let pid = c.call("daemon.status", json!({})).await?["pid"].as_u64().unwrap_or(0) as i32;
                    c.call("daemon.shutdown", json!({})).await?;
                    // Wait until it has stopped its agents and exited.
                    for _ in 0..100 {
                        // SAFETY: signal 0 only checks for existence.
                        if pid <= 0 || unsafe { libc::kill(pid, 0) } != 0 {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    println!("daemon stopped");
                }
                Err(_) => println!("daemon is not running"),
            }
            if service::is_installed() {
                println!("note: the autostart service will start it again; use `agl service uninstall` to stop for good");
            }
            Ok(())
        }
        Cmd::Status { json } => status(&cfg, json).await,
        Cmd::Agents { json, check_updates } => {
            let list = agents::detect(&cfg, check_updates).await;
            if json {
                println!("{}", serde_json::to_string_pretty(&list)?);
                return Ok(());
            }
            for a in list {
                if a.available {
                    let update = match (a.update.as_deref(), a.latest_version.as_deref()) {
                        (Some("available"), Some(v)) => format!("  → {v}"),
                        _ => String::new(),
                    };
                    println!(
                        "{:<9} ✓ {:<34} {:<8} {}{update}",
                        a.id,
                        a.version.unwrap_or_default().chars().take(34).collect::<String>(),
                        a.protocol,
                        dim(&a.features.join(","))
                    );
                } else if let Some(app) = a.desktop_app {
                    println!("{:<9} ✗ {}", a.id, dim(&format!("desktop app only: {app}")));
                } else {
                    println!("{:<9} ✗ {}", a.id, dim("not installed"));
                }
            }
            Ok(())
        }
        Cmd::Models { agent, json } => {
            let r = agents::list_models(&cfg, &agent).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&r)?);
                return Ok(());
            }
            for m in r["models"].as_array().into_iter().flatten() {
                let id = m["id"].as_str().unwrap_or("");
                let name = m["name"].as_str().unwrap_or("");
                let star = if m["default"].as_bool().unwrap_or(false) { " (default)" } else { "" };
                let efforts = m["efforts"].as_array().map(|e| e.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join("/")).unwrap_or_default();
                println!("{id:<40} {}{star} {}", name, dim(&efforts));
            }
            Ok(())
        }
        Cmd::Approve { session, request_id, decision } => {
            let c = ensure_daemon(&cfg).await?;
            c.call("session.approve", json!({"session_id": session, "request_id": request_id, "decision": decision})).await?;
            println!("{decision}");
            Ok(())
        }
        Cmd::Configure { session, model, effort, access, name } => {
            let c = ensure_daemon(&cfg).await?;
            let m = c.call("session.configure", json!({"session_id": session, "model": model, "effort": effort, "access": access, "name": name})).await?;
            println!("{}", serde_json::to_string_pretty(&m)?);
            Ok(())
        }
        Cmd::Run { agent, prompt, session, cwd, model, effort, access, resume, json } => {
            let text = prompt_text(prompt)?;
            let mut c = ensure_daemon(&cfg).await?;
            let cwd = match cwd {
                Some(p) => p,
                None => std::env::current_dir()?,
            };
            let cwd = std::fs::canonicalize(&cwd).with_context(|| format!("cwd {}", cwd.display()))?;
            let meta = c
                .call("session.create", json!({"agent": agent, "cwd": cwd, "model": model, "effort": effort, "access": access, "name": session, "resume": resume}))
                .await?;
            let sid = meta["id"].as_str().unwrap_or_default().to_string();
            let outcome = run_turn(&mut c, &sid, &text, "queue", json).await?;
            if !json {
                let hint = meta["name"].as_str().unwrap_or(&sid);
                eprintln!("{}", dim(&format!("session {sid} · continue: agl send {hint} \"…\"")));
            }
            exit_for(&outcome)
        }
        Cmd::Send { session, prompt, steer, json } => {
            let text = prompt_text(prompt)?;
            let mut c = ensure_daemon(&cfg).await?;
            let meta = c.call("session.get", json!({"session_id": session})).await?;
            let sid = meta["id"].as_str().unwrap_or_default().to_string();
            let outcome = run_turn(&mut c, &sid, &text, if steer { "steer" } else { "queue" }, json).await?;
            exit_for(&outcome)
        }
        Cmd::Chat { agent, session, cwd, model, effort, access } => {
            let mut c = ensure_daemon(&cfg).await?;
            let cwd = std::fs::canonicalize(match cwd {
                Some(p) => p,
                None => std::env::current_dir()?,
            })?;
            let meta = c
                .call("session.create", json!({"agent": agent, "cwd": cwd, "model": model, "effort": effort, "access": access, "name": session, "warm": true}))
                .await?;
            let sid = meta["id"].as_str().unwrap_or_default().to_string();
            eprintln!("{}", dim(&format!("chatting with {agent} · session {sid} · cwd {} · /exit to quit", cwd.display())));
            let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
            loop {
                eprint!("\n› ");
                let _ = std::io::stderr().flush();
                let Some(line) = lines.next_line().await? else { break };
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                if line == "/exit" || line == "/quit" {
                    break;
                }
                run_turn(&mut c, &sid, &line, "queue", false).await?;
            }
            Ok(())
        }
        Cmd::Sessions { json } => {
            let c = ensure_daemon(&cfg).await?;
            let list = c.call("session.list", json!({})).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&list)?);
                return Ok(());
            }
            let rows = list.as_array().cloned().unwrap_or_default();
            if rows.is_empty() {
                println!("no sessions");
                return Ok(());
            }
            println!("{:<13} {:<14} {:<7} {:<10} {:<5} {:>5}  {:<9} CWD", "ID", "NAME", "AGENT", "STATUS", "LIVE", "TURNS", "ACTIVE");
            for s in rows {
                println!(
                    "{:<13} {:<14} {:<7} {:<10} {:<5} {:>5}  {:<9} {}",
                    s["id"].as_str().unwrap_or(""),
                    s["name"].as_str().unwrap_or("-"),
                    s["agent"].as_str().unwrap_or(""),
                    s["status"].as_str().unwrap_or(""),
                    if s["live"].as_bool().unwrap_or(false) { "yes" } else { "no" },
                    s["turns"].as_u64().unwrap_or(0),
                    ago(s["last_active_at"].as_u64().unwrap_or(0)),
                    s["cwd"].as_str().unwrap_or(""),
                );
            }
            Ok(())
        }
        Cmd::History { session, json } => {
            let c = ensure_daemon(&cfg).await?;
            let events: Vec<EventEnvelope> =
                serde_json::from_value(c.call("session.history", json!({"session_id": session})).await?)?;
            let mut r = Renderer::new(json);
            for e in &events {
                r.render(e, true);
            }
            println!();
            Ok(())
        }
        Cmd::Attach { session } => {
            let mut c = ensure_daemon(&cfg).await?;
            match &session {
                Some(s) => c.call("subscribe", json!({"session_id": s})).await?,
                None => c.call("subscribe", json!({"all": true})).await?,
            };
            eprintln!("{}", dim("following events, Ctrl-C to stop"));
            let mut r = Renderer::new(false);
            r.prefix = session.is_none();
            loop {
                tokio::select! {
                    ev = c.events.recv() => match ev {
                        Some(e) => r.render(&e, true),
                        None => bail!("daemon connection closed"),
                    },
                    _ = tokio::signal::ctrl_c() => return Ok(()),
                }
            }
        }
        Cmd::Abort { session } => {
            let c = ensure_daemon(&cfg).await?;
            c.call("session.abort", json!({"session_id": session})).await?;
            println!("abort requested");
            Ok(())
        }
        Cmd::Close { session } => {
            let c = ensure_daemon(&cfg).await?;
            c.call("session.close", json!({"session_id": session})).await?;
            println!("closed");
            Ok(())
        }
        Cmd::Gc { idle } => {
            let c = ensure_daemon(&cfg).await?;
            let r = c.call("gc.run", json!({"idle": idle})).await?;
            println!("suspended {} · removed {}", r["suspended"].as_array().map_or(0, |a| a.len()), r["removed"].as_array().map_or(0, |a| a.len()));
            Ok(())
        }
        Cmd::Connect { url, code, token, name, access, private, transport } => {
            link::ws_url(&url, "")?;
            if code.is_none() && token.is_none() {
                bail!("pass --code <pairing code> or --token <token>");
            }
            if let Some(a) = &access {
                if !agl::session::ACCESS_LEVELS.contains(&a.as_str()) {
                    bail!("--access must be one of {}", agl::session::ACCESS_LEVELS.join(", "));
                }
            }
            if let Some(t) = &transport {
                if !config::TRANSPORTS.contains(&t.as_str()) {
                    bail!("--transport must be one of {}", config::TRANSPORTS.join(", "));
                }
            }
            let url = config::normalize_url(&url);
            // Adding a hub keeps the others; re-connecting one replaces its credentials.
            let mut entry = cfg.hub(&url).cloned().unwrap_or_else(|| config::HubLink::new(&url));
            entry.token = token;
            entry.pair_code = code;
            if let Some(a) = access {
                entry.access = a;
            }
            if private {
                entry.share_local = false;
            }
            if let Some(t) = transport {
                entry.transport = t;
            }
            cfg.hubs.retain(|h| h.url != url);
            cfg.hubs.push(entry.clone());
            if name.is_some() {
                cfg.machine_name = name;
            }
            cfg.save()?;
            let c = ensure_daemon(&cfg).await?;
            c.call("hub.reload", json!({})).await?;
            for _ in 0..60 {
                tokio::time::sleep(Duration::from_millis(250)).await;
                let st = c.call("daemon.status", json!({})).await?;
                let hs = st["hubs"]
                    .as_array()
                    .and_then(|h| h.iter().find(|h| h["url"] == url.as_str()))
                    .and_then(|h| h["status"].as_str())
                    .unwrap_or("")
                    .to_string();
                if hs == "connected" {
                    println!("✓ connected to {url}");
                    println!("  machine id: {}", cfg.machine_id);
                    println!("  sessions it creates default to `{}` access; it {} local sessions", entry.access, if entry.share_local { "can see" } else { "cannot see" });
                    return Ok(());
                }
                if hs.starts_with("error") || hs.starts_with("unauthorized") {
                    bail!("{url}: {hs}");
                }
            }
            bail!("timed out connecting to {url}; check `agl status`")
        }
        Cmd::Disconnect { url, all } => {
            let before = cfg.hubs.len();
            match (&url, all) {
                (_, true) => cfg.hubs.clear(),
                (Some(u), _) => {
                    let u = config::normalize_url(u);
                    cfg.hubs.retain(|h| h.url != u);
                }
                (None, false) if cfg.hubs.len() <= 1 => cfg.hubs.clear(),
                (None, false) => bail!("linked to {} hubs; name one or pass --all", cfg.hubs.len()),
            }
            if cfg.hubs.len() == before {
                bail!("not linked to {}", url.unwrap_or_default());
            }
            cfg.save()?;
            if let Ok(c) = Client::connect_local().await {
                c.call("hub.reload", json!({})).await?;
            }
            println!("disconnected ({} hub{} left)", cfg.hubs.len(), if cfg.hubs.len() == 1 { "" } else { "s" });
            Ok(())
        }
        Cmd::Update { check, json, if_idle, to } => {
            let mut c = update::check().await?;
            if let Some(v) = to {
                c.update_available = v.trim_start_matches('v') != c.current;
                c.latest = v.trim_start_matches('v').to_string();
            }
            let mut out = json!({"current": c.current, "latest": c.latest, "update_available": c.update_available, "updated": false});
            if !check && c.update_available {
                let busy = match Client::connect_local().await {
                    Ok(client) => client.call("daemon.status", json!({})).await.ok().and_then(|s| s["busy"].as_u64()).unwrap_or(0),
                    Err(_) => 0,
                };
                if if_idle && busy > 0 {
                    out["busy"] = json!(busy);
                } else {
                    update::install(&c.latest).await?;
                    update::restart_daemon().await?;
                    out["updated"] = json!(true);
                }
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else if out["updated"] == true {
                println!("updated agl {} → {}", c.current, c.latest);
            } else if !c.update_available {
                println!("agl {} is the newest release", c.current);
            } else if out.get("busy").is_some() {
                println!("agl {} is available; not updating while {} session(s) run a turn", c.latest, out["busy"]);
            } else {
                println!("agl {} is available (current {}); run `agl update`", c.latest, c.current);
            }
            Ok(())
        }
        Cmd::Service { action } => match action {
            ServiceAction::Install => {
                // A daemon the service manager does not own would hold the port
                // and the home; systemd/launchd replace their own instance.
                let managed = service::is_installed();
                if let (false, Ok(c)) = (managed, Client::connect_local().await) {
                    let pid = c.call("daemon.status", json!({})).await.ok().and_then(|s| s["pid"].as_u64()).unwrap_or(0) as i32;
                    let _ = c.call("daemon.shutdown", json!({})).await;
                    for _ in 0..100 {
                        if pid <= 0 || unsafe { libc::kill(pid, 0) } != 0 {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
                service::install()
            }
            ServiceAction::Uninstall => service::uninstall(),
            ServiceAction::Status => {
                println!("{}", if service::is_installed() { "installed" } else { "not installed" });
                Ok(())
            }
        },
    }
}

async fn status(cfg: &Config, as_json: bool) -> Result<()> {
    if as_json {
        let daemon = match Client::connect_local().await {
            Ok(c) => c.call("daemon.status", json!({})).await.ok(),
            Err(_) => None,
        };
        let hubs = match &daemon {
            Some(s) => s["hubs"].clone(),
            None => cfg.hubs.iter().map(|h| json!({"url": h.url, "status": "stopped", "access": h.access, "share_local": h.share_local, "transport": h.transport})).collect(),
        };
        let out = json!({
            "version": env!("CARGO_PKG_VERSION"),
            "running": daemon.is_some(),
            "daemon_version": daemon.as_ref().map(|s| s["version"].clone()),
            "pid": daemon.as_ref().map(|s| s["pid"].clone()),
            "busy": daemon.as_ref().map(|s| s["busy"].clone()),
            "machine_id": cfg.machine_id,
            "service": service::is_installed(),
            "hubs": hubs,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    let Ok(c) = Client::connect_local().await else {
        println!("daemon:   not running (start with `agl start`)");
        for h in &cfg.hubs {
            println!("hub:      {} (not connected)", h.url);
        }
        return Ok(());
    };
    let s = c.call("daemon.status", json!({})).await?;
    println!("daemon:   running · v{} · protocol {} · pid {} · 127.0.0.1:{}", s["version"].as_str().unwrap_or(""), s["protocol"], s["pid"], s["port"]);
    let hubs = s["hubs"].as_array().cloned().unwrap_or_default();
    if hubs.is_empty() {
        println!("hub:      not linked (agl connect <url> --code <code>)");
    }
    for h in hubs {
        println!(
            "hub:      {} · {} · default access {}{}{}",
            h["url"].as_str().unwrap_or(""),
            h["status"].as_str().unwrap_or(""),
            h["access"].as_str().unwrap_or(""),
            if h["share_local"].as_bool().unwrap_or(true) { "" } else { " · private" },
            if h["transport"] == "http" { " · http" } else { "" }
        );
    }
    println!("access:   {} by default (full | edit | ask | read_only)", s["default_access"].as_str().unwrap_or(""));
    println!("machine:  {}", s["machine_id"].as_str().unwrap_or(""));
    println!("sessions: {} ({} live agent processes, max {})", s["sessions"], s["live"], s["max_live"]);
    println!(
        "gc:       suspend idle after {}s · forget after {}s",
        s["idle_timeout_secs"], s["session_ttl_secs"]
    );
    println!("service:  {}", if service::is_installed() { "autostart installed" } else { "no autostart" });
    Ok(())
}

fn prompt_text(parts: Vec<String>) -> Result<String> {
    let joined = parts.join(" ");
    if joined.is_empty() || joined == "-" {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        return Ok(s);
    }
    Ok(joined)
}

fn exit_for(outcome: &str) -> Result<()> {
    match outcome {
        "completed" => Ok(()),
        "aborted" => std::process::exit(130),
        _ => std::process::exit(1),
    }
}

/// Submit a prompt and render its turn until it ends. Ctrl-C aborts the
/// turn; a second Ctrl-C exits immediately.
async fn run_turn(c: &mut Client, sid: &str, text: &str, mode: &str, json: bool) -> Result<String> {
    c.call("subscribe", json!({"session_id": sid})).await?;
    let r = c.call("session.prompt", json!({"session_id": sid, "text": text, "mode": mode})).await?;
    let turn_id = r["turn_id"].as_str().unwrap_or_default().to_string();
    let mut renderer = Renderer::new(json);
    let mut started = false;
    let mut interrupted = false;
    loop {
        tokio::select! {
            ev = c.events.recv() => {
                let Some(env) = ev else { bail!("daemon connection closed") };
                if env.session_id != sid {
                    continue;
                }
                match &env.event {
                    AgentEvent::TurnStarted { turn_id: t } if *t == turn_id => started = true,
                    AgentEvent::Queued { turn_id: t, .. } if *t == turn_id => {}
                    AgentEvent::Steered { turn_id: t, .. } if *t == turn_id => {
                        renderer.render(&env, false);
                        return Ok("completed".into());
                    }
                    AgentEvent::PermissionRequest { request_id, tool, title, input, .. } if started && !json && std::io::stdin().is_terminal() => {
                        renderer.render(&env, false);
                        let decision = ask_permission(tool, title, input).await;
                        if let Err(e) = c.call("session.approve", json!({"session_id": sid, "request_id": request_id, "decision": decision})).await {
                            eprintln!("{}", dim(&format!("[approve failed: {e:#}]")));
                        }
                        continue;
                    }
                    AgentEvent::TurnEnded { turn_id: t, outcome, .. } if *t == turn_id => {
                        renderer.render(&env, false);
                        return Ok(outcome.clone());
                    }
                    _ if !started => continue,
                    _ => {}
                }
                renderer.render(&env, false);
            }
            _ = tokio::signal::ctrl_c() => {
                if interrupted {
                    std::process::exit(130);
                }
                interrupted = true;
                eprintln!("{}", dim("\n[interrupting… press Ctrl-C again to quit]"));
                c.call("session.abort", json!({"session_id": sid})).await?;
            }
        }
    }
}

struct Renderer {
    json: bool,
    prefix: bool,
    at_line_start: bool,
}

impl Renderer {
    fn new(json: bool) -> Self {
        Renderer { json, prefix: false, at_line_start: true }
    }

    fn newline(&mut self) {
        if !self.at_line_start {
            println!();
            self.at_line_start = true;
        }
    }

    fn meta(&mut self, env: &EventEnvelope, s: &str) {
        self.newline();
        let p = if self.prefix { format!("[{}] ", env.session_id) } else { String::new() };
        eprintln!("{}", dim(&format!("{p}{s}")));
    }

    fn render(&mut self, env: &EventEnvelope, show_user: bool) {
        if self.json {
            println!("{}", serde_json::to_string(env).unwrap_or_default());
            return;
        }
        match &env.event {
            AgentEvent::TextDelta { text, .. } => {
                // No blank lines right after a tool/meta line.
                let text = if self.at_line_start { text.trim_start_matches('\n') } else { text.as_str() };
                if text.is_empty() {
                    return;
                }
                if self.prefix && self.at_line_start {
                    print!("[{}] ", env.session_id);
                }
                print!("{text}");
                if !text.is_empty() {
                    self.at_line_start = text.ends_with('\n');
                }
                let _ = std::io::stdout().flush();
            }
            AgentEvent::UserMessage { text, .. } if show_user => {
                self.newline();
                let p = if self.prefix { format!("[{}] ", env.session_id) } else { String::new() };
                println!("\n{p}› {}", text.trim());
            }
            AgentEvent::ToolStart { name, input, .. } => {
                let s = format!("⏺ {name} {}", summarize(input));
                self.meta(env, &s)
            }
            AgentEvent::ToolEnd { ok: false, output, .. } => {
                let first = output.lines().next().unwrap_or("").chars().take(120).collect::<String>();
                self.meta(env, &format!("  ✗ {first}"))
            }
            AgentEvent::Queued { position, .. } => self.meta(env, &format!("[queued, position {position}]")),
            AgentEvent::Steered { .. } => self.meta(env, "[steered into the running turn]"),
            AgentEvent::PermissionRequest { tool, title, request_id, .. } => {
                let s = format!("[permission] {tool}: {title} (agl approve {} {request_id} allow|deny)", env.session_id);
                self.meta(env, &s)
            }
            AgentEvent::PermissionResolved { decision, by, .. } if by != "client" => self.meta(env, &format!("[permission {decision} by {by}]")),
            AgentEvent::Retrying { message } => self.meta(env, &format!("[retrying] {message}")),
            AgentEvent::Heartbeat { stalled: true, idle_ms, .. } => self.meta(env, &format!("[no output for {}s]", idle_ms / 1000)),
            AgentEvent::TurnEnded { outcome, error, .. } => match outcome.as_str() {
                "completed" => self.newline(),
                other => {
                    let s = format!("[{other}{}]", error.as_ref().map(|e| format!(": {e}")).unwrap_or_default());
                    self.meta(env, &s)
                }
            },
            AgentEvent::Error { message } => self.meta(env, &format!("[error] {message}")),
            _ => {}
        }
    }
}

/// Ask on the terminal; anything but a/s counts as deny.
async fn ask_permission(tool: &str, title: &str, input: &Value) -> String {
    let prompt = format!("allow {tool}: {} ? [a]llow / allow for [s]ession / [d]eny: ", if title.is_empty() { summarize(input) } else { title.to_string() });
    tokio::task::spawn_blocking(move || {
        eprint!("{prompt}");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        let tty = std::fs::File::open("/dev/tty");
        match tty {
            Ok(f) => {
                let _ = std::io::BufRead::read_line(&mut std::io::BufReader::new(f), &mut line);
            }
            Err(_) => {
                let _ = std::io::stdin().read_line(&mut line);
            }
        }
        match line.trim() {
            "a" | "y" | "yes" | "allow" => "allow",
            "s" | "session" | "always" => "allow_session",
            _ => "deny",
        }
        .to_string()
    })
    .await
    .unwrap_or_else(|_| "deny".into())
}

fn summarize(input: &Value) -> String {
    let pick = ["command", "file_path", "path", "pattern", "url", "query", "description"]
        .iter()
        .find_map(|k| input.get(*k).and_then(|v| v.as_str()).map(String::from));
    let s = pick.unwrap_or_else(|| input.to_string());
    let s = s.replace('\n', " ");
    if s.chars().count() > 100 {
        format!("{}…", s.chars().take(100).collect::<String>())
    } else {
        s
    }
}

fn dim(s: &str) -> String {
    if std::io::stderr().is_terminal() {
        format!("\x1b[2m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

fn ago(ts: u64) -> String {
    let secs = now_ms().saturating_sub(ts) / 1000;
    match secs {
        0..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}
