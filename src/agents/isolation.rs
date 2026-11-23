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
//! The person's settings, instructions, skills and plugins stay in their own
//! home. A session decides at creation and keeps its home for life, so its
//! native conversation can always be resumed; one created to `resume` a
//! conversation from the person's home stays there.

use std::path::{Path, PathBuf};

use super::Protocol;
use crate::shellenv;

/// The home a new session of `protocol` should get, or None to use the
/// person's own (other agents, or a login that cannot be shared).
pub fn home_for(protocol: Protocol) -> Option<PathBuf> {
    let base = crate::config::home().join("agents");
    match protocol {
        Protocol::Codex => codex_shareable(&codex_source()).then(|| base.join("codex")),
        Protocol::Claude => Some(base.join("claude")),
        _ => None,
    }
}

/// The variables that put an agent in `home` (prepared first: the directory,
/// and Codex's link to the person's login).
pub fn env(protocol: Protocol, home: &Path) -> Vec<(String, String)> {
    if let Err(e) = std::fs::create_dir_all(home) {
        eprintln!("agent home {}: {e}", home.display());
    }
    match protocol {
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
        let vars = env(Protocol::Claude, &home);
        assert!(vars.contains(&("CLAUDE_CONFIG_DIR".into(), home.display().to_string())));
        assert!(vars.iter().any(|(k, _)| k == "CLAUDE_SECURESTORAGE_CONFIG_DIR"));
        assert!(env(Protocol::Acp, &home).is_empty());
        assert!(home_for(Protocol::Acp).is_none() && home_for(Protocol::Cursor).is_none());
        std::fs::remove_dir_all(&home).unwrap();
    }
}
