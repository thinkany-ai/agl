//! Agent drivers: one owned child process per live session.
//!
//! Each agent is reached through the best interface it offers:
//! - `claude`: persistent `claude -p` stream-json (control protocol for
//!   approvals, interrupt, model listing);
//! - `codex`: persistent `codex app-server` JSON-RPC (approvals, steer,
//!   interrupt, model/list);
//! - ACP agents (gemini, opencode, kimi, grok, …, plus any custom one):
//!   persistent JSON-RPC Agent Client Protocol;
//! - one-shot JSON CLIs (cursor-agent, pi): one process per turn with native
//!   resume.
//!
//! A driver turns `prompt` / `steer` / `interrupt` / `respond` calls into the
//! native protocol and reports [`DriverMsg`]s tagged with its generation so a
//! late message from a replaced process is recognisable.

mod acp;
mod claude;
mod codex;
mod install;
mod oneshot;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt};
use tokio::sync::mpsc::UnboundedSender;

use crate::config::{self, Config, CustomAgent};
use crate::protocol::{AgentEvent, AgentInfo, ToolImage};
use crate::shellenv;

/// Largest JSON frame accepted from an agent; longer lines are dropped.
pub const MAX_FRAME: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Claude,
    Codex,
    Acp,
    Cursor,
    Pi,
}

impl Protocol {
    pub fn name(self) -> &'static str {
        match self {
            Protocol::Claude => "claude",
            Protocol::Codex => "codex",
            Protocol::Acp => "acp",
            Protocol::Cursor | Protocol::Pi => "oneshot",
        }
    }

    pub fn features(self) -> Vec<String> {
        let f: &[&str] = match self {
            Protocol::Claude => &["warm", "approvals", "steer", "interrupt", "models", "effort"],
            Protocol::Codex => &["warm", "approvals", "steer", "interrupt", "models", "effort"],
            Protocol::Acp => &["warm", "approvals", "interrupt", "models"],
            Protocol::Cursor | Protocol::Pi => &["resume", "interrupt", "models"],
        };
        f.iter().map(|s| s.to_string()).collect()
    }
}

#[derive(Debug, Clone)]
pub struct AgentDef {
    pub id: String,
    pub name: String,
    pub protocol: Protocol,
    /// Executable candidates, first found wins.
    pub commands: Vec<String>,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

fn def(id: &str, name: &str, protocol: Protocol, commands: &[&str], args: &[&str], env: &[(&str, &str)]) -> AgentDef {
    AgentDef {
        id: id.into(),
        name: name.into(),
        protocol,
        commands: commands.iter().map(|s| s.to_string()).collect(),
        args: args.iter().map(|s| s.to_string()).collect(),
        env: env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
    }
}

/// Built-in agents plus `custom_agents` from the config (which may override).
pub fn registry(cfg: &Config) -> Vec<AgentDef> {
    use Protocol::*;
    let mut v = vec![
        def("claude", "Claude Code", Claude, &["claude"], &[], &[]),
        def("codex", "Codex", Codex, &["codex"], &[], &[]),
        def("gemini", "Gemini CLI", Acp, &["gemini"], &["--acp"], &[("GEMINI_CLI_TRUST_WORKSPACE", "true")]),
        def("opencode", "OpenCode", Acp, &["opencode"], &["acp"], &[]),
        def("kimi", "Kimi Code", Acp, &["kimi"], &["acp"], &[]),
        def("grok", "Grok Build", Acp, &["grok"], &["agent", "stdio"], &[]),
        def("droid", "Factory Droid", Acp, &["droid"], &["exec", "--output-format", "acp-daemon"], &[]),
        def("openclaw", "OpenClaw", Acp, &["openclaw"], &["acp"], &[]),
        def("hermes", "Hermes", Acp, &["hermes"], &["acp"], &[]),
        def("omp", "OMP", Acp, &["omp"], &["acp"], &[]),
        def("cursor", "Cursor Agent", Cursor, &["cursor-agent"], &[], &[]),
        def("pi", "Pi", Pi, &["pi"], &[], &[]),
    ];
    for c in &cfg.custom_agents {
        let protocol = match c.protocol.as_str() {
            "claude" => Claude,
            "codex" => Codex,
            _ => Acp,
        };
        let d = custom_def(c, protocol);
        v.retain(|x| x.id != d.id);
        v.push(d);
    }
    v
}

fn custom_def(c: &CustomAgent, protocol: Protocol) -> AgentDef {
    AgentDef {
        id: c.id.clone(),
        name: c.name.clone().unwrap_or_else(|| c.id.clone()),
        protocol,
        commands: vec![c.command.clone()],
        args: c.args.clone(),
        env: c.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
    }
}

pub fn find_def(cfg: &Config, id: &str) -> Option<AgentDef> {
    registry(cfg).into_iter().find(|d| d.id == id)
}

/// Locate an agent's executable (`AGL_<ID>_BIN` overrides).
pub fn find_binary(d: &AgentDef) -> Option<PathBuf> {
    let env_key = format!("AGL_{}_BIN", d.id.to_uppercase().replace('-', "_"));
    if let Ok(p) = std::env::var(&env_key) {
        return Some(PathBuf::from(p));
    }
    if let Some(p) = d.commands.iter().find_map(|c| shellenv::which(c)) {
        return Some(p);
    }
    // The Codex engine bundled in the desktop apps.
    if d.id == "codex" && cfg!(target_os = "macos") {
        let home = dirs::home_dir().unwrap_or_default();
        for root in [PathBuf::from("/Applications"), home.join("Applications")] {
            for app in ["Codex.app", "ChatGPT.app"] {
                let p = root.join(app).join("Contents/Resources/codex");
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    AllowSession,
    Deny,
}

impl Decision {
    pub fn parse(s: &str) -> Option<Decision> {
        match s {
            "allow" | "allow_once" | "accept" | "yes" => Some(Decision::Allow),
            "allow_session" | "allow_always" | "always" => Some(Decision::AllowSession),
            "deny" | "reject" | "decline" | "no" => Some(Decision::Deny),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Decision::Allow => "allow",
            Decision::AllowSession => "allow_session",
            Decision::Deny => "deny",
        }
    }
}

#[derive(Debug)]
pub struct PermissionAsk {
    /// Driver-unique key, handed back in `respond`.
    pub key: String,
    pub tool: String,
    pub title: String,
    pub input: Value,
    pub options: Vec<String>,
}

#[derive(Debug)]
pub enum DriverMsg {
    Event(AgentEvent),
    /// The runtime's own conversation id, used to resume after a restart.
    NativeId(String),
    Permission(PermissionAsk),
    /// The runtime withdrew a permission request (turn ended / cancelled).
    PermissionCancelled(String),
    TurnDone {
        outcome: String,
        error: Option<String>,
        usage: Option<Value>,
    },
    Exited(String),
}

pub type DriverTx = UnboundedSender<(u64, DriverMsg)>;

#[derive(Debug, Clone)]
pub struct SpawnSpec {
    pub def: AgentDef,
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Native conversation id to resume, if any.
    pub resume: Option<String>,
    /// full | edit | ask | read_only
    pub access: String,
    /// Retry without API-key variables so the CLI uses its own login.
    pub strip_api_key: bool,
    pub tool_output_limit: usize,
    pub generation: u64,
}

/// A throwaway spec for metadata probes (model lists).
pub(crate) fn probe_spec(id: &str, protocol: Protocol) -> SpawnSpec {
    SpawnSpec {
        def: AgentDef { id: id.into(), name: String::new(), protocol, commands: vec![], args: vec![], env: vec![] },
        cwd: std::env::temp_dir(),
        model: None,
        effort: None,
        resume: None,
        access: "read_only".into(),
        strip_api_key: false,
        tool_output_limit: 0,
        generation: 0,
    }
}

pub enum Driver {
    Claude(claude::ClaudeDriver),
    Codex(codex::CodexDriver),
    Acp(acp::AcpDriver),
    OneShot(oneshot::OneShotDriver),
}

impl Driver {
    pub async fn spawn(spec: &SpawnSpec, tx: DriverTx) -> Result<Driver> {
        let Some(bin) = find_binary(&spec.def) else {
            bail!("agent `{}` is not installed (looked for {} on PATH)", spec.def.id, spec.def.commands.join(", "));
        };
        Ok(match spec.def.protocol {
            Protocol::Claude => Driver::Claude(claude::ClaudeDriver::spawn(&bin, spec, tx).await?),
            Protocol::Codex => Driver::Codex(codex::CodexDriver::spawn(&bin, spec, tx).await?),
            Protocol::Acp => Driver::Acp(acp::AcpDriver::spawn(&bin, spec, tx).await?),
            Protocol::Cursor | Protocol::Pi => Driver::OneShot(oneshot::OneShotDriver::new(bin, spec, tx)),
        })
    }

    pub async fn prompt(&mut self, text: &str) -> Result<()> {
        match self {
            Driver::Claude(d) => d.prompt(text).await,
            Driver::Codex(d) => d.prompt(text).await,
            Driver::Acp(d) => d.prompt(text).await,
            Driver::OneShot(d) => d.prompt(text).await,
        }
    }

    /// Inject input into the running turn. Ok(false) = not supported, queue it.
    pub async fn steer(&mut self, text: &str) -> Result<bool> {
        match self {
            Driver::Claude(d) => d.prompt(text).await.map(|_| true),
            Driver::Codex(d) => d.steer(text).await.map(|_| true),
            _ => Ok(false),
        }
    }

    pub async fn interrupt(&mut self) -> Result<()> {
        match self {
            Driver::Claude(d) => d.interrupt().await,
            Driver::Codex(d) => d.interrupt().await,
            Driver::Acp(d) => d.interrupt().await,
            Driver::OneShot(d) => d.interrupt().await,
        }
    }

    pub async fn respond(&mut self, key: &str, decision: Decision) -> Result<()> {
        match self {
            Driver::Claude(d) => d.respond(key, decision).await,
            Driver::Codex(d) => d.respond(key, decision).await,
            Driver::Acp(d) => d.respond(key, decision).await,
            Driver::OneShot(_) => Ok(()),
        }
    }

    pub async fn shutdown(self) {
        match self {
            Driver::Claude(d) => d.shutdown().await,
            Driver::Codex(d) => d.shutdown().await,
            Driver::Acp(d) => d.shutdown().await,
            Driver::OneShot(d) => d.shutdown().await,
        }
    }
}

/// Probe every registered agent concurrently (bounded per probe). With
/// `check_updates`, also look up each installed agent's newest published
/// version (network).
pub async fn detect(cfg: &Config, check_updates: bool) -> Vec<AgentInfo> {
    let defs = registry(cfg);
    let custom: Vec<String> = cfg.custom_agents.iter().map(|c| c.id.clone()).collect();
    let probes = defs.into_iter().map(|d| {
        // A custom agent is the user's own command, not the built-in one.
        let builtin = !custom.contains(&d.id);
        async move {
            let path = find_binary(&d);
            let (version, latest_version) = match &path {
                Some(p) => {
                    let latest = async { if check_updates && builtin { install::latest_version(&d.id).await } else { None } };
                    tokio::join!(version_of(p), latest)
                }
                None => (None, None),
            };
            let desktop_app = builtin.then(|| install::desktop_app(&d.id)).flatten();
            let status = if path.is_some() {
                "ready"
            } else if desktop_app.is_some() {
                "desktop-only"
            } else {
                "not-found"
            };
            let update = (check_updates && builtin && path.is_some())
                .then(|| install::update_status(&d.id, version.as_deref(), latest_version.as_deref()).to_string());
            AgentInfo {
                id: d.id.clone(),
                name: d.name.clone(),
                protocol: d.protocol.name().into(),
                available: path.is_some(),
                install: builtin.then(|| install::install_info(&d.id, path.as_deref())).flatten(),
                path: path.map(|p| p.display().to_string()),
                version,
                features: d.protocol.features(),
                status: status.into(),
                desktop_app: desktop_app.map(|p| p.display().to_string()),
                latest_version,
                update,
            }
        }
    });
    futures_util::future::join_all(probes).await
}

async fn version_of(p: &std::path::Path) -> Option<String> {
    let out = run_capture(p, &["--version"], Duration::from_secs(8)).await?;
    out.lines().map(str::trim).find(|l| !l.is_empty()).map(|l| l.chars().take(160).collect())
}

/// Run a short command with the agent environment; stdout if it succeeded.
pub(crate) async fn run_capture(bin: &std::path::Path, args: &[&str], timeout: Duration) -> Option<String> {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    shellenv::apply(&mut cmd);
    let out = tokio::time::timeout(timeout, cmd.output()).await.ok()?.ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout[..out.stdout.len().min(2 << 20)]).into_owned())
}

/// Turns one line of a CLI's model listing into a model entry.
type LineParser = fn(&str) -> Option<Value>;

/// Models an agent can run, without starting a conversation.
pub async fn list_models(cfg: &Config, id: &str) -> Result<Value> {
    let Some(d) = find_def(cfg, id) else { bail!("unknown agent `{id}`") };
    let Some(bin) = find_binary(&d) else { bail!("agent `{id}` is not installed") };
    let models: Vec<Value> = match d.protocol {
        Protocol::Claude => claude::list_models(&bin).await,
        Protocol::Codex => codex::list_models(&bin).await?,
        Protocol::Acp => acp::list_models(&bin, &d).await?,
        _ => {
            let (args, parse): (&[&str], LineParser) = match d.id.as_str() {
                "opencode" => (&["models"], |l| {
                    (l.contains('/') && !l.contains(char::is_whitespace)).then(|| json!({"id": l, "name": l}))
                }),
                "cursor" => (&["models"], |l| {
                    let (id, name) = l.split_once(" - ")?;
                    let id = id.trim();
                    (!id.is_empty() && !id.contains(' ')).then(|| json!({"id": id, "name": name.trim()}))
                }),
                "grok" => (&["models"], |l| {
                    let id = l.trim().strip_prefix("* ").or(l.trim().strip_prefix("- "))?;
                    let id = id.split_whitespace().next()?;
                    Some(json!({"id": id, "name": id}))
                }),
                _ => (&[], |_| None),
            };
            if args.is_empty() {
                vec![]
            } else {
                let out = run_capture(&bin, args, Duration::from_secs(15)).await.unwrap_or_default();
                out.lines().map(str::trim).filter_map(parse).take(200).collect()
            }
        }
    };
    Ok(json!({"agent": id, "models": models}))
}

/// Read one newline-terminated frame, bounded by `max`. Oversized frames are
/// skipped whole. Ok(None) at EOF.
pub(crate) async fn next_frame<R: AsyncBufRead + Unpin>(r: &mut R, max: usize) -> std::io::Result<Option<Vec<u8>>> {
    let mut out = Vec::new();
    let mut overflow = false;
    loop {
        let buf = r.fill_buf().await?;
        if buf.is_empty() {
            return Ok(if out.is_empty() { None } else { Some(out) });
        }
        if let Some(i) = buf.iter().position(|&b| b == b'\n') {
            if !overflow {
                out.extend_from_slice(&buf[..i]);
            }
            r.consume(i + 1);
            if overflow {
                eprintln!("agl: dropped an agent frame larger than {max} bytes");
                overflow = false;
                out.clear();
                continue;
            }
            return Ok(Some(out));
        }
        let n = buf.len();
        if !overflow {
            if out.len() + n > max {
                overflow = true;
                out.clear();
            } else {
                out.extend_from_slice(buf);
            }
        }
        r.consume(n);
    }
}

/// Spawn a JSON-lines reader: each parsed frame goes to `on_frame`; EOF ends it.
pub(crate) fn spawn_frame_reader<R, F>(stdout: R, mut on_frame: F) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    F: FnMut(Value) + Send + 'static,
{
    tokio::spawn(async move {
        let mut r = tokio::io::BufReader::with_capacity(64 * 1024, stdout);
        while let Ok(Some(line)) = next_frame(&mut r, MAX_FRAME).await {
            if let Ok(v) = serde_json::from_slice::<Value>(&line) {
                on_frame(v);
            }
        }
    })
}

/// Common process setup: agent env, own process group, piped stdio.
pub(crate) fn base_command(bin: &std::path::Path, spec: &SpawnSpec) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(bin);
    shellenv::apply(&mut cmd);
    // A daemon started from inside Claude Code inherits its session markers.
    for (k, _) in std::env::vars() {
        if k == "CLAUDECODE" || k == "CLAUDE_PID" || k.starts_with("CLAUDE_CODE_") && k != "CLAUDE_CODE_OAUTH_TOKEN" {
            cmd.env_remove(k);
        }
    }
    if spec.strip_api_key {
        for k in API_KEY_VARS {
            cmd.env_remove(k);
        }
    }
    for (k, v) in &spec.def.env {
        cmd.env(k, v);
    }
    cmd.current_dir(&spec.cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    cmd
}

/// Variables that make Claude Code prefer an API key over the user's login.
pub const API_KEY_VARS: &[&str] = &["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "CLAUDE_CODE_OAUTH_TOKEN", "ANTHROPIC_BASE_URL"];

/// Stop an agent and every process it started: SIGTERM the group, then
/// SIGKILL after a grace period (taskkill /T /F on Windows).
pub(crate) async fn kill_tree(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        #[cfg(unix)]
        {
            // Tool commands (claude Bash, codex exec) run in their own sessions
            // and escape the group, so collect the whole tree first.
            let tree = descendants(pid);
            let pgid = -(pid as i32);
            // SAFETY: kill(2) on the process group we created and its descendants.
            unsafe { libc::kill(pgid, libc::SIGTERM) };
            for p in &tree {
                unsafe { libc::kill(*p as i32, libc::SIGTERM) };
            }
            let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
            tokio::time::sleep(Duration::from_millis(150)).await;
            unsafe { libc::kill(pgid, libc::SIGKILL) };
            for p in &tree {
                unsafe { libc::kill(*p as i32, libc::SIGKILL) };
            }
        }
        #[cfg(windows)]
        {
            let _ = tokio::process::Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .output()
                .await;
        }
        untrack(pid);
    }
    let _ = child.kill().await;
}

/// All descendants of `root` from the process table.
#[cfg(unix)]
fn descendants(root: u32) -> Vec<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (pid, ppid) in process_table() {
        children.entry(ppid).or_default().push(pid);
    }
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(p) = stack.pop() {
        for c in children.get(&p).into_iter().flatten() {
            out.push(*c);
            stack.push(*c);
        }
    }
    out
}

/// Keep the last lines of a child's stderr for error reports.
pub(crate) fn spawn_stderr_tail(stderr: tokio::process::ChildStderr) -> Arc<std::sync::Mutex<Vec<String>>> {
    let tail = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let t = tail.clone();
    tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let line = strip_ansi(&line);
            if line.trim().is_empty() {
                continue;
            }
            let mut v = t.lock().unwrap();
            v.push(line.chars().take(400).collect());
            if v.len() > 12 {
                v.remove(0);
            }
        }
    });
    tail
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' {
            if it.peek() == Some(&'[') {
                it.next();
                for c in it.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// Images larger than this (base64 bytes) are reported as `omitted`.
const MAX_IMAGE_BYTES: usize = 4 << 20;

/// Split a tool result into its text and its images.
///
/// Understands the shapes the runtimes use: an MCP `CallToolResult`
/// (`{content: [...], structuredContent, _meta}`), MCP/ACP content blocks
/// (`{type: "image", data, mimeType}`, ACP's `{type: "content", content}`),
/// Anthropic image blocks (`{type: "image", source: {data, media_type}}`) and
/// Codex dynamic-tool items (`{type: "inputImage", imageUrl: "data:…"}`).
/// Metadata such as `_meta` is dropped; anything unrecognized is kept as JSON.
pub(crate) fn split_content(v: &Value, limit: usize) -> (String, Vec<ToolImage>) {
    let mut text = Vec::new();
    let mut images = Vec::new();
    collect_content(v, &mut text, &mut images);
    (clip(&text.join("\n"), limit), images)
}

fn collect_content(v: &Value, text: &mut Vec<String>, images: &mut Vec<ToolImage>) {
    match v {
        Value::Null => {}
        Value::String(s) => {
            if !s.is_empty() {
                text.push(s.clone());
            }
        }
        Value::Array(items) => items.iter().for_each(|item| collect_content(item, text, images)),
        Value::Object(o) => {
            let kind = o.get("type").and_then(Value::as_str).unwrap_or_default();
            match kind {
                "text" | "input_text" | "inputText" => {
                    if let Some(t) = o.get("text").and_then(Value::as_str) {
                        text.push(t.to_string());
                    }
                }
                "image" | "input_image" | "inputImage" => {
                    if let Some(image) = image_block(o) {
                        images.push(image);
                    }
                }
                "resource_link" => {
                    if let Some(uri) = o.get("uri").and_then(Value::as_str) {
                        text.push(uri.to_string());
                    }
                }
                "resource" => collect_content(&o["resource"], text, images),
                // A tool result or an ACP content wrapper.
                _ if o.contains_key("content") => collect_content(&o["content"], text, images),
                _ if o.contains_key("contentItems") => collect_content(&o["contentItems"], text, images),
                _ if o.get("text").is_some_and(Value::is_string) => {
                    text.push(o["text"].as_str().unwrap_or_default().to_string());
                }
                _ => text.push(v.to_string()),
            }
        }
        other => text.push(other.to_string()),
    }
}

fn image_block(o: &serde_json::Map<String, Value>) -> Option<ToolImage> {
    let str_at = |v: Option<&Value>, key: &str| v.and_then(|v| v.get(key)).and_then(Value::as_str).map(String::from);
    let (mime_type, data) = if let Some(data) = o.get("data").and_then(Value::as_str) {
        let mime = o.get("mimeType").or(o.get("mime_type")).and_then(Value::as_str).unwrap_or("image/png");
        (mime.to_string(), data.to_string())
    } else if let Some(data) = str_at(o.get("source"), "data") {
        (str_at(o.get("source"), "media_type").unwrap_or_else(|| "image/png".into()), data)
    } else {
        // `data:image/png;base64,…`; remote URLs are not fetched.
        let url = o.get("imageUrl").or(o.get("image_url")).and_then(Value::as_str)?;
        let (head, data) = url.strip_prefix("data:")?.split_once(";base64,")?;
        (head.to_string(), data.to_string())
    };
    Some(tool_image(mime_type, data))
}

fn tool_image(mime_type: String, data: String) -> ToolImage {
    if data.len() > MAX_IMAGE_BYTES {
        return ToolImage { mime_type, data: String::new(), omitted: true };
    }
    ToolImage { mime_type, data, omitted: false }
}

/// An image from base64 data or a `data:` URL; bare data is taken as PNG.
pub(crate) fn base64_image(data: &str) -> Option<ToolImage> {
    if data.is_empty() {
        return None;
    }
    let (mime, data) = match data.strip_prefix("data:").and_then(|d| d.split_once(";base64,")) {
        Some((mime, data)) => (mime, data),
        None => ("image/png", data),
    };
    Some(tool_image(mime.to_string(), data.to_string()))
}

/// An image file the agent wrote or viewed, read from disk.
pub(crate) fn file_image(path: &Path) -> Option<ToolImage> {
    use base64::Engine as _;
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let mime = match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return None,
    };
    // Skip reading what would be omitted anyway (base64 grows data by 4/3).
    if std::fs::metadata(path).ok()?.len() as usize > MAX_IMAGE_BYTES / 4 * 3 {
        return Some(ToolImage { mime_type: mime.into(), data: String::new(), omitted: true });
    }
    let bytes = std::fs::read(path).ok()?;
    Some(tool_image(mime.into(), base64::engine::general_purpose::STANDARD.encode(bytes)))
}

/// Keep tool output small enough for an event stream.
pub(crate) fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [{} bytes truncated]", &s[..end], s.len() - end)
}

// --- orphan tracking -------------------------------------------------------
//
// Every agent process group is recorded with its start time. A daemon that
// crashed leaves them behind; the next daemon kills the ones whose start time
// still matches, and never touches a reused pid.

fn pid_dir() -> PathBuf {
    config::home().join("pids")
}

/// (pid, ppid) of every process: /proc on Linux (minimal images have no
/// `ps`), `ps` elsewhere.
#[cfg(unix)]
fn process_table() -> Vec<(u32, u32)> {
    if let Ok(rd) = std::fs::read_dir("/proc") {
        let table: Vec<(u32, u32)> = rd
            .flatten()
            .filter_map(|e| {
                let pid: u32 = e.file_name().to_str()?.parse().ok()?;
                let stat = std::fs::read_to_string(e.path().join("stat")).ok()?;
                // The command name may contain spaces; fields follow the last ')'.
                let ppid = stat.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()?;
                Some((pid, ppid))
            })
            .collect();
        if !table.is_empty() {
            return table;
        }
    }
    let Ok(out) = std::process::Command::new("ps").args(["-A", "-o", "pid=,ppid="]).output() else { return vec![] };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace().filter_map(|x| x.parse::<u32>().ok());
            Some((it.next()?, it.next()?))
        })
        .collect()
}

/// The process start time: with the pid it identifies a process, so a reused
/// pid is never mistaken for ours. (Not the command line: shebang scripts and
/// npm wrappers exec into something else right after spawn.)
fn fingerprint(pid: u32) -> Option<String> {
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        return stat.rsplit_once(')')?.1.split_whitespace().nth(19).map(String::from);
    }
    let out = std::process::Command::new("ps").args(["-o", "lstart=", "-p", &pid.to_string()]).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

pub(crate) fn track(pid: u32) {
    if cfg!(windows) {
        return;
    }
    if let Some(fp) = fingerprint(pid) {
        let _ = std::fs::create_dir_all(pid_dir());
        let _ = std::fs::write(pid_dir().join(pid.to_string()), fp);
    }
}

pub(crate) fn untrack(pid: u32) {
    let _ = std::fs::remove_file(pid_dir().join(pid.to_string()));
}

/// Kill agent process groups left by a daemon that did not exit cleanly.
pub fn reap_orphans() -> usize {
    let Ok(rd) = std::fs::read_dir(pid_dir()) else { return 0 };
    let mut n = 0;
    for e in rd.flatten() {
        let path = e.path();
        let pid: Option<u32> = path.file_name().and_then(|f| f.to_str()).and_then(|f| f.parse().ok());
        if let (Some(pid), Ok(saved)) = (pid, std::fs::read_to_string(&path)) {
            if fingerprint(pid).as_deref() == Some(saved.trim()) {
                #[cfg(unix)]
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
                n += 1;
            }
        }
        let _ = std::fs::remove_file(path);
    }
    n
}

/// A JSON-RPC 2.0 peer over a child's stdio, shared by codex and ACP.
pub(crate) mod rpc {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;
    use tokio::io::AsyncWriteExt;
    use tokio::process::ChildStdin;
    use tokio::sync::{oneshot, Mutex as AsyncMutex};

    pub type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

    #[derive(Clone)]
    pub struct Peer {
        pub stdin: Arc<AsyncMutex<ChildStdin>>,
        pub pending: Pending,
        next_id: Arc<AtomicU64>,
        pub name: &'static str,
    }

    impl Peer {
        pub fn new(stdin: ChildStdin, name: &'static str) -> Peer {
            Peer { stdin: Arc::new(AsyncMutex::new(stdin)), pending: Arc::default(), next_id: Arc::new(AtomicU64::new(1)), name }
        }

        pub async fn write(&self, v: &Value) -> Result<()> {
            let mut w = self.stdin.lock().await;
            w.write_all(format!("{v}\n").as_bytes()).await?;
            w.flush().await?;
            Ok(())
        }

        pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
            self.write(&json!({"jsonrpc": "2.0", "method": method, "params": params})).await
        }

        pub async fn reply(&self, id: &Value, result: Value) -> Result<()> {
            self.write(&json!({"jsonrpc": "2.0", "id": id, "result": result})).await
        }

        pub async fn reply_error(&self, id: &Value, code: i64, message: &str) -> Result<()> {
            self.write(&json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})).await
        }

        /// Request with an acknowledgement deadline (not a turn deadline).
        pub async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
            let id = self.next_id.fetch_add(1, Ordering::SeqCst);
            let (tx, rx) = oneshot::channel();
            self.pending.lock().unwrap().insert(id, tx);
            if let Err(e) = self.write(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await {
                self.pending.lock().unwrap().remove(&id);
                return Err(e);
            }
            match tokio::time::timeout(timeout, rx).await {
                Ok(Ok(Ok(v))) => Ok(v),
                Ok(Ok(Err(e))) => bail!("{} {method}: {e}", self.name),
                Ok(Err(_)) => bail!("{} {method}: process exited", self.name),
                Err(_) => {
                    self.pending.lock().unwrap().remove(&id);
                    bail!("{} did not acknowledge {method} within {}s", self.name, timeout.as_secs())
                }
            }
        }

        /// Route a reply frame to its waiter. Returns false if `v` is not a reply.
        pub fn route_reply(&self, v: &Value) -> bool {
            if v.get("method").is_some() || v.get("id").is_none() {
                return false;
            }
            if let Some(tx) = v["id"].as_u64().and_then(|id| self.pending.lock().unwrap().remove(&id)) {
                let res = match v.get("error") {
                    Some(e) if !e.is_null() => Err(e["message"].as_str().unwrap_or("rpc error").to_string()),
                    _ => Ok(v["result"].clone()),
                };
                let _ = tx.send(res);
            }
            true
        }

        pub fn fail_all(&self) {
            for (_, p) in self.pending.lock().unwrap().drain() {
                let _ = p.send(Err("process exited".into()));
            }
        }
    }
}

#[cfg(test)]
mod reap_tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn reaps_a_tracked_process_that_exec_ed_after_spawn() {
        let home = std::env::temp_dir().join(format!("agl-reap-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&home).unwrap();
        std::env::set_var("AGL_HOME", &home);
        use std::os::unix::process::CommandExt;
        // `sh` replaces itself with `sleep`, like shebang scripts and npm shims.
        let mut child = std::process::Command::new("sh").args(["-c", "exec sleep 30"]).process_group(0).spawn().unwrap();
        track(child.id());
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(reap_orphans(), 1);
        let status = child.wait().unwrap();
        assert!(!status.success(), "the orphan was killed");
        assert_eq!(reap_orphans(), 0, "records are consumed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_tool_result_splits_text_and_images() {
        // Codex computer-use screenshot, as an MCP CallToolResult.
        let result = json!({
            "content": [
                {"type": "text", "text": "Window: \"Notes\""},
                {"type": "image", "data": "iVBORw0KGgo=", "mimeType": "image/png"}
            ],
            "structuredContent": null,
            "_meta": {"codex/browserUse": true}
        });
        let (text, images) = split_content(&result, 1024);
        assert_eq!(text, "Window: \"Notes\"");
        assert_eq!(images, vec![ToolImage { mime_type: "image/png".into(), data: "iVBORw0KGgo=".into(), omitted: false }]);
    }

    #[test]
    fn anthropic_acp_and_data_url_images() {
        let claude = json!([{"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "/9j/"}}]);
        assert_eq!(split_content(&claude, 1024).1[0].mime_type, "image/jpeg");

        let acp = json!([{"type": "content", "content": {"type": "image", "data": "AAAA", "mimeType": "image/webp"}}]);
        assert_eq!(split_content(&acp, 1024).1[0].data, "AAAA");

        let dynamic = json!([{"type": "inputImage", "imageUrl": "data:image/png;base64,QUJD"}, {"type": "inputText", "text": "ok"}]);
        let (text, images) = split_content(&dynamic, 1024);
        assert_eq!((text.as_str(), images[0].data.as_str()), ("ok", "QUJD"));
    }

    #[test]
    fn oversized_images_are_omitted_and_text_is_clipped() {
        let big = "A".repeat(MAX_IMAGE_BYTES + 1);
        let (_, images) = split_content(&json!([{"type": "image", "data": big, "mimeType": "image/png"}]), 1024);
        assert!(images[0].omitted && images[0].data.is_empty());

        let (text, _) = split_content(&json!("x".repeat(100)), 10);
        assert!(text.starts_with("xxxxxxxxxx…"));
    }

    #[test]
    fn plain_and_unknown_values_stay_text() {
        assert_eq!(split_content(&json!("a.txt\n"), 1024).0, "a.txt\n");
        assert_eq!(split_content(&json!({"exit": 0}), 1024).0, r#"{"exit":0}"#);
        assert_eq!(split_content(&Value::Null, 1024), (String::new(), vec![]));
    }
}
