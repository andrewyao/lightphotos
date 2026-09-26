// SPDX-License-Identifier: GPL-3.0-or-later

//! The egui chrome: Grid, filmstrip, toolbar, panels, and overlays. The wgpu
//! renderer draws the loupe image; this module draws everything around it and
//! returns the loupe rect plus the user's actions for `main.rs` to apply. The
//! Loupe's central panel is transparent so the wgpu image shows through.

use crate::app::{App, CropEdge, FocusLevel, Region, ViewMode};
use crate::develop::Adjustments;
use crate::i18n::{t, Lang};
use crate::navigation::Cmp;

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
    /// Switch to the next color theme.
    CycleTheme,
    SetLeftTab(crate::app::LeftTab),
}

/// A bulk action requested from the toolbar, run against the current
/// multi-selection after confirmation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BulkKind {
    /// 0 clears the rating.
    Rate(u8),
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
mod export_panel;
pub mod font_size;
mod grid;
mod info_panel;
mod loupe;
mod modals;
mod survey;
pub mod theme;
/// `pub(crate)` so `app::nav` can walk `ToolbarControl`, the list the toolbar
/// row is drawn from.
pub(crate) mod toolbar;

use develop_panel::draw_develop_panel;
use export_panel::draw_export_panel;
use grid::{draw_grid, draw_left_panel};
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
        draw_left_panel(ui, app, &mut out);
    }
    if app.export_form_open() {
        draw_export_panel(ui, app, &mut out);
    } else if mode == ViewMode::Loupe && app.develop_visible() {
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

/// Shown when no folder is open: the prompt and Choose Folder button, three
/// cards on what the app does, and a tip on how edits are stored.
fn draw_landing_page(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    const MAX_COLUMN: f32 = 760.0;
    const GUTTER: f32 = 16.0;
    let pal = theme::colors(ui.ctx());
    let card_fill = pal.panel.lerp_to_gamma(pal.value, 0.05);
    let card_stroke = egui::Stroke::new(1.0, pal.panel.lerp_to_gamma(pal.value, 0.14));

    egui::CentralPanel::default().show_inside(ui, |ui| {
        let top = (ui.available_height() * 0.10).max(24.0);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let full = ui.available_rect_before_wrap();
                let col_w = (full.width() - 2.0 * GUTTER).clamp(200.0, MAX_COLUMN);
                let column = egui::Rect::from_min_size(
                    egui::pos2(full.center().x - col_w / 2.0, full.top()),
                    egui::vec2(col_w, full.height()),
                );
                ui.scope_builder(
                    egui::UiBuilder::new()
                        .max_rect(column)
                        .layout(egui::Layout::top_down(egui::Align::Center)),
                    |ui| {
                        ui.add_space(top);
                        ui.label(
                            egui::RichText::new(t().landing_prompt)
                                .size(font_size::px(ui.style(), 32.0))
                                .color(pal.value)
                                .strong(),
                        );
                        ui.add_space(6.0);
                        ui.label(
                            egui::RichText::new(t().landing_tagline)
                                .size(font_size::px(ui.style(), 18.0))
                                .color(pal.label),
                        );
                        ui.add_space(28.0);
                        choose_folder_button(ui, app, out);

                        // Web only: picking a folder hands the browser a File
                        // System Access permission, so say up front which
                        // button to press.
                        let allow_note = t().landing_allow_note;
                        if !allow_note.is_empty() {
                            ui.add_space(20.0);
                            egui::Frame::new()
                                .fill(theme::BRAND_BLUE.linear_multiply(0.10))
                                .stroke(egui::Stroke::new(
                                    1.0,
                                    theme::BRAND_BLUE.linear_multiply(0.6),
                                ))
                                .corner_radius(10.0)
                                .inner_margin(egui::Margin::symmetric(18, 12))
                                .show(ui, |ui| {
                                    ui.label(
                                        egui::RichText::new(allow_note)
                                            .size(font_size::px(ui.style(), 18.0))
                                            .color(pal.value),
                                    );
                                });
                        }

                        ui.add_space(40.0);
                        landing_steps(ui, col_w, card_fill, card_stroke, &pal);
                        ui.add_space(20.0);
                        landing_tip(ui, col_w, &pal);
                        ui.add_space(20.0);
                        ui.label(
                            egui::RichText::new(t().landing_shortcuts_hint)
                                .size(font_size::px(ui.style(), 14.0))
                                .color(pal.label),
                        );
                        ui.add_space(32.0);
                    },
                );
            });
    });
    status_toast(ui, app);
}

/// The landing page's one call to action, filled in the brand blue so it reads
/// as the thing to press.
fn choose_folder_button(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    let pending = app.folder_pick_pending();
    let label = if pending {
        t().opening
    } else {
        t().choose_folder
    };
    let resp = ui.add_enabled(
        !pending,
        egui::Button::new(
            egui::RichText::new(label)
                .size(font_size::px(ui.style(), 20.0))
                .color(egui::Color32::WHITE)
                .strong(),
        )
        .fill(theme::BRAND_BLUE)
        .corner_radius(10.0)
        .min_size(egui::vec2(240.0, 52.0)),
    );
    if resp.hovered() && !pending {
        // A fixed fill gives no hover feedback of its own.
        ui.painter().rect_stroke(
            resp.rect.expand(2.0),
            12.0,
            egui::Stroke::new(2.0, theme::BRAND_BLUE.linear_multiply(0.5)),
            egui::StrokeKind::Outside,
        );
    }
    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
        out.actions.push(UiAction::PickFolder);
    }
}

/// Three numbered cards, Browse / Rate / Develop: side by side when the column
/// is wide enough, stacked otherwise.
fn landing_steps(
    ui: &mut egui::Ui,
    col_w: f32,
    fill: egui::Color32,
    stroke: egui::Stroke,
    pal: &theme::Palette,
) {
    const GAP: f32 = 12.0;
    const MARGIN: i8 = 16;
    let side_by_side = col_w >= 600.0;
    let card_w = if side_by_side {
        (col_w - 2.0 * GAP) / 3.0
    } else {
        col_w
    };
    let inner_w = card_w - 2.0 * MARGIN as f32 - 2.0 * stroke.width;
    let min_h = font_size::px(ui.style(), 96.0);

    let card = |ui: &mut egui::Ui, n: usize, (title, body): (&str, &str)| {
        egui::Frame::new()
            .fill(fill)
            .stroke(stroke)
            .corner_radius(12.0)
            .inner_margin(egui::Margin::same(MARGIN))
            .show(ui, |ui| {
                ui.set_width(inner_w);
                if side_by_side {
                    ui.set_min_height(min_h);
                }
                ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                    ui.horizontal(|ui| {
                        number_badge(ui, n);
                        ui.label(
                            egui::RichText::new(title)
                                .size(font_size::px(ui.style(), 18.0))
                                .color(pal.value)
                                .strong(),
                        );
                    });
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(body)
                            .size(font_size::px(ui.style(), 15.0))
                            .color(pal.label),
                    );
                });
            });
    };

    let steps = t().landing_steps;
    if side_by_side {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = GAP;
            for (i, step) in steps.into_iter().enumerate() {
                card(ui, i + 1, step);
            }
        });
    } else {
        for (i, step) in steps.into_iter().enumerate() {
            if i > 0 {
                ui.add_space(GAP);
            }
            card(ui, i + 1, step);
        }
    }
}

/// A small brand-blue disc with a white step number in it.
fn number_badge(ui: &mut egui::Ui, n: usize) {
    let d = font_size::px(ui.style(), 22.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(d, d), egui::Sense::hover());
    ui.painter()
        .circle_filled(rect.center(), d / 2.0, theme::BRAND_BLUE);
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        n.to_string(),
        egui::FontId::proportional(font_size::px(ui.style(), 13.0)),
        egui::Color32::WHITE,
    );
}

/// The storage tip: left-aligned text in a blue-tinted box with an accent bar
/// down its left edge, the way docs sites set off a note.
fn landing_tip(ui: &mut egui::Ui, col_w: f32, pal: &theme::Palette) {
    const BAR: f32 = 4.0;
    const RADIUS: u8 = 10;
    let resp = egui::Frame::new()
        .fill(theme::BRAND_BLUE.linear_multiply(0.10))
        .stroke(egui::Stroke::new(1.0, theme::BRAND_BLUE.linear_multiply(0.35)))
        .corner_radius(RADIUS)
        .inner_margin(egui::Margin {
            left: 20,
            right: 18,
            top: 14,
            bottom: 14,
        })
        .show(ui, |ui| {
            ui.set_width(col_w - 40.0);
            ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.horizontal(|ui| {
                    info_icon(ui);
                    ui.label(
                        egui::RichText::new(t().landing_tip_title)
                            .size(font_size::px(ui.style(), 16.0))
                            .color(theme::BRAND_BLUE)
                            .strong(),
                    );
                });
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(t().landing_help)
                        .size(font_size::px(ui.style(), 15.0))
                        .color(pal.value),
                );
            });
        })
        .response;
    let r = resp.rect;
    ui.painter().rect_filled(
        egui::Rect::from_min_max(r.min, egui::pos2(r.min.x + BAR, r.max.y)),
        egui::CornerRadius {
            nw: RADIUS,
            sw: RADIUS,
            ne: 0,
            se: 0,
        },
        theme::BRAND_BLUE,
    );
}

/// An "i" in a brand-blue disc, painted so it can't fall back to a tofu box
/// the way an emoji glyph would.
fn info_icon(ui: &mut egui::Ui) {
    let d = font_size::px(ui.style(), 18.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(d, d), egui::Sense::hover());
    ui.painter()
        .circle_filled(rect.center(), d / 2.0, theme::BRAND_BLUE);
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        "i",
        egui::FontId::proportional(font_size::px(ui.style(), 12.0)),
        egui::Color32::WHITE,
    );
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
                let theme_name = match theme::current(ui.ctx()) {
                    theme::Theme::Dark => t.theme_dark,
                    theme::Theme::Medium => t.theme_medium,
                    theme::Theme::Light => t.theme_light,
                };
                if ui.button(theme_name).on_hover_text(t.theme_tip).clicked() {
                    out.actions.push(UiAction::CycleTheme);
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
}
