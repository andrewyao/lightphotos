use super::*;

use crate::app::{App, GridSort, Region, SHOW_EYES_FILTER};
use crate::navigation::{Cmp, FlagFilter};

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
    // The bottom row is as tall as the side panels' footer tabs, and
    // centers its content in that height rather than taking a margin.
    let margin_y = if bottom { 0.0 } else { ROW_MARGIN.y };
    let margin = egui::Margin::symmetric(
        font_size::px(&style, ROW_MARGIN.x).round() as i8,
        font_size::px(&style, margin_y).round() as i8,
    );
    let height = super::tabs::footer_height(ui);
    let panel = if bottom {
        egui::Panel::bottom(id).exact_size(height)
    } else {
        egui::Panel::top(id)
    };
    panel
        .frame(egui::Frame::side_top_panel(&style).inner_margin(margin))
        .show_inside(ui, |ui| {
            toolbar_spacing(ui);
            if !bottom {
                return add(ui);
            }
            // Centered on the whole panel, its separator line included, as
            // the Loupe's info bar is, by the row's height last frame.
            let id = egui::Id::new(id).with("row_h");
            let row_h = ui.ctx().data(|d| d.get_temp::<f32>(id));
            let center = ui.ctx().content_rect().bottom() - height / 2.0;
            let h = row_h.unwrap_or(ui.spacing().interact_size.y);
            ui.add_space((center - h / 2.0 - ui.cursor().top()).max(0.0));
            let row = ui.scope(add);
            let drawn = row.response.rect.height();
            if row_h.is_none_or(|h| (h - drawn).abs() > 0.5) {
                ui.ctx().data_mut(|d| d.insert_temp(id, drawn));
                ui.ctx().request_repaint();
            }
            row.inner
        })
        .inner
}

/// The toolbar rows' gaps and button padding, for a row drawn elsewhere.
/// One row of `add`, centered in the width available by last frame's
/// width, as the Loupe's bar is. A row wider than the space starts at the
/// left.
fn centered_row<R>(ui: &mut egui::Ui, id: &'static str, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let id = egui::Id::new(id);
    let last = ui.ctx().data(|d| d.get_temp::<f32>(id));
    let avail = ui.available_width();
    ui.horizontal(|ui| {
        if let Some(w) = last {
            let gap = ui.spacing().item_spacing.x;
            ui.add_space(((avail - w) / 2.0 - gap).max(0.0));
        }
        // Its own row, so the keyboard cursor's outline skips the lead-in.
        let row = ui.horizontal(add);
        let w = row.response.rect.width();
        if last.is_none_or(|l| (l - w).abs() > 0.5) {
            ui.ctx().data_mut(|d| d.insert_temp(id, w));
            ui.ctx().request_repaint();
        }
        row.inner
    })
    .inner
}

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
    FilterCmp(Cmp),
    Star(u8),
    Unrated,
    Flag(FlagFilter),
    EyesClosed,
    Sort,
}

impl ToolbarControl {
    const ALL: &'static [Self] = &[
        Self::Unrated,
        Self::FilterCmp(Cmp::Gte),
        Self::FilterCmp(Cmp::Eq),
        Self::FilterCmp(Cmp::Lte),
        Self::Star(1),
        Self::Star(2),
        Self::Star(3),
        Self::Star(4),
        Self::Star(5),
        Self::Flag(FlagFilter::Unflagged),
        Self::Flag(FlagFilter::Picked),
        Self::Flag(FlagFilter::Rejected),
        Self::EyesClosed,
        Self::Sort,
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
                | Self::Flag(FlagFilter::Unflagged)
                | Self::EyesClosed
                | Self::Sort
        )
    }

    /// What a click on this control, or Enter on it, does.
    pub(crate) fn action(self, app: &App) -> UiAction {
        match self {
            Self::FilterCmp(cmp) => UiAction::SetFilterCmp(cmp),
            // Clicking the star the filter is on clears it, as the toggles do.
            Self::Star(n) => {
                let star = Some((app.filter_cmp(), n));
                UiAction::SetFilter(if app.filter() == star { None } else { star })
            }
            Self::Unrated => {
                let unrated = matches!(app.filter(), Some((Cmp::Eq, 0)));
                UiAction::SetFilter(if unrated { None } else { Some((Cmp::Eq, 0)) })
            }
            // Like a star, clicking the shown flag again shows all.
            Self::Flag(f) if f == app.flag_filter() => UiAction::SetFlagFilter(FlagFilter::All),
            Self::Flag(f) => UiAction::SetFlagFilter(f),
            Self::EyesClosed => UiAction::ToggleEyesClosed,
            // The keyboard steps to the next sort; a click opens the dropdown.
            Self::Sort => {
                let sorts = sort_choices(app);
                let at = sorts.iter().position(|&s| s == app.grid_sort());
                UiAction::SetSort(sorts[at.map_or(0, |i| (i + 1) % sorts.len())])
            }
        }
    }

    fn widget(self, ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) -> egui::Response {
        let t = t();
        match self {
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
            Self::Flag(f) => {
                if f == FlagFilter::Unflagged {
                    ui.label(t.flag_filter_label);
                }
                let state = match f {
                    FlagFilter::Picked => Some(Flag::Pick),
                    FlagFilter::Rejected => Some(Flag::Reject),
                    _ => None,
                };
                let on = if app.flag_filter() == f {
                    FlagCoverage::All
                } else {
                    FlagCoverage::None
                };
                let colors = theme::colors(ui.ctx());
                let side = font_size::px(ui.style(), 20.0);
                super::flag_button(ui, state, on, side, (colors.value, colors.label))
                    .on_hover_text((t.show_flagged)(crate::app::flag_name(state)))
            }
            Self::Unrated => {
                let on = matches!(app.filter(), Some((Cmp::Eq, 0)));
                unrated_button(ui, on).on_hover_text(t.unrated_tip)
            }
            Self::EyesClosed => ui
                .add(egui::Button::selectable(
                    app.eyes_filter_on(),
                    t.eyes_closed,
                ))
                .on_hover_text(t.eyes_closed_tip),
            Self::Sort => {
                ui.label(t.sort_by);
                let current = app.grid_sort();
                let name = |sort| match sort {
                    GridSort::Name => t.sort_name,
                    GridSort::Time => t.sort_time,
                    GridSort::Quality => t.sort_quality,
                };
                egui::ComboBox::from_id_salt("grid_sort")
                    .selected_text(name(current))
                    .width(0.0)
                    .show_ui(ui, |ui| {
                        for sort in sort_choices(app) {
                            let mut item = ui.selectable_label(current == sort, name(sort));
                            match sort {
                                GridSort::Time => item = item.on_hover_text(t.sort_time_tip),
                                GridSort::Quality => item = item.on_hover_text(t.sort_quality_tip),
                                GridSort::Name => {}
                            }
                            if item.clicked() && current != sort {
                                out.actions.push(UiAction::SetSort(sort));
                            }
                        }
                    })
                    .response
            }
        }
    }
}

/// The Grid's sorts on offer. Quality waits out a scoring run, since each
/// new score would reorder the Grid, and the browser can't read capture
/// times.
fn sort_choices(app: &App) -> Vec<GridSort> {
    let mut sorts = vec![GridSort::Name];
    if !cfg!(target_arch = "wasm32") {
        sorts.push(GridSort::Time);
    }
    if app.quality_sort_available() {
        sorts.push(GridSort::Quality);
    }
    sorts
}

/// The Unrated filter as an icon toggle beside the flag ones: a hollow star
/// struck through, lit while the filter is on.
fn unrated_button(ui: &mut egui::Ui, on: bool) -> egui::Response {
    let side = font_size::px(ui.style(), 20.0);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(side, side), egui::Sense::click());
    let colors = theme::colors(ui.ctx());
    let color = if on {
        colors.star
    } else if resp.hovered() {
        colors.value
    } else {
        colors.label
    };
    let painter = ui.painter();
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        "\u{2606}",
        egui::FontId::proportional(side * 0.9),
        color,
    );
    let r = rect.shrink(side * 0.2);
    painter.line_segment(
        [r.left_bottom(), r.right_top()],
        egui::Stroke::new((side * 0.08).max(1.0), color),
    );
    resp
}

/// The flag states in the order their icons sit: Unflagged, Picked, Rejected.
pub(super) const FLAG_STATES: [Option<Flag>; 3] = [None, Some(Flag::Pick), Some(Flag::Reject)];

/// "Flag:" and a dropdown of the flag filters, for the grid's toolbar and
/// the Compare pane's. `pick` gets a choice other than `current`.
pub(super) fn flag_filter_menu(
    ui: &mut egui::Ui,
    id: &'static str,
    choices: &[FlagFilter],
    current: FlagFilter,
    mut pick: impl FnMut(FlagFilter),
) -> egui::Response {
    let t = t();
    ui.label(t.flag_filter_label);
    let name = |f| match f {
        FlagFilter::All => t.all,
        FlagFilter::Picked => t.flag_picked,
        FlagFilter::Unflagged => t.flag_unflagged,
        FlagFilter::Rejected => t.flag_rejected,
        FlagFilter::NotRejected => t.flag_not_rejected,
    };
    egui::ComboBox::from_id_salt(id)
        .selected_text(name(current))
        .width(0.0)
        .show_ui(ui, |ui| {
            for &f in choices {
                if ui.selectable_label(current == f, name(f)).clicked() && current != f {
                    pick(f);
                }
            }
        })
        .response
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
                if control.starts_group() {
                    ui.separator();
                }
                let resp = control.widget(ui, app, out);
                if resp.clicked() && !matches!(control, ToolbarControl::Sort) {
                    out.actions.push(control.action(app));
                }
                toolbar_focus_sync(ui, app, idx, &resp, out);
            }
            ui.weak((t.n_of_m_photos)(app.shown_photos(), app.total_photos()));
            all_photos_menu(ui, app, out);

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
                centered_row(ui, "selection_bar_w", |ui| {
                    // Buttons are taller than a label; keep the row one height.
                    ui.set_min_height(ui.spacing().interact_size.y);
                    // The selection reads as it does under the filmstrip.
                    if n > 0 {
                        strip_bar(ui, app, out);
                    }
                });
            });
    });
}

/// The bar under the filmstrip, over its selection: "Selected photo" or
/// "Selected n photos", the stars, the score, Copy and Apply Adjustment as
/// one pair, and an Actions menu. Across photos rated differently the
/// stars fill solid up to the lowest rating and half-transparent up to the
/// highest, and the score reads as a range.
pub(super) fn strip_bar(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    let t = t();
    let n = app.selection_count().max(1);
    let colors = theme::colors(ui.ctx());
    ui.strong(if n == 1 {
        t.selected_photo.to_string()
    } else {
        (t.selected_n_photos)(n)
    });

    let (lo, hi) = app.selection_rating_span();
    let star_w = font_size::px(ui.style(), 20.0);
    let star_font = egui::FontId::proportional(font_size::px(ui.style(), 18.0));
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for s in 1..=5u8 {
            let (r, resp) =
                ui.allocate_exact_size(egui::vec2(star_w, star_w), egui::Sense::click());
            let (glyph, color) = if s <= lo {
                ("\u{2605}", colors.star)
            } else if s <= hi {
                ("\u{2605}", colors.star.gamma_multiply(0.5))
            } else {
                ("\u{2606}", colors.label)
            };
            ui.painter().text(
                r.center(),
                egui::Align2::CENTER_CENTER,
                glyph,
                star_font.clone(),
                color,
            );
            if resp.clicked() {
                // Clicking the rating every photo already has clears it, as in Lightroom.
                let stars = if lo == s && hi == s { 0 } else { s };
                out.actions.push(if n == 1 {
                    UiAction::SetRating(stars)
                } else {
                    UiAction::RequestBulk(BulkKind::Rate(stars))
                });
            }
        }
    });
    let flag_w = font_size::px(ui.style(), 20.0);
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for flag in FLAG_STATES {
            let coverage = app.selection_flag_coverage(flag);
            let resp = super::flag_button(ui, flag, coverage, flag_w, (colors.value, colors.label))
                .on_hover_text((t.set_flag_tip)(crate::app::flag_name(flag)));
            // Marking photos with the state they all have already does nothing.
            if resp.clicked() && coverage != FlagCoverage::All {
                out.actions.push(if n == 1 {
                    UiAction::SetFlag(flag)
                } else {
                    UiAction::RequestBulk(BulkKind::Flag(flag))
                });
            }
        }
    });
    if n == 1 {
        if let Some(label) = app.selected_label() {
            let (r, _) =
                ui.allocate_exact_size(egui::vec2(star_w / 2.0, star_w), egui::Sense::hover());
            ui.painter().circle_filled(
                r.center(),
                font_size::px(ui.style(), 5.0),
                super::label_color(label),
            );
        }
    }

    if n == 1 {
        if let Some((score, stale)) = app.shown_score() {
            ui.label(t.score_label);
            ui.label(
                egui::RichText::new(score.value.to_string())
                    .color(super::grid::score_color(&colors, stale)),
            )
            .on_hover_text(super::grid::score_tip(&score, stale));
        }
    } else if let Some((lo, hi, stale)) = app.selection_score_span() {
        ui.label(t.score_label);
        let range = if lo == hi {
            lo.to_string()
        } else {
            format!("{lo}\u{2013}{hi}")
        };
        ui.label(egui::RichText::new(range).color(super::grid::score_color(&colors, stale)));
    }

    adjustment_pair(ui, app, n == 1, out);

    // The bar sits at the window's bottom, so the menu opens upward.
    let button = ui.button(actions_label(true));
    egui::Popup::menu(&button)
        .align(egui::RectAlign::TOP_START)
        .show(|ui| {
            // Several photos get a heading naming what the menu acts on.
            let score = if n == 1 {
                t.update_score
            } else {
                ui.weak((t.selected_n_photos_title)(n));
                t.update_scores
            };
            score_button(ui, app, score, out);
            auto_adjust_button(ui, t.auto_tone, out);
            if ui
                .button(t.export_jpg)
                .on_hover_text(t.export_jpg_tip)
                .clicked()
            {
                out.actions.push(UiAction::ToggleExportForm);
            }
            ui.separator();
            let delete = egui::Button::new(egui::RichText::new(t.delete).color(colors.danger));
            if ui
                .add_enabled(app.delete_available(), delete)
                .on_hover_text(t.delete_selection_tip)
                .clicked()
            {
                out.actions.push(UiAction::RequestBulk(BulkKind::Delete));
            }
        });
}

/// Copy Adjustment and Apply Adjustment as one control split down the
/// middle. Copy takes one photo's adjustment, so it waits for a single
/// selection.
fn adjustment_pair(ui: &mut egui::Ui, app: &App, one: bool, out: &mut FrameOutput) {
    let t = t();
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.x = 1.0;
        let r = ui.visuals().widgets.inactive.corner_radius;
        let left = egui::CornerRadius { ne: 0, se: 0, ..r };
        let right = egui::CornerRadius { nw: 0, sw: 0, ..r };
        if ui
            .add_enabled(one, egui::Button::new(t.copy_settings).corner_radius(left))
            .on_hover_text(crate::i18n::keys(t.copy_settings_tip))
            .clicked()
        {
            out.actions.push(UiAction::CopySettings);
        }
        let apply = egui::Button::new(t.apply_settings).corner_radius(right);
        let mut apply = ui
            .add_enabled(app.has_copied_settings(), apply)
            .on_disabled_hover_text(t.apply_settings_needs_copy);
        if let Some(name) = app.copied_settings_name() {
            apply = apply.on_hover_text((t.settings_from)(&name));
        }
        if apply.clicked() {
            out.actions
                .push(UiAction::RequestBulk(BulkKind::ApplySettings));
        }
    });
}

/// The Actions menu on every photo the Grid shows, after their count.
fn all_photos_menu(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    let t = t();
    let any = app.visible_len() > 0;
    let button = ui.add_enabled(any, egui::Button::new(actions_label(false)));
    egui::Popup::menu(&button).show(|ui| {
        if app.scoring_available()
            && ui
                .button(t.score_all)
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
            .button(t.auto_adjust_all)
            .on_hover_text(t.auto_adjust_all_tip)
            .clicked()
        {
            out.actions
                .push(UiAction::RequestBulk(BulkKind::AutoToneAll));
        }
    });
}

/// "Actions" with an arrow for which way its menu opens.
fn actions_label(up: bool) -> String {
    let arrow = if up { "\u{23f6}" } else { "\u{23f7}" };
    format!("{} {arrow}", t().actions_menu)
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
    let button = ui.button(actions_label(true));
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
    // Only as wide as its label, so it fits the Compare pane's toolbar.
    egui::ComboBox::from_id_salt("bulk_star")
        .selected_text(t.rate_menu)
        .width(0.0)
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
