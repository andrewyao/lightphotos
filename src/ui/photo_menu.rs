//! The menu a right click on a photo opens, in the Grid, the filmstrip or
//! the Loupe. It holds what the selection bar's controls and Actions menu
//! do, laid out as Lightroom's, and acts on the selection as they do.

use super::{BulkKind, FrameOutput, UiAction};
use crate::app::{App, FlagCoverage, Region, ViewMode};
use crate::catalog::{ColorLabel, Flag};
use crate::i18n::t;

/// A right click on cell `pos` selects it, unless it is already part of the
/// selection, and opens the menu at the pointer.
pub(super) fn on_cell(
    app: &App,
    pos: usize,
    region: Region,
    response: &egui::Response,
    out: &mut FrameOutput,
) {
    if !app.photo_menu_available() {
        return;
    }
    if response.secondary_clicked() && !app.is_selected(pos) {
        out.actions.push(UiAction::Select(pos));
    }
    if response.secondary_clicked() {
        out.actions.push(UiAction::Focus(region));
    }
    egui::Popup::context_menu(response).show(|ui| photo_menu(ui, app, Some(pos), out));
}

/// A right click on the Loupe's photo, which egui leaves unclaimed so the
/// app can pan and zoom it, opens the menu at the pointer.
pub(super) fn on_loupe(ui: &egui::Ui, app: &App, photo: egui::Rect, out: &mut FrameOutput) {
    if !app.photo_menu_available() || app.selected_path().is_none() {
        return;
    }
    let ctx = ui.ctx().clone();
    let open = ui.input(|i| {
        i.pointer.secondary_clicked() && i.pointer.interact_pos().is_some_and(|p| photo.contains(p))
    }) && !ctx.is_pointer_over_egui();
    egui::Popup::new(
        egui::Id::new("loupe_photo_menu"),
        ctx,
        egui::PopupAnchor::PointerFixed,
        ui.layer_id(),
    )
    .kind(egui::PopupKind::Menu)
    .layout(egui::Layout::top_down_justified(egui::Align::Min))
    .style(egui::containers::menu::menu_style)
    .open_memory(open.then_some(egui::SetOpenCommand::Bool(true)))
    .show(|ui| photo_menu(ui, app, None, out));
}

/// The menu's rows. `cell` is the Grid or filmstrip cell clicked, `None` on
/// the Loupe's photo.
fn photo_menu(ui: &mut egui::Ui, app: &App, cell: Option<usize>, out: &mut FrameOutput) {
    let t = t();
    let m = &t.menu;
    let n = app.selection_count();
    let loupe = app.mode() == ViewMode::Loupe;

    if loupe {
        ui.menu_button(m.zoom, |ui| {
            super::loupe::zoom_items(ui, app, app.zoom_bounds(), out);
        });
        ui.separator();
    }

    let mut top = false;
    if let (Some(pos), false) = (cell, loupe) {
        if ui.button(t.open_in_loupe).clicked() {
            out.actions.push(UiAction::OpenLoupe(pos));
        }
        top = true;
    }
    let stack = match cell {
        Some(pos) => app.group_at(pos).is_some(),
        None => app.shown_in_group(),
    };
    if stack {
        if ui.button(m.compare_stack).clicked() {
            if let Some(pos) = cell {
                out.actions.push(UiAction::Select(pos));
            }
            out.actions.push(UiAction::CompareStack);
        }
        top = true;
    }
    if top {
        ui.separator();
    }

    let (lo, hi) = app.selection_rating_span();
    ui.menu_button(m.rate, |ui| {
        for stars in 0..=5u8 {
            let label = if stars == 0 {
                m.no_rating.to_string()
            } else {
                "\u{2605}".repeat(stars as usize)
            };
            if ui
                .selectable_label(lo == stars && hi == stars, label)
                .clicked()
            {
                out.actions.push(UiAction::SetRating(stars));
            }
        }
    });
    ui.menu_button(m.set_flag, |ui| {
        let flags: [(Option<Flag>, &str); 3] = [
            (Some(Flag::Pick), m.flag_picked),
            (None, m.flag_unflagged),
            (Some(Flag::Reject), m.flag_rejected),
        ];
        for (flag, label) in flags {
            let all = app.selection_flag_coverage(flag) == FlagCoverage::All;
            if ui.selectable_label(all, label).clicked() && !all {
                out.actions.push(UiAction::SetFlag(flag));
            }
        }
    });
    ui.menu_button(m.color_label, |ui| {
        let labels = [
            (ColorLabel::Red, m.label_red),
            (ColorLabel::Yellow, m.label_yellow),
            (ColorLabel::Green, m.label_green),
            (ColorLabel::Blue, m.label_blue),
        ];
        let current = (n == 1).then(|| app.selected_label()).flatten();
        for (label, name) in labels {
            if ui.selectable_label(current == Some(label), name).clicked() {
                out.actions.push(UiAction::ToggleLabel(label));
            }
        }
    });
    ui.menu_button(m.group_selected, |ui| {
        if ui
            .add_enabled(app.group_available(), egui::Button::new(t.group_into_stack))
            .clicked()
        {
            out.actions.push(UiAction::GroupSelected);
        }
        if n >= 2
            && app.group_bursts_available()
            && ui
                .button(m.group_bursts)
                .on_hover_text(t.group_selected_bursts_tip)
                .clicked()
        {
            out.actions.push(UiAction::GroupSelectedBursts);
        }
        let has_stack = app.selection_has_group();
        if ui
            .add_enabled(has_stack, egui::Button::new(m.ungroup))
            .clicked()
        {
            out.actions.push(UiAction::RemoveGroups);
        }
        if ui
            .add_enabled(
                has_stack && app.delete_available(),
                egui::Button::new(m.delete_group),
            )
            .clicked()
        {
            out.actions.push(UiAction::RequestDeleteStack);
        }
    });
    ui.separator();

    if ui
        .add_enabled(n == 1, egui::Button::new(m.copy_settings))
        .clicked()
    {
        out.actions.push(UiAction::CopySettings);
    }
    if ui
        .add_enabled(
            app.has_copied_settings(),
            egui::Button::new(m.paste_settings),
        )
        .clicked()
    {
        out.actions
            .push(UiAction::RequestBulk(BulkKind::ApplySettings));
    }
    // Reset clears the shown photo's edits, which only the Loupe has.
    if loupe && ui.button(t.reset_adjustments).clicked() {
        out.actions.push(UiAction::ResetAdjustments);
    }
    if ui.button(m.auto_tone).clicked() {
        out.actions.push(UiAction::RequestBulk(BulkKind::AutoTone));
    }
    if app.scoring_available() && ui.button(m.score_photos).clicked() {
        out.actions.push(UiAction::ScoreSelection);
    }
    ui.separator();

    // Rotation turns the shown photo, so it waits for the Loupe.
    if loupe {
        if ui.button(m.rotate_left).clicked() {
            out.actions.push(UiAction::Rotate(false));
        }
        if ui.button(m.rotate_right).clicked() {
            out.actions.push(UiAction::Rotate(true));
        }
        ui.separator();
    }

    if ui.button(m.export).clicked() {
        out.actions.push(UiAction::ToggleExportForm);
    }
    ui.separator();

    let danger = super::theme::colors(ui.ctx()).danger;
    let delete = egui::Button::new(egui::RichText::new(t.delete).color(danger));
    if ui.add_enabled(app.delete_available(), delete).clicked() {
        out.actions.push(UiAction::RequestBulk(BulkKind::Delete));
    }
}
