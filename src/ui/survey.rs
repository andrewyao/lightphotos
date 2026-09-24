// SPDX-License-Identifier: GPL-3.0-or-later

//! Survey Mode: side-by-side review of one duplicate group, opened by clicking
//! a duplicate badge in the Grid.

use super::*;
use std::path::{Path, PathBuf};

use crate::app::App;

const MEMBER_CELL: f32 = 320.0;

pub(super) fn draw_survey(ui: &mut egui::Ui, app: &mut App, out: &mut FrameOutput) {
    egui::Panel::top("survey_toolbar").show_inside(ui, |ui| {
        ui.horizontal(|ui| {
            ui.heading((t().survey_heading)(app.survey_members().len()));
            ui.separator();
            if ui
                .button(t().keep_best)
                .on_hover_text(t().keep_best_tip)
                .clicked()
            {
                out.actions.push(UiAction::KeepBestRejectRest);
            }
            if ui.button(t().close_esc).clicked() {
                out.actions.push(UiAction::CloseSurvey);
            }
        });
    });

    egui::CentralPanel::default().show_inside(ui, |ui| {
        egui::ScrollArea::horizontal()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let members: Vec<PathBuf> = app.survey_members().to_vec();
                    let focus = app.survey_focus();
                    let best: Option<PathBuf> = app.survey_best().map(Path::to_path_buf);
                    for (i, path) in members.iter().enumerate() {
                        survey_member(
                            ui,
                            app,
                            i,
                            path,
                            i == focus,
                            Some(path.as_path()) == best.as_deref(),
                            out,
                        );
                    }
                });
            });
    });
}

fn survey_member(
    ui: &mut egui::Ui,
    app: &App,
    i: usize,
    path: &Path,
    focused: bool,
    is_best: bool,
    out: &mut FrameOutput,
) {
    ui.vertical(|ui| {
        ui.set_width(MEMBER_CELL);
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(MEMBER_CELL, MEMBER_CELL), egui::Sense::click());
        ui.painter()
            .rect_filled(rect, 4.0, egui::Color32::from_gray(28));
        if let Some((tex, tw, th)) = app.thumb_texture_for_path(path) {
            let scale = (MEMBER_CELL / tw as f32).min(MEMBER_CELL / th as f32);
            let (dw, dh) = (tw as f32 * scale, th as f32 * scale);
            let img_rect = egui::Rect::from_center_size(rect.center(), egui::vec2(dw, dh));
            egui::Image::from_texture((tex, egui::vec2(dw, dh))).paint_at(ui, img_rect);
        } else {
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "\u{2026}",
                egui::FontId::proportional(font_size::px(ui.style(), 18.0)),
                egui::Color32::GRAY,
            );
        }
        if is_best {
            let c = rect.left_top() + egui::vec2(20.0, 20.0);
            ui.painter().circle_filled(
                c,
                font_size::px(ui.style(), 9.0),
                egui::Color32::from_black_alpha(170),
            );
            ui.painter().text(
                c,
                egui::Align2::CENTER_CENTER,
                "\u{2605}",
                egui::FontId::proportional(font_size::px(ui.style(), 13.0)),
                theme::BURST_BADGE,
            );
        }
        ui.painter().rect_stroke(
            rect,
            4.0,
            egui::Stroke::new(
                if focused { 3.0f32 } else { 1.0f32 },
                if focused {
                    theme::CURSOR_AMBER
                } else {
                    egui::Color32::from_gray(70)
                },
            ),
            egui::StrokeKind::Inside,
        );
        if response.clicked() {
            out.actions.push(UiAction::FocusSurveyMember(i));
        }

        let stars = app.rating_of_path(path);
        ui.label(star_string(stars));
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        ui.label(name);
    });
}
