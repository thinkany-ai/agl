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
    /// Hub to link to, e.g. `https://hub.example.com`. None = local only.
    pub hub_url: Option<String>,
    pub hub_token: Option<String>,
    /// One-time pairing code, exchanged for `hub_token` on first connect.
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

impl Default for Config {
    fn default() -> Self {
        Config {
            port: DEFAULT_PORT,
            machine_id: String::new(),
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
    pub fn load() -> Result<Config> {
        let path = config_path();
        let mut cfg: Config = match std::fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s).with_context(|| format!("parse {}", path.display()))?,
            Err(_) => Config::default(),
        };
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
