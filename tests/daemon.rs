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

    d.mgr.shutdown_all().await;
}
