//! The launcher modes (`__hold`, `__held`, `__computer --responsible`) run
//! under the executable's own privacy grants, so only this home's daemon
//! (the pid in daemon.json) or the same executable may start them.
#![cfg(target_os = "macos")]

use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_agl");

fn home(daemon_pid: Option<u32>) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("agl-caller-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    if let Some(pid) = daemon_pid {
        std::fs::write(dir.join("daemon.json"), format!(r#"{{"pid":{pid},"port":1,"token":"x"}}"#)).unwrap();
    }
    dir
}

fn run(home: &std::path::Path, args: &[&str]) -> (i32, String) {
    let out = Command::new(BIN).env("AGL_HOME", home).args(args).output().unwrap();
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into_owned())
}

#[test]
fn only_the_daemon_starts_a_launcher() {
    // Started by this test process, which is not the home's daemon.
    let other = home(Some(1));
    assert_eq!(run(&other, &["__hold", "/bin/echo", "hi"]).0, 126);
    assert_eq!(run(&other, &["__held", "/bin/echo", "hi"]).0, 126);
    let none = home(None);
    assert_eq!(run(&none, &["__hold", "/bin/echo", "hi"]).0, 126);

    // daemon.json names this process: it may.
    let ours = home(Some(std::process::id()));
    let (code, out) = run(&ours, &["__hold", "/bin/echo", "hi"]);
    assert_eq!((code, out.trim()), (0, "hi"));

    for d in [other, none, ours] {
        let _ = std::fs::remove_dir_all(d);
    }
}
