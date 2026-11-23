//! Outbound links from this machine to hubs (web apps). The machine dials
//! out (works behind NAT), introduces itself with `hello`, then serves the
//! hub's requests with the same dispatcher as the local API and streams back
//! the events of the sessions that hub may see. Over one WebSocket here, or
//! over HTTP long polling (`link_http`); the messages are the same.
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

use crate::config::{self, Config, HubLink};
use crate::daemon::{dispatch, machine_info, Daemon};
use crate::protocol::WireMsg;
use crate::session::Origin;

/// How a host links this machine to a hub: [`connect`]. The same choices as
/// the `connect` command's flags.
#[derive(Clone, Debug, Default)]
pub struct ConnectOptions {
    /// One-time pairing code from the hub (exchanged for a machine token).
    pub code: Option<String>,
    /// Long-lived machine (or hub) token, instead of a code.
    pub token: Option<String>,
    /// Access for sessions this hub creates without naming one.
    pub access: Option<String>,
    /// Do not let this hub see sessions started locally.
    pub private: bool,
    /// `ws` or `http`.
    pub transport: Option<String>,
    /// The product behind this hub, for macOS privacy prompts (`""` clears).
    pub app_name: Option<String>,
    /// Wait this long for the link to come up (or fail) on a running daemon.
    pub wait: Option<Duration>,
}

/// Links this machine to a hub, as the `connect` command does: adds the hub
/// (or replaces its credentials) in the config of this home and makes a
/// daemon running there pick it up. With `wait`, returns once the daemon
/// reports the link connected, and fails on an error or when no daemon runs.
/// Without a running daemon the link comes up when one starts.
pub async fn connect(hub_url: &str, opts: ConnectOptions) -> Result<()> {
    ws_url(hub_url, "")?;
    let brand = crate::brand::get();
    if !brand.hub_allowed(hub_url) {
        return Err(brand.refuse_hub(hub_url));
    }
    if opts.code.is_some() == opts.token.is_some() {
        bail!("pass exactly one of a pairing code or a token");
    }
    if let Some(a) = &opts.access {
        if !crate::session::ACCESS_LEVELS.contains(&a.as_str()) {
            bail!("access must be one of {}", crate::session::ACCESS_LEVELS.join(", "));
        }
    }
    if let Some(t) = &opts.transport {
        if !config::TRANSPORTS.contains(&t.as_str()) {
            bail!("transport must be one of {}", config::TRANSPORTS.join(", "));
        }
    }
    let url = config::normalize_url(hub_url);
    let mut cfg = Config::load()?;
    // Adding a hub keeps the others; re-connecting one replaces its credentials.
    let mut entry = cfg.hub(&url).cloned().unwrap_or_else(|| HubLink::new(&url));
    entry.token = opts.token;
    entry.pair_code = opts.code;
    if let Some(a) = opts.access {
        entry.access = a;
    }
    if opts.private {
        entry.share_local = false;
    }
    if let Some(t) = opts.transport {
        entry.transport = t;
    }
    if let Some(n) = opts.app_name {
        entry.app_name = crate::app::validate(&n).context("app_name")?;
    }
    cfg.hubs.retain(|h| h.url != url);
    cfg.hubs.push(entry);
    cfg.save()?;
    let running = crate::client::Client::connect_local().await;
    let Some(wait) = opts.wait else {
        if let Ok(c) = running {
            c.call("hub.reload", serde_json::json!({})).await?;
        }
        return Ok(());
    };
    let c = running.map_err(|_| anyhow::anyhow!("the {} daemon is not running", brand.display_name))?;
    c.call("hub.reload", serde_json::json!({})).await?;
    let deadline = tokio::time::Instant::now() + wait;
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let st = c.call("daemon.status", serde_json::json!({})).await?;
        let hs = st["hubs"]
            .as_array()
            .and_then(|h| h.iter().find(|h| h["url"] == url.as_str()))
            .and_then(|h| h["status"].as_str())
            .unwrap_or("")
            .to_string();
        if hs == "connected" {
            return Ok(());
        }
        if hs.starts_with("error") || hs.starts_with("unauthorized") || hs.starts_with("refused") {
            bail!("{url}: {hs}");
        }
    }
    bail!("timed out connecting to {url}; check `{} status`", brand.cli_name)
}

/// Unlinks this machine from a hub, as the `disconnect` command does. False
/// when it was not linked. A daemon running in this home drops the link.
pub async fn disconnect(hub_url: &str) -> Result<bool> {
    let url = config::normalize_url(hub_url);
    let mut cfg = Config::load()?;
    let before = cfg.hubs.len();
    cfg.hubs.retain(|h| h.url != url);
    if cfg.hubs.len() == before {
        return Ok(false);
    }
    cfg.save()?;
    if let Ok(c) = crate::client::Client::connect_local().await {
        c.call("hub.reload", serde_json::json!({})).await?;
    }
    Ok(true)
}

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

pub(crate) fn set_status(d: &Daemon, url: &str, s: impl Into<String>) {
    d.hub_status.lock().unwrap().insert(url.to_string(), s.into());
}

/// Keep one link task per configured hub. A hub the brand does not allow
/// (`allowed_hubs`) stays in the config but is not linked. Returns, with
/// every link stopped, when the daemon stops.
pub async fn supervise(d: Arc<Daemon>) {
    let mut running: HashMap<String, (HubLink, tokio::task::JoinHandle<()>)> = HashMap::new();
    let mut changes = d.reload.subscribe();
    let mut refused: std::collections::HashSet<String> = Default::default();
    let brand = crate::brand::get();
    loop {
        if d.is_stopping() {
            for (url, (_, task)) in running.drain() {
                task.abort();
                d.hub_status.lock().unwrap().remove(&url);
            }
            return;
        }
        let hubs: Vec<HubLink> = d
            .mgr
            .cfg()
            .hubs
            .into_iter()
            .filter(|h| {
                if brand.hub_allowed(&h.url) {
                    return true;
                }
                set_status(&d, &h.url, "refused: not a hub this build links to");
                if refused.insert(h.url.clone()) {
                    eprintln!("hub link {}: not linked ({} links only to its own service)", h.url, brand.display_name);
                }
                false
            })
            .collect();
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
    // Kept across reconnects: what the HTTP link has received and answered.
    let mut http_state = crate::link_http::State::default();
    loop {
        let Some(link) = d.mgr.cfg().hub(&url).cloned() else { return };
        let (kind, secret) = match (&link.token, &link.pair_code) {
            (Some(t), _) => ("token", t.clone()),
            (None, Some(c)) => ("code", c.clone()),
            _ => {
                set_status(&d, &url, format!("unauthorized: no token; run `{} connect` again", crate::brand::get().cli_name));
                return;
            }
        };
        set_status(&d, &url, "connecting");
        let attempt = match link.transport.as_str() {
            "http" => crate::link_http::connect_once(&d, &link, kind, &secret, &mut http_state).await,
            _ => connect_once(&d, &link, kind, &secret).await,
        };
        // A link that got as far as `connected` dropped, not failed to come
        // up: dial again soon instead of after the delays earlier outages built.
        if d.hub_status.lock().unwrap().get(&url).is_some_and(|s| s == "connected") {
            backoff = 1;
        }
        match attempt {
            // Just paired: `supervise` restarts the link with the new token.
            Ok(()) => return,
            Err(e) if is_unauthorized(&e, kind == "code") => {
                // Revoked or expired: retrying cannot help. Wait for `agl connect`.
                let what = if kind == "code" { "pairing code is invalid or expired" } else { "this machine was revoked" };
                set_status(&d, &url, format!("unauthorized: {what}; run `{} connect` again", crate::brand::get().cli_name));
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

/// Whether the hub refused the credentials, so retrying cannot help. A
/// token is revoked only by a 401: a 403 also comes from WAFs and proxies in
/// front of the hub, and stopping on one would leave the machine offline for
/// good. A pairing code the hub turned down (401 or 403) stays turned down.
fn is_unauthorized(e: &anyhow::Error, pairing: bool) -> bool {
    e.chain().any(|c| {
        c.is::<crate::link_http::Unauthorized>()
            || matches!(c.downcast_ref::<tokio_tungstenite::tungstenite::Error>(),
                Some(tokio_tungstenite::tungstenite::Error::Http(resp)) if refused(resp.status().as_u16(), pairing))
    })
}

/// An HTTP status that ends a link for good (`is_unauthorized`).
pub(crate) fn refused(status: u16, pairing: bool) -> bool {
    status == 401 || (pairing && status == 403)
}

/// Swap the link's pairing code for the token the hub issued. The changed
/// entry restarts the link through `supervise`.
pub(crate) fn store_token(d: &Daemon, hub: &str, token: String, max_body: Option<usize>) -> Result<()> {
    let mut fresh = Config::load()?;
    let Some(entry) = fresh.hubs.iter_mut().find(|h| h.url == hub) else { bail!("hub was removed while pairing") };
    entry.token = Some(token);
    entry.pair_code = None;
    entry.max_body = max_body;
    fresh.save()?;
    *d.mgr.cfg.write().unwrap() = fresh;
    eprintln!("hub link {hub}: paired");
    d.bump_reload();
    Ok(())
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
        return store_token(d, hub, token, None);
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
                    if d.mgr.visible_to(&env.session_id, &origin) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_401_revokes_a_token() {
        assert!(refused(401, false));
        assert!(!refused(403, false), "a WAF's 403 must not stop the link for good");
        assert!(!refused(500, false));
        assert!(refused(401, true));
        assert!(refused(403, true), "a refused pairing code stays refused");
        let revoked = anyhow::Error::new(crate::link_http::Unauthorized).context("POST /machine/poll");
        assert!(is_unauthorized(&revoked, false));
        assert!(!is_unauthorized(&anyhow::anyhow!("POST /machine/poll: HTTP 403 Forbidden"), false));
    }
}
