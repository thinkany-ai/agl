//! Raw bindings to CoreFoundation, ApplicationServices (Accessibility) and
//! CoreGraphics: the few calls computer use needs, without extra crates.
#![allow(non_upper_case_globals, non_snake_case, dead_code)]

use std::ffi::c_void;

pub type CFTypeRef = *const c_void;
pub type CFStringRef = CFTypeRef;
pub type CFArrayRef = CFTypeRef;
pub type CFDictionaryRef = CFTypeRef;
pub type CFIndex = isize;
pub type CFTypeID = usize;
pub type Boolean = u8;
pub type AXError = i32;
pub type AXUIElementRef = CFTypeRef;
pub type CGEventRef = CFTypeRef;
pub type CGEventSourceRef = CFTypeRef;

pub const kCFStringEncodingUTF8: u32 = 0x0800_0100;
pub const kCFNumberDoubleType: CFIndex = 13;
pub const kCFNumberSInt64Type: CFIndex = 4;

pub const kAXErrorSuccess: AXError = 0;
pub const kAXErrorInvalidUIElement: AXError = -25202;
pub const kAXErrorCannotComplete: AXError = -25204;
pub const kAXErrorAPIDisabled: AXError = -25211;
pub const kAXValueCGPointType: u32 = 1;
pub const kAXValueCGSizeType: u32 = 2;
pub const kAXValueCFRangeType: u32 = 4;
pub const kAXValueAXErrorType: u32 = 5;

pub const kCGWindowListOptionOnScreenOnly: u32 = 1;
pub const kCGWindowListExcludeDesktopElements: u32 = 16;

pub const kCGHIDEventTap: u32 = 0;
pub const kCGEventSourceStateHIDSystemState: i32 = 1;
/// Any input event (`kCGAnyInputEventType`).
pub const kCGAnyInputEventType: u32 = !0;
pub const kCGEventLeftMouseDown: u32 = 1;
pub const kCGEventLeftMouseUp: u32 = 2;
pub const kCGEventRightMouseDown: u32 = 3;
pub const kCGEventRightMouseUp: u32 = 4;
pub const kCGEventMouseMoved: u32 = 5;
pub const kCGEventLeftMouseDragged: u32 = 6;
pub const kCGEventOtherMouseDown: u32 = 25;
pub const kCGEventOtherMouseUp: u32 = 26;
pub const kCGMouseEventClickState: u32 = 1;
pub const kCGScrollEventUnitLine: u32 = 1;

pub const FLAG_SHIFT: u64 = 0x0002_0000;
pub const FLAG_CONTROL: u64 = 0x0004_0000;
pub const FLAG_ALT: u64 = 0x0008_0000;
pub const FLAG_CMD: u64 = 0x0010_0000;
pub const FLAG_FN: u64 = 0x0080_0000;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CGPoint {
    pub x: f64,
    pub y: f64,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CGSize {
    pub width: f64,
    pub height: f64,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CGRect {
    pub origin: CGPoint,
    pub size: CGSize,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CFRange {
    pub location: CFIndex,
    pub length: CFIndex,
}

#[repr(C)]
pub struct CFArrayCallBacks {
    _private: [u8; 0],
}
#[repr(C)]
pub struct CFDictionaryKeyCallBacks {
    _private: [u8; 0],
}
#[repr(C)]
pub struct CFDictionaryValueCallBacks {
    _private: [u8; 0],
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    pub static kCFTypeArrayCallBacks: CFArrayCallBacks;
    pub static kCFTypeDictionaryKeyCallBacks: CFDictionaryKeyCallBacks;
    pub static kCFTypeDictionaryValueCallBacks: CFDictionaryValueCallBacks;
    pub static kCFBooleanTrue: CFTypeRef;
    pub static kCFBooleanFalse: CFTypeRef;

    pub fn CFRelease(cf: CFTypeRef);
    pub fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
    pub fn CFGetTypeID(cf: CFTypeRef) -> CFTypeID;
    pub fn CFEqual(a: CFTypeRef, b: CFTypeRef) -> Boolean;
    pub fn CFStringGetTypeID() -> CFTypeID;
    pub fn CFArrayGetTypeID() -> CFTypeID;
    pub fn CFBooleanGetTypeID() -> CFTypeID;
    pub fn CFNumberGetTypeID() -> CFTypeID;
    pub fn CFDictionaryGetTypeID() -> CFTypeID;
    pub fn CFArrayGetCount(a: CFArrayRef) -> CFIndex;
    pub fn CFArrayGetValueAtIndex(a: CFArrayRef, i: CFIndex) -> CFTypeRef;
    pub fn CFArrayCreate(alloc: CFTypeRef, values: *const CFTypeRef, n: CFIndex, cb: *const CFArrayCallBacks) -> CFArrayRef;
    pub fn CFStringCreateWithBytes(alloc: CFTypeRef, bytes: *const u8, n: CFIndex, enc: u32, external: Boolean) -> CFStringRef;
    pub fn CFStringGetLength(s: CFStringRef) -> CFIndex;
    pub fn CFStringGetMaximumSizeForEncoding(len: CFIndex, enc: u32) -> CFIndex;
    pub fn CFStringGetCString(s: CFStringRef, buf: *mut u8, size: CFIndex, enc: u32) -> Boolean;
    pub fn CFBooleanGetValue(b: CFTypeRef) -> Boolean;
    pub fn CFNumberGetValue(n: CFTypeRef, ty: CFIndex, out: *mut c_void) -> Boolean;
    pub fn CFNumberCreate(alloc: CFTypeRef, ty: CFIndex, value: *const c_void) -> CFTypeRef;
    pub fn CFDictionaryGetValue(d: CFDictionaryRef, key: CFTypeRef) -> CFTypeRef;
    pub fn CFDictionaryCreate(
        alloc: CFTypeRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        n: CFIndex,
        kcb: *const CFDictionaryKeyCallBacks,
        vcb: *const CFDictionaryValueCallBacks,
    ) -> CFDictionaryRef;
    pub fn CFURLCreateFromFileSystemRepresentation(alloc: CFTypeRef, buf: *const u8, len: CFIndex, is_dir: Boolean) -> CFTypeRef;
    pub fn CFBundleCreate(alloc: CFTypeRef, url: CFTypeRef) -> CFTypeRef;
    pub fn CFBundleGetIdentifier(b: CFTypeRef) -> CFStringRef;
    pub fn CFBundleGetValueForInfoDictionaryKey(b: CFTypeRef, key: CFStringRef) -> CFTypeRef;
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    pub static kAXTrustedCheckOptionPrompt: CFStringRef;
    pub fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> Boolean;
    pub fn AXUIElementCreateApplication(pid: i32) -> AXUIElementRef;
    pub fn AXUIElementCopyAttributeValue(el: AXUIElementRef, attr: CFStringRef, out: *mut CFTypeRef) -> AXError;
    pub fn AXUIElementCopyMultipleAttributeValues(el: AXUIElementRef, attrs: CFArrayRef, options: u32, out: *mut CFArrayRef) -> AXError;
    pub fn AXUIElementCopyActionNames(el: AXUIElementRef, out: *mut CFArrayRef) -> AXError;
    pub fn AXUIElementPerformAction(el: AXUIElementRef, action: CFStringRef) -> AXError;
    pub fn AXUIElementSetAttributeValue(el: AXUIElementRef, attr: CFStringRef, value: CFTypeRef) -> AXError;
    pub fn AXUIElementIsAttributeSettable(el: AXUIElementRef, attr: CFStringRef, out: *mut Boolean) -> AXError;
    pub fn AXUIElementSetMessagingTimeout(el: AXUIElementRef, seconds: f32) -> AXError;
    pub fn AXUIElementCopyElementAtPosition(app: AXUIElementRef, x: f32, y: f32, out: *mut AXUIElementRef) -> AXError;
    pub fn AXUIElementGetPid(el: AXUIElementRef, pid: *mut i32) -> AXError;
    pub fn AXUIElementCreateSystemWide() -> AXUIElementRef;
    pub fn AXValueGetTypeID() -> CFTypeID;
    pub fn AXValueCreate(ty: u32, value: *const c_void) -> CFTypeRef;
    pub fn AXValueGetType(v: CFTypeRef) -> u32;
    pub fn AXValueGetValue(v: CFTypeRef, ty: u32, out: *mut c_void) -> Boolean;
    /// Private but long-stable (used by every window manager): the
    /// CGWindowID of an AX window.
    pub fn _AXUIElementGetWindow(el: AXUIElementRef, id: *mut u32) -> AXError;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    pub static kCGWindowNumber: CFStringRef;
    pub static kCGWindowOwnerPID: CFStringRef;
    pub static kCGWindowOwnerName: CFStringRef;
    pub static kCGWindowLayer: CFStringRef;
    pub static kCGWindowBounds: CFStringRef;
    pub static kCGWindowName: CFStringRef;
    pub static kCGWindowAlpha: CFStringRef;

    pub fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> CFArrayRef;
    pub fn CGRectMakeWithDictionaryRepresentation(dict: CFDictionaryRef, rect: *mut CGRect) -> bool;
    pub fn CGPreflightScreenCaptureAccess() -> bool;
    pub fn CGRequestScreenCaptureAccess() -> bool;
    pub fn CGEventSourceCreate(state: i32) -> CGEventSourceRef;
    pub fn CGEventCreateMouseEvent(source: CGEventSourceRef, ty: u32, at: CGPoint, button: u32) -> CGEventRef;
    pub fn CGEventCreateKeyboardEvent(source: CGEventSourceRef, key: u16, down: bool) -> CGEventRef;
    pub fn CGEventKeyboardSetUnicodeString(event: CGEventRef, len: usize, chars: *const u16);
    pub fn CGEventSetFlags(event: CGEventRef, flags: u64);
    pub fn CGEventSetIntegerValueField(event: CGEventRef, field: u32, value: i64);
    pub fn CGEventCreateScrollWheelEvent2(source: CGEventSourceRef, units: u32, count: u32, w1: i32, w2: i32, w3: i32) -> CGEventRef;
    pub fn CGEventSetLocation(event: CGEventRef, at: CGPoint);
    pub fn CGEventPost(tap: u32, event: CGEventRef);
    pub fn CGEventPostToPid(pid: i32, event: CGEventRef);
    pub fn CGEventSourceSecondsSinceLastEventType(state: i32, ty: u32) -> f64;
}

/// An owned CoreFoundation reference, released on drop.
pub struct Cf(pub CFTypeRef);

impl Cf {
    /// Takes ownership of a +1 reference (Create / Copy rule); `None` if null.
    pub fn owned(r: CFTypeRef) -> Option<Cf> {
        (!r.is_null()).then_some(Cf(r))
    }
    /// Retains a borrowed reference (Get rule).
    pub fn retained(r: CFTypeRef) -> Option<Cf> {
        // SAFETY: r is a live CF object borrowed from its container.
        (!r.is_null()).then(|| Cf(unsafe { CFRetain(r) }))
    }
    pub fn str(s: &str) -> Cf {
        // SAFETY: bytes/len describe a valid UTF-8 buffer.
        Cf(unsafe { CFStringCreateWithBytes(std::ptr::null(), s.as_ptr(), s.len() as CFIndex, kCFStringEncodingUTF8, 0) })
    }
    pub fn f64(v: f64) -> Cf {
        // SAFETY: points at a live f64 for the call.
        Cf(unsafe { CFNumberCreate(std::ptr::null(), kCFNumberDoubleType, &v as *const f64 as *const c_void) })
    }
    pub fn array(items: &[CFTypeRef]) -> Cf {
        // SAFETY: items are live CF objects; the array retains them.
        Cf(unsafe { CFArrayCreate(std::ptr::null(), items.as_ptr(), items.len() as CFIndex, &kCFTypeArrayCallBacks) })
    }
    pub fn dict(pairs: &[(CFTypeRef, CFTypeRef)]) -> Cf {
        let keys: Vec<CFTypeRef> = pairs.iter().map(|p| p.0).collect();
        let vals: Vec<CFTypeRef> = pairs.iter().map(|p| p.1).collect();
        // SAFETY: parallel arrays of live CF objects; the dict retains them.
        Cf(unsafe {
            CFDictionaryCreate(
                std::ptr::null(),
                keys.as_ptr(),
                vals.as_ptr(),
                keys.len() as CFIndex,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            )
        })
    }
}

impl Clone for Cf {
    fn clone(&self) -> Self {
        // SAFETY: self.0 is live while self exists.
        Cf(unsafe { CFRetain(self.0) })
    }
}

impl Drop for Cf {
    fn drop(&mut self) {
        // SAFETY: we own one reference.
        unsafe { CFRelease(self.0) }
    }
}

fn type_is(r: CFTypeRef, id: CFTypeID) -> bool {
    // SAFETY: r is a live CF object.
    !r.is_null() && unsafe { CFGetTypeID(r) } == id
}

/// A CFString as a Rust string.
pub fn string(r: CFTypeRef) -> Option<String> {
    // SAFETY: type checked before use; buffer sized by CF for the encoding.
    unsafe {
        if !type_is(r, CFStringGetTypeID()) {
            return None;
        }
        let len = CFStringGetLength(r);
        let cap = CFStringGetMaximumSizeForEncoding(len, kCFStringEncodingUTF8) + 1;
        let mut buf = vec![0u8; cap as usize];
        if CFStringGetCString(r, buf.as_mut_ptr(), cap, kCFStringEncodingUTF8) == 0 {
            return None;
        }
        let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
        buf.truncate(end);
        String::from_utf8(buf).ok()
    }
}

pub fn number(r: CFTypeRef) -> Option<f64> {
    // SAFETY: type checked before use.
    unsafe {
        if !type_is(r, CFNumberGetTypeID()) {
            return None;
        }
        let mut v = 0f64;
        (CFNumberGetValue(r, kCFNumberDoubleType, &mut v as *mut f64 as *mut c_void) != 0).then_some(v)
    }
}

pub fn boolean(r: CFTypeRef) -> Option<bool> {
    // SAFETY: type checked before use.
    unsafe { type_is(r, CFBooleanGetTypeID()).then(|| CFBooleanGetValue(r) != 0) }
}

pub fn is_array(r: CFTypeRef) -> bool {
    // SAFETY: plain type query.
    type_is(r, unsafe { CFArrayGetTypeID() })
}

pub fn is_number(r: CFTypeRef) -> bool {
    // SAFETY: plain type query.
    type_is(r, unsafe { CFNumberGetTypeID() })
}

/// Borrowed items of a CFArray.
pub fn items(a: CFTypeRef) -> Vec<CFTypeRef> {
    if !is_array(a) {
        return Vec::new();
    }
    // SAFETY: a is a CFArray; indices are in range.
    unsafe { (0..CFArrayGetCount(a)).map(|i| CFArrayGetValueAtIndex(a, i)).collect() }
}

/// The AXError carried by an AXValue placeholder (missing attribute), if any.
pub fn ax_error_value(r: CFTypeRef) -> bool {
    // SAFETY: type checked before use.
    unsafe { type_is(r, AXValueGetTypeID()) && AXValueGetType(r) == kAXValueAXErrorType }
}

pub fn point(r: CFTypeRef) -> Option<CGPoint> {
    let mut p = CGPoint::default();
    // SAFETY: type checked; p matches kAXValueCGPointType.
    unsafe { (type_is(r, AXValueGetTypeID()) && AXValueGetValue(r, kAXValueCGPointType, &mut p as *mut CGPoint as *mut c_void) != 0).then_some(p) }
}

pub fn size(r: CFTypeRef) -> Option<CGSize> {
    let mut s = CGSize::default();
    // SAFETY: type checked; s matches kAXValueCGSizeType.
    unsafe { (type_is(r, AXValueGetTypeID()) && AXValueGetValue(r, kAXValueCGSizeType, &mut s as *mut CGSize as *mut c_void) != 0).then_some(s) }
}
