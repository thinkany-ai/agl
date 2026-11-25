//! macOS computer use: apps and windows, the accessibility tree, window
//! screenshots and input. Runs in the `__computer` helper process, so the
//! privacy grants (Accessibility, Screen Recording) belong to the product's
//! launcher (see app.rs), not to the agl daemon.

#![allow(non_upper_case_globals)]

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde_json::{json, Value};

use super::ffi::*;
use super::keys;

/// Long edge of a screenshot handed to the model.
const MAX_IMAGE_EDGE: u32 = 1280;
const AX_TIMEOUT_SECS: f32 = 3.0;
/// Nodes visited per tree, included or not.
const MAX_VISITED: usize = 20_000;
const MAX_DEPTH: usize = 60;
const MAX_TREE_BYTES: usize = 96 * 1024;
const MAX_TEXT: usize = 200;

// ---------- permissions ----------

pub fn accessibility(prompt: bool) -> bool {
    // SAFETY: plain calls with a dictionary we own.
    unsafe {
        if prompt {
            let d = Cf::dict(&[(kAXTrustedCheckOptionPrompt, kCFBooleanTrue)]);
            AXIsProcessTrustedWithOptions(d.0) != 0
        } else {
            AXIsProcessTrustedWithOptions(std::ptr::null()) != 0
        }
    }
}

pub fn screen_recording(request: bool) -> bool {
    // SAFETY: plain calls.
    unsafe {
        if CGPreflightScreenCaptureAccess() {
            return true;
        }
        if request {
            return CGRequestScreenCaptureAccess();
        }
        false
    }
}

pub fn permissions(request: bool) -> Value {
    let ax = accessibility(request);
    // One at a time: a Screen Recording request made while the Accessibility
    // prompt is up can be lost, and macOS asks for it only once. Asked after
    // Accessibility is granted, it adds this app to the Screen Recording list.
    let sr = screen_recording(request && ax);
    if request {
        let pane = if !ax {
            Some("Privacy_Accessibility")
        } else if !sr {
            Some("Privacy_ScreenCapture")
        } else {
            None
        };
        if let Some(p) = pane {
            let _ = std::process::Command::new("/usr/bin/open")
                .arg(format!("x-apple.systempreferences:com.apple.preference.security?{p}"))
                .status();
        }
    }
    let s = |b: bool| if b { "granted" } else { "denied" };
    json!({"accessibility": s(ax), "screen_recording": s(sr)})
}

fn need_accessibility() -> Result<()> {
    if !accessibility(false) {
        bail!("permission_missing: accessibility. Grant this app Accessibility in System Settings → Privacy & Security (computer.permissions {{request: true}} opens it).");
    }
    Ok(())
}

// ---------- AX helpers ----------

struct Attrs {
    names: Cf,
}

const A_ROLE: usize = 0;
const A_SUBROLE: usize = 1;
const A_TITLE: usize = 2;
const A_DESC: usize = 3;
const A_VALUE: usize = 4;
const A_ENABLED: usize = 5;
const A_FOCUSED: usize = 6;
const A_SELECTED: usize = 7;
const A_POSITION: usize = 8;
const A_SIZE: usize = 9;
const A_CHILDREN: usize = 10;
const A_PLACEHOLDER: usize = 11;
const ATTR_NAMES: [&str; 12] = [
    "AXRole",
    "AXSubrole",
    "AXTitle",
    "AXDescription",
    "AXValue",
    "AXEnabled",
    "AXFocused",
    "AXSelected",
    "AXPosition",
    "AXSize",
    "AXChildren",
    "AXPlaceholderValue",
];

impl Attrs {
    fn new() -> Self {
        let strs: Vec<Cf> = ATTR_NAMES.iter().map(|s| Cf::str(s)).collect();
        let raw: Vec<CFTypeRef> = strs.iter().map(|c| c.0).collect();
        Attrs { names: Cf::array(&raw) }
    }
}

fn copy_attr(el: &Cf, name: &str) -> Result<Option<Cf>, AXError> {
    let key = Cf::str(name);
    let mut out: CFTypeRef = std::ptr::null();
    // SAFETY: el is a live AXUIElement; out receives a +1 reference.
    let err = unsafe { AXUIElementCopyAttributeValue(el.0, key.0, &mut out) };
    match err {
        kAXErrorSuccess => Ok(Cf::owned(out)),
        e => Err(e),
    }
}

fn attr(el: &Cf, name: &str) -> Option<Cf> {
    copy_attr(el, name).ok().flatten()
}

fn attr_string(el: &Cf, name: &str) -> Option<String> {
    attr(el, name).and_then(|v| string(v.0))
}

fn set_attr(el: &Cf, name: &str, value: CFTypeRef) -> AXError {
    let key = Cf::str(name);
    // SAFETY: live element, key and value.
    unsafe { AXUIElementSetAttributeValue(el.0, key.0, value) }
}

fn settable(el: &Cf, name: &str) -> bool {
    let key = Cf::str(name);
    let mut b: Boolean = 0;
    // SAFETY: live element and key.
    unsafe { AXUIElementIsAttributeSettable(el.0, key.0, &mut b) == kAXErrorSuccess && b != 0 }
}

fn actions(el: &Cf) -> Result<Vec<String>, AXError> {
    let mut out: CFArrayRef = std::ptr::null();
    // SAFETY: live element; out is +1.
    let err = unsafe { AXUIElementCopyActionNames(el.0, &mut out) };
    if err != kAXErrorSuccess {
        return Err(err);
    }
    let arr = Cf::owned(out);
    Ok(arr.map(|a| items(a.0).into_iter().filter_map(string).collect()).unwrap_or_default())
}

fn perform(el: &Cf, action: &str) -> AXError {
    let a = Cf::str(action);
    // SAFETY: live element and action name.
    unsafe { AXUIElementPerformAction(el.0, a.0) }
}

fn app_element(pid: i32) -> Cf {
    app_element_within(pid, AX_TIMEOUT_SECS)
}

/// An app's element whose questions give up after `seconds`.
fn app_element_within(pid: i32, seconds: f32) -> Cf {
    // SAFETY: creates a +1 element for pid (valid even if pid is gone).
    let el = Cf(unsafe { AXUIElementCreateApplication(pid) });
    // SAFETY: live element.
    unsafe { AXUIElementSetMessagingTimeout(el.0, seconds) };
    el
}

/// Listing asks every running app; one that is busy must not hold the list
/// for AX_TIMEOUT_SECS per question. Its windows then come from the window
/// server, without titles.
const LIST_AX_TIMEOUT_SECS: f32 = 0.25;

fn window_id(el: &Cf) -> Option<u32> {
    let mut id = 0u32;
    // SAFETY: live element.
    (unsafe { _AXUIElementGetWindow(el.0, &mut id) } == kAXErrorSuccess && id != 0).then_some(id)
}

fn frame(el: &Cf) -> Option<CGRect> {
    let p = attr(el, "AXPosition").and_then(|v| point(v.0))?;
    let s = attr(el, "AXSize").and_then(|v| size(v.0))?;
    Some(CGRect { origin: p, size: s })
}

fn ax_err(e: AXError) -> anyhow::Error {
    match e {
        kAXErrorAPIDisabled => anyhow!("permission_missing: accessibility"),
        kAXErrorInvalidUIElement => anyhow!("stale_state: the element is gone"),
        kAXErrorCannotComplete => anyhow!("the app did not respond to accessibility (it may be busy)"),
        e => anyhow!("accessibility error {e}"),
    }
}

// ---------- apps and windows ----------

#[derive(Clone, Debug)]
pub struct AppInfo {
    pub pid: i32,
    pub name: String,
    pub bundle_id: String,
    /// Other names it answers to: the unlocalized bundle name, the .app's
    /// file name ("TextEdit" for "文本编辑").
    pub aliases: Vec<String>,
}

/// Bundle id and names (Info.plist name, .app file name) of a process.
fn bundle_for_pid(pid: i32) -> Option<(String, Vec<String>)> {
    let mut buf = vec![0u8; 4096];
    // SAFETY: buf is 4096 bytes (PROC_PIDPATHINFO_MAXSIZE).
    let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr() as *mut libc::c_void, buf.len() as u32) };
    if n <= 0 {
        return None;
    }
    let path = String::from_utf8_lossy(&buf[..n as usize]).to_string();
    // The innermost bundle: Simulator.app lives inside Xcode.app.
    let idx = path.rfind(".app/Contents/").or_else(|| path.rfind(".app/"))?;
    let app_path = &path[..idx + 4];
    let stem = std::path::Path::new(app_path).file_stem().map(|s| s.to_string_lossy().to_string());
    bundle_at(app_path).map(|(id, name)| (id, name.into_iter().chain(stem).collect()))
}

/// Bundle id and display name of the .app at `path`.
fn bundle_at(path: &str) -> Option<(String, Option<String>)> {
    // SAFETY: path bytes are valid for the call; results follow Create / Get rules.
    unsafe {
        let url = Cf::owned(CFURLCreateFromFileSystemRepresentation(std::ptr::null(), path.as_ptr(), path.len() as CFIndex, 1))?;
        let b = Cf::owned(CFBundleCreate(std::ptr::null(), url.0))?;
        let id = string(CFBundleGetIdentifier(b.0))?;
        let name = ["CFBundleDisplayName", "CFBundleName"].iter().find_map(|k| {
            let key = Cf::str(k);
            string(CFBundleGetValueForInfoDictionaryKey(b.0, key.0)).filter(|s| !s.is_empty())
        });
        Some((id, name))
    }
}

struct CgWindow {
    id: u32,
    pid: i32,
    owner: String,
}

fn cg_windows() -> Vec<CgWindow> {
    // SAFETY: the copied array is owned; dictionary values are borrowed.
    unsafe {
        let Some(list) = Cf::owned(CGWindowListCopyWindowInfo(kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements, 0)) else {
            return Vec::new();
        };
        items(list.0)
            .into_iter()
            .filter_map(|d| {
                let layer = number(CFDictionaryGetValue(d, kCGWindowLayer))?;
                if layer != 0.0 {
                    return None;
                }
                if number(CFDictionaryGetValue(d, kCGWindowAlpha)).unwrap_or(1.0) <= 0.0 {
                    return None;
                }
                let mut bounds = CGRect::default();
                let b = CFDictionaryGetValue(d, kCGWindowBounds);
                if b.is_null() || !CGRectMakeWithDictionaryRepresentation(b, &mut bounds) {
                    return None;
                }
                if bounds.size.width < 40.0 || bounds.size.height < 40.0 {
                    return None;
                }
                Some(CgWindow {
                    id: number(CFDictionaryGetValue(d, kCGWindowNumber))? as u32,
                    pid: number(CFDictionaryGetValue(d, kCGWindowOwnerPID))? as i32,
                    owner: string(CFDictionaryGetValue(d, kCGWindowOwnerName)).unwrap_or_default(),
                })
            })
            .collect()
    }
}

/// Running apps with at least one ordinary window, frontmost first.
pub fn running_apps() -> Vec<AppInfo> {
    let me = std::process::id() as i32;
    let mut seen: Vec<AppInfo> = Vec::new();
    for w in cg_windows() {
        if w.pid == me || seen.iter().any(|a| a.pid == w.pid) {
            continue;
        }
        let Some((bundle_id, aliases)) = bundle_for_pid(w.pid) else { continue };
        let name = if w.owner.is_empty() { aliases.first().cloned().unwrap_or_else(|| bundle_id.clone()) } else { w.owner.clone() };
        seen.push(AppInfo { pid: w.pid, name, bundle_id, aliases });
    }
    seen
}

fn ax_windows(app: &Cf) -> Vec<Cf> {
    attr(app, "AXWindows").map(|a| items(a.0).into_iter().filter_map(Cf::retained).collect()).unwrap_or_default()
}

pub fn list_apps() -> Result<Value> {
    need_accessibility()?;
    let cg = cg_windows();
    let apps: Vec<Value> = running_apps()
        .into_iter()
        .map(|a| {
            let el = app_element_within(a.pid, LIST_AX_TIMEOUT_SECS);
            let focused = attr(&el, "AXFocusedWindow").and_then(|w| window_id(&w));
            let mut windows: Vec<Value> = ax_windows(&el)
                .iter()
                .filter_map(|w| {
                    let id = window_id(w)?;
                    // Only windows the window server shows (not minimized, this Space).
                    cg.iter().find(|c| c.id == id)?;
                    let title = attr_string(w, "AXTitle").unwrap_or_default();
                    Some(json!({"id": id, "title": title, "focused": focused == Some(id)}))
                })
                .collect();
            if windows.is_empty() {
                windows = cg.iter().filter(|c| c.pid == a.pid).map(|c| json!({"id": c.id, "title": "", "focused": false})).collect();
            }
            json!({"bundle_id": a.bundle_id, "name": a.name, "pid": a.pid, "windows": windows})
        })
        .collect();
    Ok(json!({"apps": apps}))
}

/// Finds a running app by bundle id or name (case-insensitive).
pub fn find_app(query: &str) -> Result<Option<AppInfo>> {
    let q = query.trim();
    let apps = running_apps();
    if let Some(a) = apps.iter().find(|a| a.bundle_id.eq_ignore_ascii_case(q)) {
        return Ok(Some(a.clone()));
    }
    let by_name: Vec<&AppInfo> = apps.iter().filter(|a| a.name.eq_ignore_ascii_case(q) || a.aliases.iter().any(|n| n.eq_ignore_ascii_case(q))).collect();
    match by_name.len() {
        0 => Ok(None),
        1 => Ok(Some(by_name[0].clone())),
        _ => bail!("`{q}` matches several apps ({}); pass the bundle_id", by_name.iter().map(|a| a.bundle_id.as_str()).collect::<Vec<_>>().join(", ")),
    }
}

/// An installed app's bundle id and name, for an app that isn't running.
pub fn find_installed(query: &str) -> Option<(String, String)> {
    let q = query.trim();
    let home = dirs::home_dir().unwrap_or_default();
    let dirs = [
        "/Applications".into(),
        "/Applications/Utilities".into(),
        "/System/Applications".into(),
        "/System/Applications/Utilities".into(),
        home.join("Applications").to_string_lossy().to_string(),
    ];
    for d in &dirs {
        let Ok(rd) = std::fs::read_dir(d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_none_or(|x| x != "app") {
                continue;
            }
            let stem = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            let Some((id, name)) = bundle_at(&p.to_string_lossy()) else { continue };
            let name = name.unwrap_or_else(|| stem.clone());
            if id.eq_ignore_ascii_case(q) || stem.eq_ignore_ascii_case(q) || name.eq_ignore_ascii_case(q) {
                return Some((id, name));
            }
        }
    }
    // A bundle id of an app elsewhere (Spotlight).
    if q.contains('.') && !q.contains(' ') && !q.contains('\'') {
        let out = std::process::Command::new("/usr/bin/mdfind").arg(format!("kMDItemCFBundleIdentifier == '{q}'")).output().ok()?;
        let path = String::from_utf8_lossy(&out.stdout).lines().find(|l| l.ends_with(".app"))?.to_string();
        let (id, name) = bundle_at(&path)?;
        return Some((id.clone(), name.unwrap_or(id)));
    }
    None
}

// ---------- state ----------

struct Elem {
    el: Cf,
    rect: Option<CGRect>,
}

pub struct State {
    pub id: String,
    pid: i32,
    elems: Vec<Elem>,
    window: Option<Cf>,
    /// Window frame (points) and screenshot scale (pixels per point).
    win_rect: Option<CGRect>,
    scale: Option<f64>,
}

/// The latest state per (session, bundle id).
#[derive(Default)]
pub struct States {
    map: HashMap<(String, String), State>,
    next: u64,
}

pub struct Target<'a> {
    pub session: &'a str,
    pub app: &'a AppInfo,
}

const INTERACTIVE: &[&str] = &[
    "AXButton",
    "AXCheckBox",
    "AXRadioButton",
    "AXTextField",
    "AXTextArea",
    "AXComboBox",
    "AXPopUpButton",
    "AXMenuButton",
    "AXMenuItem",
    "AXMenuBarItem",
    "AXLink",
    "AXSlider",
    "AXIncrementor",
    "AXDisclosureTriangle",
    "AXColorWell",
    "AXRow",
    "AXTab",
    "AXDateField",
    "AXStepper",
    "AXSegmentedControl",
];
const LANDMARK: &[&str] = &[
    "AXWindow",
    "AXSheet",
    "AXDialog",
    "AXToolbar",
    "AXTabGroup",
    "AXTable",
    "AXOutline",
    "AXList",
    "AXWebArea",
    "AXMenu",
    "AXPopover",
    "AXDrawer",
    "AXHeading",
];
const NAMED_SUBROLES: &[&str] = &["AXSearchField", "AXSecureTextField", "AXCloseButton", "AXMinimizeButton", "AXZoomButton", "AXFullScreenButton", "AXTabButton"];

fn short_role(role: &str, subrole: Option<&str>) -> String {
    let r = subrole.filter(|s| NAMED_SUBROLES.contains(s)).unwrap_or(role);
    r.strip_prefix("AX").unwrap_or(r).to_ascii_lowercase()
}

fn clip(s: &str) -> String {
    let s = s.replace('\n', "\\n").replace('\r', "");
    let s = s.trim();
    if s.chars().count() <= MAX_TEXT {
        return s.replace('"', "\\\"");
    }
    format!("{}…", s.chars().take(MAX_TEXT).collect::<String>().replace('"', "\\\""))
}

fn intersects(a: &CGRect, b: &CGRect) -> bool {
    a.origin.x < b.origin.x + b.size.width && b.origin.x < a.origin.x + a.size.width && a.origin.y < b.origin.y + b.size.height && b.origin.y < a.origin.y + a.size.height
}

struct Walk<'a> {
    attrs: &'a Attrs,
    /// The app's focused element: AXFocused alone is unreliable (some apps
    /// report every cell of a focused table as focused).
    focus: Option<Cf>,
    clip_to: Option<CGRect>,
    out: String,
    elems: Vec<Elem>,
    visited: usize,
    max: usize,
    truncated: bool,
}

impl Walk<'_> {
    fn node(&mut self, el: Cf, depth: usize, indent: usize) {
        if self.visited >= MAX_VISITED || depth > MAX_DEPTH {
            self.truncated = true;
            return;
        }
        self.visited += 1;
        let mut out: CFArrayRef = std::ptr::null();
        // SAFETY: live element and attribute array; out is +1.
        let err = unsafe { AXUIElementCopyMultipleAttributeValues(el.0, self.attrs.names.0, 0, &mut out) };
        let Some(vals) = (err == kAXErrorSuccess).then(|| Cf::owned(out)).flatten() else { return };
        let v = items(vals.0);
        let get = |i: usize| v.get(i).copied().filter(|r| !r.is_null() && !ax_error_value(*r));
        let role = get(A_ROLE).and_then(string).unwrap_or_default();
        let subrole = get(A_SUBROLE).and_then(string);
        let title = get(A_TITLE).and_then(string).filter(|s| !s.trim().is_empty());
        let desc = get(A_DESC).and_then(string).filter(|s| !s.trim().is_empty());
        let raw_value = get(A_VALUE);
        let value_str = raw_value.and_then(|r| string(r).or_else(|| number(r).map(|n| if n.fract() == 0.0 { format!("{}", n as i64) } else { format!("{n}") })));
        let enabled = get(A_ENABLED).and_then(boolean).unwrap_or(true);
        // SAFETY: both are live CF objects.
        let focused = get(A_FOCUSED).and_then(boolean).unwrap_or(false) && self.focus.as_ref().is_some_and(|f| unsafe { CFEqual(f.0, el.0) } != 0);
        let selected = get(A_SELECTED).and_then(boolean).unwrap_or(false);
        let rect = match (get(A_POSITION).and_then(point), get(A_SIZE).and_then(size)) {
            (Some(p), Some(s)) => Some(CGRect { origin: p, size: s }),
            _ => None,
        };
        let placeholder = get(A_PLACEHOLDER).and_then(string).filter(|s| !s.trim().is_empty());
        let children = get(A_CHILDREN);

        // Off-window subtrees (scrolled out of view) are skipped whole.
        if let (Some(r), Some(c)) = (rect, self.clip_to) {
            if r.size.width > 0.0 && r.size.height > 0.0 && !intersects(&r, &c) {
                return;
            }
        }
        let zero = rect.is_some_and(|r| r.size.width <= 0.0 || r.size.height <= 0.0);
        let is_text = role == "AXStaticText";
        let label = if is_text { None } else { title.clone().or(desc.clone()) };
        let has_text = label.is_some() || (is_text && value_str.as_deref().is_some_and(|s| !s.trim().is_empty())) || (role == "AXImage" && desc.is_some());
        let include = !zero && (INTERACTIVE.contains(&role.as_str()) || LANDMARK.contains(&role.as_str()) || has_text);

        let mut child_indent = indent;
        if include {
            if self.elems.len() >= self.max || self.out.len() >= MAX_TREE_BYTES {
                self.truncated = true;
                return;
            }
            let n = self.elems.len() + 1;
            let mut line = format!("{}{}", "  ".repeat(indent), short_role(&role, subrole.as_deref()));
            if let Some(l) = &label {
                line.push_str(&format!(" \"{}\"", clip(l)));
            }
            let secure = subrole.as_deref() == Some("AXSecureTextField");
            let checkable = matches!(role.as_str(), "AXCheckBox" | "AXRadioButton");
            match (&value_str, checkable, secure) {
                (_, _, true) => {}
                (Some(s), true, _) => {
                    if s == "1" {
                        line.push_str(" checked");
                    }
                }
                (Some(s), false, _) if is_text => line.push_str(&format!(" \"{}\"", clip(s))),
                (Some(s), false, _) if Some(s) != label.as_ref() && (!s.is_empty() || matches!(role.as_str(), "AXTextField" | "AXTextArea" | "AXComboBox")) => {
                    line.push_str(&format!(" value=\"{}\"", clip(s)))
                }
                _ => {}
            }
            if let (Some(p), Some(d)) = (&placeholder, &value_str) {
                if d.is_empty() {
                    line.push_str(&format!(" placeholder=\"{}\"", clip(p)));
                }
            }
            if title.is_some() && desc.is_some() && !is_text && desc != title {
                line.push_str(&format!(" desc=\"{}\"", clip(desc.as_deref().unwrap_or_default())));
            }
            if !enabled {
                line.push_str(" disabled");
            }
            if focused {
                line.push_str(" focused");
            }
            if selected {
                line.push_str(" selected");
            }
            line.push_str(&format!(" [{n}]\n"));
            self.out.push_str(&line);
            self.elems.push(Elem { el: el.clone(), rect });
            child_indent += 1;
        }
        if let Some(c) = children {
            for child in items(c) {
                if let Some(child) = Cf::retained(child) {
                    self.node(child, depth + 1, child_indent);
                }
                if self.truncated && (self.elems.len() >= self.max || self.out.len() >= MAX_TREE_BYTES) {
                    return;
                }
            }
        }
    }
}

fn pick_window(app: &Cf, id: Option<u32>) -> Result<Cf> {
    let wins = ax_windows(app);
    if let Some(id) = id {
        return wins.into_iter().find(|w| window_id(w) == Some(id)).ok_or_else(|| anyhow!("window {id} not found; see computer_list_apps"));
    }
    for name in ["AXFocusedWindow", "AXMainWindow"] {
        if let Some(w) = attr(app, name) {
            return Ok(w);
        }
    }
    wins.into_iter().next().ok_or_else(|| anyhow!("the app has no window on this Space; open one (computer_open_app) or switch to it"))
}

/// Captures window `id` as a JPEG no longer than MAX_IMAGE_EDGE.
fn capture(id: u32) -> Result<(Vec<u8>, u32, u32)> {
    let dir = std::env::temp_dir().join(format!("agl-cu-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir)?;
    let res = (|| {
        let png = dir.join("w.png");
        let st = std::process::Command::new("/usr/sbin/screencapture")
            .args(["-x", "-o", "-t", "png", &format!("-l{id}")])
            .arg(&png)
            .output()
            .context("screencapture")?;
        if !png.is_file() {
            bail!("screencapture failed: {}", String::from_utf8_lossy(&st.stderr).trim());
        }
        let bytes = std::fs::read(&png)?;
        let (w, h) = png_size(&bytes).context("unreadable screenshot")?;
        let jpg = dir.join("w.jpg");
        let mut cmd = std::process::Command::new("/usr/bin/sips");
        cmd.args(["-s", "format", "jpeg", "-s", "formatOptions", "70"]);
        if w.max(h) > MAX_IMAGE_EDGE {
            cmd.args(["-Z", &MAX_IMAGE_EDGE.to_string()]);
        }
        let out = cmd.arg(&png).arg("--out").arg(&jpg).output().context("sips")?;
        if !jpg.is_file() {
            bail!("converting the screenshot failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        let data = std::fs::read(&jpg)?;
        let (jw, jh) = jpeg_size(&data).context("unreadable jpeg")?;
        Ok((data, jw, jh))
    })();
    let _ = std::fs::remove_dir_all(&dir);
    res
}

pub fn png_size(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() < 24 || &b[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let w = u32::from_be_bytes(b[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(b[20..24].try_into().ok()?);
    Some((w, h))
}

pub fn jpeg_size(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() < 4 || b[0] != 0xFF || b[1] != 0xD8 {
        return None;
    }
    let mut i = 2;
    while i + 9 < b.len() {
        if b[i] != 0xFF {
            i += 1;
            continue;
        }
        let m = b[i + 1];
        if m == 0xD8 || m == 0x01 || (0xD0..=0xD7).contains(&m) || m == 0xFF {
            i += if m == 0xFF { 1 } else { 2 };
            continue;
        }
        let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
        if matches!(m, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF) {
            let h = u16::from_be_bytes([b[i + 5], b[i + 6]]) as u32;
            let w = u16::from_be_bytes([b[i + 7], b[i + 8]]) as u32;
            return Some((w, h));
        }
        i += 2 + len;
    }
    None
}

impl States {
    fn key(t: &Target) -> (String, String) {
        (t.session.to_string(), t.app.bundle_id.clone())
    }

    /// Reads the app's window: tree, elements and (optionally) a screenshot.
    pub fn snapshot(&mut self, t: &Target, window: Option<u32>, screenshot: bool, max: usize) -> Result<Value> {
        need_accessibility()?;
        let app = app_element(t.app.pid);
        // Chromium and Electron apps build their tree only when asked.
        let _ = set_attr(&app, "AXManualAccessibility", unsafe { kCFBooleanTrue });
        let win = pick_window(&app, window)?;
        let win_rect = frame(&win);
        let attrs = Attrs::new();
        let focus = attr(&app, "AXFocusedUIElement");
        let mut walk = Walk { attrs: &attrs, focus, clip_to: win_rect, out: String::new(), elems: Vec::new(), visited: 0, max: max.clamp(50, 5000), truncated: false };
        walk.node(win.clone(), 0, 0);
        self.next += 1;
        let id = format!("s{}", self.next);
        let wid = window_id(&win);
        let mut result = json!({
            "state_id": id,
            "app": {"bundle_id": t.app.bundle_id, "name": t.app.name, "pid": t.app.pid},
            "window": {"id": wid, "title": attr_string(&win, "AXTitle").unwrap_or_default()},
            "tree": walk.out,
            "truncated": walk.truncated,
            "elements": walk.elems.len(),
        });
        let mut scale = None;
        if screenshot {
            match (wid, win_rect) {
                (_, _) if !screen_recording(false) => {
                    result["warnings"] = json!(["permission_missing: screen_recording. No screenshot; grant Screen Recording (computer.permissions {request: true})."]);
                }
                (Some(wid), Some(r)) => match capture(wid) {
                    Ok((data, w, h)) => {
                        let s = w as f64 / r.size.width.max(1.0);
                        scale = Some(s);
                        result["screenshot"] = json!({"width": w, "height": h, "scale": (s * 1000.0).round() / 1000.0});
                        result["images"] = json!([{"mime_type": "image/jpeg", "data": base64::engine::general_purpose::STANDARD.encode(&data)}]);
                    }
                    Err(e) => result["warnings"] = json!([format!("no screenshot: {e:#}")]),
                },
                _ => result["warnings"] = json!(["no screenshot: the window has no id"]),
            }
        }
        self.map.insert(Self::key(t), State { id, pid: t.app.pid, elems: walk.elems, window: Some(win), win_rect, scale });
        Ok(result)
    }

    fn state(&self, t: &Target) -> Option<&State> {
        self.map.get(&Self::key(t))
    }

    /// Whether `state_id` (if given) is not the latest for this app.
    pub fn stale(&self, t: &Target, state_id: Option<&str>) -> bool {
        match (state_id, self.state(t)) {
            (Some(id), Some(s)) => s.id != id || s.pid != t.app.pid,
            (Some(_), None) => true,
            (None, _) => false,
        }
    }

    fn element(&self, t: &Target, n: u64) -> Result<(&Cf, Option<CGRect>)> {
        let s = self.state(t).ok_or_else(|| anyhow!("no state for this app yet; call computer_get_state first"))?;
        let e = s.elems.get((n as usize).wrapping_sub(1)).ok_or_else(|| anyhow!("stale_state: element {n} is not in state {}", s.id))?;
        Ok((&e.el, e.rect))
    }

    /// A model point (screenshot pixels) as a screen point.
    fn to_screen(&self, t: &Target, x: f64, y: f64) -> Result<CGPoint> {
        let s = self.state(t).ok_or_else(|| anyhow!("no screenshot of this app yet; call computer_get_state first"))?;
        let (Some(r), Some(scale)) = (s.win_rect, s.scale) else { bail!("coordinates need a screenshot; call computer_get_state with screenshot: true") };
        Ok(CGPoint { x: r.origin.x + x / scale, y: r.origin.y + y / scale })
    }

    fn window(&self, t: &Target) -> Option<Cf> {
        self.state(t).and_then(|s| s.window.clone())
    }
}

// ---------- input ----------

fn center(r: CGRect) -> CGPoint {
    CGPoint { x: r.origin.x + r.size.width / 2.0, y: r.origin.y + r.size.height / 2.0 }
}

fn source() -> Cf {
    // SAFETY: +1 event source.
    Cf(unsafe { CGEventSourceCreate(kCGEventSourceStateHIDSystemState) })
}

/// When this helper last posted an event to the real input stream: the
/// person's idle time counts those too.
static OWN_INPUT: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);

/// The quiet a foreground action waits for, and how long it waits.
const PERSON_QUIET: f64 = 1.5;
const PERSON_WAIT: Duration = Duration::from_secs(6);

/// Before an action that takes the real pointer, keyboard or front window:
/// waits until the person has not touched the keyboard, mouse or trackpad
/// for PERSON_QUIET seconds (this helper's own events aside), up to
/// PERSON_WAIT. `stopped` ends the wait when the person presses Stop.
pub fn wait_for_person(stopped: &dyn Fn() -> bool) -> Result<()> {
    let deadline = Instant::now() + PERSON_WAIT;
    loop {
        if stopped() {
            bail!("stopped_by_user: the person pressed Stop on their screen");
        }
        // SAFETY: a read of the window server's input clock.
        let idle = unsafe { CGEventSourceSecondsSinceLastEventType(kCGEventSourceStateHIDSystemState, kCGAnyInputEventType) };
        let own = OWN_INPUT.lock().unwrap().map(|t| t.elapsed().as_secs_f64());
        if person_quiet(idle, own) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "person_active: the person is using this Mac (keyboard or mouse within the last {PERSON_QUIET}s, for {}s); a foreground action would take their pointer and keyboard, so nothing was done. Do it in the background, try again later, or ask them",
                PERSON_WAIT.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// `idle`: seconds since the last input; `own`: since this helper's last
/// posted event. The last input was ours when both name the same moment.
fn person_quiet(idle: f64, own: Option<f64>) -> bool {
    idle >= PERSON_QUIET || own.is_some_and(|o| (o - idle).abs() < 0.3)
}

fn post(ev: &Cf, pid: i32, foreground: bool) {
    // SAFETY: live event.
    unsafe {
        if foreground {
            *OWN_INPUT.lock().unwrap() = Some(Instant::now());
            CGEventPost(kCGHIDEventTap, ev.0)
        } else {
            CGEventPostToPid(pid, ev.0)
        }
    }
}

/// Brings the app and its window to the front (only when asked to).
fn activate(t: &Target, win: Option<&Cf>) {
    let app = app_element(t.app.pid);
    let _ = set_attr(&app, "AXFrontmost", unsafe { kCFBooleanTrue });
    if let Some(w) = win {
        let _ = perform(w, "AXRaise");
        let _ = set_attr(w, "AXMain", unsafe { kCFBooleanTrue });
    }
    std::thread::sleep(Duration::from_millis(200));
}

fn mouse_click(pid: i32, at: CGPoint, button: &str, count: u32, foreground: bool) -> Result<()> {
    let (down, up, b) = match button {
        "left" => (kCGEventLeftMouseDown, kCGEventLeftMouseUp, 0),
        "right" => (kCGEventRightMouseDown, kCGEventRightMouseUp, 1),
        "middle" => (kCGEventOtherMouseDown, kCGEventOtherMouseUp, 2),
        other => bail!("button must be left, right or middle, not `{other}`"),
    };
    let src = source();
    if foreground {
        // SAFETY: +1 event, posted then released.
        let mv = Cf(unsafe { CGEventCreateMouseEvent(src.0, kCGEventMouseMoved, at, 0) });
        post(&mv, pid, true);
        std::thread::sleep(Duration::from_millis(30));
    }
    for i in 1..=count.clamp(1, 3) {
        // SAFETY: +1 events, posted then released.
        unsafe {
            let d = Cf(CGEventCreateMouseEvent(src.0, down, at, b));
            CGEventSetIntegerValueField(d.0, kCGMouseEventClickState, i as i64);
            post(&d, pid, foreground);
            std::thread::sleep(Duration::from_millis(20));
            let u = Cf(CGEventCreateMouseEvent(src.0, up, at, b));
            CGEventSetIntegerValueField(u.0, kCGMouseEventClickState, i as i64);
            post(&u, pid, foreground);
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    Ok(())
}

fn type_keys(pid: i32, text: &str, foreground: bool) {
    let src = source();
    let units: Vec<u16> = text.encode_utf16().collect();
    for chunk in units.chunks(16) {
        // SAFETY: +1 events; chunk outlives the calls.
        unsafe {
            let d = Cf(CGEventCreateKeyboardEvent(src.0, 0, true));
            CGEventKeyboardSetUnicodeString(d.0, chunk.len(), chunk.as_ptr());
            post(&d, pid, foreground);
            let u = Cf(CGEventCreateKeyboardEvent(src.0, 0, false));
            CGEventKeyboardSetUnicodeString(u.0, chunk.len(), chunk.as_ptr());
            post(&u, pid, foreground);
        }
        std::thread::sleep(Duration::from_millis(15));
    }
}

fn press_combo(pid: i32, combo: &keys::Combo, foreground: bool) {
    let src = source();
    // SAFETY: +1 events, posted then released.
    unsafe {
        let d = Cf(CGEventCreateKeyboardEvent(src.0, combo.code, true));
        CGEventSetFlags(d.0, combo.flags);
        post(&d, pid, foreground);
        std::thread::sleep(Duration::from_millis(15));
        let u = Cf(CGEventCreateKeyboardEvent(src.0, combo.code, false));
        CGEventSetFlags(u.0, combo.flags);
        post(&u, pid, foreground);
    }
    std::thread::sleep(Duration::from_millis(30));
}

fn scroll_at(pid: i32, at: CGPoint, direction: &str, amount: i32, foreground: bool) -> Result<()> {
    let n = amount.clamp(1, 50);
    let (v, h) = match direction {
        "up" => (n, 0),
        "down" => (-n, 0),
        "left" => (0, n),
        "right" => (0, -n),
        other => bail!("direction must be up, down, left or right, not `{other}`"),
    };
    let src = source();
    // SAFETY: +1 event, posted then released.
    unsafe {
        let e = Cf(CGEventCreateScrollWheelEvent2(src.0, kCGScrollEventUnitLine, 2, v, h, 0));
        CGEventSetLocation(e.0, at);
        post(&e, pid, foreground);
    }
    Ok(())
}

fn drag(pid: i32, from: CGPoint, to: CGPoint) {
    let src = source();
    // SAFETY: +1 events, posted then released.
    unsafe {
        let mv = Cf(CGEventCreateMouseEvent(src.0, kCGEventMouseMoved, from, 0));
        post(&mv, pid, true);
        std::thread::sleep(Duration::from_millis(50));
        let d = Cf(CGEventCreateMouseEvent(src.0, kCGEventLeftMouseDown, from, 0));
        post(&d, pid, true);
        for i in 1..=12 {
            let f = i as f64 / 12.0;
            let p = CGPoint { x: from.x + (to.x - from.x) * f, y: from.y + (to.y - from.y) * f };
            let e = Cf(CGEventCreateMouseEvent(src.0, kCGEventLeftMouseDragged, p, 0));
            post(&e, pid, true);
            std::thread::sleep(Duration::from_millis(16));
        }
        let u = Cf(CGEventCreateMouseEvent(src.0, kCGEventLeftMouseUp, to, 0));
        post(&u, pid, true);
    }
}

/// Selects all text of the app's focused text control.
fn select_all_text(pid: i32) -> bool {
    let Some(el) = attr(&app_element(pid), "AXFocusedUIElement") else { return false };
    if !settable(&el, "AXSelectedTextRange") {
        return false;
    }
    let Some(n) = attr(&el, "AXNumberOfCharacters").and_then(|v| number(v.0)) else { return false };
    let range = CFRange { location: 0, length: n as CFIndex };
    // SAFETY: range matches kAXValueCFRangeType; the value is +1 and owned.
    let Some(v) = Cf::owned(unsafe { AXValueCreate(kAXValueCFRangeType, &range as *const CFRange as *const std::ffi::c_void) }) else { return false };
    set_attr(&el, "AXSelectedTextRange", v.0) == kAXErrorSuccess
}

/// AXMenuItemCmdModifiers bits: cmd is implied unless NO_CMD is set.
const MENU_SHIFT: i64 = 1;
const MENU_OPTION: i64 = 2;
const MENU_CONTROL: i64 = 4;
const MENU_NO_CMD: i64 = 8;

/// Presses the enabled menu item whose shortcut is `combo`. Returns its title.
fn menu_shortcut(pid: i32, combo: &keys::Combo) -> Option<String> {
    let ch = combo.ch?;
    let want = (if combo.flags & keys::FLAG_SHIFT != 0 { MENU_SHIFT } else { 0 })
        | (if combo.flags & keys::FLAG_ALT != 0 { MENU_OPTION } else { 0 })
        | (if combo.flags & keys::FLAG_CONTROL != 0 { MENU_CONTROL } else { 0 });
    let bar = attr(&app_element(pid), "AXMenuBar")?;
    let mut stack: Vec<(Cf, usize)> = vec![(bar, 0)];
    let mut seen = 0;
    while let Some((el, depth)) = stack.pop() {
        seen += 1;
        if seen > 3000 {
            break;
        }
        if depth > 6 {
            continue;
        }
        let item_ch = attr_string(&el, "AXMenuItemCmdChar").filter(|c| !c.is_empty());
        if let Some(c) = item_ch {
            let mods = attr(&el, "AXMenuItemCmdModifiers").and_then(|v| number(v.0)).unwrap_or(0.0) as i64;
            if mods & MENU_NO_CMD == 0 && (mods & 7) == want && c.chars().next().map(|c| c.to_ascii_uppercase()) == Some(ch) {
                let enabled = attr(&el, "AXEnabled").and_then(|v| boolean(v.0)).unwrap_or(true);
                // A background app's menu isn't validated: its disabled
                // items "press" without doing anything.
                if enabled && perform(&el, "AXPress") == kAXErrorSuccess {
                    return Some(attr_string(&el, "AXTitle").unwrap_or_else(|| c.clone()));
                }
            }
        }
        if let Some(children) = attr(&el, "AXChildren") {
            for c in items(children.0).into_iter().rev() {
                if let Some(c) = Cf::retained(c) {
                    stack.push((c, depth + 1));
                }
            }
        }
    }
    None
}

/// Presses (or selects) an element the accessibility way, in the
/// background. `None` when it has no such action.
fn press(el: &Cf, button: &str, count: u32) -> Result<Option<String>> {
    let acts = match actions(el) {
        Ok(a) => a,
        Err(kAXErrorInvalidUIElement) => return Err(ax_err(kAXErrorInvalidUIElement)),
        Err(_) => Vec::new(),
    };
    let pick = match (button, count) {
        ("left", 1) => ["AXPress", "AXConfirm", "AXPick", "AXOpen"].into_iter().find(|a| acts.iter().any(|x| x == a)),
        ("right", 1) => acts.iter().any(|x| x == "AXShowMenu").then_some("AXShowMenu"),
        _ => None,
    };
    if let Some(a) = pick {
        match perform(el, a) {
            kAXErrorSuccess => return Ok(Some(format!("performed {a} on"))),
            kAXErrorInvalidUIElement => return Err(ax_err(kAXErrorInvalidUIElement)),
            _ => {}
        }
    }
    // Selectable rows: select instead of pressing, when that sticks (some
    // apps accept the write and ignore it).
    if button == "left" && count == 1 && settable(el, "AXSelected") && set_attr(el, "AXSelected", unsafe { kCFBooleanTrue }) == kAXErrorSuccess {
        std::thread::sleep(Duration::from_millis(80));
        if attr(el, "AXSelected").and_then(|v| boolean(v.0)) == Some(true) {
            return Ok(Some("selected".into()));
        }
    }
    Ok(None)
}

/// The app's element at a screen point.
fn hit_test(pid: i32, at: CGPoint) -> Option<Cf> {
    let app = app_element(pid);
    let mut out: AXUIElementRef = std::ptr::null();
    // SAFETY: live app element; out receives a +1 reference.
    let err = unsafe { AXUIElementCopyElementAtPosition(app.0, at.x as f32, at.y as f32, &mut out) };
    (err == kAXErrorSuccess).then(|| Cf::owned(out)).flatten()
}

// ---------- actions ----------

fn num(args: &Value, k: &str) -> Option<f64> {
    args.get(k).and_then(Value::as_f64)
}

fn flag(args: &Value, k: &str) -> bool {
    args.get(k).and_then(Value::as_bool).unwrap_or(false)
}

/// Where a tool acts: an element of the latest state, or a screenshot point.
fn locate(states: &States, t: &Target, args: &Value) -> Result<(Option<Cf>, CGPoint)> {
    match (args.get("element").and_then(Value::as_u64), num(args, "x"), num(args, "y")) {
        (Some(n), None, None) => {
            let (el, rect) = states.element(t, n)?;
            let r = rect.or_else(|| frame(el)).ok_or_else(|| anyhow!("element {n} has no position"))?;
            Ok((Some(el.clone()), center(r)))
        }
        (None, Some(x), Some(y)) => Ok((None, states.to_screen(t, x, y)?)),
        _ => bail!("pass either `element` or both `x` and `y`"),
    }
}

fn point_arg(states: &States, t: &Target, v: Option<&Value>, what: &str) -> Result<CGPoint> {
    let v = v.ok_or_else(|| anyhow!("`{what}` is required"))?;
    locate(states, t, v).map(|(_, p)| p).with_context(|| format!("`{what}`"))
}

/// The name an action's element goes by on screen (its title, description or
/// value), for the indicator (ui.rs); None for a point or a nameless element.
pub fn target_name(states: &States, t: &Target, args: &Value) -> Option<String> {
    let n = args.get("element").and_then(Value::as_u64)?;
    let (el, _) = states.element(t, n).ok()?;
    ["AXTitle", "AXDescription", "AXValue"]
        .iter()
        .filter_map(|a| attr_string(el, a))
        .map(|s| s.trim().to_string())
        .find(|s| !s.is_empty())
        .map(|s| if s.chars().count() > 30 { format!("{}…", s.chars().take(29).collect::<String>()) } else { s })
}

/// The screen rectangle an action works on, for the outline (ui.rs): the
/// element's frame, or a small square around a point.
pub fn target_rect(states: &States, t: &Target, tool: &str, args: &Value) -> Option<CGRect> {
    let point_rect = |p: CGPoint| CGRect { origin: CGPoint { x: p.x - 14.0, y: p.y - 14.0 }, size: CGSize { width: 28.0, height: 28.0 } };
    let of = |v: &Value| -> Option<CGRect> {
        if let Some(n) = v.get("element").and_then(Value::as_u64) {
            let (el, rect) = states.element(t, n).ok()?;
            return rect.or_else(|| frame(el));
        }
        let (x, y) = (num(v, "x")?, num(v, "y")?);
        states.to_screen(t, x, y).ok().map(point_rect)
    };
    match tool {
        "computer_click" | "computer_type" | "computer_scroll" | "computer_set_value" | "computer_action" => of(args),
        "computer_drag" => args.get("from").and_then(of),
        _ => None,
    }
}

/// Runs one action tool. Returns a note on how it was done.
/// `before_foreground` runs before anything takes the real pointer,
/// keyboard or front window (wait_for_person).
pub fn act(states: &mut States, t: &Target, tool: &str, args: &Value, before_foreground: &dyn Fn() -> Result<()>) -> Result<String> {
    need_accessibility()?;
    let pid = t.app.pid;
    let fg = flag(args, "foreground");
    let win = states.window(t);
    match tool {
        "computer_click" => {
            let button = args.get("button").and_then(Value::as_str).unwrap_or("left");
            let count = args.get("count").and_then(Value::as_u64).unwrap_or(1) as u32;
            let (el, at) = locate(states, t, args)?;
            if !fg {
                if let Some(el) = &el {
                    if let Some(done) = press(el, button, count)? {
                        return Ok(done);
                    }
                } else if let Some(hit) = hit_test(pid, at) {
                    // A point: press what is there, walking up to its control.
                    let mut cur = Some(hit);
                    for _ in 0..6 {
                        let Some(e) = cur else { break };
                        if let Some(done) = press(&e, button, count)? {
                            let role = attr_string(&e, "AXRole").unwrap_or_default();
                            return Ok(format!("{done} the {} at that point", short_role(&role, None)));
                        }
                        cur = attr(&e, "AXParent");
                    }
                }
            }
            if fg {
                before_foreground()?;
                activate(t, win.as_ref());
            }
            mouse_click(pid, at, button, count, fg)?;
            Ok(format!(
                "{} {button} click{} at ({:.0}, {:.0}) screen points{}",
                if fg { "foreground" } else { "background" },
                if count > 1 { format!(" x{count}") } else { String::new() },
                at.x,
                at.y,
                if fg { "" } else { "; if nothing changed, retry with foreground: true" }
            ))
        }
        "computer_type" => {
            let text = args.get("text").and_then(Value::as_str).ok_or_else(|| anyhow!("`text` is required"))?;
            let target = match args.get("element").and_then(Value::as_u64) {
                Some(n) => {
                    let (el, _) = states.element(t, n)?;
                    let e = set_attr(el, "AXFocused", unsafe { kCFBooleanTrue });
                    if e == kAXErrorInvalidUIElement {
                        return Err(ax_err(e));
                    }
                    Some(el.clone())
                }
                None => attr(&app_element(pid), "AXFocusedUIElement"),
            };
            let via_keys = args.get("via").and_then(Value::as_str) == Some("keys");
            if let (Some(el), false, false) = (&target, via_keys, fg) {
                if settable(el, "AXSelectedText") {
                    let s = Cf::str(text);
                    if set_attr(el, "AXSelectedText", s.0) == kAXErrorSuccess {
                        return Ok("inserted the text at the cursor (accessibility); if the app ignored it, retry with via: \"keys\"".into());
                    }
                }
            }
            if fg {
                before_foreground()?;
                activate(t, win.as_ref());
            }
            type_keys(pid, text, fg);
            Ok(format!("typed {} characters as key events{}", text.chars().count(), if fg { " (foreground)" } else { "" }))
        }
        "computer_key" => {
            let spec = args.get("keys").and_then(Value::as_str).ok_or_else(|| anyhow!("`keys` is required, e.g. \"cmd+l\""))?;
            let combos = keys::parse(spec)?;
            if fg {
                before_foreground()?;
                activate(t, win.as_ref());
            }
            let mut via_menu = Vec::new();
            for c in &combos {
                // Select all in a text control, the accessibility way.
                if !fg && c.code == 0 && c.flags == keys::FLAG_CMD && select_all_text(pid) {
                    via_menu.push("select all text".to_string());
                    continue;
                }
                // Shortcuts reach a background app only through its menu.
                if !fg && c.flags & keys::FLAG_CMD != 0 {
                    if let Some(item) = menu_shortcut(pid, c) {
                        via_menu.push(item);
                        std::thread::sleep(Duration::from_millis(60));
                        continue;
                    }
                }
                press_combo(pid, c, fg);
            }
            let menu = if via_menu.is_empty() { String::new() } else { format!(" (done in the background: {})", via_menu.join(", ")) };
            let sent_keys = via_menu.len() < combos.len();
            Ok(format!(
                "pressed {spec}{menu}{}",
                if fg {
                    " (foreground)"
                } else if sent_keys {
                    "; key events went to the app in the background: if nothing changed, retry with foreground: true"
                } else {
                    ""
                }
            ))
        }
        "computer_scroll" => {
            let direction = args.get("direction").and_then(Value::as_str).unwrap_or("down");
            let amount = args.get("amount").and_then(Value::as_i64).unwrap_or(3) as i32;
            let at = if args.get("element").is_some() || args.get("x").is_some() {
                locate(states, t, args)?.1
            } else {
                states.state(t).and_then(|s| s.win_rect).map(center).ok_or_else(|| anyhow!("call computer_get_state first"))?
            };
            if fg {
                before_foreground()?;
                activate(t, win.as_ref());
            }
            scroll_at(pid, at, direction, amount, fg)?;
            Ok(format!("scrolled {direction} {amount}"))
        }
        "computer_drag" => {
            let from = point_arg(states, t, args.get("from"), "from")?;
            let to = point_arg(states, t, args.get("to"), "to")?;
            before_foreground()?;
                activate(t, win.as_ref());
            drag(pid, from, to);
            Ok("dragged (foreground: dragging moves the real pointer)".into())
        }
        "computer_set_value" => {
            let n = args.get("element").and_then(Value::as_u64).ok_or_else(|| anyhow!("`element` is required"))?;
            let value = args.get("value").ok_or_else(|| anyhow!("`value` is required"))?;
            let (el, _) = states.element(t, n)?;
            let current_is_number = attr(el, "AXValue").is_some_and(|v| is_number(v.0));
            let cf = match value {
                Value::Number(x) => Cf::f64(x.as_f64().unwrap_or(0.0)),
                Value::Bool(b) => Cf::f64(if *b { 1.0 } else { 0.0 }),
                Value::String(s) if current_is_number && s.trim().parse::<f64>().is_ok() => Cf::f64(s.trim().parse().unwrap_or(0.0)),
                Value::String(s) => Cf::str(s),
                _ => bail!("`value` must be a string, number or boolean"),
            };
            if !settable(el, "AXValue") {
                bail!("element {n}'s value can't be set; click it and use computer_type instead");
            }
            let e = set_attr(el, "AXValue", cf.0);
            if e != kAXErrorSuccess {
                return Err(ax_err(e));
            }
            Ok("set the value".into())
        }
        "computer_action" => {
            let n = args.get("element").and_then(Value::as_u64).ok_or_else(|| anyhow!("`element` is required"))?;
            let action = args.get("action").and_then(Value::as_str).ok_or_else(|| anyhow!("`action` is required"))?;
            let (el, _) = states.element(t, n)?;
            let acts = actions(el).map_err(ax_err)?;
            if !acts.iter().any(|a| a == action) {
                bail!("element {n} doesn't support {action}; it supports: {}", if acts.is_empty() { "nothing".into() } else { acts.join(", ") });
            }
            let e = perform(el, action);
            if e != kAXErrorSuccess {
                return Err(ax_err(e));
            }
            Ok(format!("performed {action}"))
        }
        other => bail!("unknown computer tool `{other}`"),
    }
}

/// Starts or activates an app; returns it once it has a window (or after a wait).
pub fn open_app(bundle_id: &str) -> Result<AppInfo> {
    let st = std::process::Command::new("/usr/bin/open").args(["-b", bundle_id]).output().context("open")?;
    if !st.status.success() {
        bail!("could not open {bundle_id}: {}", String::from_utf8_lossy(&st.stderr).trim());
    }
    let start = Instant::now();
    loop {
        if let Some(a) = running_apps().into_iter().find(|a| a.bundle_id.eq_ignore_ascii_case(bundle_id)) {
            std::thread::sleep(Duration::from_millis(300));
            return Ok(a);
        }
        if start.elapsed() > Duration::from_secs(10) {
            bail!("{bundle_id} started but shows no window yet; call computer_list_apps again shortly");
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(test)]
mod person_tests {
    use super::*;

    #[test]
    fn the_persons_own_input_holds_a_foreground_action() {
        assert!(person_quiet(5.0, None));
        assert!(!person_quiet(0.4, None));
        // The last input was this helper's own click.
        assert!(person_quiet(0.4, Some(0.45)));
        // The person moved the mouse after it.
        assert!(!person_quiet(0.4, Some(3.0)));
    }
}
