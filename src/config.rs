use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const DEFAULT_PORT: u16 = 7799;

/// Persistent machine configuration, stored at `~/.agl/config.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Port of the local daemon (bound to 127.0.0.1 only).
    pub port: u16,
    /// Stable identity of this machine on a hub.
    pub machine_id: String,
    /// Hubs (web apps) this machine is linked to. Each one sees only the
    /// sessions it created, plus local ones when `share_local` is set.
    pub hubs: Vec<HubLink>,
    /// Legacy single-hub fields, migrated into `hubs` on load.
    #[serde(skip_serializing)]
    pub hub_url: Option<String>,
    #[serde(skip_serializing)]
    pub hub_token: Option<String>,
    #[serde(skip_serializing)]
    pub hub_pair_code: Option<String>,
    /// Display name on the hub (defaults to hostname).
    pub machine_name: Option<String>,
    /// Kill an idle agent process after this many seconds. The session stays
    /// and is resumed with its native id on the next prompt.
    pub idle_timeout_secs: u64,
    /// Forget a session that has not been used for this many seconds.
    pub session_ttl_secs: u64,
    /// Cap on concurrently running agent processes: the least recently used
    /// idle session is suspended to make room, otherwise a turn waits.
    pub max_live: usize,
    /// Default access for new sessions: full | edit | ask | read_only.
    /// `full` runs without permission prompts; `edit`/`ask` forward the
    /// agent's permission requests to clients as `permission_request` events.
    pub default_access: String,
    /// Unanswered permission requests are denied after this many seconds.
    pub approval_timeout_secs: u64,
    /// After the agent reported an API retry, kill the turn if it then stays
    /// silent this long. Plain silence (long tool runs) never kills.
    pub stall_timeout_secs: u64,
    /// Hard cap on one turn; 0 = no cap.
    pub turn_timeout_secs: u64,
    /// Tool output kept per tool call in events.
    pub tool_output_limit: usize,
    /// Events kept per session for `session.history` (also persisted).
    pub history_limit: usize,
    /// Images whose data the history keeps; older ones are kept as `omitted`.
    pub history_image_limit: usize,
    /// Extra agents, e.g. any ACP-speaking CLI:
    /// `{"id":"goose","command":"goose","args":["acp"],"protocol":"acp"}`
    pub custom_agents: Vec<CustomAgent>,
    /// Directories searched for agent executables before the login shell's
    /// PATH, and put first on the PATH agents run with (absolute or `~/…`).
    pub extra_paths: Vec<String>,
    /// Computer use (macOS): apps it may never operate. Only editable here,
    /// never by a hub.
    pub computer: ComputerConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ComputerConfig {
    /// More bundle ids to block (`com.example.app`, or a prefix ending in `*`).
    pub blocked_apps: Vec<String>,
    /// Built-in blocked bundle ids to allow after all (e.g. a terminal).
    pub allowed_blocked_apps: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomAgent {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// acp | claude | codex
    #[serde(default = "default_protocol")]
    pub protocol: String,
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    /// SVG markup shown as the agent's icon (`AgentInfo::icon`); see
    /// `agents::icons` for what is accepted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

fn default_protocol() -> String {
    "acp".into()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HubLink {
    /// e.g. `https://app.example.com/agl`
    pub url: String,
    /// The machine token this hub issued.
    #[serde(default)]
    pub token: Option<String>,
    /// One-time pairing code, exchanged for `token` on first connect.
    #[serde(default)]
    pub pair_code: Option<String>,
    /// Access for sessions this hub creates without naming one.
    #[serde(default = "default_remote_access")]
    pub access: String,
    /// Let this hub see and drive sessions started locally (CLI, apps).
    #[serde(default = "default_true")]
    pub share_local: bool,
    /// `ws` (one WebSocket) or `http` (long polling, for backends that cannot
    /// hold a socket per machine).
    #[serde(default = "default_transport")]
    pub transport: String,
    /// Largest request body the hub accepts (HTTP link), as it said when
    /// pairing; bigger messages are sent in parts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_body: Option<usize>,
    /// The product behind this hub: its sessions' macOS privacy prompts
    /// name it instead of agl, unless `session.create` names another.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_name: Option<String>,
    /// Computer use (macOS) for this hub: its `computer_*` tools. Only the
    /// person sets it, on this computer (`agl computer-use on|off`, or
    /// `agl connect --computer-use`); unset: the build's default (off for
    /// agl).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub computer_use: Option<bool>,
    /// Run this hub's new Claude Code and Codex sessions with data homes of
    /// their own (`$AGL_HOME/agents/<agent>`), signed in with the person's
    /// login, so their conversations stay out of the person's own history
    /// and apps. Only the person sets it, on this computer; unset: the
    /// build's default (off for agl). See `agents::isolation`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolated_agents: Option<bool>,
    /// Let this hub set a session's `instructions` (system / developer
    /// instructions, otherwise local only). Only the person sets it, on this
    /// computer; unset: the build's default (off for agl).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<bool>,
}

pub const TRANSPORTS: [&str; 2] = ["ws", "http"];

fn default_transport() -> String {
    "ws".into()
}

fn default_remote_access() -> String {
    "ask".into()
}

fn default_true() -> bool {
    true
}

impl HubLink {
    pub fn new(url: &str) -> HubLink {
        HubLink { url: normalize_url(url), token: None, pair_code: None, access: default_remote_access(), share_local: true, transport: default_transport(), max_body: None, app_name: None, computer_use: None, isolated_agents: None, instructions: None }
    }
}

pub fn normalize_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

impl Default for Config {
    fn default() -> Self {
        Config {
            port: DEFAULT_PORT,
            machine_id: String::new(),
            hubs: Vec::new(),
            hub_url: None,
            hub_token: None,
            hub_pair_code: None,
            machine_name: None,
            idle_timeout_secs: 10 * 60,
            session_ttl_secs: 7 * 24 * 3600,
            max_live: 8,
            default_access: "full".into(),
            approval_timeout_secs: 600,
            stall_timeout_secs: 120,
            turn_timeout_secs: 0,
            tool_output_limit: 16 * 1024,
            history_limit: 2000,
            history_image_limit: 16,
            custom_agents: Vec::new(),
            extra_paths: Vec::new(),
            computer: ComputerConfig::default(),
        }
    }
}

/// The state directory: `AGL_HOME`, else the brand's home, else `~/.agl`.
pub fn home() -> PathBuf {
    resolve_home(std::env::var("AGL_HOME").ok(), crate::brand::get().home.clone())
}

fn resolve_home(env: Option<String>, brand: Option<PathBuf>) -> PathBuf {
    if let Some(dir) = env {
        return PathBuf::from(dir);
    }
    brand.unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".agl"))
}

pub fn config_path() -> PathBuf {
    home().join("config.json")
}

pub fn sessions_path() -> PathBuf {
    home().join("sessions.json")
}

pub fn log_path() -> PathBuf {
    home().join("daemon.log")
}

/// `daemon.log` for appending, readable by this user only (also when an
/// earlier version created it world-readable).
pub fn open_log() -> Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.create(true).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    let f = o.open(log_path())?;
    #[cfg(unix)]
    f.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    Ok(f)
}

/// Written by a running daemon: `{pid, port, token, version}` (0600).
pub fn daemon_file() -> PathBuf {
    home().join("daemon.json")
}

pub fn history_dir() -> PathBuf {
    home().join("history")
}

/// Write a file atomically with owner-only permissions.
pub fn write_private(path: &std::path::Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp).with_context(|| format!("write {}", tmp.display()))?;
        f.write_all(data)?;
        f.sync_all().ok();
    }
    std::fs::rename(&tmp, path).with_context(|| format!("write {}", path.display()))
}

impl Config {
    pub fn hub(&self, url: &str) -> Option<&HubLink> {
        let url = normalize_url(url);
        self.hubs.iter().find(|h| h.url == url)
    }

    /// Whether computer use is on for this hub (see `HubLink::computer_use`).
    pub fn computer_use_on(&self, url: &str) -> bool {
        self.hub(url).and_then(|h| h.computer_use).unwrap_or(crate::brand::get().computer_use_default)
    }

    /// Whether this hub's new sessions get agent data homes of their own
    /// (see `HubLink::isolated_agents`).
    pub fn isolated_agents_on(&self, url: &str) -> bool {
        self.hub(url).and_then(|h| h.isolated_agents).unwrap_or(crate::brand::get().isolated_agents_default)
    }

    /// Whether this hub may set `instructions` (see `HubLink::instructions`).
    pub fn hub_instructions_on(&self, url: &str) -> bool {
        self.hub(url).and_then(|h| h.instructions).unwrap_or(crate::brand::get().hub_instructions_default)
    }

    pub fn load() -> Result<Config> {
        let path = config_path();
        let mut cfg: Config = match std::fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s).with_context(|| format!("parse {}", path.display()))?,
            Err(_) => Config::default(),
        };
        // Migrate the single-hub fields of agl ≤0.1.
        if let Some(url) = cfg.hub_url.take() {
            if !cfg.hubs.iter().any(|h| h.url == normalize_url(&url)) {
                let mut h = HubLink::new(&url);
                h.token = cfg.hub_token.take();
                h.pair_code = cfg.hub_pair_code.take();
                // Keep the old behaviour for an existing link.
                h.access = cfg.default_access.clone();
                cfg.hubs.push(h);
            }
            cfg.save()?;
        }
        if let Ok(p) = std::env::var("AGL_PORT") {
            cfg.port = p.parse().context("AGL_PORT")?;
        }
        if cfg.machine_id.is_empty() {
            cfg.machine_id = uuid::Uuid::new_v4().to_string();
            cfg.save()?;
        }
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(home())?;
        let path = config_path();
        write_private(&path, serde_json::to_string_pretty(self)?.as_bytes())
    }
}

/// The name the person gave this computer: macOS's ComputerName. Read once.
pub fn computer_name() -> Option<String> {
    static NAME: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    NAME.get_or_init(|| {
        if !cfg!(target_os = "macos") {
            return None;
        }
        let out = std::process::Command::new("/usr/sbin/scutil").args(["--get", "ComputerName"]).output().ok()?;
        let name = String::from_utf8(out.stdout).ok()?.trim().to_string();
        (out.status.success() && !name.is_empty()).then_some(name)
    })
    .clone()
}

/// The kind of computer ("MacBook Pro", "Mac mini"): macOS's model name.
/// Read once (`system_profiler` takes a fraction of a second).
pub fn model() -> Option<String> {
    static MODEL: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    MODEL
        .get_or_init(|| {
            if !cfg!(target_os = "macos") {
                return None;
            }
            let out = std::process::Command::new("/usr/sbin/system_profiler").args(["SPHardwareDataType", "-json"]).output().ok()?;
            model_from_profile(&String::from_utf8_lossy(&out.stdout))
        })
        .clone()
}

fn model_from_profile(json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let name = v["SPHardwareDataType"][0]["machine_name"].as_str()?.trim();
    (!name.is_empty()).then(|| name.to_string())
}

pub fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(test)]
mod tests {

    #[test]
    fn model_name_from_system_profiler() {
        let json = r#"{"SPHardwareDataType":[{"machine_model":"Mac16,1","machine_name":"MacBook Pro"}]}"#;
        assert_eq!(model_from_profile(json).as_deref(), Some("MacBook Pro"));
        assert_eq!(model_from_profile(r#"{"SPHardwareDataType":[{}]}"#), None);
        assert_eq!(model_from_profile("not json"), None);
    }

    use super::*;

    #[test]
    fn home_is_agl_home_then_the_brand_then_dot_agl() {
        let brand = Some(PathBuf::from("/tmp/product-home"));
        assert_eq!(resolve_home(Some("/tmp/env".into()), brand.clone()), PathBuf::from("/tmp/env"));
        assert_eq!(resolve_home(None, brand), PathBuf::from("/tmp/product-home"));
        assert!(resolve_home(None, None).ends_with(".agl"));
    }

    #[test]
    fn hub_links_default_to_ask_and_share_local() {
        let h: HubLink = serde_json::from_str(r#"{"url": "https://a.example/"}"#).unwrap();
        assert_eq!((h.access.as_str(), h.share_local), ("ask", true));
        assert_eq!(HubLink::new("https://a.example/").url, "https://a.example");
    }

    #[test]
    fn legacy_hub_fields_parse_and_are_not_written_back() {
        let cfg: Config = serde_json::from_str(r#"{"hub_url": "https://old.example", "hub_token": "aglm_x"}"#).unwrap();
        assert_eq!(cfg.hub_url.as_deref(), Some("https://old.example"));
        let out = serde_json::to_value(&cfg).unwrap();
        assert!(out.get("hub_url").is_none() && out["hubs"].is_array());
    }
}
