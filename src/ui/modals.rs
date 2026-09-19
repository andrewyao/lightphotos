use super::*;

use crate::app::App;

/// Confirms a pending bulk action. Cancel, Esc, or a backdrop click dismisses it.
pub(super) fn confirm_modal(ui: &egui::Ui, app: &App, out: &mut FrameOutput) {
    let Some(prompt) = app.pending_bulk_prompt() else {
        return;
    };
    let resp = egui::Modal::new(egui::Id::new("bulk_confirm")).show(ui.ctx(), |ui| {
        ui.set_width(300.0);
        ui.heading(t().confirm);
        ui.add_space(6.0);
        ui.label(prompt);
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if ui.button(t().cancel).clicked() {
                out.actions.push(UiAction::CancelBulk);
            }
            if ui.button(t().confirm).clicked() {
                out.actions.push(UiAction::ConfirmBulk);
            }
        });
    });
    if resp.should_close() {
        out.actions.push(UiAction::CancelBulk);
    }
}

/// Confirms quit, opened by Esc in the grid.
pub(super) fn quit_modal(ui: &egui::Ui, app: &App, out: &mut FrameOutput) {
    if !app.pending_quit() {
        return;
    }
    let resp = egui::Modal::new(egui::Id::new("quit_confirm")).show(ui.ctx(), |ui| {
        ui.set_width(300.0);
        ui.heading(t().quit_title);
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if ui.button(t().cancel).clicked() {
                out.actions.push(UiAction::CancelQuit);
            }
            if ui.button(t().quit).clicked() {
                out.actions.push(UiAction::ConfirmQuit);
            }
        });
    });
    // A backdrop click cancels. A second Esc quits, matching `App::handle_key`.
    if resp.should_close() {
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            out.actions.push(UiAction::ConfirmQuit);
        } else {
            out.actions.push(UiAction::CancelQuit);
        }
    }
}

/// The keyboard-shortcut list, toggled by `?`.
pub(super) fn help_modal(ui: &egui::Ui, app: &App, out: &mut FrameOutput) {
    if !app.show_help() {
        return;
    }
    let resp = egui::Modal::new(egui::Id::new("help_overlay")).show(ui.ctx(), |ui| {
        ui.set_width(460.0);
        ui.heading(t().shortcuts_title);
        ui.add_space(6.0);
        egui::ScrollArea::vertical()
            .max_height(ui.ctx().content_rect().height() * 0.7)
            .show(ui, |ui| {
                for (i, section) in t().help.iter().enumerate() {
                    if i > 0 {
                        ui.add_space(10.0);
                    }
                    ui.label(egui::RichText::new(section.title).small().weak());
                    ui.separator();
                    egui::Grid::new(("help_grid", i))
                        .num_columns(2)
                        .min_col_width(150.0)
                        .spacing([18.0, 6.0])
                        .striped(true)
                        .show(ui, |ui| {
                            for (key, desc) in section.rows {
                                ui.label(egui::RichText::new(*key).strong());
                                ui.label(*desc);
                                ui.end_row();
                            }
                        });
                }
            });
        ui.add_space(10.0);
        if ui.button(t().close).clicked() {
            out.actions.push(UiAction::ToggleHelp);
        }
    });
    if resp.should_close() {
        out.actions.push(UiAction::ToggleHelp);
    }
}
