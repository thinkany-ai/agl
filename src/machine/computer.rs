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

use super::images::{Shots, ShotKey};
pub use super::images::shape_images;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::computer::{HELPER_ARG, TOOLS};
use crate::config::Config;
use crate::session::Origin;

/// Longest a helper request may take (opening an app waits for its window).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

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
    // agl's own product launchers (`cc.agl.app.*` in earlier builds).
    "run.agl.app.*",
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
    // A brand's own apps stay blocked whatever the config allows.
    if crate::brand::get().blocked_apps.iter().any(|p| matches(p, bundle_id)) {
        return true;
    }
    if cfg.computer.allowed_blocked_apps.iter().any(|p| matches(p, bundle_id)) {
        return cfg.computer.blocked_apps.iter().any(|p| matches(p, bundle_id));
    }
    BLOCKED.iter().any(|p| matches(p, bundle_id)) || cfg.computer.blocked_apps.iter().any(|p| matches(p, bundle_id))
}

/// The name macOS shows for the process doing computer use without a
/// product launcher: the brand helper's app bundle (`<Name>.app`), else
/// the brand's display name.
fn helper_name() -> String {
    let b = crate::brand::get();
    b.computer_helper
        .as_deref()
        .and_then(|p| p.ancestors().find(|a| a.extension().is_some_and(|e| e == "app")))
        .and_then(|a| a.file_stem())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| b.display_name.clone())
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
    if let Origin::Hub { url, .. } = origin {
        if !cfg.computer_use_on(url) {
            return Level::Deny(format!(
                "computer use is off for this link; the person can turn it on on that computer with `{} computer-use on {url}`",
                crate::brand::get().cli_name
            ));
        }
    }
    link_level(cfg, origin, approvals, "computer use")
}

/// The link rule for tools that act on the person's screen or behalf
/// (computer use, host-registered tools): a `full` link may; `ask` / `edit`
/// only with approvals; `read_only` never. Local clients always may.
pub(super) fn link_level(cfg: &Config, origin: &Origin, approvals: bool, what: &str) -> Level {
    let Origin::Hub { url, .. } = origin else { return Level::Allow };
    let access = cfg.hub(url).map(|h| h.access.clone()).unwrap_or_else(|| "ask".into());
    match access.as_str() {
        "full" => Level::Allow,
        "ask" | "edit" if approvals => Level::Ask,
        a => Level::Deny(format!(
            "{what} needs a `full` link, or `ask` / `edit` with approvals: true (this link is `{a}`; agl connect … --access)"
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

/// Helpers per product, the sessions the person stopped, and the last
/// screenshot sent per window.
#[derive(Default)]
pub struct Computer {
    helpers: tokio::sync::Mutex<HashMap<String, Slot>>,
    stopped: std::sync::Mutex<HashSet<(String, String)>>,
    /// (hub, session, bundle id, window id) → the last screenshot sent.
    shots: Shots,
    next: std::sync::atomic::AtomicU64,
}

/// A helper reply that says a macOS permission is missing (as its error, or
/// as a warning of a result that went without a screenshot).
fn mentions_missing_permission(reply: &Value) -> bool {
    let has = |v: &Value| v.as_str().is_some_and(|s| s.starts_with("permission_missing"));
    has(&reply["error"]) || reply["result"]["warnings"].as_array().is_some_and(|w| w.iter().any(has))
}

pub(super) fn hub_key(origin: &Origin) -> String {
    match origin {
        Origin::Hub { url, .. } => url.clone(),
        Origin::Local => String::new(),
    }
}

fn spawn(app_name: Option<&str>) -> Result<Helper> {
    let (program, pre, post): (std::path::PathBuf, Vec<std::ffi::OsString>, Vec<&str>) = match &crate::brand::get().computer_helper {
        // A brand's own helper app, responsible for itself.
        Some(helper) => (helper.clone(), Vec::new(), vec![crate::computer::RESPONSIBLE_ARG]),
        None => {
            let exe = std::env::current_exe().context("locate agl")?;
            let (program, pre) = crate::app::wrap(&exe, app_name);
            (program, pre, Vec::new())
        }
    };
    let mut child = tokio::process::Command::new(program)
        .args(pre)
        .arg(HELPER_ARG)
        .args(post)
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
            // macOS answers a process's permission checks from what it read
            // when the process started: after a missing permission, the next
            // request starts a new helper, which sees a grant made meanwhile.
            if mentions_missing_permission(&v) {
                *guard = None;
            }
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
        let hub = hub_key(origin);
        // The next screenshot of every window is sent again.
        self.shots.forget(&hub, &session);
        self.stopped.lock().unwrap().remove(&(hub, session));
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
        let app = app_name(cfg, origin, params.get("app_name").and_then(Value::as_str))?;
        let mut r = self.request(app.as_deref(), json!({"op": "permissions", "args": {"request": request}})).await?;
        r["app_name"] = json!(app.unwrap_or_else(helper_name));
        Ok(r)
    }

    /// Sends each screenshot of `r` the way the caller asked (images.rs).
    async fn deliver(&self, origin: &Origin, session: &str, r: &mut Value, upload: Option<&str>) {
        self.shots.deliver(shot_key(origin, session, r), r, upload).await;
    }
}

fn shot_key(origin: &Origin, session: &str, r: &Value) -> ShotKey {
    let bundle = r["app"]["bundle_id"].as_str().unwrap_or_default().to_string();
    let window = match &r["window"]["id"] {
        Value::Null => String::new(),
        v => v.to_string(),
    };
    (hub_key(origin), session.to_string(), bundle, window)
}

/// The product the helper runs under: the link's `app_name`, else the
/// caller's (`tools.call {app_name}`, agl 0.3.17), else none (agl).
fn app_name(cfg: &Config, origin: &Origin, asked: Option<&str>) -> Result<Option<String>> {
    // A brand's own helper serves everything: no product launcher.
    if crate::brand::get().computer_helper.is_some() {
        return Ok(None);
    }
    if let Some(name) = super::link_app(cfg, origin) {
        return Ok(Some(name));
    }
    match asked {
        Some(a) => crate::app::validate(a),
        None => Ok(None),
    }
}

/// How a computer tool call is made, besides its arguments.
#[derive(Default, Clone)]
pub struct CallOpts {
    pub approvals: bool,
    /// The hub's conversation: scopes `allow_session`, state and `unchanged`.
    pub session: String,
    /// Return screenshots (`images: true`).
    pub images: bool,
    /// The product the helper runs under when the link names none.
    pub app_name: Option<String>,
    /// Where to PUT a screenshot instead of returning its bytes.
    pub image_upload: Option<String>,
}

/// Options a held call keeps for when it runs.
fn opts(args: &Value) -> CallOpts {
    let o = args.get("_agl");
    let s = |k: &str| o.and_then(|o| o.get(k)).and_then(Value::as_str).map(str::to_string);
    CallOpts {
        approvals: true,
        session: s("session").unwrap_or_default(),
        images: o.and_then(|o| o.get("images")).and_then(Value::as_bool).unwrap_or(false),
        app_name: s("app_name"),
        image_upload: s("image_upload"),
    }
}

/// `tools.call` for a computer tool.
pub async fn call(runtime: &super::Runtime, cfg: &Config, origin: &Origin, name: &str, mut args: Value, o: CallOpts) -> Result<Value> {
    let session = o.session.as_str();
    let lvl = level(cfg, origin, o.approvals);
    if let Level::Deny(e) = &lvl {
        bail!("{e}");
    }
    let c = &runtime.computer;
    if c.is_stopped(origin, session) {
        bail!("stopped_by_user: the person stopped computer use in this conversation; wait for them");
    }
    let app = app_name(cfg, origin, o.app_name.as_deref())?;
    // A bad upload URL is checked when the screenshot is sent: the call
    // still runs and the screenshot comes inline with a warning.
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
    args["_agl"] = json!({
        "images": o.images,
        "session": session,
        "title": format!("Use {display} ({bundle})"),
        "app_name": app,
        "image_upload": o.image_upload,
    });
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
    let session = opts(&args).session;
    if runtime.computer.is_stopped(origin, &session) {
        bail!("stopped_by_user: the person stopped computer use in this conversation; wait for them");
    }
    if blocked(cfg, args["app"].as_str().unwrap_or_default()) {
        bail!("app_blocked: computer use never operates this app");
    }
    run(runtime, cfg, origin, name, args).await
}

async fn run(runtime: &super::Runtime, cfg: &Config, origin: &Origin, name: &str, mut args: Value) -> Result<Value> {
    let o = opts(&args);
    let session = o.session.as_str();
    if let Some(a) = args.as_object_mut() {
        a.remove("_agl");
    }
    if name == "computer_open_app" {
        args["bundle_id"] = args["app"].clone();
    }
    let req = json!({"op": "tool", "tool": name, "args": args, "session": format!("{}\u{1}{session}", hub_key(origin))});
    let app = app_name(cfg, origin, o.app_name.as_deref())?;
    let mut r = runtime.computer.request(app.as_deref(), req).await?;
    shape_images(&mut r, o.images);
    if o.images {
        runtime.computer.deliver(origin, session, &mut r, o.image_upload.as_deref()).await;
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::super::approvals::{self, Approvals};
    use super::*;

    #[test]
    fn computer_use_is_off_for_a_link_until_the_person_turns_it_on() {
        let url = "https://hub.example/agl";
        let mut cfg = Config::default();
        let mut link = crate::config::HubLink::new(url);
        link.access = "full".into();
        cfg.hubs.push(link);
        let origin = Origin::Hub { url: url.into(), share_local: false };
        assert!(matches!(level(&cfg, &origin, false), Level::Deny(_)));
        assert!(list(&cfg, &origin, false).is_empty());
        cfg.hubs[0].computer_use = Some(true);
        if crate::computer::supported() {
            assert!(matches!(level(&cfg, &origin, false), Level::Allow));
            assert!(!list(&cfg, &origin, false).is_empty());
        }
        // A local client is the person.
        if crate::computer::supported() {
            assert!(matches!(level(&Config::default(), &Origin::Local, false), Level::Allow));
        }
    }

    #[test]
    fn a_missing_permission_restarts_the_helper() {
        assert!(mentions_missing_permission(&json!({"ok": false, "error": "permission_missing: accessibility"})));
        assert!(mentions_missing_permission(&json!({"ok": true, "result": {"warnings": ["permission_missing: screen_recording. No screenshot"]}})));
        assert!(!mentions_missing_permission(&json!({"ok": true, "result": {"warnings": ["window is minimized"]}})));
        assert!(!mentions_missing_permission(&json!({"ok": false, "error": "app_blocked"})));
    }

    #[test]
    fn blocked_apps() {
        let mut cfg = Config::default();
        assert!(blocked(&cfg, "com.apple.Terminal"));
        assert!(blocked(&cfg, "com.apple.terminal"));
        assert!(blocked(&cfg, "run.agl.app.sumus"));
        assert!(blocked(&cfg, "cc.agl.app.sumus"));
        assert!(!blocked(&cfg, "com.apple.Notes"));
        cfg.computer.allowed_blocked_apps = vec!["com.apple.Terminal".into()];
        cfg.computer.blocked_apps = vec!["com.tinyspeck.*".into()];
        assert!(!blocked(&cfg, "com.apple.Terminal"));
        assert!(blocked(&cfg, "com.tinyspeck.slackmacgap"));
    }

    fn shot() -> Value {
        json!({
            "app": {"bundle_id": "com.apple.Notes"},
            "window": {"id": 7},
            "images": [{"mime_type": "image/jpeg", "data": "aGVsbG8="}],
        })
    }

    #[tokio::test]
    async fn unchanged_is_per_window_and_reset_clears_it() {
        let c = Computer::default();
        let o = Origin::Hub { url: "https://h".into(), share_local: true };
        let mut r = shot();
        c.deliver(&o, "s", &mut r, None).await;
        assert_eq!(r["images"][0]["data"], "aGVsbG8=");
        let mut r = shot();
        c.deliver(&o, "s", &mut r, None).await;
        assert_eq!(r["images"][0]["unchanged"], true);
        // Another window, another session: sent.
        let mut r = shot();
        r["window"]["id"] = json!(8);
        c.deliver(&o, "s", &mut r, None).await;
        assert!(r["images"][0]["data"].is_string());
        let mut r = shot();
        c.deliver(&o, "t", &mut r, None).await;
        assert!(r["images"][0]["data"].is_string());
        c.reset(&o, &json!({"session": "s"}));
        let mut r = shot();
        c.deliver(&o, "s", &mut r, None).await;
        assert!(r["images"][0]["data"].is_string());
    }

    #[test]
    fn app_name_from_the_link_or_the_caller() {
        let cfg = Config::default();
        let local = Origin::Local;
        assert_eq!(app_name(&cfg, &local, None).unwrap(), None);
        assert_eq!(app_name(&cfg, &local, Some("Acme")).unwrap().as_deref(), Some("Acme"));
        assert_eq!(app_name(&cfg, &local, Some("agl")).unwrap(), None);
        assert!(app_name(&cfg, &local, Some("a/b")).is_err());
        let o = Origin::Hub { url: "https://h".into(), share_local: true };
        assert_eq!(app_name(&cfg, &o, Some("Acme")).unwrap().as_deref(), Some("Acme"));
        // The link's own app name wins.
        let mut cfg = Config::default();
        let mut link = crate::config::HubLink::new("https://h");
        link.app_name = Some("Linked".into());
        cfg.hubs.push(link);
        assert_eq!(app_name(&cfg, &o, Some("Acme")).unwrap().as_deref(), Some("Linked"));
        assert_eq!(app_name(&cfg, &o, Some("a/b")).unwrap().as_deref(), Some("Linked"));
    }

    #[test]
    fn held_calls_keep_their_options() {
        let args = json!({"app": "x", "_agl": {"images": true, "session": "s", "app_name": "Acme", "image_upload": "https://u"}});
        let o = opts(&args);
        assert!(o.images);
        assert_eq!(o.session, "s");
        assert_eq!(o.app_name.as_deref(), Some("Acme"));
        assert_eq!(o.image_upload.as_deref(), Some("https://u"));
        let o = opts(&json!({"app": "x", "_agl": {"images": false, "session": "s", "app_name": null, "image_upload": null}}));
        assert_eq!((o.app_name, o.image_upload), (None, None));
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
