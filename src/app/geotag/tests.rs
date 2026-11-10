// SPDX-License-Identifier: MIT OR Apache-2.0

use super::*;
use crate::app::test_support::folder_app;
use crate::command::{Command, GeotagParams};
use exif::{Field, In, Tag, Value};

/// A small JPEG with these EXIF fields.
fn jpeg_with(fields: &[Field]) -> Vec<u8> {
    use crate::decode::image_encode::{Embedded, Encoding, JpegOptions, Pixels, Prepared};
    let block = crate::export::metadata::block(fields).unwrap();
    let rgba = [128u8; 8 * 8 * 4];
    Prepared::new(8, 8, Pixels::Rgba8(&rgba))
        .unwrap()
        .encode(
            Encoding::Jpeg(JpegOptions::default()),
            Embedded {
                exif: block.as_deref(),
                xmp: None,
            },
        )
        .unwrap()
}

fn field(tag: Tag, value: Value) -> Field {
    Field {
        tag,
        ifd_num: In::PRIMARY,
        value,
    }
}

fn taken_at(local: &str) -> Field {
    field(
        Tag::DateTimeOriginal,
        Value::Ascii(vec![local.as_bytes().to_vec()]),
    )
}

fn degrees(d: u32) -> Value {
    Value::Rational(vec![
        exif::Rational { num: d, denom: 1 },
        exif::Rational { num: 0, denom: 1 },
        exif::Rational { num: 0, denom: 1 },
    ])
}

/// A track over 2026-11-08 09:00 to 09:10 UTC, with no LightRemote clock
/// data.
const GPX: &str = r#"<?xml version="1.0"?>
<gpx version="1.1" creator="test" xmlns="http://www.topografix.com/GPX/1/1"><trk><trkseg>
<trkpt lat="35.0" lon="139.0"><ele>10</ele><time>2026-11-08T09:00:00Z</time></trkpt>
<trkpt lat="35.1" lon="139.2"><ele>20</ele><time>2026-11-08T09:10:00Z</time></trkpt>
</trkseg></trk></gpx>"#;

fn run_to_end(app: &mut App) {
    let start = std::time::Instant::now();
    while app.pump_geotag() {
        assert!(start.elapsed().as_secs() < 30, "the run finishes");
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

#[test]
fn a_run_locates_each_photo_in_its_sidecar_and_leaves_the_files_alone() {
    let (mut app, dir, paths) = folder_app("geotag-run", 4);
    // On the camera's clock at +09:00: on the track, on the track with its
    // own GPS, an hour past it; and an empty file.
    std::fs::write(&paths[0], jpeg_with(&[taken_at("2026:11:08 18:05:00")])).unwrap();
    let with_gps = [
        taken_at("2026:11:08 18:05:00"),
        field(Tag::GPSLatitudeRef, Value::Ascii(vec![b"N".to_vec()])),
        field(Tag::GPSLatitude, degrees(1)),
        field(Tag::GPSLongitudeRef, Value::Ascii(vec![b"E".to_vec()])),
        field(Tag::GPSLongitude, degrees(2)),
    ];
    std::fs::write(&paths[1], jpeg_with(&with_gps)).unwrap();
    std::fs::write(&paths[2], jpeg_with(&[taken_at("2026:11:08 19:05:00")])).unwrap();
    let before: Vec<Vec<u8>> = paths.iter().map(|p| std::fs::read(p).unwrap()).collect();
    let gpx = dir.join("day.gpx");
    std::fs::write(&gpx, GPX).unwrap();

    let asks = app.run(Command::GeotagFromGpx(GeotagParams {
        gpx: gpx.clone(),
        camera_minus_utc_s: None,
    }));
    assert!(
        matches!(&asks, Err(CommandError::Failed { reason }) if reason.contains("camera_minus_utc_s")),
        "{asks:?}"
    );
    assert!(!app.geotag_running());

    let out = app
        .run(Command::GeotagFromGpx(GeotagParams {
            gpx,
            camera_minus_utc_s: Some(9 * 3600),
        }))
        .unwrap();
    assert_eq!((out.photos, out.pending), (4, true));
    assert!(app.batch_running(), "drive's idle waits for the run");
    run_to_end(&mut app);

    let tag = app.stored_location(&paths[0]).cloned().unwrap();
    assert!((tag.lat - 35.05).abs() < 1e-9, "halfway along: {tag:?}");
    assert!((tag.lon - 139.1).abs() < 1e-9, "{tag:?}");
    assert_eq!(tag.alt, Some(15.0));
    assert_eq!(
        tag.source,
        GeoSource::Gpx {
            file_name: "day.gpx".into(),
            camera_minus_utc_s: 9 * 3600
        }
    );
    for p in &paths[1..] {
        assert_eq!(app.stored_location(p), None, "{}", p.display());
    }
    assert_eq!(
        app.status_text(),
        Some((crate::i18n::t().geotag_done)(1, 1, 1, 0, 1).as_str())
    );
    for (p, bytes) in paths.iter().zip(&before) {
        assert_eq!(
            &std::fs::read(p).unwrap(),
            bytes,
            "{} is untouched",
            p.display()
        );
    }

    assert_eq!(app.run(Command::ClearGeotags).unwrap().photos, 1);
    assert_eq!(app.stored_location(&paths[0]), None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_bad_gpx_file_is_an_error_and_starts_nothing() {
    let (mut app, dir, _) = folder_app("geotag-bad", 1);
    for (name, body) in [("bad.gpx", "<gpx><trk>"), ("empty.gpx", "")] {
        let gpx = dir.join(name);
        std::fs::write(&gpx, body).unwrap();
        let err = app
            .run(Command::GeotagFromGpx(GeotagParams {
                gpx,
                camera_minus_utc_s: Some(0),
            }))
            .unwrap_err();
        assert!(
            matches!(err, CommandError::Failed { .. }),
            "{name}: {err:?}"
        );
    }
    let missing = app.run(Command::GeotagFromGpx(GeotagParams {
        gpx: dir.join("nope.gpx"),
        camera_minus_utc_s: Some(0),
    }));
    assert!(missing.is_err());
    assert!(!app.geotag_running());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_offset_prompt_holds_the_keys_and_runs_only_on_a_real_offset() {
    let (mut app, dir, paths) = folder_app("geotag-prompt", 1);
    std::fs::write(&paths[0], jpeg_with(&[taken_at("2026:11:08 18:05:00")])).unwrap();
    let gpx = dir.join("day.gpx");
    std::fs::write(&gpx, GPX).unwrap();
    app.geotag_or_ask(gpx, None);
    assert!(app.geotag_prompt().is_some(), "no clock data, so it asks");
    assert_eq!(
        app.input_blocker(),
        Some(crate::command::Blocker::GeotagOffset)
    );

    app.set_geotag_offset_text("nine".into());
    assert_eq!(app.geotag_prompt(), Some(("nine", false)));
    app.commit_geotag_prompt();
    assert!(app.geotag_prompt().is_some(), "a bad offset keeps it open");

    app.set_geotag_offset_text("+09:00".into());
    app.commit_geotag_prompt();
    assert!(app.geotag_prompt().is_none());
    run_to_end(&mut app);
    assert!(app.stored_location(&paths[0]).is_some());
    let _ = std::fs::remove_dir_all(&dir);
}
