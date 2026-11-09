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
    /// `turn_ended.text`
    answer: Option<String>,
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
    let mut t = Turn { outcome: String::new(), text: String::new(), events: vec![], timing: None, answer: None };
    let mut started = false;
    let fut = async {
        loop {
            let env = rx.recv().await.expect("bus");
            if env.session_id != sid {
                continue;
            }
            match &env.event {
                AgentEvent::TurnStarted { turn_id: id } if *id == turn_id => started = true,
                AgentEvent::TextDelta { text, .. } if started => t.text.push_str(text),
                AgentEvent::PermissionRequest { request_id, .. } => {
                    if let Some(d) = decision {
                        mgr.approve(sid, request_id, d).await.expect("approve");
                    }
                }
                AgentEvent::TurnEnded { turn_id: id, outcome, timing, text, .. } if *id == turn_id => {
                    t.outcome = outcome.clone();
                    t.timing = timing.clone();
                    t.answer = text.clone();
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
    assert_eq!(t.answer.as_deref(), Some("echo: hi"), "turn_ended carries the final answer");
    assert!(has(&t.events, |e| matches!(e, AgentEvent::Plan { .. })));
    let t = turn(&mgr, &mut rx, &s, "ANSWER", "queue", None).await;
    assert!(t.text.starts_with("Looking."));
    assert_eq!(t.answer.as_deref(), Some("Final answer."), "only the text after the last tool call");
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
    let ended_text = mgr.history(&q, 0, None).unwrap().into_iter().find_map(|e| match e.event {
        AgentEvent::TurnEnded { turn_id, text, .. } if turn_id == slow_id => Some(text),
        _ => None,
    });
    assert_eq!(ended_text, Some(None), "an aborted turn has no final answer");

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

    // A `local` session is hidden from every hub, share_local or not.
    let hidden = mgr
        .create(CreateParams { agent: "mock".into(), name: Some("hidden".into()), visibility: Some("local".into()), ..Default::default() })
        .unwrap();
    assert_eq!(hidden.visibility, "local");
    assert!(mgr.resolve_for(&hidden.id, &b).is_err(), "share_local does not expose a local session");
    assert!(mgr.resolve_for("hidden", &b).is_err(), "not by name either");
    assert!(!mgr.visible_to(&hidden.id, &b), "its events are not forwarded");
    assert!(mgr.resolve_for(&hidden.id, &Origin::Local).is_ok());
    let reuse = mgr.create(CreateParams { agent: "mock".into(), name: Some("hidden".into()), origin: Some(b.clone()), ..Default::default() });
    assert!(reuse.unwrap_err().to_string().contains("in use"), "a hub cannot reuse it by name");
    let from_hub = mgr.create(CreateParams { agent: "mock".into(), visibility: Some("local".into()), origin: Some(b.clone()), ..Default::default() });
    assert!(from_hub.unwrap_err().to_string().contains("cannot create a `local` session"));
    assert!(mgr.create(CreateParams { agent: "mock".into(), visibility: Some("secret".into()), ..Default::default() }).is_err());
    assert_eq!(mgr.get(&s).unwrap().visibility, "shared", "the default");

    // --- Claude stream-json driver ------------------------------------------
    let c = create(&mgr, "mockclaude", "ask", None);
    let t = turn(&mgr, &mut rx, &c, "hello", "queue", None).await;
    assert_eq!((t.outcome.as_str(), t.text.as_str()), ("completed", "echo: hello"));
    assert_eq!(t.answer.as_deref(), Some("echo: hello"), "claude's `result`");
    let t = turn(&mgr, &mut rx, &c, "TOOL", "queue", Some("allow")).await;
    assert_eq!(t.text, "allow");
    assert!(has(&t.events, |e| matches!(e, AgentEvent::ToolStart { name, .. } if name == "Bash")));
    let t = turn(&mgr, &mut rx, &c, "TOOL", "queue", Some("deny")).await;
    assert_eq!(t.text, "deny");
    // allow_session covers the session's later requests: agl answers them
    // itself (claude would ask again for every new command).
    let t = turn(&mgr, &mut rx, &c, "TOOL", "queue", Some("allow_session")).await;
    assert_eq!(t.text, "allow");
    assert!(mgr.get(&c).unwrap().allow_session);
    let t = turn(&mgr, &mut rx, &c, "TOOL", "queue", None).await;
    assert_eq!(t.text, "allow", "allowed for the session, not denied on timeout");
    assert!(has(&t.events, |e| matches!(e, AgentEvent::PermissionRequest { decided_by: Some(by), .. } if by == "session")));
    assert!(has(&t.events, |e| matches!(e, AgentEvent::PermissionResolved { by, .. } if by == "session")));
    // A one-off allow grants nothing; changing access drops the grant.
    let c1 = create(&mgr, "mockclaude", "ask", None);
    turn(&mgr, &mut rx, &c1, "TOOL", "queue", Some("allow")).await;
    assert!(!mgr.get(&c1).unwrap().allow_session);
    mgr.configure(&c, agl::session::ConfigureParams { access: Some("edit".into()), ..Default::default() }).unwrap();
    assert!(!mgr.get(&c).unwrap().allow_session);
    let t = turn(&mgr, &mut rx, &c, "TOOL", "queue", None).await;
    assert_eq!(t.text, "deny", "after an access change it asks again (and times out)");

    let cf = create(&mgr, "mockclaude", "full", None);
    let t = turn(&mgr, &mut rx, &cf, "TOOL", "queue", None).await;
    assert_eq!(t.text, "allow", "full access allows by policy");
    assert!(has(&t.events, |e| matches!(e, AgentEvent::PermissionRequest { decided_by: Some(by), .. } if by == "policy")));

    // --- Launch settings: extra_args, instructions, ephemeral ---------------
    let p = CreateParams {
        agent: "mockclaude".into(),
        extra_args: vec!["--custom-flag".into(), "value with spaces; $(not run)".into()],
        instructions: Some("Answer in French.".into()),
        ephemeral: true,
        ..Default::default()
    };
    let custom = mgr.create(p).unwrap();
    assert!(custom.ephemeral && custom.instructions.is_some());
    let argv = |t: &Turn| serde_json::from_str::<Vec<String>>(&t.text).unwrap();
    let args = argv(&turn(&mgr, &mut rx, &custom.id, "ARGS", "queue", None).await);
    let at = |a: &str| args.iter().position(|x| x == a).unwrap_or_else(|| panic!("{a} missing from {args:?}"));
    assert_eq!(args[at("--append-system-prompt") + 1], "Answer in French.");
    at("--no-session-persistence");
    assert_eq!(&args[args.len() - 2..], ["--custom-flag", "value with spaces; $(not run)"], "appended last, verbatim");
    // An ephemeral session never resumes: its process restarts afresh.
    let first_native = mgr.get(&custom.id).unwrap().native_id.unwrap();
    mgr.gc(true).await;
    wait_status(&mgr, &custom.id, "suspended").await;
    let t = turn(&mgr, &mut rx, &custom.id, "ARGS", "queue", None).await;
    let args = argv(&t);
    assert!(!args.iter().any(|a| a == "--resume"), "{args:?}");
    assert_ne!(mgr.get(&custom.id).unwrap().native_id.unwrap(), first_native);
    let lost = mgr.history(&custom.id, 0, None).unwrap();
    assert!(lost.iter().any(|e| matches!(&e.event, AgentEvent::Error { message } if message.contains("without its earlier context"))));

    let acp_args = mgr.create(CreateParams { agent: "mock".into(), extra_args: vec!["--x".into()], ..Default::default() }).unwrap();
    let t = turn(&mgr, &mut rx, &acp_args.id, "ARGS", "queue", None).await;
    assert_eq!(argv(&t), ["--x"], "ACP agents take extra_args too");
    let unsupported = |p: CreateParams| mgr.create(p).unwrap_err().to_string();
    let e = unsupported(CreateParams { agent: "mock".into(), instructions: Some("x".into()), ..Default::default() });
    assert!(e.contains("does not support `instructions`"), "{e}");
    let e = unsupported(CreateParams { agent: "mock".into(), ephemeral: true, ..Default::default() });
    assert!(e.contains("does not support `ephemeral`"), "{e}");
    let e = unsupported(CreateParams { agent: "mockclaude".into(), extra_args: vec!["a".into(); 65], ..Default::default() });
    assert!(e.contains("at most 64"), "{e}");
    let e = unsupported(CreateParams { agent: "mockclaude".into(), extra_args: vec!["a".repeat(5000)], ..Default::default() });
    assert!(e.contains("limit is 4096"), "{e}");
    let e = unsupported(CreateParams { agent: "mockclaude".into(), instructions: Some("a".repeat(40 * 1024)), ..Default::default() });
    assert!(e.contains("limit is 32768"), "{e}");
    let e = unsupported(CreateParams { agent: "mockclaude".into(), ephemeral: true, resume: Some("x".into()), ..Default::default() });
    assert!(e.contains("cannot `resume`"), "{e}");
    let e = unsupported(CreateParams { agent: "mockclaude".into(), extra_args: vec!["--x".into()], origin: Some(b.clone()), ..Default::default() });
    assert!(e.contains("local connections only"), "{e}");

    // --- Persistence ----------------------------------------------------------
    let history = mgr.history(&r, 0, None).unwrap();
    assert!(history.iter().any(|e| matches!(&e.event, AgentEvent::TextDelta { text, .. } if text == "banana")));
    let history_file = |id: &str| agl::config::history_dir().join(format!("{id}.json"));
    assert!(history_file(&r).is_file());

    // history: false keeps events in memory only.
    let p = CreateParams { agent: "mock".into(), history: Some(false), ..Default::default() };
    let nh = mgr.create(p).unwrap().id;
    let t = turn(&mgr, &mut rx, &nh, "secret", "queue", None).await;
    assert_eq!(t.text, "echo: secret");
    assert!(mgr.history(&nh, 0, None).unwrap().iter().any(|e| matches!(&e.event, AgentEvent::UserMessage { .. })), "in memory");
    assert!(!history_file(&nh).exists(), "never written to disk");
    assert!(!std::fs::read_to_string(agl::config::sessions_path()).unwrap().contains("secret"), "prompts are not in sessions.json");
    let last_seq = mgr.get(&nh).unwrap().last_seq;
    assert!(last_seq > 0);

    // After a restart: no history, but numbering goes on.
    mgr.shutdown_all().await;
    let mgr = Manager::new(mgr.cfg());
    mgr.load();
    let meta = mgr.get(&nh).unwrap();
    assert!(!meta.history);
    assert!(mgr.history(&nh, 0, None).unwrap().is_empty());
    assert!(meta.last_seq >= last_seq, "seq continues after a restart");
    assert!(!mgr.history(&r, 0, None).unwrap().is_empty(), "persisted history is back");
    mgr.shutdown_all().await;
}
