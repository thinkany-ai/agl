//! Agent data homes apart from the person's own.
//!
//! A hub's sessions run Claude Code and Codex on this computer, and those
//! CLIs keep every conversation in their data home (`~/.claude`,
//! `~/.codex`), where the person's own apps list them — the Codex desktop
//! app shows them among the person's chats. With `HubLink::isolated_agents`
//! a hub's new sessions get a home of their own under
//! `$AGL_HOME/agents/<agent>`, still signed in with the person's login:
//!
//! - Codex: `CODEX_HOME=<home>`, with `auth.json` a symbolic link to the
//!   person's. Codex follows the link when it saves refreshed tokens, so both
//!   homes keep one login. Codex keeping its login in the keyring
//!   (`cli_auth_credentials_store`), a custom `model_provider`, or no
//!   `auth.json` cannot be shared this way: such a session keeps the
//!   person's home.
//! - Claude Code: `CLAUDE_CONFIG_DIR=<home>`, and
//!   `CLAUDE_SECURESTORAGE_CONFIG_DIR` naming the person's own config
//!   directory (empty for the default `~/.claude`), so its credentials are
//!   read from, and refreshed in, the person's keychain item.
//!
//! - Pi, OMP, OpenCode and Antigravity: only their sessions move (Pi's
//!   `PI_CODING_AGENT_SESSION_DIR`, OMP's `--session-dir`, OpenCode's
//!   `OPENCODE_DB`, Antigravity's `--app-data-dir`), so the login and
//!   settings stay the person's as they are. Antigravity's app data
//!   directory holds its conversations, not its login or settings; it must
//!   be relative to its Gemini directory (`~/.gemini`), so agl passes the
//!   path from there to the agent's home.
//!
//! - Kiro CLI: `KIRO_HOME=<home>` (its `~/.kiro`), with everything in the
//!   person's `~/.kiro` but its sessions and logs linked in (settings,
//!   agents, steering, skills, powers, per-workspace permissions, …), so it
//!   runs as the person set it up. Its login is not in that directory.
//!
//! Other agents keep the person's home: moving theirs moves their login with
//! it, and several of them replace a symbolically linked credentials file
//! with a copy when they refresh it.
//!
//! The person's settings, instructions, skills and plugins stay in their own
//! home. A session decides at creation and keeps its home for life, so its
//! native conversation can always be resumed; one created to `resume` a
//! conversation from the person's home stays there.

use std::path::{Path, PathBuf};

use super::{AgentDef, Protocol};
use crate::shellenv;

/// Where an agent keeps its conversations apart from the person's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Isolation {
    /// Its whole data home (`CODEX_HOME`, `CLAUDE_CONFIG_DIR`), the login
    /// shared back.
    Home,
    /// Only its sessions; its login and settings stay the person's: Pi
    /// (`PI_CODING_AGENT_SESSION_DIR`), OMP (`--session-dir`), OpenCode
    /// (`OPENCODE_DB`, its session database), Antigravity
    /// (`--app-data-dir`), Kiro (`KIRO_HOME`, the rest linked back).
    Sessions,
}

fn isolation(def: &AgentDef) -> Option<Isolation> {
    match (def.protocol, def.id.as_str()) {
        (Protocol::Codex | Protocol::Claude, _) => Some(Isolation::Home),
        (Protocol::Pi, "pi") | (Protocol::Acp, "omp" | "opencode" | "antigravity" | "kiro") => Some(Isolation::Sessions),
        _ => None,
    }
}

/// The home a new session of `def` should get, or None to use the person's
/// own (other agents, or a login that cannot be shared).
pub fn home_for(def: &AgentDef) -> Option<PathBuf> {
    let base = crate::config::home().join("agents");
    match (isolation(def)?, def.protocol) {
        (Isolation::Home, Protocol::Codex) => codex_shareable(&codex_source()).then(|| base.join("codex")),
        (Isolation::Home, _) => Some(base.join("claude")),
        (Isolation::Sessions, _) => Some(base.join(&def.id)),
    }
}

/// Where a model-list probe of `def` keeps its throwaway session: agents
/// that can only close one, not delete it, would otherwise leave an empty
/// conversation in the person's history each time a hub refreshes models.
pub fn probe_home(def: &AgentDef) -> Option<PathBuf> {
    (isolation(def) == Some(Isolation::Sessions) && def.id == "antigravity").then(|| crate::config::home().join("agents").join("antigravity-probe"))
}

/// The command-line arguments that put an agent in `home`.
pub fn args(def: &AgentDef, home: &Path) -> Vec<String> {
    match (isolation(def), def.id.as_str()) {
        (Some(Isolation::Sessions), "omp") => vec!["--session-dir".into(), home.join("sessions").display().to_string()],
        (Some(Isolation::Sessions), "antigravity") => {
            if let Err(e) = std::fs::create_dir_all(home) {
                eprintln!("agent home {}: {e}", home.display());
            }
            let gemini = dirs::home_dir().unwrap_or_default().join(".gemini");
            match relative_to(&gemini, home) {
                Some(rel) => vec!["--app-data-dir".into(), rel.display().to_string()],
                None => Vec::new(),
            }
        }
        _ => Vec::new(),
    }
}

/// `to` as a path relative to `from`, through `..` where needed; both are
/// resolved first (an existing prefix of each), so a symbolic link on either
/// side cannot send `..` elsewhere. None when they share no root.
fn relative_to(from: &Path, to: &Path) -> Option<PathBuf> {
    let (from, to) = (resolved(from), resolved(to));
    let (a, b): (Vec<_>, Vec<_>) = (from.components().collect(), to.components().collect());
    let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    if common == 0 {
        return None;
    }
    let mut out: PathBuf = a[common..].iter().map(|_| std::path::Component::ParentDir).collect();
    out.extend(&b[common..]);
    Some(if out.as_os_str().is_empty() { PathBuf::from(".") } else { out })
}

/// `p` with its longest existing prefix resolved through symbolic links.
fn resolved(p: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut cur = p.to_path_buf();
    loop {
        if let Ok(real) = std::fs::canonicalize(&cur) {
            return rest.iter().rev().fold(real, |acc, c| acc.join(c));
        }
        match (cur.file_name().map(|n| n.to_os_string()), cur.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                cur = parent.to_path_buf();
            }
            _ => return p.to_path_buf(),
        }
    }
}

/// The variables that put an agent in `home` (prepared first: the directory,
/// and Codex's link to the person's login).
pub fn env(def: &AgentDef, home: &Path) -> Vec<(String, String)> {
    if let Err(e) = std::fs::create_dir_all(home) {
        eprintln!("agent home {}: {e}", home.display());
    }
    if isolation(def) == Some(Isolation::Sessions) {
        return match def.id.as_str() {
            "pi" => vec![("PI_CODING_AGENT_SESSION_DIR".into(), home.join("sessions").display().to_string())],
            "opencode" => vec![("OPENCODE_DB".into(), home.join("opencode.db").display().to_string())],
            "kiro" => {
                if let Err(e) = link_kiro_config(&kiro_source(), home) {
                    eprintln!("agent home {}: {e}", home.display());
                }
                vec![("KIRO_HOME".into(), home.display().to_string())]
            }
            _ => Vec::new(),
        };
    }
    match def.protocol {
        Protocol::Codex => {
            if let Err(e) = link_codex_login(&codex_source(), home) {
                eprintln!("agent home {}: {e}", home.display());
            }
            vec![("CODEX_HOME".into(), home.display().to_string())]
        }
        Protocol::Claude => {
            // The person's own config directory names their keychain item;
            // empty means the default one. A value they set themselves wins.
            let store = shellenv::var("CLAUDE_SECURESTORAGE_CONFIG_DIR")
                .or_else(|| shellenv::var("CLAUDE_CONFIG_DIR"))
                .unwrap_or_default();
            vec![
                ("CLAUDE_CONFIG_DIR".into(), home.display().to_string()),
                ("CLAUDE_SECURESTORAGE_CONFIG_DIR".into(), store),
            ]
        }
        _ => Vec::new(),
    }
}

/// The person's Codex home.
fn codex_source() -> PathBuf {
    shellenv::var("CODEX_HOME").map(PathBuf::from).unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".codex"))
}

/// Whether Codex's login in `source` can be shared through `auth.json`.
fn codex_shareable(source: &Path) -> bool {
    if !source.join("auth.json").is_file() {
        return false;
    }
    let config = std::fs::read_to_string(source.join("config.toml")).unwrap_or_default();
    let store = top_level(&config, "cli_auth_credentials_store");
    let provider = top_level(&config, "model_provider");
    matches!(store.as_deref(), None | Some("file")) && matches!(provider.as_deref(), None | Some("openai"))
}

/// A top-level string setting of a `config.toml` (before the first table).
fn top_level(config: &str, key: &str) -> Option<String> {
    for line in config.lines().map(str::trim) {
        if line.starts_with('[') {
            break;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        if k.trim() == key {
            let v = v.split('#').next().unwrap_or("").trim();
            return Some(v.trim_matches(|c| c == '"' || c == '\'').to_string());
        }
    }
    None
}

/// The person's Kiro home.
fn kiro_source() -> PathBuf {
    shellenv::var("KIRO_HOME").map(PathBuf::from).unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".kiro"))
}

/// What Kiro keeps per conversation in its home; the rest is the person's
/// setup.
const KIRO_SESSION_ENTRIES: &[&str] = &["sessions", "session-index", "session-leases", "session-relationships", "logs"];

/// Links each entry of the person's Kiro home but its sessions and logs into
/// `home`, leaving alone what is already there.
fn link_kiro_config(source: &Path, home: &Path) -> std::io::Result<()> {
    let Ok(entries) = std::fs::read_dir(source) else { return Ok(()) };
    for e in entries.flatten() {
        let name = e.file_name();
        if KIRO_SESSION_ENTRIES.iter().any(|s| name == *s) {
            continue;
        }
        let link = home.join(&name);
        if std::fs::symlink_metadata(&link).is_ok() {
            continue;
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(e.path(), &link)?;
    }
    Ok(())
}

/// Points `<home>/auth.json` at the person's, replacing a file Codex may
/// have left there (the person's login is the one to keep).
fn link_codex_login(source: &Path, home: &Path) -> std::io::Result<()> {
    let target = source.join("auth.json");
    let link = home.join("auth.json");
    if std::fs::read_link(&link).ok().as_deref() == Some(target.as_path()) {
        return Ok(());
    }
    match std::fs::symlink_metadata(&link) {
        Ok(_) => std::fs::remove_file(&link)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&target, &link)
    }
    #[cfg(not(unix))]
    {
        // Windows needs a privilege for symbolic links: share by copy, which
        // a token refresh on either side outdates.
        std::fs::copy(&target, &link).map(|_| ())
    }
}

#[cfg(test)]
mod tests {

    fn agent(id: &str) -> AgentDef {
        super::super::registry(&crate::config::Config::default()).into_iter().find(|d| d.id == id).unwrap()
    }

    #[test]
    fn pi_omp_opencode_and_antigravity_keep_only_their_sessions_apart() {
        let home = dir("sessions");
        assert_eq!(env(&agent("pi"), &home), vec![("PI_CODING_AGENT_SESSION_DIR".to_string(), home.join("sessions").display().to_string())]);
        assert_eq!(env(&agent("opencode"), &home), vec![("OPENCODE_DB".to_string(), home.join("opencode.db").display().to_string())]);
        assert!(env(&agent("omp"), &home).is_empty());
        assert_eq!(args(&agent("omp"), &home), vec!["--session-dir".to_string(), home.join("sessions").display().to_string()]);
        for id in ["pi", "omp", "opencode", "antigravity", "kiro"] {
            assert!(home_for(&agent(id)).unwrap().ends_with(format!("agents/{id}")), "{id}");
        }
        assert!(env(&agent("antigravity"), &home).is_empty());
        let a = args(&agent("antigravity"), &home.join("antigravity"));
        assert_eq!(a[0], "--app-data-dir");
        assert!(Path::new(&a[1]).is_relative(), "agy refuses an absolute app data dir");
        // `..` taken lexically from the resolved Gemini directory, as agy does.
        let mut landed = resolved(&dirs::home_dir().unwrap().join(".gemini"));
        for c in Path::new(&a[1]).components() {
            match c {
                std::path::Component::ParentDir => assert!(landed.pop()),
                c => landed.push(c),
            }
        }
        assert_eq!(landed, resolved(&home.join("antigravity")));
        assert!(probe_home(&agent("antigravity")).unwrap().ends_with("agents/antigravity-probe"));
        assert_eq!(probe_home(&agent("omp")), None);
    }

    #[cfg(unix)]
    #[test]
    fn kiro_keeps_the_persons_setup_and_only_its_sessions_apart() {
        let (source, home) = (dir("kiro-src"), dir("kiro-home"));
        for d in ["settings", "agents", "sessions", "logs", "session-index"] {
            std::fs::create_dir_all(source.join(d)).unwrap();
        }
        std::fs::create_dir_all(home.join("steering")).unwrap();
        std::fs::create_dir_all(source.join("steering")).unwrap();
        link_kiro_config(&source, &home).unwrap();
        link_kiro_config(&source, &home).unwrap(); // again: nothing to do
        assert_eq!(std::fs::read_link(home.join("settings")).unwrap(), source.join("settings"));
        assert_eq!(std::fs::read_link(home.join("agents")).unwrap(), source.join("agents"));
        for d in ["sessions", "logs", "session-index"] {
            assert!(!home.join(d).exists(), "{d}");
        }
        assert!(std::fs::read_link(home.join("steering")).is_err(), "what is there stays");
        let vars = env(&agent("kiro"), &home);
        assert_eq!(vars, vec![("KIRO_HOME".to_string(), home.display().to_string())]);
        assert!(args(&agent("kiro"), &home).is_empty());
        std::fs::remove_dir_all(&source).unwrap();
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn relative_paths_climb_out_of_the_gemini_dir() {
        let root = dir("rel");
        let (gemini, target) = (root.join("u/.gemini"), root.join("u/.agl/agents/antigravity"));
        assert_eq!(relative_to(&gemini, &target).unwrap(), PathBuf::from("../.agl/agents/antigravity"));
        assert_eq!(relative_to(&gemini, &gemini.join("x")).unwrap(), PathBuf::from("x"));
        assert_eq!(relative_to(&gemini, &gemini).unwrap(), PathBuf::from("."));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn agents_whose_login_would_move_keep_the_persons_home() {
        for id in ["gemini", "kimi", "grok", "hermes", "openclaw", "droid", "cursor"] {
            assert_eq!(home_for(&agent(id)), None, "{id}");
            assert!(args(&agent(id), Path::new("/x")).is_empty(), "{id}");
        }
    }

    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("agl-isolation-{name}-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn codex_login_is_shared_only_from_a_file() {
        let src = dir("src");
        assert!(!codex_shareable(&src), "no auth.json");
        std::fs::write(src.join("auth.json"), "{}").unwrap();
        assert!(codex_shareable(&src));
        std::fs::write(src.join("config.toml"), "model = \"x\"\ncli_auth_credentials_store = \"file\" # ok\n[plugins.\"a\"]\nmodel_provider = \"z\"\n").unwrap();
        assert!(codex_shareable(&src), "file store; a table's keys are not top level");
        std::fs::write(src.join("config.toml"), "cli_auth_credentials_store = \"keyring\"\n").unwrap();
        assert!(!codex_shareable(&src));
        std::fs::write(src.join("config.toml"), "cli_auth_credentials_store = 'auto'\n").unwrap();
        assert!(!codex_shareable(&src));
        std::fs::write(src.join("config.toml"), "model_provider = \"ollama\"\n").unwrap();
        assert!(!codex_shareable(&src));
        std::fs::write(src.join("config.toml"), "model_provider = \"openai\"\n").unwrap();
        assert!(codex_shareable(&src));
        std::fs::remove_dir_all(&src).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn codex_home_links_the_persons_login() {
        let src = dir("src");
        let home = dir("home");
        std::fs::write(src.join("auth.json"), "{\"a\":1}").unwrap();
        // A stale file Codex left behind is replaced by the link.
        std::fs::write(home.join("auth.json"), "stale").unwrap();
        link_codex_login(&src, &home).unwrap();
        assert_eq!(std::fs::read_link(home.join("auth.json")).unwrap(), src.join("auth.json"));
        assert_eq!(std::fs::read_to_string(home.join("auth.json")).unwrap(), "{\"a\":1}");
        // Idempotent.
        link_codex_login(&src, &home).unwrap();
        assert_eq!(std::fs::read_link(home.join("auth.json")).unwrap(), src.join("auth.json"));
        std::fs::remove_dir_all(&src).unwrap();
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn claude_keeps_the_persons_keychain_item() {
        let home = dir("claude");
        let vars = env(&agent("claude"), &home);
        assert!(vars.contains(&("CLAUDE_CONFIG_DIR".into(), home.display().to_string())));
        assert!(vars.iter().any(|(k, _)| k == "CLAUDE_SECURESTORAGE_CONFIG_DIR"));
        assert!(env(&agent("gemini"), &home).is_empty());
        assert!(home_for(&agent("gemini")).is_none() && home_for(&agent("cursor")).is_none());
        std::fs::remove_dir_all(&home).unwrap();
    }
}
