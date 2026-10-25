//! The hub link over plain HTTP, for backends that cannot hold a WebSocket
//! per machine: several stateless instances behind a load balancer, a WAF,
//! a serverless host. The messages are the WebSocket link's; only the
//! transport differs (protocol.md, "HTTP link").
//!
//! - `POST <hub>/machine/pair {machine_id, code}` → `{token}`
//! - `POST <hub>/machine/send {messages}`         hello, responses, events
//! - `POST <hub>/machine/poll {ack, wait_ms}`     → `{messages, cursor}`: requests
//!
//! Delivery is at least once both ways. The hub redelivers requests until a
//! poll acknowledges their cursor, so a repeated request id is answered from
//! a cache instead of running twice. A batch that failed to send is sent
//! again, so the hub drops responses and events it already has (by `id`, and
//! by `session_id` + `seq`).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc};

use crate::config::HubLink;
use crate::daemon::{dispatch, machine_info, Daemon};
use crate::link::{set_status, store_token};
use crate::protocol::{EventEnvelope, WireMsg};
use crate::session::Origin;

/// How long the hub may hold a poll open.
pub const POLL_WAIT_MS: u64 = 25_000;
/// Limits of one `send` batch.
const BATCH_MAX: usize = 500;
const BATCH_BYTES: usize = 256 * 1024;
/// How long a batch waits for more output once it has some.
const BATCH_WINDOW: Duration = Duration::from_millis(50);
const SEND_ATTEMPTS: u32 = 3;
/// Request ids remembered for redelivery.
const SEEN_CAP: usize = 1024;

/// The hub refused the machine's credentials (401/403): retrying cannot help.
#[derive(Debug)]
pub struct Unauthorized;

impl std::fmt::Display for Unauthorized {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("unauthorized")
    }
}

impl std::error::Error for Unauthorized {}

/// What a link has received and answered; survives reconnects so requests
/// redelivered after one are neither lost nor run twice.
#[derive(Default)]
pub struct State {
    ack: Option<u64>,
    seen: Arc<Mutex<Seen>>,
}

/// Request id -> its reply once there is one (None while it runs).
#[derive(Default)]
struct Seen {
    replies: HashMap<String, Option<WireMsg>>,
    order: VecDeque<String>,
}

impl Seen {
    /// None for a new request (now marked running).
    fn begin(&mut self, id: &str) -> Option<Option<WireMsg>> {
        if let Some(r) = self.replies.get(id) {
            return Some(r.clone());
        }
        self.replies.insert(id.to_string(), None);
        self.order.push_back(id.to_string());
        while self.order.len() > SEEN_CAP {
            if let Some(old) = self.order.pop_front() {
                self.replies.remove(&old);
            }
        }
        None
    }

    fn finish(&mut self, id: &str, reply: &WireMsg) {
        if let Some(slot) = self.replies.get_mut(id) {
            *slot = Some(reply.clone());
        }
    }
}

struct Http {
    client: reqwest::Client,
    base: String,
    machine_id: String,
    token: Option<String>,
}

/// The hub URL as http(s)://, whatever scheme it was given with.
pub fn http_url(hub: &str) -> Result<String> {
    let base = hub.trim_end_matches('/');
    Ok(if let Some(rest) = base.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if let Some(rest) = base.strip_prefix("ws://") {
        format!("http://{rest}")
    } else if base.starts_with("https://") || base.starts_with("http://") {
        base.to_string()
    } else {
        bail!("hub url must start with http(s):// or ws(s)://: {hub}");
    })
}

impl Http {
    fn new(hub: &str, machine_id: &str, token: Option<String>) -> Result<Http> {
        let base = http_url(hub)?;
        // Proxies come from the login shell too (launchd/systemd), not only this environment.
        let mut builder = reqwest::Client::builder().no_proxy().connect_timeout(Duration::from_secs(15));
        if let Some(proxy) = crate::proxy::proxy_for(&crate::proxy::target(&base)?) {
            builder = builder.proxy(reqwest::Proxy::all(&proxy).with_context(|| format!("proxy {proxy}"))?);
        }
        Ok(Http { client: builder.build()?, base, machine_id: machine_id.to_string(), token })
    }

    async fn post(&self, path: &str, body: &Value, timeout: Duration) -> Result<Value> {
        let mut req = self.client.post(format!("{}{path}", self.base)).timeout(timeout).header("x-agl-machine-id", &self.machine_id).json(body);
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        let resp = req.send().await.with_context(|| format!("POST {path}"))?;
        let status = resp.status();
        if matches!(status.as_u16(), 401 | 403) {
            return Err(anyhow::Error::new(Unauthorized).context(format!("POST {path}")));
        }
        if !status.is_success() {
            bail!("POST {path}: HTTP {status}");
        }
        let text = resp.text().await.with_context(|| format!("POST {path}"))?;
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).with_context(|| format!("POST {path}: invalid JSON"))
    }
}

pub async fn connect_once(d: &Arc<Daemon>, link: &HubLink, kind: &str, secret: &str, state: &mut State) -> Result<()> {
    let cfg = d.mgr.cfg();
    let hub = &link.url;
    if kind == "code" {
        let http = Http::new(hub, &cfg.machine_id, None)?;
        let r = http.post("/machine/pair", &json!({"machine_id": cfg.machine_id, "code": secret}), Duration::from_secs(15)).await?;
        let Some(token) = r["token"].as_str() else { bail!("unexpected pairing reply") };
        return store_token(d, hub, token.to_string());
    }
    let http = Http::new(hub, &cfg.machine_id, Some(secret.to_string()))?;
    // Subscribe before `hello` so no event falls in between.
    let bus = d.mgr.bus.subscribe();
    let hello = WireMsg::Hello { machine: machine_info(&cfg).await };
    http.post("/machine/send", &json!({"messages": [hello]}), Duration::from_secs(30)).await?;
    set_status(d, hub, "connected");
    eprintln!("hub link {hub}: connected (http)");

    let origin = Origin::Hub { url: hub.clone(), share_local: link.share_local };
    let (out_tx, out_rx) = mpsc::unbounded_channel::<WireMsg>();
    tokio::select! {
        r = poll_loop(d, &http, &origin, out_tx, state) => r,
        r = send_loop(d, &http, &origin, out_rx, bus) => r,
    }
}

/// Receive the hub's requests and run them. Each poll acknowledges the
/// previous one's messages.
async fn poll_loop(d: &Arc<Daemon>, http: &Http, origin: &Origin, out: mpsc::UnboundedSender<WireMsg>, state: &mut State) -> Result<()> {
    loop {
        let r = http
            .post("/machine/poll", &json!({"ack": state.ack, "wait_ms": POLL_WAIT_MS}), Duration::from_millis(POLL_WAIT_MS + 15_000))
            .await?;
        for m in r["messages"].as_array().into_iter().flatten() {
            let Ok(WireMsg::Request { id, method, params }) = serde_json::from_value::<WireMsg>(m.clone()) else { continue };
            match state.seen.lock().unwrap().begin(&id) {
                // Redelivered: answer again, or let the running one answer.
                Some(Some(reply)) => {
                    let _ = out.send(reply);
                    continue;
                }
                Some(None) => continue,
                None => {}
            }
            let (d, out, origin, seen) = (d.clone(), out.clone(), origin.clone(), state.seen.clone());
            tokio::spawn(async move {
                let reply = WireMsg::reply(id.clone(), dispatch(&d, &method, params, &origin).await);
                seen.lock().unwrap().finish(&id, &reply);
                let _ = out.send(reply);
            });
        }
        if let Some(c) = r["cursor"].as_u64() {
            state.ack = Some(c);
        }
    }
}

/// Post responses and the visible sessions' events in batches, in order.
async fn send_loop(
    d: &Arc<Daemon>,
    http: &Http,
    origin: &Origin,
    mut out: mpsc::UnboundedReceiver<WireMsg>,
    mut bus: broadcast::Receiver<EventEnvelope>,
) -> Result<()> {
    loop {
        let first = next_out(d, origin, &mut out, &mut bus).await?;
        let mut bytes = serde_json::to_string(&first)?.len();
        let mut batch = vec![first];
        // A burst of deltas goes out as one request.
        let window = tokio::time::Instant::now() + BATCH_WINDOW;
        while batch.len() < BATCH_MAX && bytes < BATCH_BYTES {
            match tokio::time::timeout_at(window, next_out(d, origin, &mut out, &mut bus)).await {
                Ok(m) => {
                    let m = m?;
                    bytes += serde_json::to_string(&m)?.len();
                    batch.push(m);
                }
                Err(_) => break,
            }
        }
        let body = json!({ "messages": batch });
        let mut attempt = 1;
        loop {
            match http.post("/machine/send", &body, Duration::from_secs(30)).await {
                Ok(_) => break,
                Err(e) if attempt >= SEND_ATTEMPTS || e.chain().any(|c| c.is::<Unauthorized>()) => return Err(e),
                Err(_) => {
                    tokio::time::sleep(Duration::from_secs(attempt as u64)).await;
                    attempt += 1;
                }
            }
        }
    }
}

async fn next_out(
    d: &Arc<Daemon>,
    origin: &Origin,
    out: &mut mpsc::UnboundedReceiver<WireMsg>,
    bus: &mut broadcast::Receiver<EventEnvelope>,
) -> Result<WireMsg> {
    loop {
        tokio::select! {
            m = out.recv() => return m.context("link closed"),
            ev = bus.recv() => match ev {
                // Only the sessions this hub may see.
                Ok(env) => if d.mgr.origin_of(&env.session_id).map(|o| origin.sees(&o)).unwrap_or(false) {
                    return Ok(WireMsg::Event(env));
                },
                Err(broadcast::error::RecvError::Lagged(n)) => eprintln!("hub link: dropped {n} events"),
                Err(_) => bail!("event bus closed"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hub_urls() {
        assert_eq!(http_url("https://app.example.com/agl/").unwrap(), "https://app.example.com/agl");
        assert_eq!(http_url("wss://app.example.com").unwrap(), "https://app.example.com");
        assert_eq!(http_url("ws://127.0.0.1:8787").unwrap(), "http://127.0.0.1:8787");
        assert!(http_url("app.example.com").is_err());
    }

    #[test]
    fn redelivered_requests_run_once() {
        let mut seen = Seen::default();
        assert!(seen.begin("1").is_none());
        assert!(matches!(seen.begin("1"), Some(None)));
        let reply = WireMsg::Response { id: "1".into(), result: Some(json!(1)), error: None };
        seen.finish("1", &reply);
        assert!(matches!(seen.begin("1"), Some(Some(WireMsg::Response { .. }))));
        for i in 0..SEEN_CAP + 1 {
            seen.begin(&format!("x{i}"));
        }
        assert!(seen.begin("1").is_none(), "the oldest ids are forgotten");
    }
}
