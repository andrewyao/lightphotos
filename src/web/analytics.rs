//! Browser-only, best-effort events. Paths are used solely for local deduplication.
use std::{
    cell::RefCell,
    collections::HashSet,
    path::{Path, PathBuf},
};
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;
use web_time::Instant;

// Reflect and Function::call2 return JavaScript exceptions as Results. No new
// JS asset is needed by the site's four-file WASM deployment process.
#[cfg(target_arch = "wasm32")]
fn lp_event(name: &str, key: &str, value: &str) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let Ok(callback) = js_sys::Reflect::get(&window, &"lpTrack".into()) else {
        return;
    };
    let Some(callback) = callback.dyn_ref::<js_sys::Function>() else {
        return;
    };
    let props = if key.is_empty() {
        JsValue::NULL
    } else {
        let props = js_sys::Object::new();
        if js_sys::Reflect::set(&props, &key.into(), &value.into()).is_err() {
            return;
        }
        props.into()
    };
    let _ = callback.call2(&window, &name.into(), &props);
}

#[derive(Default)]
struct State {
    start: Option<Instant>,
    started: bool,
    first_photo: bool,
    frame_photo: bool,
    failed: HashSet<PathBuf>,
}
thread_local! { static STATE: RefCell<State> = RefCell::new(State::default()); }

pub fn event(name: &'static str) {
    lp_event(name, "", "");
}
pub fn property(name: &'static str, key: &'static str, value: &'static str) {
    lp_event(name, key, value);
}
pub fn start() {
    STATE.with_borrow_mut(|s| s.start = Some(Instant::now()));
}
pub fn started() {
    let send = STATE.with_borrow_mut(|s| !std::mem::replace(&mut s.started, true));
    if send {
        event("app_started");
    }
}
pub fn folder_opened(count: usize) {
    STATE.with_borrow_mut(|s| s.failed.clear());
    property(
        "folder_opened",
        "photo_count_bucket",
        match count {
            0 => "0",
            1..=99 => "1-99",
            100..=999 => "100-999",
            _ => "1000+",
        },
    );
}
pub fn decode_failed(path: &Path, reason: &'static str) {
    let send = STATE.with_borrow_mut(|s| s.failed.insert(path.to_path_buf()));
    if send {
        property("decode_error", "reason", reason);
    }
}
pub fn begin_frame() {
    STATE.with_borrow_mut(|s| s.frame_photo = false);
}
pub fn photo_drawn() {
    STATE.with_borrow_mut(|s| s.frame_photo = true);
}
pub fn presented() {
    let elapsed = STATE.with_borrow_mut(|s| {
        if s.first_photo || !s.frame_photo {
            return None;
        }
        s.first_photo = true;
        s.start.map(|start| start.elapsed().as_millis())
    });
    if let Some(ms) = elapsed {
        event("first_photo_rendered");
        property(
            "first_render_ms",
            "ms_bucket",
            match ms {
                0..=999 => "<1s",
                1000..=4999 => "1-5s",
                5000..=14999 => "5-15s",
                15000..=59999 => "15-60s",
                _ => "60s+",
            },
        );
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
thread_local! { static EVENTS: RefCell<Vec<(String, String, String)>> = const { RefCell::new(Vec::new()) }; }
#[cfg(all(test, not(target_arch = "wasm32")))]
fn lp_event(name: &str, key: &str, value: &str) {
    EVENTS.with_borrow_mut(|events| events.push((name.into(), key.into(), value.into())));
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    fn reset() {
        STATE.with_borrow_mut(|s| *s = State::default());
        EVENTS.with_borrow_mut(Vec::clear);
        start();
    }
    fn names() -> Vec<String> {
        EVENTS.with_borrow(|events| events.iter().map(|e| e.0.clone()).collect())
    }
    #[test]
    fn startup_and_first_photo_are_once_and_require_a_presented_photo() {
        reset();
        started();
        started();
        begin_frame();
        presented(); // Empty app frame.
        begin_frame();
        photo_drawn(); // Surface timed out: no presentation.
        begin_frame();
        presented(); // Next frame contains no photo.
        assert_eq!(names(), ["app_started"]);
        begin_frame();
        photo_drawn();
        presented();
        begin_frame();
        photo_drawn();
        presented();
        assert_eq!(
            names(),
            ["app_started", "first_photo_rendered", "first_render_ms"]
        );
    }
    #[test]
    fn decode_errors_dedupe_across_stages_until_a_folder_load_without_sending_paths() {
        reset();
        let photo = Path::new("/private/photos/sensitive-name.raw");
        decode_failed(photo, "preview");
        decode_failed(photo, "full");
        assert_eq!(names(), ["decode_error"]);
        folder_opened(100);
        decode_failed(photo, "full");
        assert_eq!(names(), ["decode_error", "folder_opened", "decode_error"]);
        EVENTS.with_borrow(|events| {
            assert_eq!(events[1].2, "100-999");
            assert!(events
                .iter()
                .all(|e| !format!("{e:?}").contains("sensitive-name")));
        });
    }
}
