//! macOS: a command run for an `app_name` is attributed to that product's
//! launcher bundle (what privacy prompts name), not to the agl daemon.
#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_agl");

extern "C" {
    fn responsibility_get_pid_responsible_for_pid(pid: libc::pid_t) -> libc::pid_t;
    fn proc_pidpath(pid: libc::c_int, buf: *mut libc::c_void, size: u32) -> libc::c_int;
}

fn exe_of(pid: i32) -> PathBuf {
    let mut buf = vec![0u8; 4096];
    let n = unsafe { proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    buf.truncate(n.max(0) as usize);
    PathBuf::from(String::from_utf8_lossy(&buf).into_owned())
}

fn agl(home: &Path) -> Command {
    let mut c = Command::new(BIN);
    c.env("AGL_HOME", home).env("AGL_NO_LOGIN_SHELL", "1");
    c
}

struct Stop(PathBuf);
impl Drop for Stop {
    fn drop(&mut self) {
        let _ = agl(&self.0).arg("stop").stdout(Stdio::null()).stderr(Stdio::null()).status();
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Run `script` through the daemon's `exec` and return the shell's pid and
/// the executable macOS holds responsible for it, read while it runs.
async fn responsible(c: &agl::client::Client, home: &Path, app_name: Option<&str>) -> (Value, PathBuf) {
    let pid_file = home.join(format!("pid-{}", uuid::Uuid::new_v4().simple()));
    let script = format!("echo $$ > '{}'; sleep 2", pid_file.display());
    let mut params = json!({"argv": ["/bin/sh", "-c", script]});
    if let Some(n) = app_name {
        params["app_name"] = json!(n);
    }
    let call = c.call("exec", params);
    let probe = async {
        for _ in 0..100 {
            if let Some(pid) = std::fs::read_to_string(&pid_file).ok().and_then(|s| s.trim().parse::<i32>().ok()) {
                let r = unsafe { responsibility_get_pid_responsible_for_pid(pid) };
                return exe_of(r);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the command never started");
    };
    let (res, who) = tokio::join!(call, probe);
    (res.expect("exec"), who)
}

#[tokio::test]
async fn exec_for_an_app_is_attributed_to_its_bundle() {
    let home = std::env::temp_dir().join(format!("agl-app-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&home).unwrap();
    let _stop = Stop(home.clone());
    assert!(agl(&home).arg("start").stdout(Stdio::null()).status().unwrap().success());
    let info: Value = serde_json::from_str(&std::fs::read_to_string(home.join("daemon.json")).unwrap()).unwrap();
    let c = agl::client::Client::connect(info["port"].as_u64().unwrap() as u16, info["token"].as_str().unwrap()).await.unwrap();

    let (res, who) = responsible(&c, &home, Some("Sumus Test")).await;
    assert_eq!(res["exit_code"], 0, "{res}");
    let bundle = home.join("apps/Sumus Test.app");
    assert_eq!(who.canonicalize().unwrap(), bundle.join("Contents/MacOS/Sumus Test").canonicalize().unwrap());
    let plist = std::fs::read_to_string(bundle.join("Contents/Info.plist")).unwrap();
    assert!(plist.contains("<string>Sumus Test</string>") && plist.contains("cc.agl.app.sumus-test"));

    // The launcher is reused: same file, so macOS keeps the grants.
    let before = std::fs::metadata(bundle.join("Contents/MacOS/Sumus Test")).unwrap().modified().unwrap();
    let (_, again) = responsible(&c, &home, Some("Sumus Test")).await;
    assert_eq!(again, who);
    assert_eq!(std::fs::metadata(bundle.join("Contents/MacOS/Sumus Test")).unwrap().modified().unwrap(), before);

    // Without a name nothing changes: not attributed to any launcher.
    let (_, plain) = responsible(&c, &home, None).await;
    assert!(!plain.starts_with(home.join("apps")), "{}", plain.display());

    let bad = c.call("exec", json!({"argv": ["/usr/bin/true"], "app_name": "a/b"})).await;
    assert!(bad.is_err());
}
