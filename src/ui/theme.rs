// SPDX-License-Identifier: GPL-3.0-or-later

//! The three color themes and the accent colors they share. Dark and Light
//! fix their text colors; Medium derives its text from its gray, so text stays
//! light while the gray sits on the dark side and turns dark past the middle.
//! Every theme has two text tiers: a label color for names and captions, and a
//! value color with more contrast for the data they name.

use egui::{Color32, Stroke};

/// Colors that read on every theme's panels, or that sit on a photo.
pub const BURST_BADGE: Color32 = Color32::from_rgb(120, 230, 160);
/// The badge colors differ from each other and from the stars because one
/// photo can carry several badges at once. Eyes-closed is the only cool color
/// because it marks a defect, not a keeper.
pub const DUP_BADGE: Color32 = Color32::from_rgb(255, 150, 90);
pub const EYES_BADGE: Color32 = Color32::from_rgb(150, 190, 255);
/// The wordmark's "Photos" color. Matches `--lp-accent` in lightphotos.app's
/// `lp.css`. The site uses a gradient that egui can't draw, so this is its
/// dominant color. Update it if `lp.css` changes.
pub const BRAND_BLUE: Color32 = Color32::from_rgb(79, 140, 255);

const STAR_GOLD: Color32 = Color32::from_rgb(255, 210, 80);
/// Mouse selection outline on a thumbnail cell.
const SELECTION_BLUE: Color32 = Color32::from_rgb(90, 160, 255);
const SELECTION_BG: Color32 = Color32::from_rgb(40, 80, 140);
/// Keyboard cursor outline, kept distinct from the blue mouse selection.
const CURSOR_AMBER: Color32 = Color32::from_rgb(255, 190, 90);
/// Text of the destructive Delete action.
const DANGER_RED: Color32 = Color32::from_rgb(235, 95, 95);

const PREF_KEY: &str = "theme";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Theme {
    #[default]
    Dark,
    Medium,
    Light,
}

impl Theme {
    /// The theme the toolbar button switches to.
    pub fn next(self) -> Self {
        match self {
            Theme::Dark => Theme::Medium,
            Theme::Medium => Theme::Light,
            Theme::Light => Theme::Dark,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Theme::Dark => "dark",
            Theme::Medium => "medium",
            Theme::Light => "light",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "dark" => Some(Theme::Dark),
            "medium" => Some(Theme::Medium),
            "light" => Some(Theme::Light),
            _ => None,
        }
    }
}

/// Every color the app paints itself, for one theme.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub panel: Color32,
    pub window: Color32,
    /// Behind the loupe image; the renderer clears to it.
    pub loupe_bg: Color32,
    pub grid_cell: Color32,
    pub strip_cell: Color32,
    /// Names and captions: "ISO", the camera line, empty stars.
    pub label: Color32,
    /// The data a label names: its value, the filename, the exposure.
    pub value: Color32,
    /// Hairlines between regions, such as the compare divider.
    pub divider: Color32,
    pub histogram_bg: Color32,
    pub histogram_border: Color32,
    pub star: Color32,
    pub cursor: Color32,
    pub selection: Color32,
    pub selection_bg: Color32,
    pub danger: Color32,
    /// Button backgrounds: idle, hovered, pressed, open menu.
    widget: [u8; 4],
    separator: Color32,
    /// Text field backgrounds.
    field: Color32,
    dark_base: bool,
}

/// Which way text should go on a background: light text on the darker half of
/// the grays, dark text on the lighter half.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Polarity {
    LightText,
    DarkText,
}

/// The polarity that gives more contrast on `bg`. Black and white text have
/// equal contrast on a background of relative luminance sqrt(1.05 * 0.05) - 0.05.
pub fn text_polarity(bg: Color32) -> Polarity {
    let mid = (1.05f32 * 0.05).sqrt() - 0.05;
    if relative_luminance(bg) < mid {
        Polarity::LightText
    } else {
        Polarity::DarkText
    }
}

/// WCAG relative luminance of an sRGB color.
pub fn relative_luminance(c: Color32) -> f32 {
    let lin = |v: u8| {
        let v = v as f32 / 255.0;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b())
}

/// WCAG contrast ratio, 1 to 21.
#[cfg(test)]
fn contrast_ratio(a: Color32, b: Color32) -> f32 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

pub fn palette(theme: Theme) -> Palette {
    match theme {
        Theme::Dark => Palette {
            panel: Color32::from_gray(27),
            window: Color32::from_gray(27),
            loupe_bg: Color32::from_gray(38),
            grid_cell: Color32::from_gray(30),
            strip_cell: Color32::from_gray(24),
            label: Color32::from_gray(200),
            value: Color32::from_gray(245),
            divider: Color32::from_gray(90),
            histogram_bg: Color32::from_gray(16),
            histogram_border: Color32::from_gray(48),
            star: STAR_GOLD,
            cursor: CURSOR_AMBER,
            selection: SELECTION_BLUE,
            selection_bg: SELECTION_BG,
            danger: DANGER_RED,
            widget: [60, 70, 55, 45],
            separator: Color32::from_gray(60),
            field: Color32::from_gray(10),
            dark_base: true,
        },
        Theme::Medium => medium(Color32::from_gray(96)),
        Theme::Light => Palette {
            panel: Color32::from_gray(236),
            window: Color32::from_gray(244),
            loupe_bg: Color32::from_gray(214),
            grid_cell: Color32::from_gray(222),
            strip_cell: Color32::from_gray(214),
            label: Color32::from_gray(72),
            value: Color32::from_gray(20),
            divider: Color32::from_gray(170),
            histogram_bg: Color32::from_gray(250),
            histogram_border: Color32::from_gray(190),
            star: Color32::from_rgb(160, 105, 0),
            cursor: Color32::from_rgb(170, 85, 0),
            selection: Color32::from_rgb(30, 100, 220),
            selection_bg: Color32::from_rgb(185, 210, 245),
            danger: Color32::from_rgb(190, 40, 40),
            widget: [214, 204, 190, 212],
            separator: Color32::from_gray(190),
            field: Color32::from_gray(255),
            dark_base: false,
        },
    }
}

/// The Medium theme around a neutral `panel` gray. Its text takes the polarity
/// the gray calls for, so moving the gray past the middle flips the text.
fn medium(panel: Color32) -> Palette {
    let p = panel.r();
    let shift = |d: i16| (p as i16 + d).clamp(0, 255) as u8;
    let gray = |d: i16| Color32::from_gray(shift(d));
    let base = Palette {
        panel,
        window: panel,
        loupe_bg: gray(14),
        grid_cell: gray(-8),
        strip_cell: gray(-14),
        label: Color32::PLACEHOLDER,
        value: Color32::PLACEHOLDER,
        divider: gray(44),
        histogram_bg: gray(-40),
        histogram_border: gray(-20),
        star: STAR_GOLD,
        cursor: CURSOR_AMBER,
        selection: SELECTION_BLUE,
        selection_bg: SELECTION_BG,
        danger: DANGER_RED,
        widget: [shift(-24), shift(-32), shift(-40), shift(-16)],
        separator: gray(32),
        field: gray(-32),
        dark_base: true,
    };
    match text_polarity(panel) {
        Polarity::LightText => Palette {
            label: Color32::from_gray(228),
            value: Color32::WHITE,
            danger: Color32::from_rgb(255, 140, 140),
            ..base
        },
        Polarity::DarkText => Palette {
            label: Color32::from_gray(24),
            value: Color32::BLACK,
            star: Color32::from_rgb(120, 80, 0),
            cursor: Color32::from_rgb(150, 80, 0),
            selection: Color32::from_rgb(20, 70, 170),
            selection_bg: Color32::from_rgb(150, 180, 225),
            danger: Color32::from_rgb(150, 20, 20),
            widget: [shift(24), shift(32), shift(40), shift(16)],
            divider: gray(-44),
            separator: gray(-32),
            field: gray(32),
            dark_base: false,
            ..base
        },
    }
}

fn theme_id() -> egui::Id {
    egui::Id::new("lightphotos_theme")
}

/// The theme `ctx` was last given, or Dark before any.
pub fn current(ctx: &egui::Context) -> Theme {
    ctx.data(|d| d.get_temp(theme_id())).unwrap_or_default()
}

/// The palette of the current theme.
pub fn colors(ctx: &egui::Context) -> Palette {
    palette(current(ctx))
}

/// Apply the saved theme, or Dark when none is saved.
pub fn init(ctx: &egui::Context) {
    let saved = crate::prefs::load(PREF_KEY).and_then(|s| Theme::parse(&s));
    apply(ctx, saved.unwrap_or_default());
}

/// Switch to `theme` and remember it for the next launch.
pub fn set(ctx: &egui::Context, theme: Theme) {
    apply(ctx, theme);
    if let Err(e) = crate::prefs::save(PREF_KEY, theme.as_str()) {
        eprintln!("[lightphotos] could not save the theme: {e}");
    }
}

fn apply(ctx: &egui::Context, theme: Theme) {
    let p = palette(theme);
    let (base_theme, mut v) = if p.dark_base {
        (egui::Theme::Dark, egui::Visuals::dark())
    } else {
        (egui::Theme::Light, egui::Visuals::light())
    };
    v.panel_fill = p.panel;
    v.window_fill = p.window;
    v.extreme_bg_color = p.field;
    v.weak_text_color = Some(p.label);
    v.hyperlink_color = p.selection;

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.panel;
    w.noninteractive.weak_bg_fill = p.panel;
    w.noninteractive.bg_stroke.color = p.separator;
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.value);
    for (visuals, gray) in [
        (&mut w.inactive, p.widget[0]),
        (&mut w.hovered, p.widget[1]),
        (&mut w.active, p.widget[2]),
        (&mut w.open, p.widget[3]),
    ] {
        visuals.bg_fill = Color32::from_gray(gray);
        visuals.weak_bg_fill = Color32::from_gray(gray);
        visuals.fg_stroke.color = p.value;
    }
    v.selection.bg_fill = p.selection_bg;
    v.selection.stroke.color = p.value;

    ctx.set_visuals_of(base_theme, v);
    ctx.set_theme(base_theme);
    ctx.data_mut(|d| d.insert_temp(theme_id(), theme));
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Theme; 3] = [Theme::Dark, Theme::Medium, Theme::Light];

    #[test]
    fn labels_and_values_meet_aa_contrast_on_their_panels() {
        for theme in ALL {
            let p = palette(theme);
            for bg in [p.panel, p.window] {
                let label = contrast_ratio(p.label, bg);
                let value = contrast_ratio(p.value, bg);
                assert!(label >= 4.5, "{theme:?} label {label:.2}:1");
                assert!(
                    value >= label,
                    "{theme:?} value {value:.2} < label {label:.2}"
                );
            }
        }
    }

    #[test]
    fn button_text_meets_aa_contrast_on_every_button_state() {
        for theme in ALL {
            let p = palette(theme);
            for gray in p.widget {
                let ratio = contrast_ratio(p.value, Color32::from_gray(gray));
                assert!(ratio >= 4.5, "{theme:?} button {gray}: {ratio:.2}:1");
            }
        }
    }

    #[test]
    fn stars_cursor_and_danger_stand_out_from_panels_and_cells() {
        for theme in ALL {
            let p = palette(theme);
            for bg in [p.panel, p.grid_cell, p.strip_cell] {
                for (name, c) in [("star", p.star), ("cursor", p.cursor)] {
                    let ratio = contrast_ratio(c, bg);
                    assert!(ratio >= 3.0, "{theme:?} {name} {ratio:.2}:1");
                }
            }
            let ratio = contrast_ratio(p.danger, Color32::from_gray(p.widget[0]));
            assert!(ratio >= 3.0, "{theme:?} danger {ratio:.2}:1");
        }
    }

    #[test]
    fn dark_and_light_text_go_the_expected_way() {
        let dark = palette(Theme::Dark);
        assert!(relative_luminance(dark.label) > relative_luminance(dark.panel));
        assert!(relative_luminance(dark.value) > relative_luminance(dark.label));
        let light = palette(Theme::Light);
        assert!(relative_luminance(light.label) < relative_luminance(light.panel));
        assert!(relative_luminance(light.value) < relative_luminance(light.label));
    }

    #[test]
    fn medium_text_flips_as_its_gray_crosses_the_middle() {
        assert_eq!(text_polarity(Color32::from_gray(96)), Polarity::LightText);
        assert_eq!(text_polarity(Color32::from_gray(150)), Polarity::DarkText);
        let darkish = medium(Color32::from_gray(96));
        assert!(relative_luminance(darkish.value) > relative_luminance(darkish.panel));
        let lightish = medium(Color32::from_gray(150));
        assert!(relative_luminance(lightish.value) < relative_luminance(lightish.panel));
        assert!(contrast_ratio(lightish.label, lightish.panel) >= 4.5);
    }

    #[test]
    fn apply_sets_the_text_tiers_and_keeps_font_sizes() {
        let ctx = egui::Context::default();
        let body = ctx.global_style().text_styles[&egui::TextStyle::Body].size;
        for theme in ALL {
            apply(&ctx, theme);
            let p = palette(theme);
            let style = ctx.global_style();
            assert_eq!(current(&ctx), theme);
            assert_eq!(style.visuals.text_color(), p.value);
            assert_eq!(style.visuals.weak_text_color(), p.label);
            assert_eq!(style.visuals.panel_fill, p.panel);
            assert_eq!(style.text_styles[&egui::TextStyle::Body].size, body);
        }
    }

    #[test]
    fn the_button_cycles_through_all_three_and_names_round_trip() {
        assert_eq!(Theme::Dark.next().next().next(), Theme::Dark);
        for theme in ALL {
            assert_eq!(Theme::parse(theme.as_str()), Some(theme));
        }
        assert_eq!(Theme::parse("sepia"), None);
    }
}
