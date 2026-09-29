use super::form::Form;
use super::*;

#[cfg(not(target_arch = "wasm32"))]
use crate::app::ImmichLink;
#[cfg(not(target_arch = "wasm32"))]
use crate::export::AlbumChoice;
use crate::export::{ExportSettings, ExportSize, ExportTarget, FolderChoice};
#[cfg(not(target_arch = "wasm32"))]
use crate::immich::Album;

/// The right-hand export form. It takes the Develop panel's place while open,
/// so the photos being exported stay in view. With `develop_tab`, it is the
/// Export tab of the Develop panel, same width and footer. Every change goes
/// back as a whole `SetExportSettings`, the way the develop sliders push
/// `SetAdjustments`.
pub(super) fn draw_export_panel(
    ui: &mut egui::Ui,
    app: &App,
    develop_tab: bool,
    out: &mut FrameOutput,
) {
    let t = t();
    let settings = app.export_settings();
    let mut next: Option<ExportSettings> = None;

    let panel = if develop_tab {
        egui::Panel::right("develop")
            .resizable(true)
            .default_size(340.0)
    } else {
        egui::Panel::right("export")
            .resizable(false)
            .default_size(300.0)
    };
    panel.show_inside(ui, |ui| {
        if develop_tab {
            super::develop_panel::right_tabs(ui, app, out);
        }
        ui.add_space(6.0);
        ui.heading((t.export_title)(app.selection_count()));
        ui.separator();

        ui.add_space(6.0);
        let form = Form::new(
            ui,
            &[
                t.immich_album,
                t.album_name,
                t.export_folder,
                t.immich_server_url,
                t.immich_api_key,
                t.immich_account,
                t.export_size,
            ],
        );
        form.section(ui, t.export_destination, |ui| {
            let immich = settings.target == ExportTarget::Immich;
            let tabs = [(false, t.export_to_folder), (true, t.export_to_immich)];
            if let Some(to_immich) = super::tabs::bar(ui, &tabs, immich) {
                next = Some(ExportSettings {
                    target: if to_immich {
                        ExportTarget::Immich
                    } else {
                        ExportTarget::default()
                    },
                    ..settings.clone()
                });
            }
            match &settings.target {
                ExportTarget::Folder(choice) => {
                    form.row(ui, t.export_folder, |ui| {
                        folder_rows(ui, app, choice, settings, &mut next, &mut out.actions)
                    });
                }
                #[cfg(not(target_arch = "wasm32"))]
                ExportTarget::Immich => {
                    immich_rows(ui, &form, app, settings, &mut next, &mut out.actions)
                }
                #[cfg(target_arch = "wasm32")]
                ExportTarget::Immich => native_only_notice(ui),
            }
        });

        // The browser can't reach an Immich server. The notice above says so,
        // and there is nothing left to set or explain below it.
        let immich_unavailable =
            cfg!(target_arch = "wasm32") && settings.target == ExportTarget::Immich;
        if !immich_unavailable {
            form.section(ui, t.export_output, |ui| {
                form.row(ui, t.export_size, |ui| {
                    let size_label = |size: ExportSize| match size {
                        ExportSize::Full => t.export_size_full.to_string(),
                        ExportSize::LongEdge(px) => (t.export_size_long_edge)(px),
                    };
                    egui::ComboBox::from_id_salt("export_size")
                        .selected_text(size_label(settings.size))
                        .width(ui.available_width())
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
                });
            });
        }

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
        if let Some(why) = blocker.filter(|_| !immich_unavailable) {
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
        if ui.radio(subfolder, t.export_exports_subfolder).clicked() && !subfolder {
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
        ui.label(egui::RichText::new(dir.display().to_string()).weak())
            .on_hover_text(dir.display().to_string());
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn immich_rows(
    ui: &mut egui::Ui,
    form: &Form,
    app: &App,
    settings: &ExportSettings,
    next: &mut Option<ExportSettings>,
    actions: &mut Vec<UiAction>,
) {
    let t = t();
    match app.immich() {
        ImmichLink::Disconnected { url, key, error } => {
            form.row(ui, t.immich_server_url, |ui| {
                let mut url_text = url.clone();
                if ui
                    .add(egui::TextEdit::singleline(&mut url_text).desired_width(f32::INFINITY))
                    .changed()
                {
                    actions.push(UiAction::SetImmichUrl(url_text));
                }
                ui.label(egui::RichText::new(t.immich_url_example).small().weak());
            });
            let mut entered = false;
            form.row(ui, t.immich_api_key, |ui| {
                let mut key_text = key.clone();
                let key_field = ui.add(
                    egui::TextEdit::singleline(&mut key_text)
                        .password(true)
                        .desired_width(f32::INFINITY),
                );
                if key_field.changed() {
                    actions.push(UiAction::SetImmichKey(key_text));
                }
                entered = key_field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            });
            form.row(ui, "", |ui| {
                let ready = !url.trim().is_empty() && !key.trim().is_empty();
                if ui
                    .add_enabled(ready, egui::Button::new(t.immich_connect))
                    .clicked()
                    || (ready && entered)
                {
                    actions.push(UiAction::ConnectImmich);
                }
                ui.label(egui::RichText::new(t.immich_key_storage).small().weak());
                if let Some(e) = error {
                    ui.label(
                        egui::RichText::new(e)
                            .small()
                            .color(theme::colors(ui.ctx()).danger),
                    );
                }
            });
        }
        ImmichLink::Connecting { url, .. } => {
            form.row(ui, t.immich_account, |ui| {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(t.immich_connecting);
                });
                ui.label(egui::RichText::new(url).small().weak());
            });
        }
        ImmichLink::Connected {
            server,
            account,
            albums,
        } => {
            form.row(ui, t.immich_account, |ui| {
                ui.label((t.immich_connected_as)(&account.name));
                ui.label(
                    egui::RichText::new(format!("{} \u{b7} {}", account.email, server.origin()))
                        .small()
                        .weak(),
                );
                if ui.button(t.immich_disconnect).clicked() {
                    actions.push(UiAction::DisconnectImmich);
                }
            });
            album_rows(ui, form, albums, settings, next);
        }
    }
}

/// The album picker: no album, an existing one, or a new one to name.
#[cfg(not(target_arch = "wasm32"))]
fn album_rows(
    ui: &mut egui::Ui,
    form: &Form,
    albums: &Result<Vec<Album>, String>,
    settings: &ExportSettings,
    next: &mut Option<ExportSettings>,
) {
    let t = t();
    let mut choose = |album: AlbumChoice| {
        *next = Some(ExportSettings {
            album,
            ..settings.clone()
        })
    };
    form.row(ui, t.immich_album, |ui| {
        let current = match &settings.album {
            AlbumChoice::None => t.album_none.to_string(),
            AlbumChoice::Existing { name, .. } => name.clone(),
            AlbumChoice::New(_) => t.album_new.to_string(),
        };
        egui::ComboBox::from_id_salt("immich_album")
            .selected_text(current)
            .width(ui.available_width())
            .show_ui(ui, |ui| {
                let none = settings.album == AlbumChoice::None;
                if ui.selectable_label(none, t.album_none).clicked() && !none {
                    choose(AlbumChoice::None);
                }
                let new = matches!(settings.album, AlbumChoice::New(_));
                if ui.selectable_label(new, t.album_new).clicked() && !new {
                    choose(AlbumChoice::New(String::new()));
                }
                if let Ok(list) = albums {
                    if !list.is_empty() {
                        ui.separator();
                    }
                    for album in list {
                        let on = matches!(&settings.album, AlbumChoice::Existing { id, .. } if *id == album.id);
                        if ui.selectable_label(on, &album.name).clicked() && !on {
                            choose(AlbumChoice::Existing {
                                id: album.id.clone(),
                                name: album.name.clone(),
                            });
                        }
                    }
                }
            });
        if let Err(e) = albums {
            ui.label(
                egui::RichText::new((t.albums_failed)(e))
                    .small()
                    .color(theme::colors(ui.ctx()).danger),
            );
        }
    });
    if let AlbumChoice::New(name) = &settings.album {
        form.row(ui, t.album_name, |ui| {
            let mut text = name.clone();
            let field = ui.add(egui::TextEdit::singleline(&mut text).desired_width(f32::INFINITY));
            if field.changed() {
                choose(AlbumChoice::New(text));
            }
        });
    }
}

/// Why the browser build can't upload to Immich, boxed so it reads as a
/// notice rather than as a setting.
#[cfg(target_arch = "wasm32")]
fn native_only_notice(ui: &mut egui::Ui) {
    egui::Frame::new()
        .stroke(egui::Stroke::new(1.0_f32, theme::colors(ui.ctx()).divider))
        .corner_radius(4)
        .inner_margin(8)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(t().immich_native_only);
        });
}
