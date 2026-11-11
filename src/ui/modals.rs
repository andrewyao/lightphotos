use super::*;

use super::form::{self, Button, Form, Role};
use crate::app::App;

/// Confirms a pending bulk action, titled and buttoned with the action itself.
/// Cancel, Esc, or a backdrop click dismisses it.
pub(super) fn confirm_modal(ui: &egui::Ui, app: &App, out: &mut FrameOutput) {
    let Some((kind, prompt)) = app.pending_bulk_prompt() else {
        return;
    };
    let t = t();
    let action = match kind {
        BulkKind::ApplySettings => t.apply_settings,
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

/// "Import GPS Tracks…": what was chosen and how many GPX files it holds,
/// a button to choose more, then a row per camera found among the folder's
/// photos: the camera menu's UTC offset, its daylight saving switch and
/// whether LightRemote was paired with it, as remembered for that camera.
/// The lists scroll, so a long one keeps the footer on screen.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn gps_import_modal(ui: &egui::Ui, app: &App, out: &mut FrameOutput) {
    use crate::app::CameraClock;
    use crate::app::PickKind;
    use crate::geotag::{CameraZone, ZONES};
    const LIST_HEIGHT: f32 = 160.0;
    let Some(view) = app.import_dialog() else {
        return;
    };
    let t = t();
    let resp = form::dialog(ui.ctx(), "gps_import", form::DIALOG_WIDTH, |ui| {
        form::title(ui, t.gps_import_title);
        ui.label(t.gps_import_prompt);
        let form = Form::new(ui, &[t.gps_tracks, t.camera_time_zone]);
        form.section(ui, t.gps_tracks, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("gps_sources")
                .max_height(LIST_HEIGHT)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for source in view.sources {
                        ui.label(source.display().to_string());
                    }
                });
            match view.files {
                None => form::hint(ui, t.gps_files_looking),
                Some((0, _)) => form::error(ui, t.geotag_no_tracks),
                Some((n, in_icloud)) => form::hint(ui, &(t.gps_files_found)(n, in_icloud)),
            }
            ui.horizontal(|ui| {
                #[cfg(target_os = "macos")]
                let pickers = [(t.gps_choose, PickKind::Any)];
                #[cfg(not(target_os = "macos"))]
                let pickers = [
                    (t.gps_choose_files, PickKind::Files),
                    (t.gps_choose_folder, PickKind::Folder),
                ];
                for (label, kind) in pickers {
                    if ui.button(label).clicked() {
                        out.actions.push(UiAction::AddGpsTracks(kind));
                    }
                }
            });
        });
        form.section(ui, t.camera_time_zone, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("gps_cameras")
                .max_height(LIST_HEIGHT * 2.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for (camera, clock) in view.cameras {
                        let label = crate::app::camera_label(camera);
                        form.row(ui, &label, |ui| {
                            let mut picked = *clock;
                            egui::ComboBox::from_id_salt(("camera_zone", camera))
                                .width(ui.available_width())
                                .selected_text(zone_text(clock.zone.utc_offset_min))
                                .show_ui(ui, |ui| {
                                    for (minutes, _) in ZONES {
                                        ui.selectable_value(
                                            &mut picked.zone.utc_offset_min,
                                            *minutes,
                                            zone_text(*minutes),
                                        );
                                    }
                                });
                            ui.checkbox(&mut picked.zone.dst, t.camera_dst);
                            ui.checkbox(&mut picked.paired, t.camera_paired);
                            if picked == *clock {
                                return;
                            }
                            let zone = picked.zone;
                            if let Some(zone) = CameraZone::new(zone.utc_offset_min, zone.dst) {
                                let clock = CameraClock { zone, ..picked };
                                out.actions.push(UiAction::SetCameraClock {
                                    camera: camera.clone(),
                                    clock,
                                });
                            }
                        });
                    }
                });
            if view.reading {
                form::hint(ui, t.cameras_reading);
            }
            form::hint(ui, t.camera_time_zone_hint);
        });
        let buttons = [
            Button::new(t.cancel, Role::Cancel),
            Button {
                enabled: view.files.is_some_and(|(n, _)| n > 0) && !view.reading,
                ..Button::new(t.geotag_import, Role::Primary)
            },
        ];
        match form::footer(ui, &buttons) {
            Some(Role::Cancel) => out.actions.push(UiAction::CancelImport),
            Some(_) => out.actions.push(UiAction::CommitImport),
            None => {}
        }
    });
    if resp.should_close() {
        out.actions.push(UiAction::CancelImport);
    }
}

/// A camera menu's offset as `UTC+09:00 Tokyo, Seoul`.
#[cfg(not(target_arch = "wasm32"))]
fn zone_text(minutes: i32) -> String {
    let offset = crate::geotag::format_utc_offset(i64::from(minutes) * 60);
    let cities = crate::geotag::ZONES
        .iter()
        .find(|(m, _)| *m == minutes)
        .map_or("", |(_, c)| c);
    format!("UTC{offset} {cities}")
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
    let sections: Vec<_> = t().help.iter().filter(|s| !s.rows.is_empty()).collect();
    // Each section's height in rows, counting its heading as two.
    let heights: Vec<usize> = sections.iter().map(|s| s.rows.len() + 2).collect();
    let total: usize = heights.iter().sum();
    // Keep the reading order and cut where the left column is closest to half.
    let split = (0..=sections.len())
        .min_by_key(|&n| {
            let left: usize = heights.iter().take(n).sum();
            (2 * left).abs_diff(total)
        })
        .unwrap_or(0);
    // Two columns of sections, so the dialog is wider than a form but never
    // wider than the window.
    let style = ui.ctx().global_style();
    let window = ui.ctx().content_rect().width() / font_size::px(&style, 1.0);
    let width = 1100.0_f32.min(window - 40.0);
    let resp = form::dialog(ui.ctx(), "help_overlay", width, |ui| {
        form::title(ui, t().shortcuts_title);
        egui::ScrollArea::vertical()
            .max_height(ui.ctx().content_rect().height() * 0.8)
            .show(ui, |ui| {
                let (left, right) = sections.split_at(split);
                ui.columns(2, |columns| {
                    let halves = [(0, left), (split, right)];
                    for (ui, (first, half)) in columns.iter_mut().zip(halves) {
                        for (i, section) in half.iter().enumerate() {
                            if i > 0 {
                                ui.add_space(10.0);
                            }
                            ui.label(egui::RichText::new(section.title).small().weak());
                            ui.separator();
                            egui::Grid::new(("help_grid", first + i))
                                .num_columns(2)
                                .min_col_width(140.0)
                                .spacing([14.0, 6.0])
                                .striped(true)
                                .show(ui, |ui| {
                                    for (key, desc) in section.rows {
                                        ui.label(
                                            egui::RichText::new(crate::i18n::keys(key)).strong(),
                                        );
                                        ui.add(egui::Label::new(*desc).wrap());
                                        ui.end_row();
                                    }
                                });
                        }
                    }
                });
            });
        if form::footer(ui, &[Button::new(t().close, Role::Primary)]).is_some() {
            out.actions.push(UiAction::ToggleHelp);
        }
    });
    if resp.should_close() {
        out.actions.push(UiAction::ToggleHelp);
    }
}

/// Copy Settings: a checkbox per group over a checkbox per item, in two
/// columns, Check All and Check None, then Copy. Opened by the cog beside
/// Copy Adjustment and Cmd+Shift+C; Esc, Cancel or a backdrop click closes
/// it without copying.
pub(super) fn copy_settings_modal(ui: &egui::Ui, app: &App, out: &mut FrameOutput) {
    use crate::develop::copy::{CopyGroup, CopyItems};
    let Some(chosen) = app.copy_dialog() else {
        return;
    };
    let t = t();
    let resp = form::dialog(ui.ctx(), "copy_settings", form::DIALOG_WIDTH, |ui| {
        form::title(ui, t.copy_settings);
        ui.label(t.copy_settings_prompt);
        ui.add_space(font_size::px(ui.style(), form::SLIDER_GAP));
        let mut next = chosen;
        // The first four groups hold about half the rows.
        let (left, right) = CopyGroup::ALL.split_at(4);
        ui.columns(2, |columns| {
            for (ui, groups) in columns.iter_mut().zip([left, right]) {
                for &group in groups {
                    copy_group_checkboxes(ui, group, chosen, &mut next);
                }
            }
        });
        ui.add_space(font_size::px(ui.style(), form::SLIDER_GAP));
        ui.horizontal(|ui| {
            if form::button(ui, &Button::new(t.check_all, Role::Cancel)).clicked() {
                next = CopyItems::all();
            }
            if form::button(ui, &Button::new(t.check_none, Role::Cancel)).clicked() {
                next = CopyItems::NONE;
            }
        });
        if next != chosen {
            out.actions.push(UiAction::SetCopyItems(next));
        }
        let buttons = [
            Button::new(t.cancel, Role::Cancel),
            Button {
                enabled: chosen != CopyItems::NONE,
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

/// One Copy Settings group: its checkbox, which shows a dash while only some
/// of its items are ticked, then its items indented beneath, unless it holds
/// just one.
fn copy_group_checkboxes(
    ui: &mut egui::Ui,
    group: crate::develop::copy::CopyGroup,
    chosen: crate::develop::copy::CopyItems,
    next: &mut crate::develop::copy::CopyItems,
) {
    use crate::develop::copy::GroupState;
    let t = t();
    let state = chosen.group_state(group);
    let mut on = state == GroupState::On;
    let header =
        egui::Checkbox::new(&mut on, t.copy_group(group)).indeterminate(state == GroupState::Mixed);
    if ui.add(header).changed() {
        // A click on a mixed group ticks all of it, as Lightroom does.
        *next = next.with_group(group, state != GroupState::On);
    }
    let items = group.items();
    if items.len() < 2 {
        return;
    }
    ui.indent(group, |ui| {
        for &item in items {
            let mut on = chosen.has(item);
            if ui.checkbox(&mut on, t.copy_item(item)).changed() {
                *next = next.with(item, on);
            }
        }
    });
}

/// Theme as a segmented choice, language as a list of radios and, on macOS,
/// the RAW tone curve as a checkbox. Opened
/// from the landing page, the header, or Cmd+,; Close, Esc, or a backdrop click
/// dismisses it.
pub(super) fn settings_modal(ui: &egui::Ui, app: &App, out: &mut FrameOutput) {
    if !app.show_settings() {
        return;
    }
    let t = t();
    let resp = form::dialog(ui.ctx(), "settings_overlay", form::DIALOG_WIDTH, |ui| {
        form::title(ui, t.settings_title);
        let raw_setting = cfg!(target_os = "macos");
        let mut labels = vec![t.settings_theme, t.settings_language];
        if raw_setting {
            labels.push(t.settings_raw);
        }
        let version = app.current_version();
        if version.is_some() {
            labels.push(t.settings_version);
            labels.push(t.settings_updates);
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
                // them can find their own.
                let lang = crate::i18n::lang();
                for choice in Lang::ALL {
                    if ui.radio(lang == choice, choice.name()).clicked() && lang != choice {
                        out.actions.push(UiAction::SetLanguage(choice));
                    }
                }
            });
            if let Some(version) = version {
                form.row(ui, t.settings_version, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(version.to_string());
                        if app.update_available().is_some()
                            && super::update_link(ui, t.update_available).clicked()
                        {
                            out.actions.push(UiAction::OpenDownloads);
                        }
                    });
                });
                form.row(ui, t.settings_updates, |ui| {
                    let mut on = app.update_check_on();
                    if ui.checkbox(&mut on, t.check_updates).changed() {
                        out.actions.push(UiAction::SetUpdateCheck(on));
                    }
                    form::hint(ui, t.check_updates_hint);
                });
            }
        });
        #[cfg(target_os = "macos")]
        form.section(ui, t.develop, |ui| {
            form.row(ui, t.settings_raw, |ui| {
                let mut on = app.apple_tone_curve();
                if ui.checkbox(&mut on, t.apple_tone_curve).changed() {
                    out.actions.push(UiAction::SetAppleToneCurve(on));
                }
                form::hint(ui, t.apple_tone_curve_hint);
            });
        });
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
