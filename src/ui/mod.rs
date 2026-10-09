// SPDX-License-Identifier: MIT OR Apache-2.0

//! The egui chrome: Grid, filmstrip, toolbar, panels, and overlays. The wgpu
//! renderer draws the loupe image; this module draws everything around it and
//! returns the loupe rect plus the user's actions for `main.rs` to apply. The
//! Loupe's central panel is transparent so the wgpu image shows through.

use crate::app::{App, CropEdge, FlagCoverage, FocusLevel, Region, ViewMode};
use crate::catalog::Flag;
use crate::develop::Adjustments;
use crate::i18n::{t, Lang};
use crate::navigation::Cmp;

/// An action the UI wants `App` to perform after the frame is built. Positions
/// are indices into the *visible* list (same space as `App::sel`).
#[derive(Debug, PartialEq)]
pub enum UiAction {
    Select(usize),
    /// Group the bursts among every photo in the grid. Native only.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    GroupAllBursts,
    /// Group the bursts among the selected photos.
    GroupSelectedBursts,
    /// Stack the selected photos into one.
    GroupSelected,
    /// Ask to trash every photo in the selected stacks.
    RequestDeleteStack,
    /// Open the stack under the cursor in the Compare pane.
    CompareStack,
    /// Expand or collapse the stack whose badge this cell carries.
    ToggleStack(usize),
    /// Give the selection this color label, or clear it when every photo
    /// has it already.
    ToggleLabel(crate::catalog::ColorLabel),
    /// Score every photo in the grid, group members included.
    ScoreAll,
    /// Show this page of the group's tiles.
    ComparePage(usize),
    /// Native only, as is the row that sends it.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    SetTileFidelity(crate::app::TileFidelity),
    /// A click on this member's tile in the Compare pane, which changes the
    /// picks as `how` says. The representative is never picked.
    PickGroupTile {
        path: std::path::PathBuf,
        how: crate::app::PickHow,
    },
    /// Stars on a member's tile in the Compare pane (0 clears).
    RateGroupMember {
        path: std::path::PathBuf,
        stars: u8,
    },
    /// A flag on a member's tile in the Compare pane (`None` clears).
    FlagGroupMember {
        path: std::path::PathBuf,
        flag: Option<crate::catalog::Flag>,
    },
    /// Move Compare's focus square to center on this point of the photo.
    SetCompareCenter(egui::Pos2),
    /// Make this member of the shown group its representative.
    SetMemberAsRep(std::path::PathBuf),
    /// Ask to trash this member of the shown group, from its tile.
    DeleteMember(std::path::PathBuf),
    /// Ask to confirm trashing the picked members.
    RequestDeletePicks,
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
    /// Open the Lightroom preset picker. Native only; the browser has no
    /// multi-file picker wired up yet.
    #[cfg(not(target_arch = "wasm32"))]
    ImportLrPresets,
    ToggleHelp,
    /// Open the export form, or close it if it is showing.
    ToggleExportForm,
    SetExportSettings(crate::export::ExportSettings),
    /// Export the selection with the form's settings.
    RunExport,
    #[cfg(not(target_arch = "wasm32"))]
    ChooseExportFolder,
    #[cfg(not(target_arch = "wasm32"))]
    SetImmichUrl(String),
    #[cfg(not(target_arch = "wasm32"))]
    SetImmichKey(String),
    #[cfg(not(target_arch = "wasm32"))]
    ConnectImmich,
    #[cfg(not(target_arch = "wasm32"))]
    DisconnectImmich,
    /// Ask to run a bulk action on the current selection (opens a confirm modal).
    RequestBulk(BulkKind),
    /// Score the selection's quality. Read-only on the photos, so no confirm.
    ScoreSelection,
    CancelScoring,
    SetSort(crate::app::GridSort),
    /// Zoom the Loupe to this many screen pixels per source pixel, about
    /// the center.
    SetZoom(f32),
    /// Run the action the open confirm dialog asks about.
    ConfirmPending,
    CancelPending,
    RemoveGroups,
    TrashGroups,
    /// `None` clears the star filter.
    SetFilter(Option<(Cmp, u8)>),
    /// Change the toolbar comparator applied to star-level clicks (≥ / = / ≤).
    SetFilterCmp(Cmp),
    /// Rate the current selection/shown image (0 clears).
    SetRating(u8),
    /// Flag the selected photo (`None` clears).
    SetFlag(Option<crate::catalog::Flag>),
    /// The grid's flag filter.
    SetFlagFilter(crate::navigation::FlagFilter),
    /// The grid's color label filter. Empty shows all.
    SetLabelFilter(Vec<crate::catalog::ColorLabel>),
    /// The Compare pane's flag filter.
    SetCompareFlagFilter(crate::navigation::FlagFilter),
    /// Raw per-frame wheel delta over the filmstrip (egui: positive is up or
    /// left). `App` accumulates it across frames into whole photo steps.
    ScrollFilmstrip(f32),
    /// Show only photos where the face pass found a blink. Covers only photos
    /// the face pass has reached so far.
    ToggleEyesClosed,
    /// Subject-selection overlay. The mask is computed the first time it turns on.
    ToggleSelection,
    ToggleSelectionInvert,
    /// Load this folder's images and toggle its expansion.
    OpenFolder(std::path::PathBuf),
    /// Open the folder picker (`App::open_folder_picker`).
    PickFolder,
    /// Reopen where the user left off, or the folder picker when that folder
    /// is gone (`App::reopen_session`).
    ReopenSession,
    /// Leave the Loupe for the Grid, as the G key does.
    EnterGrid,
    CropGrab(CropEdge),
    /// Begin moving the whole crop rectangle, anchored at this texture coordinate.
    CropGrabMove(f32, f32),
    /// Move the active crop drag to this normalized texture coordinate.
    CropDragTo(f32, f32),
    CropRelease,
    ToggleStraightenTool,
    /// Start the Straighten tool's line at this canvas coordinate.
    StraightenLineFrom(f32, f32),
    StraightenLineTo(f32, f32),
    ResetStraighten,
    /// Put the crop draft back to the full, level frame.
    ResetCrop,
    SetCropAspect(crate::app::CropAspect),
    SetCropOverlay(crate::app::CropOverlay),
    SetCropOrientation(crate::app::CropOrientation),
    /// Rotate the shown photo 90 degrees, clockwise if true.
    Rotate(bool),
    /// While armed, the next Loupe click samples a pixel and solves temp and
    /// tint to make it neutral gray.
    ToggleWbPicker,
    /// The WB picker's armed click landed at this normalized texture coordinate.
    PickWhiteBalance(f32, f32),
    /// A rail icon: turns its page off when it is the one lit, otherwise
    /// shows it.
    ClickRail(crate::app::RailItem),
    ToggleTouchUp,
    SetTouchUpRadius(f32),
    SetTouchUpFeather(f32),
    SetTouchUpOpacity(f32),
    TouchUpClick(f32, f32),
    SelectTouchUp(usize),
    DeleteTouchUp,
    SetAdjustments(Adjustments),
    /// Tick or clear Remove Chromatic Aberration for the loupe photo.
    SetRemoveCa(bool),
    /// The Sliders tab's Reset: tone, color, detail and curve back to
    /// default. Crop, straighten and touch-ups belong to other tabs and stay.
    ResetAdjustments,
    /// The photo menu's Reset: the shown photo's whole edit, touch-ups too.
    ResetAllEdits,
    /// Pick develop settings for each selected photo from its own histogram.
    AutoTone,
    /// Turn the selection black and white, or back to color.
    ToggleBlackAndWhite,
    Focus(Region),
    /// Focus the Toolbar with the keyboard cursor on this control index.
    FocusToolbar(usize),
    /// Focus the Develop panel with the keyboard cursor on this slider index.
    FocusDevelop(usize),
    SetLanguage(Lang),
    SetTheme(theme::Theme),
    SetAutoToneCentering(crate::autotone::Centering),
    /// Open the Settings dialog, or close it if it is showing.
    ToggleSettings,
    CloseSettings,
    /// Start the guided tour from its first stop.
    StartTour,
    /// The tour's next stop; past the last one, the tour ends.
    TourNext,
    TourBack,
    /// Skip or finish the tour.
    EndTour,
}

/// A bulk action requested from the toolbar, run against the current
/// multi-selection after confirmation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BulkKind {
    /// Apply the copied develop settings.
    ApplySettings,
    /// Apply this saved preset.
    ApplyPreset(u64),
    AutoTone,
    /// Auto Tone every photo in the grid, not the selection.
    AutoToneAll,
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
mod export_panel;
pub mod font_size;
mod form;
mod grid;
mod home;
mod info_panel;
mod loupe;
mod modals;
mod photo_menu;
mod tabs;
pub mod theme;
/// `pub(crate)` so `app::nav` can walk `ToolbarControl`, the list the toolbar
/// row is drawn from.
pub(crate) mod toolbar;
pub(crate) mod tour;

#[cfg(test)]
pub(crate) use develop_panel::rail_button_rect;
use develop_panel::{draw_develop_panel, draw_develop_rail};
use export_panel::draw_export_panel;
pub(crate) use form::Role;
use grid::{draw_grid, draw_left_panel};
use info_panel::draw_info_panel;
use loupe::draw_loupe;
pub(crate) use modals::delete_group_tab_order;
use modals::{
    confirm_modal, delete_group_modal, delete_preset_modal, help_modal, preset_name_modal,
    settings_modal,
};
use toolbar::{grid_toolbar, selection_bar};

/// Build the egui UI for one frame.
pub fn draw(ui: &mut egui::Ui, app: &mut App) -> FrameOutput {
    let mut out = FrameOutput::default();
    app.clear_cell_rects();

    // Drawn first and on every screen, so the Open button never moves.
    app_header(ui, app, &mut out);

    // Everything below assumes a folder is open.
    if !app.has_playlist() {
        home::draw_home(ui, app, &mut out);
        tour::draw(ui, app, &mut out);
        return out;
    }

    let mode = app.mode();

    // egui panels claim space in call order. Drawing the side panels before
    // the toolbar gives them full window height and keeps the toolbar in the
    // middle column.
    if app.left_panel_visible() {
        draw_left_panel(ui, app, &mut out);
    }
    // Info, Develop and Export are pages of one right-hand panel, inside the
    // rail that switches between them.
    draw_develop_rail(ui, app, true, &mut out);
    if app.export_form_open() {
        draw_export_panel(ui, app, mode == ViewMode::Loupe, &mut out);
    } else if app.info_open() {
        draw_info_panel(ui, app, mode == ViewMode::Loupe);
    } else if app.develop_page_shown().is_some() {
        draw_develop_panel(ui, app, &mut out);
    }

    // The Loupe has no toolbar. Changing the filter while a photo is open
    // could drop that photo out of the Grid's selection and break rating it.
    // Its actions on the photo sit in its info bar instead.
    folder_title_bar(ui, app, &mut out);
    if mode != ViewMode::Loupe {
        grid_toolbar(ui, app, &mut out);
        selection_bar(ui, app, &mut out);
    }
    match mode {
        ViewMode::Grid => draw_grid(ui, app, &mut out),
        ViewMode::Loupe => draw_loupe(ui, app, &mut out),
    }
    status_toast(ui, app);
    confirm_modal(ui, app, &mut out);
    delete_group_modal(ui, app, &mut out);
    delete_preset_modal(ui, app, &mut out);
    preset_name_modal(ui, app, &mut out);
    help_modal(ui, app, &mut out);
    settings_modal(ui, app, &mut out);
    tour::draw(ui, app, &mut out);
    out
}

/// The "LightPhotos" wordmark and Open Folder at the left; help, Tour and
/// Settings at the right, on every screen. The wordmark copies the
/// lightphotos.app site's `.lp-wordmark` style: "Light" in the default text
/// color, "Photos" in italic brand blue. The web canvas fills the viewport, so
/// the site's HTML header can't wrap it; native draws the same header.
fn app_header(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    let panel = egui::Panel::top("lp_app_header").show_inside(ui, |ui| {
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

            // Not in the F6 focus cycle; `Cmd+O`, `?` and `Cmd+,` are their
            // keyboard routes.
            // The home page shows it switched off, as part of the layout
            // behind its card; the card's own Open Folder is the one to press.
            ui.add_space(12.0);
            if ui
                .add_enabled(app.has_playlist(), egui::Button::new(t().open_folder))
                .on_hover_text(crate::i18n::keys(t().open_folder_tip))
                .clicked()
            {
                out.actions.push(UiAction::PickFolder);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(8.0);
                if ui
                    .button(t().settings)
                    .on_hover_text(crate::i18n::keys(t().settings_tip))
                    .clicked()
                {
                    out.actions.push(UiAction::ToggleSettings);
                }
                if ui.button(t().tour).on_hover_text(t().tour_tip).clicked() {
                    out.actions.push(UiAction::StartTour);
                }
                if ui.button("?").on_hover_text(t().help_tip).clicked() {
                    out.actions.push(UiAction::ToggleHelp);
                }
            });
        });
        ui.add_space(4.0);
    });
    tour::anchor(ui.ctx(), tour::TourStep::Header, panel.response.rect);
}

/// The shown folder's name above the Grid or Loupe. In the Loupe it adds a
/// back arrow to the Grid and the open photo's name and dimensions.
fn folder_title_bar(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    let name = app
        .folder_sel()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default();
    egui::Panel::top("folder_title").show_inside(ui, |ui| {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if app.mode() == ViewMode::Loupe
                && ui
                    .add(egui::Button::new(
                        egui::RichText::new("\u{2190}").size(font_size::px(ui.style(), 18.0)),
                    ))
                    .on_hover_text(t().back_to_grid_tip)
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
            {
                out.actions.push(UiAction::EnterGrid);
            }
            if app.mode() != ViewMode::Loupe {
                ui.label(
                    egui::RichText::new(name)
                        .size(font_size::px(ui.style(), 18.0))
                        .strong(),
                );
            } else {
                loupe_title(ui, app, name);
            }
        });
        ui.add_space(4.0);
    });
}

/// `fx / IMG_1.JPG   4032 × 3024` beside the Loupe's back arrow. The
/// dimensions are smaller and wait for the photo's metadata. `ui.horizontal`
/// centers each label on the row, so the sizes line up on one center line.
fn loupe_title(ui: &mut egui::Ui, app: &App, folder: String) {
    let size = font_size::px(ui.style(), 15.0);
    let gap = font_size::px(ui.style(), 6.0);
    let dim = theme::colors(ui.ctx()).label;
    ui.label(egui::RichText::new(folder).size(size).strong());
    let Some(path) = app.selected_path() else {
        return;
    };
    let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return;
    };
    ui.add_space(gap);
    ui.label(egui::RichText::new("/").size(size).color(dim));
    ui.add_space(gap);
    ui.label(egui::RichText::new(name).size(size));
    if let Some((w, h)) = app.current_metadata().and_then(|m| m.source_size) {
        ui.add_space(3.0 * gap);
        ui.label(
            egui::RichText::new(format!("{w} \u{d7} {h}"))
                .size(font_size::px(ui.style(), 12.0))
                .color(dim),
        );
    }
}

/// A status message (such as an export result) shown bottom-center for a few
/// seconds.
fn status_toast(ui: &egui::Ui, app: &App) {
    let Some((kind, text)) = app.status() else {
        return;
    };
    let colors = theme::colors(ui.ctx()).toast(kind);
    let screen = ui.ctx().content_rect();
    egui::Area::new(egui::Id::new("status_toast"))
        .order(egui::Order::Foreground)
        .fixed_pos(egui::pos2(screen.center().x, screen.max.y - 48.0))
        .pivot(egui::Align2::CENTER_CENTER)
        // The toast repaints only every 250 ms, which stretches egui's fade-in
        // into a washed-out first half second.
        .fade_in(false)
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style())
                .fill(colors.fill)
                .stroke(egui::Stroke::new(1.5_f32, colors.stroke))
                .corner_radius(8)
                .inner_margin(egui::Margin::symmetric(16, 10))
                .show(ui, |ui| {
                    // One line: a wrapped toast reads as two messages.
                    ui.add(
                        egui::Label::new(egui::RichText::new(text).color(colors.text).strong())
                            .extend(),
                    );
                });
        });
    // Keep repainting until the toast expires so it clears on its own.
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(250));
}

fn label_name(label: crate::catalog::ColorLabel) -> &'static str {
    use crate::catalog::ColorLabel::*;
    let m = &t().menu;
    match label {
        Red => m.label_red,
        Yellow => m.label_yellow,
        Green => m.label_green,
        Blue => m.label_blue,
        Purple => m.label_purple,
    }
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

/// The tint a photo's pixels are drawn with: a reject fades toward the
/// background, as in Lightroom, so it reads as set aside while still in view.
fn photo_tint(flag: Option<Flag>) -> egui::Color32 {
    match flag {
        Some(Flag::Reject) => egui::Color32::WHITE.gamma_multiply(0.35),
        _ => egui::Color32::WHITE,
    }
}

/// Paints flag state `flag`'s icon into `rect`: a square flag on a pole for
/// Picked, the same with a cross inside it for Rejected, and a ring for
/// Unflagged. `filled` paints the flag solid, else as an outline; the ring
/// is always an outline. Drawn with strokes rather than a glyph so it needs
/// no font coverage.
fn paint_flag(
    painter: &egui::Painter,
    rect: egui::Rect,
    flag: Option<Flag>,
    color: egui::Color32,
    filled: bool,
) {
    let at = |x: f32, y: f32| rect.min + egui::vec2(x * rect.width(), y * rect.height());
    let stroke = egui::Stroke::new((rect.width() * 0.1).max(1.2), color);
    let Some(flag) = flag else {
        painter.circle_stroke(rect.center(), rect.width() * 0.34, stroke);
        return;
    };
    painter.line_segment([at(0.18, 0.06), at(0.18, 0.96)], stroke);
    let cloth = egui::Rect::from_min_max(at(0.18, 0.08), at(0.9, 0.62));
    if filled {
        painter.rect_filled(cloth, 0.0, color);
    } else {
        painter.rect_stroke(cloth, 0.0, stroke, egui::StrokeKind::Inside);
    }
    if flag == Flag::Reject {
        // On a solid flag the cross is cut out in black so it still reads.
        let cross = if filled {
            egui::Stroke::new(stroke.width, egui::Color32::from_black_alpha(220))
        } else {
            stroke
        };
        let x = cloth.shrink2(egui::vec2(cloth.width() * 0.28, cloth.height() * 0.24));
        painter.line_segment([x.left_top(), x.right_bottom()], cross);
        painter.line_segment([x.right_top(), x.left_bottom()], cross);
    }
}

/// A flag state's color: green for Picked, the danger red for Rejected, and
/// `neutral` for Unflagged.
fn flag_color(
    colors: &theme::Palette,
    flag: Option<Flag>,
    neutral: egui::Color32,
) -> egui::Color32 {
    match flag {
        Some(Flag::Pick) => colors.pick,
        Some(Flag::Reject) => colors.danger,
        None => neutral,
    }
}

/// A clickable flag-state icon `side` wide: solid when the whole selection
/// is in that state, half-transparent when part of it is, and outlined in
/// `idle` when none is, taking its own color on hover. `neutral` colors the
/// Unflagged circle.
fn flag_button(
    ui: &mut egui::Ui,
    flag: Option<Flag>,
    coverage: FlagCoverage,
    side: f32,
    (neutral, idle): (egui::Color32, egui::Color32),
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(side, side), egui::Sense::click());
    paint_flag_mark(ui, rect, flag, coverage, (neutral, idle), resp.hovered());
    resp
}

/// The icon `flag_button` draws, for a caller that places its own hit area.
fn paint_flag_mark(
    ui: &egui::Ui,
    rect: egui::Rect,
    flag: Option<Flag>,
    coverage: FlagCoverage,
    (neutral, idle): (egui::Color32, egui::Color32),
    hovered: bool,
) {
    let full = flag_color(&theme::colors(ui.ctx()), flag, neutral);
    let (color, filled) = match coverage {
        FlagCoverage::All => (full, true),
        FlagCoverage::Some => (full.gamma_multiply(0.5), true),
        FlagCoverage::None if hovered => (full, false),
        FlagCoverage::None => (idle, false),
    };
    paint_flag(
        ui.painter(),
        rect.shrink(rect.width() * 0.1),
        flag,
        color,
        filled,
    );
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
/// move keyboard focus to it on click. `idx` is a position in
/// `toolbar::ToolbarControl::drawn`.
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
            egui::Stroke::new(2.0f32, theme::colors(ui.ctx()).cursor),
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
            egui::Stroke::new(1.0f32, theme::colors(ui.ctx()).cursor),
            egui::StrokeKind::Outside,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::loupe::*;
    use crate::image_decode::Gps;

    #[test]
    fn file_sizes_use_decimal_units() {
        assert_eq!(format_file_size(512), "512 B");
        assert_eq!(format_file_size(45_300), "45.3 KB");
        assert_eq!(format_file_size(3_738_709), "3.7 MB");
        assert_eq!(
            format_file_size(999_990),
            "1.0 MB",
            "rounding carries up a unit"
        );
        assert_eq!(format_file_size(52_000_000_000), "52.0 GB");
    }

    #[test]
    fn dimensions_show_megapixels() {
        assert_eq!(format_dimensions(4032, 3024), "4032 \u{d7} 3024 (12.2 MP)");
    }

    #[test]
    fn focal_length_keeps_a_decimal_only_when_it_has_one() {
        assert_eq!(format_focal_length(50.0), "50 mm");
        assert_eq!(format_focal_length(4.2), "4.2 mm");
    }

    #[test]
    fn exposure_bias_is_signed_and_zero_is_bare() {
        assert_eq!(format_exposure_bias(4.0 / 3.0), "+1.3 EV");
        assert_eq!(format_exposure_bias(-2.0 / 3.0), "-0.7 EV");
        assert_eq!(format_exposure_bias(0.0), "0 EV");
        assert_eq!(format_exposure_bias(-0.0), "0 EV");
    }

    #[test]
    fn coordinates_show_the_hemisphere_instead_of_a_sign() {
        assert_eq!(format_latitude(37.5385117), "37.53851\u{b0} N");
        assert_eq!(format_latitude(-33.86), "33.86000\u{b0} S");
        assert_eq!(format_longitude(-122.2409883), "122.24099\u{b0} W");
        assert_eq!(format_longitude(151.2), "151.20000\u{b0} E");
        assert_eq!(format_altitude(-12.5), "-12 m");
    }

    #[test]
    fn maps_link_keeps_the_signs() {
        let gps = Gps {
            lat: -33.86,
            lon: 151.2,
            alt: None,
        };
        assert_eq!(
            maps_url(&gps),
            "https://www.google.com/maps/search/?api=1&query=-33.860000,151.200000"
        );
    }

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
    fn exposure_parts_follow_lightroom_order() {
        let meta = crate::image_decode::ImageMetadata {
            f_number: Some(2.8),
            iso: Some(200),
            exposure_time: Some(1.0 / 125.0),
            focal_length: Some(55.0),
            ..Default::default()
        };
        assert_eq!(
            exposure_parts(&meta),
            ["ISO 200", "55 mm", "f/2.8", "1/125 s"]
        );
    }

    #[test]
    fn exposure_parts_omit_missing_fields() {
        let meta = crate::image_decode::ImageMetadata {
            exposure_time: Some(2.0),
            ..Default::default()
        };
        assert_eq!(exposure_parts(&meta), ["2 s"]);
    }
}
