// SPDX-License-Identifier: MIT OR Apache-2.0

//! The height of the bars docked at the window's bottom.

use super::font_size;

/// The height for a bar docked at the window's bottom: a row of slightly
/// enlarged body text with padding, plus a side panel's bottom margin.
pub(super) fn footer_height(ui: &egui::Ui) -> f32 {
    let mut font = egui::TextStyle::Body.resolve(ui.style());
    font.size += font_size::px(ui.style(), 1.0);
    let text_h = ui
        .painter()
        .layout_no_wrap("Ag".to_owned(), font, egui::Color32::PLACEHOLDER)
        .size()
        .y;
    let pad_y = font_size::px(ui.style(), 7.0);
    let panel = egui::Frame::side_top_panel(ui.style()).inner_margin;
    (text_h + 2.0 * pad_y + BOTTOM_MARGIN).round() + f32::from(panel.bottom)
}

const BOTTOM_MARGIN: f32 = 6.0;
