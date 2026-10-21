//! End-to-end session tests against scripted agents (tests/fixtures): the
//! real session manager, drivers and process handling, with no model and no
//! account. Needs python3.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agl::config::{Config, CustomAgent};
use agl::protocol::{AgentEvent, EventEnvelope};
use agl::session::{CreateParams, Manager, Origin};
use serde_json::Value;
use tokio::sync::broadcast::Receiver;

struct Turn {
    outcome: String,
    text: String,
    events: Vec<AgentEvent>,
    timing: Option<Value>,
}

fn fixture(name: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name).display().to_string()
}

fn setup() -> Arc<Manager> {
    let home = std::env::temp_dir().join(format!("agl-test-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&home).unwrap();
    std::env::set_var("AGL_HOME", &home);
    std::env::set_var("AGL_NO_LOGIN_SHELL", "1");
    std::env::set_var("MOCK_STATE_DIR", &home);
    let cfg = Config {
        machine_id: "test-machine".into(),
        approval_timeout_secs: 1,
        custom_agents: vec![
            CustomAgent { id: "mock".into(), name: None, command: fixture("mock_acp.py"), args: vec![], protocol: "acp".into(), env: Default::default() },
            CustomAgent { id: "mockclaude".into(), name: None, command: fixture("mock_claude.py"), args: vec![], protocol: "claude".into(), env: Default::default() },
        ],
        ..Config::default()
    };
    Manager::new(cfg)
}

fn create(mgr: &Arc<Manager>, agent: &str, access: &str, origin: Option<Origin>) -> String {
    let p = CreateParams {
        agent: agent.into(),
        cwd: Some(std::env::temp_dir()),
        access: Some(access.into()),
        origin,
        ..Default::default()
    };
    mgr.create(p).expect("create").id
}

/// Send a prompt and collect its turn. `decision` answers permission requests.
async fn turn(mgr: &Arc<Manager>, rx: &mut Receiver<EventEnvelope>, sid: &str, text: &str, mode: &str, decision: Option<&str>) -> Turn {
    let r = mgr.prompt(sid, text.into(), Some(mode), None).expect("prompt");
    let turn_id = r["turn_id"].as_str().unwrap().to_string();
    let mut t = Turn { outcome: String::new(), text: String::new(), events: vec![], timing: None };
    let mut started = false;
    let fut = async {
        loop {
            let env = rx.recv().await.expect("bus");
            if env.session_id != sid {
                continue;
            }
            match &env.event {
                AgentEvent::TurnStarted { turn_id: id } if *id == turn_id => started = true,
                AgentEvent::TextDelta { text } if started => t.text.push_str(text),
                AgentEvent::PermissionRequest { request_id, .. } => {
                    if let Some(d) = decision {
                        mgr.approve(sid, request_id, d).await.expect("approve");
                    }
                }
                AgentEvent::TurnEnded { turn_id: id, outcome, timing, .. } if *id == turn_id => {
                    t.outcome = outcome.clone();
                    t.timing = timing.clone();
                    t.events.push(env.event.clone());
                    return;
                }
                AgentEvent::Steered { turn_id: id, .. } if *id == turn_id => {
                    t.outcome = "steered".into();
                    return;
                }
                _ => {}
            }
            t.events.push(env.event.clone());
        }
    };
    tokio::time::timeout(Duration::from_secs(30), fut).await.expect("turn timed out");
    t
}

fn has(events: &[AgentEvent], f: impl Fn(&AgentEvent) -> bool) -> bool {
    events.iter().any(f)
}

async fn wait_status(mgr: &Arc<Manager>, sid: &str, status: &str) {
    for _ in 0..100 {
        if mgr.get(sid).unwrap().status == status {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("session {sid} never became {status} (is {})", mgr.get(sid).unwrap().status);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sessions_end_to_end() {
    let mgr = setup();
    let mut rx = mgr.bus.subscribe();

    // --- ACP: warm multi-turn, plan, model readback -------------------------
    let s = create(&mgr, "mock", "full", None);
    let t = turn(&mgr, &mut rx, &s, "hi", "queue", None).await;
    assert_eq!((t.outcome.as_str(), t.text.as_str()), ("completed", "echo: hi"));
    assert!(has(&t.events, |e| matches!(e, AgentEvent::Plan { .. })));
    let t = turn(&mgr, &mut rx, &s, "again", "queue", None).await;
    assert_eq!(t.timing.as_ref().unwrap()["warm"], true, "second turn reuses the process");
    let meta = mgr.get(&s).unwrap();
    assert_eq!((meta.model.as_deref(), meta.current_model.as_deref()), (None, Some("m1")), "readback is not a request");

    // --- ACP approvals: client allow / deny, policy, timeout ----------------
    let ask = create(&mgr, "mock", "ask", None);
    let t = turn(&mgr, &mut rx, &ask, "TOOL", "queue", Some("allow")).await;
    assert_eq!(t.text, "allowed");
    assert!(has(&t.events, |e| matches!(e, AgentEvent::ToolEnd { ok: true, .. })));
    let t = turn(&mgr, &mut rx, &ask, "TOOL", "queue", Some("deny")).await;
    assert_eq!(t.text, "denied");
    let t = turn(&mgr, &mut rx, &ask, "TOOL", "queue", None).await;
    assert_eq!(t.text, "denied", "an unanswered request is denied on timeout");
    assert!(has(&t.events, |e| matches!(e, AgentEvent::PermissionResolved { by, .. } if by == "timeout")));

    let ro = create(&mgr, "mock", "read_only", None);
    let t = turn(&mgr, &mut rx, &ro, "TOOL", "queue", None).await;
    assert_eq!(t.text, "denied", "read_only denies by policy");
    assert!(has(&t.events, |e| matches!(e, AgentEvent::PermissionResolved { by, .. } if by == "policy")));

    // --- Queueing, steer fallback, abort ------------------------------------
    let q = create(&mgr, "mock", "full", None);
    let first = mgr.prompt(&q, "WAIT".into(), None, None).unwrap();
    let t = turn(&mgr, &mut rx, &q, "after", "steer", None).await;
    assert!(has(&t.events, |e| matches!(e, AgentEvent::Queued { .. })), "ACP cannot steer: the prompt queues");
    assert_eq!(t.text, "echo: after");
    assert!(first["turn_id"].is_string());

    let slow = mgr.prompt(&q, "SLOW".into(), None, None).unwrap();
    let slow_id = slow["turn_id"].as_str().unwrap().to_string();
    tokio::time::sleep(Duration::from_millis(600)).await;
    mgr.abort(&q).unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let env = rx.recv().await.unwrap();
            if let AgentEvent::TurnEnded { turn_id, outcome, .. } = env.event {
                if turn_id == slow_id {
                    return outcome;
                }
            }
        }
    })
    .await
    .expect("abort took too long");
    assert_eq!(ended, "aborted");

    // --- Suspension and native resume keep the conversation -----------------
    let r = create(&mgr, "mock", "full", None);
    turn(&mgr, &mut rx, &r, "REMEMBER banana", "queue", None).await;
    mgr.gc(true).await;
    wait_status(&mgr, &r, "suspended").await;
    assert!(!mgr.get(&r).unwrap().live);
    let t = turn(&mgr, &mut rx, &r, "RECALL", "queue", None).await;
    assert_eq!(t.text, "banana", "the respawned process resumed the same session");
    assert_eq!(t.timing.as_ref().unwrap()["warm"], false);

    // --- Hub visibility -----------------------------------------------------
    let a = Origin::Hub { url: "https://a.example".into(), share_local: false };
    let b = Origin::Hub { url: "https://b.example".into(), share_local: true };
    let sa = create(&mgr, "mock", "full", Some(a.clone()));
    assert!(mgr.resolve_for(&sa, &a).is_ok());
    assert!(mgr.resolve_for(&sa, &b).is_err(), "hub B must not see hub A's session");
    assert!(mgr.resolve_for(&sa, &Origin::Local).is_ok());
    assert!(mgr.resolve_for(&s, &b).is_ok(), "share_local exposes local sessions");
    assert!(mgr.resolve_for(&s, &a).is_err(), "private hubs do not see local sessions");

    // --- Claude stream-json driver ------------------------------------------
    let c = create(&mgr, "mockclaude", "ask", None);
    let t = turn(&mgr, &mut rx, &c, "hello", "queue", None).await;
    assert_eq!((t.outcome.as_str(), t.text.as_str()), ("completed", "echo: hello"));
    let t = turn(&mgr, &mut rx, &c, "TOOL", "queue", Some("allow")).await;
    assert_eq!(t.text, "allow");
    assert!(has(&t.events, |e| matches!(e, AgentEvent::ToolStart { name, .. } if name == "Bash")));
    let t = turn(&mgr, &mut rx, &c, "TOOL", "queue", Some("deny")).await;
    assert_eq!(t.text, "deny");
    let cf = create(&mgr, "mockclaude", "full", None);
    let t = turn(&mgr, &mut rx, &cf, "TOOL", "queue", None).await;
    assert_eq!(t.text, "allow", "full access allows by policy");

    // --- Persistence ----------------------------------------------------------
    let history = mgr.history(&r, 0, None).unwrap();
    assert!(history.iter().any(|e| matches!(&e.event, AgentEvent::TextDelta { text } if text == "banana")));

    mgr.shutdown_all().await;
}
