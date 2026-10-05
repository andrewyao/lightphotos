use super::*;

use crate::app::{App, Region, SHOW_EYES_FILTER};
use crate::navigation::Cmp;

/// A keyboard-focusable control in the Grid toolbar. `ALL` is the
/// row order: `grid_toolbar` draws the row from it and `App::toolbar_move`
/// walks it, so the F6 cursor's index is a position in
/// [`ToolbarControl::drawn`].
#[derive(Clone, Copy)]
pub(crate) enum ToolbarControl {
    AnyRating,
    FilterCmp(Cmp),
    Star(u8),
    Unrated,
    EyesClosed,
}

impl ToolbarControl {
    const ALL: &'static [Self] = &[
        Self::AnyRating,
        Self::Unrated,
        Self::FilterCmp(Cmp::Gte),
        Self::FilterCmp(Cmp::Eq),
        Self::FilterCmp(Cmp::Lte),
        Self::Star(1),
        Self::Star(2),
        Self::Star(3),
        Self::Star(4),
        Self::Star(5),
        Self::EyesClosed,
    ];

    /// How far the F6 cursor walks. Bulk actions are excluded because their
    /// count varies with the selection.
    pub(crate) const DRAWN: usize = {
        let mut n = 0;
        let mut i = 0;
        while i < Self::ALL.len() {
            if Self::ALL[i].is_drawn() {
                n += 1;
            }
            i += 1;
        }
        n
    };

    const fn is_drawn(self) -> bool {
        match self {
            Self::EyesClosed => SHOW_EYES_FILTER,
            _ => true,
        }
    }

    pub(crate) fn drawn() -> impl Iterator<Item = Self> {
        Self::ALL.iter().copied().filter(|c| c.is_drawn())
    }

    /// Controls that open a group in the row get a separator before them.
    fn starts_group(self) -> bool {
        matches!(
            self,
            Self::FilterCmp(Cmp::Gte) | Self::Star(1) | Self::EyesClosed
        )
    }

    /// What a click on this control, or Enter on it, does.
    pub(crate) fn action(self, app: &App) -> UiAction {
        match self {
            Self::AnyRating => UiAction::SetFilter(None),
            Self::FilterCmp(cmp) => UiAction::SetFilterCmp(cmp),
            Self::Star(n) => UiAction::SetFilter(Some((app.filter_cmp(), n))),
            Self::Unrated => {
                let unrated = matches!(app.filter(), Some((Cmp::Eq, 0)));
                UiAction::SetFilter(if unrated { None } else { Some((Cmp::Eq, 0)) })
            }
            Self::EyesClosed => UiAction::ToggleEyesClosed,
        }
    }

    fn widget(self, ui: &mut egui::Ui, app: &App) -> egui::Response {
        let t = t();
        match self {
            Self::AnyRating => ui.selectable_label(app.filter().is_none(), t.all),
            // The comparator applies to the next star click. It stays
            // highlighted even when no filter is set.
            Self::FilterCmp(cmp) => ui
                .selectable_label(app.filter_cmp() == cmp, cmp_glyph(cmp))
                .on_hover_text(match cmp {
                    Cmp::Gte => t.at_least_n_stars,
                    Cmp::Eq => t.exactly_n_stars,
                    Cmp::Lte => t.at_most_n_stars,
                }),
            Self::Star(n) => {
                // `Eq` lights only star N; `Gte` and `Lte` light stars 1 through N.
                let filled = match app.filter() {
                    Some((cmp, v)) if (1..=5).contains(&v) => {
                        if cmp == Cmp::Eq {
                            n == v
                        } else {
                            n <= v
                        }
                    }
                    _ => false,
                };
                let glyph = if filled { "\u{2605}" } else { "\u{2606}" };
                let color = if filled {
                    theme::colors(ui.ctx()).star
                } else {
                    theme::colors(ui.ctx()).label
                };
                let star = egui::Label::new(egui::RichText::new(glyph).size(20.0).color(color))
                    .sense(egui::Sense::click());
                ui.add(star)
                    .on_hover_text((t.show_rated)(cmp_glyph(app.filter_cmp()), n))
            }
            Self::Unrated => ui
                .selectable_label(matches!(app.filter(), Some((Cmp::Eq, 0))), t.unrated)
                .on_hover_text(t.unrated_tip),
            Self::EyesClosed => ui
                .add(egui::Button::selectable(
                    app.eyes_filter_on(),
                    t.eyes_closed,
                ))
                .on_hover_text(t.eyes_closed_tip),
        }
    }
}

fn cmp_glyph(cmp: Cmp) -> &'static str {
    match cmp {
        Cmp::Gte => "\u{2265}",
        Cmp::Eq => "=",
        Cmp::Lte => "\u{2264}",
    }
}

pub(super) fn grid_toolbar(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    egui::Panel::top("grid_toolbar").show_inside(ui, |ui| {
        ui.horizontal(|ui| {
            let t = t();
            ui.label(t.rating_filter);
            for (idx, control) in ToolbarControl::drawn().enumerate() {
                if control.starts_group() {
                    ui.separator();
                }
                let resp = control.widget(ui, app);
                if resp.clicked() {
                    out.actions.push(control.action(app));
                }
                toolbar_focus_sync(ui, app, idx, &resp, out);
            }

            ui.separator();
            ui.weak((t.n_photos)(app.visible_len()));

            if let Some((done, total)) = app.score_progress() {
                ui.separator();
                ui.weak((t.scoring_progress)(done, total));
                if ui.small_button(t.cancel).clicked() {
                    out.actions.push(UiAction::CancelScoring);
                }
            }

            region_focus_marker(ui, app, Region::Toolbar);
        });
    });
}

/// Bulk actions on the Grid selection, in a row under the toolbar.
/// The row stays up with nothing selected so the grid doesn't shift when a
/// selection starts. Every action opens a confirm modal before it runs. These
/// are not in the keyboard cycle because they come and go with the selection;
/// each has its own shortcut instead.
pub(super) fn selection_bar(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    let n = app.selection_count();
    let t = t();
    egui::Panel::top("selection_bar").show_inside(ui, |ui| {
        ui.horizontal(|ui| {
            // Buttons are taller than a label; keep the row one height.
            ui.set_min_height(ui.spacing().interact_size.y);
            if n == 0 {
                ui.weak(crate::i18n::keys(t.no_selection));
                return;
            }
            ui.strong((t.n_selected)(n));
            ui.separator();
            egui::ComboBox::from_id_salt("bulk_star")
                .selected_text(t.rate_menu)
                .show_ui(ui, |ui| {
                    for s in (1u8..=5).rev() {
                        if ui.button(star_string(s)).clicked() {
                            out.actions.push(UiAction::RequestBulk(BulkKind::Rate(s)));
                        }
                    }
                    if ui.button(t.clear_rating).clicked() {
                        out.actions.push(UiAction::RequestBulk(BulkKind::Rate(0)));
                    }
                });
            if ui
                .button(t.auto_tone)
                .on_hover_text(crate::i18n::keys(t.auto_tone_selection_tip))
                .clicked()
            {
                out.actions.push(UiAction::RequestBulk(BulkKind::AutoTone));
            }
            if app.scoring_available()
                && ui
                    .button(t.score_selection)
                    .on_hover_text(crate::i18n::keys(t.score_selection_tip))
                    .clicked()
            {
                out.actions.push(UiAction::ScoreSelection);
            }
            ui.separator();
            if ui
                .add_enabled(n == 1, egui::Button::new(t.copy_settings))
                .on_hover_text(crate::i18n::keys(t.copy_settings_tip))
                .on_disabled_hover_text(t.copy_settings_needs_one)
                .clicked()
            {
                out.actions.push(UiAction::CopySettings);
            }
            let apply = ui
                .add_enabled(
                    app.has_copied_settings(),
                    egui::Button::new(t.apply_settings),
                )
                .on_disabled_hover_text(t.apply_settings_needs_copy);
            if apply.clicked() {
                out.actions
                    .push(UiAction::RequestBulk(BulkKind::ApplySettings));
            }
            if let Some(name) = app.copied_settings_name() {
                ui.weak((t.settings_from)(&name));
            }
            ui.add_enabled_ui(!app.presets().is_empty(), |ui| {
                if !crate::app::SHOW_PRESETS {
                    return;
                }
                egui::ComboBox::from_id_salt("bulk_preset")
                    .selected_text(t.preset_menu)
                    .show_ui(ui, |ui| {
                        for preset in app.presets() {
                            if ui.button(&preset.name).clicked() {
                                out.actions
                                    .push(UiAction::RequestBulk(BulkKind::ApplyPreset(preset.id)));
                            }
                        }
                    });
            })
            .response
            .on_hover_text(t.apply_preset_selection_tip)
            .on_disabled_hover_text(t.no_presets);
            ui.separator();
            if ui
                .add_enabled(
                    app.group_available(),
                    egui::Button::new(t.menu.group_selected),
                )
                .on_hover_text(crate::i18n::keys(t.group_selection_tip))
                .clicked()
            {
                out.actions.push(UiAction::GroupSelection);
            }
            if ui
                .add_enabled(
                    app.ungroup_button_enabled(),
                    egui::Button::new(t.menu.ungroup),
                )
                .on_hover_text(crate::i18n::keys(t.ungroup_selection_tip))
                .clicked()
            {
                out.actions.push(UiAction::UngroupSelection);
            }
            ui.separator();
            if ui
                .button(t.export_jpg)
                .on_hover_text(t.export_jpg_tip)
                .clicked()
            {
                out.actions.push(UiAction::ToggleExportForm);
            }

            // Destructive, so it sits last, in its own group.
            ui.separator();
            let delete = egui::Button::new(
                egui::RichText::new(t.delete).color(theme::colors(ui.ctx()).danger),
            );
            if ui
                .add_enabled(app.delete_available(), delete)
                .on_hover_text(t.delete_selection_tip)
                .clicked()
            {
                out.actions.push(UiAction::RequestBulk(BulkKind::Delete));
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rects the keyboard cursor outlines in one headless toolbar frame.
    fn cursor_rects(app: &App) -> Vec<egui::Rect> {
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1600.0, 400.0),
            )),
            ..Default::default()
        };
        let output = ctx.run_ui(input, |ui| {
            grid_toolbar(ui, app, &mut FrameOutput::default());
        });
        output
            .shapes
            .into_iter()
            .filter_map(|clipped| match clipped.shape {
                egui::Shape::Rect(shape)
                    if shape.stroke.color == theme::palette(theme::Theme::Dark).cursor =>
                {
                    Some(shape.rect)
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_focus_cycle_reaches_every_control_the_toolbar_draws() {
        let mut app = App::new(None);
        let mut prev_left = f32::NEG_INFINITY;
        for idx in 0..App::TOOLBAR_CONTROLS {
            app.focus_toolbar_control(idx);
            let rects = cursor_rects(&app);
            assert_eq!(rects.len(), 1, "cursor {idx} outlines one control");
            assert!(
                rects[0].left() > prev_left,
                "cursor {idx} follows the row left to right"
            );
            prev_left = rects[0].left();
        }
        app.focus_toolbar_control(App::TOOLBAR_CONTROLS);
        assert!(
            cursor_rects(&app).is_empty(),
            "a cursor past the last control outlines nothing"
        );
    }
}
