use super::*;

use super::form::{self, Button, Form, Role};
use crate::app::{
    App, CropAspect, CropOrientation, CropOverlay, DevelopTab, FocusLevel, RailItem, Region,
    StraightenTool,
};

/// The right-hand Develop panel, with sliders in Lightroom's order. Pushes one
/// `SetAdjustments` only on frames where a slider changed. Double-clicking a
/// slider resets it to 0.
pub(super) fn draw_develop_panel(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    egui::Panel::right("develop")
        .resizable(true)
        .default_size(340.0)
        .show_inside(ui, |ui| {
            // Scroll rather than overflow, which would push the Loupe's
            // bottom panels off the window when the rows outgrow its height.
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| match app.develop_tab() {
                    DevelopTab::Sliders => draw_sliders_tab(ui, app, out),
                    DevelopTab::Crop => draw_crop_tab(ui, app, out),
                    DevelopTab::Masks => draw_masks_tab(ui, app, out),
                });
            region_focus_marker(ui, app, Region::Develop);
        });
}

/// The rail of page icons at the window's right edge. It is its own panel,
/// outside Develop, so it stays on screen to bring a page back after its own
/// icon turned it off.
pub(super) fn draw_develop_rail(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    egui::Panel::right("develop_rail")
        .resizable(false)
        .exact_size(rail_width(ui))
        .frame(egui::Frame::NONE.fill(ui.visuals().panel_fill))
        .show_inside(ui, |ui| develop_rail(ui, app, out));
}

/// The saved-look library. A plain section header like the slider sections
/// below it rather than a collapsing one, because a library folded away by
/// default is a library nobody finds. The row list scrolls instead, so a long
/// one cannot push the eleven sliders off a short window.
fn draw_presets(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    let t = t();
    ui.label(egui::RichText::new(t.presets).strong());
    ui.horizontal(|ui| {
        if ui.button("+").on_hover_text(t.save_preset_tip).clicked() {
            out.actions.push(UiAction::SavePresetPrompt);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if ui
            .button(t.import_lr_presets)
            .on_hover_text(t.import_lr_presets_tip)
            .clicked()
        {
            out.actions.push(UiAction::ImportLrPresets);
        }
        if app.presets().is_empty() {
            ui.weak(t.no_presets);
        }
    });
    egui::ScrollArea::vertical()
        .max_height(140.0)
        .show(ui, |ui| {
            for preset in app.presets() {
                ui.horizontal(|ui| {
                    let hover = if preset.notes.is_empty() {
                        t.apply_preset_tip.to_string()
                    } else {
                        preset.notes.join("\n")
                    };
                    let row = ui
                        .selectable_label(false, &preset.name)
                        .on_hover_text(hover);
                    if row.clicked() {
                        out.actions.push(UiAction::ApplyPreset(preset.id));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.menu_button("\u{22ef}", |ui| {
                            if ui.button(t.rename).clicked() {
                                out.actions.push(UiAction::RenamePresetPrompt(preset.id));
                                ui.close();
                            }
                            if ui.button(t.delete).clicked() {
                                out.actions.push(UiAction::RequestDeletePreset(preset.id));
                                ui.close();
                            }
                        })
                        .response
                        .on_hover_text(t.preset_actions_tip);
                    });
                });
            }
        });
    ui.add_space(6.0);
}

/// Sliders, Crop, Cleanup, the group's Compare pane and Export as a column
/// of icons, the one on screen lit. Compare is greyed out unless the photo is
/// in a group.
fn develop_rail(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    let margin = font_size::px(ui.style(), RAIL_MARGIN);
    let inner = ui.max_rect().shrink(margin);
    ui.scope_builder(egui::UiBuilder::new().max_rect(inner), |ui| {
        ui.spacing_mut().item_spacing.y = margin;
        let t = t();
        let lit = app.rail_lit();
        let in_group = app.shown_in_group();
        for (item, tip, enabled) in [
            (RailItem::Develop(DevelopTab::Sliders), t.tab_sliders, true),
            (RailItem::Develop(DevelopTab::Crop), t.tab_crop, true),
            (RailItem::Develop(DevelopTab::Masks), t.tab_masks, true),
            (RailItem::GroupCompare, t.view_compare, in_group),
            (RailItem::Export, t.export_jpg_tip, true),
        ] {
            let tip = if enabled {
                tip
            } else {
                t.view_compare_needs_group
            };
            let button =
                ui.add_enabled_ui(enabled, |ui| rail_button(ui, item, tip, lit == Some(item)));
            if button.inner.clicked() {
                out.actions.push(UiAction::ClickRail(item));
            }
        }
    });
}

/// How wide the rail is: one icon and its margins.
fn rail_width(ui: &egui::Ui) -> f32 {
    font_size::px(ui.style(), RAIL_BUTTON + 2.0 * RAIL_MARGIN)
}

const RAIL_BUTTON: f32 = 36.0;
const RAIL_MARGIN: f32 = 6.0;

fn rail_id(item: RailItem) -> egui::Id {
    egui::Id::new(("develop_rail", item))
}

/// Where the rail drew `item`'s icon last frame, for tests that click it.
#[cfg(test)]
pub(crate) fn rail_button_rect(ctx: &egui::Context, item: RailItem) -> Option<egui::Rect> {
    ctx.read_response(rail_id(item)).map(|r| r.rect)
}

/// One of `develop_rail`'s icons, painted in strokes like
/// `eyedropper_button` so it follows the theme.
fn rail_button(ui: &mut egui::Ui, item: RailItem, tip: &str, selected: bool) -> egui::Response {
    let side = font_size::px(ui.style(), RAIL_BUTTON);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(side, side), egui::Sense::hover());
    let response = ui.interact(rect, rail_id(item), egui::Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Button, ui.is_enabled(), selected, tip)
    });
    let visuals = ui.style().interact_selectable(&response, selected);
    let painter = ui.painter();
    if selected || response.hovered() {
        painter.rect_filled(rect, 6.0, visuals.weak_bg_fill);
    }
    let u = font_size::px(ui.style(), 1.0);
    let c = rect.center();
    let p = |x: f32, y: f32| egui::pos2(c.x + x * u, c.y + y * u);
    let stroke = egui::Stroke::new(1.6 * u, visuals.fg_stroke.color);
    let frame = |x0: f32, y0: f32, x1: f32, y1: f32| {
        painter.rect_stroke(
            egui::Rect::from_min_max(p(x0, y0), p(x1, y1)),
            0.8 * u,
            stroke,
            egui::StrokeKind::Middle,
        );
    };
    match item {
        // Three faders, each with its knob at a different height.
        RailItem::Develop(DevelopTab::Sliders) => {
            for (y, knob) in [(-6.0, 3.0), (0.0, -4.0), (6.0, 1.0)] {
                painter.line_segment([p(-9.0, y), p(9.0, y)], stroke);
                painter.line_segment([p(knob, y - 3.0), p(knob, y + 3.0)], stroke);
            }
        }
        // Two corner brackets crossing, as a crop tool's marks do.
        RailItem::Develop(DevelopTab::Crop) => {
            painter.add(egui::Shape::line(
                vec![p(-5.0, -10.0), p(-5.0, 5.0), p(10.0, 5.0)],
                stroke,
            ));
            painter.add(egui::Shape::line(
                vec![p(-10.0, -5.0), p(5.0, -5.0), p(5.0, 10.0)],
                stroke,
            ));
        }
        // An eraser leaning to the right on a baseline, a band across it
        // where the rubber tip starts.
        RailItem::Develop(DevelopTab::Masks) => {
            painter.add(egui::Shape::closed_line(
                vec![p(-10.0, 1.5), p(0.0, -8.5), p(7.0, -1.5), p(-3.0, 8.5)],
                stroke,
            ));
            painter.line_segment([p(-5.0, -3.5), p(2.0, 3.5)], stroke);
            painter.line_segment([p(-3.0, 8.5), p(10.0, 8.5)], stroke);
        }
        // A large frame beside a column of three small ones, as the Compare
        // pane sets the group beside the photo.
        RailItem::GroupCompare => {
            frame(-10.0, -7.0, 2.0, 7.0);
            for top in [-7.0, -2.0, 3.0] {
                frame(4.5, top, 10.0, top + 4.0);
            }
        }
        // An arrow rising out of an open tray.
        RailItem::Export => {
            painter.add(egui::Shape::line(
                vec![
                    p(-5.0, -2.0),
                    p(-9.0, -2.0),
                    p(-9.0, 9.0),
                    p(9.0, 9.0),
                    p(9.0, -2.0),
                    p(5.0, -2.0),
                ],
                stroke,
            ));
            painter.line_segment([p(0.0, 4.0), p(0.0, -10.0)], stroke);
            painter.add(egui::Shape::line(
                vec![p(-4.0, -6.0), p(0.0, -10.0), p(4.0, -6.0)],
                stroke,
            ));
        }
    }
    response
        .on_hover_text(tip)
        .on_disabled_hover_text(tip)
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Crop mode's controls: Reset, rotation, straighten, the ratio and its
/// orientation, and the crop's size in pixels. Nothing saves until Enter,
/// which saves and goes back, as in Lightroom; Esc discards.
fn draw_crop_tab(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    let t = t();
    let (Some(aspect), Some(orientation)) = (app.crop_aspect(), app.crop_orientation()) else {
        return;
    };
    form::page_heading(ui, t.tab_crop, |ui| {
        if ui.button(t.reset).clicked() {
            out.actions.push(UiAction::ResetCrop);
        }
    });
    form::page(ui, |ui| {
        let form = Form::stacked();
        form.section(ui, "", |ui| {
            form.row(ui, t.crop_rotate, |ui| {
                ui.horizontal(|ui| {
                    for (label, tip, clockwise) in [
                        (t.crop_left, t.menu.rotate_left, false),
                        (t.crop_right, t.menu.rotate_right, true),
                    ] {
                        let b = form::Button {
                            label,
                            role: form::Role::Cancel,
                            enabled: true,
                        };
                        if form::button(ui, &b).on_hover_text(tip).clicked() {
                            out.actions.push(UiAction::Rotate(clockwise));
                        }
                    }
                });
            });
            form.row(ui, t.crop_straighten, |ui| {
                let tool_on = app.straighten_tool() != StraightenTool::Off;
                let angle = app.crop_straighten().unwrap_or(0.0);
                // Filled while the tool is on, as a pressed toggle.
                let role = if tool_on { Role::Primary } else { Role::Cancel };
                let tool = Button::new(t.crop_straighten_tool, role);
                if form::button(ui, &tool).clicked() {
                    out.actions.push(UiAction::ToggleStraightenTool);
                }
                // The angle and Reset get their own line, so the row never
                // outgrows a narrow panel.
                ui.horizontal(|ui| {
                    ui.label(format!("{angle:+.1}\u{b0}"));
                    if angle != 0.0
                        && form::button(ui, &Button::new(t.reset, Role::Cancel)).clicked()
                    {
                        out.actions.push(UiAction::ResetStraighten);
                    }
                });
                if tool_on {
                    form::hint(ui, t.crop_straighten_hint);
                }
            });
            form.row(ui, t.crop_aspect, |ui| {
                let aspects = [
                    (CropAspect::Original, t.crop_original, None),
                    (CropAspect::Custom, t.crop_custom, None),
                    (CropAspect::R4x3, "4:3", None),
                    (CropAspect::R16x9, "16:9", None),
                    (CropAspect::Square, "1:1", None),
                ];
                // Picking the current ratio again re-centers its box.
                if let Some(choice) = form::segmented(ui, &aspects, aspect) {
                    out.actions.push(UiAction::SetCropAspect(choice));
                }
            });
            form.row(ui, t.crop_orientation, |ui| {
                let orientable = !matches!(aspect, CropAspect::Custom | CropAspect::Square);
                ui.add_enabled_ui(orientable, |ui| {
                    let choices = [
                        (CropOrientation::Horizontal, t.crop_horizontal, None),
                        (CropOrientation::Vertical, t.crop_vertical, None),
                    ];
                    let picked = form::segmented(ui, &choices, orientation);
                    if let Some(choice) = picked.filter(|&c| c != orientation) {
                        out.actions.push(UiAction::SetCropOrientation(choice));
                    }
                });
            });
            form.row(ui, t.crop_overlay, |ui| {
                let overlays = [
                    (CropOverlay::Thirds, t.crop_thirds, None),
                    (CropOverlay::Grid, t.crop_grid, None),
                    (CropOverlay::Golden, t.crop_golden, None),
                    (CropOverlay::Diagonal, t.crop_diagonal, None),
                    (CropOverlay::Spiral, t.crop_spiral, None),
                    (CropOverlay::None, t.crop_overlay_none, None),
                ];
                let current = app.crop_overlay();
                let picked = form::segmented(ui, &overlays, current);
                if let Some(choice) = picked.filter(|&c| c != current) {
                    out.actions.push(UiAction::SetCropOverlay(choice));
                }
            });
            if let Some((w, h)) = app.crop_pixel_size() {
                form.row(ui, t.crop_size, |ui| {
                    ui.label(format!("{w} \u{d7} {h}"));
                });
            }
        });
    });
}

/// The Develop header with Reset, presets, and every tone, color, and detail
/// slider in Lightroom's order, with Auto Tone on the Tone header.
fn draw_sliders_tab(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    let t = t();
    let mut adj = app.current_adjustments();
    form::page_heading(ui, t.tab_sliders, |ui| {
        if ui.button(t.reset).clicked() {
            out.actions.push(UiAction::ResetAdjustments);
            out.actions.push(UiAction::Focus(Region::Develop));
        }
    });
    form::page(ui, |ui| {
        // Only this page has the histogram: Crop and Masks work on the frame,
        // not its tones.
        draw_histogram(ui, app);
        draw_exposure_row(ui, app);
        ui.add_space(6.0);
        if crate::app::SHOW_PRESETS {
            draw_presets(ui, app, out);
        }
        // `interacted_idx` is the slider the mouse touched this frame, so the
        // keyboard cursor can follow it.
        let mut changed = false;
        let mut interacted_idx: Option<usize> = None;
        let focus_idx =
            if app.focus() == Region::Develop && app.focus_level() == FocusLevel::Entered {
                Some(app.develop_focus())
            } else {
                None
            };
        ui.spacing_mut().item_spacing.y = font_size::px(ui.style(), form::SLIDER_GAP);

        let mut section = None;
        for (idx, s) in crate::develop::SLIDERS.iter().enumerate() {
            if section != Some(s.section) {
                if section.is_some() {
                    form::divider(ui);
                }
                section = Some(s.section);
                form::section_header(ui, t.section(s.section), |ui| match s.section {
                    crate::develop::Section::WhiteBalance => {
                        let picker = eyedropper_button(ui, app.wb_picker_active())
                            .on_hover_text(t.pick_gray_tip);
                        if picker.clicked() {
                            out.actions.push(UiAction::ToggleWbPicker);
                        }
                    }
                    crate::develop::Section::Tone => {
                        let auto = ui.button(t.auto_tone).on_hover_text(t.auto_tone_tip);
                        if auto.clicked() {
                            out.actions.push(UiAction::AutoTone);
                            out.actions.push(UiAction::Focus(Region::Develop));
                        }
                    }
                    _ => {}
                });
            }
            let field = (s.field)(&mut adj);
            let (c, i) = develop_slider(
                ui,
                t.slider(s.id),
                field,
                s.range.clone(),
                s.decimals,
                focus_idx == Some(idx),
            );
            changed |= c;
            if i {
                interacted_idx = Some(idx);
            }
        }

        if changed {
            out.actions.push(UiAction::SetAdjustments(adj));
        }
        if let Some(idx) = interacted_idx {
            out.actions.push(UiAction::FocusDevelop(idx));
        }
    });
}

/// One Develop slider, its value typed or dragged in the readout. Double-
/// clicking the track resets it to 0. Returns (value changed, mouse
/// interacted).
fn develop_slider(
    ui: &mut egui::Ui,
    label: &str,
    field: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    decimals: usize,
    focused: bool,
) -> (bool, bool) {
    // The readout and the track can't both borrow `field`, so the readout
    // edits a copy that is written back when it changes.
    let mut typed = *field;
    let speed = (range.end() - range.start()) / 300.0;
    let readout = egui::DragValue::new(&mut typed)
        .range(range.clone())
        // Preserve fractional Auto Tone/catalog values on display. Clamping
        // would round them on first draw, producing an edit without any user
        // interaction.
        .clamp_existing_to_range(false)
        .max_decimals(decimals)
        .speed(speed);
    let track = egui::Slider::new(field, range)
        .clamping(egui::SliderClamping::Edits)
        .max_decimals(decimals);
    let (track, value) = form::slider(
        ui,
        label,
        |ui| {
            ui.scope(|ui| {
                ui.visuals_mut().button_frame = false;
                ui.spacing_mut().button_padding.x = 0.0;
                ui.add(readout)
            })
            .inner
        },
        track,
    );
    let mut changed = track.changed();
    if value.changed() {
        *field = typed;
        changed = true;
    }
    if track.double_clicked() {
        *field = 0.0;
        changed = true;
    }
    if focused {
        ui.painter().rect_stroke(
            track.rect.union(value.rect).expand(3.0),
            2.0,
            egui::Stroke::new(2.0f32, theme::colors(ui.ctx()).cursor),
            egui::StrokeKind::Outside,
        );
    }
    let interacted = track.clicked()
        || track.dragged()
        || track.double_clicked()
        || value.clicked()
        || value.dragged();
    (changed, interacted)
}

/// The white-balance picker's toggle: an eyedropper, painted so it can't fall
/// back to a tofu box the way an emoji glyph would. Highlighted while armed.
fn eyedropper_button(ui: &mut egui::Ui, active: bool) -> egui::Response {
    let side = font_size::px(ui.style(), 22.0);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(side, side), egui::Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::Button,
            ui.is_enabled(),
            active,
            t().pick_gray,
        )
    });
    let visuals = ui.visuals();
    if active {
        ui.painter()
            .rect_filled(rect, 4.0, visuals.selection.bg_fill);
    } else if response.hovered() {
        ui.painter()
            .rect_filled(rect, 4.0, visuals.widgets.hovered.weak_bg_fill);
    }
    let color = if active {
        visuals.selection.stroke.color
    } else {
        visuals.text_color()
    };
    let u = font_size::px(ui.style(), 1.0);
    let c = rect.center();
    let p = |x: f32, y: f32| egui::pos2(c.x + x * u, c.y + y * u);
    let painter = ui.painter();
    painter.line_segment(
        [p(-6.5, 6.5), p(2.0, -2.0)],
        egui::Stroke::new(2.0 * u, color),
    );
    painter.line_segment(
        [p(-1.0, -4.5), p(4.5, 1.0)],
        egui::Stroke::new(1.6 * u, color),
    );
    painter.circle_filled(p(4.0, -4.0), 3.2 * u, color);
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// An on/off switch: a pill with a knob that slides right when `on`.
fn toggle_switch(ui: &mut egui::Ui, on: bool) -> egui::Response {
    let size = ui.spacing().interact_size.y * egui::vec2(2.0, 1.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Checkbox, ui.is_enabled(), on, "")
    });
    if ui.is_rect_visible(rect) {
        let how_on = ui.ctx().animate_bool_responsive(response.id, on);
        let visuals = ui.style().interact_selectable(&response, on);
        let rect = rect.expand(visuals.expansion);
        let radius = 0.5 * rect.height();
        ui.painter().rect(
            rect,
            radius,
            visuals.bg_fill,
            visuals.bg_stroke,
            egui::StrokeKind::Inside,
        );
        let x = egui::lerp((rect.left() + radius)..=(rect.right() - radius), how_on);
        let center = egui::pos2(x, rect.center().y);
        ui.painter()
            .circle(center, 0.75 * radius, visuals.bg_fill, visuals.fg_stroke);
    }
    response
}

/// Touch Up: the tool toggle, brush size and feather, and the list of spots.
/// The brush only matters while the tool is armed.
fn draw_masks_tab(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    let t = t();
    form::page_heading(ui, t.tab_masks, |_| {});
    form::page(ui, |ui| {
        let active = app.touchup_active();
        let form = Form::new(ui, &[t.touch_up, t.spots]).spacious();
        form.section(ui, "", |ui| {
            form.row(ui, t.touch_up, |ui| {
                if toggle_switch(ui, active).clicked() {
                    out.actions.push(UiAction::ToggleTouchUp);
                }
            });
        });
        ui.add_space(font_size::px(ui.style(), form::SLIDER_GAP));
        ui.spacing_mut().item_spacing.y = font_size::px(ui.style(), form::SLIDER_GAP);
        let mut radius = app.touchup_radius();
        let range = app.touchup_radius_min()..=crate::app::TOUCHUP_MAX_RADIUS;
        let size_px = format!("{:.0} px", app.touchup_radius_px());
        if brush_slider(
            ui,
            active,
            t.brush_size,
            &size_px,
            t.brush_size_tip,
            &mut radius,
            range,
        ) {
            out.actions.push(UiAction::SetTouchUpRadius(radius));
        }
        let mut feather = app.touchup_feather();
        let range = crate::app::TOUCHUP_MIN_FEATHER..=1.0;
        let percent = |v: f32| format!("{:.0}%", v * 100.0);
        if brush_slider(
            ui,
            active,
            t.feather,
            &percent(feather),
            t.feather_tip,
            &mut feather,
            range,
        ) {
            out.actions.push(UiAction::SetTouchUpFeather(feather));
        }
        let mut opacity = app.touchup_opacity();
        if brush_slider(
            ui,
            active,
            t.opacity,
            &percent(opacity),
            t.opacity_tip,
            &mut opacity,
            0.0..=1.0,
        ) {
            out.actions.push(UiAction::SetTouchUpOpacity(opacity));
        }
        if !app.current_touchups().is_empty() {
            form.section(ui, "", |ui| {
                form.row(ui, t.spots, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        for i in 0..app.current_touchups().len() {
                            let label = format!("{}", i + 1);
                            if ui
                                .selectable_label(app.touchup_selected() == Some(i), label)
                                .clicked()
                            {
                                out.actions.push(UiAction::SelectTouchUp(i));
                            }
                        }
                        let delete =
                            egui::RichText::new(t.delete).color(theme::colors(ui.ctx()).danger);
                        if ui
                            .add_enabled(
                                app.touchup_selected().is_some(),
                                egui::Button::new(delete),
                            )
                            .clicked()
                        {
                            out.actions.push(UiAction::DeleteTouchUp);
                        }
                    });
                });
            });
        }
    });
}

/// One of the brush's sliders, with its value as text and its key and
/// scroll shortcuts on hover. Returns whether it changed.
fn brush_slider(
    ui: &mut egui::Ui,
    enabled: bool,
    label: &str,
    readout: &str,
    tip: &str,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
) -> bool {
    ui.add_enabled_ui(enabled, |ui| {
        let (track, _) = form::slider(
            ui,
            label,
            |ui| ui.label(egui::RichText::new(readout).color(theme::colors(ui.ctx()).value)),
            egui::Slider::new(value, range),
        );
        track.on_hover_text(tip).changed()
    })
    .inner
}

/// ISO, focal length, aperture and shutter spread across the histogram's
/// width, as Lightroom shows them. The row keeps its height without EXIF
/// exposure, so the panel below doesn't jump while metadata loads or between
/// photos that have it and photos that don't.
fn draw_exposure_row(ui: &mut egui::Ui, app: &App) {
    let font = egui::FontId::proportional(font_size::px(ui.style(), 12.0));
    let height = ui.fonts_mut(|f| f.row_height(&font)) + 6.0;
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), height),
        egui::Sense::hover(),
    );
    let parts = app
        .current_metadata()
        .map(super::loupe::exposure_parts)
        .unwrap_or_default();
    if parts.is_empty() {
        return;
    }
    let painter = ui.painter_at(rect);
    let color = theme::colors(ui.ctx()).value;
    let inset = 4.0;
    let (left, right) = (rect.left() + inset, rect.right() - inset);
    let last = parts.len() - 1;
    for (i, part) in parts.iter().enumerate() {
        // First flush left, last flush right, the rest evenly between.
        let (x, align) = if i == 0 {
            (left, egui::Align2::LEFT_CENTER)
        } else if i == last {
            (right, egui::Align2::RIGHT_CENTER)
        } else {
            (
                left + (right - left) * i as f32 / last as f32,
                egui::Align2::CENTER_CENTER,
            )
        };
        painter.text(
            egui::pos2(x, rect.center().y),
            align,
            part,
            font.clone(),
            color,
        );
    }
}

/// The R, G, B histogram of the image after develop adjustments. `App`
/// re-bins it on every adjustment change.
fn draw_histogram(ui: &mut egui::Ui, app: &App) {
    let height = 120.0;
    let width = ui.available_width();
    let (rect, _resp) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    let painter = ui.painter_at(rect);

    let colors = theme::colors(ui.ctx());
    painter.rect_filled(rect, 3.0, colors.histogram_bg);
    painter.rect_stroke(
        rect,
        3.0,
        egui::Stroke::new(1.0f32, colors.histogram_border),
        egui::StrokeKind::Inside,
    );

    let Some(bins) = app.histogram() else { return };

    // `recompute_histogram` already spreads each sample across two bins, so
    // tone stretches don't leave a comb. A radius-1 box blur fills the small
    // gaps left by strong stretches without flattening peaks.
    let smooth = |ch: &[f32; 256]| -> [f32; 256] {
        let mut a = *ch;
        const R: usize = 1;
        let src = a;
        for i in 0..256usize {
            let lo = i.saturating_sub(R);
            let hi = (i + R).min(255);
            let mut sum = 0.0;
            for j in lo..=hi {
                sum += src[j];
            }
            a[i] = sum / (hi - lo + 1) as f32;
        }
        a
    };
    let smoothed: [[f32; 256]; 3] = [smooth(&bins[0]), smooth(&bins[1]), smooth(&bins[2])];

    // One max across all channels keeps their heights comparable. Bins 0 and
    // 255 are skipped because clipping spikes there would flatten the rest.
    let mut max = 1f32;
    for ch in &smoothed {
        for (i, &c) in ch.iter().enumerate() {
            if i == 0 || i == 255 {
                continue;
            }
            max = max.max(c);
        }
    }

    let colors = [
        egui::Color32::from_rgba_unmultiplied(255, 70, 70, 120),
        egui::Color32::from_rgba_unmultiplied(70, 255, 70, 120),
        egui::Color32::from_rgba_unmultiplied(90, 120, 255, 120),
    ];

    let x_at = |i: usize| rect.left() + (i as f32 / 255.0) * rect.width();
    let y_at = |count: f32| {
        let n = (count / max).min(1.0);
        rect.bottom() - n * rect.height()
    };

    for (ch, &color) in smoothed.iter().zip(colors.iter()) {
        // A translucent filled area per channel, so overlaps look brighter,
        // with an opaque line along the top.
        let mut mesh = egui::Mesh::default();
        let base = rect.bottom();
        let mut top_line: Vec<egui::Pos2> = Vec::with_capacity(256);
        for (i, &count) in ch.iter().enumerate() {
            let x = x_at(i);
            let top = y_at(count);
            top_line.push(egui::pos2(x, top));
            let idx = mesh.vertices.len() as u32;
            mesh.colored_vertex(egui::pos2(x, base), color);
            mesh.colored_vertex(egui::pos2(x, top), color);
            if i > 0 {
                let p = idx - 2; // previous (base, top) pair
                mesh.add_triangle(p, p + 1, idx + 1);
                mesh.add_triangle(p, idx + 1, idx);
            }
        }
        painter.add(egui::Shape::mesh(mesh));
        let line_color = color.to_opaque();
        painter.add(egui::Shape::line(
            top_line,
            egui::Stroke::new(1.0f32, line_color),
        ));
    }
}
