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

/// Confirms deleting one preset. This can't go through `confirm_modal`, whose
/// text is built from the selected photo count, which has nothing to do with
/// deleting a preset.
pub(super) fn delete_preset_modal(ui: &egui::Ui, app: &App, out: &mut FrameOutput) {
    let Some(name) = app.pending_preset_delete_name() else {
        return;
    };
    let resp = egui::Modal::new(egui::Id::new("preset_delete_confirm")).show(ui.ctx(), |ui| {
        ui.set_width(300.0);
        ui.heading(t().confirm);
        ui.add_space(6.0);
        ui.label((t().confirm_delete_preset)(&name));
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if ui.button(t().cancel).clicked() {
                out.actions.push(UiAction::CancelDeletePreset);
            }
            if ui.button(t().delete).clicked() {
                out.actions.push(UiAction::ConfirmDeletePreset);
            }
        });
    });
    if resp.should_close() {
        out.actions.push(UiAction::CancelDeletePreset);
    }
}

/// Names a new preset or renames one. The in-progress string lives in `App`,
/// because a draw function only reads it; the field pushes every change back as
/// an action, the way the develop sliders do.
pub(super) fn preset_name_modal(ui: &egui::Ui, app: &App, out: &mut FrameOutput) {
    let Some((name, renaming)) = app.preset_name_edit() else {
        return;
    };
    let mut text = name;
    let resp = egui::Modal::new(egui::Id::new("preset_name")).show(ui.ctx(), |ui| {
        ui.set_width(300.0);
        ui.heading(if renaming {
            t().rename_preset_title
        } else {
            t().save_preset_title
        });
        ui.add_space(6.0);
        let field = ui.add(
            egui::TextEdit::singleline(&mut text)
                .hint_text(t().preset_name_hint)
                .desired_width(f32::INFINITY),
        );
        if field.changed() {
            out.actions.push(UiAction::SetPresetNameText(text.clone()));
        }
        // Enter is how a singleline field reports itself done. It surrenders
        // focus that frame. This has to be read before asking for focus again,
        // because `request_focus` makes the field focused for the frame it runs
        // in, and `lost_focus` would then report nothing.
        let entered = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        // Tab is stripped before egui sees it, so the prompt focuses its own
        // field. Asking only while unfocused also recovers a prompt whose focus
        // was lost, without fighting a click inside the modal every frame.
        if !entered && !field.has_focus() {
            field.request_focus();
        }
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if ui.button(t().cancel).clicked() {
                out.actions.push(UiAction::CancelPresetName);
            }
            let save = if renaming { t().rename } else { t().save };
            if ui.button(save).clicked() || entered {
                out.actions.push(UiAction::CommitPresetName);
            }
        });
    });
    if resp.should_close() {
        out.actions.push(UiAction::CancelPresetName);
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
