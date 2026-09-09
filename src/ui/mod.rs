// SPDX-License-Identifier: GPL-3.0-or-later

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
    /// Content-duplicate-group badge (amber-orange), distinct from the burst
    /// badge (mint) and rating stars (gold) — a photo can carry both a burst
    /// and a duplicate-group badge at once, so the colors must never be
    /// confusable at a glance.
    pub const DUP_BADGE: Color32 = Color32::from_rgb(255, 150, 90);
    /// Eyes-closed warning badge. Deliberately the one *cool* badge colour: the
    /// other two mark a frame worth keeping, this one marks a defect, so it
    /// should not read as another kind of award at a glance.
    pub const EYES_BADGE: Color32 = Color32::from_rgb(150, 190, 255);
    /// The site wordmark's "Photos" run (italic, blue) — matches
    /// lightphotos.app's `--lp-accent` custom property, dark-theme value
    /// (`lp.css:11`). The site paints that text with a CSS gradient
    /// (`background-clip: text`) that egui has no equivalent for, so this
    /// is a flat stand-in for the gradient's dominant color; keep it in
    /// sync with `lp.css` if that value ever changes. `site_nav`-only
    /// (wasm32's persistent marketing-site nav bar), hence the cfg gate
    /// unlike every other color in this module.
    #[cfg(target_arch = "wasm32")]
    pub const BRAND_BLUE: Color32 = Color32::from_rgb(79, 140, 255);
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
    /// Toggle content-duplicate (dHash) grouping badges. Independent of the
    /// star filter — unlike bursts, this grouping is order-independent.
    ToggleDupes,
    /// Toggle the "eyes closed" filter: narrow the grid to photos where the
    /// face pass found a blink. Reads the same cache the badge does, so it only
    /// covers photos that pass has actually reached.
    ToggleEyesClosed,
    /// Toggle the Loupe's subject-selection overlay, computing the mask for the
    /// photo on screen the first time it's switched on.
    ToggleSelection,
    /// Swap the overlay between highlighting the subject and the background.
    ToggleSelectionInvert,
    /// Open Survey Mode on the duplicate group containing this visible cell
    /// (a duplicate-badge click in the grid).
    OpenSurvey(usize),
    /// Close Survey Mode, back to the Grid.
    CloseSurvey,
    /// Survey Mode's one-click "keep best, reject rest" action.
    KeepBestRejectRest,
    /// Click on a Survey Mode member: focus it (rating hotkeys then apply to it).
    FocusSurveyMember(usize),
    /// Open this folder as one unit: load its images and toggle its expansion.
    OpenFolder(std::path::PathBuf),
    /// The landing page's "Choose Folder" button and the toolbar "Open" /
    /// `Cmd+O` shortcut — opens a folder picker. On the web that's the File
    /// System Access `showDirectoryPicker`; on native it's the OS folder
    /// dialog (`crate::dialog::pick_folder`). Routed through
    /// `App::open_folder_picker`.
    PickFolder,
    /// The toolbar "Home" button — close the current folder and return to the
    /// landing page (`App::close_folder`). Does not quit the app.
    CloseFolder,
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

mod develop_panel;
mod grid;
mod loupe;
mod modals;
mod survey;
/// Build the egui UI for one frame and return the loupe rect + actions.
mod toolbar;

use develop_panel::draw_develop_panel;
use grid::{draw_folders_panel, draw_grid};
use loupe::draw_loupe;
use modals::{confirm_modal, help_modal, quit_modal};
use survey::draw_survey;
use toolbar::{grid_toolbar, loupe_toolbar};

pub fn draw(ui: &mut egui::Ui, app: &mut App) -> FrameOutput {
    let mut out = FrameOutput::default();

    // The lightphotos.app marketing site's own nav, redrawn in egui —
    // persistent across every screen (landing page, Grid, Loupe), not just
    // the landing page, per direct request. Drawn first so it stacks above
    // everything else `draw` shows this frame.
    #[cfg(target_arch = "wasm32")]
    site_nav(ui);

    // Landing page: shown whenever no folder is open. On the web that's the
    // start state (no CLI arg / AppleEvent path there); on native it's the
    // no-arg launch and the state the "Home" button returns to
    // (`App::close_folder`). Every other draw path below assumes a playlist
    // exists, so this returns early rather than falling through.
    if !app.has_playlist() {
        draw_landing_page(ui, app, &mut out);
        return out;
    }

    let mode = app.mode();

    // Left folder sidebar and right Develop panel are drawn first, outside
    // (before) the toolbar, so egui's panel system — which claims space in
    // call order against the same shrinking `Ui` rect — gives them the full
    // window height, with the toolbar (and, in the loupe, the filmstrip/info
    // bar/central rect below it) confined to the middle column between them.
    // Grid + Loupe only; Survey mode has no sidebar and stays full-width.
    if mode == ViewMode::Grid || mode == ViewMode::Loupe {
        draw_folders_panel(ui, app, &mut out);
    }
    if mode == ViewMode::Loupe && app.develop_visible() {
        draw_develop_panel(ui, app, &mut out);
    }

    // Loupe gets its own, much smaller toolbar (see `toolbar::loupe_toolbar`)
    // instead of the Grid one shown-but-disabled — filter/grouping/bulk
    // actions are Grid concepts that don't apply to one open photo, and
    // letting the filter stay live while a photo was open was the root
    // cause of a real bug: it could silently drop out of the Grid's
    // filtered selection cursor, breaking rating for it.
    if mode == ViewMode::Loupe {
        loupe_toolbar(ui, app, &mut out);
    } else {
        grid_toolbar(ui, app, &mut out);
    }
    match mode {
        ViewMode::Grid => draw_grid(ui, app, &mut out),
        ViewMode::Loupe => draw_loupe(ui, app, &mut out),
        ViewMode::Survey => draw_survey(ui, app, &mut out),
    }
    status_toast(ui, app);
    confirm_modal(ui, app, &mut out);
    quit_modal(ui, app, &mut out);
    help_modal(ui, app, &mut out);
    out
}

/// The "pick a folder to get started" screen — see `draw`'s landing-page
/// branch. Shown on every platform when no folder is open. Deliberately
/// minimal: title + one hint line + one button, no styling investment.
fn draw_landing_page(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    egui::CentralPanel::default().show_inside(ui, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.4);
            ui.heading("LightPhotos");
            ui.add_space(4.0);
            ui.label("Choose a folder of photos to get started");
            ui.add_space(12.0);
            let pending = app.folder_pick_pending();
            let resp = ui.add_enabled(
                !pending,
                egui::Button::new(if pending {
                    "Opening…"
                } else {
                    "Choose Folder"
                })
                .min_size(egui::vec2(160.0, 32.0)),
            );
            if resp.clicked() {
                out.actions.push(UiAction::PickFolder);
            }
        });
    });
    status_toast(ui, app);
}

/// The lightphotos.app marketing site's own nav, redrawn in egui so it's
/// consistent even though this page lives inside the wasm canvas rather than
/// the site's plain-HTML chrome (the canvas wants the full viewport —
/// `overflow: hidden` — so wrapping it in the site's HTML header wasn't an
/// option). Persistent across every screen (landing page, Grid, Loupe), per
/// direct request — called once from `draw`'s own top, before the
/// landing-page early return. Just the "LightPhotos" wordmark, styled to
/// match the real site's own CSS treatment (`lp.css`'s `.lp-wordmark`/
/// `.lp-brand` rules: "Light" in the default ink color, "Photos" italic in
/// the brand blue, abutting with no gap) via a two-section `LayoutJob`; no
/// Downloads/Blogs/Help. Plain, non-interactive text, not a link — an
/// earlier version linked back to lightphotos.app via `hyperlink_to`, but
/// the click never actually opened a tab on web and wasn't worth chasing
/// further, so the link was dropped.
#[cfg(target_arch = "wasm32")]
fn site_nav(ui: &mut egui::Ui) {
    egui::Panel::top("lp_site_nav").show_inside(ui, |ui| {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            let font_id = egui::TextStyle::Heading.resolve(ui.style());
            let mut job = egui::text::LayoutJob::default();
            job.append(
                "Light",
                0.0,
                egui::TextFormat {
                    font_id: font_id.clone(),
                    // PLACEHOLDER = "not explicitly colored"; egui's
                    // text-shape painter substitutes the widget's normal
                    // text color for any PLACEHOLDER glyph at paint time
                    // — exactly "inherit the default ink color" with no
                    // color logic of our own to keep in sync.
                    color: egui::Color32::PLACEHOLDER,
                    ..Default::default()
                },
            );
            job.append(
                "Photos",
                0.0,
                egui::TextFormat {
                    font_id,
                    color: theme::BRAND_BLUE,
                    italics: true,
                    ..Default::default()
                },
            );
            ui.label(job);
        });
        ui.add_space(4.0);
    });
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
