// SPDX-License-Identifier: GPL-3.0-or-later

//! The base font size, which Alt+= and Alt+- step. egui's Body style is the
//! base, and every other text style and every hand-painted label keeps its
//! ratio to it, so one number sizes all the app's text.

use std::ops::RangeInclusive;

use egui::{FontId, Style, TextStyle};

/// egui's own Body size. Sizes written in the code are what they are at this base.
pub const DEFAULT: f32 = 13.0;
const RANGE: RangeInclusive<f32> = 9.0..=28.0;
const PREF_KEY: &str = "font_size";

/// Apply the saved base size, or the default when none is saved.
pub fn init(ctx: &egui::Context) {
    let saved = crate::prefs::load(PREF_KEY).and_then(|s| s.trim().parse::<f32>().ok());
    apply(ctx, saved.unwrap_or(DEFAULT));
}

/// Grow (`steps > 0`) or shrink the base size by one point per step, and
/// remember it for the next launch.
pub fn step(ctx: &egui::Context, steps: i32) {
    let size = base(&ctx.global_style()) + steps as f32;
    let size = apply(ctx, size);
    if let Err(e) = crate::prefs::save(PREF_KEY, &size.to_string()) {
        eprintln!("[lightphotos] could not save the font size: {e}");
    }
}

/// `size` at the current base, for a size written against [`DEFAULT`].
pub fn px(style: &Style, size: f32) -> f32 {
    size * base(style) / DEFAULT
}

fn base(style: &Style) -> f32 {
    style.text_styles[&TextStyle::Body].size
}

fn apply(ctx: &egui::Context, size: f32) -> f32 {
    let size = size.round().clamp(*RANGE.start(), *RANGE.end());
    let ratio = size / DEFAULT;
    let spacing = egui::style::Spacing::default();
    ctx.all_styles_mut(|style| {
        style.text_styles = egui::style::default_text_styles()
            .into_iter()
            .map(|(text_style, font)| (text_style, FontId::new(font.size * ratio, font.family)))
            .collect();
        // Widget heights and the chevrons on combo boxes sit beside text.
        style.spacing.interact_size = spacing.interact_size * ratio;
        style.spacing.icon_width = spacing.icon_width * ratio;
        style.spacing.icon_width_inner = spacing.icon_width_inner * ratio;
        style.spacing.icon_spacing = spacing.icon_spacing * ratio;
    });
    size
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_text_style_keeps_its_ratio_to_the_base() {
        let ctx = egui::Context::default();
        apply(&ctx, 26.0);
        let style = ctx.global_style();
        assert_eq!(style.text_styles[&TextStyle::Body].size, 26.0);
        assert_eq!(style.text_styles[&TextStyle::Heading].size, 36.0);
        assert_eq!(style.text_styles[&TextStyle::Small].size, 18.0);
        assert_eq!(px(&style, 18.0), 36.0);
    }

    #[test]
    fn the_base_stays_in_range() {
        let ctx = egui::Context::default();
        assert_eq!(apply(&ctx, 100.0), *RANGE.end());
        assert_eq!(apply(&ctx, 1.0), *RANGE.start());
    }
}
