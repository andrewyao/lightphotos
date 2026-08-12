// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared Apple Vision plumbing.
//!
//! Every Vision-backed feature in lightphotos (`featureprint.rs`'s duplicate
//! refinement, `facequality.rs`'s face/eye detection, and whatever comes next)
//! needs the same four lines of setup: turn a filesystem path into an `NSURL`,
//! hand it to a `VNImageRequestHandler`, run the request, and turn Vision's
//! `NSError` into a `String`. This module owns that once so the feature modules
//! contain only the part that differs — which request they build and how they
//! read its results.
//!
//! Vision decodes the file itself through its own ImageIO-backed path, so none
//! of this touches lightphotos' decode/thumbnail pipeline: a path is the entire
//! input.
//!
//! Deliberately free of other crate modules. `src/bin/face_probe.rs` pulls this
//! in by `#[path]` (the crate has no lib target), which only works while the
//! module's dependencies stop at `objc2`.
//!
//! **Known gap**: `initWithURL:options:` assumes an upright image — Vision does
//! not read the file's EXIF orientation tag. Feature prints don't care (a
//! burst's frames all share an orientation, so comparisons stay apples to
//! apples), and Vision's face detector tolerates roll well enough to still find
//! a sideways face. If landmark quality on rotated portraits turns out to
//! matter, the fix belongs here: switch to `initWithURL:orientation:options:`
//! and thread the EXIF value (`image_decode.rs` already parses it) through.

use std::path::Path;

use objc2::rc::Retained;
use objc2::AnyThread;
use objc2_foundation::{NSArray, NSDictionary, NSString, NSURL};
use objc2_vision::{VNImageRequestHandler, VNRequest};

/// Run one Vision request over the image at `path`, blocking until it
/// finishes. Read the outcome from the request's own `results()` afterwards.
///
/// An `Err` means Vision itself failed (unreadable file, unsupported format,
/// framework error) — a successful run that simply found nothing is `Ok`.
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
