//! The home page, shown when no folder is open. It draws the Grid's own
//! layout with nothing in it, so the window looks the way it will once a
//! folder opens and the tour can point at each region: the left panel waits
//! for the folder tree, the toolbars and rail are switched off, and the
//! content area is a field of empty tiles under the prompt and the Open
//! Folder and Reopen Session buttons.

use super::*;

/// Panels claim space in the order `ui::draw` claims them for the Grid, so
/// every region sits where it will once a folder is open.
pub(super) fn draw_home(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    left_panel(ui);
    draw_develop_rail(ui, app, false, out);
    toolbar::placeholder_toolbar(ui, app);
    toolbar::placeholder_selection_bar(ui);
    content(ui, app, out);
    status_toast(ui, app);
    help_modal(ui, app, out);
    settings_modal(ui, app, out);
}

/// The left panel's width, the Grid's minimum for its folder tree.
const PANEL_WIDTH: f32 = 220.0;
const PANEL_MARGIN: f32 = 12.0;

/// "Folders" over the empty space the folder tree will fill.
fn left_panel(ui: &mut egui::Ui) {
    let width = font_size::px(ui.style(), PANEL_WIDTH);
    let panel = egui::Panel::left("folders")
        .resizable(false)
        .exact_size(width)
        .show_inside(ui, |ui| {
            let margin = font_size::px(ui.style(), PANEL_MARGIN);
            ui.add_space(margin);
            ui.label(egui::RichText::new(t().folders_heading).strong());
        });
    tour::anchor(ui.ctx(), tour::TourStep::Folders, panel.response.rect);
}

const BUTTON_WIDTH: f32 = 220.0;
const BUTTON_HEIGHT: f32 = 52.0;
const BUTTON_GAP: f32 = 12.0;

/// Open Folder, with Reopen Session beside it once a folder has been opened
/// before, centred as a row.
fn buttons(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    let style = ui.style().clone();
    let size = egui::vec2(
        font_size::px(&style, BUTTON_WIDTH),
        font_size::px(&style, BUTTON_HEIGHT),
    );
    let gap = font_size::px(&style, BUTTON_GAP);
    let session = app.saved_session();
    let row_w = if session.is_some() {
        2.0 * size.x + gap
    } else {
        size.x
    };
    ui.allocate_ui_with_layout(
        egui::vec2(row_w, size.y),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.spacing_mut().item_spacing.x = gap;
            open_folder_button(ui, app, size, out);
            if let Some(session) = session {
                let reopen = ui
                    .add_enabled(
                        !app.folder_pick_pending(),
                        egui::Button::new(
                            egui::RichText::new(t().reopen_session)
                                .size(font_size::px(&style, 18.0)),
                        )
                        .corner_radius(10.0)
                        .min_size(size),
                    )
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text((t().reopen_session_tip)(
                        &session.root.display().to_string(),
                    ));
                if reopen.clicked() {
                    out.actions.push(UiAction::ReopenSession);
                }
            }
        },
    );
}

/// The home page's one call to action, filled in the brand blue so it reads
/// as the thing to press.
fn open_folder_button(ui: &mut egui::Ui, app: &App, size: egui::Vec2, out: &mut FrameOutput) {
    let pending = app.folder_pick_pending();
    let label = if pending {
        t().opening
    } else {
        t().open_folder
    };
    let resp = ui.add_enabled(
        !pending,
        egui::Button::new(
            egui::RichText::new(label)
                .size(font_size::px(ui.style(), 20.0))
                .color(egui::Color32::WHITE)
                .strong(),
        )
        .fill(theme::BRAND_BLUE)
        .corner_radius(10.0)
        .min_size(size),
    );
    if resp.hovered() && !pending {
        // A fixed fill gives no hover feedback of its own.
        ui.painter().rect_stroke(
            resp.rect.expand(2.0),
            12.0,
            egui::Stroke::new(2.0_f32, theme::BRAND_BLUE.linear_multiply(0.5)),
            egui::StrokeKind::Outside,
        );
    }
    if resp
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
    {
        out.actions.push(UiAction::PickFolder);
    }
}

const TILE: f32 = crate::app::GRID_CELL_PT;
/// How far the overlay darkens the placeholder layout behind the card, and
/// the card's widest.
const OVERLAY_ALPHA: u8 = 150;
const CARD_WIDTH: f32 = 820.0;
/// The room inside the card, sides and top-bottom.
const CARD_PAD: egui::Vec2 = egui::vec2(72.0, 64.0);
const TILE_GAP: f32 = 8.0;

/// Rows of empty tiles where the thumbnails will go, the whole layout
/// dimmed, and a card over it holding the prompt, the buttons, on the web a
/// note on the browser's file permission, and a Take the Tour link.
fn content(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    let pal = theme::colors(ui.ctx());
    let tile_fill = pal.panel.lerp_to_gamma(pal.value, 0.05);
    let panel = egui::CentralPanel::default().show_inside(ui, |ui| {
        let area = ui.max_rect();
        let painter = ui.painter_at(area);
        let step = TILE + TILE_GAP;
        let cols = ((area.width() + TILE_GAP) / step).floor().max(1.0) as usize;
        let rows = ((area.height() + TILE_GAP) / step).ceil() as usize;
        for row in 0..rows {
            for col in 0..cols {
                let min = area.min + egui::vec2(col as f32 * step, row as f32 * step);
                painter.rect_filled(
                    egui::Rect::from_min_size(min, egui::vec2(TILE, TILE)),
                    6.0,
                    tile_fill,
                );
            }
        }

        // One dim layer over every placeholder region below the header, so
        // the layout reads as a preview behind the card. It paints on the
        // panels' own layer after them and before the card's widgets.
        let screen = ui.ctx().content_rect();
        let top =
            tour::anchored(ui.ctx(), tour::TourStep::Header).map_or(screen.min.y, |h| h.max.y);
        ui.ctx()
            .layer_painter(egui::LayerId::background())
            .rect_filled(
                egui::Rect::from_min_max(egui::pos2(screen.min.x, top), screen.max),
                0.0,
                egui::Color32::from_black_alpha(OVERLAY_ALPHA),
            );

        let prompt = egui::RichText::new(t().landing_prompt)
            .size(font_size::px(ui.style(), 32.0))
            .color(pal.value)
            .strong();
        let tagline = egui::RichText::new(t().landing_tagline)
            .size(font_size::px(ui.style(), 19.0))
            .color(pal.label);
        // The tiles pack to the left like the Grid's, so the card centres on
        // the block they fill, not on the panel.
        let tiles_w = cols as f32 * step - TILE_GAP;
        let width = (tiles_w - 32.0)
            .min(font_size::px(ui.style(), CARD_WIDTH))
            .max(120.0);
        // The card's height is known only once it is laid out, so it centres
        // on last frame's and discards the frame when that changes.
        let size_id = ui.id().with("home_card_size");
        let last: Option<egui::Vec2> = ui.ctx().data(|d| d.get_temp(size_id));
        let height = last.map_or(font_size::px(ui.style(), 420.0), |s| s.y);
        let center = egui::pos2(
            area.min.x + tiles_w.min(area.width()) / 2.0,
            area.center().y,
        );
        let card = ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(egui::Rect::from_center_size(
                    center,
                    egui::vec2(width, height),
                ))
                .layout(egui::Layout::top_down(egui::Align::Center)),
            |ui| {
                egui::Frame::popup(ui.style())
                    .fill(pal.panel)
                    .corner_radius(14.0)
                    .inner_margin(egui::Margin::symmetric(
                        font_size::px(ui.style(), CARD_PAD.x).min(127.0) as i8,
                        font_size::px(ui.style(), CARD_PAD.y).min(127.0) as i8,
                    ))
                    .show(ui, |ui| {
                        ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                            ui.add(egui::Label::new(prompt).wrap());
                            ui.add_space(font_size::px(ui.style(), 10.0));
                            ui.add(egui::Label::new(tagline).wrap());
                            ui.add_space(font_size::px(ui.style(), 40.0));
                            buttons(ui, app, out);
                            ui.add_space(font_size::px(ui.style(), 20.0));
                            allow_note(ui);
                            if ui.link(t().take_tour).on_hover_text(t().tour_tip).clicked() {
                                out.actions.push(UiAction::StartTour);
                            }
                        });
                    })
                    .response
                    .rect
                    .size()
            },
        );
        let size = card.inner;
        if last.is_none_or(|s| (s.y - size.y).abs() > 0.5) {
            ui.ctx().data_mut(|d| d.insert_temp(size_id, size));
            ui.ctx().request_discard("home card size");
        }
    });
    tour::anchor(ui.ctx(), tour::TourStep::Content, panel.response.rect);
}

/// Web only: picking a folder hands the browser a File System Access
/// permission, so the card says up front which button to press.
fn allow_note(ui: &mut egui::Ui) {
    let note = t().landing_allow_note;
    if note.is_empty() {
        return;
    }
    let note = egui::RichText::new(note).color(theme::colors(ui.ctx()).label);
    // Its lines start flush left, the block centred under the buttons.
    ui.add(egui::Label::new(note).halign(egui::Align::LEFT).wrap());
    ui.add_space(font_size::px(ui.style(), 20.0));
}
