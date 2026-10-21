//! Client for the local daemon, used by the CLI commands.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

use crate::config::{self, Config};
use crate::protocol::{EventEnvelope, WireMsg};

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value>>>>>;

pub struct Client {
    out: mpsc::UnboundedSender<String>,
    pending: Pending,
    next: AtomicU64,
    pub events: mpsc::UnboundedReceiver<EventEnvelope>,
}

impl Client {
    /// Connect to the daemon described by `$AGL_HOME/daemon.json`.
    pub async fn connect_local() -> Result<Client> {
        let info = std::fs::read_to_string(config::daemon_file()).context("daemon is not running")?;
        let info: Value = serde_json::from_str(&info)?;
        let port = info["port"].as_u64().ok_or_else(|| anyhow!("daemon.json has no port"))? as u16;
        let token = info["token"].as_str().unwrap_or_default();
        Client::connect(port, token).await
    }

    pub async fn connect(port: u16, token: &str) -> Result<Client> {
        let url = format!("ws://127.0.0.1:{port}/ws?token={}", crate::link::urlencode(token));
        let (ws, _) = tokio_tungstenite::connect_async(url).await?;
        let (mut sink, mut stream) = ws.split();
        let (out, mut out_rx) = mpsc::unbounded_channel::<String>();
        let (ev_tx, events) = mpsc::unbounded_channel();
        let pending: Pending = Arc::default();
        tokio::spawn(async move {
            while let Some(s) = out_rx.recv().await {
                if sink.send(Message::Text(s.into())).await.is_err() {
                    break;
                }
            }
        });
        let p = pending.clone();
        tokio::spawn(async move {
            while let Some(Ok(msg)) = stream.next().await {
                let Message::Text(text) = msg else { continue };
                match serde_json::from_str::<WireMsg>(&text) {
                    Ok(WireMsg::Response { id, result, error }) => {
                        if let Some(tx) = p.lock().unwrap().remove(&id) {
                            let _ = tx.send(match error {
                                Some(e) => Err(anyhow!(e)),
                                None => Ok(result.unwrap_or(Value::Null)),
                            });
                        }
                    }
                    Ok(WireMsg::Event(env)) => {
                        let _ = ev_tx.send(env);
                    }
                    _ => {}
                }
            }
            for (_, tx) in p.lock().unwrap().drain() {
                let _ = tx.send(Err(anyhow!("daemon connection closed")));
            }
        });
        Ok(Client { out, pending, next: AtomicU64::new(1), events })
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next.fetch_add(1, Ordering::SeqCst).to_string();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id.clone(), tx);
        let msg = WireMsg::Request { id, method: method.into(), params };
        self.out.send(msg.to_json()).map_err(|_| anyhow!("daemon connection closed"))?;
        rx.await.map_err(|_| anyhow!("daemon connection closed"))?
    }
}

/// Connect to the daemon, starting it in the background if needed.
pub async fn ensure_daemon(_cfg: &Config) -> Result<Client> {
    if let Ok(c) = Client::connect_local().await {
        return Ok(c);
    }
    spawn_detached_daemon()?;
    for _ in 0..80 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Ok(c) = Client::connect_local().await {
            return Ok(c);
        }
    }
    bail!("daemon did not start; see {}", config::log_path().display())
}

pub fn spawn_detached_daemon() -> Result<()> {
    std::fs::create_dir_all(config::home())?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(config::log_path())?;
    let exe = std::env::current_exe().context("locate agl executable")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn().context("start daemon")?;
    Ok(())
}
