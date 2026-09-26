// SPDX-License-Identifier: GPL-3.0-or-later

//! A row of text tabs over a baseline. The active tab is outlined on three
//! sides and open at the bottom, so it joins the content under it; the others
//! are plain text on the baseline.

use super::{font_size, theme};

const PAD_X: f32 = 10.0;
const PAD_Y: f32 = 4.0;
const GAP: f32 = 2.0;

/// Draws `tabs` with `current` active and returns the tab clicked, if it
/// isn't `current` already.
pub(super) fn bar<T: Copy + PartialEq>(
    ui: &mut egui::Ui,
    tabs: &[(T, &str)],
    current: T,
) -> Option<T> {
    let pad = egui::vec2(
        font_size::px(ui.style(), PAD_X),
        font_size::px(ui.style(), PAD_Y),
    );
    let font = egui::TextStyle::Body.resolve(ui.style());
    let stroke = egui::Stroke::new(1.0, theme::colors(ui.ctx()).divider);
    let mut picked = None;
    let mut active_x = None;

    let row = ui
        .horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = font_size::px(ui.style(), GAP);
            for &(tab, label) in tabs {
                let active = tab == current;
                let galley = ui.painter().layout_no_wrap(
                    label.to_string(),
                    font.clone(),
                    egui::Color32::PLACEHOLDER,
                );
                let (rect, resp) =
                    ui.allocate_exact_size(galley.size() + pad * 2.0, egui::Sense::click());
                let color = if active || resp.hovered() {
                    ui.visuals().strong_text_color()
                } else {
                    ui.visuals().weak_text_color()
                };
                if active {
                    active_x = Some(rect.x_range());
                    let v = ui.visuals();
                    ui.painter().rect_filled(
                        rect,
                        egui::CornerRadius {
                            nw: 4,
                            ne: 4,
                            sw: 0,
                            se: 0,
                        },
                        v.faint_bg_color,
                    );
                    ui.painter().add(egui::Shape::line(
                        vec![
                            rect.left_bottom(),
                            rect.left_top(),
                            rect.right_top(),
                            rect.right_bottom(),
                        ],
                        stroke,
                    ));
                }
                ui.painter().galley(rect.min + pad, galley, color);
                if resp.clicked() && !active {
                    picked = Some(tab);
                }
                resp.widget_info(|| {
                    egui::WidgetInfo::selected(egui::WidgetType::Button, true, active, label)
                });
            }
        })
        .response;

    // The baseline runs the full width, broken under the active tab.
    let y = row.rect.bottom();
    let (left, right) = (ui.min_rect().left(), ui.max_rect().right());
    let painter = ui.painter();
    match active_x {
        Some(x) => {
            painter.hline(left..=x.min, y, stroke);
            painter.hline(x.max..=right, y, stroke);
        }
        None => {
            painter.hline(left..=right, y, stroke);
        }
    }
    ui.add_space(font_size::px(ui.style(), PAD_Y));
    picked
}
