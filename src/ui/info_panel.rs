// SPDX-License-Identifier: GPL-3.0-or-later

//! The left panel's Info tab: the focused photo's file, camera, exposure,
//! date, and location details.

use super::*;

pub(super) fn draw_info_panel(ui: &mut egui::Ui, app: &App) {
    if app.selected_path().is_none() {
        ui.add_space(8.0);
        ui.label(egui::RichText::new(t().info_no_selection).weak());
    }
}
