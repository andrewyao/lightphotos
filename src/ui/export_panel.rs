use super::*;

#[cfg(not(target_arch = "wasm32"))]
use crate::app::ImmichLink;
use crate::export::{ExportSettings, ExportSize, ExportTarget, FolderChoice};

/// The right-hand export form. It takes the Develop panel's place while open,
/// so the photos being exported stay in view. Every change goes back as a
/// whole `SetExportSettings`, the way the develop sliders push `SetAdjustments`.
pub(super) fn draw_export_panel(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    let t = t();
    let settings = app.export_settings();
    let mut next: Option<ExportSettings> = None;

    egui::Panel::right("export")
        .resizable(false)
        .default_size(300.0)
        .show_inside(ui, |ui| {
            ui.add_space(6.0);
            ui.heading((t.export_title)(app.selection_count()));
            ui.separator();

            ui.label(egui::RichText::new(t.export_destination).strong());
            #[cfg(not(target_arch = "wasm32"))]
            ui.horizontal(|ui| {
                let folder = matches!(settings.target, ExportTarget::Folder(_));
                if ui.selectable_label(folder, t.export_to_folder).clicked() && !folder {
                    next = Some(ExportSettings {
                        target: ExportTarget::default(),
                        ..settings.clone()
                    });
                }
                if ui.selectable_label(!folder, t.export_to_immich).clicked() && folder {
                    next = Some(ExportSettings {
                        target: ExportTarget::Immich,
                        ..settings.clone()
                    });
                }
            });
            ui.add_space(4.0);

            match &settings.target {
                ExportTarget::Folder(choice) => {
                    folder_rows(ui, app, choice, settings, &mut next, &mut out.actions)
                }
                #[cfg(not(target_arch = "wasm32"))]
                ExportTarget::Immich => immich_rows(ui, app, &mut out.actions),
            }

            ui.add_space(10.0);
            ui.label(egui::RichText::new(t.export_size).strong());
            let size_label = |size: ExportSize| match size {
                ExportSize::Full => t.export_size_full.to_string(),
                ExportSize::LongEdge(px) => (t.export_size_long_edge)(px),
            };
            egui::ComboBox::from_id_salt("export_size")
                .selected_text(size_label(settings.size))
                .width(200.0)
                .show_ui(ui, |ui| {
                    for size in ExportSize::CHOICES {
                        if ui
                            .selectable_label(settings.size == size, size_label(size))
                            .clicked()
                        {
                            next = Some(ExportSettings {
                                size,
                                ..settings.clone()
                            });
                        }
                    }
                });

            ui.add_space(14.0);
            let blocker = app.export_blocker();
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(blocker.is_none(), egui::Button::new(t.export_run))
                    .clicked()
                {
                    out.actions.push(UiAction::RunExport);
                }
                if ui.button(t.cancel).clicked() {
                    out.actions.push(UiAction::ToggleExportForm);
                }
            });
            if let Some(why) = blocker {
                ui.add_space(4.0);
                ui.label(egui::RichText::new(why).small().weak());
            }
        });
    if let Some(changed) = next {
        out.actions.push(UiAction::SetExportSettings(changed));
    }
}

fn folder_rows(
    ui: &mut egui::Ui,
    app: &App,
    choice: &FolderChoice,
    settings: &ExportSettings,
    next: &mut Option<ExportSettings>,
    actions: &mut Vec<UiAction>,
) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let t = t();
        let subfolder = *choice == FolderChoice::ExportsSubfolder;
        if ui
            .radio(subfolder, t.export_exports_subfolder)
            .clicked()
            && !subfolder
        {
            *next = Some(ExportSettings {
                target: ExportTarget::default(),
                ..settings.clone()
            });
        }
        ui.horizontal(|ui| {
            // Picking a folder is what selects this option, so the radio and
            // the button do the same thing.
            if ui.radio(!subfolder, t.export_chosen_folder).clicked()
                | ui.button(t.export_choose_folder).clicked()
            {
                actions.push(UiAction::ChooseExportFolder);
            }
        });
    }
    #[cfg(target_arch = "wasm32")]
    let _ = (choice, settings, next, actions);
    if let Some(dir) = app.export_folder() {
        ui.label(
            egui::RichText::new(dir.display().to_string())
                .small()
                .weak(),
        )
        .on_hover_text(dir.display().to_string());
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn immich_rows(ui: &mut egui::Ui, app: &App, actions: &mut Vec<UiAction>) {
    let t = t();
    match app.immich() {
        ImmichLink::Disconnected { url, key, error } => {
            ui.label(t.immich_server_url);
            let mut url_text = url.clone();
            if ui
                .add(
                    egui::TextEdit::singleline(&mut url_text)
                        .hint_text("https://immich.gumnut.ai")
                        .desired_width(f32::INFINITY),
                )
                .changed()
            {
                actions.push(UiAction::SetImmichUrl(url_text));
            }
            ui.label(t.immich_api_key);
            let mut key_text = key.clone();
            let key_field = ui.add(
                egui::TextEdit::singleline(&mut key_text)
                    .password(true)
                    .desired_width(f32::INFINITY),
            );
            if key_field.changed() {
                actions.push(UiAction::SetImmichKey(key_text));
            }
            let ready = !url.trim().is_empty() && !key.trim().is_empty();
            let entered =
                key_field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.add_space(4.0);
            if ui
                .add_enabled(ready, egui::Button::new(t.immich_connect))
                .clicked()
                || (ready && entered)
            {
                actions.push(UiAction::ConnectImmich);
            }
            ui.label(egui::RichText::new(t.immich_key_storage).small().weak());
            if let Some(e) = error {
                ui.label(egui::RichText::new(e).small().color(theme::DANGER_RED));
            }
        }
        ImmichLink::Connecting { url, .. } => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(t.immich_connecting);
            });
            ui.label(egui::RichText::new(url).small().weak());
        }
        ImmichLink::Connected { server, account } => {
            ui.label((t.immich_connected_as)(&account.name));
            ui.label(
                egui::RichText::new(format!("{} \u{b7} {}", account.email, server.origin()))
                    .small()
                    .weak(),
            );
            if ui.button(t.immich_disconnect).clicked() {
                actions.push(UiAction::DisconnectImmich);
            }
        }
    }
}
