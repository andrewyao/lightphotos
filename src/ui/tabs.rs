// SPDX-License-Identifier: MIT OR Apache-2.0

//! A side panel's footer strip of text tabs. The active tab is outlined on
//! three sides and open at the top, so it joins the content above it.

use super::font_size;

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
        bottom: BOTTOM_MARGIN as i8,
    });
    egui::Panel::bottom(id.to_owned())
        .exact_size(strip_height(ui))
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

const BOTTOM_MARGIN: f32 = 6.0;

fn strip_height(ui: &egui::Ui) -> f32 {
    let (galley, pad) = footer_tab_text(ui, "Ag");
    (galley.size().y + 2.0 * pad.y + BOTTOM_MARGIN).round()
}

/// The height for a bar docked at the window's bottom so its top meets the
/// footer tabs' rule, which the side panel's own margin lifts off the bottom.
pub(super) fn footer_height(ui: &egui::Ui) -> f32 {
    let panel = egui::Frame::side_top_panel(ui.style()).inner_margin;
    strip_height(ui) + f32::from(panel.bottom)
}

/// A footer tab's laid-out label and its padding.
fn footer_tab_text(ui: &egui::Ui, label: &str) -> (std::sync::Arc<egui::Galley>, egui::Vec2) {
    let mut font = egui::TextStyle::Body.resolve(ui.style());
    font.size += font_size::px(ui.style(), 1.0);
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font, egui::Color32::PLACEHOLDER);
    let pad = egui::vec2(
        font_size::px(ui.style(), 16.0),
        font_size::px(ui.style(), 7.0),
    );
    (galley, pad)
}

/// One footer tab, hanging from the strip's top rule and opening upward into
/// the panel. The active tab breaks the rule so it joins the content above.
fn footer_tab(ui: &mut egui::Ui, label: &str, active: bool) -> egui::Response {
    let visuals = ui.visuals().clone();
    let (galley, pad) = footer_tab_text(ui, label);
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
