//! Host-registered tools (agl::machine::provider): listed and called next
//! to the built-ins, under the link rule, with images handled like
//! computer-use screenshots.

use std::sync::{Arc, Mutex, OnceLock};

use agl::config::{Config, HubLink};
use agl::machine::{register_provider, CallContext, Runtime, ToolDef, ToolProvider};
use agl::session::Origin;
use serde_json::{json, Value};

/// An example host's tools: `acme_echo` returns what it got, `acme_shot`
/// returns an image (its data and place from the arguments), `acme_fail`
/// fails.
struct Acme;

impl ToolProvider for Acme {
    fn tools(&self) -> Vec<ToolDef> {
        ["acme_echo", "acme_shot", "acme_fail"]
            .into_iter()
            .map(|n| ToolDef { name: n.into(), description: format!("{n} does a thing."), input_schema: json!({"type": "object"}) })
            .collect()
    }

    fn call<'a>(&'a self, name: &'a str, args: Value, ctx: CallContext) -> agl::machine::provider::BoxFuture<'a, anyhow::Result<Value>> {
        Box::pin(async move {
            match name {
                "acme_echo" => Ok(json!({"args": args, "session": ctx.session, "origin": ctx.origin_key, "images": ctx.images})),
                "acme_shot" => {
                    let mut r = json!({"images": [{"mime_type": "image/jpeg", "data": args["data"].clone()}]});
                    if let Some(k) = args.get("place") {
                        r["image_key"] = k.clone();
                    }
                    Ok(r)
                }
                _ => anyhow::bail!("acme_failed: the thing did not work"),
            }
        })
    }
}

fn setup() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("agl-provider-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("AGL_HOME", &dir);
        register_provider(Arc::new(Acme)).unwrap();
    });
}

fn hub(access: &str) -> (Config, Origin) {
    let mut cfg = Config::default();
    let mut link = HubLink::new("https://hub.example");
    link.access = access.into();
    cfg.hubs.push(link);
    (cfg, Origin::Hub { url: "https://hub.example".into(), share_local: true })
}

async fn call(rt: &Runtime, cfg: &Config, o: &Origin, params: Value) -> anyhow::Result<Value> {
    rt.dispatch(cfg, o, "tools.call", params).await
}

#[test]
fn names_may_not_clash() {
    setup();
    struct One(&'static str);
    impl ToolProvider for One {
        fn tools(&self) -> Vec<ToolDef> {
            vec![ToolDef { name: self.0.into(), description: String::new(), input_schema: json!({"type": "object"}) }]
        }
        fn call<'a>(&'a self, _: &'a str, _: Value, _: CallContext) -> agl::machine::provider::BoxFuture<'a, anyhow::Result<Value>> {
            Box::pin(async { Ok(json!({})) })
        }
    }
    assert!(register_provider(Arc::new(One("read"))).is_err());
    assert!(register_provider(Arc::new(One("computer_click"))).is_err());
    assert!(register_provider(Arc::new(One("acme_echo"))).is_err());
    assert!(register_provider(Arc::new(One(""))).is_err());
}

#[tokio::test]
async fn listed_and_called_like_a_built_in() {
    setup();
    let rt = Runtime::default();
    let local = Origin::Local;
    let cfg = Config::default();
    let list = rt.dispatch(&cfg, &local, "tools.list", json!({})).await.unwrap();
    let names: Vec<&str> = list.as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.ends_with(&["acme_echo", "acme_shot", "acme_fail"]), "{names:?}");
    assert!(names.contains(&"read"));
    assert!(agl::machine::tool_names().contains(&"acme_echo".to_string()));

    // Arguments pass through, the caller's keys included; agl's own are not added.
    let r = call(&rt, &cfg, &local, json!({"name": "acme_echo", "arguments": {"x": 1, "_sites": ["https://a.example"]}, "session": "s1", "images": true}))
        .await
        .unwrap();
    assert_eq!(r, json!({"args": {"x": 1, "_sites": ["https://a.example"]}, "session": "s1", "origin": "", "images": true}));

    // A provider error is the call's error.
    let e = call(&rt, &cfg, &local, json!({"name": "acme_fail", "arguments": {}})).await.unwrap_err();
    assert_eq!(e.to_string(), "acme_failed: the thing did not work");
    assert!(call(&rt, &cfg, &local, json!({"name": "acme_echo", "arguments": []})).await.is_err());
}

#[tokio::test]
async fn the_link_rule_applies() {
    setup();
    let rt = Runtime::default();
    // full: listed and runs.
    let (cfg, o) = hub("full");
    let r = call(&rt, &cfg, &o, json!({"name": "acme_echo", "arguments": {}, "session": "s"})).await.unwrap();
    assert_eq!(r["origin"], "https://hub.example");

    // ask: not without approvals; with them, held once per session.
    let (cfg, o) = hub("ask");
    let list = rt.dispatch(&cfg, &o, "tools.list", json!({})).await.unwrap();
    assert!(!list.to_string().contains("acme_echo"));
    let e = call(&rt, &cfg, &o, json!({"name": "acme_echo", "arguments": {}})).await.unwrap_err();
    assert!(e.to_string().contains("needs a `full` link"), "{e}");
    let list = rt.dispatch(&cfg, &o, "tools.list", json!({"approvals": true})).await.unwrap();
    assert!(list.to_string().contains("waits for the person"));
    let held = call(&rt, &cfg, &o, json!({"name": "acme_echo", "arguments": {"y": 2}, "approvals": true, "session": "s", "images": true})).await.unwrap();
    assert_eq!(held["status"], "approval_required");
    let r = rt
        .dispatch(&cfg, &o, "tools.approve", json!({"approval_id": held["approval_id"], "decision": "allow_session"}))
        .await
        .unwrap();
    // The held call keeps its options and loses agl's.
    assert_eq!(r, json!({"args": {"y": 2}, "session": "s", "origin": "https://hub.example", "images": true}));
    let r = call(&rt, &cfg, &o, json!({"name": "acme_echo", "arguments": {}, "approvals": true, "session": "s"})).await.unwrap();
    assert!(r.get("status").is_none(), "{r}");
    let held = call(&rt, &cfg, &o, json!({"name": "acme_echo", "arguments": {}, "approvals": true, "session": "other"})).await.unwrap();
    assert_eq!(held["status"], "approval_required");

    // read_only: neither listed nor callable.
    let (cfg, o) = hub("read_only");
    let list = rt.dispatch(&cfg, &o, "tools.list", json!({"approvals": true})).await.unwrap();
    assert!(!list.to_string().contains("acme_echo"));
    assert!(call(&rt, &cfg, &o, json!({"name": "acme_echo", "arguments": {}, "approvals": true})).await.is_err());
}

/// A tiny HTTP server on this machine taking PUTs: answers `status`, keeps the bodies.
async fn upload_server(status: u16) -> (String, Arc<Mutex<Vec<Vec<u8>>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}/put?sig=1", listener.local_addr().unwrap().port());
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let kept = bodies.clone();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            let kept = kept.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = s.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let len = text[..end]
                            .lines()
                            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                            .unwrap_or(0);
                        if buf.len() >= end + 4 + len {
                            kept.lock().unwrap().push(buf[end + 4..end + 4 + len].to_vec());
                            let reply = format!("HTTP/1.1 {status} X\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
                            let _ = s.write_all(reply.as_bytes()).await;
                            return;
                        }
                    }
                }
            });
        }
    });
    (url, bodies)
}

#[tokio::test]
async fn images_are_shaped_uploaded_and_deduplicated() {
    setup();
    let rt = Runtime::default();
    let cfg = Config::default();
    let o = Origin::Local;
    let shot = |data: &str, place: Option<&str>, extra: Value| {
        let mut args = json!({"data": data});
        if let Some(p) = place {
            args["place"] = json!(p);
        }
        let mut p = json!({"name": "acme_shot", "arguments": args, "session": "s"});
        for (k, v) in extra.as_object().unwrap() {
            p[k] = v.clone();
        }
        p
    };
    // Not asked for: omitted.
    let r = call(&rt, &cfg, &o, shot("aGVsbG8=", None, json!({}))).await.unwrap();
    assert_eq!(r["images"][0], json!({"mime_type": "image/jpeg", "omitted": true}));
    assert!(r.get("image_key").is_none());
    // Asked for: inline; the same place again: unchanged; another place: inline.
    let r = call(&rt, &cfg, &o, shot("aGVsbG8=", Some("tab-1"), json!({"images": true}))).await.unwrap();
    assert_eq!(r["images"][0]["data"], "aGVsbG8=");
    assert!(r.get("image_key").is_none());
    let r = call(&rt, &cfg, &o, shot("aGVsbG8=", Some("tab-1"), json!({"images": true}))).await.unwrap();
    assert_eq!(r["images"][0], json!({"mime_type": "image/jpeg", "unchanged": true}));
    let r = call(&rt, &cfg, &o, shot("aGVsbG8=", Some("tab-2"), json!({"images": true}))).await.unwrap();
    assert_eq!(r["images"][0]["data"], "aGVsbG8=");

    // Uploaded to the signed URL: no data, its base64 length.
    let (url, bodies) = upload_server(200).await;
    let r = call(&rt, &cfg, &o, shot("d29ybGQ=", Some("tab-3"), json!({"images": true, "image_upload": {"url": url}}))).await.unwrap();
    assert_eq!(r["images"][0], json!({"mime_type": "image/jpeg", "uploaded": true, "bytes": 8}));
    assert_eq!(bodies.lock().unwrap().as_slice(), &[b"world".to_vec()]);

    // A refused upload: inline, with a warning that never repeats the URL.
    let (url, _) = upload_server(403).await;
    let r = call(&rt, &cfg, &o, shot("Zm9v", Some("tab-4"), json!({"images": true, "image_upload": {"url": url}}))).await.unwrap();
    assert_eq!(r["images"][0]["data"], "Zm9v");
    assert_eq!(r["warnings"], json!(["image_upload_failed: HTTP 403"]));
}

#[tokio::test]
async fn an_embedded_daemon_offers_them() {
    setup();
    let d = agl::daemon::spawn_embedded(Config::load().unwrap(), agl::daemon::DaemonOptions { port: Some(0), ..Default::default() })
        .await
        .unwrap();
    let c = agl::client::Client::connect(d.port, &d.token).await.unwrap();
    // The hello hubs get names them.
    let m = c.call("machine.info", json!({})).await.unwrap();
    assert!(m["tools"].as_array().unwrap().iter().any(|t| t == "acme_shot"), "{m}");
    let r = c.call("tools.call", json!({"name": "acme_echo", "arguments": {"z": 3}, "session": "s"})).await.unwrap();
    assert_eq!(r["args"], json!({"z": 3}));
    drop(c);
    d.shutdown().await;
}
