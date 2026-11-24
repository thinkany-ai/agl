//! What the person sees while an agent uses their screen: a small panel in
//! the corner ("Sumus is using Safari", with Stop) and, just before an
//! action, an outline on what it acts on. Both belong to the helper app (the
//! product's own signed bundle), float above other windows without taking
//! focus, and are left out of screen captures.
//!
//! AppKit runs on the main thread (`run`); the helper's requests run on a
//! worker thread and reach the windows through the main queue. Stop marks the
//! session and turn the panel shows; the helper refuses that turn's further
//! calls (`stopped`), and a call of another turn lifts it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, sel, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSButton, NSColor, NSFont, NSPanel, NSScreen,
    NSTextField, NSView, NSWindowCollectionBehavior, NSWindowSharingType, NSWindowStyleMask,
};
use objc2_foundation::{NSLocale, NSObject, NSPoint, NSRect, NSSize, NSString};

use super::ffi::CGRect;

/// How long the panel stays after the last call.
const LINGER: Duration = Duration::from_secs(6);
/// How long an outline shows before its action runs.
pub const OUTLINE_LEAD: Duration = Duration::from_millis(200);
/// A Stop without a turn to hold for (a hub that sends none) lasts this long.
const STOP_WITHOUT_TURN: Duration = Duration::from_secs(10 * 60);

struct Stop {
    turn: Option<String>,
    at: Instant,
}

/// Sessions the person stopped, and the turn each stop holds for.
static STOPS: Mutex<Option<HashMap<String, Stop>>> = Mutex::new(None);
/// The session and turn the panel shows (what its Stop stops).
static SHOWN: Mutex<Option<(String, Option<String>)>> = Mutex::new(None);
/// Bumped by every call; a hide that finds it changed leaves the panel.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Whether the person stopped `session` for `turn`. A call of another turn
/// lifts the stop (the person asked again).
pub fn stopped(session: &str, turn: Option<&str>) -> bool {
    let mut guard = STOPS.lock().unwrap();
    let stops = guard.get_or_insert_with(HashMap::new);
    let Some(s) = stops.get(session) else { return false };
    let holds = match (&s.turn, turn) {
        (Some(a), Some(b)) => a == b,
        _ => s.at.elapsed() < STOP_WITHOUT_TURN,
    };
    if !holds {
        stops.remove(session);
    }
    holds
}

fn stop_shown() {
    let Some((session, turn)) = SHOWN.lock().unwrap().clone() else { return };
    let mut guard = STOPS.lock().unwrap();
    let stops = guard.get_or_insert_with(HashMap::new);
    if stops.len() > 1024 {
        stops.clear();
    }
    stops.insert(session, Stop { turn, at: Instant::now() });
}

/// Runs AppKit on this (the main) thread and `worker` beside it; returns
/// only if AppKit cannot start (`worker` then runs here).
pub fn run(worker: impl FnOnce() -> i32 + Send + 'static) -> i32 {
    let Some(mtm) = MainThreadMarker::new() else { return worker() };
    let app = NSApplication::sharedApplication(mtm);
    // No Dock icon, no menu bar: windows only.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    std::thread::spawn(move || std::process::exit(worker()));
    app.run();
    0
}

/// Shows the panel for a call on `app` by `operator` in `session`, with the
/// step being done (`step_text`) when there is one.
pub fn busy(operator: &str, app: &str, session: &str, turn: Option<&str>, step: Option<String>) {
    *SHOWN.lock().unwrap() = Some((session.to_string(), turn.map(str::to_string)));
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    let text = match step {
        Some(s) => format!("{} · {s}", label(operator, app)),
        None => label(operator, app),
    };
    DispatchQueue::main().exec_async(move || {
        if let Some(mtm) = MainThreadMarker::new() {
            UI.with(|ui| ui.borrow_mut().show_panel(mtm, &text));
        }
    });
    std::thread::spawn(move || {
        std::thread::sleep(LINGER);
        if GENERATION.load(Ordering::SeqCst) == generation {
            DispatchQueue::main().exec_async(|| UI.with(|ui| ui.borrow().hide_panel()));
        }
    });
}

/// Outlines `rect` (screen points, top-left origin) until `unmark`.
pub fn mark(rect: CGRect) {
    DispatchQueue::main().exec_sync(move || {
        if let Some(mtm) = MainThreadMarker::new() {
            UI.with(|ui| ui.borrow_mut().show_outline(mtm, rect));
        }
    });
}

pub fn unmark() {
    DispatchQueue::main().exec_async(|| UI.with(|ui| ui.borrow().hide_outline()));
}

fn chinese() -> bool {
    NSLocale::preferredLanguages().firstObject().is_some_and(|l| l.to_string().starts_with("zh"))
}

/// What a tool call does, in a few words: "点击「7」", "Typing “milk”".
pub fn step_text(tool: &str, name: Option<&str>, args: &serde_json::Value) -> Option<String> {
    let zh = chinese();
    let quoted = |s: &str| if zh { format!("「{s}」") } else { format!("“{s}”") };
    let short = |s: &str| if s.chars().count() > 20 { format!("{}…", s.chars().take(19).collect::<String>()) } else { s.to_string() };
    Some(match tool {
        "computer_click" => match name {
            Some(n) => format!("{}{}", if zh { "点击" } else { "Clicking " }, quoted(n)),
            None => (if zh { "点击" } else { "Clicking" }).into(),
        },
        "computer_type" => match args.get("text").and_then(|v| v.as_str()) {
            Some(t) => format!("{}{}", if zh { "输入" } else { "Typing " }, quoted(&short(t))),
            None => (if zh { "输入文字" } else { "Typing" }).into(),
        },
        "computer_key" => format!("{}{}", if zh { "按 " } else { "Pressing " }, args.get("keys").and_then(|v| v.as_str()).unwrap_or_default()),
        "computer_scroll" => (if zh { "滚动" } else { "Scrolling" }).into(),
        "computer_drag" => (if zh { "拖动" } else { "Dragging" }).into(),
        "computer_set_value" => format!("{}{}", if zh { "设置" } else { "Setting " }, quoted(name.unwrap_or(""))),
        "computer_action" => format!("{}{}", if zh { "操作" } else { "Using " }, quoted(name.unwrap_or(""))),
        "computer_get_state" => (if zh { "查看窗口" } else { "Looking at the window" }).into(),
        "computer_open_app" => (if zh { "打开" } else { "Opening" }).into(),
        _ => return None,
    })
}

fn label(operator: &str, app: &str) -> String {
    if chinese() {
        format!("{operator} 正在操作 {app}")
    } else {
        format!("{operator} is using {app}")
    }
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "AglComputerStopTarget"]
    struct StopTarget;

    impl StopTarget {
        #[unsafe(method(stop:))]
        fn stop(&self, _sender: Option<&AnyObject>) {
            stop_shown();
            GENERATION.fetch_add(1, Ordering::SeqCst);
            UI.with(|ui| ui.borrow().hide_panel());
        }
    }
);

#[derive(Default)]
struct Ui {
    panel: Option<(Retained<NSPanel>, Retained<NSTextField>, Retained<StopTarget>)>,
    outline: Option<Retained<NSPanel>>,
}

thread_local! {
    static UI: RefCell<Ui> = RefCell::new(Ui::default());
}

/// A borderless panel above other apps' windows that never takes focus, is
/// on every Space, and is not in screenshots or screen recordings.
fn floating(mtm: MainThreadMarker, frame: NSRect) -> Retained<NSPanel> {
    let style = NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel;
    let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
        NSPanel::alloc(mtm),
        frame,
        style,
        NSBackingStoreType::Buffered,
        false,
    );
    // SAFETY: plain property setters on a panel this thread made.
    unsafe {
        panel.setReleasedWhenClosed(false);
    }
    panel.setLevel(25); // NSStatusWindowLevel
    panel.setHidesOnDeactivate(false);
    panel.setFloatingPanel(true);
    panel.setBecomesKeyOnlyIfNeeded(true);
    panel.setSharingType(NSWindowSharingType::None);
    panel.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    panel
}

impl Ui {
    fn show_panel(&mut self, mtm: MainThreadMarker, text: &str) {
        if self.panel.is_none() {
            let size = NSSize::new(400.0, 40.0);
            let panel = floating(mtm, NSRect::new(NSPoint::new(0.0, 0.0), size));
            panel.setOpaque(false);
            panel.setBackgroundColor(Some(&NSColor::colorWithCalibratedWhite_alpha(0.12, 0.92)));
            panel.setHasShadow(true);
            let content: Retained<NSView> = panel.contentView().expect("content view");
            let field = NSTextField::labelWithString(&NSString::from_str(text), mtm);
            field.setFrame(NSRect::new(NSPoint::new(12.0, 11.0), NSSize::new(310.0, 18.0)));
            // A long step is cut at the end rather than wrapped.
            field.setLineBreakMode(objc2_app_kit::NSLineBreakMode::ByTruncatingTail);
            field.setTextColor(Some(&NSColor::whiteColor()));
            field.setFont(Some(&NSFont::systemFontOfSize(12.5)));
            content.addSubview(&field);
            let target: Retained<StopTarget> = unsafe { msg_send![StopTarget::alloc(mtm), init] };
            let title = NSString::from_str(if chinese() { "停止" } else { "Stop" });
            // SAFETY: the target outlives the button (both kept in `Ui`).
            let button = unsafe {
                NSButton::buttonWithTitle_target_action(&title, Some(&target), Some(sel!(stop:)), mtm)
            };
            button.setFrame(NSRect::new(NSPoint::new(328.0, 6.0), NSSize::new(62.0, 28.0)));
            content.addSubview(&button);
            self.panel = Some((panel, field, target));
        }
        let (panel, field, _) = self.panel.as_ref().expect("panel");
        field.setStringValue(&NSString::from_str(text));
        if let Some(screen) = NSScreen::mainScreen(mtm) {
            let v = screen.visibleFrame();
            let size = panel.frame().size;
            // Bottom right, above the Dock: where the eye finds it while
            // watching the app work.
            panel.setFrameOrigin(NSPoint::new(v.origin.x + v.size.width - size.width - 16.0, v.origin.y + 16.0));
        }
        panel.orderFrontRegardless();
    }

    fn hide_panel(&self) {
        if let Some((panel, _, _)) = &self.panel {
            panel.orderOut(None);
        }
    }

    fn show_outline(&mut self, mtm: MainThreadMarker, rect: CGRect) {
        // AX rects are top-left based on the primary screen; AppKit's are
        // bottom-left based on it.
        let Some(primary) = NSScreen::screens(mtm).firstObject() else { return };
        let ph = primary.frame().size.height;
        let pad = 3.0;
        let frame = NSRect::new(
            NSPoint::new(rect.origin.x - pad, ph - rect.origin.y - rect.size.height - pad),
            NSSize::new(rect.size.width + 2.0 * pad, rect.size.height + 2.0 * pad),
        );
        let outline = self.outline.get_or_insert_with(|| {
            let p = floating(mtm, frame);
            p.setOpaque(false);
            p.setIgnoresMouseEvents(true);
            p.setHasShadow(false);
            p.setBackgroundColor(Some(&NSColor::systemBlueColor().colorWithAlphaComponent(0.28)));
            p
        });
        outline.setFrame_display(frame, true);
        outline.orderFrontRegardless();
    }

    fn hide_outline(&self) {
        if let Some(o) = &self.outline {
            o.orderOut(None);
        }
    }
}
