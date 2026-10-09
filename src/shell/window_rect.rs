// SPDX-License-Identifier: MIT OR Apache-2.0

//! The main window's size and place: a roomy default for a first launch, and
//! the rectangle it last closed at for every launch after.

use winit::dpi::{LogicalPosition, LogicalSize};
use winit::event_loop::ActiveEventLoop;
use winit::monitor::MonitorHandle;
use winit::window::{Window, WindowAttributes};

const PREF_KEY: &str = "window";
const DEFAULT: (f64, f64) = (1440.0, 900.0);
/// The share of the monitor a default window may take, so its edges and
/// title bar stay clear of the screen's.
const MAX_SHARE: f64 = 0.9;

/// A rectangle in logical points. For the window, `x` and `y` are what winit's
/// `with_position` takes and `w` and `h` size its content.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Rect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

impl Rect {
    /// Reads the saved `"x y w h"`, or `None` for anything else.
    fn parse(s: &str) -> Option<Rect> {
        let v: Vec<f64> = s
            .split_whitespace()
            .map(|n| n.parse().ok().filter(|n: &f64| n.is_finite()))
            .collect::<Option<_>>()?;
        let &[x, y, w, h] = v.as_slice() else {
            return None;
        };
        (w > 0.0 && h > 0.0).then_some(Rect { x, y, w, h })
    }

    fn to_pref(self) -> String {
        format!("{} {} {} {}", self.x, self.y, self.w, self.h)
    }

    fn intersects(self, o: Rect) -> bool {
        self.x < o.x + o.w && o.x < self.x + self.w && self.y < o.y + o.h && o.y < self.y + self.h
    }

    fn of_monitor(m: &MonitorHandle) -> Rect {
        let scale = m.scale_factor();
        let pos = m.position().to_logical::<f64>(scale);
        let size = m.size().to_logical::<f64>(scale);
        Rect {
            x: pos.x,
            y: pos.y,
            w: size.width,
            h: size.height,
        }
    }
}

/// [`DEFAULT`], shrunk to fit `monitor` (logical width and height) if needed.
fn default_size(monitor: Option<(f64, f64)>) -> (f64, f64) {
    match monitor {
        Some((w, h)) => (DEFAULT.0.min(w * MAX_SHARE), DEFAULT.1.min(h * MAX_SHARE)),
        None => DEFAULT,
    }
}

/// Places `attrs` at the saved rectangle while it still overlaps a connected
/// monitor, and otherwise sizes it to the default.
pub(crate) fn apply(attrs: WindowAttributes, event_loop: &ActiveEventLoop) -> WindowAttributes {
    let monitors: Vec<Rect> = event_loop
        .available_monitors()
        .map(|m| Rect::of_monitor(&m))
        .collect();
    let saved = crate::persist::prefs::load(PREF_KEY)
        .and_then(|s| Rect::parse(&s))
        .filter(|r| monitors.iter().any(|m| r.intersects(*m)));
    if let Some(r) = saved {
        return attrs
            .with_inner_size(LogicalSize::new(r.w, r.h))
            .with_position(LogicalPosition::new(r.x, r.y));
    }
    let monitor = event_loop
        .primary_monitor()
        .or_else(|| event_loop.available_monitors().next())
        .map(|m| Rect::of_monitor(&m));
    let (w, h) = default_size(monitor.map(|m| (m.w, m.h)));
    attrs.with_inner_size(LogicalSize::new(w, h))
}

/// Remembers the window's rectangle for the next launch. A full-screen or
/// minimized window is skipped, since its rectangle isn't one to reopen at.
pub(crate) fn save(window: &Window) {
    if window.fullscreen().is_some() || window.is_minimized() == Some(true) {
        return;
    }
    // `with_position` places the content on macOS and the frame elsewhere,
    // so save the same corner it restores. Wayland reports no position and
    // ignores one, so its size is still worth saving.
    let pos = if cfg!(target_os = "macos") {
        window.inner_position()
    } else {
        window.outer_position()
    };
    let scale = window.scale_factor();
    let pos = pos.unwrap_or_default().to_logical::<f64>(scale);
    let size = window.inner_size().to_logical::<f64>(scale);
    let rect = Rect {
        x: pos.x,
        y: pos.y,
        w: size.width,
        h: size.height,
    };
    if let Err(e) = crate::persist::prefs::save(PREF_KEY, &rect.to_pref()) {
        eprintln!("[lightphotos] could not save the window size: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_rect_reads_back() {
        let r = Rect {
            x: -12.5,
            y: 40.0,
            w: 1300.0,
            h: 850.0,
        };
        assert_eq!(Rect::parse(&r.to_pref()), Some(r));
    }

    #[test]
    fn anything_but_four_finite_numbers_and_a_real_size_is_rejected() {
        for bad in [
            "",
            "1 2 3",
            "1 2 3 4 5",
            "a b c d",
            "0 0 NaN 800",
            "0 0 inf 800",
            "0 0 0 800",
            "0 0 1200 -1",
        ] {
            assert_eq!(Rect::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn only_an_overlapping_rect_intersects() {
        let screen = Rect {
            x: 0.0,
            y: 0.0,
            w: 1512.0,
            h: 982.0,
        };
        let at = |x, y| Rect {
            x,
            y,
            w: 1300.0,
            h: 850.0,
        };
        assert!(at(100.0, 50.0).intersects(screen));
        assert!(at(-1200.0, 50.0).intersects(screen), "partly off the left");
        assert!(!at(1512.0, 50.0).intersects(screen), "just past the right");
        assert!(
            !at(-3000.0, 0.0).intersects(screen),
            "on a detached monitor"
        );
    }

    #[test]
    fn the_default_fits_a_small_monitor_and_keeps_its_size_on_a_big_one() {
        assert_eq!(default_size(Some((2560.0, 1440.0))), (1440.0, 900.0));
        assert_eq!(default_size(Some((1280.0, 800.0))), (1152.0, 720.0));
        assert_eq!(default_size(None), (1440.0, 900.0));
    }
}
