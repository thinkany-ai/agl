//! The daemon's request dispatcher, in process: what local clients and hub
//! links may call, with a scripted agent and no network. Needs python3 only
//! for agents it runs.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agl::config::Config;
use agl::daemon::{dispatch, Daemon};
use agl::session::{Manager, Origin};
use serde_json::{json, Value};

fn daemon(cfg: Config) -> Arc<Daemon> {
    Arc::new(Daemon {
        mgr: Manager::new(cfg),
        hub_status: Mutex::default(),
        reload: tokio::sync::watch::Sender::new(0),
        shutdown: tokio::sync::Notify::new(),
        token: "test".into(),
        port: Mutex::new(0),
    })
}

fn hub() -> Origin {
    Origin::Hub { url: "https://hub.example".into(), share_local: true }
}

async fn call(d: &Arc<Daemon>, method: &str, params: Value, origin: &Origin) -> Result<Value, String> {
    dispatch(d, method, params, origin).await.map_err(|e| format!("{e:#}"))
}

fn agent<'a>(list: &'a Value, id: &str) -> &'a Value {
    list.as_array().unwrap().iter().find(|a| a["id"] == id).unwrap()
}

#[cfg(unix)]
fn write_script(path: &std::path::Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// One test: the process environment (AGL_HOME, the agent PATH) is global.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatcher() {
    let home = std::env::temp_dir().join(format!("agl-daemon-test-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&home).unwrap();
    std::env::set_var("AGL_HOME", &home);
    std::env::set_var("AGL_NO_LOGIN_SHELL", "1");
    let local = Origin::Local;

    // --- extra_paths and env.reload ---------------------------------------
    // An agent whose CLI is installed while the daemon runs, outside PATH.
    let mut config = json!({
        "machine_id": "test-machine",
        "custom_agents": [{"id": "late", "command": "agl-late-agent", "protocol": "acp"}],
    });
    std::fs::write(home.join("config.json"), config.to_string()).unwrap();
    let d = daemon(Config::load().unwrap());
    let pong = call(&d, "ping", json!({}), &local).await.unwrap();
    assert!(pong["started_at"].as_u64().unwrap() > 0, "restart detection: {pong}");
    let status = call(&d, "daemon.status", json!({}), &local).await.unwrap();
    assert_eq!(status["started_at"], pong["started_at"]);
    let list = call(&d, "agents.list", json!({}), &local).await.unwrap();
    assert_eq!(agent(&list, "late")["available"], false);

    let bin_dir: PathBuf = home.join("app-bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    write_script(&bin_dir.join("agl-late-agent"), "#!/bin/sh\necho late 1.0\n");
    config["extra_paths"] = json!([bin_dir]);
    std::fs::write(home.join("config.json"), config.to_string()).unwrap();
    let list = call(&d, "agents.list", json!({}), &local).await.unwrap();
    assert_eq!(agent(&list, "late")["available"], false, "config.json is read on reload only");

    let err = call(&d, "env.reload", json!({}), &hub()).await.unwrap_err();
    assert!(err.contains("unknown method"), "env.reload is local only: {err}");
    let r = call(&d, "env.reload", json!({}), &local).await.unwrap();
    assert_eq!(r["path"][0], json!(bin_dir), "extra_paths come first");
    let list = call(&d, "agents.list", json!({}), &local).await.unwrap();
    let late = agent(&list, "late");
    assert_eq!(late["available"], true, "{late}");
    assert_eq!(late["path"], json!(bin_dir.join("agl-late-agent")));
    assert_eq!(late["version"], "late 1.0");
    let claude = agent(&list, "claude");
    for f in ["extra_args", "instructions", "ephemeral"] {
        assert!(claude["features"].as_array().unwrap().iter().any(|x| x == f), "claude reports {f}");
    }
    assert!(!agent(&list, "late")["features"].as_array().unwrap().iter().any(|x| x == "instructions"), "ACP agents do not");

    // --- Launch settings are for local connections only --------------------
    let cwd = std::env::temp_dir();
    for field in [json!({"extra_args": ["--x"]}), json!({"instructions": "be brief"}), json!({"ephemeral": true}), json!({"ephemeral": "yes"})] {
        let mut p = json!({"agent": "claude", "cwd": cwd});
        p.as_object_mut().unwrap().extend(field.as_object().unwrap().clone());
        let err = call(&d, "session.create", p, &hub()).await.unwrap_err();
        assert!(err.contains("accepted from local connections only"), "{field}: {err}");
    }
    // Defaults that generic clients send for "not set" are not refused.
    let p = json!({"agent": "claude", "cwd": cwd, "extra_args": [], "instructions": " ", "ephemeral": false, "name": "hub-defaults"});
    let from_hub = call(&d, "session.create", p, &hub()).await.unwrap();
    assert_eq!(from_hub["name"], "hub-defaults");
    call(&d, "session.close", json!({"session_id": "hub-defaults"}), &hub()).await.unwrap();
    let p = json!({"agent": "claude", "cwd": cwd, "extra_args": ["--x"], "instructions": "be brief", "ephemeral": true, "name": "tuned"});
    let created = call(&d, "session.create", p, &local).await.unwrap();
    assert_eq!((created["extra_args"].clone(), created["instructions"].clone(), created["ephemeral"].clone()), (json!(["--x"]), json!("be brief"), json!(true)));
    let seen = call(&d, "session.get", json!({"session_id": "tuned"}), &hub()).await.unwrap();
    assert!(seen["extra_args"].is_null() && seen["instructions"].is_null(), "a hub never sees them: {seen}");
    let listed = call(&d, "session.list", json!({}), &hub()).await.unwrap();
    assert!(listed[0]["instructions"].is_null());
    let saved = std::fs::read_to_string(home.join("sessions.json")).unwrap();
    assert!(saved.contains("be brief"), "kept for restarts");

    // --- Local sessions do not exist for hubs ------------------------------
    let p = json!({"agent": "claude", "cwd": cwd, "name": "private", "visibility": "local"});
    let private = call(&d, "session.create", p, &local).await.unwrap();
    let id = private["id"].as_str().unwrap();
    for key in [id, "private", &id[..6]] {
        let err = call(&d, "session.get", json!({"session_id": key}), &hub()).await.unwrap_err();
        assert_eq!(err, format!("no session `{key}`"), "same answer as for an unknown session");
        let err = call(&d, "session.prompt", json!({"session_id": key, "text": "hi"}), &hub()).await.unwrap_err();
        assert_eq!(err, format!("no session `{key}`"));
    }
    let listed = call(&d, "session.list", json!({}), &hub()).await.unwrap();
    assert!(!listed.as_array().unwrap().iter().any(|m| m["id"] == id));
    let status = call(&d, "daemon.status", json!({}), &hub()).await.unwrap();
    assert_eq!(status["sessions"], 1, "only `tuned`");
    let err = call(&d, "session.create", json!({"agent": "claude", "cwd": cwd, "visibility": "local"}), &hub()).await.unwrap_err();
    assert!(err.contains("cannot create a `local` session"), "{err}");
    assert_eq!(call(&d, "session.list", json!({}), &local).await.unwrap().as_array().unwrap().len(), 2);

    d.mgr.shutdown_all().await;
}
