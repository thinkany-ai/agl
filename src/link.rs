//! Outbound links from this machine to hubs (web apps). The machine dials
//! out over WebSocket (works behind NAT), introduces itself with `hello`,
//! then serves the hub's requests with the same dispatcher as the local API
//! and streams back the events of the sessions that hub may see.
//!
//! One task per configured hub; tasks are started and stopped as `hubs` in
//! the config changes (`hub.reload`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::Message;

use crate::config::{Config, HubLink};
use crate::daemon::{dispatch, machine_info, Daemon};
use crate::protocol::WireMsg;
use crate::session::Origin;

pub fn ws_url(hub: &str, path: &str) -> Result<String> {
    let base = hub.trim_end_matches('/');
    let base = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if base.starts_with("ws://") || base.starts_with("wss://") {
        base.to_string()
    } else {
        bail!("hub url must start with http(s):// or ws(s)://: {hub}");
    };
    Ok(format!("{base}{path}"))
}

fn set_status(d: &Daemon, url: &str, s: impl Into<String>) {
    d.hub_status.lock().unwrap().insert(url.to_string(), s.into());
}

/// Keep one link task per configured hub.
pub async fn supervise(d: Arc<Daemon>) {
    let mut running: HashMap<String, (HubLink, tokio::task::JoinHandle<()>)> = HashMap::new();
    let mut changes = d.reload.subscribe();
    loop {
        let hubs = d.mgr.cfg().hubs;
        // Stop links that were removed or re-paired.
        running.retain(|url, (link, task)| {
            let keep = hubs.iter().any(|h| h == link);
            if !keep {
                task.abort();
                d.hub_status.lock().unwrap().remove(url);
            }
            keep
        });
        for h in hubs {
            if !running.contains_key(&h.url) {
                let task = tokio::spawn(run_link(d.clone(), h.url.clone()));
                running.insert(h.url.clone(), (h, task));
            }
        }
        if changes.changed().await.is_err() {
            return;
        }
    }
}

/// Connect to one hub, reconnecting with backoff until the link is removed.
async fn run_link(d: Arc<Daemon>, url: String) {
    let mut backoff = 1u64;
    loop {
        let Some(link) = d.mgr.cfg().hub(&url).cloned() else { return };
        let (kind, secret) = match (&link.token, &link.pair_code) {
            (Some(t), _) => ("token", t.clone()),
            (None, Some(c)) => ("code", c.clone()),
            _ => {
                set_status(&d, &url, "unauthorized: no token; run `agl connect` again");
                return;
            }
        };
        set_status(&d, &url, "connecting");
        match connect_once(&d, &link, kind, &secret).await {
            // Just paired: `supervise` restarts the link with the new token.
            Ok(()) => return,
            Err(e) if is_unauthorized(&e) => {
                // Revoked or expired: retrying cannot help. Wait for `agl connect`.
                let what = if kind == "code" { "pairing code is invalid or expired" } else { "this machine was revoked" };
                set_status(&d, &url, format!("unauthorized: {what}; run `agl connect` again"));
                eprintln!("hub link {url}: unauthorized ({what})");
                return;
            }
            Err(e) => {
                set_status(&d, &url, format!("error: {e:#} (retry in {backoff}s)"));
                eprintln!("hub link {url}: {e:#}");
            }
        }
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(30);
    }
}

fn is_unauthorized(e: &anyhow::Error) -> bool {
    e.chain().any(|c| match c.downcast_ref::<tokio_tungstenite::tungstenite::Error>() {
        Some(tokio_tungstenite::tungstenite::Error::Http(resp)) => matches!(resp.status().as_u16(), 401 | 403),
        _ => false,
    })
}

async fn connect_once(d: &Arc<Daemon>, link: &HubLink, kind: &str, secret: &str) -> Result<()> {
    let cfg = d.mgr.cfg();
    let hub = &link.url;
    let url = format!(
        "{}?{kind}={}&machine_id={}",
        ws_url(hub, "/ws/machine")?,
        urlencode(secret),
        urlencode(&cfg.machine_id)
    );
    let dial = async {
        let t = crate::proxy::target(&url)?;
        match crate::proxy::proxy_for(&t) {
            Some(proxy) => {
                let stream = crate::proxy::tunnel(&proxy, &t).await?;
                Ok(tokio_tungstenite::client_async_tls(url.as_str(), stream).await?)
            }
            None => anyhow::Ok(tokio_tungstenite::connect_async(url.as_str()).await?),
        }
    };
    let (ws, _) = tokio::time::timeout(Duration::from_secs(15), dial).await.context("connect timed out")?.context("connect")?;
    let (mut sink, mut stream) = ws.split();
    if kind == "code" {
        // The hub answers a valid code with this machine's own token.
        let first = tokio::time::timeout(Duration::from_secs(15), stream.next()).await.context("pairing timed out")?;
        let Some(Ok(Message::Text(text))) = first else { bail!("hub closed the connection during pairing") };
        let Ok(WireMsg::Paired { token }) = serde_json::from_str::<WireMsg>(&text) else { bail!("unexpected pairing reply") };
        let mut fresh = Config::load()?;
        let Some(entry) = fresh.hubs.iter_mut().find(|h| h.url == *hub) else { bail!("hub was removed while pairing") };
        entry.token = Some(token);
        entry.pair_code = None;
        fresh.save()?;
        *d.mgr.cfg.write().unwrap() = fresh;
        eprintln!("hub link {hub}: paired");
        // The changed entry restarts this link through `supervise`.
        d.reload.send_modify(|g| *g += 1);
        return Ok(());
    }
    sink.send(Message::Text(WireMsg::Hello { machine: machine_info(&cfg).await }.to_json().into())).await?;
    set_status(d, hub, "connected");
    eprintln!("hub link {hub}: connected");

    let origin = Origin::Hub { url: hub.clone(), share_local: link.share_local };
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    let mut bus = d.mgr.bus.subscribe();
    let mut ping = tokio::time::interval(Duration::from_secs(20));
    let mut last_seen = tokio::time::Instant::now();

    loop {
        tokio::select! {
            msg = stream.next() => {
                let Some(msg) = msg else { bail!("hub closed the connection") };
                last_seen = tokio::time::Instant::now();
                match msg? {
                    Message::Text(text) => {
                        if let Ok(WireMsg::Request { id, method, params }) = serde_json::from_str::<WireMsg>(&text) {
                            let (d, out_tx, origin) = (d.clone(), out_tx.clone(), origin.clone());
                            tokio::spawn(async move {
                                let res = dispatch(&d, &method, params, &origin).await;
                                let _ = out_tx.send(WireMsg::reply(id, res).to_json());
                            });
                        }
                    }
                    Message::Ping(p) => sink.send(Message::Pong(p)).await?,
                    Message::Close(_) => bail!("hub closed the connection"),
                    _ => {}
                }
            }
            Some(out) = out_rx.recv() => sink.send(Message::Text(out.into())).await?,
            ev = bus.recv() => match ev {
                Ok(env) => {
                    // Only the sessions this hub may see.
                    if d.mgr.origin_of(&env.session_id).map(|o| origin.sees(&o)).unwrap_or(false) {
                        sink.send(Message::Text(WireMsg::Event(env).to_json().into())).await?;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => eprintln!("hub link {hub}: dropped {n} events"),
                Err(_) => bail!("event bus closed"),
            },
            _ = ping.tick() => {
                if last_seen.elapsed() > Duration::from_secs(60) {
                    bail!("hub stopped responding");
                }
                sink.send(Message::Ping(Vec::new().into())).await?;
            }
        }
    }
}

pub fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}
