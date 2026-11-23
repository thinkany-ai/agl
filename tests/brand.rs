//! A product shipping agl under its own name: brand set once per process
//! (this test binary), its home, hubs, command line and an embedded daemon.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use agl::brand::{self, Brand, Product};

fn home() -> &'static PathBuf {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("agl-brand-test-{}", uuid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        // The brand's home applies only without AGL_HOME.
        std::env::remove_var("AGL_HOME");
        brand::set(Brand {
            cli_name: "example-link".into(),
            display_name: "Example Link".into(),
            home: Some(dir.clone()),
            service_label: "com.example.link".into(),
            systemd_unit: "example-link.service".into(),
            releases: None,
            allowed_hubs: Some(vec!["http://127.0.0.1:9".into()]),
            computer_helper: None,
            app_launcher: None,
            service: None,
            blocked_apps: vec!["com.example.desktop".into()],
            product: Some(Product { name: "example-desktop".into(), version: "9.1.0".into() }),
            computer_use_default: true,
            isolated_agents_default: false,
            hub_instructions_default: false,
        })
        .unwrap();
        dir
    })
}

fn uuid_like() -> String {
    format!("{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos())
}

#[test]
fn the_brand_names_the_command_line() {
    home();
    let mut cmd = agl::cli::command();
    assert_eq!(cmd.get_name(), "example-link");
    assert_eq!(cmd.get_version(), Some("9.1.0"));
    let help = cmd.render_help().to_string();
    assert!(help.contains("example-link"), "{help}");
    assert!(help.contains("Example Link"), "{help}");
    assert!(!help.contains("Usage: agl"), "{help}");
    // Set once.
    assert!(brand::set(Brand::default()).is_err());
}

#[test]
fn computer_use_follows_the_brand_unless_the_link_says() {
    home();
    let mut cfg = agl::config::Config::default();
    let mut link = agl::config::HubLink::new("http://127.0.0.1:9/hub");
    cfg.hubs.push(link.clone());
    assert!(cfg.computer_use_on("http://127.0.0.1:9/hub"));
    link.computer_use = Some(false);
    cfg.hubs = vec![link];
    assert!(!cfg.computer_use_on("http://127.0.0.1:9/hub"));
}

#[test]
fn the_home_is_the_brands() {
    let dir = home();
    assert_eq!(&agl::config::home(), dir);
    assert_eq!(agl::config::config_path(), dir.join("config.json"));
}

#[test]
fn updates_come_with_the_app() {
    home();
    let rt = tokio::runtime::Runtime::new().unwrap();
    std::env::remove_var("AGL_RELEASES_BASE");
    let e = rt.block_on(agl::update::check()).unwrap_err().to_string();
    assert!(e.contains("Example Link is updated with the app"), "{e}");
}

#[test]
fn embedded_daemon_links_only_allowed_hubs() {
    let dir = home().clone();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let cfg = agl::config::Config::load().unwrap();
        let d = agl::daemon::spawn_embedded(cfg, agl::daemon::DaemonOptions { port: Some(0), ..Default::default() }).await.unwrap();
        assert!(d.is_running());
        assert_eq!(d.busy(), 0);
        let info: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("daemon.json")).unwrap()).unwrap();
        assert_eq!(info["port"], d.port);

        // The local API answers, and hello tells hubs which product this is.
        let c = agl::client::Client::connect(d.port, &d.token).await.unwrap();
        let m = c.call("machine.info", serde_json::json!({})).await.unwrap();
        assert_eq!(m["product"], serde_json::json!({"name": "example-desktop", "version": "9.1.0"}));

        // Another hub is refused before anything is written.
        let e = agl::link::connect("https://other.example.com/x", agl::link::ConnectOptions { code: Some("c".into()), ..Default::default() }).await.unwrap_err();
        assert!(e.to_string().contains("links only to its own service"), "{e}");
        assert!(agl::config::Config::load().unwrap().hubs.is_empty());

        // An allowed hub is saved and the running daemon picks it up (port 9
        // refuses: nothing leaves this machine).
        agl::link::connect("http://127.0.0.1:9/w/1/", agl::link::ConnectOptions { code: Some("c".into()), access: Some("full".into()), ..Default::default() })
            .await
            .unwrap();
        let cfg = agl::config::Config::load().unwrap();
        assert_eq!(cfg.hubs.len(), 1);
        assert_eq!(cfg.hubs[0].url, "http://127.0.0.1:9/w/1");
        assert_eq!(cfg.hubs[0].access, "full");
        tokio::time::sleep(Duration::from_millis(300)).await;
        let st = c.call("daemon.status", serde_json::json!({})).await.unwrap();
        assert_eq!(st["hubs"][0]["url"], "http://127.0.0.1:9/w/1");

        // A hub outside the list that is already in the config is not linked.
        let mut cfg = agl::config::Config::load().unwrap();
        cfg.hubs.push(agl::config::HubLink::new("https://legacy.example.org/hub"));
        cfg.save().unwrap();
        c.call("hub.reload", serde_json::json!({})).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let st = c.call("daemon.status", serde_json::json!({})).await.unwrap();
        let legacy = st["hubs"].as_array().unwrap().iter().find(|h| h["url"] == "https://legacy.example.org/hub").unwrap();
        assert!(legacy["status"].as_str().unwrap().starts_with("refused"), "{legacy}");

        assert!(agl::link::disconnect("http://127.0.0.1:9/w/1").await.unwrap());
        assert!(!agl::link::disconnect("http://127.0.0.1:9/w/1").await.unwrap());
        drop(c);

        // One daemon per home.
        let again = agl::daemon::spawn_embedded(agl::config::Config::load().unwrap(), agl::daemon::DaemonOptions { port: Some(0), ..Default::default() }).await;
        assert!(again.is_err());

        d.shutdown().await;
        assert!(!dir.join("daemon.json").exists());
    });
    let _ = std::fs::remove_dir_all(&dir);
}
