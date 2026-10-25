//! Discovery beyond the PATH: desktop apps that ship an agent, how the found
//! binary was installed and the command that installs or updates it, and an
//! opt-in check for the newest published version.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use crate::protocol::InstallInfo;
use crate::shellenv;

/// Where the newest version of an agent is published.
#[derive(Clone, Copy)]
enum Latest {
    /// The npm registry, for `Meta::package`.
    Npm,
    /// The tag the `releases/latest` page redirects to (no API rate limit).
    GitHub(&'static str),
    /// A URL whose body is the version.
    Text(&'static str),
    /// The version directory named in Cursor's install script (read, never run).
    CursorInstaller,
}

#[derive(Default)]
struct Meta {
    /// macOS desktop apps that ship the agent.
    apps: &'static [&'static str],
    /// npm package of the CLI.
    package: Option<&'static str>,
    guide: Option<&'static str>,
    /// Official install script; it also replaces a standalone install.
    installer: Option<&'static str>,
    /// Subcommand that updates a standalone install in place.
    self_update: Option<&'static str>,
    latest: Option<Latest>,
}

fn meta(id: &str) -> Meta {
    use Latest::*;
    match id {
        "claude" => Meta {
            apps: &["Claude.app"],
            package: Some("@anthropic-ai/claude-code"),
            guide: Some("https://code.claude.com/docs/en/setup"),
            installer: Some("curl -fsSL https://claude.ai/install.sh | bash"),
            latest: Some(Npm),
            ..Meta::default()
        },
        "codex" => Meta {
            apps: &["Codex.app"],
            package: Some("@openai/codex"),
            guide: Some("https://github.com/openai/codex"),
            latest: Some(Npm),
            ..Meta::default()
        },
        "gemini" => Meta {
            package: Some("@google/gemini-cli"),
            guide: Some("https://geminicli.com/docs/get-started/installation/"),
            latest: Some(Npm),
            ..Meta::default()
        },
        "opencode" => Meta {
            apps: &["OpenCode.app"],
            package: Some("opencode-ai"),
            guide: Some("https://opencode.ai/docs/"),
            self_update: Some("upgrade"),
            latest: Some(Npm),
            ..Meta::default()
        },
        "kimi" => Meta {
            apps: &["Kimi.app"],
            guide: Some("https://github.com/MoonshotAI/kimi-cli"),
            installer: Some("curl -fsSL https://code.kimi.com/kimi-code/install.sh | bash"),
            // Kimi Code publishes no version agl can read (PyPI `kimi-cli` is
            // the older CLI).
            self_update: Some("upgrade"),
            ..Meta::default()
        },
        "grok" => Meta {
            apps: &["Grok Bot.app"],
            guide: Some("https://github.com/xai-org/grok-build"),
            installer: Some("curl -fsSL https://x.ai/cli/install.sh | bash"),
            latest: Some(Text("https://x.ai/cli/stable")),
            ..Meta::default()
        },
        "droid" => Meta {
            guide: Some("https://docs.factory.ai/cli/getting-started/quickstart"),
            installer: Some("curl -fsSL https://app.factory.ai/cli | sh"),
            ..Meta::default()
        },
        "openclaw" => Meta {
            apps: &["OpenClaw.app"],
            package: Some("openclaw"),
            guide: Some("https://docs.openclaw.ai/install"),
            self_update: Some("update"),
            latest: Some(Npm),
            ..Meta::default()
        },
        "hermes" => Meta {
            apps: &["Hermes.app"],
            guide: Some("https://hermes-agent.nousresearch.com/docs/getting-started/quickstart"),
            installer: Some("curl -fsSL https://hermes-agent.nousresearch.com/install.sh | bash"),
            self_update: Some("update"),
            latest: Some(GitHub("NousResearch/hermes-agent")),
            ..Meta::default()
        },
        "omp" => Meta {
            package: Some("@oh-my-pi/pi-coding-agent"),
            installer: Some("curl -fsSL https://omp.sh/install | sh"),
            latest: Some(Npm),
            ..Meta::default()
        },
        "cursor" => Meta {
            apps: &["Cursor.app"],
            guide: Some("https://docs.cursor.com/en/cli/installation"),
            installer: Some("curl -fsSL https://cursor.com/install | bash"),
            self_update: Some("update"),
            latest: Some(CursorInstaller),
            ..Meta::default()
        },
        "pi" => Meta {
            package: Some("@earendil-works/pi-coding-agent"),
            latest: Some(Npm),
            ..Meta::default()
        },
        _ => Meta::default(),
    }
}

/// The first installed desktop app that ships this agent (macOS).
pub fn desktop_app(id: &str) -> Option<PathBuf> {
    desktop_app_in(id, &app_roots())
}

fn app_roots() -> Vec<PathBuf> {
    if !cfg!(target_os = "macos") {
        return Vec::new();
    }
    let home = dirs::home_dir().unwrap_or_default();
    vec![PathBuf::from("/Applications"), PathBuf::from("/System/Applications"), home.join("Applications")]
}

fn desktop_app_in(id: &str, roots: &[PathBuf]) -> Option<PathBuf> {
    let apps = meta(id).apps;
    roots.iter().flat_map(|r| apps.iter().map(move |a| r.join(a))).find(|p| p.exists())
}

/// How to install a missing agent, or how the found binary was installed and
/// how to update it. `None` when agl knows nothing about the agent.
pub fn install_info(id: &str, path: Option<&Path>) -> Option<InstallInfo> {
    let resolved = path.map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()));
    let info = plan(id, path, resolved.as_deref());
    (info != InstallInfo::default()).then_some(info)
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// `path` is the binary as found on the PATH, `resolved` its real location.
fn plan(id: &str, path: Option<&Path>, resolved: Option<&Path>) -> InstallInfo {
    let m = meta(id);
    let mut info = InstallInfo { guide: m.guide.map(Into::into), ..InstallInfo::default() };
    let (Some(path), Some(resolved)) = (path, resolved) else {
        info.package = m.package.map(Into::into);
        info.install_command = m.installer.map(Into::into).or_else(|| m.package.map(|p| format!("npm install -g {p}@latest")));
        return info;
    };
    let r = resolved.to_string_lossy();
    let source = |info: &mut InstallInfo, s: &str, cmd: Option<String>| {
        info.source = Some(s.into());
        info.update_command = cmd;
    };
    // The Codex engine inside a desktop app is updated with the app.
    if r.contains(".app/Contents/") {
        source(&mut info, "app-bundle", None);
        return info;
    }
    for cellar in ["/Cellar/", "/Caskroom/"] {
        if let Some(at) = r.find(cellar) {
            let formula = r[at + cellar.len()..].split('/').next().unwrap_or_default();
            let brew = format!("{}/bin/brew", &r[..at]);
            let cask = if cellar == "/Caskroom/" { " --cask" } else { "" };
            source(&mut info, "brew", Some(format!("{} upgrade{cask} {formula}", quote(&brew))));
            return info;
        }
    }
    if let Some(pkg) = m.package.filter(|p| r.contains(&format!("/node_modules/{p}/"))) {
        info.package = Some(pkg.into());
        if r.contains("/.bun/") {
            source(&mut info, "bun", Some(format!("bun install -g {pkg}@latest")));
        } else {
            // A custom global prefix often holds the CLI links but no npm:
            // target that prefix rather than npm's current default.
            let prefix = path.parent().and_then(Path::parent).unwrap_or(path);
            info.prefix = Some(prefix.display().to_string());
            let cmd = format!("npm install --global --prefix {} {pkg}@latest", quote(&prefix.to_string_lossy()));
            source(&mut info, "npm", Some(cmd));
        }
        return info;
    }
    let bin = quote(&path.to_string_lossy());
    if id == "claude" && r.contains("/.local/share/claude/versions/") {
        source(&mut info, "native", Some(format!("{bin} update")));
    } else if let Some(sub) = m.self_update {
        source(&mut info, "native", Some(format!("{bin} {sub}")));
    } else {
        source(&mut info, "unknown", m.installer.map(Into::into));
    }
    info
}

/// The newest published version of an agent, if agl knows where to look.
pub async fn latest_version(id: &str) -> Option<String> {
    let m = meta(id);
    let url = match m.latest? {
        Latest::Npm => format!("https://registry.npmjs.org/{}/latest", m.package?.replace('/', "%2F")),
        Latest::GitHub(repo) => {
            let to = fetch(&["-o", "/dev/null", "-w", "%{redirect_url}"], &format!("https://github.com/{repo}/releases/latest")).await?;
            return to.rsplit_once("/releases/tag/").map(|(_, t)| t.trim().to_string()).filter(|t| !t.is_empty() && t.len() <= 80);
        }
        Latest::Text(url) => url.into(),
        Latest::CursorInstaller => "https://cursor.com/install".into(),
    };
    let body = fetch(&[], &url).await?;
    let v = match m.latest? {
        Latest::Text(_) => Some(body.trim().to_string()),
        Latest::CursorInstaller => cursor_installer_version(&body),
        _ => serde_json::from_str::<Value>(&body).ok()?["version"].as_str().map(Into::into),
    };
    v.filter(|s| !s.is_empty() && s.len() <= 80 && !s.contains(char::is_whitespace))
}

/// curl honours the proxy variables imported from the login shell.
async fn fetch(extra: &[&str], url: &str) -> Option<String> {
    let curl = shellenv::which("curl")?;
    let ua = concat!("agl/", env!("CARGO_PKG_VERSION"));
    let mut args = vec!["-fsS", "--max-time", "5", "-A", ua];
    args.extend_from_slice(extra);
    if extra.is_empty() {
        args.push("-L");
    }
    args.push(url);
    super::run_capture(&curl, &args, Duration::from_secs(7)).await
}

fn cursor_installer_version(script: &str) -> Option<String> {
    let at = script.find("FINAL_DIR=\"")? + "FINAL_DIR=\"".len();
    let dir = &script[at..at + script[at..].find(['"', '\n'])?];
    let v = &dir[dir.find("/versions/")? + "/versions/".len()..];
    Some(v.split('/').next()?.to_string()).filter(|v| !v.is_empty())
}

/// `available`, `current` or `unknown` for an installed version against the
/// newest one. Unparseable versions are never reported as current.
pub fn update_status(id: &str, current: Option<&str>, latest: Option<&str>) -> &'static str {
    let (Some(a), Some(b)) = (current.and_then(parse_version), latest.and_then(parse_version)) else { return "unknown" };
    let ord = compare(&a, &b);
    // Cursor versions are a date plus a commit hash: builds of the same day
    // have no order.
    if id == "cursor" && ord != Ordering::Equal && a.0 == b.0 {
        return "unknown";
    }
    if ord == Ordering::Less {
        "available"
    } else {
        "current"
    }
}

/// Numeric components (2 to 4) and the SemVer prerelease of the first version
/// in a string such as `codex-cli 0.164.0` or `v0.21.4`. Build metadata is
/// ignored.
fn parse_version(s: &str) -> Option<(Vec<u64>, Option<String>)> {
    let b = s.as_bytes();
    let word = |c: u8| c.is_ascii_alphanumeric();
    for start in 0..b.len() {
        if start > 0 && word(b[start - 1]) {
            continue;
        }
        let mut i = start + usize::from(b[start] == b'v');
        let mut nums = Vec::new();
        while nums.len() < 4 {
            let d = b[i.min(b.len())..].iter().take_while(|c| c.is_ascii_digit()).count();
            if d == 0 {
                break;
            }
            nums.push(s[i..i + d].parse().ok()?);
            i += d;
            if nums.len() < 4 && b.get(i) == Some(&b'.') && b.get(i + 1).is_some_and(u8::is_ascii_digit) {
                i += 1;
            } else {
                break;
            }
        }
        if nums.len() < 2 {
            continue;
        }
        let pre = (b.get(i) == Some(&b'-'))
            .then(|| s[i + 1..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-').collect::<String>())
            .filter(|p| !p.is_empty());
        return Some((nums, pre));
    }
    None
}

fn compare(a: &(Vec<u64>, Option<String>), b: &(Vec<u64>, Option<String>)) -> Ordering {
    for i in 0..a.0.len().max(b.0.len()) {
        let o = a.0.get(i).unwrap_or(&0).cmp(b.0.get(i).unwrap_or(&0));
        if o != Ordering::Equal {
            return o;
        }
    }
    match (&a.1, &b.1) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => {
            let (x, y): (Vec<&str>, Vec<&str>) = (x.split('.').collect(), y.split('.').collect());
            for i in 0..x.len().max(y.len()) {
                let o = match (x.get(i), y.get(i)) {
                    (None, None) => Ordering::Equal,
                    (None, Some(_)) => Ordering::Less,
                    (Some(_), None) => Ordering::Greater,
                    (Some(p), Some(q)) => match (p.parse::<u64>(), q.parse::<u64>()) {
                        (Ok(m), Ok(n)) => m.cmp(&n),
                        (Ok(_), Err(_)) => Ordering::Less,
                        (Err(_), Ok(_)) => Ordering::Greater,
                        _ => p.cmp(q),
                    },
                };
                if o != Ordering::Equal {
                    return o;
                }
            }
            Ordering::Equal
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Option<PathBuf> {
        Some(PathBuf::from(s))
    }

    fn plan_at(id: &str, path: &str, resolved: &str) -> InstallInfo {
        plan(id, p(path).as_deref(), p(resolved).as_deref())
    }

    #[test]
    fn npm_install_under_a_custom_prefix() {
        let i = plan_at(
            "codex",
            "/Users/u/code/node/npm_global/bin/codex",
            "/Users/u/code/node/npm_global/lib/node_modules/@openai/codex/bin/codex.js",
        );
        assert_eq!(i.source.as_deref(), Some("npm"));
        assert_eq!(i.prefix.as_deref(), Some("/Users/u/code/node/npm_global"));
        assert_eq!(
            i.update_command.as_deref(),
            Some("npm install --global --prefix '/Users/u/code/node/npm_global' @openai/codex@latest")
        );
    }

    #[test]
    fn brew_native_self_update_and_app_bundle() {
        let brew = plan_at("gemini", "/opt/homebrew/bin/gemini", "/opt/homebrew/Cellar/gemini-cli/0.61.0/bin/gemini");
        assert_eq!(brew.update_command.as_deref(), Some("'/opt/homebrew/bin/brew' upgrade gemini-cli"));
        let claude = plan_at("claude", "/Users/u/.local/bin/claude", "/Users/u/.local/share/claude/versions/2.1.310");
        assert_eq!(claude.update_command.as_deref(), Some("'/Users/u/.local/bin/claude' update"));
        let cursor = plan_at("cursor", "/Users/u/.local/bin/cursor-agent", "/Users/u/.local/share/cursor-agent/versions/x/cursor-agent");
        assert_eq!(cursor.update_command.as_deref(), Some("'/Users/u/.local/bin/cursor-agent' update"));
        let grok = plan_at("grok", "/Users/u/.grok/bin/grok", "/Users/u/.grok/bin/grok");
        assert_eq!((grok.source.as_deref(), grok.update_command.as_deref()), (Some("unknown"), Some("curl -fsSL https://x.ai/cli/install.sh | bash")));
        let app = plan_at("codex", "/Applications/Codex.app/Contents/Resources/codex", "/Applications/Codex.app/Contents/Resources/codex");
        assert_eq!((app.source.as_deref(), app.update_command), (Some("app-bundle"), None));
    }

    #[test]
    fn missing_agents_get_an_install_command() {
        assert_eq!(plan("claude", None, None).install_command.as_deref(), Some("curl -fsSL https://claude.ai/install.sh | bash"));
        assert_eq!(plan("openclaw", None, None).install_command.as_deref(), Some("npm install -g openclaw@latest"));
        assert_eq!(install_info("goose", None), None);
    }

    #[test]
    fn desktop_apps_are_found_in_the_given_roots() {
        let dir = std::env::temp_dir().join(format!("agl-apps-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("Grok Bot.app")).unwrap();
        let roots = [dir.clone()];
        assert_eq!(desktop_app_in("grok", &roots), Some(dir.join("Grok Bot.app")));
        assert_eq!(desktop_app_in("cursor", &roots), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn version_comparison() {
        assert_eq!(update_status("codex", Some("codex-cli 0.163.0"), Some("0.164.0")), "available");
        assert_eq!(update_status("claude", Some("2.1.310 (Claude Code)"), Some("2.1.310")), "current");
        assert_eq!(update_status("hermes", Some("Hermes v0.21.4"), Some("v0.22.0")), "available");
        assert_eq!(update_status("x", Some("1.0.0-beta.2"), Some("1.0.0-beta.11")), "available");
        assert_eq!(update_status("x", Some("1.0.0"), Some("1.0.0-rc.1")), "current");
        assert_eq!(update_status("x", Some("dev build"), Some("1.0.0")), "unknown");
        assert_eq!(update_status("cursor", Some("2026.10.18-ea085cb"), Some("2026.10.18-1f2e3d4")), "unknown");
        assert_eq!(update_status("cursor", Some("2026.10.18-ea085cb"), Some("2026.10.20-1f2e3d4")), "available");
    }

    #[test]
    fn cursor_installer_is_read_not_run() {
        let s = "set -e\nFINAL_DIR=\"$HOME/.local/share/cursor-agent/versions/2026.10.20-1f2e3d4\"\n";
        assert_eq!(cursor_installer_version(s).as_deref(), Some("2026.10.20-1f2e3d4"));
    }
}
