//! The Presets page: the library and every preset folder's presets,
//! grouped, each applied with one click to the photos the page acts on. Like
//! the Presets panel in Lightroom, it takes the edit panel's place beside
//! the rail, and it stays open, so one look after another can be tried on
//! the same photos.

use std::collections::BTreeMap;

use super::*;
use crate::app::{DevelopScope, FolderState};
use crate::persist::preset_folders::{display_name, FolderPreset};

/// The right-hand panel while the rail's Presets page is lit: the search
/// over a list that fills the panel, with the footer's actions under it.
pub(super) fn draw_presets_panel(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    let t = t();
    egui::Panel::right("develop")
        .resizable(true)
        .default_size(340.0)
        .show_inside(ui, |ui| {
            form::page_heading(ui, t.presets, |_| {});
            egui::Panel::bottom("presets_footer")
                .frame(egui::Frame::NONE.inner_margin(font_size::px(ui.style(), 8.0)))
                .show_inside(ui, |ui| footer(ui, app, out));
            form::page(ui, |ui| list(ui, app, out));
        });
}

/// The search field and every preset that matches it.
fn list(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    let t = t();
    let id = egui::Id::new("presets_search");
    let mut query: String = ui.data(|d| d.get_temp(id)).unwrap_or_default();
    ui.add(
        egui::TextEdit::singleline(&mut query)
            .hint_text(t.search_presets)
            .desired_width(f32::INFINITY),
    );
    ui.data_mut(|d| d.insert_temp(id, query.clone()));
    let query = query.trim().to_lowercase();
    let shows = |name_key: &str, group_key: &str| {
        query.is_empty() || name_key.contains(&query) || group_key.contains(&query)
    };
    let library_key = t.my_presets.to_lowercase();
    let live = app.develop_scope() != DevelopScope::None;
    ui.separator();

    let mut shown = 0usize;
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let library: Vec<_> = app
                .presets()
                .iter()
                .filter(|p| query.is_empty() || shows(&p.name.to_lowercase(), &library_key))
                .collect();
            if !library.is_empty() {
                shown += library.len();
                let id = egui::Id::new("preset_library");
                group(ui, id, t.my_presets, true, !query.is_empty(), |ui| {
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

            let mut groups: BTreeMap<&str, (&str, Vec<&FolderPreset>)> = BTreeMap::new();
            for preset in app.folder_presets() {
                if shows(&preset.name_key, &preset.group_key) {
                    groups
                        .entry(preset.group_key.as_str())
                        .or_insert_with(|| (preset.group.as_str(), Vec::new()))
                        .1
                        .push(preset);
                }
            }
            for (key, (name, presets)) in groups {
                shown += presets.len();
                let id = egui::Id::new(("preset_group", key));
                group(ui, id, name, false, !query.is_empty(), |ui| {
                    for preset in presets {
                        if preset_row(ui, live, &preset.name, &preset.notes).clicked() {
                            out.actions.push(UiAction::ApplyPreset(PresetRef::Folder(
                                preset.path.clone(),
                            )));
                        }
                    }
                });
            }

            folder_notes(ui, app, out);
            if shown == 0 {
                let empty = app.presets().is_empty() && app.preset_folder_states().is_empty();
                ui.weak(if empty {
                    t.no_presets
                } else {
                    t.no_matching_presets
                });
            }
        });
}

/// A collapsible group of rows. A search opens every group with a match.
/// The library and the folder groups key their `id`s apart, so a folder
/// group called My Presets keeps its own folded state.
fn group(
    ui: &mut egui::Ui,
    id: egui::Id,
    name: &str,
    default_open: bool,
    searching: bool,
    rows: impl FnOnce(&mut egui::Ui),
) {
    egui::CollapsingHeader::new(egui::RichText::new(name).strong())
        .id_salt(id)
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
fn folder_notes(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
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
            FolderState::NeedsPermission => {
                if ui.link((t.preset_folder_needs_permission)(&name)).clicked() {
                    out.actions.push(UiAction::RescanPresetFolders);
                }
            }
            FolderState::Unavailable(why) => {
                ui.weak((t.preset_folder_unavailable)(&name))
                    .on_hover_text(why);
            }
            FolderState::Ready { presets, .. } if presets.is_empty() => {
                ui.weak((t.preset_folder_empty)(&name))
                    .on_hover_text(folder.display().to_string());
            }
            FolderState::Ready {
                truncated: true, ..
            } => {
                ui.weak((t.preset_folder_truncated)(
                    &name,
                    crate::persist::preset_folders::LIMITS.files,
                ))
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
    }
    #[cfg(not(target_arch = "wasm32"))]
    if ui
        .button(t.import_lr_presets)
        .on_hover_text(t.import_lr_presets_tip)
        .clicked()
    {
        out.actions.push(UiAction::ImportLrPresets);
    }
    if ui
        .button(t.add_preset_folder)
        .on_hover_text(t.add_preset_folder_tip)
        .clicked()
    {
        out.actions.push(UiAction::AddPresetFolder);
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
            FolderState::Scanning | FolderState::NeedsPermission | FolderState::Unavailable(_) => 0,
        })
        .sum();
    if skipped > 0 {
        ui.weak((t.presets_skipped)(skipped))
            .on_hover_text(t.presets_skipped_tip);
    }
}
