//! Embedding agl in a Rust app (Tauri, GPUI, any tokio program): no daemon,
//! no port, no token.
//!
//! cargo run --example embed -- claude "Reply with one word: hi"

use agl::config::Config;
use agl::protocol::AgentEvent;
use agl::session::{CreateParams, Manager};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let agent = args.next().unwrap_or_else(|| "claude".into());
    let prompt = args.next().unwrap_or_else(|| "Reply with one word: hi".into());

    let mgr = Manager::new(Config::load()?);
    mgr.load();
    let mut events = mgr.bus.subscribe();

    let s = mgr.create(CreateParams {
        agent,
        cwd: Some(std::env::current_dir()?),
        access: Some("read_only".into()),
        name: Some("embed-example".into()),
        ..Default::default()
    })?;
    let turn = mgr.prompt(&s.id, prompt, None, None)?;
    let turn_id = turn["turn_id"].as_str().unwrap_or_default().to_string();

    while let Ok(env) = events.recv().await {
        if env.session_id != s.id {
            continue;
        }
        match env.event {
            AgentEvent::TextDelta { text, .. } => print!("{text}"),
            AgentEvent::PermissionRequest { request_id, .. } => mgr.approve(&s.id, &request_id, "deny").await?,
            AgentEvent::TurnEnded { turn_id: t, outcome, timing, .. } if t == turn_id => {
                println!("\n[{outcome}] {}", timing.unwrap_or_default());
                break;
            }
            _ => {}
        }
    }
    mgr.shutdown_all().await;
    Ok(())
}
