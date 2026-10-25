//! Direct access to the machine, beside the agents: read and write a file,
//! run a command. For products that keep their own files next to the
//! agents' work (attachments in, artifacts out) or run their own tools there.
//!
//! Local clients may do all of it. A hub only as far as its link's access
//! allows, since nobody is asked first (see `allowed`).

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::config::Config;
use crate::session::Origin;

/// Largest file `fs.read` / `fs.write` move in one call.
pub const MAX_FILE_BYTES: u64 = 100 * 1024 * 1024;
const EXEC_TIMEOUT_DEFAULT: Duration = Duration::from_secs(10 * 60);
const EXEC_TIMEOUT_MAX: Duration = Duration::from_secs(30 * 60);
/// Per stream; the rest is dropped and `truncated` set.
const EXEC_OUTPUT_MAX: usize = 16 * 1024 * 1024;

/// What a hub may do, by its link's access: any link may read, `edit` and
/// `full` may also write, and only `full` may run a command.
pub(crate) fn allowed(cfg: &Config, origin: &Origin, method: &str) -> Result<()> {
    let Origin::Hub { url, .. } = origin else { return Ok(()) };
    let access = cfg.hub(url).map(|h| h.access.as_str()).unwrap_or("ask");
    let ok = match method {
        "fs.read" => true,
        "fs.write" => matches!(access, "edit" | "full"),
        _ => access == "full",
    };
    if ok {
        Ok(())
    } else {
        bail!("`{method}` needs a link with more access than `{access}` (agl connect … --access)")
    }
}

/// An absolute path, or one under the home directory (`~/…`).
fn resolve(path: &str) -> Result<PathBuf> {
    if path.contains('\0') {
        bail!("invalid path");
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return Ok(dirs::home_dir().context("no home directory")?.join(rest));
    }
    let p = PathBuf::from(path);
    if !p.is_absolute() {
        bail!("path must be absolute or start with ~/: {path}");
    }
    Ok(p)
}

pub async fn dispatch(cfg: &Config, origin: &Origin, method: &str, params: Value) -> Result<Value> {
    allowed(cfg, origin, method)?;
    match method {
        "fs.read" => read(params).await,
        "fs.write" => write(params).await,
        "exec" => exec(params).await,
        other => Err(anyhow!("unknown method `{other}`")),
    }
}

#[derive(Deserialize)]
struct ReadParams {
    path: String,
}

async fn read(params: Value) -> Result<Value> {
    let p: ReadParams = serde_json::from_value(params).context("fs.read params")?;
    let path = resolve(&p.path)?;
    let meta = tokio::fs::metadata(&path).await.with_context(|| format!("read {}", path.display()))?;
    if !meta.is_file() {
        bail!("not a file: {}", path.display());
    }
    if meta.len() > MAX_FILE_BYTES {
        bail!("file is larger than {} MB: {}", MAX_FILE_BYTES / 1024 / 1024, path.display());
    }
    let bytes = tokio::fs::read(&path).await.with_context(|| format!("read {}", path.display()))?;
    Ok(json!({"content_base64": base64::engine::general_purpose::STANDARD.encode(&bytes), "size": bytes.len()}))
}

#[derive(Deserialize)]
struct WriteParams {
    path: String,
    content_base64: String,
}

/// Parent directories are created; the file is replaced atomically.
async fn write(params: Value) -> Result<Value> {
    let p: WriteParams = serde_json::from_value(params).context("fs.write params")?;
    let path = resolve(&p.path)?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(p.content_base64.as_bytes()).context("content_base64 is not base64")?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        bail!("file is larger than {} MB", MAX_FILE_BYTES / 1024 / 1024);
    }
    let parent = path.parent().context("path has no parent directory")?;
    tokio::fs::create_dir_all(parent).await.with_context(|| format!("create {}", parent.display()))?;
    let tmp = parent.join(format!(".{}.agl-{}", path.file_name().and_then(|n| n.to_str()).unwrap_or("file"), uuid::Uuid::new_v4().simple()));
    tokio::fs::write(&tmp, &bytes).await.with_context(|| format!("write {}", path.display()))?;
    if let Err(e) = tokio::fs::rename(&tmp, &path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e).with_context(|| format!("write {}", path.display()));
    }
    Ok(json!({"size": bytes.len()}))
}

#[derive(Deserialize)]
struct ExecParams {
    argv: Vec<String>,
    cwd: Option<String>,
    timeout_ms: Option<u64>,
    #[serde(default)]
    env: std::collections::HashMap<String, String>,
    stdin: Option<String>,
}

/// Run `argv` (no shell) with the agents' environment, in its own process
/// group, killed with everything it started on timeout.
async fn exec(params: Value) -> Result<Value> {
    let p: ExecParams = serde_json::from_value(params).context("exec params")?;
    let Some(program) = p.argv.first() else { bail!("argv is empty") };
    let cwd = match &p.cwd {
        Some(c) => resolve(c)?,
        None => dirs::home_dir().context("no home directory")?,
    };
    let timeout = p.timeout_ms.map(Duration::from_millis).unwrap_or(EXEC_TIMEOUT_DEFAULT).min(EXEC_TIMEOUT_MAX);
    let mut cmd = tokio::process::Command::new(crate::shellenv::which(program).unwrap_or_else(|| PathBuf::from(program)));
    crate::shellenv::apply(&mut cmd);
    cmd.args(&p.argv[1..])
        .envs(&p.env)
        .current_dir(&cwd)
        .stdin(if p.stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd.spawn().with_context(|| format!("run {program}"))?;
    if let (Some(input), Some(mut sink)) = (p.stdin, child.stdin.take()) {
        tokio::spawn(async move {
            let _ = sink.write_all(input.as_bytes()).await;
        });
    }
    let stdout = tokio::spawn(read_capped(child.stdout.take().expect("piped")));
    let stderr = tokio::spawn(read_capped(child.stderr.take().expect("piped")));
    let (status, timed_out) = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(status) => (Some(status?), false),
        Err(_) => {
            crate::agents::kill_tree(&mut child).await;
            (None, true)
        }
    };
    let (out, out_cut) = stdout.await.unwrap_or_default();
    let (err, err_cut) = stderr.await.unwrap_or_default();
    Ok(json!({
        "stdout": String::from_utf8_lossy(&out),
        "stderr": String::from_utf8_lossy(&err),
        "exit_code": status.and_then(|s| s.code()).unwrap_or(-1),
        "timed_out": timed_out,
        "truncated": out_cut || err_cut,
    }))
}

async fn read_capped(mut r: impl tokio::io::AsyncRead + Unpin) -> (Vec<u8>, bool) {
    let mut out = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    let mut cut = false;
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = EXEC_OUTPUT_MAX.saturating_sub(out.len());
                out.extend_from_slice(&buf[..n.min(room)]);
                cut |= n > room;
            }
        }
    }
    (out, cut)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HubLink;

    fn hub_cfg(access: &str) -> (Config, Origin) {
        let mut link = HubLink::new("https://hub.example");
        link.access = access.into();
        let cfg = Config { hubs: vec![link], ..Config::default() };
        (cfg, Origin::Hub { url: "https://hub.example".into(), share_local: true })
    }

    #[test]
    fn hub_access_gates_methods() {
        let (cfg, hub) = hub_cfg("ask");
        assert!(allowed(&cfg, &hub, "fs.read").is_ok());
        assert!(allowed(&cfg, &hub, "fs.write").is_err());
        assert!(allowed(&cfg, &hub, "exec").is_err());
        let (cfg, hub) = hub_cfg("edit");
        assert!(allowed(&cfg, &hub, "fs.write").is_ok());
        assert!(allowed(&cfg, &hub, "exec").is_err());
        let (cfg, hub) = hub_cfg("full");
        assert!(allowed(&cfg, &hub, "exec").is_ok());
        assert!(allowed(&Config::default(), &Origin::Local, "exec").is_ok());
    }

    #[test]
    fn paths_must_be_absolute_or_home() {
        assert!(resolve("relative/file").is_err());
        assert!(resolve("/tmp/x").is_ok());
        assert!(resolve("~/x").unwrap().is_absolute());
    }

    #[tokio::test]
    async fn write_read_and_exec() {
        let dir = std::env::temp_dir().join(format!("agl-machine-{}", uuid::Uuid::new_v4().simple()));
        let file = dir.join("a/b.txt");
        let path = file.to_str().unwrap();
        let data = base64::engine::general_purpose::STANDARD.encode("hello");
        write(json!({"path": path, "content_base64": data})).await.unwrap();
        let r = read(json!({"path": path})).await.unwrap();
        assert_eq!(r["content_base64"], data);
        let r = exec(json!({"argv": ["sh", "-c", "cat b.txt; echo oops >&2; exit 3"], "cwd": dir.join("a").to_str().unwrap()})).await.unwrap();
        assert_eq!((r["stdout"].as_str(), r["stderr"].as_str(), r["exit_code"].as_i64()), (Some("hello"), Some("oops\n"), Some(3)));
        let r = exec(json!({"argv": ["sleep", "5"], "timeout_ms": 200})).await.unwrap();
        assert_eq!(r["timed_out"], true);
        let _ = std::fs::remove_dir_all(dir);
    }
}
