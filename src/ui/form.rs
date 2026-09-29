// SPDX-License-Identifier: GPL-3.0-or-later

//! The one layout every form follows: an optional title, sections, each a
//! header over rows of a weak label and its value, top aligned, with a
//! divider and whitespace between sections, and a footer of buttons on the
//! right. Every size goes through `font_size::px`, so forms scale with Alt+=
//! and Alt+-.

use std::cell::Cell;

use super::{font_size, theme};

pub(super) const HEADER: f32 = 15.0;
const HEADER_GAP: f32 = 8.0;
const ROW_GAP: f32 = 10.0;
const BEFORE_DIVIDER: f32 = 14.0;
const AFTER_DIVIDER: f32 = 18.0;
const TITLE_GAP: f32 = 6.0;
const FOOTER_GAP: f32 = 20.0;
const BUTTON_MIN_WIDTH: f32 = 84.0;
const BUTTON_PAD: egui::Vec2 = egui::vec2(12.0, 5.0);
const BUTTON_GAP: f32 = 8.0;
const DIALOG_MARGIN: f32 = 20.0;

/// Confirms, prompts and Settings.
pub(super) const DIALOG_WIDTH: f32 = 400.0;

pub(super) struct Form {
    label_w: f32,
    first: Cell<bool>,
}

impl Form {
    /// A form whose label column fits the widest of `labels`.
    pub fn new(ui: &egui::Ui, labels: &[&str]) -> Self {
        let body = egui::TextStyle::Body.resolve(ui.style());
        let widest = labels
            .iter()
            .map(|label| {
                ui.fonts_mut(|f| {
                    f.layout_no_wrap(label.to_string(), body.clone(), egui::Color32::WHITE)
                        .size()
                        .x
                })
            })
            .fold(0.0, f32::max);
        Self {
            label_w: widest + ui.spacing().item_spacing.x * 2.0,
            first: Cell::new(true),
        }
    }

    /// A header over `body`'s rows, set off from the section before it. An
    /// empty `title` draws no header.
    pub fn section(&self, ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
        if !self.first.replace(false) {
            ui.add_space(font_size::px(ui.style(), BEFORE_DIVIDER));
            ui.separator();
            ui.add_space(font_size::px(ui.style(), AFTER_DIVIDER));
        }
        if !title.is_empty() {
            let size = font_size::px(ui.style(), HEADER);
            ui.label(egui::RichText::new(title).size(size).strong());
            ui.add_space(font_size::px(ui.style(), HEADER_GAP));
        }
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.y = font_size::px(ui.style(), ROW_GAP);
            body(ui);
        });
    }

    /// `label` in the label column and `value` beside it, both from the row's
    /// top. Returns the label.
    pub fn row(
        &self,
        ui: &mut egui::Ui,
        label: &str,
        value: impl FnOnce(&mut egui::Ui),
    ) -> egui::Response {
        ui.horizontal_top(|ui| {
            let label = ui
                .allocate_ui_with_layout(
                    egui::vec2(self.label_w, 0.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.set_width(self.label_w);
                        ui.label(egui::RichText::new(label).weak())
                    },
                )
                .inner;
            ui.vertical(|ui| {
                // Controls stacked inside one value keep egui's own spacing;
                // the wider gap is for between rows.
                ui.spacing_mut().item_spacing.y = ui.ctx().global_style().spacing.item_spacing.y;
                value(ui)
            });
            label
        })
        .inner
    }
}

/// A modal dialog `width` wide, before scaling, with the forms' margin.
pub(super) fn dialog<R>(
    ctx: &egui::Context,
    id: &str,
    width: f32,
    body: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::ModalResponse<R> {
    let style = ctx.global_style();
    let margin = font_size::px(&style, DIALOG_MARGIN);
    egui::Modal::new(egui::Id::new(id))
        .frame(egui::Frame::popup(&style).inner_margin(margin))
        .show(ctx, |ui| {
            ui.set_width(font_size::px(ui.style(), width));
            body(ui)
        })
}

/// A dialog's or panel's heading, ruled off from the form under it.
pub(super) fn title(ui: &mut egui::Ui, text: &str) {
    ui.heading(text);
    ui.add_space(font_size::px(ui.style(), TITLE_GAP));
    ui.separator();
    ui.add_space(font_size::px(ui.style(), TITLE_GAP));
}

/// A side panel's title. A panel, unlike a dialog, has no margin above it.
pub(super) fn panel_title(ui: &mut egui::Ui, text: &str) {
    ui.add_space(font_size::px(ui.style(), TITLE_GAP));
    title(ui, text);
}

/// Help for a value, set under it.
pub(super) fn hint(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).small().weak());
}

/// What went wrong with a value, set under it.
pub(super) fn error(ui: &mut egui::Ui, text: &str) {
    ui.label(
        egui::RichText::new(text)
            .small()
            .color(theme::colors(ui.ctx()).danger),
    );
}

/// What a footer button does. The role, not the caller, decides where the
/// button sits and how it is filled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Role {
    Cancel,
    Primary,
    /// A primary that destroys something.
    Danger,
}

pub(super) struct Button<'a> {
    pub label: &'a str,
    pub role: Role,
    pub enabled: bool,
}

impl<'a> Button<'a> {
    pub fn new(label: &'a str, role: Role) -> Self {
        Self {
            label,
            role,
            enabled: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Platform {
    /// macOS, and also Linux and the web, whose GNOME and browser dialogs
    /// agree with it: Cancel, then the action.
    Mac,
    /// The action, then Cancel.
    Windows,
}

impl Platform {
    const CURRENT: Self = if cfg!(target_os = "windows") {
        Platform::Windows
    } else {
        Platform::Mac
    };
}

/// The left-to-right order of `roles` on `platform`, as indices into `roles`.
fn order(roles: &[Role], platform: Platform) -> Vec<usize> {
    let cancel_first = platform == Platform::Mac;
    let mut indices: Vec<usize> = (0..roles.len()).collect();
    indices.sort_by_key(|&i| (roles[i] == Role::Cancel) != cancel_first);
    indices
}

/// A row of `buttons` against the right edge, set off from the form above
/// and ordered for this platform. Returns the role of the button clicked.
pub(super) fn footer(ui: &mut egui::Ui, buttons: &[Button]) -> Option<Role> {
    ui.add_space(font_size::px(ui.style(), FOOTER_GAP));
    let roles: Vec<Role> = buttons.iter().map(|b| b.role).collect();
    let mut clicked = None;
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
        ui.spacing_mut().item_spacing.x = font_size::px(ui.style(), BUTTON_GAP);
        // A right-to-left layout places the rightmost button first.
        for i in order(&roles, Platform::CURRENT).into_iter().rev() {
            if button(ui, &buttons[i]).clicked() {
                clicked = Some(buttons[i].role);
            }
        }
    });
    clicked
}

/// One footer-style button, for an action that belongs to a row rather than
/// to the whole form, such as Connect.
pub(super) fn button(ui: &mut egui::Ui, b: &Button) -> egui::Response {
    let colors = theme::colors(ui.ctx());
    let filled = match b.role {
        Role::Cancel => None,
        Role::Primary => Some((colors.primary_fill, colors.primary_text)),
        Role::Danger => Some((colors.danger_fill, colors.danger_text)),
    };
    let widget = match filled {
        Some((fill, text)) => {
            egui::Button::new(egui::RichText::new(b.label).color(text)).fill(fill)
        }
        None => egui::Button::new(b.label),
    };
    let min = egui::vec2(font_size::px(ui.style(), BUTTON_MIN_WIDTH), 0.0);
    ui.scope(|ui| {
        ui.spacing_mut().button_padding = BUTTON_PAD * font_size::px(ui.style(), 1.0);
        ui.add_enabled(b.enabled, widget.min_size(min))
    })
    .inner
}

/// A short single choice as one bar of equal segments filling the row, each
/// `(value, label, tooltip)`, with `current` filled. Returns the segment
/// clicked, if it isn't `current` already.
pub(super) fn segmented<T: Copy + PartialEq>(
    ui: &mut egui::Ui,
    choices: &[(T, &str, Option<&str>)],
    current: T,
) -> Option<T> {
    let font = egui::TextStyle::Body.resolve(ui.style());
    // A button's height, so the label beside it lines up with its text the
    // way it does with a combo box or a text field.
    let height = ui.spacing().interact_size.y;
    let width = ui.available_width();
    let (row, bar) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    let segment_w = width / choices.len() as f32;
    let colors = theme::colors(ui.ctx());
    let visuals = ui.visuals().clone();
    let radius = visuals.widgets.inactive.corner_radius;
    ui.painter()
        .rect_filled(row, radius, visuals.widgets.inactive.weak_bg_fill);
    let mut picked = None;
    for (i, &(value, label, tip)) in choices.iter().enumerate() {
        let rect = egui::Rect::from_min_size(
            row.min + egui::vec2(segment_w * i as f32, 0.0),
            egui::vec2(segment_w, height),
        );
        let mut resp = ui.interact(rect, bar.id.with(i), egui::Sense::click());
        let selected = value == current;
        resp.widget_info(|| {
            egui::WidgetInfo::selected(egui::WidgetType::Button, ui.is_enabled(), selected, label)
        });
        let text = if selected {
            ui.painter().rect_filled(rect, radius, colors.primary_fill);
            colors.primary_text
        } else {
            if resp.hovered() {
                ui.painter()
                    .rect_filled(rect, radius, visuals.widgets.hovered.weak_bg_fill);
            }
            visuals.text_color()
        };
        ui.painter_at(rect).text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            label,
            font.clone(),
            text,
        );
        if let Some(tip) = tip {
            resp = resp.on_hover_text(tip);
        }
        if resp.clicked() && !selected {
            picked = Some(value);
        }
    }
    picked
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Seen {
        labels: Vec<egui::Rect>,
        values: Vec<egui::Rect>,
    }

    /// Two sections of two rows, each value two lines tall.
    fn draw() -> Seen {
        let mut seen = Seen::default();
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(400.0, 600.0),
            )),
            ..Default::default()
        };
        let _ = ctx.run_ui(input, |ui| {
            let labels = ["Short", "A much longer label"];
            let form = Form::new(ui, &labels);
            for title in ["First", "Second"] {
                form.section(ui, title, |ui| {
                    for label in labels {
                        let l = form.row(ui, label, |ui| {
                            seen.values.push(ui.label("line one\nline two").rect);
                        });
                        seen.labels.push(l.rect);
                    }
                });
            }
        });
        seen
    }

    #[test]
    fn rows_share_a_label_column_and_align_to_their_tops() {
        let seen = draw();
        for (l, v) in seen.labels.iter().zip(&seen.values) {
            assert_eq!(l.left(), seen.labels[0].left(), "labels share one column");
            assert_eq!(v.left(), seen.values[0].left(), "values share one column");
            assert_eq!(
                l.top(),
                v.top(),
                "a two-line value starts level with its label"
            );
        }
        let widest = seen.labels.iter().map(|l| l.right()).fold(0.0, f32::max);
        assert!(
            seen.values[0].left() > widest,
            "values sit past the widest label"
        );
    }

    #[test]
    fn sections_are_divided_and_spaced() {
        let seen = draw();
        let first_end = seen.values[1].bottom();
        let second_row = seen.labels[2].top();
        assert!(
            second_row - first_end > BEFORE_DIVIDER + AFTER_DIVIDER + HEADER,
            "the next section's first row clears the divider, the gap and its header: {first_end} -> {second_row}"
        );
        let in_section = seen.labels[1].top() - seen.values[0].bottom();
        assert!(
            in_section >= ROW_GAP - 0.5,
            "rows are {ROW_GAP}px apart, got {in_section}"
        );
    }

    #[test]
    fn cancel_leads_on_mac_and_trails_on_windows() {
        use Role::*;
        let pick = |roles: &[Role], platform| -> Vec<Role> {
            order(roles, platform)
                .into_iter()
                .map(|i| roles[i])
                .collect()
        };
        for roles in [[Cancel, Primary], [Primary, Cancel]] {
            assert_eq!(pick(&roles, Platform::Mac), [Cancel, Primary]);
            assert_eq!(pick(&roles, Platform::Windows), [Primary, Cancel]);
        }
        assert_eq!(pick(&[Danger, Cancel], Platform::Mac), [Cancel, Danger]);
        assert_eq!(pick(&[Cancel, Danger], Platform::Windows), [Danger, Cancel]);
        assert_eq!(pick(&[Primary], Platform::Windows), [Primary]);
    }

    /// Where a drawing sat: the `Ui`'s full rect and the bottom of what was
    /// drawn.
    #[derive(Clone, Copy)]
    struct Drawn {
        max: egui::Rect,
        bottom: f32,
    }

    /// Lays out `draw` once, then clicks the point `at` picks from that
    /// layout, and returns what `draw` returned on the frame of the release.
    fn click<R>(mut draw: impl FnMut(&mut egui::Ui) -> R, at: impl Fn(Drawn) -> egui::Pos2) -> R {
        let ctx = egui::Context::default();
        // egui snaps a click within a few points of a widget onto it, which
        // would hide a button a little narrower or further off than it should be.
        ctx.all_styles_mut(|s| s.interaction.interact_radius = 0.0);
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 600.0));
        let mut frame = |events: Vec<egui::Event>| {
            let input = egui::RawInput {
                screen_rect: Some(screen),
                events,
                ..Default::default()
            };
            let mut out = None;
            let _ = ctx.run_ui(input, |ui| {
                let max = ui.max_rect();
                let result = draw(ui);
                let bottom = ui.min_rect().bottom();
                out = Some((result, Drawn { max, bottom }));
            });
            out.expect("drawn")
        };
        let (_, drawn) = frame(vec![]);
        let pos = at(drawn);
        let button = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        frame(vec![egui::Event::PointerMoved(pos)]);
        frame(vec![button(true)]);
        frame(vec![button(false)]).0
    }

    fn footer_of(enabled: bool) -> impl FnMut(&mut egui::Ui) -> Option<Role> {
        move |ui| {
            footer(
                ui,
                &[
                    Button::new("OK", Role::Primary),
                    Button {
                        enabled,
                        ..Button::new("No", Role::Cancel)
                    },
                ],
            )
        }
    }

    #[test]
    fn footer_buttons_share_one_width_against_the_right_edge() {
        let roles = [Role::Primary, Role::Cancel];
        let ordered = order(&roles, Platform::CURRENT);
        let (left, right) = (roles[ordered[0]], roles[ordered[1]]);
        // Both labels are shorter than the minimum, so each button is exactly
        // that wide, and the gap sits between them.
        let w = BUTTON_MIN_WIDTH;
        let spans = [
            (-2.0, right),
            (-w + 2.0, right),
            (-w - BUTTON_GAP - 2.0, left),
            (-2.0 * w - BUTTON_GAP + 2.0, left),
        ];
        for (dx, expected) in spans {
            let got = click(footer_of(true), |d| {
                egui::pos2(d.max.right() + dx, d.bottom - 4.0)
            });
            assert_eq!(got, Some(expected), "{dx}px from the right edge");
        }
        let gap = click(footer_of(true), |d| {
            egui::pos2(d.max.right() - w - BUTTON_GAP / 2.0, d.bottom - 4.0)
        });
        assert_eq!(gap, None, "the gap between the buttons clicks nothing");
    }

    #[test]
    fn a_disabled_footer_button_never_clicks() {
        let cancel_x = |d: Drawn| {
            let right_is_cancel = *order(&[Role::Primary, Role::Cancel], Platform::CURRENT)
                .last()
                .unwrap()
                == 1;
            let x = if right_is_cancel {
                d.max.right() - BUTTON_MIN_WIDTH / 2.0
            } else {
                d.max.right() - BUTTON_MIN_WIDTH * 1.5 - BUTTON_GAP
            };
            egui::pos2(x, d.bottom - 4.0)
        };
        assert_eq!(click(footer_of(true), cancel_x), Some(Role::Cancel));
        assert_eq!(click(footer_of(false), cancel_x), None);
    }

    #[test]
    fn segments_split_the_row_equally_and_span_it() {
        let draw = |ui: &mut egui::Ui| {
            segmented(
                ui,
                &[
                    (0, "One", None),
                    (1, "Two", Some("tip")),
                    (2, "Three", None),
                ],
                0,
            )
        };
        let at = |fraction: f32| {
            move |d: Drawn| egui::pos2(d.max.left() + d.max.width() * fraction, d.bottom - 4.0)
        };
        assert_eq!(click(draw, at(0.5)), Some(1));
        assert_eq!(click(draw, at(0.66)), Some(1), "just short of two thirds");
        assert_eq!(click(draw, at(0.68)), Some(2), "just past two thirds");
        assert_eq!(click(draw, at(0.995)), Some(2), "the last reaches the edge");
        assert_eq!(click(draw, at(0.1)), None, "the current segment");
    }
}
