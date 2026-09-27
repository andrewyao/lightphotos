use super::*;

use crate::app::{App, DevelopTab, FocusLevel, Region, SHOW_CROP_TAB};

/// The right-hand Develop panel, with sliders in Lightroom's order. Pushes one
/// `SetAdjustments` only on frames where a slider changed. Double-clicking a
/// slider resets it to 0.
pub(super) fn draw_develop_panel(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    let t = t();

    egui::Panel::right("develop")
        .resizable(true)
        .default_size(340.0)
        .show_inside(ui, |ui| {
            // Scroll rather than overflow, which would push the Loupe's bottom
            // panels off the window when the rows outgrow its height.
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    draw_histogram(ui, app);
                    draw_exposure_row(ui, app);
                    ui.add_space(6.0);
                    draw_tab_row(ui, app, out);
                    match app.develop_tab() {
                        DevelopTab::Sliders => draw_sliders_tab(ui, app, out),
                        DevelopTab::Crop => {
                            ui.weak(t.tab_crop);
                        }
                        DevelopTab::Masks => draw_masks_tab(ui, app, out),
                    }
                    draw_export_button(ui, out);
                });

            region_focus_marker(ui, app, Region::Develop);
        });
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

/// Sliders | Crop | Masks. Crop is left out while `SHOW_CROP_TAB` is off.
fn draw_tab_row(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    let t = t();
    let tabs: Vec<_> = [
        (DevelopTab::Sliders, t.tab_sliders),
        (DevelopTab::Crop, t.tab_crop),
        (DevelopTab::Masks, t.tab_masks),
    ]
    .into_iter()
    .filter(|&(tab, _)| tab != DevelopTab::Crop || SHOW_CROP_TAB)
    .collect();
    if let Some(tab) = super::tabs::bar(ui, &tabs, app.develop_tab()) {
        out.actions.push(UiAction::SetDevelopTab(tab));
    }
}

/// The Develop header with Reset, presets, and every tone, color, and detail
/// slider in Lightroom's order, with Auto Tone on the Tone header.
fn draw_sliders_tab(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    let t = t();
    let mut adj = app.current_adjustments();
    ui.horizontal(|ui| {
        ui.heading(t.develop);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button(t.reset).clicked() {
                out.actions.push(UiAction::ResetAdjustments);
                out.actions.push(UiAction::Focus(Region::Develop));
            }
        });
    });
    ui.separator();
    if crate::app::SHOW_PRESETS {
        draw_presets(ui, app, out);
    }
    // `interacted_idx` is the slider the mouse touched this frame, so the
    // keyboard cursor can follow it.
    let mut changed = false;
    let mut interacted_idx: Option<usize> = None;
    let focus_idx = if app.focus() == Region::Develop && app.focus_level() == FocusLevel::Entered {
        Some(app.develop_focus())
    } else {
        None
    };

    // Returns (value changed, mouse interacted).
    fn slider(
        ui: &mut egui::Ui,
        label: &str,
        field: &mut f32,
        range: std::ops::RangeInclusive<f32>,
        decimals: usize,
        focused: bool,
    ) -> (bool, bool) {
        ui.label(label);
        // Widen the track to the panel, leaving room for the value box.
        // egui keeps spacing changes for the rest of the frame, so restore
        // the old width afterward.
        let prev_width = ui.spacing().slider_width;
        let value_box = ui.spacing().interact_size.x + 2.0 * ui.spacing().item_spacing.x;
        ui.spacing_mut().slider_width = (ui.available_width() - value_box).max(80.0);
        let resp = ui.add(
            egui::Slider::new(field, range)
                // Preserve fractional Auto Tone/catalog values on
                // display. Default clamping rounds them on first draw,
                // producing an edit without any user interaction.
                .clamping(egui::SliderClamping::Edits)
                .max_decimals(decimals)
                .show_value(true),
        );
        ui.spacing_mut().slider_width = prev_width;
        let mut changed = resp.changed();
        if resp.double_clicked() {
            *field = 0.0;
            changed = true;
        }
        if focused {
            ui.painter().rect_stroke(
                resp.rect.expand(1.0),
                2.0,
                egui::Stroke::new(2.0f32, theme::colors(ui.ctx()).cursor),
                egui::StrokeKind::Outside,
            );
        }
        let interacted = resp.clicked() || resp.dragged() || resp.double_clicked();
        (changed, interacted)
    }

    let mut section = None;
    for (idx, s) in crate::develop::SLIDERS.iter().enumerate() {
        if section != Some(s.section) {
            if section.is_some() {
                ui.add_space(6.0);
                ui.separator();
            }
            section = Some(s.section);
            let title = egui::RichText::new(t.section(s.section))
                .size(font_size::px(ui.style(), 16.0))
                .strong();
            if s.section == crate::develop::Section::WhiteBalance {
                ui.horizontal(|ui| {
                    ui.label(title);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if eyedropper_button(ui, app.wb_picker_active())
                            .on_hover_text(t.pick_gray_tip)
                            .clicked()
                        {
                            out.actions.push(UiAction::ToggleWbPicker);
                        }
                    });
                });
            } else if s.section == crate::develop::Section::Tone {
                ui.horizontal(|ui| {
                    ui.label(title);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .button(t.auto_tone)
                            .on_hover_text(t.auto_tone_tip)
                            .clicked()
                        {
                            out.actions.push(UiAction::AutoTone);
                            out.actions.push(UiAction::Focus(Region::Develop));
                        }
                    });
                });
            } else {
                ui.label(title);
            }
        }
        let field = (s.field)(&mut adj);
        let (c, i) = slider(
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
}

/// Opens the Export form, which takes the panel's place. Under every tab, so
/// it is there whichever one the edit ended on.
fn draw_export_button(ui: &mut egui::Ui, out: &mut FrameOutput) {
    let t = t();
    ui.add_space(6.0);
    ui.separator();
    if ui.button(t.export_jpg).on_hover_text(t.export_jpg_tip).clicked() {
        out.actions.push(UiAction::ToggleExportForm);
    }
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
fn draw_masks_tab(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    let t = t();
    let active = app.touchup_active();
    ui.horizontal(|ui| {
        ui.label(t.touch_up);
        if toggle_switch(ui, active).clicked() {
            out.actions.push(UiAction::ToggleTouchUp);
        }
        if ui
            .add_enabled(
                app.touchup_selected().is_some(),
                egui::Button::new(t.delete),
            )
            .clicked()
        {
            out.actions.push(UiAction::DeleteTouchUp);
        }
    });
    // Size and Feather each get a row, their sliders lined up in a grid.
    // Size and Feather each get a row, their sliders lined up. The brush
    // only matters while the tool is armed.
    egui::Grid::new("touchup_brush").show(ui, |ui| {
        ui.add_enabled(active, egui::Label::new(t.brush_size))
            .on_hover_text(t.brush_size_tip);
        let mut radius = app.touchup_radius();
        if ui
            .add_enabled(
                active,
                egui::Slider::new(
                    &mut radius,
                    app.touchup_radius_min()..=crate::app::TOUCHUP_MAX_RADIUS,
                )
                .show_value(false),
            )
            .changed()
        {
            out.actions.push(UiAction::SetTouchUpRadius(radius));
        }
        ui.end_row();

        ui.add_enabled(active, egui::Label::new(t.feather))
            .on_hover_text(t.feather_tip);
        let mut feather = app.touchup_feather();
        if ui
            .add_enabled(
                active,
                egui::Slider::new(&mut feather, crate::app::TOUCHUP_MIN_FEATHER..=1.0)
                    .show_value(false),
            )
            .changed()
        {
            out.actions.push(UiAction::SetTouchUpFeather(feather));
        }
        ui.end_row();
    });
    if !app.current_touchups().is_empty() {
        ui.horizontal_wrapped(|ui| {
            ui.label(t.spots);
            for i in 0..app.current_touchups().len() {
                let label = format!("{}", i + 1);
                if ui
                    .selectable_label(app.touchup_selected() == Some(i), label)
                    .clicked()
                {
                    out.actions.push(UiAction::SelectTouchUp(i));
                }
            }
        });
    }
    ui.add_space(4.0);
}

/// ISO, focal length, aperture and shutter spread across the histogram's
/// width, as Lightroom shows them. Nothing is drawn without EXIF exposure.
fn draw_exposure_row(ui: &mut egui::Ui, app: &App) {
    let parts = app
        .current_metadata()
        .map(super::loupe::exposure_parts)
        .unwrap_or_default();
    if parts.is_empty() {
        return;
    }
    let font = egui::FontId::proportional(font_size::px(ui.style(), 12.0));
    let height = ui.fonts_mut(|f| f.row_height(&font)) + 6.0;
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), height),
        egui::Sense::hover(),
    );
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
pub(super) fn draw_histogram(ui: &mut egui::Ui, app: &App) {
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
