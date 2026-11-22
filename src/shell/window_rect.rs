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

    /// The part of `self` that lies within `o`, or `None` when they are
    /// disjoint (a shared edge, zero area, counts as disjoint).
    fn overlap(self, o: Rect) -> Option<Rect> {
        let x = self.x.max(o.x);
        let y = self.y.max(o.y);
        let right = (self.x + self.w).min(o.x + o.w);
        let bottom = (self.y + self.h).min(o.y + o.h);
        (right > x && bottom > y).then_some(Rect {
            x,
            y,
            w: right - x,
            h: bottom - y,
        })
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

/// The smallest on-screen strip (logical points) a restored window must still
/// show, so a grab-able edge and title bar stay reachable.
const MIN_VISIBLE: (f64, f64) = (120.0, 48.0);

/// Fits a `saved` rectangle to the connected `monitors`: clamps its size to
/// [`MAX_SHARE`] of the monitor it sits on most, and accepts it only if, after
/// that clamp, it still shows at least [`MIN_VISIBLE`] on that monitor. Returns
/// `None` when nothing qualifies (no monitor, a stale off-screen rect, or a
/// hostile size like `1e300`), so the caller falls back to the default.
fn fit_saved(saved: Rect, monitors: &[Rect]) -> Option<Rect> {
    let area = |r: Rect| r.w * r.h;
    let best = monitors
        .iter()
        .copied()
        .filter_map(|m| saved.overlap(m).map(|ov| (m, area(ov))))
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(m, _)| m)?;
    let clamped = Rect {
        x: saved.x,
        y: saved.y,
        w: saved.w.min(best.w * MAX_SHARE),
        h: saved.h.min(best.h * MAX_SHARE),
    };
    let visible = clamped.overlap(best)?;
    (visible.w >= MIN_VISIBLE.0 && visible.h >= MIN_VISIBLE.1).then_some(clamped)
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
        .and_then(|r| fit_saved(r, &monitors));
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
    fn a_fully_visible_rect_is_kept_as_saved() {
        let screen = Rect {
            x: 0.0,
            y: 0.0,
            w: 1512.0,
            h: 982.0,
        };
        let saved = Rect {
            x: 100.0,
            y: 50.0,
            w: 1300.0,
            h: 850.0,
        };
        assert_eq!(fit_saved(saved, &[screen]), Some(saved));
    }

    #[test]
    fn a_rect_on_a_detached_monitor_falls_back() {
        let screen = Rect {
            x: 0.0,
            y: 0.0,
            w: 1512.0,
            h: 982.0,
        };
        let off = Rect {
            x: -3000.0,
            y: 0.0,
            w: 1300.0,
            h: 850.0,
        };
        assert_eq!(fit_saved(off, &[screen]), None);
        assert_eq!(fit_saved(off, &[]), None, "no monitor at all");
    }

    #[test]
    fn a_sliver_overlap_is_rejected_so_the_window_is_not_lost_off_screen() {
        let screen = Rect {
            x: 0.0,
            y: 0.0,
            w: 1512.0,
            h: 982.0,
        };
        // Title bar off the top, only ~20 pt of the window peeking onto the
        // screen: not enough to grab, so fall back to the default.
        let barely = Rect {
            x: 400.0,
            y: -830.0,
            w: 1300.0,
            h: 850.0,
        };
        assert!(barely.overlap(screen).is_some(), "it does touch the screen");
        assert_eq!(fit_saved(barely, &[screen]), None);
    }

    #[test]
    fn a_hostile_or_oversized_size_is_clamped_to_the_monitor() {
        let screen = Rect {
            x: 0.0,
            y: 0.0,
            w: 1512.0,
            h: 982.0,
        };
        let huge = Rect {
            x: 0.0,
            y: 0.0,
            w: 1e300,
            h: 1e300,
        };
        let got = fit_saved(huge, &[screen]).expect("origin is on-screen");
        assert_eq!((got.w, got.h), (1512.0 * MAX_SHARE, 982.0 * MAX_SHARE));
        assert!(got.w.is_finite() && got.h.is_finite());
    }

    #[test]
    fn the_default_fits_a_small_monitor_and_keeps_its_size_on_a_big_one() {
        assert_eq!(default_size(Some((2560.0, 1440.0))), (1440.0, 900.0));
        assert_eq!(default_size(Some((1280.0, 800.0))), (1152.0, 720.0));
        assert_eq!(default_size(None), (1440.0, 900.0));
    }
}
