//! Autostart: a launchd agent on macOS, a systemd user unit on Linux. The
//! unit captures the current PATH so the daemon finds `claude` / `codex`.
//! Its label and unit name come from the brand (`cc.agl.daemon`,
//! `agl.service` for agl itself).

use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::{brand, config};

fn label() -> &'static str {
    &brand::get().service_label
}

fn unit() -> &'static str {
    &brand::get().systemd_unit
}

fn unit_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    if cfg!(target_os = "macos") {
        Some(home.join("Library/LaunchAgents").join(format!("{}.plist", label())))
    } else if cfg!(target_os = "linux") {
        Some(home.join(".config/systemd/user").join(unit()))
    } else {
        None
    }
}

pub fn is_installed() -> bool {
    unit_path().map(|p| p.exists()).unwrap_or(false)
}

/// Whether this process is the autostart service's own daemon, which the
/// service restarts when it exits. The unit being installed is not enough:
/// a daemon started with `agl start` (or by an installer's fallback) runs
/// beside an idle unit, and nothing would bring it back.
pub fn runs_this_process() -> bool {
    is_installed() && started_by_service(label(), unit(), std::env::var("XPC_SERVICE_NAME").ok().as_deref(), &cgroup())
}

fn cgroup() -> String {
    if cfg!(target_os = "linux") {
        std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default()
    } else {
        String::new()
    }
}

/// The shell command that makes the service start its daemon right away
/// (macOS: launchd defers KeepAlive respawns), or None where the service
/// restarts it promptly by itself (systemd).
pub fn kickstart_command() -> Option<String> {
    if cfg!(target_os = "macos") {
        Some(format!("launchctl kickstart gui/{}/{}", uid(), label()))
    } else {
        None
    }
}

/// launchd names the job in XPC_SERVICE_NAME; systemd puts the unit's
/// processes in its own cgroup.
fn started_by_service(label: &str, unit: &str, xpc_service: Option<&str>, cgroup: &str) -> bool {
    if cfg!(target_os = "macos") {
        xpc_service == Some(label)
    } else if cfg!(target_os = "linux") {
        cgroup.lines().any(|l| l.ends_with(&format!("/{unit}")))
    } else {
        false
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

pub fn install() -> Result<()> {
    let b = brand::get();
    let Some(path) = unit_path() else { bail!("autostart is only supported on macOS and Linux; run `{} start` instead", b.cli_name) };
    let exe = std::env::current_exe()?.canonicalize()?;
    let path_env = std::env::var("PATH").unwrap_or_default();
    let log = config::log_path();
    std::fs::create_dir_all(config::home())?;
    std::fs::create_dir_all(path.parent().unwrap())?;

    if cfg!(target_os = "macos") {
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key><array><string>{exe}</string><string>daemon</string></array>
  <key>EnvironmentVariables</key><dict><key>PATH</key><string>{path_env}</string></dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
            label = xml_escape(label()),
            exe = xml_escape(&exe.display().to_string()),
            path_env = xml_escape(&path_env),
            log = xml_escape(&log.display().to_string()),
        );
        std::fs::write(&path, plist)?;
        let domain = format!("gui/{}", uid());
        let _ = Command::new("launchctl").args(["bootout", &domain, &path.display().to_string()]).output();
        let out = Command::new("launchctl")
            .args(["bootstrap", &domain, &path.display().to_string()])
            .output()
            .context("launchctl")?;
        if !out.status.success() {
            bail!("launchctl bootstrap failed: {}", String::from_utf8_lossy(&out.stderr));
        }
        // RunAtLoad alone may leave the job pending ("pended nondemand spawn
        // = speculative") and the daemon down: start it now.
        let _ = Command::new("launchctl").args(["kickstart", &format!("{domain}/{}", label())]).output();
    } else {
        let unit_file = format!(
            "[Unit]\nDescription={name} daemon\nAfter=network-online.target\n\n[Service]\nExecStart={exe} daemon\nEnvironment=PATH={path_env}\nRestart=always\nRestartSec=3\nStandardOutput=append:{log}\nStandardError=append:{log}\n\n[Install]\nWantedBy=default.target\n",
            name = if b.display_name == "agl" { "AgentLink" } else { b.display_name.as_str() },
            exe = exe.display(),
            log = log.display(),
        );
        std::fs::write(&path, unit_file)?;
        let systemctl = |args: &[&str]| Command::new("systemctl").arg("--user").args(args).status().map(|s| s.success()).unwrap_or(false);
        // `restart` also replaces a running instance (a reinstall or upgrade).
        let ok = systemctl(&["daemon-reload"]) && systemctl(&["enable", unit()]) && systemctl(&["restart", unit()]);
        if !ok {
            let _ = std::fs::remove_file(&path);
            bail!("systemctl --user is unavailable; run `{} start` instead", b.cli_name);
        }
        // Keep the user service running without an open login session.
        let user = std::env::var("USER").unwrap_or_default();
        let lingering = Command::new("loginctl").args(["enable-linger", &user]).status().map(|s| s.success()).unwrap_or(false);
        if !lingering {
            println!("note: run `sudo loginctl enable-linger {user}` so {} keeps running after you log out", b.display_name);
        }
    }
    println!("autostart installed ({})", path.display());
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let Some(path) = unit_path() else { return Ok(()) };
    if !path.exists() {
        println!("autostart not installed");
        return Ok(());
    }
    if cfg!(target_os = "macos") {
        let _ = Command::new("launchctl").args(["bootout", &format!("gui/{}", uid()), &path.display().to_string()]).output();
    } else {
        let _ = Command::new("systemctl").args(["--user", "disable", "--now", unit()]).status();
    }
    std::fs::remove_file(&path)?;
    println!("autostart removed");
    Ok(())
}

fn uid() -> String {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "501".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_daemon_is_recognized() {
        if cfg!(target_os = "macos") {
            assert!(started_by_service(label(), unit(), Some("cc.agl.daemon"), ""));
            // `agl start` from a shell: XPC_SERVICE_NAME is the terminal's or unset.
            assert!(!started_by_service(label(), unit(), Some("application.com.apple.Terminal.123"), ""));
            assert!(!started_by_service(label(), unit(), None, ""));
            // A branded build answers to its own label only.
            assert!(started_by_service("com.example.link", "example-link.service", Some("com.example.link"), ""));
            assert!(!started_by_service("com.example.link", "example-link.service", Some("cc.agl.daemon"), ""));
        } else if cfg!(target_os = "linux") {
            assert!(started_by_service(label(), unit(), None, "0::/user.slice/user-1000.slice/user@1000.service/app.slice/agl.service\n"));
            assert!(!started_by_service(label(), unit(), None, "0::/user.slice/user-1000.slice/session-3.scope\n"));
            assert!(started_by_service("x", "example-link.service", None, "0::/user.slice/app.slice/example-link.service\n"));
            assert!(!started_by_service("x", "example-link.service", None, "0::/user.slice/app.slice/agl.service\n"));
        }
    }

    #[test]
    fn default_label_and_unit_are_agl() {
        assert_eq!((label(), unit()), ("cc.agl.daemon", "agl.service"));
    }
}
