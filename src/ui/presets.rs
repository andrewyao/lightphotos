//! The Presets button at the right of the Sliders heading, and its popup:
//! the library and every preset folder's presets, grouped, each applied with
//! one click to the photos the Develop panel acts on. Like the Presets panel
//! in Lightroom, the popup stays open, so one look after another can be
//! tried on the same photos.

use std::collections::BTreeMap;

use super::*;
use crate::app::{DevelopScope, FolderState};
use crate::persist::preset_folders::{display_name, FolderPreset};

/// Where the search text lives between frames, cleared each time the popup
/// opens.
fn search_id() -> egui::Id {
    egui::Id::new("presets_search")
}

/// The heading's Presets button. Opening its popup rescans the folders, so
/// a preset saved in Lightroom meanwhile shows up.
pub(super) fn presets_button(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    let t = t();
    let side = font_size::px(ui.style(), 22.0);
    let (rect, button) = ui.allocate_exact_size(egui::vec2(side, side), egui::Sense::click());
    let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&button));
    button.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Button, ui.is_enabled(), open, t.presets)
    });
    paint_glyph(ui, rect, open, button.hovered());
    let button = button
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(t.presets_tip);
    if button.clicked() && !open {
        out.actions.push(UiAction::RescanPresetFolders);
        ui.data_mut(|d| d.insert_temp(search_id(), String::new()));
    }
    egui::Popup::menu(&button)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .align(egui::RectAlign::BOTTOM_END)
        .width(font_size::px(ui.style(), 280.0))
        .show(|ui| popup(ui, app, out));
}

/// Three slider tracks with their knobs at different places.
fn paint_glyph(ui: &egui::Ui, rect: egui::Rect, open: bool, hovered: bool) {
    let visuals = ui.visuals();
    if open {
        ui.painter()
            .rect_filled(rect, 4.0, visuals.selection.bg_fill);
    } else if hovered {
        ui.painter()
            .rect_filled(rect, 4.0, visuals.widgets.hovered.weak_bg_fill);
    }
    let color = if open {
        visuals.selection.stroke.color
    } else {
        visuals.text_color()
    };
    let u = font_size::px(ui.style(), 1.0);
    let c = rect.center();
    let painter = ui.painter();
    for (y, knob) in [(-5.0, 3.0), (0.0, -3.0), (5.0, 1.0)] {
        painter.line_segment(
            [
                egui::pos2(c.x - 7.0 * u, c.y + y * u),
                egui::pos2(c.x + 7.0 * u, c.y + y * u),
            ],
            egui::Stroke::new(1.4 * u, color),
        );
        painter.circle_filled(egui::pos2(c.x + knob * u, c.y + y * u), 2.2 * u, color);
    }
}

fn popup(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    let t = t();
    let mut query: String = ui.data(|d| d.get_temp(search_id())).unwrap_or_default();
    ui.add(
        egui::TextEdit::singleline(&mut query)
            .hint_text(t.search_presets)
            .desired_width(f32::INFINITY),
    );
    ui.data_mut(|d| d.insert_temp(search_id(), query.clone()));
    let query = query.trim().to_lowercase();
    let shows = |name: &str, group: &str| {
        query.is_empty()
            || name.to_lowercase().contains(&query)
            || group.to_lowercase().contains(&query)
    };
    let live = app.develop_scope() != DevelopScope::None;
    ui.separator();

    let mut shown = 0usize;
    egui::ScrollArea::vertical()
        .max_height(font_size::px(ui.style(), 360.0))
        .show(ui, |ui| {
            let library: Vec<_> = app
                .presets()
                .iter()
                .filter(|p| shows(&p.name, t.my_presets))
                .collect();
            if !library.is_empty() {
                shown += library.len();
                group(ui, t.my_presets, true, !query.is_empty(), |ui| {
                    for preset in library {
                        ui.horizontal(|ui| {
                            let row = preset_row(ui, live, &preset.name, &preset.notes);
                            if row.clicked() {
                                out.actions
                                    .push(UiAction::ApplyPreset(PresetRef::Library(preset.id)));
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| library_menu(ui, preset.id, out),
                            );
                        });
                    }
                });
            }

            let mut groups: BTreeMap<String, (&str, Vec<&FolderPreset>)> = BTreeMap::new();
            for preset in app.folder_presets() {
                if shows(&preset.name, &preset.group) {
                    groups
                        .entry(preset.group.to_lowercase())
                        .or_insert_with(|| (preset.group.as_str(), Vec::new()))
                        .1
                        .push(preset);
                }
            }
            for (name, presets) in groups.into_values() {
                shown += presets.len();
                group(ui, name, false, !query.is_empty(), |ui| {
                    for preset in presets {
                        if preset_row(ui, live, &preset.name, &preset.notes).clicked() {
                            out.actions.push(UiAction::ApplyPreset(PresetRef::Folder(
                                preset.path.clone(),
                            )));
                        }
                    }
                });
            }

            folder_notes(ui, app);
            if shown == 0 {
                let empty = app.presets().is_empty() && app.preset_folder_states().is_empty();
                ui.weak(if empty {
                    t.no_presets
                } else {
                    t.no_matching_presets
                });
            }
        });

    ui.separator();
    footer(ui, app, out);
}

/// A collapsible group of rows. A search opens every group with a match.
fn group(
    ui: &mut egui::Ui,
    name: &str,
    default_open: bool,
    searching: bool,
    rows: impl FnOnce(&mut egui::Ui),
) {
    egui::CollapsingHeader::new(egui::RichText::new(name).strong())
        .id_salt(("preset_group", name.to_lowercase()))
        .default_open(default_open)
        .open(searching.then_some(true))
        .show(ui, rows);
}

/// A preset's row, off while no photo is selected. Its hover is what the
/// preset could not carry, when anything.
fn preset_row(ui: &mut egui::Ui, live: bool, name: &str, notes: &[String]) -> egui::Response {
    let t = t();
    let hover = if notes.is_empty() {
        t.apply_preset_tip.to_string()
    } else {
        notes.join("\n")
    };
    ui.add_enabled(live, egui::Button::selectable(false, name))
        .on_hover_text(hover)
        .on_disabled_hover_text(t.no_photo_selected)
}

fn library_menu(ui: &mut egui::Ui, id: u64, out: &mut FrameOutput) {
    let t = t();
    ui.menu_button("\u{22ef}", |ui| {
        if ui.button(t.rename).clicked() {
            out.actions.push(UiAction::RenamePresetPrompt(id));
            ui.close();
        }
        if ui.button(t.delete).clicked() {
            out.actions.push(UiAction::RequestDeletePreset(id));
            ui.close();
        }
    })
    .response
    .on_hover_text(t.preset_actions_tip);
}

/// A line for each folder with nothing to list yet: still reading, not
/// readable, or holding no presets.
fn folder_notes(ui: &mut egui::Ui, app: &App) {
    let t = t();
    for (folder, state) in app.preset_folder_states() {
        let name = display_name(folder);
        match state {
            FolderState::Scanning => {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new());
                    ui.weak((t.reading_presets)(&name));
                });
            }
            FolderState::Unavailable(why) => {
                ui.weak((t.preset_folder_unavailable)(&name))
                    .on_hover_text(why);
            }
            FolderState::Ready { presets, .. } if presets.is_empty() => {
                ui.weak((t.preset_folder_empty)(&name))
                    .on_hover_text(folder.display().to_string());
            }
            FolderState::Ready { .. } => {}
        }
    }
}

fn footer(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    let t = t();
    let one = app.develop_scope() == DevelopScope::One;
    if ui
        .add_enabled(one, egui::Button::new(t.save_current_as_preset))
        .on_hover_text(crate::i18n::keys(t.save_preset_tip))
        .clicked()
    {
        out.actions.push(UiAction::SavePresetPrompt);
        ui.close();
    }
    #[cfg(not(target_arch = "wasm32"))]
    if ui
        .button(t.import_lr_presets)
        .on_hover_text(t.import_lr_presets_tip)
        .clicked()
    {
        out.actions.push(UiAction::ImportLrPresets);
        ui.close();
    }
    #[cfg(not(target_arch = "wasm32"))]
    if ui
        .button(t.add_preset_folder)
        .on_hover_text(t.add_preset_folder_tip)
        .clicked()
    {
        out.actions.push(UiAction::AddPresetFolder);
        ui.close();
    }
    let folders = app.preset_folder_states();
    if !folders.is_empty() {
        ui.menu_button(t.preset_folders, |ui| {
            for (folder, _) in folders {
                ui.horizontal(|ui| {
                    ui.label(display_name(folder))
                        .on_hover_text(folder.display().to_string());
                    if ui.button(t.remove).clicked() {
                        out.actions
                            .push(UiAction::RemovePresetFolder(folder.clone()));
                    }
                });
            }
        });
    }
    let skipped: usize = folders
        .iter()
        .map(|(_, state)| match state {
            FolderState::Ready { skipped, .. } => *skipped,
            FolderState::Scanning | FolderState::Unavailable(_) => 0,
        })
        .sum();
    if skipped > 0 {
        ui.weak((t.presets_skipped)(skipped))
            .on_hover_text(t.presets_skipped_tip);
    }
}
