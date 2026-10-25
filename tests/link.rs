//! End-to-end hub links: a real `agl hub` and a real daemon in their own
//! homes, linked over WebSocket and over HTTP long polling. A browser-side
//! client pairs the machine, drives a scripted agent through the hub, and
//! revokes the machine. Needs python3.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

const BIN: &str = env!("CARGO_BIN_EXE_agl");
const ADMIN: &str = "test-admin-token";
const MACHINE_ID: &str = "11111111-2222-3333-4444-555555555555";

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn temp_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("agl-link-{tag}-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn agl(home: &Path) -> Command {
    let mut c = Command::new(BIN);
    c.env("AGL_HOME", home).env("AGL_NO_LOGIN_SHELL", "1").env("MOCK_STATE_DIR", home).env_remove("HTTPS_PROXY").env_remove("HTTP_PROXY").env_remove("ALL_PROXY");
    c
}

/// Kills the hub and stops the daemon however the test ends.
struct Procs {
    hub: Child,
    machine_home: PathBuf,
}

impl Drop for Procs {
    fn drop(&mut self) {
        let _ = self.hub.kill();
        let _ = agl(&self.machine_home).arg("stop").stdout(Stdio::null()).stderr(Stdio::null()).status();
    }
}

type Ui = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The hub's browser socket.
struct Browser {
    ws: Ui,
    next: u64,
    /// Events seen while waiting for something else.
    events: Vec<Value>,
}

impl Browser {
    async fn recv(&mut self) -> Value {
        let msg = tokio::time::timeout(Duration::from_secs(20), self.ws.next()).await.expect("hub went quiet").expect("hub closed").unwrap();
        let Message::Text(t) = msg else { return Value::Null };
        serde_json::from_str(&t).unwrap()
    }

    async fn call(&mut self, machine_id: Option<&str>, method: &str, params: Value) -> Value {
        self.next += 1;
        let id = self.next.to_string();
        let mut req = json!({"type": "request", "id": id, "method": method, "params": params});
        if let Some(m) = machine_id {
            req["machine_id"] = json!(m);
        }
        self.ws.send(Message::Text(req.to_string().into())).await.unwrap();
        loop {
            let v = self.recv().await;
            if v["type"] == "event" {
                self.events.push(v);
            } else if v["type"] == "response" && v["id"] == id.as_str() {
                assert!(v["error"].is_null(), "{method}: {}", v["error"]);
                return v["result"].clone();
            }
        }
    }

    /// Wait until the machine list shows ours online, or (`online: false`)
    /// offline or gone, as after a revoke.
    async fn machine_online(&mut self, online: bool) -> Value {
        loop {
            let v = self.recv().await;
            if v["type"] == "event" {
                self.events.push(v);
                continue;
            }
            if v["type"] != "machines" {
                continue;
            }
            match v["machines"].as_array().unwrap().iter().find(|m| m["machine_id"] == MACHINE_ID) {
                Some(m) if m["online"] == online => return m.clone(),
                None if !online => return Value::Null,
                _ => {}
            }
        }
    }

    /// Events of `session_id` until its turn ends.
    async fn turn_events(&mut self, session_id: &str) -> Vec<Value> {
        let mut out: Vec<Value> = std::mem::take(&mut self.events).into_iter().filter(|e| e["session_id"] == session_id).collect();
        while !out.iter().any(|e| e["event"]["kind"] == "turn_ended") {
            let v = self.recv().await;
            if v["type"] == "event" && v["session_id"] == session_id {
                out.push(v);
            }
        }
        out
    }
}

async fn linked_machine_runs_a_turn(transport: &str) {
    let hub_home = temp_home("hub");
    let machine_home = temp_home("machine");
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mock_acp.py");
    std::fs::write(
        machine_home.join("config.json"),
        json!({
            "port": free_port(),
            "machine_id": MACHINE_ID,
            "custom_agents": [{"id": "mock", "command": fixture, "protocol": "acp"}],
        })
        .to_string(),
    )
    .unwrap();

    let port = free_port();
    let hub = agl(&hub_home)
        .args(["hub", "--listen", &format!("127.0.0.1:{port}"), "--token", ADMIN, "--release-url", ""])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _procs = Procs { hub, machine_home: machine_home.clone() };
    let ui_url = format!("ws://127.0.0.1:{port}/ws/ui?token={ADMIN}");
    let mut ws = None;
    for _ in 0..100 {
        if let Ok((s, _)) = tokio_tungstenite::connect_async(ui_url.as_str()).await {
            ws = Some(s);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mut ui = Browser { ws: ws.expect("hub did not start"), next: 0, events: vec![] };

    // Pair with a one-time code.
    let code = ui.call(None, "pairing.create", json!({})).await["code"].as_str().unwrap().to_string();
    let out = agl(&machine_home)
        .args(["connect", &format!("http://127.0.0.1:{port}"), "--code", &code, "--transport", transport, "--access", "full"])
        .output()
        .unwrap();
    assert!(out.status.success(), "connect: {}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let machine = ui.machine_online(true).await;
    assert_eq!(machine["transport"], transport);

    // Requests reach the machine and come back.
    let agents = ui.call(Some(MACHINE_ID), "agents.list", json!({})).await;
    assert!(agents.as_array().unwrap().iter().any(|a| a["id"] == "mock" && a["available"] == true), "{agents}");

    // A turn streams its events through the hub, in order and without repeats.
    let meta = ui.call(Some(MACHINE_ID), "session.create", json!({"agent": "mock", "cwd": std::env::temp_dir()})).await;
    let sid = meta["id"].as_str().unwrap().to_string();
    ui.call(Some(MACHINE_ID), "session.prompt", json!({"session_id": sid, "text": "hello"})).await;
    let events = ui.turn_events(&sid).await;
    let seqs: Vec<u64> = events.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "seq out of order or repeated: {seqs:?}");
    let text: String = events.iter().filter(|e| e["event"]["kind"] == "text_delta").map(|e| e["event"]["text"].as_str().unwrap()).collect();
    assert!(!text.is_empty(), "no text: {events:?}");
    let ended = events.iter().find(|e| e["event"]["kind"] == "turn_ended").unwrap();
    assert_eq!(ended["event"]["outcome"], "completed", "{ended}");

    // Revoked: the machine goes offline and stops retrying.
    ui.call(None, "machines.revoke", json!({"machine_id": MACHINE_ID})).await;
    ui.machine_online(false).await;
    let mut status = String::new();
    for _ in 0..100 {
        let out = agl(&machine_home).arg("status").output().unwrap();
        status = String::from_utf8_lossy(&out.stdout).to_string();
        if status.contains("unauthorized") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(status.contains("unauthorized: this machine was revoked"), "{status}");
}

#[tokio::test]
async fn websocket_link() {
    linked_machine_runs_a_turn("ws").await;
}

#[tokio::test]
async fn http_link() {
    linked_machine_runs_a_turn("http").await;
}
