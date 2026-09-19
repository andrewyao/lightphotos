// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared Apple Vision plumbing: run one Vision request against an image file.
//! Vision decodes the file itself, so this never touches our decode pipeline.
//!
//! Keep this module free of other crate modules. `src/bin/face_probe.rs`
//! includes it by `#[path]` because the crate has no lib target.
//!
//! Vision does not read EXIF orientation here. Feature prints don't care, and
//! face detection tolerates roll. To fix it, switch to
//! `initWithURL:orientation:options:` and pass the EXIF value through.

use std::path::Path;

use objc2::rc::Retained;
use objc2::AnyThread;
use objc2_foundation::{NSArray, NSDictionary, NSString, NSURL};
use objc2_vision::{VNImageRequestHandler, VNRequest};

/// Run one Vision request on the image at `path` and block until it finishes.
/// Read results from the request's `results()` afterwards. `Err` means Vision
/// failed; a run that found nothing is `Ok`.
pub fn perform_request(path: &Path, request: &VNRequest) -> Result<(), String> {
    let path_str = path.to_str().ok_or("path is not valid UTF-8")?;
    let ns_path = NSString::from_str(path_str);
    let url = NSURL::fileURLWithPath(&ns_path);
    let options: Retained<NSDictionary<NSString, objc2::runtime::AnyObject>> = NSDictionary::new();

    unsafe {
        let handler = VNImageRequestHandler::initWithURL_options(
            VNImageRequestHandler::alloc(),
            &url,
            &options,
        );
        let requests: Retained<NSArray<VNRequest>> = NSArray::from_slice(&[request]);
        handler
            .performRequests_error(&requests)
            .map_err(|e| e.localizedDescription().to_string())?;
    }
    Ok(())
}
