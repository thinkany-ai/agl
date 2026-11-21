//! Computer use (macOS): the `__computer` helper process.
//!
//! The daemon starts one helper per product (`app_name`) through that
//! product's launcher (see app.rs), so macOS asks for Accessibility and
//! Screen Recording on behalf of the product, and keeps one grant per
//! product. The helper reads JSON requests on stdin, one per line, and
//! answers each on stdout: `{"id", "ok": true, "result"}` or
//! `{"id", "ok": false, "error"}`. It exits when stdin closes or after
//! `IDLE` without a request; the daemon starts it again on the next call.

pub mod keys;

#[cfg(target_os = "macos")]
mod ffi;
#[cfg(target_os = "macos")]
mod mac;

/// The argument that runs agl as the computer-use helper.
pub const HELPER_ARG: &str = "__computer";

/// Built-in computer tools, in `tools.list` order.
pub const TOOLS: &[&str] = &[
    "computer_list_apps",
    "computer_get_state",
    "computer_click",
    "computer_type",
    "computer_key",
    "computer_scroll",
    "computer_drag",
    "computer_set_value",
    "computer_action",
    "computer_open_app",
];

/// Whether this build and process can run the helper.
pub fn supported() -> bool {
    cfg!(target_os = "macos") && crate::app::can_hold()
}

#[cfg(target_os = "macos")]
pub use helper::run as helper_main;

#[cfg(not(target_os = "macos"))]
pub fn helper_main() -> i32 {
    eprintln!("agl {HELPER_ARG}: computer use needs macOS");
    2
}

#[cfg(target_os = "macos")]
mod helper {
    use std::io::{BufRead, Write};
    use std::sync::mpsc;
    use std::time::Duration;

    use anyhow::{anyhow, bail, Result};
    use serde_json::{json, Value};

    use super::mac::{self, AppInfo, States, Target};

    const IDLE: Duration = Duration::from_secs(10 * 60);
    /// Lets the UI settle before reading the state after an action.
    const SETTLE: Duration = Duration::from_millis(350);

    pub fn run() -> i32 {
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut states = States::default();
        let mut out = std::io::stdout().lock();
        loop {
            let line = match rx.recv_timeout(IDLE) {
                Ok(l) => l,
                Err(_) => return 0,
            };
            if line.trim().is_empty() {
                continue;
            }
            let req: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(e) => {
                    let _ = writeln!(out, "{}", json!({"id": null, "ok": false, "error": format!("bad request: {e}")}));
                    continue;
                }
            };
            let id = req.get("id").cloned().unwrap_or(Value::Null);
            let reply = match handle(&mut states, &req) {
                Ok(result) => json!({"id": id, "ok": true, "result": result}),
                Err(e) => json!({"id": id, "ok": false, "error": format!("{e:#}")}),
            };
            if writeln!(out, "{reply}").and_then(|_| out.flush()).is_err() {
                return 0;
            }
        }
    }

    fn app_of(args: &Value) -> Result<AppInfo> {
        let q = args.get("app").and_then(Value::as_str).ok_or_else(|| anyhow!("`app` is required (a bundle_id or name from computer_list_apps)"))?;
        mac::find_app(q)?.ok_or_else(|| anyhow!("`{q}` is not running with a window; see computer_list_apps, or start it with computer_open_app"))
    }

    fn handle(states: &mut States, req: &Value) -> Result<Value> {
        let op = req.get("op").and_then(Value::as_str).unwrap_or_default();
        let args = req.get("args").cloned().unwrap_or(json!({}));
        let session = req.get("session").and_then(Value::as_str).unwrap_or_default();
        match op {
            "permissions" => Ok(mac::permissions(args.get("request").and_then(Value::as_bool).unwrap_or(false))),
            "list_apps" => mac::list_apps(),
            // Which app a call is for: {bundle_id, name, running}.
            "resolve" => {
                let q = args.get("app").and_then(Value::as_str).ok_or_else(|| anyhow!("`app` is required"))?;
                if let Some(a) = mac::find_app(q)? {
                    return Ok(json!({"bundle_id": a.bundle_id, "name": a.name, "running": true}));
                }
                if args.get("installed").and_then(Value::as_bool).unwrap_or(false) {
                    if let Some((id, name)) = mac::find_installed(q) {
                        return Ok(json!({"bundle_id": id, "name": name, "running": false}));
                    }
                    bail!("no app `{q}` found in /Applications or by bundle id");
                }
                bail!("`{q}` is not running with a window; see computer_list_apps, or start it with computer_open_app")
            }
            "tool" => {
                let tool = req.get("tool").and_then(Value::as_str).unwrap_or_default();
                let screenshot = args.get("screenshot").and_then(Value::as_bool).unwrap_or(true);
                let max = args.get("max_elements").and_then(Value::as_u64).unwrap_or(1500) as usize;
                let window = args.get("window").and_then(Value::as_u64).map(|w| w as u32);
                if tool == "computer_open_app" {
                    let id = args.get("bundle_id").and_then(Value::as_str).ok_or_else(|| anyhow!("missing bundle_id"))?;
                    let app = mac::open_app(id)?;
                    let t = Target { session, app: &app };
                    let mut r = states.snapshot(&t, None, screenshot, max)?;
                    r["done"] = json!(format!("opened {}", app.name));
                    return Ok(r);
                }
                let app = app_of(&args)?;
                let t = Target { session, app: &app };
                if tool == "computer_get_state" {
                    return states.snapshot(&t, window, screenshot, max);
                }
                // An action against an outdated state would hit the wrong thing.
                let state_id = args.get("state_id").and_then(Value::as_str);
                if states.stale(&t, state_id) {
                    let mut r = states.snapshot(&t, window, screenshot, max)?;
                    r["error"] = json!("stale_state");
                    r["message"] = json!("The state changed since that state_id; nothing was done. Here is the current state: choose again.");
                    return Ok(r);
                }
                let done = match mac::act(states, &t, tool, &args) {
                    Ok(d) => d,
                    Err(e) if format!("{e}").starts_with("stale_state") => {
                        let mut r = states.snapshot(&t, window, screenshot, max)?;
                        r["error"] = json!("stale_state");
                        r["message"] = json!(format!("{e}; nothing was done. Here is the current state: choose again."));
                        return Ok(r);
                    }
                    Err(e) => return Err(e),
                };
                if args.get("return_state").and_then(Value::as_bool) == Some(false) {
                    return Ok(json!({"done": done}));
                }
                std::thread::sleep(SETTLE);
                let mut r = states.snapshot(&t, window, screenshot, max)?;
                r["done"] = json!(done);
                Ok(r)
            }
            other => bail!("unknown op `{other}`"),
        }
    }
}
