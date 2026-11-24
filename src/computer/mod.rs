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
#[cfg(target_os = "macos")]
mod ui;

/// The argument that runs agl as the computer-use helper.
pub const HELPER_ARG: &str = "__computer";
/// Passed after [`HELPER_ARG`] to a brand's own helper executable
/// (`Brand::computer_helper`): it first makes itself responsible for itself,
/// so macOS attributes Accessibility and Screen Recording to its bundle.
pub const RESPONSIBLE_ARG: &str = "--responsible";

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
    "computer_steps",
    "computer_open_app",
];

/// Whether this build and process can run the helper: agl itself (started
/// through `hold_from_args`), or a brand's own helper executable.
pub fn supported() -> bool {
    cfg!(target_os = "macos") && (crate::app::can_hold() || crate::brand::get().computer_helper.is_some())
}

/// The computer-use helper: reads requests on stdin until it closes or
/// idles. A brand's helper executable calls this from `main` and exits with
/// its code; agl calls it for `agl __computer`.
#[cfg(target_os = "macos")]
pub fn helper_main() -> i32 {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if args.iter().any(|a| a == RESPONSIBLE_ARG) {
        // Started by a host's daemon: take responsibility for this process
        // (same pid and pipes), then serve. Without the flag it serves at once.
        let me = args.first().cloned().unwrap_or_default();
        let rest: Vec<&std::ffi::OsStr> = args.iter().filter(|a| *a != RESPONSIBLE_ARG).map(|a| a.as_os_str()).collect();
        crate::app::reexec_responsible(&me, &rest);
    }
    helper::run()
}

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

    /// AppKit on the main thread for the on-screen indicator (ui.rs); the
    /// requests on a worker thread.
    pub fn run() -> i32 {
        super::ui::run(serve)
    }

    fn serve() -> i32 {
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

    /// Actions `computer_steps` runs, and the most it takes.
    const STEP_TOOLS: &[&str] = &["computer_click", "computer_type", "computer_key", "computer_scroll", "computer_set_value", "computer_action"];
    const MAX_STEPS: usize = 7;
    /// Between two steps: lets the UI take the last one in (a click that opens
    /// a field, then typing into it), as a round trip to the hub used to.
    const BETWEEN_STEPS: Duration = Duration::from_millis(100);

    /// `computer_steps {app, state_id, steps: [{tool, args}]}`: actions on one
    /// window in order, every element number from that state, the window read
    /// once after the last or after the first that failed.
    #[allow(clippy::too_many_arguments)]
    fn steps(
        states: &mut States,
        t: &Target,
        args: &Value,
        operator: &str,
        session: &str,
        turn: Option<&str>,
        window: Option<u32>,
        screenshot: bool,
        max: usize,
    ) -> Result<Value> {
        let list = args.get("steps").and_then(Value::as_array).cloned().unwrap_or_default();
        if list.is_empty() || list.len() > MAX_STEPS {
            bail!("`steps` takes 1 to {MAX_STEPS} actions");
        }
        // Checked before anything runs: a bad step runs none.
        for (i, s) in list.iter().enumerate() {
            let tool = s.get("tool").and_then(Value::as_str).unwrap_or_default();
            if !STEP_TOOLS.contains(&tool) {
                bail!("step {}: `tool` must be one of {}", i + 1, STEP_TOOLS.join(", "));
            }
        }
        let state_id = args.get("state_id").and_then(Value::as_str);
        if states.stale(t, state_id) {
            let mut r = states.snapshot(t, window, screenshot, max)?;
            r["error"] = json!("stale_state");
            r["message"] = json!("The state changed since that state_id; no step was run. Here is the current state: choose again.");
            return Ok(r);
        }
        let mut done: Vec<Value> = Vec::new();
        let mut failed: Option<String> = None;
        for (i, s) in list.iter().enumerate() {
            if super::ui::stopped(session, turn) {
                failed = Some(format!("step {}: stopped_by_user: the person pressed Stop on their screen", i + 1));
                break;
            }
            if i > 0 {
                std::thread::sleep(BETWEEN_STEPS);
            }
            let tool = s["tool"].as_str().unwrap_or_default();
            let mut step_args = s.get("args").cloned().unwrap_or(json!({}));
            step_args["app"] = json!(t.app.bundle_id);
            let name = mac::target_name(states, t, &step_args);
            let step = super::ui::step_text(tool, name.as_deref(), &step_args).map(|w| format!("{}/{} {w}", i + 1, list.len()));
            super::ui::busy(operator, &t.app.name, session, turn, step);
            let outline = mac::target_rect(states, t, tool, &step_args);
            if let Some(r) = outline {
                super::ui::mark(r);
                // The first step shows where the run starts; the next ones
                // follow quickly, still visible.
                std::thread::sleep(if i == 0 { super::ui::OUTLINE_LEAD } else { super::ui::OUTLINE_LEAD / 2 });
            }
            let before_foreground = || mac::wait_for_person(&|| super::ui::stopped(session, turn));
            let acted = mac::act(states, t, tool, &step_args, &before_foreground);
            if outline.is_some() {
                super::ui::unmark();
            }
            match acted {
                Ok(d) => done.push(json!(d)),
                Err(e) => {
                    failed = Some(format!("step {} ({tool}): {e:#}", i + 1));
                    break;
                }
            }
        }
        std::thread::sleep(SETTLE);
        let mut r = states.snapshot(t, window, screenshot, max)?;
        r["steps"] = json!(done);
        if let Some(f) = failed {
            r["step_error"] = json!(f);
        }
        Ok(r)
    }

    fn handle(states: &mut States, req: &Value) -> Result<Value> {
        let op = req.get("op").and_then(Value::as_str).unwrap_or_default();
        let args = req.get("args").cloned().unwrap_or(json!({}));
        let session = req.get("session").and_then(Value::as_str).unwrap_or_default();
        let turn = req.get("turn").and_then(Value::as_str);
        let operator = req.get("operator").and_then(Value::as_str).unwrap_or("agl");
        if op == "tool" && super::ui::stopped(session, turn) {
            bail!("stopped_by_user: the person pressed Stop on their screen; do not use it again in this turn");
        }
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
                    super::ui::busy(operator, &app.name, session, turn, super::ui::step_text(tool, None, &args));
                    let t = Target { session, app: &app };
                    let mut r = states.snapshot(&t, None, screenshot, max)?;
                    r["done"] = json!(format!("opened {}", app.name));
                    return Ok(r);
                }
                let app = app_of(&args)?;
                let t = Target { session, app: &app };
                // The step on the indicator, by the name its element has on screen.
                let name = mac::target_name(states, &t, &args);
                super::ui::busy(operator, &app.name, session, turn, super::ui::step_text(tool, name.as_deref(), &args));
                if tool == "computer_get_state" {
                    return states.snapshot(&t, window, screenshot, max);
                }
                if tool == "computer_steps" {
                    return steps(states, &t, &args, operator, session, turn, window, screenshot, max);
                }
                // An action against an outdated state would hit the wrong thing.
                let state_id = args.get("state_id").and_then(Value::as_str);
                if states.stale(&t, state_id) {
                    let mut r = states.snapshot(&t, window, screenshot, max)?;
                    r["error"] = json!("stale_state");
                    r["message"] = json!("The state changed since that state_id; nothing was done. Here is the current state: choose again.");
                    return Ok(r);
                }
                // What it acts on is outlined a moment first (ui.rs).
                let outline = mac::target_rect(states, &t, tool, &args);
                if let Some(r) = outline {
                    super::ui::mark(r);
                    std::thread::sleep(super::ui::OUTLINE_LEAD);
                }
                let before_foreground = || mac::wait_for_person(&|| super::ui::stopped(session, turn));
                let acted = mac::act(states, &t, tool, &args, &before_foreground);
                if outline.is_some() {
                    super::ui::unmark();
                }
                let done = match acted {
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
