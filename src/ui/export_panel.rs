use super::form::{self, Button, Form, Role};
use super::*;

#[cfg(not(target_arch = "wasm32"))]
use crate::app::ImmichLink;
use crate::develop::sharpen::{OutputAmount, OutputMedia, OutputSharpening};
#[cfg(not(target_arch = "wasm32"))]
use crate::export::immich::Album;
#[cfg(not(target_arch = "wasm32"))]
use crate::export::AlbumChoice;
use crate::export::{
    ExportFormat, ExportSettings, ExportSize, ExportTarget, FolderChoice, NameTemplate,
};

/// The right-hand export form. It takes the Develop panel's place while open,
/// so the photos being exported stay in view. With `develop_tab`, it is the
/// Export tab of the Develop panel, same width and footer. Every change goes
/// back as a whole `SetExportSettings`, the way the develop sliders push
/// `SetAdjustments`.
pub(super) fn draw_export_panel(
    ui: &mut egui::Ui,
    app: &App,
    in_loupe: bool,
    out: &mut FrameOutput,
) {
    let t = t();
    let settings = app.export_settings();
    let mut next: Option<ExportSettings> = None;

    // In the Loupe, Export takes Develop's place and width beside the rail.
    let panel = if in_loupe {
        egui::Panel::right("develop")
            .resizable(true)
            .default_size(340.0)
    } else {
        egui::Panel::right("export")
            .resizable(false)
            .default_size(300.0)
    };
    panel.show_inside(ui, |ui| {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                form::page_heading(ui, &(t.export_title)(app.action_count()), |_| {});
                form::page(ui, |ui| draw_export_form(ui, app, settings, &mut next, out));
            });
    });
    if let Some(changed) = next {
        out.actions.push(UiAction::SetExportSettings(changed));
    }
}

/// Output sharpening: the medium as a ComboBox, then its amount as a
/// segmented choice once a medium is picked.
fn sharpen_rows(
    ui: &mut egui::Ui,
    form: &Form,
    settings: &ExportSettings,
    next: &mut Option<ExportSettings>,
) {
    let t = t();
    let sharpen = settings.sharpen;
    let set = |next: &mut Option<ExportSettings>, sharpen| {
        *next = Some(ExportSettings {
            sharpen,
            ..settings.clone()
        });
    };
    let media_label = |m: OutputMedia| match m {
        OutputMedia::Off => t.export_sharpen_off,
        OutputMedia::Screen => t.export_sharpen_screen,
        OutputMedia::Matte => t.export_sharpen_matte,
        OutputMedia::Glossy => t.export_sharpen_glossy,
    };
    form.row(ui, t.export_sharpen, |ui| {
        egui::ComboBox::from_id_salt("export_sharpen")
            .selected_text(media_label(sharpen.media))
            .width(ui.available_width())
            .show_ui(ui, |ui| {
                for media in [
                    OutputMedia::Off,
                    OutputMedia::Screen,
                    OutputMedia::Matte,
                    OutputMedia::Glossy,
                ] {
                    let picked = ui.selectable_label(sharpen.media == media, media_label(media));
                    if picked.clicked() && sharpen.media != media {
                        set(next, OutputSharpening { media, ..sharpen });
                    }
                }
            });
    });
    if sharpen.media == OutputMedia::Off {
        return;
    }
    form.button_row(ui, t.amount, |ui| {
        let amounts = [
            (OutputAmount::Low, t.export_sharpen_low, None),
            (OutputAmount::Standard, t.export_sharpen_standard, None),
            (OutputAmount::High, t.export_sharpen_high, None),
        ];
        let picked = form::segmented(ui, &amounts, sharpen.amount);
        if let Some(amount) = picked.filter(|&a| a != sharpen.amount) {
            set(next, OutputSharpening { amount, ..sharpen });
        }
    });
}

/// Size limits a JPEG export can be held to, in KB.
const JPEG_LIMITS_KB: [u32; 5] = [250, 500, 1024, 2048, 5120];

/// The file: its name, its format, and a size limit for a JPEG, in
/// Lightroom's order.
fn file_rows(
    ui: &mut egui::Ui,
    form: &Form,
    app: &App,
    settings: &ExportSettings,
    next: &mut Option<ExportSettings>,
) {
    let t = t();
    let format_label = |f: ExportFormat| match f {
        ExportFormat::Jpeg => "JPEG",
        ExportFormat::Png => "PNG",
        ExportFormat::Tiff16 => t.export_format_tiff16,
        ExportFormat::Webp => t.export_format_webp,
        ExportFormat::Avif => "AVIF",
    };
    form.row(ui, t.export_naming, |ui| {
        let mut text = settings.naming.0.clone();
        let field = ui.add(egui::TextEdit::singleline(&mut text).desired_width(f32::INFINITY));
        if field.changed() {
            *next = Some(ExportSettings {
                naming: NameTemplate(text),
                ..settings.clone()
            });
        }
        form::hint(ui, t.export_naming_hint);
        if let Some(example) = app.export_example_name() {
            form::hint(ui, &(t.export_naming_example)(&example));
        }
    });
    let format = settings.output().format;
    form.row(ui, t.export_format, |ui| {
        egui::ComboBox::from_id_salt("export_format")
            .selected_text(format_label(format))
            .width(ui.available_width())
            .show_ui(ui, |ui| {
                for f in ExportFormat::available() {
                    if ui.selectable_label(format == f, format_label(f)).clicked() && format != f {
                        *next = Some(ExportSettings {
                            format: f,
                            ..settings.clone()
                        });
                    }
                }
            });
    });
    if format == ExportFormat::Jpeg {
        let limit_label = |kb: Option<u32>| match kb {
            None => t.export_limit_none.to_string(),
            Some(kb) => (t.export_limit_kb)(kb),
        };
        form.row(ui, t.export_limit, |ui| {
            egui::ComboBox::from_id_salt("export_limit")
                .selected_text(limit_label(settings.jpeg_limit_kb))
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    let choices = std::iter::once(None).chain(JPEG_LIMITS_KB.map(Some));
                    for kb in choices {
                        let on = settings.jpeg_limit_kb == kb;
                        if ui.selectable_label(on, limit_label(kb)).clicked() && !on {
                            *next = Some(ExportSettings {
                                jpeg_limit_kb: kb,
                                ..settings.clone()
                            });
                        }
                    }
                });
        });
    }
}

/// The export page under its heading: where to, how big, and the footer.
fn draw_export_form(
    ui: &mut egui::Ui,
    app: &App,
    settings: &ExportSettings,
    next: &mut Option<ExportSettings>,
    out: &mut FrameOutput,
) {
    let t = t();
    let form = Form::stacked();
    form.section(ui, t.export_destination, |ui| {
        let immich = settings.target == ExportTarget::Immich;
        form.row(ui, "", |ui| {
            let choices = [
                (false, t.export_to_folder, None),
                (true, t.export_to_immich, None),
            ];
            let picked = form::segmented(ui, &choices, immich);
            if let Some(to_immich) = picked.filter(|&to| to != immich) {
                *next = Some(ExportSettings {
                    target: if to_immich {
                        ExportTarget::Immich
                    } else {
                        ExportTarget::default()
                    },
                    ..settings.clone()
                });
            }
        });
        match &settings.target {
            ExportTarget::Folder(choice) => {
                form.row(ui, t.export_folder, |ui| {
                    folder_rows(ui, app, choice, settings, next, &mut out.actions)
                });
            }
            #[cfg(not(target_arch = "wasm32"))]
            ExportTarget::Immich => immich_rows(ui, &form, app, settings, next, &mut out.actions),
            #[cfg(target_arch = "wasm32")]
            ExportTarget::Immich => native_only_notice(ui),
        }
    });

    // The browser can't reach an Immich server. The notice above says so,
    // and there is nothing left to set or explain below it.
    let immich_unavailable =
        cfg!(target_arch = "wasm32") && settings.target == ExportTarget::Immich;
    if !immich_unavailable {
        form.section(ui, t.export_file, |ui| {
            file_rows(ui, &form, app, settings, next)
        });
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
                                *next = Some(ExportSettings {
                                    size,
                                    ..settings.clone()
                                });
                            }
                        }
                    });
            });
            sharpen_rows(ui, &form, settings, next);
        });
    }

    let blocker = app.export_blocker();
    let buttons = [
        Button::new(t.cancel, Role::Cancel),
        Button {
            enabled: blocker.is_none(),
            ..Button::new(t.export_run, Role::Primary)
        },
    ];
    match form::footer(ui, &buttons) {
        Some(Role::Cancel) => out.actions.push(UiAction::ToggleExportForm),
        Some(_) => out.actions.push(UiAction::RunExport),
        None => {}
    }
    // Under the buttons, because it says why Export is off.
    if let Some(why) = blocker.filter(|_| !immich_unavailable) {
        ui.with_layout(egui::Layout::top_down(egui::Align::Max), |ui| {
            form::hint(ui, why)
        });
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
            // Center the radio on the taller button beside it.
            ui.spacing_mut().interact_size.y = form::button_height(ui);
            // Picking a folder is what selects this option, so the radio and
            // the button do the same thing.
            if ui.radio(!subfolder, t.export_chosen_folder).clicked()
                | form::button(
                    ui,
                    &form::Button {
                        label: t.export_choose_folder,
                        role: form::Role::Cancel,
                        enabled: true,
                    },
                )
                .clicked()
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
                form::hint(ui, t.immich_url_example);
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
                let connect = Button {
                    enabled: ready,
                    ..Button::new(t.immich_connect, Role::Primary)
                };
                if form::button(ui, &connect).clicked() || (ready && entered) {
                    actions.push(UiAction::ConnectImmich);
                }
                form::hint(ui, t.immich_key_storage);
                if let Some(e) = error {
                    form::error(ui, e);
                }
            });
        }
        ImmichLink::Connecting { url, .. } => {
            form.row(ui, t.immich_account, |ui| {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(t.immich_connecting);
                });
                form::hint(ui, url);
            });
        }
        ImmichLink::Connected {
            server,
            account,
            albums,
        } => {
            form.row(ui, t.immich_account, |ui| {
                ui.label((t.immich_connected_as)(&account.name));
                form::hint(ui, &format!("{} \u{b7} {}", account.email, server.origin()));
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
            form::error(ui, &(t.albums_failed)(e));
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
