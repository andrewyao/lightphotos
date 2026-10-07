use super::*;

use crate::app::{App, GridSort, Region, SHOW_EYES_FILTER};
use crate::navigation::Cmp;

/// The grid's two rows of controls share these, in points before
/// `font_size::px` scales them: the gap between items, the room inside each
/// button, and the margin around the row.
const ITEM_GAP: f32 = 10.0;
pub(super) const BUTTON_PAD: egui::Vec2 = egui::vec2(8.0, 3.0);
const ROW_MARGIN: egui::Vec2 = egui::vec2(10.0, 6.0);

/// A panel for one of the grid's rows of controls, on top or at the
/// bottom, laid out with the spacing above.
fn toolbar_row<R>(
    ui: &mut egui::Ui,
    id: &'static str,
    bottom: bool,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let style = ui.style().clone();
    let margin = egui::Margin::symmetric(
        font_size::px(&style, ROW_MARGIN.x).round() as i8,
        font_size::px(&style, ROW_MARGIN.y).round() as i8,
    );
    let panel = if bottom {
        egui::Panel::bottom(id)
    } else {
        egui::Panel::top(id)
    };
    panel
        .frame(egui::Frame::side_top_panel(&style).inner_margin(margin))
        .show_inside(ui, |ui| {
            toolbar_spacing(ui);
            add(ui)
        })
        .inner
}

/// The toolbar rows' gaps and button padding, for a row drawn elsewhere.
pub(super) fn toolbar_spacing(ui: &mut egui::Ui) {
    let style = ui.style().clone();
    let spacing = ui.spacing_mut();
    spacing.item_spacing.x = font_size::px(&style, ITEM_GAP);
    spacing.button_padding = egui::vec2(
        font_size::px(&style, BUTTON_PAD.x),
        font_size::px(&style, BUTTON_PAD.y),
    );
    // A row centers its items in this height, so it must fit the padded
    // buttons or the labels ride high.
    let text = egui::TextStyle::Button.resolve(&style).size;
    spacing.interact_size.y = spacing
        .interact_size
        .y
        .max(text + 2.0 * spacing.button_padding.y);
}

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
    Sort(GridSort),
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
        Self::Sort(GridSort::Name),
        Self::Sort(GridSort::Quality),
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
            Self::FilterCmp(Cmp::Gte)
                | Self::Star(1)
                | Self::EyesClosed
                | Self::Sort(GridSort::Name)
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
            Self::Sort(sort) => UiAction::SetSort(sort),
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
            Self::Sort(GridSort::Name) => {
                ui.label(t.sort_by);
                ui.selectable_label(app.grid_sort() == GridSort::Name, t.sort_name)
            }
            Self::Sort(GridSort::Quality) => ui
                .selectable_label(app.grid_sort() == GridSort::Quality, t.sort_quality)
                .on_hover_text(t.sort_quality_tip),
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
    toolbar_row(ui, "grid_toolbar", false, |ui| {
        ui.horizontal(|ui| {
            let t = t();
            ui.label(t.rating_filter);
            for (idx, control) in ToolbarControl::drawn().enumerate() {
                // The count reads as the filters' result, so it closes their
                // group rather than trailing the sort.
                if matches!(control, ToolbarControl::Sort(GridSort::Name)) {
                    ui.separator();
                    ui.weak((t.n_photos)(app.visible_len()));
                }
                if control.starts_group() {
                    ui.separator();
                }
                let resp = control.widget(ui, app);
                if resp.clicked() {
                    out.actions.push(control.action(app));
                }
                toolbar_focus_sync(ui, app, idx, &resp, out);
            }

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

/// Actions in one row under the grid. The grid's own half acts on every
/// photo the filter shows: Score All, Group All Bursts and Auto Adjust All.
/// With one photo selected a second half acts on it; with more, the row
/// acts on the selection alone. Delete sits last behind a divider. The row
/// stays up with nothing selected so the grid doesn't shift when a
/// selection starts.
pub(super) fn selection_bar(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    let n = app.selection_count();
    toolbar_row(ui, "selection_bar", true, |ui| {
        // One line that never wraps; a window too narrow for it scrolls
        // sideways rather than hiding Export and Delete.
        egui::ScrollArea::horizontal()
            .id_salt("selection_bar_scroll")
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    // Buttons are taller than a label; keep the row one height.
                    ui.set_min_height(ui.spacing().interact_size.y);
                    if n <= 1 {
                        grid_actions(ui, app, out);
                    }
                    match n {
                        0 => {}
                        1 => {
                            ui.separator();
                            one_photo_actions(ui, app, out);
                            delete_button(ui, app, out);
                        }
                        n => {
                            selection_actions(ui, app, n, out);
                            delete_button(ui, app, out);
                        }
                    }
                });
            });
    });
}

/// The actions under the filmstrip: the Loupe photo's, or with several
/// photos selected in the strip, the selection bar's for all of them. The
/// bar already rates the shown photo with its stars, so one photo gets
/// neither the label nor Rate.
pub(super) fn strip_actions(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    match app.selection_count() {
        n if n > 1 => selection_actions(ui, app, n, out),
        _ => photo_actions(ui, app, out),
    }
    delete_button(ui, app, out);
}

/// "All n photos:" or, under a filter, "Filtered n photos:", then the
/// actions on all of them.
fn grid_actions(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    let t = t();
    let cells = app.visible_len();
    ui.strong(if app.filter().is_some() {
        (t.filtered_n_photos)(cells)
    } else {
        (t.all_n_photos)(cells)
    });
    let any = cells > 0;
    if app.scoring_available()
        && ui
            .add_enabled(any, egui::Button::new(t.score_all))
            .on_hover_text(t.score_all_tip)
            .clicked()
    {
        out.actions.push(UiAction::ScoreAll);
    }
    #[cfg(not(target_arch = "wasm32"))]
    if ui
        .add_enabled(
            app.group_bursts_available(),
            egui::Button::new(t.group_all_bursts),
        )
        .on_hover_text(t.group_all_bursts_tip)
        .clicked()
    {
        out.actions.push(UiAction::GroupAllBursts);
    }
    if ui
        .add_enabled(any, egui::Button::new(t.auto_adjust_all))
        .on_hover_text(t.auto_adjust_all_tip)
        .clicked()
    {
        out.actions
            .push(UiAction::RequestBulk(BulkKind::AutoToneAll));
    }
}

/// "Selected photo:" and the actions on that one photo.
fn one_photo_actions(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    ui.strong(t().selected_photo);
    rate_menu(ui, out);
    photo_actions(ui, app, out);
}

/// One photo's actions after its rating: score, adjust, export.
fn photo_actions(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    let t = t();
    score_button(ui, app, t.update_score, out);
    auto_adjust_button(ui, t.auto_tone, out);
    copy_button(ui, out);
    apply_adjustment(ui, app, out);
    if app.ungroup_button_enabled()
        && ui
            .button(t.menu.ungroup)
            .on_hover_text(crate::i18n::keys(t.ungroup_selection_tip))
            .clicked()
    {
        out.actions.push(UiAction::UngroupSelection);
    }
    export_button(ui, out);
}

/// "Selected n photos:" and the actions on them.
fn selection_actions(ui: &mut egui::Ui, app: &App, n: usize, out: &mut FrameOutput) {
    let t = t();
    ui.strong((t.selected_n_photos)(n));
    rate_menu(ui, out);
    score_button(ui, app, t.score_all, out);
    #[cfg(not(target_arch = "wasm32"))]
    if ui
        .add_enabled(
            app.group_bursts_available(),
            egui::Button::new(t.group_all_bursts),
        )
        .on_hover_text(crate::i18n::keys(t.group_bursts_tip))
        .clicked()
    {
        out.actions.push(UiAction::GroupBursts);
    }
    auto_adjust_button(ui, t.auto_adjust_all, out);
    apply_adjustment(ui, app, out);
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
    export_button(ui, out);
}

/// "Selected n photos", Rate and an Actions menu over the Compare pane's
/// picks, which the pane's toolbar has no room to lay out as buttons.
pub(super) fn pick_actions(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    let t = t();
    let picks = app.group_picks().len();
    if picks == 0 {
        return;
    }
    let label = if picks == 1 {
        t.selected_photo.to_string()
    } else {
        (t.selected_n_photos)(picks)
    };
    // The toolbar wraps, but never inside the label.
    ui.add(egui::Label::new(egui::RichText::new(label).strong()).extend());
    rate_menu(ui, out);
    // The toolbar sits at the window's bottom, so the menu opens upward.
    let button = ui.button(t.actions_menu);
    egui::Popup::menu(&button)
        .align(egui::RectAlign::TOP_START)
        .show(|ui| {
            if picks == 1 {
                score_button(ui, app, t.update_score, out);
                auto_adjust_button(ui, t.auto_tone, out);
                copy_button(ui, out);
            } else {
                score_button(ui, app, t.score_all, out);
                auto_adjust_button(ui, t.auto_adjust_all, out);
            }
            apply_adjustment(ui, app, out);
            export_button(ui, out);
            ui.separator();
            let delete = egui::Button::new(
                egui::RichText::new(t.delete).color(theme::colors(ui.ctx()).danger),
            );
            if ui
                .add_enabled(app.delete_available(), delete)
                .on_hover_text(t.delete_selection_tip)
                .clicked()
            {
                out.actions.push(UiAction::RequestDeletePicks);
            }
        });
}

fn copy_button(ui: &mut egui::Ui, out: &mut FrameOutput) {
    let t = t();
    if ui
        .button(t.copy_settings)
        .on_hover_text(crate::i18n::keys(t.copy_settings_tip))
        .clicked()
    {
        out.actions.push(UiAction::CopySettings);
    }
}

fn rate_menu(ui: &mut egui::Ui, out: &mut FrameOutput) {
    let t = t();
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
}

fn score_button(ui: &mut egui::Ui, app: &App, label: &str, out: &mut FrameOutput) {
    if app.scoring_available()
        && ui
            .button(label)
            .on_hover_text(crate::i18n::keys(t().score_selection_tip))
            .clicked()
    {
        out.actions.push(UiAction::ScoreSelection);
    }
}

fn auto_adjust_button(ui: &mut egui::Ui, label: &str, out: &mut FrameOutput) {
    let t = t();
    if ui
        .button(label)
        .on_hover_text(crate::i18n::keys(t.auto_tone_selection_tip))
        .clicked()
    {
        out.actions.push(UiAction::RequestBulk(BulkKind::AutoTone));
    }
}

/// Apply Adjustment, live once one is copied, with where it came from, and
/// the presets menu while presets are shown.
fn apply_adjustment(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    let t = t();
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
    if !crate::app::SHOW_PRESETS {
        return;
    }
    ui.add_enabled_ui(!app.presets().is_empty(), |ui| {
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
}

fn export_button(ui: &mut egui::Ui, out: &mut FrameOutput) {
    let t = t();
    if ui
        .button(t.export_jpg)
        .on_hover_text(t.export_jpg_tip)
        .clicked()
    {
        out.actions.push(UiAction::ToggleExportForm);
    }
}

/// Destructive, so it sits last, behind a divider.
fn delete_button(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    let t = t();
    ui.separator();
    let delete =
        egui::Button::new(egui::RichText::new(t.delete).color(theme::colors(ui.ctx()).danger));
    if ui
        .add_enabled(app.delete_available(), delete)
        .on_hover_text(t.delete_selection_tip)
        .clicked()
    {
        out.actions.push(UiAction::RequestBulk(BulkKind::Delete));
    }
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
