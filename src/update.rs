//! Updating agl in place: the newest release, checked against its published
//! sha256 and swapped in for the running binary, then the daemon restarted on
//! it. `agl update` runs it from the command line; a hub runs it with
//! `daemon.update` (full-access links only, like `exec`).

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{client, service};

/// GitHub releases: `<base>/latest` redirects to `<base>/tag/v<version>`, and
/// `<base>/download/v<version>/agl-<os>-<arch>` is the binary.
const RELEASES: &str = "https://github.com/thinkany-ai/agl/releases";

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub current: String,
    pub latest: String,
    pub update_available: bool,
}

/// `AGL_RELEASES_BASE` points at a mirror laid out like GitHub releases.
fn base() -> String {
    std::env::var("AGL_RELEASES_BASE").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| RELEASES.into()).trim_end_matches('/').to_string()
}

fn http(redirects: bool) -> Result<reqwest::Client> {
    let policy = if redirects { reqwest::redirect::Policy::limited(10) } else { reqwest::redirect::Policy::none() };
    Ok(reqwest::Client::builder()
        .user_agent(format!("agl/{VERSION}"))
        .redirect(policy)
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(300))
        .build()?)
}

/// The newest released version.
pub async fn latest() -> Result<String> {
    let r = http(false)?.get(format!("{}/latest", base())).send().await.context("check for updates")?;
    let location = r
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| anyhow!("no latest release (HTTP {})", r.status()))?;
    let tag = location.trim_end_matches('/').rsplit('/').next().unwrap_or_default();
    let version = tag.trim_start_matches('v');
    if parse(version).is_empty() {
        bail!("unexpected release tag `{tag}`");
    }
    Ok(version.to_string())
}

pub async fn check() -> Result<Check> {
    let latest = latest().await?;
    Ok(Check { update_available: newer(&latest, VERSION), current: VERSION.into(), latest })
}

fn parse(v: &str) -> Vec<u64> {
    v.split('.').map_while(|p| p.parse().ok()).collect()
}

/// Whether version `a` is newer than `b`.
pub fn newer(a: &str, b: &str) -> bool {
    parse(a) > parse(b)
}

/// The release binary's name for this machine, as install.sh names it.
fn target() -> Result<&'static str> {
    Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "darwin-arm64",
        ("macos", "x86_64") => "darwin-x64",
        ("linux", "aarch64") => "linux-arm64",
        ("linux", "x86_64") => "linux-x64",
        (os, arch) => bail!("no agl release for {os}/{arch}"),
    })
}

/// Download `version`, verify it, and put it in place of the running binary
/// (atomically; the running process keeps its old copy). Restart separately.
pub async fn install(version: &str) -> Result<PathBuf> {
    let url = format!("{}/download/v{version}/agl-{}", base(), target()?);
    let client = http(true)?;
    let get = |url: String| {
        let client = client.clone();
        async move {
            let r = client.get(&url).send().await.with_context(|| format!("download {url}"))?;
            if !r.status().is_success() {
                bail!("download {url}: HTTP {}", r.status());
            }
            Ok::<_, anyhow::Error>(r.bytes().await?)
        }
    };
    let binary = get(url.clone()).await?;
    let sum = get(format!("{url}.sha256")).await?;
    let want = String::from_utf8_lossy(&sum).split_whitespace().next().unwrap_or_default().to_lowercase();
    let got: String = Sha256::digest(&binary).iter().map(|b| format!("{b:02x}")).collect();
    if want.is_empty() || want != got {
        bail!("checksum mismatch for {url}");
    }

    let exe = std::env::current_exe()?.canonicalize()?;
    if exe.to_string_lossy().contains(".app/Contents/") {
        bail!("this agl is part of an app ({}); it is updated with the app", exe.display());
    }
    let temp = exe.with_file_name(format!(".agl.update-{}", std::process::id()));
    std::fs::write(&temp, &binary)?;
    let result = (|| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o755))?;
        }
        let out = std::process::Command::new(&temp).arg("--version").output().context("run the downloaded agl")?;
        let printed = String::from_utf8_lossy(&out.stdout);
        if !out.status.success() || !printed.contains(version) {
            bail!("the downloaded agl does not run here ({})", printed.trim());
        }
        std::fs::rename(&temp, &exe).with_context(|| format!("replace {}", exe.display()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result?;
    Ok(exe)
}

/// From the command line, after `install`: restart the daemon on the new
/// binary. The service manager's instance is re-registered; a background one
/// is stopped and started again.
pub async fn restart_daemon() -> Result<()> {
    if service::is_installed() {
        return service::install();
    }
    let Ok(c) = client::Client::connect_local().await else { return Ok(()) };
    let pid = c.call("daemon.status", serde_json::json!({})).await.ok().and_then(|s| s["pid"].as_u64()).unwrap_or(0) as i32;
    let _ = c.call("daemon.shutdown", serde_json::json!({})).await;
    for _ in 0..100 {
        // SAFETY: signal 0 only checks for existence.
        if pid <= 0 || unsafe { libc::kill(pid, 0) } != 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    client::spawn_detached_daemon()
}

/// From inside the daemon (`daemon.update`), once the answer has gone out:
/// a service manager starts the new binary when this one exits; otherwise a
/// detached shell starts it once this one has let go of its port.
pub fn relaunch_after_exit() -> Result<()> {
    if service::is_installed() {
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(crate::config::log_path())?;
    let mut cmd = std::process::Command::new("/bin/sh");
    // The old daemon exits two seconds after answering (daemon.update).
    cmd.arg("-c")
        .arg("sleep 5; exec \"$0\" daemon")
        .arg(exe)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn().context("restart the daemon")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(newer("0.3.2", "0.3.1"));
        assert!(newer("0.10.0", "0.9.9"));
        assert!(!newer("0.3.1", "0.3.1"));
        assert!(!newer("0.3.0", "0.3.1"));
    }
}
