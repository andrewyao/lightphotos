// SPDX-License-Identifier: GPL-3.0-or-later

//! Finder "open document" handling.
//!
//! winit 0.30 installs its own app delegate and panics if it is replaced, and
//! that delegate drops Finder open events. So we add an `application:openURLs:`
//! method to winit's delegate class at runtime and forward each path into the
//! event loop through an `EventLoopProxy`.

use std::path::PathBuf;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::OnceLock;

#[cfg(target_os = "macos")]
use std::ffi::c_char;

#[cfg(target_os = "macos")]
use objc2::ffi;
#[cfg(target_os = "macos")]
use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
#[cfg(target_os = "macos")]
use objc2_foundation::{NSArray, NSURL};

#[cfg(not(target_arch = "wasm32"))]
use winit::event_loop::EventLoopProxy;

/// Events delivered from the OS into the winit event loop.
#[derive(Debug, Clone)]
pub enum UserEvent {
    /// Finder (or CLI) asked us to open this image file.
    OpenFile(PathBuf),
}

/// Set once in `main`; read by the injected Objective-C method.
#[cfg(not(target_arch = "wasm32"))]
static PROXY: OnceLock<EventLoopProxy<UserEvent>> = OnceLock::new();

#[cfg(not(target_arch = "wasm32"))]
pub fn set_proxy(proxy: EventLoopProxy<UserEvent>) {
    let _ = PROXY.set(proxy);
}

/// The body of `-[WinitApplicationDelegate application:openURLs:]`.
#[cfg(target_os = "macos")]
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

/// Add `application:openURLs:` to winit's delegate class. Returns true if the
/// method was installed. Call it after winit registers the class but before
/// AppKit dispatches the launch-time open event.
#[cfg(target_os = "macos")]
pub fn install_open_handler() -> bool {
    let class = match AnyClass::get(c"WinitApplicationDelegate") {
        Some(c) => c,
        None => return false,
    };

    // SAFETY: the type encoding "v@:@@" (void; self, _cmd, id, id) matches both
    // `application_open_urls` and the `application:openURLs:` selector.
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

/// No-op off macOS: other desktops pass the file as a CLI argument instead.
#[cfg(all(not(target_os = "macos"), not(target_arch = "wasm32")))]
pub fn install_open_handler() -> bool {
    false
}
