//! Bounded, caller-owned executions. Polling cursors work over both hub transports
//! and survive a socket reconnect to the same daemon. No model or agent needed.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::{watch, Notify};

use crate::session::Origin;

const OUTPUT_MAX: usize = 16 * 1024 * 1024;
const MAX_JOBS: usize = 1024;
const MAX_RESULTS: usize = 8;
const MAX_RUNNING: usize = 4;
const RETENTION: Duration = Duration::from_secs(600);

#[derive(Default)]
pub struct Executions {
    jobs: Mutex<HashMap<(String, String), Arc<Job>>>,
    stopping: std::sync::atomic::AtomicBool,
}

struct Job {
    id: String,
    owner: String,
    fingerprint: Vec<u8>,
    state: Mutex<State>,
    changed: Notify,
    cancel: watch::Sender<bool>,
}

#[derive(Default)]
struct State {
    stdout: String,
    stderr: String,
    done: bool,
    exit_code: Option<i32>,
    timed_out: bool,
    cancelled: bool,
    truncated: bool,
    error: Option<String>,
    finished: Option<Instant>,
    evicted: bool,
}

impl Executions {
    fn launch(&self, mut input: Value, app: Option<String>, origin: &Origin) -> Result<Arc<Job>> {
        let object = input.as_object_mut().context("exec params must be an object")?;
        let id = match object.remove("execution_id") {
            Some(Value::String(id)) if !id.is_empty() && id.len() <= 128 => id,
            None | Some(Value::Null) => uuid::Uuid::new_v4().simple().to_string(),
            _ => bail!("execution_id must be a nonempty string of at most 128 bytes"),
        };
        let owner = origin.tag();
        let key = (owner.clone(), id.clone());
        use sha2::{Digest, Sha256};
        let fingerprint = Sha256::digest(serde_json::to_vec(&input)?).to_vec();
        // Lock covers lookup, spawn and insert: concurrent retries launch only once.
        let mut jobs = self.jobs.lock().unwrap();
        if self.stopping.load(std::sync::atomic::Ordering::SeqCst) {
            bail!("tool runtime is shutting down");
        }
        jobs.retain(|_, j| j.state.lock().unwrap().finished.is_none_or(|t| t.elapsed() < RETENTION));
        if let Some(j) = jobs.get(&key) {
            if j.fingerprint != fingerprint {
                bail!("execution_id already used with different arguments");
            }
            if j.state.lock().unwrap().evicted {
                bail!("execution result expired; this ID will not be executed again during retention");
            }
            return Ok(j.clone());
        }
        if jobs.len() >= MAX_JOBS {
            bail!("execution ID storage is full; IDs expire after 10 minutes");
        }
        if jobs.values().filter(|j| !j.state.lock().unwrap().done).count() >= MAX_RUNNING {
            bail!("at most {MAX_RUNNING} commands may run concurrently");
        }
        let (child, timeout, stdin) = super::spawn_exec(input.clone(), app)?;
        if let Some(pid) = child.id() {
            crate::agents::track(pid);
        }
        let (cancel, rx) = watch::channel(false);
        // Bound output memory while retaining tombstones that prevent a late
        // retry from launching an already completed command a second time.
        let mut completed: Vec<_> = jobs
            .values()
            .filter_map(|j| {
                let s = j.state.lock().unwrap();
                s.finished.filter(|_| !s.evicted).map(|t| (t, j.clone()))
            })
            .collect();
        completed.sort_by_key(|(t, _)| *t);
        let evict = completed.len().saturating_sub(MAX_RESULTS - MAX_RUNNING);
        for (_, old) in completed.into_iter().take(evict) {
            let mut s = old.state.lock().unwrap();
            s.stdout = String::new();
            s.stderr = String::new();
            s.evicted = true;
        }
        let j = Arc::new(Job { id, owner, fingerprint, state: Mutex::new(State::default()), changed: Notify::new(), cancel });
        jobs.insert(key, j.clone());
        tokio::spawn(run(j.clone(), child, timeout, stdin, rx));
        Ok(j)
    }

    pub fn start(&self, input: Value, app: Option<String>, origin: &Origin) -> Result<Value> {
        let j = self.launch(input, app, origin)?;
        Ok(json!({"execution_id": j.id}))
    }

    /// The original blocking exec response remains compatible.
    pub async fn exec(&self, input: Value, app: Option<String>, origin: &Origin) -> Result<Value> {
        let remember = input["execution_id"].is_string();
        let j = self.launch(input, app, origin)?;
        loop {
            let changed = j.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if j.state.lock().unwrap().done {
                break;
            }
            changed.await;
        }
        if !remember {
            self.jobs.lock().unwrap().remove(&(j.owner.clone(), j.id.clone()));
        }
        let s = j.state.lock().unwrap();
        Ok(json!({"stdout": s.stdout, "stderr": s.stderr, "exit_code": s.exit_code.unwrap_or(-1),
            "timed_out": s.timed_out, "cancelled": s.cancelled, "truncated": s.truncated, "error": s.error}))
    }

    fn get(&self, id: &str, origin: &Origin) -> Result<Arc<Job>> {
        let jobs = self.jobs.lock().unwrap();
        // IDs are private to an origin. A local administrator may query a hub's job.
        let found = jobs.get(&(origin.tag(), id.to_string())).cloned().or_else(|| {
            if *origin == Origin::Local {
                let mut matches = jobs.values().filter(|j| j.id == id);
                let first = matches.next().cloned();
                if matches.next().is_none() {
                    first
                } else {
                    None
                }
            } else {
                None
            }
        });
        found.filter(|j| j.state.lock().unwrap().finished.is_none_or(|t| t.elapsed() < RETENTION)).with_context(|| format!("no execution `{id}`"))
    }

    pub async fn read(&self, input: Value, origin: &Origin) -> Result<Value> {
        #[derive(Deserialize)]
        struct Params {
            execution_id: String,
            #[serde(default)]
            stdout_offset: usize,
            #[serde(default)]
            stderr_offset: usize,
            limit: Option<usize>,
            #[serde(default)]
            wait_ms: u64,
        }
        let p: Params = serde_json::from_value(input)?;
        let limit = p.limit.unwrap_or(64 * 1024);
        if !(4..=64 * 1024).contains(&limit) {
            bail!("limit must be 4..65536 bytes per stream");
        }
        let j = self.get(&p.execution_id, origin)?;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(p.wait_ms.min(30_000));
        loop {
            let changed = j.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let s = j.state.lock().unwrap();
                if s.evicted {
                    bail!("execution result expired");
                }
                let (out, next_out) = slice(&s.stdout, p.stdout_offset, limit)?;
                let (err, next_err) = slice(&s.stderr, p.stderr_offset, limit)?;
                if s.done || !out.is_empty() || !err.is_empty() || tokio::time::Instant::now() >= deadline {
                    return Ok(json!({"execution_id": j.id, "status": if s.done { "completed" } else { "running" },
                        "stdout": out, "stderr": err, "stdout_offset": next_out, "stderr_offset": next_err,
                        "has_more": next_out < s.stdout.len() || next_err < s.stderr.len(),
                        "exit_code": s.exit_code, "timed_out": s.timed_out, "cancelled": s.cancelled,
                        "truncated": s.truncated, "error": s.error}));
                }
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                continue;
            }
        }
    }

    pub fn cancel(&self, input: Value, origin: &Origin) -> Result<Value> {
        let id = input["execution_id"].as_str().context("execution_id required")?;
        let j = self.get(id, origin)?;
        if !j.state.lock().unwrap().done {
            j.cancel.send_replace(true);
        }
        Ok(json!({"execution_id": id, "ok": true}))
    }

    pub fn busy_counts(&self, origin: &Origin) -> (usize, usize) {
        let jobs = self.jobs.lock().unwrap();
        let mut own = 0;
        let mut elsewhere = 0;
        for j in jobs.values().filter(|j| !j.state.lock().unwrap().done) {
            if *origin == Origin::Local || j.owner == origin.tag() {
                own += 1;
            } else {
                elsewhere += 1;
            }
        }
        (own, elsewhere)
    }

    pub fn gc(&self) {
        self.jobs.lock().unwrap().retain(|_, j| j.state.lock().unwrap().finished.is_none_or(|t| t.elapsed() < RETENTION));
    }

    pub async fn shutdown(&self) {
        self.stopping.store(true, std::sync::atomic::Ordering::SeqCst);
        let jobs: Vec<_> = self.jobs.lock().unwrap().values().cloned().collect();
        for j in &jobs {
            j.cancel.send_replace(true);
        }
        for j in jobs {
            loop {
                let changed = j.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if j.state.lock().unwrap().done {
                    break;
                }
                changed.await;
            }
        }
    }
}

impl Drop for Executions {
    fn drop(&mut self) {
        for j in self.jobs.get_mut().unwrap().values() {
            j.cancel.send_replace(true);
        }
    }
}

fn slice(text: &str, offset: usize, limit: usize) -> Result<(&str, usize)> {
    if offset > text.len() || !text.is_char_boundary(offset) {
        bail!("invalid output cursor");
    }
    let mut end = (offset + limit).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    Ok((&text[offset..end], end))
}

async fn run(j: Arc<Job>, mut child: tokio::process::Child, timeout: Duration, stdin: Option<String>, mut cancel: watch::Receiver<bool>) {
    let pid = child.id();
    let input = child.stdin.take();
    let writer = tokio::spawn(async move {
        if let (Some(text), Some(mut sink)) = (stdin, input) {
            let _ = sink.write_all(text.as_bytes()).await;
        }
    });
    let out = tokio::spawn(collect(child.stdout.take().expect("piped"), j.clone(), false));
    let err = tokio::spawn(collect(child.stderr.take().expect("piped"), j.clone(), true));
    // Include pipe draining in the deadline: a shell can exit while a child
    // keeps its inherited stdout open. Such a command must not hang forever.
    let completed = async {
        let status = child.wait().await?;
        while !out.is_finished() || !err.is_finished() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        anyhow::Ok(status)
    };
    let (status, timed_out, cancelled) = tokio::select! {
        status = completed => (Some(status), false, false),
        _ = tokio::time::sleep(timeout) => (None, true, false),
        _ = async { if !*cancel.borrow_and_update() { let _ = cancel.changed().await; } } => (None, false, true),
    };
    if status.is_none() {
        // kill_tree handles descendants while the root lives. Also signal the
        // original group when wait() already reaped the root (open-pipe case).
        if child.id().is_some() {
            crate::agents::kill_tree(&mut child).await;
        } else {
            #[cfg(unix)]
            if let Some(pid) = pid {
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
        }
    }
    writer.abort();
    // Readers normally reach EOF after the process group is killed. Bound
    // cleanup for escaped descendants that retain a pipe.
    let mut out = out;
    let mut err = err;
    if tokio::time::timeout(Duration::from_millis(500), async {
        let _ = (&mut out).await;
        let _ = (&mut err).await;
    })
    .await
    .is_err()
    {
        out.abort();
        err.abort();
    }
    if let Some(pid) = pid {
        crate::agents::untrack(pid);
    }
    let mut s = j.state.lock().unwrap();
    match status {
        Some(Ok(status)) => s.exit_code = Some(status.code().unwrap_or(-1)),
        Some(Err(e)) => s.error = Some(e.to_string()),
        None => {}
    }
    s.timed_out = timed_out;
    s.cancelled = cancelled;
    s.finished = Some(Instant::now());
    s.done = true;
    drop(s);
    j.changed.notify_waiters();
}

async fn collect(mut reader: impl AsyncRead + Unpin, j: Arc<Job>, stderr: bool) {
    let mut pending = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = match reader.read(&mut buf).await {
            Ok(n) => n,
            Err(e) => {
                j.state.lock().unwrap().error = Some(format!("read output: {e}"));
                0
            }
        };
        pending.extend_from_slice(&buf[..n]);
        // Keep an incomplete UTF-8 suffix until the next read; binary output
        // is lossily decoded, exactly as the original synchronous exec was.
        let mut consumed = 0;
        let mut text = String::new();
        while consumed < pending.len() {
            match std::str::from_utf8(&pending[consumed..]) {
                Ok(t) => {
                    text.push_str(t);
                    consumed = pending.len();
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    text.push_str(std::str::from_utf8(&pending[consumed..consumed + valid]).unwrap());
                    consumed += valid;
                    match e.error_len() {
                        Some(len) => {
                            text.push('\u{fffd}');
                            consumed += len;
                        }
                        None if n == 0 => {
                            text.push('\u{fffd}');
                            consumed = pending.len();
                        }
                        None => break,
                    }
                }
            }
        }
        pending.drain(..consumed);
        {
            let mut s = j.state.lock().unwrap();
            let stream = if stderr { &mut s.stderr } else { &mut s.stdout };
            let mut keep = text.len().min(OUTPUT_MAX - stream.len());
            while !text.is_char_boundary(keep) {
                keep -= 1;
            }
            stream.push_str(&text[..keep]);
            s.truncated |= keep < text.len();
        }
        j.changed.notify_waiters();
        if n == 0 {
            break;
        }
    }
}
