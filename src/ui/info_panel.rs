// SPDX-License-Identifier: GPL-3.0-or-later

//! The left panel's Info tab: the focused photo's file, camera, exposure,
//! date, and location details.

use std::path::Path;

use super::loupe::{
    format_altitude, format_aperture, format_dimensions, format_exposure_bias, format_file_size,
    format_focal_length, format_latitude, format_longitude, format_shutter, maps_url,
};
use super::*;
use crate::image_decode::{Flash, ImageMetadata, WhiteBalance};

/// One heading and its rows of (label, value). Rows exist only for fields
/// the photo has. `link` is a map URL, shown under the location rows.
pub(super) struct InfoGroup {
    pub title: &'static str,
    pub rows: Vec<(&'static str, String)>,
    pub link: Option<String>,
}

/// The groups to show for `path`, with missing fields and empty groups left
/// out. `meta` is `None` until the background read lands, when only the name
/// is known.
pub(super) fn info_groups(path: &Path, meta: Option<&ImageMetadata>) -> Vec<InfoGroup> {
    let t = t();
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
    let m = |f: fn(&ImageMetadata) -> Option<String>| meta.and_then(f);
    let groups = [
        (
            t.info_file,
            vec![
                (t.info_name, name),
                (t.info_size, m(|m| m.file_size.map(format_file_size))),
                (
                    t.info_dimensions,
                    m(|m| m.source_size.map(|(w, h)| format_dimensions(w, h))),
                ),
                (t.info_format, m(|m| m.format.clone())),
                (t.info_modified, m(|m| m.modified.map(date_text))),
            ],
        ),
        (
            t.info_camera,
            vec![
                (t.info_make, m(|m| m.camera_make.clone())),
                (t.info_model, m(|m| m.camera_model.clone())),
                (t.info_lens, m(|m| m.lens_model.clone())),
            ],
        ),
        (
            t.info_exposure,
            vec![
                (t.info_shutter, m(|m| m.exposure_time.map(format_shutter))),
                (t.info_aperture, m(|m| m.f_number.map(format_aperture))),
                (t.info_iso, m(|m| m.iso.map(|iso| iso.to_string()))),
                (
                    t.info_focal_length,
                    m(|m| m.focal_length.map(format_focal_length)),
                ),
                (
                    t.info_exposure_comp,
                    m(|m| m.exposure_bias.map(format_exposure_bias)),
                ),
                (t.info_flash, m(|m| m.flash.map(flash_text))),
                (t.info_white_balance, m(|m| m.white_balance.map(wb_text))),
            ],
        ),
        (
            t.info_date,
            vec![(t.info_captured, m(|m| m.capture_date.map(date_text)))],
        ),
        (
            t.info_location,
            vec![
                (
                    t.info_latitude,
                    m(|m| m.gps.map(|g| format_latitude(g.lat))),
                ),
                (
                    t.info_longitude,
                    m(|m| m.gps.map(|g| format_longitude(g.lon))),
                ),
                (
                    t.info_altitude,
                    m(|m| m.gps.and_then(|g| g.alt).map(format_altitude)),
                ),
            ],
        ),
    ];
    groups
        .into_iter()
        .filter_map(|(title, rows)| {
            let rows: Vec<_> = rows
                .into_iter()
                .filter_map(|(label, value)| Some((label, value?)))
                .collect();
            let link = (title == t.info_location)
                .then(|| meta.and_then(|m| m.gps).map(|gps| maps_url(&gps)))
                .flatten();
            (!rows.is_empty()).then_some(InfoGroup { title, rows, link })
        })
        .collect()
}

fn date_text(d: crate::image_decode::CaptureDate) -> String {
    (t().capture_date)(d.year, d.month, d.day, d.hour, d.minute)
}

fn flash_text(f: Flash) -> String {
    match f {
        Flash::Fired => t().flash_fired,
        Flash::DidNotFire => t().flash_did_not_fire,
    }
    .to_string()
}

fn wb_text(w: WhiteBalance) -> String {
    match w {
        WhiteBalance::Auto => t().wb_auto,
        WhiteBalance::Manual => t().wb_manual,
    }
    .to_string()
}

pub(super) fn draw_info_panel(ui: &mut egui::Ui, app: &App) {
    ui.add_space(4.0);
    let Some(path) = app.selected_path() else {
        ui.label(egui::RichText::new(t().info_no_selection).weak());
        return;
    };
    let selected = app.selected_paths().len();
    if selected > 1 {
        ui.label(egui::RichText::new((t().n_selected)(selected)).strong());
        ui.add_space(4.0);
    }
    let meta = app.current_metadata();
    let groups = info_groups(&path, meta);

    let body = egui::TextStyle::Body.resolve(ui.style());
    let label_w = groups
        .iter()
        .flat_map(|g| g.rows.iter())
        .map(|(label, _)| {
            ui.fonts_mut(|f| {
                f.layout_no_wrap(label.to_string(), body.clone(), egui::Color32::WHITE)
                    .size()
                    .x
            })
        })
        .fold(0.0, f32::max)
        + ui.spacing().item_spacing.x * 2.0;

    for (i, group) in groups.iter().enumerate() {
        if i > 0 {
            ui.add_space(font_size::px(ui.style(), 8.0));
        }
        ui.label(egui::RichText::new(group.title).small().strong());
        ui.separator();
        for (label, value) in &group.rows {
            ui.horizontal_top(|ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(label_w, 0.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.set_width(label_w);
                        ui.label(egui::RichText::new(*label).weak());
                    },
                );
                ui.vertical(|ui| {
                    ui.add(egui::Label::new(value.as_str()).selectable(true).wrap());
                });
            });
        }
        if let Some(url) = &group.link {
            ui.hyperlink_to(t().info_open_in_maps, url);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_decode::Gps;

    fn titles(groups: &[InfoGroup]) -> Vec<&'static str> {
        groups.iter().map(|g| g.title).collect()
    }

    #[test]
    fn before_metadata_arrives_only_the_file_name_shows() {
        let groups = info_groups(Path::new("/p/IMG_1.JPG"), None);
        assert_eq!(titles(&groups), [t().info_file]);
        assert_eq!(groups[0].rows, [(t().info_name, "IMG_1.JPG".to_string())]);
    }

    #[test]
    fn missing_fields_and_empty_groups_are_hidden() {
        let meta = ImageMetadata {
            iso: Some(200),
            flash: Some(Flash::DidNotFire),
            ..Default::default()
        };
        let groups = info_groups(Path::new("a.jpg"), Some(&meta));
        assert_eq!(titles(&groups), [t().info_file, t().info_exposure]);
        assert_eq!(
            groups[1].rows,
            [
                (t().info_iso, "200".to_string()),
                (t().info_flash, t().flash_did_not_fire.to_string()),
            ]
        );
    }

    #[test]
    fn location_rows_follow_the_gps_fix() {
        let with_alt = ImageMetadata {
            gps: Some(Gps {
                lat: 37.5,
                lon: -122.25,
                alt: Some(7.0),
            }),
            ..Default::default()
        };
        let groups = info_groups(Path::new("a.jpg"), Some(&with_alt));
        let location = groups
            .iter()
            .find(|g| g.title == t().info_location)
            .unwrap();
        assert_eq!(
            location.rows,
            [
                (t().info_latitude, "37.50000\u{b0} N".to_string()),
                (t().info_longitude, "122.25000\u{b0} W".to_string()),
                (t().info_altitude, "7 m".to_string()),
            ]
        );
        assert_eq!(
            location.link.as_deref(),
            Some("https://maps.apple.com/?ll=37.500000,-122.250000")
        );

        let no_alt = ImageMetadata {
            gps: Some(Gps {
                lat: 1.0,
                lon: 2.0,
                alt: None,
            }),
            ..Default::default()
        };
        let groups = info_groups(Path::new("a.jpg"), Some(&no_alt));
        let location = groups
            .iter()
            .find(|g| g.title == t().info_location)
            .unwrap();
        assert_eq!(
            location.rows.len(),
            2,
            "no altitude row without an altitude"
        );
    }
}
