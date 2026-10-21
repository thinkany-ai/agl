//! Outbound link from this machine to a hub. The machine dials out over
//! WebSocket (works behind NAT), introduces itself with `hello`, then serves
//! the hub's requests with the same dispatcher as the local API and streams
//! every session event back.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::Message;

use crate::daemon::{dispatch, machine_info, Daemon};
use crate::protocol::WireMsg;

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

fn set_status(d: &Daemon, s: impl Into<String>) {
    *d.hub_status.lock().unwrap() = s.into();
}

pub async fn supervise(d: Arc<Daemon>) {
    let mut backoff = 1u64;
    loop {
        let cfg = d.mgr.cfg();
        let auth = match (&cfg.hub_token, &cfg.hub_pair_code) {
            (Some(t), _) => Some(("token", t.clone())),
            (None, Some(c)) => Some(("code", c.clone())),
            _ => None,
        };
        let (Some(hub), Some((kind, secret))) = (cfg.hub_url.clone(), auth) else {
            set_status(&d, "disabled");
            d.reload.notified().await;
            continue;
        };
        set_status(&d, format!("connecting to {hub}"));
        match connect_once(&d, &hub, kind, &secret).await {
            Ok(()) => {
                // Reload requested (or just paired): reconnect at once.
                backoff = 1;
                continue;
            }
            Err(e) if is_unauthorized(&e) => {
                // Revoked or expired: retrying cannot help. Wait for `agl connect`.
                let what = if kind == "code" { "pairing code is invalid or expired" } else { "this machine was revoked" };
                set_status(&d, format!("unauthorized: {what}; run `agl connect` again"));
                eprintln!("hub link: unauthorized ({what})");
                d.reload.notified().await;
                continue;
            }
            Err(e) => {
                set_status(&d, format!("error: {e:#} (retry in {backoff}s)"));
                eprintln!("hub link: {e:#}");
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(backoff)) => {}
            _ = d.reload.notified() => {}
        }
        backoff = (backoff * 2).min(30);
    }
}

fn is_unauthorized(e: &anyhow::Error) -> bool {
    e.chain().any(|c| match c.downcast_ref::<tokio_tungstenite::tungstenite::Error>() {
        Some(tokio_tungstenite::tungstenite::Error::Http(resp)) => matches!(resp.status().as_u16(), 401 | 403),
        _ => false,
    })
}

async fn connect_once(d: &Arc<Daemon>, hub: &str, kind: &str, secret: &str) -> Result<()> {
    let cfg = d.mgr.cfg();
    let url = format!(
        "{}?{kind}={}&machine_id={}",
        ws_url(hub, "/ws/machine")?,
        urlencode(secret),
        urlencode(&cfg.machine_id)
    );
    let (ws, _) = tokio::time::timeout(Duration::from_secs(15), tokio_tungstenite::connect_async(url))
        .await
        .context("connect timed out")?
        .context("connect")?;
    let (mut sink, mut stream) = ws.split();
    if kind == "code" {
        // The hub answers a valid code with this machine's own token.
        let first = tokio::time::timeout(Duration::from_secs(15), stream.next()).await.context("pairing timed out")?;
        let Some(Ok(Message::Text(text))) = first else { bail!("hub closed the connection during pairing") };
        let Ok(WireMsg::Paired { token }) = serde_json::from_str::<WireMsg>(&text) else { bail!("unexpected pairing reply") };
        let mut fresh = crate::config::Config::load()?;
        fresh.hub_token = Some(token);
        fresh.hub_pair_code = None;
        fresh.save()?;
        *d.mgr.cfg.write().unwrap() = fresh;
        eprintln!("hub link: paired with {hub}");
        return Ok(());
    }
    sink.send(Message::Text(WireMsg::Hello { machine: machine_info(&cfg).await }.to_json().into())).await?;
    set_status(d, format!("connected to {hub}"));
    eprintln!("hub link: connected to {hub}");

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
                            let (d, out_tx) = (d.clone(), out_tx.clone());
                            tokio::spawn(async move {
                                let res = dispatch(&d, &method, params, false).await;
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
                Ok(env) => sink.send(Message::Text(WireMsg::Event(env).to_json().into())).await?,
                Err(broadcast::error::RecvError::Lagged(n)) => eprintln!("hub link: dropped {n} events"),
                Err(_) => bail!("event bus closed"),
            },
            _ = ping.tick() => {
                if last_seen.elapsed() > Duration::from_secs(60) {
                    bail!("hub stopped responding");
                }
                sink.send(Message::Ping(Vec::new().into())).await?;
            }
            _ = d.reload.notified() => {
                let _ = sink.send(Message::Close(None)).await;
                return Ok(());
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
