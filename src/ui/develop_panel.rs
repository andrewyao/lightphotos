use super::*;

use crate::app::{App, FocusLevel, Region};

/// The right-hand Develop panel, with sliders in Lightroom's order. Pushes one
/// `SetAdjustments` only on frames where a slider changed. Double-clicking a
/// slider resets it to 0.
pub(super) fn draw_develop_panel(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    let mut adj = app.current_adjustments();
    let t = t();

    egui::Panel::right("develop")
        .resizable(true)
        .default_size(340.0)
        .show_inside(ui, |ui| {
            draw_histogram(ui, app);
            ui.add_space(6.0);

            ui.horizontal(|ui| {
                ui.heading(t.develop);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(t.reset).clicked() {
                        out.actions.push(UiAction::ResetAdjustments);
                        out.actions.push(UiAction::Focus(Region::Develop));
                    }
                    if ui.button(t.auto).on_hover_text(t.auto_tone_tip).clicked() {
                        out.actions.push(UiAction::AutoTone);
                        out.actions.push(UiAction::Focus(Region::Develop));
                    }
                });
            });
            ui.separator();
            draw_presets(ui, app, out);
            ui.horizontal(|ui| {
                if ui
                    .selectable_label(app.touchup_active(), t.touch_up)
                    .clicked()
                {
                    out.actions.push(UiAction::ToggleTouchUp);
                }
                ui.label(t.brush_size);
                let mut radius = app.touchup_radius();
                if ui
                    .add(
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
                if ui.button(t.undo).clicked() {
                    out.actions.push(UiAction::UndoTouchUp);
                }
                if ui.button(t.delete).clicked() {
                    out.actions.push(UiAction::DeleteTouchUp);
                }
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
                ui.spacing_mut().slider_width = (ui.available_width() - 56.0).max(80.0);
                let resp = ui.add(
                    egui::Slider::new(field, range)
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
                        egui::Stroke::new(2.0f32, theme::CURSOR_AMBER),
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
                    }
                    section = Some(s.section);
                    let title = t.section(s.section);
                    if s.section == crate::develop::Section::WhiteBalance {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(title).strong());
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .selectable_label(app.wb_picker_active(), t.pick_gray)
                                        .on_hover_text(t.pick_gray_tip)
                                        .clicked()
                                    {
                                        out.actions.push(UiAction::ToggleWbPicker);
                                    }
                                },
                            );
                        });
                    } else {
                        ui.label(egui::RichText::new(title).strong());
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

            region_focus_marker(ui, app, Region::Develop);
        });
}

/// The saved-look library. Collapsed by default, so opening a photo never
/// moves a slider, and the list scrolls so a long library can't push the
/// eleven sliders off a short window.
fn draw_presets(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    let t = t();
    egui::CollapsingHeader::new(t.presets)
        .id_salt("presets")
        .default_open(false)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.button("+").on_hover_text(t.save_preset_tip).clicked() {
                    out.actions.push(UiAction::SavePreset);
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
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.menu_button("\u{22ef}", |ui| {
                                        if ui.button(t.delete).clicked() {
                                            out.actions
                                                .push(UiAction::RequestDeletePreset(preset.id));
                                            ui.close();
                                        }
                                    })
                                    .response
                                    .on_hover_text(t.preset_actions_tip);
                                },
                            );
                        });
                    }
                });
        });
}

/// The R, G, B histogram of the image after develop adjustments. `App`
/// re-bins it on every adjustment change.
pub(super) fn draw_histogram(ui: &mut egui::Ui, app: &App) {
    let height = 120.0;
    let width = ui.available_width();
    let (rect, _resp) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    let painter = ui.painter_at(rect);

    painter.rect_filled(rect, 3.0, egui::Color32::from_gray(16));
    painter.rect_stroke(
        rect,
        3.0,
        egui::Stroke::new(1.0f32, egui::Color32::from_gray(48)),
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
