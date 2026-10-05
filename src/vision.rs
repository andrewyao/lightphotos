// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared Apple Vision plumbing: run Vision requests against an image file or
//! an image already in memory. Given a file, Vision decodes it itself, so
//! this never touches our decode pipeline.
//!
//! Keep this module free of other crate modules. The probe binaries in
//! `src/bin/` include it by `#[path]` because the crate has no lib target.
//!
//! Vision does not read EXIF orientation from a file here. Feature prints
//! don't care, and face detection tolerates roll. To fix it, switch to
//! `initWithURL:orientation:options:` and pass the EXIF value through.

use std::path::Path;

use objc2::rc::Retained;
use objc2::runtime::AnyClass;
use objc2::AnyThread;
use objc2_core_graphics::CGImage;
use objc2_foundation::{NSArray, NSDictionary, NSString, NSURL};
use objc2_vision::{VNImageRequestHandler, VNRequest};

/// What a request handler reads pixels from.
pub enum Source<'a> {
    File(&'a Path),
    /// Already decoded and upright, so one decode serves every request.
    Image(&'a CGImage),
}

/// Run `requests` on one handler over `source` and block until they finish.
/// Read results from each request's `results()` afterwards. `Err` means
/// Vision failed; a run that found nothing is `Ok`.
pub fn perform(source: Source, requests: &[&VNRequest]) -> Result<(), String> {
    let options: Retained<NSDictionary<NSString, objc2::runtime::AnyObject>> = NSDictionary::new();
    let handler = match source {
        Source::File(path) => {
            let path_str = path.to_str().ok_or("path is not valid UTF-8")?;
            let url = NSURL::fileURLWithPath(&NSString::from_str(path_str));
            // SAFETY: `options` is an empty dictionary of the declared type.
            unsafe {
                VNImageRequestHandler::initWithURL_options(
                    VNImageRequestHandler::alloc(),
                    &url,
                    &options,
                )
            }
        }
        // SAFETY: as above; the handler retains `image`.
        Source::Image(image) => unsafe {
            VNImageRequestHandler::initWithCGImage_options(
                VNImageRequestHandler::alloc(),
                image,
                &options,
            )
        },
    };
    let requests: Retained<NSArray<VNRequest>> = NSArray::from_slice(requests);
    handler
        .performRequests_error(&requests)
        .map_err(|e| e.localizedDescription().to_string())
}

/// Many Vision requests postdate the app's own `LSMinimumSystemVersion` of
/// 11.0, and `ClassType::new` aborts on a class the runtime never registered.
/// Ask the runtime by name first so an older system takes the ordinary error
/// path instead of killing the process.
pub fn require_class(name: &std::ffi::CStr, needs: &str) -> Result<(), String> {
    if AnyClass::get(name).is_some() {
        return Ok(());
    }
    Err(format!(
        "{} needs macOS {needs} or later",
        name.to_string_lossy()
    ))
}
