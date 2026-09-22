// SPDX-License-Identifier: GPL-3.0-or-later

//! The egui chrome: Grid, filmstrip, toolbar, panels, and overlays. The wgpu
//! renderer draws the loupe image; this module draws everything around it and
//! returns the loupe rect plus the user's actions for `main.rs` to apply. The
//! Loupe's central panel is transparent so the wgpu image shows through.

use crate::app::{App, CropEdge, FocusLevel, Region, ViewMode};
use crate::develop::Adjustments;
use crate::i18n::{t, Lang};
use crate::navigation::Cmp;

/// Colors shared by the grid, filmstrip, and panels.
mod theme {
    use egui::Color32;
    pub const STAR_GOLD: Color32 = Color32::from_rgb(255, 210, 80);
    /// Mouse selection outline on a thumbnail cell.
    pub const SELECTION_BLUE: Color32 = Color32::from_rgb(90, 160, 255);
    pub const SELECTION_BG: Color32 = Color32::from_rgb(40, 80, 140);
    /// Keyboard cursor outline, kept distinct from the blue mouse selection.
    pub const CURSOR_AMBER: Color32 = Color32::from_rgb(255, 190, 90);
    /// The badge colors differ from each other and from the stars because one
    /// photo can carry several badges at once. Eyes-closed is the only cool
    /// color because it marks a defect, not a keeper.
    pub const BURST_BADGE: Color32 = Color32::from_rgb(120, 230, 160);
    pub const DUP_BADGE: Color32 = Color32::from_rgb(255, 150, 90);
    pub const EYES_BADGE: Color32 = Color32::from_rgb(150, 190, 255);
    /// The wordmark's "Photos" color. Matches `--lp-accent` in lightphotos.app's
    /// `lp.css`. The site uses a gradient that egui can't draw, so this is its
    /// dominant color. Update it if `lp.css` changes.
    pub const BRAND_BLUE: Color32 = Color32::from_rgb(79, 140, 255);
    /// Text of the destructive Delete action.
    pub const DANGER_RED: Color32 = Color32::from_rgb(235, 95, 95);
}

/// An action the UI wants `App` to perform after the frame is built. Positions
/// are indices into the *visible* list (same space as `App::sel`).
#[derive(Debug, PartialEq)]
pub enum UiAction {
    Select(usize),
    /// Cmd-click: toggle this cell in the multi-selection.
    SelectToggle(usize),
    /// Shift-click: extend the range selection to this cell.
    SelectRange(usize),
    OpenLoupe(usize),
    /// Copy the primary photo's develop settings to the in-app clipboard.
    CopySettings,
    /// Open the name prompt for a new preset, seeded with a suggestion.
    SavePresetPrompt,
    /// Open the name prompt on an existing preset.
    RenamePresetPrompt(u64),
    /// The name prompt's field changed.
    SetPresetNameText(String),
    CommitPresetName,
    CancelPresetName,
    /// Apply this preset to the shown photo.
    ApplyPreset(u64),
    /// Ask to delete this preset (opens its own confirm modal).
    RequestDeletePreset(u64),
    ConfirmDeletePreset,
    CancelDeletePreset,
    /// Open the Lightroom preset picker. Native only; the browser has no
    /// multi-file picker wired up yet.
    #[cfg(not(target_arch = "wasm32"))]
    ImportLrPresets,
    ToggleHelp,
    ConfirmQuit,
    CancelQuit,
    /// Ask to run a bulk action on the current selection (opens a confirm modal).
    RequestBulk(BulkKind),
    ConfirmBulk,
    CancelBulk,
    /// `None` clears the star filter.
    SetFilter(Option<(Cmp, u8)>),
    /// Change the toolbar comparator applied to star-level clicks (≥ / = / ≤).
    SetFilterCmp(Cmp),
    /// Rate the current selection/shown image (0 clears).
    SetRating(u8),
    /// Raw per-frame wheel delta over the filmstrip (egui: positive is up or
    /// left). `App` accumulates it across frames into whole photo steps.
    ScrollFilmstrip(f32),
    /// Best-of-burst badges and dimming. Ignored while a star filter is active.
    ToggleBursts,
    /// Duplicate-group badges. Unlike bursts, these ignore the star filter
    /// because duplicate grouping does not depend on photo order.
    ToggleDupes,
    /// Show only photos where the face pass found a blink. Covers only photos
    /// the face pass has reached so far.
    ToggleEyesClosed,
    /// Subject-selection overlay. The mask is computed the first time it turns on.
    ToggleSelection,
    ToggleSelectionInvert,
    /// Open Survey Mode on the duplicate group containing this visible cell.
    OpenSurvey(usize),
    CloseSurvey,
    KeepBestRejectRest,
    /// Focus this Survey member so rating hotkeys apply to it.
    FocusSurveyMember(usize),
    /// Load this folder's images and toggle its expansion.
    OpenFolder(std::path::PathBuf),
    /// Open the folder picker (`App::open_folder_picker`).
    PickFolder,
    CropGrab(CropEdge),
    /// Begin moving the whole crop rectangle, anchored at this texture coordinate.
    CropGrabMove(f32, f32),
    /// Move the active crop drag to this normalized texture coordinate.
    CropDragTo(f32, f32),
    CropRelease,
    /// While armed, the next Loupe click samples a pixel and solves temp and
    /// tint to make it neutral gray.
    ToggleWbPicker,
    /// The WB picker's armed click landed at this normalized texture coordinate.
    PickWhiteBalance(f32, f32),
    ToggleTouchUp,
    SetTouchUpRadius(f32),
    TouchUpClick(f32, f32),
    SelectTouchUp(usize),
    DeleteTouchUp,
    UndoTouchUp,
    SetAdjustments(Adjustments),
    ResetAdjustments,
    /// Pick develop settings for the loupe image from its own histogram.
    AutoTone,
    Focus(Region),
    /// Focus the Toolbar with the keyboard cursor on this control index.
    FocusToolbar(usize),
    /// Focus the Develop panel with the keyboard cursor on this slider index.
    FocusDevelop(usize),
    SetLanguage(Lang),
}

/// A bulk action requested from the toolbar, run against the current
/// multi-selection after confirmation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BulkKind {
    /// 0 clears the rating.
    Rate(u8),
    /// Export as JPG with develop settings applied.
    Export,
    /// Apply the copied develop settings.
    ApplySettings,
    /// Apply this saved preset.
    ApplyPreset(u64),
    AutoTone,
    /// Move to the Trash.
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
mod toolbar;

use develop_panel::draw_develop_panel;
use grid::{draw_folders_panel, draw_grid};
use loupe::draw_loupe;
use modals::{confirm_modal, delete_preset_modal, help_modal, preset_name_modal, quit_modal};
use survey::draw_survey;
use toolbar::{grid_toolbar, selection_bar};

/// Build the egui UI for one frame.
pub fn draw(ui: &mut egui::Ui, app: &mut App) -> FrameOutput {
    let mut out = FrameOutput::default();

    // Drawn first and on every screen, so the Open button never moves.
    app_header(ui, app, &mut out);

    // Everything below assumes a folder is open.
    if !app.has_playlist() {
        draw_landing_page(ui, app, &mut out);
        return out;
    }

    let mode = app.mode();

    // egui panels claim space in call order. Drawing the side panels before
    // the toolbar gives them full window height and keeps the toolbar in the
    // middle column. Survey mode has no side panels.
    if mode == ViewMode::Grid || mode == ViewMode::Loupe {
        draw_folders_panel(ui, app, &mut out);
    }
    if mode == ViewMode::Loupe && app.develop_visible() {
        draw_develop_panel(ui, app, &mut out);
    }

    // The Loupe has no toolbar. Changing the filter while a photo is open
    // could drop that photo out of the Grid's selection and break rating it.
    if mode != ViewMode::Loupe {
        grid_toolbar(ui, app, &mut out);
        selection_bar(ui, app, &mut out);
    }
    match mode {
        ViewMode::Grid => draw_grid(ui, app, &mut out),
        ViewMode::Loupe => draw_loupe(ui, app, &mut out),
        ViewMode::Survey => draw_survey(ui, app, &mut out),
    }
    status_toast(ui, app);
    confirm_modal(ui, app, &mut out);
    delete_preset_modal(ui, app, &mut out);
    preset_name_modal(ui, app, &mut out);
    quit_modal(ui, app, &mut out);
    help_modal(ui, app, &mut out);
    out
}

/// Shown when no folder is open.
fn draw_landing_page(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    egui::CentralPanel::default().show_inside(ui, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.4);
            ui.heading("LightPhotos");
            ui.add_space(4.0);
            ui.label(t().landing_prompt);
            ui.add_space(12.0);
            let pending = app.folder_pick_pending();
            let resp = ui.add_enabled(
                !pending,
                egui::Button::new(if pending {
                    t().opening
                } else {
                    t().choose_folder
                })
                .min_size(egui::vec2(160.0, 32.0)),
            );
            if resp.clicked() {
                out.actions.push(UiAction::PickFolder);
            }
            // Web only: picking a folder hands the browser a File System
            // Access permission, so say up front which button to press.
            let allow_note = t().landing_allow_note;
            if !allow_note.is_empty() {
                ui.add_space(8.0);
                ui.scope(|ui| {
                    ui.set_max_width(420.0);
                    ui.vertical_centered(|ui| {
                        ui.label(egui::RichText::new(allow_note).small().weak());
                    });
                });
            }
        });
    });
    status_toast(ui, app);
}

/// The "LightPhotos" wordmark and the Open button. The wordmark copies the
/// lightphotos.app site's `.lp-wordmark` style: "Light" in the default text
/// color, "Photos" in italic brand blue. The web canvas fills the viewport, so
/// the site's HTML header can't wrap it; native draws the same header.
fn app_header(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    egui::Panel::top("lp_app_header").show_inside(ui, |ui| {
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
                    // egui paints PLACEHOLDER glyphs in the widget's normal
                    // text color.
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

            // Not in the F6 focus cycle; `Cmd+O` and `?` are their keyboard
            // routes. The landing page has its own "Choose Folder" button.
            if app.has_playlist() {
                ui.add_space(12.0);
                if ui
                    .button(t().open_folder)
                    .on_hover_text(t().open_folder_tip)
                    .clicked()
                {
                    out.actions.push(UiAction::PickFolder);
                }
                if ui.button("?").on_hover_text(t().help_tip).clicked() {
                    out.actions.push(UiAction::ToggleHelp);
                }
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(8.0);
                let t = t();
                if ui
                    .button(t.other_language)
                    .on_hover_text(t.other_language_tip)
                    .clicked()
                {
                    out.actions.push(UiAction::SetLanguage(t.other));
                }
            });
        });
        ui.add_space(4.0);
    });
}

/// A status message (such as an export result) shown bottom-center for a few
/// seconds.
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

fn label_color(label: crate::catalog::ColorLabel) -> egui::Color32 {
    use crate::catalog::ColorLabel::*;
    match label {
        Red => egui::Color32::from_rgb(230, 70, 70),
        Yellow => egui::Color32::from_rgb(235, 200, 60),
        Green => egui::Color32::from_rgb(80, 190, 90),
        Blue => egui::Color32::from_rgb(70, 130, 230),
        Purple => egui::Color32::from_rgb(160, 90, 210),
    }
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

/// Outline `resp` if it is the Toolbar's keyboard-cursor control `idx`, and
/// move keyboard focus to it on click. `idx` order must match
/// `App::activate_toolbar_focus`.
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

/// Outline a panel selected with F6. The Develop panel keeps the outline while
/// one of its sliders is active.
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
    use super::loupe::format_shutter;

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
}
