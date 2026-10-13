use super::*;

use super::form::{self, Button, Form, Role};
use crate::app::App;
use crate::autotone::Centering;

/// Confirms a pending bulk action, titled and buttoned with the action itself.
/// Cancel, Esc, or a backdrop click dismisses it.
pub(super) fn confirm_modal(ui: &egui::Ui, app: &App, out: &mut FrameOutput) {
    let Some((kind, prompt)) = app.pending_bulk_prompt() else {
        return;
    };
    let t = t();
    let action = match kind {
        BulkKind::ApplySettings => t.apply_settings,
        BulkKind::ApplyPreset(_) => t.bulk_apply_preset,
        BulkKind::AutoTone | BulkKind::AutoToneAll => t.auto_tone,
        BulkKind::Delete => t.bulk_delete,
    };
    let role = if kind == BulkKind::Delete {
        Role::Danger
    } else {
        Role::Primary
    };
    let resp = form::dialog(ui.ctx(), "bulk_confirm", form::DIALOG_WIDTH, |ui| {
        form::title(ui, action);
        ui.label(prompt);
        let buttons = [
            Button::new(t.cancel, Role::Cancel),
            Button::new(action, role),
        ];
        match form::footer(ui, &buttons) {
            Some(Role::Cancel) => out.actions.push(UiAction::CancelPending),
            Some(_) => out.actions.push(UiAction::ConfirmPending),
            None => {}
        }
    });
    if resp.should_close() {
        out.actions.push(UiAction::CancelPending);
    }
}

pub(super) fn delete_group_modal(ui: &egui::Ui, app: &App, out: &mut FrameOutput) {
    let Some((photos, groups)) = app.pending_group_delete() else {
        return;
    };
    let focus = app.group_delete_focus();
    let t = t();
    let trash = (t.trash_group_photos)(photos);
    let resp = form::dialog(ui.ctx(), "delete_group", form::DIALOG_WIDTH, |ui| {
        form::title(ui, t.delete_group_title);
        ui.label((t.delete_group_prompt)(photos, groups));
        let buttons = delete_group_buttons(t, &trash, form::Platform::CURRENT);
        match form::footer_with_focus(ui, &buttons, focus) {
            Some(Role::Cancel) => out.actions.push(UiAction::CancelPending),
            Some(Role::Primary) => out.actions.push(UiAction::RemoveGroups),
            Some(Role::Danger) => out.actions.push(UiAction::TrashGroups),
            None => {}
        }
    });
    if resp.should_close() {
        out.actions.push(UiAction::CancelPending);
    }
}

fn delete_group_roles(platform: form::Platform) -> [Role; 3] {
    match platform {
        form::Platform::Mac => [Role::Cancel, Role::Danger, Role::Primary],
        form::Platform::Windows => [Role::Primary, Role::Danger, Role::Cancel],
    }
}

fn delete_group_buttons<'a>(
    t: &'a crate::i18n::Strings,
    trash: &'a str,
    platform: form::Platform,
) -> [Button<'a>; 3] {
    delete_group_roles(platform).map(|role| {
        let label = match role {
            Role::Cancel => t.cancel,
            Role::Primary => t.remove_group,
            Role::Danger => trash,
        };
        Button::new(label, role)
    })
}

pub(crate) fn delete_group_tab_order() -> Vec<Role> {
    let roles = delete_group_roles(form::Platform::CURRENT);
    form::order(&roles, form::Platform::CURRENT)
        .into_iter()
        .map(|i| roles[i])
        .collect()
}

/// Confirms deleting one preset. This can't go through `confirm_modal`, whose
/// text is built from the selected photo count, which has nothing to do with
/// deleting a preset.
pub(super) fn delete_preset_modal(ui: &egui::Ui, app: &App, out: &mut FrameOutput) {
    let Some(name) = app.pending_preset_delete_name() else {
        return;
    };
    let t = t();
    let resp = form::dialog(
        ui.ctx(),
        "preset_delete_confirm",
        form::DIALOG_WIDTH,
        |ui| {
            form::title(ui, t.confirm);
            ui.label((t.confirm_delete_preset)(&name));
            let buttons = [
                Button::new(t.cancel, Role::Cancel),
                Button::new(t.delete, Role::Danger),
            ];
            match form::footer(ui, &buttons) {
                Some(Role::Cancel) => out.actions.push(UiAction::CancelPending),
                Some(_) => out.actions.push(UiAction::ConfirmPending),
                None => {}
            }
        },
    );
    if resp.should_close() {
        out.actions.push(UiAction::CancelPending);
    }
}

/// Names a new preset or renames one. The in-progress string lives in `App`,
/// because a draw function only reads it; the field pushes every change back as
/// an action, the way the develop sliders do.
pub(super) fn preset_name_modal(ui: &egui::Ui, app: &App, out: &mut FrameOutput) {
    let Some((name, renaming)) = app.preset_name_edit() else {
        return;
    };
    let t = t();
    let mut text = name;
    let resp = form::dialog(ui.ctx(), "preset_name", form::DIALOG_WIDTH, |ui| {
        form::title(
            ui,
            if renaming {
                t.rename_preset_title
            } else {
                t.save_preset_title
            },
        );
        let form = Form::new(ui, &[t.preset_name_label]);
        let mut field = None;
        form.section(ui, "", |ui| {
            form.row(ui, t.preset_name_label, |ui| {
                field = Some(
                    ui.add(
                        egui::TextEdit::singleline(&mut text)
                            .hint_text(t.preset_name_hint)
                            .desired_width(f32::INFINITY),
                    ),
                );
            });
        });
        let mut entered = false;
        if let Some(field) = field {
            if field.changed() {
                out.actions.push(UiAction::SetPresetNameText(text.clone()));
            }
            // Enter is how a singleline field reports itself done. It surrenders
            // focus that frame. This has to be read before asking for focus again,
            // because `request_focus` makes the field focused for the frame it runs
            // in, and `lost_focus` would then report nothing.
            entered = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            // Tab is stripped before egui sees it, so the prompt focuses its own
            // field. Asking only while unfocused also recovers a prompt whose
            // focus was lost, without fighting a click inside the modal every
            // frame.
            if !entered && !field.has_focus() {
                field.request_focus();
            }
        }
        let save = if renaming { t.rename } else { t.save };
        let buttons = [
            Button::new(t.cancel, Role::Cancel),
            Button::new(save, Role::Primary),
        ];
        match form::footer(ui, &buttons) {
            Some(Role::Cancel) => out.actions.push(UiAction::CancelPresetName),
            Some(_) => out.actions.push(UiAction::CommitPresetName),
            None if entered => out.actions.push(UiAction::CommitPresetName),
            None => {}
        }
    });
    if resp.should_close() {
        out.actions.push(UiAction::CancelPresetName);
    }
}

/// Names a new browser catalog, in the preset prompt's shape. Native New
/// Catalog… is a save dialog instead.
#[cfg(target_arch = "wasm32")]
pub(super) fn catalog_name_modal(ui: &egui::Ui, app: &App, out: &mut FrameOutput) {
    let Some(name) = app.catalog_name_edit() else {
        return;
    };
    let t = t();
    let mut text = name.to_string();
    let resp = form::dialog(ui.ctx(), "catalog_name", form::DIALOG_WIDTH, |ui| {
        form::title(ui, t.catalog_save_title);
        let form = Form::new(ui, &[t.catalog_name_label]);
        let mut field = None;
        form.section(ui, "", |ui| {
            form.row(ui, t.catalog_name_label, |ui| {
                field = Some(
                    ui.add(
                        egui::TextEdit::singleline(&mut text)
                            .hint_text(t.catalog_name_hint)
                            .char_limit(crate::persist::catalog::MAX_NAME_CHARS)
                            .desired_width(f32::INFINITY),
                    ),
                );
            });
        });
        let mut entered = false;
        if let Some(field) = field {
            if field.changed() {
                out.actions.push(UiAction::SetCatalogNameText(text.clone()));
            }
            // As in `preset_name_modal`: read Enter before asking for focus.
            entered = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if !entered && !field.has_focus() {
                field.request_focus();
            }
        }
        let buttons = [
            Button::new(t.cancel, Role::Cancel),
            Button::new(t.create, Role::Primary),
        ];
        match form::footer(ui, &buttons) {
            Some(Role::Cancel) => out.actions.push(UiAction::CancelCatalogName),
            Some(_) => out.actions.push(UiAction::CommitCatalogName),
            None if entered => out.actions.push(UiAction::CommitCatalogName),
            None => {}
        }
    });
    if resp.should_close() {
        out.actions.push(UiAction::CancelCatalogName);
    }
}

/// The keyboard-shortcut list, toggled by `?`.
pub(super) fn help_modal(ui: &egui::Ui, app: &App, out: &mut FrameOutput) {
    if !app.show_help() {
        return;
    }
    // A reference sheet of two columns, wider than a form.
    let resp = form::dialog(ui.ctx(), "help_overlay", 460.0, |ui| {
        form::title(ui, t().shortcuts_title);
        egui::ScrollArea::vertical()
            .max_height(ui.ctx().content_rect().height() * 0.7)
            .show(ui, |ui| {
                let sections = t().help.iter().filter(|s| !s.rows.is_empty());
                for (i, section) in sections.enumerate() {
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
                                ui.label(egui::RichText::new(crate::i18n::keys(key)).strong());
                                ui.label(*desc);
                                ui.end_row();
                            }
                        });
                }
            });
        if form::footer(ui, &[Button::new(t().close, Role::Primary)]).is_some() {
            out.actions.push(UiAction::ToggleHelp);
        }
    });
    if resp.should_close() {
        out.actions.push(UiAction::ToggleHelp);
    }
}

/// Copy Settings: a checkbox per section, Check All and Check None, then
/// Copy. Opened by Copy Adjustment and Cmd+Shift+C; Esc, Cancel or a
/// backdrop click closes it without copying.
pub(super) fn copy_settings_modal(ui: &egui::Ui, app: &App, out: &mut FrameOutput) {
    use crate::develop::copy::{CopySection, CopySections};
    let Some(chosen) = app.copy_dialog() else {
        return;
    };
    let t = t();
    let resp = form::dialog(ui.ctx(), "copy_settings", form::DIALOG_WIDTH, |ui| {
        form::title(ui, t.copy_settings);
        ui.label(t.copy_settings_prompt);
        ui.add_space(font_size::px(ui.style(), form::SLIDER_GAP));
        let mut next = chosen;
        for section in CopySection::ALL {
            let mut on = chosen.has(section);
            if ui.checkbox(&mut on, t.copy_section(section)).changed() {
                next = next.with(section, on);
            }
        }
        ui.horizontal(|ui| {
            if form::button(ui, &Button::new(t.check_all, Role::Cancel)).clicked() {
                next = CopySections::all();
            }
            if form::button(ui, &Button::new(t.check_none, Role::Cancel)).clicked() {
                next = CopySections::NONE;
            }
        });
        if next != chosen {
            out.actions.push(UiAction::SetCopySections(next));
        }
        let buttons = [
            Button::new(t.cancel, Role::Cancel),
            Button {
                enabled: chosen != CopySections::NONE,
                ..Button::new(t.copy_button, Role::Primary)
            },
        ];
        match form::footer(ui, &buttons) {
            Some(Role::Cancel) => out.actions.push(UiAction::CancelCopySettings),
            Some(_) => out.actions.push(UiAction::CommitCopySettings),
            None => {}
        }
    });
    if resp.should_close() {
        out.actions.push(UiAction::CancelCopySettings);
    }
}

/// Theme as a segmented choice and language as a list of radios. Opened
/// from the landing page, the header, or Cmd+,; Close, Esc, or a backdrop click
/// dismisses it.
pub(super) fn settings_modal(ui: &egui::Ui, app: &App, out: &mut FrameOutput) {
    if !app.show_settings() {
        return;
    }
    let t = t();
    let resp = form::dialog(ui.ctx(), "settings_overlay", form::DIALOG_WIDTH, |ui| {
        form::title(ui, t.settings_title);
        let mut labels = vec![t.settings_theme, t.settings_language];
        if crate::app::SHOW_AUTOTONE_CENTERING {
            labels.push(t.settings_auto_tone);
        }
        let form = Form::new(ui, &labels);
        form.section(ui, t.form_general, |ui| {
            form.button_row(ui, t.settings_theme, |ui| {
                let themes = [
                    (theme::Theme::Dark, t.theme_dark, None),
                    (theme::Theme::Light, t.theme_light, None),
                    (theme::Theme::Medium, t.theme_medium, None),
                ];
                let current = theme::current(ui.ctx());
                let picked = form::segmented(ui, &themes, current);
                if let Some(choice) = picked.filter(|&c| c != current) {
                    out.actions.push(UiAction::SetTheme(choice));
                }
            });
            form.row(ui, t.settings_language, |ui| {
                // Each language is named in itself, so a reader of any of
                // them can find their own. A list, not a bar, because more
                // languages are coming.
                let lang = crate::i18n::lang();
                for (choice, name) in [(Lang::En, t.lang_english), (Lang::Zh, t.lang_chinese)] {
                    if ui.radio(lang == choice, name).clicked() && lang != choice {
                        out.actions.push(UiAction::SetLanguage(choice));
                    }
                }
            });
        });
        if crate::app::SHOW_AUTOTONE_CENTERING {
            form.section(ui, t.develop, |ui| {
                form.button_row(ui, t.settings_auto_tone, |ui| {
                    let centerings = [
                        (
                            Centering::Range,
                            t.autotone_center_range,
                            Some(t.autotone_center_range_tip),
                        ),
                        (
                            Centering::Median,
                            t.autotone_center_median,
                            Some(t.autotone_center_median_tip),
                        ),
                    ];
                    let current = app.autotone_centering();
                    let picked = form::segmented(ui, &centerings, current);
                    if let Some(choice) = picked.filter(|&c| c != current) {
                        out.actions.push(UiAction::SetAutoToneCentering(choice));
                    }
                });
            });
        }
        if form::footer(ui, &[Button::new(t.close, Role::Primary)]).is_some() {
            out.actions.push(UiAction::CloseSettings);
        }
    });
    if resp.should_close() {
        out.actions.push(UiAction::CloseSettings);
    }
}

#[cfg(test)]
mod tests {
    use super::form::{order, Platform};
    use super::*;

    #[test]
    fn delete_group_orders_its_footer_per_platform_and_trash_is_the_danger() {
        let t = t();
        let on_screen = |platform| -> Vec<(&str, Role)> {
            let buttons = delete_group_buttons(t, "Trash", platform);
            let roles: Vec<Role> = buttons.iter().map(|b| b.role).collect();
            order(&roles, platform)
                .into_iter()
                .map(|i| (buttons[i].label, buttons[i].role))
                .collect()
        };
        let (cancel, trash, remove) = (
            (t.cancel, Role::Cancel),
            ("Trash", Role::Danger),
            (t.remove_group, Role::Primary),
        );
        assert_eq!(on_screen(Platform::Mac), [cancel, trash, remove]);
        assert_eq!(on_screen(Platform::Windows), [remove, trash, cancel]);
    }
}
