//! Autostart: a launchd agent on macOS, a systemd user unit on Linux. The
//! unit captures the current PATH so the daemon finds `claude` / `codex`.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::config;

const LABEL: &str = "cc.agl.daemon";

fn unit_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    if cfg!(target_os = "macos") {
        Some(home.join("Library/LaunchAgents").join(format!("{LABEL}.plist")))
    } else if cfg!(target_os = "linux") {
        Some(home.join(".config/systemd/user/agl.service"))
    } else {
        None
    }
}

pub fn is_installed() -> bool {
    unit_path().map(|p| p.exists()).unwrap_or(false)
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

pub fn install() -> Result<()> {
    let Some(path) = unit_path() else { bail!("autostart is only supported on macOS and Linux; run `agl start` instead") };
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
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key><array><string>{exe}</string><string>daemon</string></array>
  <key>EnvironmentVariables</key><dict><key>PATH</key><string>{path_env}</string></dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
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
    } else {
        let unit = format!(
            "[Unit]\nDescription=AgentLink daemon\nAfter=network-online.target\n\n[Service]\nExecStart={exe} daemon\nEnvironment=PATH={path_env}\nRestart=always\nRestartSec=3\nStandardOutput=append:{log}\nStandardError=append:{log}\n\n[Install]\nWantedBy=default.target\n",
            exe = exe.display(),
            log = log.display(),
        );
        std::fs::write(&path, unit)?;
        let systemctl = |args: &[&str]| Command::new("systemctl").arg("--user").args(args).status().map(|s| s.success()).unwrap_or(false);
        // `restart` also replaces a running instance (a reinstall or upgrade).
        let ok = systemctl(&["daemon-reload"]) && systemctl(&["enable", "agl.service"]) && systemctl(&["restart", "agl.service"]);
        if !ok {
            let _ = std::fs::remove_file(&path);
            bail!("systemctl --user is unavailable; run `agl start` instead");
        }
        // Keep the user service running without an open login session.
        let user = std::env::var("USER").unwrap_or_default();
        let lingering = Command::new("loginctl").args(["enable-linger", &user]).status().map(|s| s.success()).unwrap_or(false);
        if !lingering {
            println!("note: run `sudo loginctl enable-linger {user}` so agl keeps running after you log out");
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
        let _ = Command::new("systemctl").args(["--user", "disable", "--now", "agl.service"]).status();
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
