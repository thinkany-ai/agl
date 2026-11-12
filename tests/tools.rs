//! Built-in tools work without launching any coding agent or calling a model.
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agl::config::{Config, HubLink};
use agl::daemon::{dispatch, Daemon};
use agl::session::{Manager, Origin};
use serde_json::{json, Value};

fn daemon() -> Arc<Daemon> {
    let mut a = HubLink::new("https://a.example");
    a.access = "full".into();
    let mut b = HubLink::new("https://b.example");
    b.access = "edit".into();
    let c = HubLink::new("https://c.example");
    Arc::new(Daemon {
        mgr: Manager::new(Config { hubs: vec![a, b, c], ..Config::default() }),
        tools: Default::default(),
        hub_status: Mutex::default(),
        reload: tokio::sync::watch::Sender::new(0),
        shutdown: Default::default(),
        token: "test".into(),
        port: Mutex::new(0),
    })
}

fn hub(name: &str) -> Origin {
    Origin::Hub { url: format!("https://{name}.example"), share_local: true }
}

async fn call(d: &Arc<Daemon>, method: &str, p: Value, o: &Origin) -> anyhow::Result<Value> {
    tokio::time::timeout(Duration::from_secs(15), dispatch(d, method, p, o)).await.expect("tool request hung")
}

struct Dir(std::path::PathBuf);
impl Dir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("agl-tools-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn tool(d: &Arc<Daemon>, name: &str, arguments: Value) -> anyhow::Result<Value> {
    call(d, "tools.call", json!({"name":name,"arguments":arguments}), &Origin::Local).await
}

async fn completed(d: &Arc<Daemon>, id: &str, o: &Origin) -> Value {
    for _ in 0..100 {
        let r = call(d, "exec.read", json!({"execution_id":id,"wait_ms":100}), o).await.unwrap();
        if r["status"] == "completed" {
            return r;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("execution did not complete")
}

#[tokio::test]
async fn discovery_and_dispatch_enforce_link_permissions() {
    let d = daemon();
    for (origin, names) in [
        (Origin::Local, vec!["read", "write", "edit", "exec"]),
        (hub("a"), vec!["read", "write", "edit", "exec"]),
        (hub("b"), vec!["read", "write", "edit"]),
        (hub("c"), vec!["read"]),
    ] {
        let list = call(&d, "tools.list", json!({}), &origin).await.unwrap();
        assert_eq!(list.as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect::<Vec<_>>(), names);
        assert!(list[0]["input_schema"]["properties"]["path"].is_object());
    }
    for (method, args) in [
        ("tools.call", json!({"name":"exec","arguments":{"command":"true"}})),
        ("exec.start", json!({"command":"true"})),
        ("exec.read", json!({"execution_id":"unknown"})),
        ("exec.cancel", json!({"execution_id":"unknown"})),
        ("fs.edit", json!({"path":"/not-used","old_text":"a","new_text":"b"})),
    ] {
        assert!(call(&d, method, args, &hub("c")).await.unwrap_err().to_string().contains("more access"));
    }
    assert!(tool(&d, "unknown", json!({})).await.is_err());
    assert!(d.mgr.list().is_empty());
}

#[tokio::test]
async fn text_pages_exact_edits_and_legacy_binary_io() {
    let d = daemon();
    let dir = Dir::new();
    let path = dir.0.join("nested/file.txt");
    tool(&d, "write", json!({"path":path,"content":"第一行\nsecond\nlast"})).await.unwrap();
    let page = tool(&d, "read", json!({"path":path,"limit":1})).await.unwrap();
    assert_eq!(page["content"], "第一行\n");
    assert_eq!(page["next_offset"], 2);
    let page = tool(&d, "read", json!({"path":path,"offset":2})).await.unwrap();
    assert_eq!(page["content"], "second\nlast");
    assert_eq!(page["lines"], 2);
    assert!(page["next_offset"].is_null());
    let beyond = tool(&d, "read", json!({"path":path,"offset":99})).await.unwrap();
    assert_eq!(beyond["content"], "");
    assert_eq!(beyond["lines"], 0);
    tool(&d, "edit", json!({"path":path,"old_text":"第一行","new_text":"你好"})).await.unwrap();
    for old in ["", "missing", "\n"] {
        assert!(tool(&d, "edit", json!({"path":path,"old_text":old,"new_text":"oops"})).await.is_err());
    }
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "你好\nsecond\nlast");
    tool(&d, "write", json!({"path":path,"content":"哈哈哈"})).await.unwrap();
    assert!(tool(&d, "edit", json!({"path":path,"old_text":"哈哈","new_text":"x"})).await.is_err(), "overlapping matches are ambiguous");
    let binary = dir.0.join("binary");
    call(&d, "fs.write", json!({"path":binary,"content_base64":"AP8="}), &Origin::Local).await.unwrap();
    let r = call(&d, "fs.read", json!({"path":binary}), &Origin::Local).await.unwrap();
    assert_eq!(r["content_base64"], "AP8=");
    assert!(tool(&d, "read", json!({"path":binary})).await.is_err());
    tool(&d, "write", json!({"path":path,"content":"x".repeat(65537)})).await.unwrap();
    assert!(tool(&d, "read", json!({"path":path})).await.is_err(), "oversized lines fail with a useful error");
    let body = format!("{}\n{}\n", "x".repeat(40000), "y".repeat(40000));
    tool(&d, "write", json!({"path":path,"content":body})).await.unwrap();
    let page = tool(&d, "read", json!({"path":path})).await.unwrap();
    assert_eq!(page["next_offset"], 2);
    assert_eq!(page["content"].as_str().unwrap().len(), 40001);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        tool(&d, "edit", json!({"path":path,"old_text":"y".repeat(40000),"new_text":"z"})).await.unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o700);
        let link = dir.0.join("linked.txt");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        tool(&d, "edit", json!({"path":link,"old_text":"z","new_text":"through link"})).await.unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert!(std::fs::read_to_string(&path).unwrap().contains("through link"));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn executions_stream_deduplicate_and_isolate_owners() {
    let d = daemon();
    let dir = Dir::new();
    let input = json!({"execution_id":"stable-call","command":"echo once >> marker; printf '\\344'; sleep 0.05; printf '\\275\\240\\345\\245\\275'; echo warning >&2; sleep 0.15; exit 3","cwd":dir.0});
    let owner = hub("a");
    let (a, b) = tokio::join!(call(&d, "exec.start", input.clone(), &owner), call(&d, "exec.start", input.clone(), &owner));
    assert_eq!(a.unwrap(), b.unwrap());
    let status = call(&d, "daemon.status", json!({}), &Origin::Local).await.unwrap();
    assert_eq!(status["executions"], 1);
    assert_eq!(d.tools.executions.busy_counts(&hub("b")), (0, 1));
    // A different full-access hub still cannot read or cancel this job.
    d.mgr.cfg.write().unwrap().hubs[1].access = "full".into();
    for method in ["exec.read", "exec.cancel"] {
        assert!(call(&d, method, json!({"execution_id":"stable-call"}), &hub("b")).await.unwrap_err().to_string().contains("no execution"));
    }
    let mut out = String::new();
    let mut err = String::new();
    let (mut o, mut e) = (0, 0);
    loop {
        let r =
            call(&d, "exec.read", json!({"execution_id":"stable-call","stdout_offset":o,"stderr_offset":e,"limit":4,"wait_ms":1000}), &hub("a")).await.unwrap();
        out.push_str(r["stdout"].as_str().unwrap());
        err.push_str(r["stderr"].as_str().unwrap());
        o = r["stdout_offset"].as_u64().unwrap();
        e = r["stderr_offset"].as_u64().unwrap();
        if r["status"] == "completed" && r["has_more"] == false {
            assert_eq!(r["exit_code"], 3);
            break;
        }
    }
    assert_eq!(out, "你好");
    assert_eq!(err, "warning\n");
    call(&d, "exec.start", input, &hub("a")).await.unwrap();
    assert_eq!(std::fs::read_to_string(dir.0.join("marker")).unwrap(), "once\n");
    assert!(call(&d, "exec.start", json!({"execution_id":"stable-call","command":"echo changed"}), &hub("a")).await.is_err());
    assert!(call(&d, "exec.read", json!({"execution_id":"stable-call","stdout_offset":1}), &hub("a")).await.is_err());
    let legacy = call(&d, "exec", json!({"argv":["/bin/sh","-c","printf ok; exit 7"]}), &Origin::Local).await.unwrap();
    assert_eq!(legacy["stdout"], "ok");
    assert_eq!(legacy["exit_code"], 7);
}

#[cfg(unix)]
#[tokio::test]
async fn cancellation_timeout_and_shutdown_end_processes() {
    let d = daemon();
    for (id, timeout) in [("cancel", 10000), ("timeout", 100)] {
        call(&d, "exec.start", json!({"execution_id":id,"command":"sleep 30 & wait","timeout_ms":timeout}), &Origin::Local).await.unwrap();
        if id == "cancel" {
            call(&d, "exec.cancel", json!({"execution_id":id}), &Origin::Local).await.unwrap();
        }
        let r = completed(&d, id, &Origin::Local).await;
        assert_eq!(r["cancelled"], id == "cancel");
        assert_eq!(r["timed_out"], id == "timeout");
    }
    // The root exits immediately but its child's pipe stays open.
    call(&d, "exec.start", json!({"execution_id":"open-pipe","command":"sleep 30 & exit 0","timeout_ms":100}), &Origin::Local).await.unwrap();
    assert_eq!(completed(&d, "open-pipe", &Origin::Local).await["timed_out"], true);
    tool(&d, "exec", json!({"execution_id":"shutdown","command":"sleep 30","wait_ms":0})).await.unwrap();
    d.tools.executions.shutdown().await;
    assert_eq!(completed(&d, "shutdown", &Origin::Local).await["cancelled"], true);
    assert!(tool(&d, "exec", json!({"command":"true"})).await.is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn output_limits_eviction_and_concurrency_are_bounded() {
    let d = daemon();
    let dir = Dir::new();
    let original = json!({"execution_id":"original","command":"echo once >> marker","cwd":dir.0});
    call(&d, "exec.start", original.clone(), &Origin::Local).await.unwrap();
    completed(&d, "original", &Origin::Local).await;
    // More than the buffered-result limit still permits new executions.
    // Old IDs stay as tombstones, so eviction cannot turn a retry into a write.
    for i in 0..20 {
        let id = format!("short-{i}");
        call(&d, "exec.start", json!({"execution_id":id,"command":"true"}), &Origin::Local).await.unwrap();
        completed(&d, &id, &Origin::Local).await;
    }
    assert!(call(&d, "exec.read", json!({"execution_id":"original"}), &Origin::Local).await.unwrap_err().to_string().contains("expired"));
    assert!(call(&d, "exec.start", original, &Origin::Local).await.unwrap_err().to_string().contains("expired"));
    assert_eq!(std::fs::read_to_string(dir.0.join("marker")).unwrap(), "once\n");

    call(&d, "exec.start", json!({"execution_id":"large","argv":["/usr/bin/head","-c","16777316","/dev/zero"]}), &Origin::Local).await.unwrap();
    let r = completed(&d, "large", &Origin::Local).await;
    assert_eq!(r["truncated"], true);
    assert_eq!(r["stdout"].as_str().unwrap().len(), 65536);
    assert_eq!(r["has_more"], true);
    let end = call(&d, "exec.read", json!({"execution_id":"large","stdout_offset":16777216}), &Origin::Local).await.unwrap();
    assert_eq!(end["stdout"], "");
    assert_eq!(end["has_more"], false);

    for i in 0..4 {
        call(&d, "exec.start", json!({"execution_id":format!("long-{i}"),"command":"sleep 30"}), &Origin::Local).await.unwrap();
    }
    assert!(call(&d, "exec.start", json!({"command":"true"}), &Origin::Local).await.unwrap_err().to_string().contains("concurrently"));
    d.tools.executions.shutdown().await;
}
