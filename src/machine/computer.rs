//! Computer use for cloud agent loops (macOS): the `computer_*` built-in
//! tools, their gate, and the helper processes that do the work.
//!
//! One helper (`agl __computer`, see `crate::computer`) runs per product,
//! started through the product's launcher so macOS asks for Accessibility
//! and Screen Recording in the product's name. Calls to one helper are
//! serialized.
//!
//! Who may use it:
//!
//! | Caller               | Without `approvals` | With `approvals: true`                    |
//! |----------------------|---------------------|-------------------------------------------|
//! | local client, `full` | everything          | everything                                |
//! | `ask` / `edit`       | not offered         | each app asks once (`allow_session` keeps it) |
//! | `read_only`          | not offered         | not offered                               |
//!
//! Some apps are never operated (terminals, password managers, System
//! Settings, agl's own launchers), plus `computer.blocked_apps` in the
//! config, which only the person can edit.

use std::collections::{HashMap, HashSet};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::computer::{HELPER_ARG, TOOLS};
use crate::config::Config;
use crate::session::Origin;

/// Longest a helper request may take (opening an app waits for its window).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Largest image handed back (base64 bytes); bigger ones are `omitted`.
const MAX_IMAGE_B64: usize = 4 * 1024 * 1024 * 4 / 3;

/// Bundle ids computer use never operates. A trailing `*` is a prefix.
const BLOCKED: &[&str] = &[
    // Terminals: typing into one would bypass `exec` approvals.
    "com.apple.Terminal",
    "com.googlecode.iterm2",
    "com.mitchellh.ghostty",
    "dev.warp.Warp-Stable",
    "dev.warp.Warp",
    "com.github.wez.wezterm",
    "org.alacritty",
    "net.kovidgoyal.kitty",
    "co.zeit.hyper",
    "com.raphaelamorim.rio",
    // Secrets.
    "com.apple.keychainaccess",
    "com.apple.Passwords",
    "com.1password.1password",
    "com.agilebits.onepassword7",
    "com.bitwarden.desktop",
    "com.dashlane.dashlanephonefinal",
    "com.lastpass.LastPass",
    // Where privacy grants are changed.
    "com.apple.systempreferences",
    "com.apple.SecurityAgent",
    // agl's own product launchers.
    "cc.agl.app.*",
];

pub fn is_tool(name: &str) -> bool {
    TOOLS.contains(&name)
}

fn matches(pattern: &str, id: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => id.to_ascii_lowercase().starts_with(&prefix.to_ascii_lowercase()),
        None => pattern.eq_ignore_ascii_case(id),
    }
}

/// Whether computer use must leave this app alone.
pub fn blocked(cfg: &Config, bundle_id: &str) -> bool {
    if cfg.computer.allowed_blocked_apps.iter().any(|p| matches(p, bundle_id)) {
        return cfg.computer.blocked_apps.iter().any(|p| matches(p, bundle_id));
    }
    BLOCKED.iter().any(|p| matches(p, bundle_id)) || cfg.computer.blocked_apps.iter().any(|p| matches(p, bundle_id))
}

#[derive(Debug, PartialEq)]
pub enum Level {
    Allow,
    /// Each app waits for the person once.
    Ask,
    Deny(String),
}

pub fn level(cfg: &Config, origin: &Origin, approvals: bool) -> Level {
    if !crate::computer::supported() {
        return Level::Deny("computer use needs macOS".into());
    }
    let Origin::Hub { url, .. } = origin else { return Level::Allow };
    let access = cfg.hub(url).map(|h| h.access.clone()).unwrap_or_else(|| "ask".into());
    match access.as_str() {
        "full" => Level::Allow,
        "ask" | "edit" if approvals => Level::Ask,
        a => Level::Deny(format!(
            "computer use needs a `full` link, or `ask` / `edit` with approvals: true (this link is `{a}`; agl connect … --access)"
        )),
    }
}

// ---------- tool definitions ----------

const GUIDE: &str = "Computer use: operate the apps on this Mac the way a person would, in the background, without taking over the person's mouse or keyboard. \
Prefer files, commands or APIs when they can do the job. Start here to see the running apps, then call computer_get_state for the app: it returns the window's \
accessibility tree, where each element has a number like [12], and a screenshot. Act on elements by number (most reliable); use x/y screenshot pixel \
coordinates only for what the tree doesn't show. Every action returns the new state: check that it worked before the next step. Pass the state_id you \
acted on; if the app changed meanwhile you get error stale_state with the current state, and nothing was done. If a background action had no effect, \
retry it with foreground: true (that brings the app to the front and uses the real pointer). Ask the person before anything irreversible: sending \
messages, purchases, deleting, signing in. Some apps (terminals, password managers, System Settings) are blocked. If a call fails with \
stopped_by_user, stop and wait for the person.";

fn common(extra: Value) -> Value {
    let mut p = json!({
        "app": {"type": "string", "description": "bundle_id (preferred) or name from computer_list_apps"},
        "state_id": {"type": "string", "description": "the state_id these element numbers come from"},
        "screenshot": {"type": "boolean", "default": true, "description": "include a screenshot of the window in the result"},
        "return_state": {"type": "boolean", "default": true, "description": "return the state after acting"},
        "window": {"type": "integer", "description": "window id from computer_list_apps (default: the focused window)"},
    });
    if let (Some(p), Some(e)) = (p.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            p.insert(k.clone(), v.clone());
        }
    }
    p
}

fn point() -> Value {
    json!({"type": "object", "properties": {
        "element": {"type": "integer", "minimum": 1},
        "x": {"type": "number"}, "y": {"type": "number"}
    }, "additionalProperties": false})
}

/// `(name, description, properties, required)` for each tool.
pub fn definitions() -> Vec<(&'static str, String, Value, Vec<&'static str>)> {
    let fg = json!({"type": "boolean", "default": false, "description": "bring the app to the front and use the real pointer/keyboard (only if the background way had no effect)"});
    vec![
        ("computer_list_apps", format!("{GUIDE} Returns {{apps: [{{bundle_id, name, pid, windows: [{{id, title, focused}}], blocked}}]}}."), json!({}), vec![]),
        (
            "computer_get_state",
            "Read an app's window: state_id, the accessibility tree as text (elements numbered [n]; flags like focused, selected, disabled, checked), and a screenshot (in images). Coordinates elsewhere are pixels of this screenshot.".into(),
            common(json!({"max_elements": {"type": "integer", "minimum": 50, "maximum": 5000, "default": 1500}})),
            vec!["app"],
        ),
        (
            "computer_click",
            "Click an element (by number, preferred) or a point (x, y in screenshot pixels). Buttons are pressed and rows selected through accessibility, in the background.".into(),
            common(json!({
                "element": {"type": "integer", "minimum": 1},
                "x": {"type": "number"}, "y": {"type": "number"},
                "button": {"type": "string", "enum": ["left", "right", "middle"], "default": "left"},
                "count": {"type": "integer", "minimum": 1, "maximum": 3, "default": 1, "description": "2 for a double click"},
                "foreground": fg,
            })),
            vec!["app"],
        ),
        (
            "computer_type",
            "Type text into an element (focused first) or into the app's focused element. Inserts at the cursor; select first (e.g. computer_key cmd+a) to replace.".into(),
            common(json!({
                "text": {"type": "string"},
                "element": {"type": "integer", "minimum": 1},
                "via": {"type": "string", "enum": ["auto", "keys"], "default": "auto", "description": "keys: send key events instead of inserting through accessibility (for fields that ignored the insert)"},
                "foreground": fg,
            })),
            vec!["app", "text"],
        ),
        (
            "computer_key",
            "Press keys: a combo like \"cmd+l\", \"shift+tab\", \"return\", \"escape\", \"down\", or several separated by spaces (\"cmd+a delete\"). Modifiers: cmd, shift, alt, ctrl, fn.".into(),
            common(json!({"keys": {"type": "string"}, "foreground": fg})),
            vec!["app", "keys"],
        ),
        (
            "computer_scroll",
            "Scroll at an element or point (default: the middle of the window).".into(),
            common(json!({
                "direction": {"type": "string", "enum": ["up", "down", "left", "right"]},
                "amount": {"type": "integer", "minimum": 1, "maximum": 50, "default": 3, "description": "lines"},
                "element": {"type": "integer", "minimum": 1},
                "x": {"type": "number"}, "y": {"type": "number"},
                "foreground": fg,
            })),
            vec!["app", "direction"],
        ),
        (
            "computer_drag",
            "Drag from one element or point to another. Always in the foreground: it moves the real pointer.".into(),
            common(json!({"from": point(), "to": point()})),
            vec!["app", "from", "to"],
        ),
        (
            "computer_set_value",
            "Set an element's value directly: text fields, sliders, steppers, pop-ups. Replaces the whole value.".into(),
            common(json!({"element": {"type": "integer", "minimum": 1}, "value": {"type": ["string", "number", "boolean"]}})),
            vec!["app", "element", "value"],
        ),
        (
            "computer_action",
            "Perform another accessibility action on an element: AXShowMenu, AXIncrement, AXDecrement, AXConfirm, AXCancel, AXRaise, AXPick, AXScrollToVisible (the error lists what it supports).".into(),
            common(json!({"element": {"type": "integer", "minimum": 1}, "action": {"type": "string"}})),
            vec!["app", "element", "action"],
        ),
        (
            "computer_open_app",
            "Start or switch to an app (by bundle_id or name, also apps that aren't running) and return its state. The only tool that brings an app to the front by itself.".into(),
            common(json!({})),
            vec!["app"],
        ),
    ]
}

/// The tool definitions this caller may use, for `tools.list`.
pub fn list(cfg: &Config, origin: &Origin, approvals: bool) -> Vec<Value> {
    let note = match level(cfg, origin, approvals) {
        Level::Deny(_) => return Vec::new(),
        Level::Allow => "",
        Level::Ask => " On this link, the first use of each app waits for the person to approve it, and the result says so (status approval_required).",
    };
    definitions()
        .into_iter()
        .map(|(name, description, properties, required)| {
            let note = if name == "computer_list_apps" { "" } else { note };
            json!({"name": name, "description": format!("{description}{note}"), "input_schema": {
                "type": "object", "properties": properties, "required": required, "additionalProperties": false
            }})
        })
        .collect()
}

// ---------- helpers ----------

struct Helper {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
}

type Slot = Arc<tokio::sync::Mutex<Option<Helper>>>;

/// Helpers per product, and the sessions the person stopped.
#[derive(Default)]
pub struct Computer {
    helpers: tokio::sync::Mutex<HashMap<String, Slot>>,
    stopped: std::sync::Mutex<HashSet<(String, String)>>,
    next: std::sync::atomic::AtomicU64,
}

fn hub_key(origin: &Origin) -> String {
    match origin {
        Origin::Hub { url, .. } => url.clone(),
        Origin::Local => String::new(),
    }
}

fn spawn(app_name: Option<&str>) -> Result<Helper> {
    let exe = std::env::current_exe().context("locate agl")?;
    let (program, pre) = crate::app::wrap(&exe, app_name);
    let mut child = tokio::process::Command::new(program)
        .args(pre)
        .arg(HELPER_ARG)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .context("start the computer-use helper")?;
    let stdin = child.stdin.take().context("helper stdin")?;
    let stdout = BufReader::new(child.stdout.take().context("helper stdout")?).lines();
    Ok(Helper { child, stdin, stdout })
}

impl Computer {
    /// One request to the product's helper, started (again) when needed.
    async fn request(&self, app_name: Option<&str>, mut req: Value) -> Result<Value> {
        let key = app_name.unwrap_or_default().to_string();
        let slot = self.helpers.lock().await.entry(key).or_default().clone();
        let mut guard = slot.lock().await;
        let id = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        req["id"] = json!(id);
        let line = format!("{req}\n");
        for attempt in 0..2 {
            if guard.as_mut().is_none_or(|h| h.child.try_wait().ok().flatten().is_some()) {
                *guard = Some(spawn(app_name)?);
            }
            let h = guard.as_mut().expect("helper");
            if h.stdin.write_all(line.as_bytes()).await.is_err() || h.stdin.flush().await.is_err() {
                // It exited while idle: start it again once.
                *guard = None;
                if attempt == 0 {
                    continue;
                }
                bail!("the computer-use helper could not be started");
            }
            let reply = match tokio::time::timeout(REQUEST_TIMEOUT, h.stdout.next_line()).await {
                Ok(Ok(Some(l))) => l,
                Ok(_) => {
                    *guard = None;
                    if attempt == 0 && req["op"] != "tool" {
                        continue;
                    }
                    bail!("the computer-use helper exited");
                }
                Err(_) => {
                    *guard = None;
                    bail!("the app did not respond within {}s", REQUEST_TIMEOUT.as_secs());
                }
            };
            let v: Value = serde_json::from_str(&reply).context("helper reply")?;
            if v["ok"] == true {
                return Ok(v["result"].clone());
            }
            bail!("{}", v["error"].as_str().unwrap_or("computer use failed"));
        }
        unreachable!()
    }

    fn is_stopped(&self, origin: &Origin, session: &str) -> bool {
        self.stopped.lock().unwrap().contains(&(hub_key(origin), session.to_string()))
    }

    /// `computer.stop {session}`: later calls in that session fail with
    /// stopped_by_user until `computer.reset`.
    pub fn stop(&self, origin: &Origin, params: &Value) -> Value {
        let session = params.get("session").and_then(Value::as_str).unwrap_or_default().to_string();
        let mut s = self.stopped.lock().unwrap();
        if s.len() > 4096 {
            s.clear();
        }
        s.insert((hub_key(origin), session));
        json!({"ok": true})
    }

    pub fn reset(&self, origin: &Origin, params: &Value) -> Value {
        let session = params.get("session").and_then(Value::as_str).unwrap_or_default().to_string();
        self.stopped.lock().unwrap().remove(&(hub_key(origin), session));
        json!({"ok": true})
    }

    /// `computer.permissions {request?}`: whether this product may use
    /// Accessibility and Screen Recording; `request` asks macOS (and opens
    /// System Settings) for what is missing.
    pub async fn permissions(&self, cfg: &Config, origin: &Origin, params: &Value) -> Result<Value> {
        if let Level::Deny(e) = level(cfg, origin, true) {
            bail!(e);
        }
        let request = params.get("request").and_then(Value::as_bool).unwrap_or(false);
        let mut r = self.request(app_name(cfg, origin).as_deref(), json!({"op": "permissions", "args": {"request": request}})).await?;
        r["app_name"] = json!(app_name(cfg, origin).unwrap_or_else(|| "agl".into()));
        Ok(r)
    }
}

fn app_name(cfg: &Config, origin: &Origin) -> Option<String> {
    super::link_app(cfg, origin)
}

/// Options a held call keeps for when it runs.
fn opts(args: &Value) -> (bool, String) {
    let o = args.get("_agl");
    (
        o.and_then(|o| o.get("images")).and_then(Value::as_bool).unwrap_or(false),
        o.and_then(|o| o.get("session")).and_then(Value::as_str).unwrap_or_default().to_string(),
    )
}

/// `tools.call` for a computer tool.
#[allow(clippy::too_many_arguments)]
pub async fn call(
    runtime: &super::Runtime,
    cfg: &Config,
    origin: &Origin,
    name: &str,
    mut args: Value,
    approvals: bool,
    session: &str,
    images: bool,
) -> Result<Value> {
    let lvl = level(cfg, origin, approvals);
    if let Level::Deny(e) = &lvl {
        bail!("{e}");
    }
    let c = &runtime.computer;
    if c.is_stopped(origin, session) {
        bail!("stopped_by_user: the person stopped computer use in this conversation; wait for them");
    }
    let app = app_name(cfg, origin);
    if name == "computer_list_apps" {
        let mut r = c.request(app.as_deref(), json!({"op": "list_apps"})).await?;
        if let Some(apps) = r.get_mut("apps").and_then(Value::as_array_mut) {
            for a in apps {
                let b = blocked(cfg, a["bundle_id"].as_str().unwrap_or_default());
                a["blocked"] = json!(b);
            }
        }
        return Ok(r);
    }
    let query = args.get("app").and_then(Value::as_str).ok_or_else(|| anyhow!("`app` is required"))?.to_string();
    let target = c
        .request(app.as_deref(), json!({"op": "resolve", "args": {"app": query, "installed": name == "computer_open_app"}}))
        .await?;
    let bundle = target["bundle_id"].as_str().unwrap_or_default().to_string();
    let display = target["name"].as_str().unwrap_or(&bundle).to_string();
    if blocked(cfg, &bundle) {
        bail!("app_blocked: computer use never operates {display} ({bundle})");
    }
    // Act on exactly the app that was approved.
    args["app"] = json!(bundle);
    args["_agl"] = json!({"images": images, "session": session, "title": format!("Use {display} ({bundle})")});
    let scope = format!("computer:{bundle}");
    if lvl == Level::Ask && !runtime.approvals.granted(origin, session, &scope) {
        let title = format!("Use {display} ({bundle})");
        return Ok(runtime.approvals.hold(origin, session, name, &scope, args, title));
    }
    run(runtime, cfg, origin, name, args).await
}

/// Runs a computer tool the person approved (`tools.approve`): the link and
/// the blocked list are checked again.
pub async fn approved(runtime: &super::Runtime, cfg: &Config, origin: &Origin, name: &str, args: Value) -> Result<Value> {
    if let Level::Deny(e) = level(cfg, origin, true) {
        bail!(e);
    }
    let (_, session) = opts(&args);
    if runtime.computer.is_stopped(origin, &session) {
        bail!("stopped_by_user: the person stopped computer use in this conversation; wait for them");
    }
    if blocked(cfg, args["app"].as_str().unwrap_or_default()) {
        bail!("app_blocked: computer use never operates this app");
    }
    run(runtime, cfg, origin, name, args).await
}

async fn run(runtime: &super::Runtime, cfg: &Config, origin: &Origin, name: &str, mut args: Value) -> Result<Value> {
    let (images, session) = opts(&args);
    if let Some(o) = args.as_object_mut() {
        o.remove("_agl");
    }
    if name == "computer_open_app" {
        args["bundle_id"] = args["app"].clone();
    }
    let req = json!({"op": "tool", "tool": name, "args": args, "session": format!("{}\u{1}{session}", hub_key(origin))});
    let mut r = runtime.computer.request(app_name(cfg, origin).as_deref(), req).await?;
    shape_images(&mut r, images);
    Ok(r)
}

/// Images go back only to callers that asked for them (`images: true`);
/// others, and images over 4 MB, get `omitted` placeholders.
pub fn shape_images(r: &mut Value, wanted: bool) {
    let Some(list) = r.get_mut("images").and_then(Value::as_array_mut) else { return };
    for img in list {
        let too_big = img["data"].as_str().is_some_and(|d| d.len() > MAX_IMAGE_B64);
        if !wanted || too_big {
            if let Some(o) = img.as_object_mut() {
                o.remove("data");
                o.insert("omitted".into(), json!(true));
            }
        }
    }
    if !wanted {
        r["images_note"] = json!("pass images: true to tools.call to receive screenshots");
    }
}

#[cfg(test)]
mod tests {
    use super::super::approvals::{self, Approvals};
    use super::*;

    #[test]
    fn blocked_apps() {
        let mut cfg = Config::default();
        assert!(blocked(&cfg, "com.apple.Terminal"));
        assert!(blocked(&cfg, "com.apple.terminal"));
        assert!(blocked(&cfg, "cc.agl.app.sumus"));
        assert!(!blocked(&cfg, "com.apple.Notes"));
        cfg.computer.allowed_blocked_apps = vec!["com.apple.Terminal".into()];
        cfg.computer.blocked_apps = vec!["com.tinyspeck.*".into()];
        assert!(!blocked(&cfg, "com.apple.Terminal"));
        assert!(blocked(&cfg, "com.tinyspeck.slackmacgap"));
    }

    #[test]
    fn images_are_opt_in() {
        let mut r = json!({"images": [{"mime_type": "image/jpeg", "data": "abcd"}]});
        shape_images(&mut r, false);
        assert_eq!(r["images"][0], json!({"mime_type": "image/jpeg", "omitted": true}));
        let mut r = json!({"images": [{"mime_type": "image/jpeg", "data": "abcd"}]});
        shape_images(&mut r, true);
        assert_eq!(r["images"][0]["data"], "abcd");
    }

    #[test]
    fn definitions_are_complete() {
        let d = definitions();
        assert_eq!(d.iter().map(|x| x.0).collect::<Vec<_>>(), TOOLS.to_vec());
        for (name, _, props, required) in d {
            for r in required {
                assert!(props.get(r).is_some(), "{name}: {r}");
            }
        }
    }

    #[test]
    fn approvals_scope_per_app() {
        let a = Approvals::default();
        let o = Origin::Hub { url: "https://h".into(), share_local: true };
        let held = a.hold(&o, "s", "computer_click", "computer:com.apple.Notes", json!({"app": "com.apple.Notes"}), "Use Notes".into());
        a.take(&o, held["approval_id"].as_str().unwrap(), "allow_session").unwrap();
        assert!(a.granted(&o, "s", "computer:com.apple.Notes"));
        assert!(!a.granted(&o, "s", "computer:com.apple.Safari"));
        assert!(!a.granted(&o, "s", "computer_click"));
        let _ = approvals::HOLD_FOR;
    }
}
