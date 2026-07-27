// SPDX-License-Identifier: MIT OR Apache-2.0

//! egui chrome: the thumbnail Grid, the Loupe filmstrip, the filter bar, and
//! rating overlays. The GPU renderer draws the loupe image itself; this module
//! draws everything around it and reports back the central image rect plus any
//! user actions (clicks, slider, filter changes) for `main.rs` to apply.
//!
//! Built against egui 0.34's `Panel`/`show_inside` API: `main.rs` runs us with
//! the root background `Ui` (from `Context::run_ui`), and we nest panels inside
//! it. The Loupe leaves its central region frameless/transparent so the wgpu
//! image shows through.


use crate::app::{App, CropEdge, FocusLevel, Region, ViewMode};
use crate::develop::Adjustments;
use crate::navigation::Cmp;

/// Shared palette. Several of these colors were previously duplicated as inline
/// `from_rgb(...)` literals across the grid and filmstrip cells; naming them
/// keeps the two views in sync.
mod theme {
    use egui::Color32;
    /// Star rating overlay (gold).
    pub const STAR_GOLD: Color32 = Color32::from_rgb(255, 210, 80);
    /// Selection outline on a thumbnail cell (blue).
    pub const SELECTION_BLUE: Color32 = Color32::from_rgb(90, 160, 255);
    /// Background tint behind the selected/active cell.
    pub const SELECTION_BG: Color32 = Color32::from_rgb(40, 80, 140);
    /// Keyboard-cursor outline (amber) — distinct from the blue mouse selection,
    /// used for the folder-tree cursor and the focused Develop slider.
    pub const CURSOR_AMBER: Color32 = Color32::from_rgb(255, 190, 90);
    /// Best-of-burst winner badge (mint green = "the keeper"), distinct from the
    /// gold rating stars so the two overlays never read as the same mark.
    pub const BURST_BADGE: Color32 = Color32::from_rgb(120, 230, 160);
}

/// An action the UI wants `App` to perform after the frame is built. Positions
/// are indices into the *visible* list (same space as `App::sel`).
pub enum UiAction {
    /// Select the cell at this visible position (plain click / single select).
    Select(usize),
    /// Cmd-click: toggle this cell in the multi-selection.
    SelectToggle(usize),
    /// Shift-click: extend the range selection to this cell.
    SelectRange(usize),
    /// Open the loupe on this visible position.
    OpenLoupe(usize),
    /// Copy the primary photo's develop settings to the in-app clipboard.
    CopySettings,
    /// Show/hide the keyboard-shortcut help overlay.
    ToggleHelp,
    /// Switch to the Loupe (single-image / edit) view.
    EnterLoupe,
    /// Switch to the Grid (thumbnail) view.
    EnterGrid,
    /// Confirm quitting the app (from the Esc quit-confirmation modal).
    ConfirmQuit,
    /// Dismiss the quit-confirmation modal without quitting.
    CancelQuit,
    /// Ask to run a bulk action on the current selection (opens a confirm modal).
    RequestBulk(BulkKind),
    /// Confirm the pending bulk action.
    ConfirmBulk,
    /// Dismiss the pending bulk action without running it.
    CancelBulk,
    /// Set the thumbnail size (longest-side px).
    SetThumbPx(u32),
    /// Set (or clear) the star filter.
    SetFilter(Option<(Cmp, u8)>),
    /// Change the toolbar comparator applied to star-level clicks (≥ / = / ≤).
    SetFilterCmp(Cmp),
    /// Rate the current selection/shown image (0 clears).
    SetRating(u8),
    /// Mouse-wheel scroll over the loupe filmstrip: step through photos.
    /// Carries the raw per-frame wheel delta (egui convention: positive =
    /// scroll up/left), accumulated in `App` across frames into whole steps.
    ScrollFilmstrip(f32),
    /// Toggle best-of-burst detection (badges + dimming). Ignored while a star
    /// filter is active.
    ToggleBursts,
    /// Open this folder as one unit: load its images and toggle its expansion.
    OpenFolder(std::path::PathBuf),
    /// Begin dragging this crop edge (pointer pressed near it).
    CropGrab(CropEdge),
    /// Begin moving the whole crop rectangle, anchored at this texture coordinate.
    CropGrabMove(f32, f32),
    /// Move the active crop drag to this normalized texture coordinate.
    CropDragTo(f32, f32),
    /// Release the active crop drag (drag ended).
    CropRelease,
    /// Arm/disarm the White Balance gray-picker: while armed, the next Loupe
    /// click samples that pixel and solves temp/tint to neutralize it.
    ToggleWbPicker,
    /// The WB picker's armed click landed at this normalized texture coordinate.
    PickWhiteBalance(f32, f32),
    ToggleTouchUp,
    SetTouchUpRadius(f32),
    TouchUpClick(f32, f32),
    SelectTouchUp(usize),
    DeleteTouchUp,
    UndoTouchUp,
    /// Set the develop adjustments for the current loupe image.
    SetAdjustments(Adjustments),
    /// Reset the current loupe image's develop adjustments to identity.
    ResetAdjustments,
    /// Give keyboard focus to this region (e.g. the user clicked into its panel).
    Focus(Region),
    /// Clicked toolbar control at this index: give the Toolbar keyboard focus,
    /// with the cursor on this control (see `toolbar_focus_sync`).
    FocusToolbar(usize),
    /// Clicked/dragged this Develop slider: give the Develop panel keyboard
    /// focus, with the cursor on this slider.
    FocusDevelop(usize),
}

/// A bulk action requested from the toolbar, run against the current
/// multi-selection after confirmation.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BulkKind {
    /// Set every selected photo's rating (0 clears it).
    Rate(u8),
    /// Export every selected photo as a baked JPG.
    Export,
    /// Apply the copied develop settings to every selected photo.
    ApplySettings,
    /// Move every selected photo to the Trash.
    Delete,
}

/// What `draw` returns to `main.rs` each frame.
#[derive(Default)]
pub struct FrameOutput {
    /// The central rect (logical px) left for the loupe image, in Loupe mode.
    pub loupe_rect: Option<egui::Rect>,
    /// Actions to apply this frame.
    pub actions: Vec<UiAction>,
}

/// Build the egui UI for one frame and return the loupe rect + actions.

mod toolbar;
mod modals;
mod grid;
mod loupe;
mod develop_panel;

use toolbar::global_toolbar;
use grid::draw_grid;
use loupe::draw_loupe;
use modals::{confirm_modal, help_modal, quit_modal};

pub fn draw(ui: &mut egui::Ui, app: &mut App) -> FrameOutput {
    let mut out = FrameOutput::default();
    // Global toolbar first, so it reserves height above both modes (and, in the
    // loupe, before the frameless central rect is read).
    global_toolbar(ui, app, &mut out);
    match app.mode() {
        ViewMode::Grid => draw_grid(ui, app, &mut out),
        ViewMode::Loupe => draw_loupe(ui, app, &mut out),
    }
    status_toast(ui, app);
    confirm_modal(ui, app, &mut out);
    quit_modal(ui, app, &mut out);
    help_modal(ui, app, &mut out);
    out
}

/// A transient status message (e.g. an export result), shown bottom-center for a
/// few seconds. Requests a repaint so it disappears without further input.
fn status_toast(ui: &egui::Ui, app: &App) {
    let Some(text) = app.status_text() else {
        return;
    };
    let screen = ui.ctx().content_rect();
    egui::Area::new(egui::Id::new("status_toast"))
        .order(egui::Order::Foreground)
        .fixed_pos(egui::pos2(screen.center().x, screen.max.y - 48.0))
        .pivot(egui::Align2::CENTER_CENTER)
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style())
                .fill(egui::Color32::from_black_alpha(210))
                .show(ui, |ui| {
                    ui.label(egui::RichText::new(text).color(egui::Color32::WHITE));
                });
        });
    // Keep repainting until the toast expires so it clears on its own.
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(250));
}

/// Stars as a compact string, e.g. 3 → "★★★☆☆".
fn star_string(stars: u8) -> String {
    let s = stars.min(5) as usize;
    let mut out = String::new();
    for _ in 0..s {
        out.push('\u{2605}'); // ★
    }
    for _ in s..5 {
        out.push('\u{2606}'); // ☆
    }
    out
}

/// Draw the amber keyboard-cursor outline on `resp` if it's the Toolbar's
/// `idx`-th keyboard-focusable control, and sync keyboard focus to it on
/// click — mirrors the folder-tree cursor (`folder_node`) and Develop slider
/// (`slider`) patterns. Index order here must match `App::activate_toolbar_focus`.
fn toolbar_focus_sync(
    ui: &egui::Ui,
    app: &App,
    idx: usize,
    resp: &egui::Response,
    out: &mut FrameOutput,
) {
    if app.focus() == Region::Toolbar
        && app.focus_level() == FocusLevel::Entered
        && app.toolbar_focus() == idx
    {
        ui.painter().rect_stroke(
            resp.rect.expand(2.0),
            2.0,
            egui::Stroke::new(2.0f32, theme::CURSOR_AMBER),
            egui::StrokeKind::Outside,
        );
    }
    if resp.clicked() {
        out.actions.push(UiAction::FocusToolbar(idx));
    }
}

/// Draw the region-level focus marker used when F6 has selected a panel. The
/// Develop panel keeps its border while an individual control is active too.
fn region_focus_marker(ui: &egui::Ui, app: &App, region: Region) {
    let selected = app.focus() == region && app.focus_level() == FocusLevel::Selected;
    let develop_active = region == Region::Develop && app.focus() == Region::Develop;
    if selected || develop_active {
        ui.painter().rect_stroke(
            ui.min_rect().expand(1.0),
            2.0,
            egui::Stroke::new(1.0f32, theme::CURSOR_AMBER),
            egui::StrokeKind::Outside,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::loupe::{format_capture_date, format_shutter};

    #[test]
    fn format_shutter_sub_second_is_a_fraction() {
        assert_eq!(format_shutter(1.0 / 250.0), "1/250s");
        assert_eq!(format_shutter(1.0 / 60.0), "1/60s");
    }

    #[test]
    fn format_shutter_whole_seconds_has_no_decimal() {
        assert_eq!(format_shutter(2.0), "2s");
        assert_eq!(format_shutter(10.0), "10s");
    }

    #[test]
    fn format_shutter_fractional_seconds_keeps_one_decimal() {
        assert_eq!(format_shutter(1.6), "1.6s");
    }

    #[test]
    fn format_capture_date_formats_month_day_year_and_12h_clock() {
        assert_eq!(
            format_capture_date(2026, 7, 14, 15, 42),
            "Jul 14, 2026 3:42 PM"
        );
        assert_eq!(
            format_capture_date(2026, 1, 1, 0, 5),
            "Jan 1, 2026 12:05 AM"
        );
        assert_eq!(
            format_capture_date(2026, 1, 1, 12, 0),
            "Jan 1, 2026 12:00 PM"
        );
    }
}
