//! Finder "open document" handling.
//!
//! winit 0.30 registers its OWN `NSApplicationDelegate` (class
//! `WinitApplicationDelegate`) and panics if you replace the app delegate.
//! winit's delegate does not implement `application:openURLs:`, so Finder open
//! events are otherwise dropped.
//!
//! Solution: at runtime, add an `application:openURLs:` method to winit's
//! delegate class via the Objective-C runtime. AppKit then calls it for
//! double-click / "Open With" / `open -a`, and we forward the path into the
//! winit event loop via an `EventLoopProxy`. We do not touch the delegate
//! object, so winit's identity assertion still holds.

use std::ffi::c_char;
use std::path::PathBuf;
use std::sync::OnceLock;

use objc2::ffi;
use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
use objc2_foundation::{NSArray, NSURL};
use winit::event_loop::EventLoopProxy;

/// Events delivered from the OS into the winit event loop.
#[derive(Debug, Clone)]
pub enum UserEvent {
    /// Finder (or CLI) asked us to open this image file.
    OpenFile(PathBuf),
}

/// Set once in `main`; read by the injected Objective-C method.
static PROXY: OnceLock<EventLoopProxy<UserEvent>> = OnceLock::new();

pub fn set_proxy(proxy: EventLoopProxy<UserEvent>) {
    let _ = PROXY.set(proxy);
}

/// The implementation for `-[WinitApplicationDelegate application:openURLs:]`.
/// Objective-C calls this with (self, _cmd, NSApplication*, NSArray<NSURL>*).
extern "C-unwind" fn application_open_urls(
    _this: *mut AnyObject,
    _cmd: Sel,
    _app: *mut AnyObject,
    urls: *const NSArray<NSURL>,
) {
    let Some(proxy) = PROXY.get() else { return };
    if urls.is_null() {
        return;
    }
    // SAFETY: AppKit passes a valid non-null NSArray<NSURL> for this selector.
    let urls = unsafe { &*urls };
    for url in urls.iter() {
        if let Some(path) = url.path() {
            let _ = proxy.send_event(UserEvent::OpenFile(PathBuf::from(path.to_string())));
        }
    }
}

/// Add `application:openURLs:` to winit's delegate class. Call this once the
/// delegate class is registered (i.e. from `ApplicationHandler::resumed`,
/// which runs inside `applicationDidFinishLaunching:`, before the launch-time
/// open event is dispatched). Returns true if the method was installed.
pub fn install_open_handler() -> bool {
    let class = match AnyClass::get(c"WinitApplicationDelegate") {
        Some(c) => c,
        None => return false,
    };

    // SAFETY: We register the selector, then add a method whose type encoding
    // ("v@:@@": void return; self, _cmd, id, id args) matches both the C
    // function above and the Objective-C `application:openURLs:` selector.
    unsafe {
        let Some(sel) = ffi::sel_registerName(c"application:openURLs:".as_ptr()) else {
            return false;
        };
        let imp: Imp = std::mem::transmute::<*const (), Imp>(application_open_urls as *const ());
        let types = c"v@:@@".as_ptr() as *const c_char;
        let cls = class as *const AnyClass as *mut AnyClass;
        ffi::class_addMethod(cls, sel, imp, types).as_bool()
    }
}
