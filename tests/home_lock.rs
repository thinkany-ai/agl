//! One daemon per home: the lock a daemon holds while it serves its home.

#![cfg(unix)]

use std::time::Duration;

use agl::daemon::HomeLock;

fn home() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("agl-lock-test-{}", uuid::Uuid::new_v4().simple()))
}

#[tokio::test]
async fn a_home_has_one_daemon_at_a_time() {
    let home = home();
    let first = HomeLock::acquire(&home, Duration::ZERO).await.unwrap().expect("a free home");
    // A second daemon for the same home does not start…
    assert!(HomeLock::acquire(&home, Duration::from_millis(300)).await.unwrap().is_none());
    // …while another home is unaffected.
    assert!(HomeLock::acquire(&self::home(), Duration::ZERO).await.unwrap().is_some());
    drop(first);
    assert!(HomeLock::acquire(&home, Duration::ZERO).await.unwrap().is_some());
}

#[tokio::test]
async fn a_restart_waits_for_the_daemon_that_is_ending() {
    let home = home();
    let old = HomeLock::acquire(&home, Duration::ZERO).await.unwrap().unwrap();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        drop(old);
    });
    assert!(HomeLock::acquire(&home, Duration::from_secs(3)).await.unwrap().is_some());
}
