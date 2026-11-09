//! The environment agents should run with.
//!
//! A daemon started by launchd/systemd (or a GUI app) has a bare PATH and
//! none of the API keys the user exported in their shell profile, so agents
//! would not be found or would run unauthenticated. We ask the user's login
//! shell once (interactive first: `-lc` skips `.zshrc`) and merge its PATH
//! and an allowlist of credential variables into what children get.
//!
//! The config's `extra_paths` go in front of all of that. `reload` asks the
//! login shell again, e.g. after an app installed a CLI.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

const MARKER: &str = "__agl_env__";

/// Credential/config variables worth importing from the login shell.
const IMPORT_PREFIXES: &[&str] = &[
    "ANTHROPIC_",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "OPENAI_",
    "CODEX_",
    "GEMINI_",
    "GOOGLE_",
    "KIMI_",
    "MOONSHOT_",
    "XAI_",
    "GROK_",
    "OPENROUTER_",
    "CURSOR_",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "all_proxy",
];

struct ShellEnv {
    path: Vec<PathBuf>,
    vars: HashMap<String, String>,
}

/// The resolved environment; None until first needed or after `reload`.
static ENV: RwLock<Option<Arc<ShellEnv>>> = RwLock::new(None);
/// Serializes resolving, so concurrent first uses ask the shell once.
static RESOLVING: Mutex<()> = Mutex::new(());
/// `extra_paths` from the config, searched before everything else.
static EXTRA_PATHS: RwLock<Vec<PathBuf>> = RwLock::new(Vec::new());

fn login_shell_env() -> Option<HashMap<String, String>> {
    if cfg!(windows) || std::env::var_os("AGL_NO_LOGIN_SHELL").is_some() {
        return None;
    }
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let script = format!("printf '%s\\n' '{MARKER}'; env -0");
    for flag in ["-lic", "-lc"] {
        let Ok(mut child) = Command::new(&shell)
            .args([flag, &script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        // Bounded: a profile that blocks must not hang the daemon.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                _ => {
                    let _ = child.kill();
                    break;
                }
            }
        }
        let Ok(out) = child.wait_with_output() else { continue };
        let text = String::from_utf8_lossy(&out.stdout);
        let Some(idx) = text.find(MARKER) else { continue };
        let body = &text[idx + MARKER.len()..];
        let vars: HashMap<String, String> = body
            .trim_start_matches('\n')
            .split('\0')
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        if vars.contains_key("PATH") {
            return Some(vars);
        }
    }
    None
}

fn extra_dirs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(home) = dirs::home_dir() {
        for d in [
            ".local/bin",
            ".claude/local",
            ".local/share/pnpm",
            ".local/share/mise/shims",
            ".asdf/shims",
            ".volta/bin",
            ".fnm/aliases/default/bin",
            ".npm-global/bin",
            ".bun/bin",
            ".cargo/bin",
            ".opencode/bin",
            ".kimi-code/bin",
            ".grok/bin",
        ] {
            v.push(home.join(d));
        }
        // npm prefix from ~/.npmrc without running npm.
        if let Ok(rc) = std::fs::read_to_string(home.join(".npmrc")) {
            for line in rc.lines() {
                if let Some(p) = line.trim().strip_prefix("prefix=") {
                    let p = p.trim().replace("${HOME}", &home.display().to_string());
                    let p = p.strip_prefix("~/").map(|r| home.join(r)).unwrap_or_else(|| PathBuf::from(&p));
                    v.push(p.join("bin"));
                }
            }
        }
        // nvm installs, newest first.
        if let Ok(rd) = std::fs::read_dir(home.join(".nvm/versions/node")) {
            let mut vers: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path().join("bin"))).collect();
            vers.sort();
            vers.reverse();
            v.extend(vers);
        }
    }
    for d in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"] {
        v.push(PathBuf::from(d));
    }
    v
}

fn get() -> Arc<ShellEnv> {
    if let Some(env) = ENV.read().unwrap().clone() {
        return env;
    }
    let _guard = RESOLVING.lock().unwrap();
    if let Some(env) = ENV.read().unwrap().clone() {
        return env;
    }
    let env = Arc::new(resolve());
    *ENV.write().unwrap() = Some(env.clone());
    env
}

fn resolve() -> ShellEnv {
    let login = login_shell_env();
    let mut path: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        if !p.as_os_str().is_empty() && !path.contains(&p) {
            path.push(p);
        }
    };
    if let Some(l) = &login {
        for p in std::env::split_paths(l.get("PATH").map(String::as_str).unwrap_or("")) {
            push(p);
        }
    }
    if let Some(p) = std::env::var_os("PATH") {
        for p in std::env::split_paths(&p) {
            push(p);
        }
    }
    for p in extra_dirs() {
        push(p);
    }
    let mut vars = HashMap::new();
    if let Some(l) = login {
        for (k, v) in l {
            if IMPORT_PREFIXES.iter().any(|p| k.starts_with(p)) && std::env::var_os(&k).is_none() {
                vars.insert(k, v);
            }
        }
    }
    ShellEnv { path, vars }
}

/// Resolve the login-shell environment now (it takes up to a few seconds).
pub fn warm_up() {
    let _ = get();
}

/// Ask the login shell again (blocking, up to a few seconds), so a CLI
/// installed since the daemon started is found. Callers keep the old
/// environment until the new one is ready.
pub fn reload() {
    let _guard = RESOLVING.lock().unwrap();
    let env = Arc::new(resolve());
    *ENV.write().unwrap() = Some(env);
}

/// Set the config's `extra_paths`: directories searched for agents (and put
/// on their PATH) before anything else. `~/` is expanded; relative and empty
/// entries are ignored.
pub fn set_extra_paths(paths: &[String]) {
    let home = dirs::home_dir();
    let dirs: Vec<PathBuf> = paths
        .iter()
        .map(|p| p.trim())
        .filter_map(|p| match (p.strip_prefix("~/"), &home) {
            (Some(rest), Some(h)) => Some(h.join(rest)),
            _ if p == "~" => home.clone(),
            _ => Some(PathBuf::from(p)),
        })
        .filter(|p| p.is_absolute())
        .collect();
    *EXTRA_PATHS.write().unwrap() = dirs;
}

/// The directories searched, in order: `extra_paths`, the login shell's PATH,
/// this process's PATH, then common install directories.
pub fn path_entries() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for p in EXTRA_PATHS.read().unwrap().iter().chain(get().path.iter()) {
        if !out.contains(p) {
            out.push(p.clone());
        }
    }
    out
}

/// PATH to give children.
pub fn path_var() -> std::ffi::OsString {
    std::env::join_paths(path_entries()).unwrap_or_default()
}

/// Apply the agent environment to a command.
pub fn apply(cmd: &mut tokio::process::Command) {
    cmd.env("PATH", path_var());
    for (k, v) in &get().vars {
        cmd.env(k, v);
    }
}

/// Whether a variable is visible to agents (process env or imported).
pub fn has_var(key: &str) -> bool {
    std::env::var_os(key).is_some() || get().vars.contains_key(key)
}

/// A variable from the process environment, else from the login shell.
pub fn var(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty()).or_else(|| get().vars.get(key).cloned().filter(|v| !v.is_empty()))
}

/// Find an executable on the agent PATH. Skips scripts whose interpreter is
/// missing (broken pipx/uv shims), which would otherwise fail confusingly.
pub fn which(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let p = PathBuf::from(name);
        return runnable(&p).then_some(p);
    }
    path_entries().iter().map(|d| d.join(name)).find(|p| runnable(p))
}

fn runnable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::io::Read;
        use std::os::unix::fs::PermissionsExt;
        let Ok(meta) = p.metadata() else { return false };
        if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
            return false;
        }
        let mut head = [0u8; 256];
        let n = std::fs::File::open(p).and_then(|mut f| f.read(&mut head)).unwrap_or(0);
        if n > 2 && &head[..2] == b"#!" {
            let line = String::from_utf8_lossy(&head[2..n]);
            let line = line.lines().next().unwrap_or("").trim();
            if let Some(interp) = line.split_whitespace().next() {
                if interp.starts_with('/') && !Path::new(interp).exists() {
                    return false;
                }
            }
        }
        true
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn extra_paths_are_searched_first_and_survive_a_reload() {
        use std::os::unix::fs::PermissionsExt;
        std::env::set_var("AGL_NO_LOGIN_SHELL", "1");
        let dir = std::env::temp_dir().join(format!("agl-extra-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("agl-extra-probe");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(which("agl-extra-probe"), None);

        set_extra_paths(&[dir.display().to_string(), "relative/bin".into(), " ".into()]);
        assert_eq!(path_entries()[0], dir);
        assert!(!path_entries().iter().any(|p| p.ends_with("relative/bin")), "relative entries are ignored");
        assert_eq!(which("agl-extra-probe"), Some(bin.clone()));
        assert!(path_var().to_string_lossy().starts_with(&dir.display().to_string()));
        reload();
        assert_eq!(which("agl-extra-probe"), Some(bin), "a reload keeps extra_paths");

        set_extra_paths(&["~/agl-extra-home".into()]);
        assert_eq!(path_entries()[0], dirs::home_dir().unwrap().join("agl-extra-home"));
        set_extra_paths(&[]);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
