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
        HubLink { url: normalize_url(url), token: None, pair_code: None, access: default_remote_access(), share_local: true, transport: default_transport() }
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
        }
    }
}

pub fn home() -> PathBuf {
    if let Ok(dir) = std::env::var("AGL_HOME") {
        return PathBuf::from(dir);
    }
    dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".agl")
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
    use super::*;

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
