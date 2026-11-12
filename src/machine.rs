//! Direct access to the machine, beside the agents: read and write a file,
//! run a command. For products that keep their own files next to the
//! agents' work (attachments in, artifacts out) or run their own tools there.
//!
//! Local clients may do all of it. A hub only as far as its link's access
//! allows (see `allowed`). Through `tools.call`, a hub that opts in may
//! also hold a call for the person's approval (see approvals.rs).

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

pub mod approvals;
mod executions;
mod tools;
pub use executions::Executions;
pub use tools::names as tool_names;

/// Built-in tools, independent of installed coding agents or model credentials.
#[derive(Default)]
pub struct Runtime {
    pub executions: Executions,
    pub approvals: approvals::Approvals,
    files: tokio::sync::Mutex<()>,
}

impl Runtime {
    pub async fn dispatch(&self, cfg: &Config, origin: &Origin, method: &str, params: Value) -> Result<Value> {
        if method == "tools.list" {
            return Ok(tools::list(cfg, origin, &params));
        }
        if method == "tools.call" {
            return tools::call(self, cfg, origin, params).await;
        }
        if method == "tools.approve" {
            return tools::approve(self, cfg, origin, params).await;
        }
        allowed(cfg, origin, method)?;
        match method {
            "exec" => self.executions.exec(params, link_app(cfg, origin), origin).await,
            "exec.start" => self.executions.start(params, link_app(cfg, origin), origin),
            "exec.read" => self.executions.read(params, origin).await,
            "exec.cancel" => self.executions.cancel(params, origin),
            "fs.write" | "fs.edit" => {
                // Serialize agl mutations so concurrent exact edits cannot lose updates.
                let _guard = self.files.lock().await;
                dispatch(cfg, origin, method, params).await
            }
            _ => dispatch(cfg, origin, method, params).await,
        }
    }
}

fn link_app(cfg: &Config, origin: &Origin) -> Option<String> {
    match origin {
        Origin::Hub { url, .. } => cfg.hub(url).and_then(|h| h.app_name.clone()),
        Origin::Local => None,
    }
}

/// Largest file `fs.read` / `fs.write` move in one call.
pub const MAX_FILE_BYTES: u64 = 100 * 1024 * 1024;
const EXEC_TIMEOUT_DEFAULT: Duration = Duration::from_secs(10 * 60);
const EXEC_TIMEOUT_MAX: Duration = Duration::from_secs(30 * 60);

/// What a hub may do, by its link's access: any link may read, `edit` and
/// `full` may also write, and only `full` may run a command.
pub(crate) fn allowed(cfg: &Config, origin: &Origin, method: &str) -> Result<()> {
    let Origin::Hub { url, .. } = origin else { return Ok(()) };
    let access = cfg.hub(url).map(|h| h.access.as_str()).unwrap_or("ask");
    let ok = match method {
        "fs.read" => true,
        "fs.write" | "fs.edit" => matches!(access, "edit" | "full"),
        _ => access == "full",
    };
    if ok {
        Ok(())
    } else {
        bail!("`{method}` needs a link with more access than `{access}` (agl connect … --access)")
    }
}

/// Whether `daemon.update` from `origin` must wait until no turn runs.
/// Any hub may update agl: it only installs the newest official release,
/// checked against its sha256, never a binary or version the hub picks.
/// Restarting ends running turns, though, so below `full` a hub's update
/// always waits for its own sessions to be idle (`if_idle`).
pub(crate) fn update_only_if_idle(cfg: &Config, origin: &Origin, if_idle: bool) -> bool {
    match origin {
        Origin::Local => if_idle,
        Origin::Hub { url, .. } => if_idle || cfg.hub(url).map(|h| h.access.as_str()).unwrap_or("ask") != "full",
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
        "fs.edit" => edit(params).await,
        "exec" => {
            // A hub's commands are its product's, as its sessions are.
            let link_app = match origin {
                Origin::Hub { url, .. } => cfg.hub(url).and_then(|h| h.app_name.clone()),
                Origin::Local => None,
            };
            exec(params, link_app).await
        }
        other => Err(anyhow!("unknown method `{other}`")),
    }
}

#[derive(Deserialize)]
struct ReadParams {
    path: String,
    /// Absent/base64 preserves the original binary API. UTF-8 reads are paged.
    encoding: Option<String>,
    offset: Option<usize>,
    limit: Option<usize>,
}

async fn read(params: Value) -> Result<Value> {
    let p: ReadParams = serde_json::from_value(params).context("fs.read params")?;
    let path = resolve(&p.path)?;
    let text = match p.encoding.as_deref() {
        None | Some("base64") => false,
        Some("utf8") => true,
        _ => bail!("encoding must be `base64` or `utf8`"),
    };
    if text {
        return read_text(&path, p.offset.unwrap_or(1), p.limit.unwrap_or(2000)).await;
    }
    if p.offset.is_some() || p.limit.is_some() {
        bail!("offset and limit require encoding `utf8`");
    }
    let meta = tokio::fs::metadata(&path).await.with_context(|| format!("read {}", path.display()))?;
    if !meta.is_file() {
        bail!("not a file: {}", path.display());
    }
    if meta.len() > MAX_FILE_BYTES {
        bail!("file is larger than {} MB: {}", MAX_FILE_BYTES / 1024 / 1024, path.display());
    }
    let mut bytes = Vec::new();
    tokio::fs::File::open(&path).await?.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes).await?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        bail!("file is larger than 100 MB");
    }
    Ok(json!({"content_base64": base64::engine::general_purpose::STANDARD.encode(&bytes), "size": bytes.len()}))
}

#[derive(Deserialize)]
struct WriteParams {
    path: String,
    content_base64: Option<String>,
    content: Option<String>,
}

/// Parent directories are created; the file is replaced atomically.
async fn write(params: Value) -> Result<Value> {
    let p: WriteParams = serde_json::from_value(params).context("fs.write params")?;
    let path = resolve(&p.path)?;
    let bytes = match (p.content, p.content_base64) {
        (Some(s), None) => s.into_bytes(),
        (None, Some(s)) => base64::engine::general_purpose::STANDARD.decode(s.as_bytes()).context("content_base64 is not base64")?,
        _ => bail!("provide exactly one of content or content_base64"),
    };
    atomic_write(&path, &bytes).await?;
    Ok(json!({"size": bytes.len()}))
}

async fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    if bytes.len() as u64 > MAX_FILE_BYTES {
        bail!("file is larger than {} MB", MAX_FILE_BYTES / 1024 / 1024);
    }
    let parent = path.parent().context("path has no parent directory")?;
    tokio::fs::create_dir_all(parent).await.with_context(|| format!("create {}", parent.display()))?;
    let tmp = parent.join(format!(".{}.agl-{}", path.file_name().and_then(|n| n.to_str()).unwrap_or("file"), uuid::Uuid::new_v4().simple()));
    let result = async {
        // Preserve the mode of existing files (scripts and private credentials).
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut f = options.open(&tmp).await?;
        f.write_all(bytes).await?;
        if let Ok(meta) = tokio::fs::metadata(path).await {
            f.set_permissions(meta.permissions()).await?;
        }
        f.flush().await?;
        tokio::fs::rename(&tmp, path).await
    }
    .await;
    if let Err(e) = result {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e).with_context(|| format!("write {}", path.display()));
    }
    Ok(())
}

const TEXT_OUTPUT_MAX: usize = 64 * 1024;

/// Line-oriented text reads never load the whole file or an unbounded line.
async fn read_text(path: &std::path::Path, offset: usize, limit: usize) -> Result<Value> {
    use tokio::io::AsyncBufReadExt;
    if offset == 0 || limit == 0 || limit > 10_000 {
        bail!("offset is 1-based; limit must be 1..10000");
    }
    let meta = tokio::fs::metadata(path).await?;
    if !meta.is_file() {
        bail!("not a file: {}", path.display());
    }
    let file = tokio::fs::File::open(path).await?;
    let mut reader = tokio::io::BufReader::new(file);
    let mut line = 1;
    let mut bytes = Vec::new();
    let mut lines = 0;
    let mut eof = false;
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            eof = true;
            break;
        }
        let end = chunk.iter().position(|b| *b == b'\n').map_or(chunk.len(), |i| i + 1);
        if line >= offset {
            if lines == limit {
                break;
            }
            if bytes.len() + end > TEXT_OUTPUT_MAX {
                if bytes.is_empty() {
                    bail!("line {line} exceeds the 64 KiB text output limit; use exec or a binary read");
                }
                // Never return a partial line: remove this line's earlier chunks.
                let keep = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
                bytes.truncate(keep);
                if bytes.is_empty() {
                    bail!("line {line} exceeds the 64 KiB text output limit");
                }
                break;
            }
            bytes.extend_from_slice(&chunk[..end]);
        }
        let newline = chunk[end - 1] == b'\n';
        reader.consume(end);
        if newline {
            if line >= offset {
                lines += 1;
            }
            line += 1;
        }
    }
    if eof && !bytes.is_empty() && bytes.last() != Some(&b'\n') {
        lines += 1;
    }
    let content = String::from_utf8(bytes).context("file is not UTF-8; use encoding base64")?;
    Ok(json!({"content": content, "size": meta.len(), "offset": offset, "lines": lines,
        "next_offset": if eof { None } else { Some(offset + lines) }, "truncated": !eof}))
}

#[derive(Deserialize)]
struct EditParams {
    path: String,
    old_text: String,
    new_text: String,
}

async fn edit(params: Value) -> Result<Value> {
    let p: EditParams = serde_json::from_value(params).context("fs.edit params")?;
    if p.old_text.is_empty() {
        bail!("old_text must not be empty");
    }
    // Read and replace the same target, keeping a user-facing symlink intact.
    let path = tokio::fs::canonicalize(resolve(&p.path)?).await?;
    let meta = tokio::fs::metadata(&path).await?;
    if !meta.is_file() {
        bail!("not a file: {}", path.display());
    }
    if meta.len() > MAX_FILE_BYTES {
        bail!("file is larger than 100 MB");
    }
    // Take at most the cap even if another process grows the file.
    let mut bytes = Vec::new();
    tokio::fs::File::open(&path).await?.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes).await?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        bail!("file is larger than 100 MB");
    }
    let content = String::from_utf8(bytes).context("file is not UTF-8")?;
    let first = content.find(&p.old_text).context("old_text was not found; read the file again")?;
    if content[first + content[first..].chars().next().unwrap().len_utf8()..].contains(&p.old_text) {
        bail!("old_text matches more than once; provide more context");
    }
    let updated = format!("{}{}{}", &content[..first], p.new_text, &content[first + p.old_text.len()..]);
    atomic_write(&path, updated.as_bytes()).await?;
    Ok(json!({"size": updated.len(), "replacements": 1}))
}

#[derive(Deserialize)]
struct ExecParams {
    #[serde(default)]
    argv: Vec<String>,
    command: Option<String>,
    cwd: Option<String>,
    timeout_ms: Option<u64>,
    #[serde(default)]
    env: std::collections::HashMap<String, String>,
    stdin: Option<String>,
    /// The product the command runs for (macOS privacy prompts name it).
    app_name: Option<String>,
}

/// Run `argv` (no shell) with the agents' environment, in its own process
/// group, killed with everything it started on timeout.
fn spawn_exec(params: Value, link_app: Option<String>) -> Result<(tokio::process::Child, Duration, Option<String>)> {
    let mut p: ExecParams = serde_json::from_value(params).context("exec params")?;
    let app_name = match p.app_name.as_deref().or(link_app.as_deref()) {
        Some(n) => crate::app::validate(n)?,
        None => None,
    };
    if let Some(command) = p.command.take() {
        if !p.argv.is_empty() {
            bail!("provide argv or command, not both");
        }
        p.argv = vec!["/bin/sh".into(), "-c".into(), command];
    }
    let Some(program) = p.argv.first() else { bail!("argv is empty") };
    let cwd = match &p.cwd {
        Some(c) => resolve(c)?,
        None => dirs::home_dir().context("no home directory")?,
    };
    let timeout = p.timeout_ms.map(Duration::from_millis).unwrap_or(EXEC_TIMEOUT_DEFAULT).min(EXEC_TIMEOUT_MAX);
    let bin = crate::shellenv::which(program).unwrap_or_else(|| PathBuf::from(program));
    let (launch, pre_args) = crate::app::wrap(&bin, app_name.as_deref());
    let mut cmd = tokio::process::Command::new(launch);
    cmd.args(pre_args);
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
    let child = cmd.spawn().with_context(|| format!("run {program}"))?;
    Ok((child, timeout, p.stdin))
}

async fn exec(params: Value, link_app: Option<String>) -> Result<Value> {
    Executions::default().exec(params, link_app, &Origin::Local).await
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
    fn hub_updates_wait_for_idle_below_full() {
        for access in ["read_only", "ask", "edit"] {
            let (cfg, hub) = hub_cfg(access);
            assert!(update_only_if_idle(&cfg, &hub, false), "{access}");
        }
        let (cfg, hub) = hub_cfg("full");
        assert!(!update_only_if_idle(&cfg, &hub, false));
        assert!(update_only_if_idle(&cfg, &hub, true));
        assert!(!update_only_if_idle(&Config::default(), &Origin::Local, false));
    }

    #[test]
    fn paths_must_be_absolute_or_home() {
        assert!(resolve("relative/file").is_err());
        assert!(resolve("/tmp/x").is_ok());
        assert!(resolve("~/x").unwrap().is_absolute());
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // a test lock; nothing else awaits it
    async fn write_read_and_exec() {
        let _serial = crate::agents::TRACKED_PROCESS_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("agl-machine-{}", uuid::Uuid::new_v4().simple()));
        let file = dir.join("a/b.txt");
        let path = file.to_str().unwrap();
        let data = base64::engine::general_purpose::STANDARD.encode("hello");
        write(json!({"path": path, "content_base64": data})).await.unwrap();
        let r = read(json!({"path": path})).await.unwrap();
        assert_eq!(r["content_base64"], data);
        let r = exec(json!({"argv": ["sh", "-c", "cat b.txt; echo oops >&2; exit 3"], "cwd": dir.join("a").to_str().unwrap()}), None).await.unwrap();
        assert_eq!((r["stdout"].as_str(), r["stderr"].as_str(), r["exit_code"].as_i64()), (Some("hello"), Some("oops\n"), Some(3)));
        let r = exec(json!({"argv": ["sleep", "5"], "timeout_ms": 200}), None).await.unwrap();
        assert_eq!(r["timed_out"], true);
        let _ = std::fs::remove_dir_all(dir);
    }
}
