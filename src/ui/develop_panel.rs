use super::*;

use crate::app::{App, FocusLevel, Region};


/// The right-hand Develop panel: the Basic tone sliders, matching Lightroom's
/// order (WB → Tone → Highlights/Shadows/Whites/Blacks). Reads the current
/// image's adjustments from the app, and pushes `SetAdjustments` whenever a
/// slider actually changes (never every frame). A double-click on any slider
/// resets that one field to 0.
pub(super) fn draw_develop_panel(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    let mut adj = app.current_adjustments();

    egui::Panel::right("develop")
        .resizable(true)
        .default_size(340.0)
        .show_inside(ui, |ui| {
            draw_histogram(ui, app);
            ui.add_space(6.0);

            ui.horizontal(|ui| {
                ui.heading("Develop");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Reset").clicked() {
                        out.actions.push(UiAction::ResetAdjustments);
                        out.actions.push(UiAction::Focus(Region::Develop));
                    }
                });
            });
            ui.separator();
            ui.horizontal(|ui| {
                if ui
                    .selectable_label(app.touchup_active(), "Touch Up")
                    .clicked()
                {
                    out.actions.push(UiAction::ToggleTouchUp);
                }
                ui.label("Size");
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
                if ui.button("Undo").clicked() {
                    out.actions.push(UiAction::UndoTouchUp);
                }
                if ui.button("Delete").clicked() {
                    out.actions.push(UiAction::DeleteTouchUp);
                }
            });
            if !app.current_touchups().is_empty() {
                ui.horizontal_wrapped(|ui| {
                    ui.label("Spots:");
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

            // True once any slider in this frame changed, so we push exactly one
            // SetAdjustments after rendering the whole group. `interacted_idx`
            // tracks which slider (if any) had a mouse click/drag this frame, so
            // the keyboard cursor follows the mouse to that exact slider.
            let mut changed = false;
            let mut interacted_idx: Option<usize> = None;
            // Index of the slider being drawn, matched against `develop_focus` to
            // draw the keyboard-cursor outline. Advanced by every `slider(...)`.
            let mut idx = 0usize;
            let focus_idx =
                if app.focus() == Region::Develop && app.focus_level() == FocusLevel::Entered {
                    Some(app.develop_focus())
                } else {
                    None
                };

            // One labeled slider over `field`. `focused` draws the amber keyboard
            // cursor. Returns (value changed, mouse-interacted).
            fn slider(
                ui: &mut egui::Ui,
                label: &str,
                field: &mut f32,
                range: std::ops::RangeInclusive<f32>,
                decimals: usize,
                focused: bool,
            ) -> (bool, bool) {
                ui.label(label);
                // Let the slider track fill the panel width, leaving room only
                // for the value box egui draws to its right. spacing is persistent
                // for the rest of the frame, so snapshot and restore it — otherwise
                // every widget drawn after the last slider inherits this width.
                let prev_width = ui.spacing().slider_width;
                ui.spacing_mut().slider_width = (ui.available_width() - 56.0).max(80.0);
                let resp = ui.add(
                    egui::Slider::new(field, range)
                        .max_decimals(decimals)
                        .show_value(true),
                );
                ui.spacing_mut().slider_width = prev_width;
                let mut changed = resp.changed();
                // Double-click the slider to reset this field to its default.
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

            // Render one slider: accumulate changed/interacted and bump `idx`.
            macro_rules! row {
                ($label:expr, $field:expr, $range:expr, $dec:expr) => {{
                    let (c, i) = slider(ui, $label, $field, $range, $dec, focus_idx == Some(idx));
                    changed |= c;
                    if i {
                        interacted_idx = Some(idx);
                    }
                    idx += 1;
                }};
            }

            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("White Balance").strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .selectable_label(app.wb_picker_active(), "Pick Gray")
                        .on_hover_text("Click a neutral-gray pixel in the image")
                        .clicked()
                    {
                        out.actions.push(UiAction::ToggleWbPicker);
                    }
                });
            });
            row!("Temp", &mut adj.temp, crate::develop::TONE_RANGE, 0);
            row!("Tint", &mut adj.tint, crate::develop::TONE_RANGE, 0);
            ui.add_space(6.0);

            ui.label(egui::RichText::new("Tone").strong());
            row!(
                "Exposure",
                &mut adj.exposure,
                crate::develop::EXPOSURE_RANGE,
                2
            );
            row!("Contrast", &mut adj.contrast, crate::develop::TONE_RANGE, 0);
            row!(
                "Highlights",
                &mut adj.highlights,
                crate::develop::TONE_RANGE,
                0
            );
            row!("Shadows", &mut adj.shadows, crate::develop::TONE_RANGE, 0);
            row!("Whites", &mut adj.whites, crate::develop::TONE_RANGE, 0);
            row!("Blacks", &mut adj.blacks, crate::develop::TONE_RANGE, 0);
            ui.add_space(6.0);

            ui.label(egui::RichText::new("Presence").strong());
            row!("Vibrance", &mut adj.vibrance, crate::develop::TONE_RANGE, 0);
            row!(
                "Saturation",
                &mut adj.saturation,
                crate::develop::TONE_RANGE,
                0
            );
            ui.add_space(6.0);

            ui.label(egui::RichText::new("Detail").strong());
            row!(
                "Denoise",
                &mut adj.denoise,
                crate::develop::DENOISE_RANGE,
                0
            );
            let _ = idx; // final bump isn't read; silence unused-assignment

            if changed {
                out.actions.push(UiAction::SetAdjustments(adj));
            }
            if let Some(idx) = interacted_idx {
                out.actions.push(UiAction::FocusDevelop(idx));
            }

            region_focus_marker(ui, app, Region::Develop);
        });
}

/// The live post-adjustment histogram at the top of the Develop panel. Draws a
/// dark frame, then the R/G/B channels as translucent filled curves (additive
/// overlap brightens) over a fixed-height rect. Reflects the current image's
/// adjustments because `App` re-bins `apply_linear`'d samples on every change.
pub(super) fn draw_histogram(ui: &mut egui::Ui, app: &App) {
    let height = 120.0;
    let width = ui.available_width();
    let (rect, _resp) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    let painter = ui.painter_at(rect);

    // Dark background frame.
    painter.rect_filled(rect, 3.0, egui::Color32::from_gray(16));
    painter.rect_stroke(
        rect,
        3.0,
        egui::Stroke::new(1.0f32, egui::Color32::from_gray(48)),
        egui::StrokeKind::Inside,
    );

    let Some(bins) = app.histogram() else { return };

    // Bins arrive float (fractional splat in recompute_histogram), so the comb
    // from re-quantizing a tone stretch is already gone. A single light box blur
    // tidies any residual gaps from strong stretches without flattening peaks —
    // giving Lightroom's smooth-but-detailed curve.
    let smooth = |ch: &[f32; 256]| -> [f32; 256] {
        let mut a = *ch;
        const R: usize = 1; // box radius
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

    // Shared max across all channels so relative channel heights stay honest.
    // Skip the extreme end bins (0 and 255) when scaling: pure black/white
    // clipping spikes would otherwise flatten everything else.
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
        // Each channel is a filled area curve: a triangle strip between the
        // baseline and the curve top. Translucent fills overlap to brighten,
        // giving the Lightroom additive look. A brighter polyline traces the top.
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
        // Crisper top edge.
        let line_color = color.to_opaque();
        painter.add(egui::Shape::line(
            top_line,
            egui::Stroke::new(1.0f32, line_color),
        ));
    }
}
