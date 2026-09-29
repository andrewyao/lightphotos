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
    let stroke = egui::Stroke::new(1.0_f32, theme::colors(ui.ctx()).divider);
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

/// A side panel's footer strip of tabs, each `(tab, label, tooltip)`, docked
/// to the panel's bottom. Returns the tab clicked, if it isn't `current`
/// already. Call it before the panel's content so the content fills the rest.
pub(super) fn footer<T: Copy + PartialEq>(
    ui: &mut egui::Ui,
    id: &str,
    tabs: &[(T, &str, &str)],
    current: T,
) -> Option<T> {
    let frame = egui::Frame::side_top_panel(ui.style()).inner_margin(egui::Margin {
        left: 8,
        right: 8,
        top: 0,
        bottom: 6,
    });
    egui::Panel::bottom(id.to_owned())
        .show_separator_line(false)
        .frame(frame)
        .show_inside(ui, |ui| {
            let strip = ui.max_rect();
            let line = ui.visuals().widgets.noninteractive.bg_stroke;
            ui.painter().hline(
                (strip.left() - 8.0)..=(strip.right() + 8.0),
                strip.top(),
                line,
            );
            let mut picked = None;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                for &(tab, label, tip) in tabs {
                    let active = tab == current;
                    if footer_tab(ui, label, active).on_hover_text(tip).clicked() && !active {
                        picked = Some(tab);
                    }
                }
            });
            picked
        })
        .inner
}

/// One footer tab, hanging from the strip's top rule and opening upward into
/// the panel. The active tab breaks the rule so it joins the content above.
fn footer_tab(ui: &mut egui::Ui, label: &str, active: bool) -> egui::Response {
    let visuals = ui.visuals().clone();
    let mut font = egui::TextStyle::Body.resolve(ui.style());
    font.size += font_size::px(ui.style(), 1.0);
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font, egui::Color32::PLACEHOLDER);
    let pad = egui::vec2(
        font_size::px(ui.style(), 16.0),
        font_size::px(ui.style(), 7.0),
    );
    let size = galley.size() + 2.0 * pad;
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let r = font_size::px(ui.style(), 5.0).round() as u8;
    let corners = egui::CornerRadius {
        nw: 0,
        ne: 0,
        sw: r,
        se: r,
    };
    let text_color = if active {
        visuals.strong_text_color()
    } else if response.hovered() {
        visuals.text_color()
    } else {
        visuals.weak_text_color()
    };
    let painter = ui.painter();
    if active {
        // Cover the rule, then outline the sides and bottom only.
        let covered = egui::Rect::from_min_max(rect.min - egui::vec2(0.0, 1.0), rect.max);
        painter.rect_filled(covered, corners, visuals.panel_fill);
        let stroke = visuals.widgets.noninteractive.bg_stroke;
        let open_top = egui::Rect::from_min_max(rect.min - egui::vec2(0.0, 4.0), rect.max);
        painter
            .with_clip_rect(
                rect.expand2(egui::vec2(2.0, 0.0))
                    .translate(egui::vec2(0.0, 1.0)),
            )
            .rect_stroke(open_top, corners, stroke, egui::StrokeKind::Inside);
    } else if response.hovered() {
        let inset = egui::Rect::from_min_max(rect.min + egui::vec2(0.0, 1.0), rect.max);
        painter.rect_filled(inset, corners, visuals.widgets.hovered.weak_bg_fill);
    }
    painter.galley(rect.min + pad, galley, text_color);
    response
}
