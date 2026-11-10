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
            camera_minus_utc_s: 9 * 3600,
            clock: Some(crate::geotag::ClockSource::Setting),
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
    app.geotag_or_ask(GpxSource::File(gpx), None);
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

mod icloud_days {
    use super::super::icloud::{self, Day, DayState, Folder};
    use super::super::source::{self, GpxSource, Note};
    use super::*;
    use chrono::NaiveDate;
    use std::time::Duration;

    fn date(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, d).unwrap()
    }

    /// One LightRemote day at +09:00: fixes from `from` to `to` (UTC,
    /// `HH:MM` on `day`), a minute apart, along a line of latitude.
    fn lightremote_day(day: u32, from: (u32, u32), to: (u32, u32), lat: f64) -> String {
        let at = |h: u32, m: u32| format!("2026-11-{day:02}T{h:02}:{m:02}:00Z");
        let mut x = format!(
            "<?xml version=\"1.0\"?>\n<gpx version=\"1.1\" creator=\"LightRemote\" \
             xmlns=\"http://www.topografix.com/GPX/1/1\" xmlns:lr=\"{}\"><metadata><extensions>\
             <lr:utcOffset from=\"{}\" minutes=\"540\" zone=\"Asia/Tokyo\"/></extensions></metadata>\
             <trk><trkseg>\n",
            crate::geotag::LR_NS,
            at(from.0, from.1)
        );
        let (mut h, mut m) = from;
        while (h, m) <= to {
            x += &format!(
                "<trkpt lat=\"{lat}\" lon=\"139.0\"><time>{}</time></trkpt>\n",
                at(h, m)
            );
            (h, m) = if m == 59 { (h + 1, 0) } else { (h, m + 1) };
        }
        x + "</trkseg></trk></gpx>\n"
    }

    fn put(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    /// 7th and 8th here, 9th a placeholder, 10th missing, 11th here.
    fn fake_icloud(tag: &str) -> PathBuf {
        let dir = crate::app::test_support::temp_folder(tag);
        put(
            &dir,
            "2026-11-07.gpx",
            &lightremote_day(7, (1, 0), (1, 5), 1.0),
        );
        put(
            &dir,
            "2026-11-08.gpx",
            &lightremote_day(8, (1, 0), (1, 5), 2.0),
        );
        put(&dir, ".2026-11-09.gpx.icloud", "placeholder");
        put(
            &dir,
            "2026-11-11.gpx",
            &lightremote_day(11, (1, 0), (1, 5), 4.0),
        );
        put(&dir, "notes.txt", "not a track");
        put(&dir, "2026-11-10 copy.gpx", "not a day file");
        dir
    }

    #[test]
    fn a_range_reads_its_days_and_one_either_side() {
        let dir = fake_icloud("icloud-scan");
        let days = icloud::scan(&dir, date(8), date(10));
        assert_eq!(
            days,
            [
                (7, DayState::OnMac),
                (8, DayState::OnMac),
                (9, DayState::InICloud),
                (10, DayState::Missing),
                (11, DayState::OnMac),
            ]
            .map(|(d, state)| Day {
                date: date(d),
                state
            })
        );
        assert!(
            icloud::scan(&dir, date(10), date(8)).is_empty(),
            "backwards"
        );
        let far = date(8) + chrono::Days::new(366);
        assert!(icloud::scan(&dir, date(8), far).is_empty(), "over a year");
        assert_eq!(
            icloud::scan(&dir, date(8), far - chrono::Days::new(1)).len(),
            368
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolving_reads_the_days_here_and_reports_the_picked_ones_that_are_not() {
        let dir = fake_icloud("icloud-resolve");
        let before = icloud::scan(&dir, date(8), date(10));
        let resolved = GpxSource::ICloud {
            first: date(8),
            last: date(10),
        }
        .resolve(Some(&dir), &before);
        let names: Vec<&str> = resolved.files.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            ["2026-11-07.gpx", "2026-11-08.gpx", "2026-11-11.gpx"]
        );
        assert_eq!(
            resolved.notes,
            [
                Note::Missing(vec![date(10)]),
                Note::NotDownloaded(vec![date(9)])
            ]
        );

        // The 9th arrives, and a missing adjacent day goes unmentioned.
        std::fs::remove_file(dir.join(".2026-11-09.gpx.icloud")).unwrap();
        put(
            &dir,
            "2026-11-09.gpx",
            &lightremote_day(9, (1, 0), (1, 5), 3.0),
        );
        std::fs::remove_file(dir.join("2026-11-11.gpx")).unwrap();
        let resolved = GpxSource::ICloud {
            first: date(8),
            last: date(10),
        }
        .resolve(Some(&dir), &before);
        assert_eq!(
            resolved.notes,
            [
                Note::Missing(vec![date(10)]),
                Note::Downloaded(vec![date(9)])
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_malformed_or_oversized_day_is_skipped_with_its_reason() {
        let dir = crate::app::test_support::temp_folder("icloud-bad");
        put(
            &dir,
            "2026-11-08.gpx",
            &lightremote_day(8, (1, 0), (1, 5), 2.0),
        );
        put(&dir, "2026-11-09.gpx", "<gpx><trk>");
        let huge = std::fs::File::create(dir.join("2026-11-10.gpx")).unwrap();
        huge.set_len(crate::geotag::MAX_GPX_BYTES as u64 + 1)
            .unwrap();
        drop(huge);
        let source = GpxSource::ICloud {
            first: date(8),
            last: date(10),
        };
        let before = icloud::scan(&dir, date(8), date(10));
        let loaded = source::load(source.resolve(Some(&dir), &before)).unwrap();
        assert_eq!(loaded.track.file_names(), ["2026-11-08.gpx"]);
        let skipped: Vec<(&str, &str)> = loaded
            .notes
            .iter()
            .filter_map(|n| match n {
                Note::Skipped { file, reason } => Some((file.as_str(), reason.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(skipped.len(), 2, "{:?}", loaded.notes);
        assert!(
            skipped[0].0 == "2026-11-09.gpx" && skipped[0].1.contains("XML"),
            "{skipped:?}"
        );
        assert!(
            skipped[1].0 == "2026-11-10.gpx" && skipped[1].1.contains("limit"),
            "{skipped:?}"
        );

        std::fs::remove_file(dir.join("2026-11-08.gpx")).unwrap();
        let none = source::load(source.resolve(Some(&dir), &before)).unwrap_err();
        assert!(
            none.contains("2026-11-09.gpx") && none.contains("2026-11-10.gpx"),
            "{none}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_download_wait_ends_when_the_file_arrives_or_the_time_is_up() {
        let dir = fake_icloud("icloud-wait");
        let folder = Folder {
            dir: dir.clone(),
            timeout: Duration::from_millis(400),
        };
        let gave_up = icloud::download(&folder, date(8), date(10));
        assert_eq!(gave_up[2].state, DayState::InICloud, "never arrived");

        let arrive = {
            let dir = dir.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                put(
                    &dir,
                    "2026-11-09.gpx",
                    &lightremote_day(9, (1, 0), (1, 5), 3.0),
                );
                std::fs::remove_file(dir.join(".2026-11-09.gpx.icloud")).unwrap();
            })
        };
        let folder = Folder {
            dir: dir.clone(),
            timeout: Duration::from_secs(20),
        };
        let start = std::time::Instant::now();
        let days = icloud::download(&folder, date(8), date(10));
        arrive.join().unwrap();
        assert_eq!(days[2].state, DayState::OnMac);
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_run_from_icloud_matches_across_days_and_names_the_day_that_never_came() {
        let (mut app, dir, paths) = folder_app("geotag-icloud", 4);
        let icloud_dir = crate::app::test_support::temp_folder("geotag-icloud-gpx");
        // The 8th ends at 14:55 UTC (23:55 at +09:00), the 9th starts at
        // 15:05 UTC the same day, which is its own local date.
        put(
            &icloud_dir,
            "2026-11-08.gpx",
            &lightremote_day(8, (14, 0), (14, 55), 35.0),
        );
        put(
            &icloud_dir,
            "2026-11-09.gpx",
            &lightremote_day(8, (15, 5), (15, 30), 36.0),
        );
        put(&icloud_dir, ".2026-11-10.gpx.icloud", "placeholder");
        app.geotag.icloud = Some(Folder {
            dir: icloud_dir.clone(),
            timeout: Duration::from_millis(300),
        });
        let taken = [
            "2026:11:08 23:30:00",
            "2026:11:09 00:00:10",
            "2026:11:09 00:20:00",
            "2026:11:10 12:00:00",
        ];
        for (p, t) in paths.iter().zip(taken) {
            std::fs::write(p, jpeg_with(&[taken_at(t)])).unwrap();
        }
        let before: Vec<Vec<u8>> = paths.iter().map(|p| std::fs::read(p).unwrap()).collect();

        let out = app
            .run(Command::GeotagFromICloud(
                crate::command::ICloudGeotagParams {
                    first: date(8),
                    last: date(10),
                    camera_minus_utc_s: Some(9 * 3600),
                },
            ))
            .unwrap();
        assert_eq!((out.photos, out.pending), (4, true));
        assert!(app.batch_running(), "drive's idle waits for the download");
        // The run keeps the photos it began with while it downloads.
        let elsewhere = crate::app::test_support::temp_folder("geotag-icloud-elsewhere");
        let open = app
            .playlist
            .replace(crate::navigation::Playlist::from_dir(&elsewhere));
        run_to_end(&mut app);
        app.playlist = open;

        let file = |p: &PathBuf| {
            app.stored_location(p).map(|t| match &t.source {
                GeoSource::Gpx { file_name, .. } => file_name.clone(),
            })
        };
        assert_eq!(file(&paths[0]).as_deref(), Some("2026-11-08.gpx"));
        assert_eq!(
            file(&paths[1]).as_deref(),
            Some("2026-11-09.gpx"),
            "across midnight"
        );
        assert_eq!(file(&paths[2]).as_deref(), Some("2026-11-09.gpx"));
        assert_eq!(file(&paths[3]), None, "its day never downloaded");
        let lat = app.stored_location(&paths[1]).unwrap().lat;
        assert!(lat > 35.0 && lat < 36.0, "between the days' fixes: {lat}");
        let t = crate::i18n::t();
        let status = app.status_text().unwrap();
        assert!(
            status.starts_with(&(t.geotag_done)(3, 0, 1, 0, 0)),
            "{status}"
        );
        assert!(
            status.contains(&(t.geotag_days_not_downloaded)("2026-11-10")),
            "{status}"
        );
        for (p, bytes) in paths.iter().zip(&before) {
            assert_eq!(
                &std::fs::read(p).unwrap(),
                bytes,
                "{} is untouched",
                p.display()
            );
        }

        let empty = app.run(Command::GeotagFromICloud(
            crate::command::ICloudGeotagParams {
                first: date(20),
                last: date(21),
                camera_minus_utc_s: None,
            },
        ));
        assert!(
            matches!(&empty, Err(CommandError::Failed { reason }) if reason.contains("2026-11-20")),
            "{empty:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&icloud_dir);
        let _ = std::fs::remove_dir_all(&elsewhere);
    }
}
