//! The hub link over plain HTTP, for backends that cannot hold a WebSocket
//! per machine: several stateless instances behind a load balancer, a WAF,
//! a serverless host. The messages are the WebSocket link's; only the
//! transport differs (protocol.md, "HTTP link").
//!
//! - `POST <hub>/machine/pair {machine_id, code}` → `{token, max_body?}`
//! - `POST <hub>/machine/send {messages}`         hello, responses, events
//! - `POST <hub>/machine/poll {ack, wait_ms}`     → `{messages, cursor, max_body?}`: requests
//!
//! A hub behind a WAF may cap request bodies (`max_body`). Batches stay under
//! it, and a single message too large for it goes as `part` messages.
//!
//! Delivery is at least once both ways. The hub redelivers requests until a
//! poll acknowledges their cursor, so a repeated request id is answered from
//! a cache instead of running twice. A batch that failed to send is sent
//! again, so the hub drops responses and events it already has (by `id`, and
//! by `session_id` + `seq`).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;

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
/// Limits of one `send` batch: messages, and body bytes unless the hub
/// asks for less.
const BATCH_MAX: usize = 500;
const BODY_MAX: usize = 256 * 1024;
/// Below this a hub's limit is not usable.
const BODY_MIN: usize = 1024;
/// `{"messages":[` … `]}`.
const ENVELOPE: usize = 16;
/// A part's own fields around its data.
const PART_OVERHEAD: usize = 160;
/// How long a batch waits for more output once it has some.
const BATCH_WINDOW: Duration = Duration::from_millis(50);
const SEND_ATTEMPTS: u32 = 3;
/// Request ids remembered for redelivery.
const SEEN_CAP: usize = 1024;

/// The hub refused the machine's credentials (`link::refused`): retrying cannot help.
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
        self.post_raw(path, body.to_string(), timeout).await
    }

    async fn post_raw(&self, path: &str, body: String, timeout: Duration) -> Result<Value> {
        let mut req = self
            .client
            .post(format!("{}{path}", self.base))
            .timeout(timeout)
            .header("x-agl-machine-id", &self.machine_id)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body);
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        let resp = req.send().await.with_context(|| format!("POST {path}"))?;
        let status = resp.status();
        // Only pairing goes without a token.
        if crate::link::refused(status.as_u16(), self.token.is_none()) {
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
        return store_token(d, hub, token.to_string(), body_limit(&r));
    }
    let http = Http::new(hub, &cfg.machine_id, Some(secret.to_string()))?;
    let limit = Arc::new(AtomicUsize::new(link.max_body.map_or(BODY_MAX, |n| n.clamp(BODY_MIN, BODY_MAX))));
    // Subscribe before `hello` so no event falls in between.
    let bus = d.mgr.bus.subscribe();
    let hello = WireMsg::Hello { machine: machine_info(&cfg).await };
    // In parts too if it must: a machine with many agents says a lot.
    for body in batches(&mut encode(&hello, limit.load(Ordering::Relaxed))?.into(), limit.load(Ordering::Relaxed)) {
        http.post_raw("/machine/send", body, Duration::from_secs(30)).await?;
    }
    set_status(d, hub, "connected");
    eprintln!("hub link {hub}: connected (http)");

    let origin = Origin::Hub { url: hub.clone(), share_local: link.share_local };
    let (out_tx, out_rx) = mpsc::unbounded_channel::<WireMsg>();
    tokio::select! {
        r = poll_loop(d, &http, &origin, out_tx, state, &limit) => r,
        r = send_loop(d, &http, &origin, out_rx, bus, &limit) => r,
    }
}

/// The request size limit a hub announced, if usable.
fn body_limit(v: &Value) -> Option<usize> {
    v["max_body"].as_u64().map(|n| (n as usize).clamp(BODY_MIN, BODY_MAX))
}

/// A message as JSON, or as parts that each fit a request of `limit` bytes.
fn encode(msg: &WireMsg, limit: usize) -> Result<Vec<String>> {
    let json = serde_json::to_string(msg)?;
    if json.len() + ENVELOPE <= limit {
        return Ok(vec![json]);
    }
    let per_part = (limit - ENVELOPE - PART_OVERHEAD) / 4 * 3;
    let bytes = json.as_bytes();
    let count = bytes.len().div_ceil(per_part);
    let id = uuid::Uuid::new_v4().simple().to_string();
    bytes
        .chunks(per_part)
        .enumerate()
        .map(|(index, chunk)| {
            let part = WireMsg::Part {
                id: id.clone(),
                index: index as u32,
                count: count as u32,
                data: base64::engine::general_purpose::STANDARD.encode(chunk),
            };
            Ok(serde_json::to_string(&part)?)
        })
        .collect()
}

/// Take encoded messages off the front of `queue` into request bodies of at
/// most `limit` bytes (an encoded message always fits one on its own).
fn batches(queue: &mut VecDeque<String>, limit: usize) -> Vec<String> {
    let mut out = Vec::new();
    while !queue.is_empty() {
        out.push(next_batch(queue, limit));
    }
    out
}

fn next_batch(queue: &mut VecDeque<String>, limit: usize) -> String {
    let mut body = String::from("{\"messages\":[");
    let mut taken = 0;
    while let Some(front) = queue.front() {
        if taken > 0 && (taken >= BATCH_MAX || body.len() + front.len() + 1 + 2 > limit) {
            break;
        }
        if taken > 0 {
            body.push(',');
        }
        body.push_str(front);
        queue.pop_front();
        taken += 1;
    }
    body.push_str("]}");
    body
}

/// Receive the hub's requests and run them. Each poll acknowledges the
/// previous one's messages.
async fn poll_loop(d: &Arc<Daemon>, http: &Http, origin: &Origin, out: mpsc::UnboundedSender<WireMsg>, state: &mut State, limit: &AtomicUsize) -> Result<()> {
    loop {
        let r = http
            .post("/machine/poll", &json!({"ack": state.ack, "wait_ms": POLL_WAIT_MS}), Duration::from_millis(POLL_WAIT_MS + 15_000))
            .await?;
        if let Some(n) = body_limit(&r) {
            limit.store(n, Ordering::Relaxed);
        }
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
    limit: &AtomicUsize,
) -> Result<()> {
    let mut queue: VecDeque<String> = VecDeque::new();
    loop {
        let max = limit.load(Ordering::Relaxed);
        if queue.is_empty() {
            let m = next_out(d, origin, &mut out, &mut bus).await?;
            queue.extend(encode(&m, max)?);
        }
        // A burst of deltas goes out as one request.
        let window = tokio::time::Instant::now() + BATCH_WINDOW;
        while queue.len() < BATCH_MAX && queue.iter().map(String::len).sum::<usize>() < max {
            match tokio::time::timeout_at(window, next_out(d, origin, &mut out, &mut bus)).await {
                Ok(m) => queue.extend(encode(&m?, max)?),
                Err(_) => break,
            }
        }
        let body = next_batch(&mut queue, max);
        let mut attempt = 1;
        loop {
            match http.post_raw("/machine/send", body.clone(), Duration::from_secs(30)).await {
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
    fn large_messages_go_in_parts_that_fit() {
        let limit = 2048;
        let text = "x".repeat(10_000);
        let msg = WireMsg::Response { id: "r1".into(), result: Some(json!({"text": text})), error: None };
        let parts = encode(&msg, limit).unwrap();
        assert!(parts.len() > 1);
        let mut joined = Vec::new();
        for (i, p) in parts.iter().enumerate() {
            assert!(p.len() + ENVELOPE <= limit, "part {i} is {} bytes", p.len());
            let Ok(WireMsg::Part { index, count, data, .. }) = serde_json::from_str::<WireMsg>(p) else { panic!("not a part") };
            assert_eq!((index as usize, count as usize), (i, parts.len()));
            joined.extend(base64::engine::general_purpose::STANDARD.decode(data).unwrap());
        }
        assert!(matches!(serde_json::from_slice::<WireMsg>(&joined).unwrap(), WireMsg::Response { .. }));

        let mut queue: VecDeque<String> = parts.into_iter().chain(std::iter::repeat_n("{\"type\":\"x\"}".to_string(), 300)).collect();
        for body in batches(&mut queue, limit) {
            assert!(body.len() <= limit, "batch of {} bytes", body.len());
            serde_json::from_str::<Value>(&body).unwrap();
        }
        assert_eq!(encode(&WireMsg::Paired { token: "t".into() }, limit).unwrap().len(), 1);
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
