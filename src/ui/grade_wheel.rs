// SPDX-License-Identifier: MIT OR Apache-2.0

//! The Color Grading wheels: one row of four hue discs, the angle a hue and
//! the distance from the centre a saturation, as in Lightroom.

use super::*;

use crate::develop::{ColorGrade, Wheel, Zone};

/// The most a wheel spans, before font scaling.
const WHEEL_MAX: f32 = 68.0;
/// Hue steps around a painted wheel.
const RIM_STEPS: usize = 48;

/// The four wheels in a row, `picked` ringed. A press anywhere in a wheel's
/// cell picks it, so its sliders show below. When `editable`, a primary
/// press that starts on the disc also sets its hue and saturation and
/// follows the drag, and a double-click puts it back in the centre.
/// Returns whether `grade` changed.
pub(super) fn grade_wheels(
    ui: &mut egui::Ui,
    grade: &mut ColorGrade,
    picked: Zone,
    editable: bool,
    out: &mut FrameOutput,
) -> bool {
    let t = t();
    let cell_w = ui.available_width() / Zone::ALL.len() as f32;
    let label_h = ui.text_style_height(&egui::TextStyle::Body);
    // Never below a small disc, so a squeezed panel can't divide by a
    // radius of 0 or less.
    let side = (cell_w - font_size::px(ui.style(), 8.0))
        .min(font_size::px(ui.style(), WHEEL_MAX))
        .max(font_size::px(ui.style(), 16.0));
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for zone in Zone::ALL {
            let (cell, response) = ui.allocate_exact_size(
                egui::vec2(cell_w, side + label_h + font_size::px(ui.style(), 4.0)),
                egui::Sense::click_and_drag(),
            );
            let center = egui::pos2(cell.center().x, cell.top() + side / 2.0);
            let radius = side / 2.0;
            let wheel = grade.wheel_mut(zone);
            response.widget_info(|| {
                egui::WidgetInfo::selected(
                    egui::WidgetType::RadioButton,
                    ui.is_enabled(),
                    zone == picked,
                    t.zone(zone),
                )
            });
            let pressed = response.is_pointer_button_down_on() || response.clicked();
            if pressed && zone != picked {
                out.actions.push(UiAction::SetGradeZone(zone));
            }
            if editable && response.double_clicked() {
                changed |= wheel.hue != 0.0 || wheel.saturation != 0.0;
                wheel.hue = 0.0;
                wheel.saturation = 0.0;
            } else if editable && primary_press_on_disc(ui, &response, center, radius) {
                if let Some(pos) = response.interact_pointer_pos() {
                    let v = pos - center;
                    let hue = (-v.y).atan2(v.x).to_degrees().round().rem_euclid(360.0);
                    let saturation = (v.length() / radius * 100.0).min(100.0).round();
                    changed |= wheel.hue != hue || wheel.saturation != saturation;
                    wheel.hue = hue;
                    wheel.saturation = saturation;
                }
            }
            paint_wheel(ui, center, radius, *wheel, zone == picked);
            let text = if zone == picked {
                egui::RichText::new(t.zone(zone)).strong()
            } else {
                egui::RichText::new(t.zone(zone))
            };
            let galley = egui::WidgetText::from(text).into_galley(
                ui,
                Some(egui::TextWrapMode::Truncate),
                cell_w,
                egui::TextStyle::Body,
            );
            let at = egui::pos2(
                cell.center().x - galley.size().x / 2.0,
                cell.bottom() - galley.size().y,
            );
            ui.painter().galley(at, galley, ui.visuals().text_color());
        }
    });
    changed
}

/// Whether the primary button is held or was clicked on `response`, having
/// gone down inside the disc, so a click on the label only picks.
fn primary_press_on_disc(
    ui: &egui::Ui,
    response: &egui::Response,
    center: egui::Pos2,
    radius: f32,
) -> bool {
    let primary = response.clicked_by(egui::PointerButton::Primary)
        || (response.is_pointer_button_down_on() && ui.input(|i| i.pointer.primary_down()));
    let origin = ui.input(|i| i.pointer.press_origin());
    primary && origin.is_some_and(|p| p.distance(center) <= radius)
}

/// A hue disc, gray at the centre and coloured at the rim, with a dot at
/// `wheel`'s hue and saturation.
fn paint_wheel(ui: &egui::Ui, center: egui::Pos2, radius: f32, wheel: Wheel, picked: bool) {
    let dim = !ui.is_enabled();
    let shade = |c: egui::Color32| if dim { c.gamma_multiply(0.4) } else { c };
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(center, shade(egui::Color32::from_gray(128)));
    for i in 0..=RIM_STEPS {
        let a = i as f32 / RIM_STEPS as f32 * std::f32::consts::TAU;
        let [r, g, b] = crate::develop::color::hue_rgb(a.to_degrees())
            .map(|c| ((0.25 + 0.75 * c) * 0.85 * 255.0).round() as u8);
        let rim = center + radius * egui::vec2(a.cos(), -a.sin());
        mesh.colored_vertex(rim, shade(egui::Color32::from_rgb(r, g, b)));
    }
    for i in 1..=RIM_STEPS as u32 {
        mesh.add_triangle(0, i, i + 1);
    }
    let painter = ui.painter();
    painter.add(mesh);
    let ring = if picked {
        egui::Stroke::new(2.0f32, ui.visuals().selection.stroke.color)
    } else {
        egui::Stroke::new(1.0f32, ui.visuals().widgets.noninteractive.bg_stroke.color)
    };
    painter.circle_stroke(center, radius, ring);
    let a = wheel.hue.clamp(0.0, 360.0).to_radians();
    let r = radius * wheel.saturation.clamp(0.0, 100.0) / 100.0;
    let dot = center + r * egui::vec2(a.cos(), -a.sin());
    painter.circle_filled(dot, 4.0, egui::Color32::WHITE);
    painter.circle_stroke(dot, 4.0, egui::Stroke::new(1.0f32, egui::Color32::BLACK));
}
