// SPDX-License-Identifier: MIT OR Apache-2.0

use super::*;
use crate::app::test_support::folder_app;
use crate::command::{Blocker, Command, GeotagParams};
use crate::geotag::{ClockSource, LR_NS};
use exif::{Field, In, Tag, Value};
use source::{GpxFile, Note, MAX_DEPTH, MAX_ENTRIES};

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
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn model(name: &str) -> Field {
    field(Tag::Model, Value::Ascii(vec![name.as_bytes().to_vec()]))
}

fn import(paths: &[&Path], camera_minus_utc_s: Option<i64>) -> Command {
    Command::ImportGpsTracks(GeotagParams {
        paths: paths.iter().map(|p| p.to_path_buf()).collect(),
        camera_minus_utc_s,
    })
}

fn put(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

/// A LightRemote day: fixes a minute apart from `from` to `to`, UTC times
/// on 2026-11-`day`, along latitude `lat`, with a clock measurement when
/// `measured` gives camera-minus-UTC seconds.
fn day_gpx(day: u32, from: (u32, u32), to: (u32, u32), lat: f64, measured: Option<i64>) -> String {
    let at = |h: u32, m: u32| format!("2026-11-{day:02}T{h:02}:{m:02}:00Z");
    let clock = measured.map_or(String::new(), |s| {
        format!(
            "<lr:cameraClock measuredAt=\"{}\" cameraMinusUtcSeconds=\"{s}\"/>",
            at(from.0, from.1)
        )
    });
    let mut x = format!(
        "<?xml version=\"1.0\"?>\n<gpx version=\"1.1\" creator=\"LightRemote\" \
         xmlns=\"http://www.topografix.com/GPX/1/1\" xmlns:lr=\"{LR_NS}\"><metadata><extensions>\
         <lr:utcOffset from=\"{}\" minutes=\"540\" zone=\"Asia/Tokyo\"/>{clock}</extensions></metadata>\
         <trk><trkseg>\n",
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

fn location_file(app: &App, p: &Path) -> Option<String> {
    app.stored_location(p).map(|t| match &t.source {
        GeoSource::Gpx { file_name, .. } => file_name.clone(),
    })
}

fn clock_of(app: &App, p: &Path) -> Option<(i64, Option<ClockSource>)> {
    app.stored_location(p).map(|t| match &t.source {
        GeoSource::Gpx {
            camera_minus_utc_s,
            clock,
            ..
        } => (*camera_minus_utc_s, *clock),
    })
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

    // No measurement and no setting: the photo has no clock to match by.
    app.run(import(&[&gpx], None)).unwrap();
    run_to_end(&mut app);
    assert_eq!(app.stored_location(&paths[0]), None);
    assert_eq!(
        app.status_text(),
        Some((crate::i18n::t().geotag_done)(0, 1, 0, 0, 1, 2).as_str())
    );

    let out = app.run(import(&[&gpx], Some(9 * 3600))).unwrap();
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
            clock: Some(ClockSource::Setting),
        }
    );
    for p in &paths[1..] {
        assert_eq!(app.stored_location(p), None, "{}", p.display());
    }
    assert_eq!(
        app.status_text(),
        Some((crate::i18n::t().geotag_done)(1, 1, 1, 0, 1, 0).as_str())
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
fn nothing_to_read_is_an_error_and_a_bad_file_fails_the_run_not_the_app() {
    let (mut app, dir, _) = folder_app("geotag-bad", 1);
    let missing = app.run(import(&[&dir.join("nope.gpx")], Some(0)));
    assert!(
        matches!(&missing, Err(CommandError::Failed { reason }) if reason.contains("nope.gpx")),
        "{missing:?}"
    );
    let notes = dir.join("notes.txt");
    put(&notes, "not a track");
    assert!(app.run(import(&[&notes], Some(0))).is_err());
    let empty = dir.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    assert!(app.run(import(&[&empty], Some(0))).is_err());
    assert!(!app.geotag_running());

    for (name, body) in [("bad.gpx", "<gpx><trk>"), ("empty.gpx", "")] {
        let gpx = dir.join(name);
        put(&gpx, body);
        app.run(import(&[&gpx], Some(0))).unwrap();
        run_to_end(&mut app);
        let status = app.status_text().unwrap_or_default();
        assert!(status.contains(name), "{name}: {status}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_folder_is_read_to_a_depth_limit_skipping_hidden_folders_and_other_files() {
    let dir = crate::app::test_support::temp_folder("geotag-walk");
    let body = day_gpx(8, (9, 0), (9, 1), 1.0, None);
    put(&dir.join("a.gpx"), &body);
    put(&dir.join("B.GPX"), &body);
    put(&dir.join("2026/11/c.gpx"), &body);
    put(&dir.join("notes.txt"), "x");
    put(&dir.join("photo.jpg"), "x");
    put(&dir.join(".lightphotos/hidden.gpx"), &body);
    put(&dir.join(".hidden.gpx"), &body);
    put(&dir.join("cloud/.d.gpx.icloud"), "placeholder");
    // A placeholder whose file is already here is the file.
    put(&dir.join("cloud/e.gpx"), &body);
    put(&dir.join("cloud/.e.gpx.icloud"), "placeholder");
    let mut deep = dir.clone();
    for level in 0..=MAX_DEPTH {
        deep = deep.join(format!("l{level}"));
    }
    put(&deep.join("too-deep.gpx"), &body);
    put(&deep.parent().unwrap().join("deep-enough.gpx"), &body);

    let found = source::find(std::slice::from_ref(&dir));
    let names: Vec<(String, bool)> = found
        .files
        .iter()
        .map(|f| (source::name_of(&f.path), f.in_icloud))
        .collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(
        sorted,
        [
            ("B.GPX".to_string(), false),
            ("a.gpx".into(), false),
            ("c.gpx".into(), false),
            ("d.gpx".into(), true),
            ("deep-enough.gpx".into(), false),
            ("e.gpx".into(), false),
        ]
    );
    assert!(
        found
            .files
            .iter()
            .any(|f| f.path == dir.join("cloud/d.gpx")),
        "a placeholder stands for the file under its own name"
    );
    assert_eq!(found.notes, [source::Note::TooDeep]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn files_and_folders_chosen_together_join_once_and_a_malformed_file_is_skipped() {
    let dir = crate::app::test_support::temp_folder("geotag-multi");
    let one = dir.join("picked/one.gpx");
    put(&one, &day_gpx(8, (9, 0), (9, 5), 1.0, None));
    let two = dir.join("folder/two.gpx");
    put(&two, &day_gpx(9, (9, 0), (9, 5), 2.0, None));
    let bad = dir.join("folder/bad.gpx");
    put(&bad, "<gpx><trk>");
    let chosen = [
        one.clone(),
        dir.join("folder"),
        two.clone(),
        dir.join("gone"),
    ];
    let found = source::find(&chosen);
    assert_eq!(found.files.len(), 3, "{:?}", found.files);
    assert!(
        matches!(&found.notes[..], [source::Note::Skipped { file, .. }] if file == "gone"),
        "{:?}",
        found.notes
    );
    let loaded = source::load(&found.files, Vec::new()).unwrap();
    assert_eq!(loaded.track.file_names(), ["two.gpx", "one.gpx"]);
    assert!(
        matches!(&loaded.notes[..], [source::Note::Skipped { file, reason }] if file == "bad.gpx" && !reason.is_empty()),
        "{:?}",
        loaded.notes
    );
    let none = source::load(
        &[GpxFile {
            path: bad,
            in_icloud: false,
        }],
        Vec::new(),
    );
    assert!(
        matches!(&none, Err(notes) if matches!(&notes[..], [Note::Skipped { file, .. }] if file == "bad.gpx")),
        "{none:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_download_wait_ends_when_the_file_arrives_or_the_time_is_up() {
    let dir = crate::app::test_support::temp_folder("geotag-wait");
    put(&dir.join(".late.gpx.icloud"), "placeholder");
    put(&dir.join(".soon.gpx.icloud"), "placeholder");
    let found = source::find(std::slice::from_ref(&dir));
    assert!(found.files.iter().all(|f| f.in_icloud));
    let arrive = {
        let dir = dir.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            put(
                &dir.join("soon.gpx"),
                &day_gpx(8, (9, 0), (9, 1), 1.0, None),
            );
            std::fs::remove_file(dir.join(".soon.gpx.icloud")).unwrap();
        })
    };
    let start = std::time::Instant::now();
    let (files, notes) = source::download(found.files, Duration::from_millis(800), |_| Ok(()));
    arrive.join().unwrap();
    assert!(start.elapsed() < Duration::from_secs(5));
    assert_eq!(
        files.iter().map(|f| f.in_icloud).collect::<Vec<_>>(),
        [true, false],
        "late never came; soon did"
    );
    assert_eq!(notes, [Note::Downloaded(1), Note::NotDownloaded(1)]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_download_that_never_started_is_not_waited_for() {
    let dir = crate::app::test_support::temp_folder("geotag-nostart");
    put(&dir.join(".a.gpx.icloud"), "placeholder");
    put(&dir.join(".b.gpx.icloud"), "placeholder");
    let found = source::find(std::slice::from_ref(&dir));
    let start = std::time::Instant::now();
    let (files, notes) = source::download(found.files, Duration::from_secs(20), |_| {
        Err("not in iCloud".into())
    });
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );
    assert!(files.iter().all(|f| f.in_icloud));
    assert_eq!(notes, [Note::NotDownloaded(2)]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_huge_folder_is_searched_only_so_far() {
    let dir = crate::app::test_support::temp_folder("geotag-huge");
    let body = day_gpx(8, (9, 0), (9, 1), 1.0, None);
    put(&dir.join("a/first.gpx"), &body);
    let many = dir.join("b");
    std::fs::create_dir_all(&many).unwrap();
    for i in 0..MAX_ENTRIES {
        std::fs::File::create(many.join(format!("{i}.jpg"))).unwrap();
    }
    let found = source::find(std::slice::from_ref(&dir));
    assert_eq!(found.notes, [Note::TooBig]);
    assert_eq!(found.files.len(), 1, "what it saw before the cap is kept");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn camera_settings_are_kept_by_model_and_serial_and_survive_hostile_prefs() {
    let a7 = CameraKey {
        model: "ILCE-7M2".into(),
        serial: None,
    };
    let numbered = CameraKey {
        model: "ILCE-7M2".into(),
        serial: Some("1234567".into()),
    };
    let tokyo = CameraClock {
        zone: CameraZone::new(540, false).unwrap(),
        paired: true,
    };
    let summer = CameraClock {
        zone: CameraZone::new(-480, true).unwrap(),
        paired: false,
    };
    let mut zones = CameraClocks::default();
    zones.set(&a7, tokyo);
    zones.set(&numbered, summer);
    let back = CameraClocks::parse(&zones.to_json());
    assert_eq!(back, zones);
    assert_eq!(back.get(&a7), Some(tokyo));
    assert_eq!(back.get(&numbered), Some(summer));
    assert_eq!(numbered.pref_key(), "ILCE-7M2#1234567");

    let hostile = CameraClocks::parse(
        r#"{"A":{"utc_offset_min":17,"dst":false},"B":"x","C":{"utc_offset_min":540,"dst":true}}"#,
    );
    let c = CameraKey {
        model: "C".into(),
        serial: None,
    };
    assert_eq!(
        hostile.get(&c),
        Some(CameraClock {
            zone: CameraZone::new(540, true).unwrap(),
            paired: false
        })
    );
    assert_eq!(
        hostile.to_json(),
        r#"{"C":{"utc_offset_min":540,"dst":true,"paired":false}}"#
    );
    assert_eq!(CameraClocks::parse("[not json"), CameraClocks::default());

    let meta = crate::decode::image_decode::ImageMetadata {
        camera_model: Some(" ILCE-7M2\0 ".into()),
        camera_serial: Some("  ".into()),
        ..Default::default()
    };
    assert_eq!(CameraKey::of(&meta), a7);
    assert_eq!(
        CameraKey::of(&Default::default()).label(),
        None,
        "no model, an unknown camera"
    );
}

#[test]
fn a_body_whose_jpegs_lack_its_serial_is_one_camera() {
    let key = |model: &str, serial: Option<&str>| CameraKey {
        model: model.into(),
        serial: serial.map(Into::into),
    };
    let mut cameras = [
        key("Z 8", Some("3001")),
        key("Z 8", None),
        key("X100V", None),
        key("A1", Some("1")),
        key("A1", Some("2")),
        key("A1", None),
    ];
    cameras::fold_serials(cameras.iter_mut());
    assert_eq!(
        cameras,
        [
            key("Z 8", Some("3001")),
            key("Z 8", Some("3001")),
            key("X100V", None),
            key("A1", Some("1")),
            key("A1", Some("2")),
            key("A1", None),
        ],
        "one numbered body takes the rest; two leave them unknown"
    );
}

#[test]
fn the_dialog_asks_each_camera_once_and_remembers_its_setting() {
    let (mut app, dir, paths) = folder_app("geotag-dialog", 3);
    let serial = field(Tag::BodySerialNumber, Value::Ascii(vec![b"0042".to_vec()]));
    // Two photos from one A7II, on the track at +09:00 with DST on (+10:00),
    // and one from another camera.
    std::fs::write(
        &paths[0],
        jpeg_with(&[taken_at("2026:11:08 19:05:00"), model("ILCE-7M2")]),
    )
    .unwrap();
    std::fs::write(
        &paths[1],
        jpeg_with(&[taken_at("2026:11:08 19:06:00"), model("ILCE-7M2")]),
    )
    .unwrap();
    std::fs::write(
        &paths[2],
        jpeg_with(&[taken_at("2026:11:08 18:05:00"), model("X100V"), serial]),
    )
    .unwrap();
    let gpx = dir.join("tracks/day.gpx");
    put(&gpx, GPX);

    app.open_import_dialog(vec![dir.join("tracks")]);
    assert_eq!(app.input_blocker(), Some(Blocker::GeotagImport));
    run_to_end(&mut app);
    let view = app.import_dialog().unwrap();
    assert_eq!(view.files, Some((1, 0)));
    let cameras: Vec<CameraKey> = view.cameras.unwrap().iter().map(|r| r.0.clone()).collect();
    assert_eq!(
        cameras.iter().map(|c| c.pref_key()).collect::<Vec<_>>(),
        ["ILCE-7M2", "X100V#0042"]
    );
    let clock = |minutes, dst| CameraClock {
        zone: CameraZone::new(minutes, dst).unwrap(),
        paired: false,
    };
    assert!(
        app.import_dialog()
            .unwrap()
            .cameras
            .unwrap()
            .iter()
            .all(|(_, c)| !c.paired),
        "with two cameras, neither is taken as LightRemote's"
    );
    app.set_camera_clock(0, clock(540, true));
    app.set_camera_clock(1, clock(540, false));
    app.commit_import_dialog();
    assert!(app.import_dialog().is_none());
    run_to_end(&mut app);
    assert_eq!(
        clock_of(&app, &paths[0]),
        Some((10 * 3600, Some(ClockSource::Setting)))
    );
    assert_eq!(
        clock_of(&app, &paths[1]),
        Some((10 * 3600, Some(ClockSource::Setting)))
    );
    assert_eq!(
        clock_of(&app, &paths[2]),
        Some((9 * 3600, Some(ClockSource::Setting)))
    );

    app.open_import_dialog(vec![gpx]);
    run_to_end(&mut app);
    let rows = app.import_dialog().unwrap().cameras.unwrap().to_vec();
    assert_eq!(
        rows,
        [
            (cameras[0].clone(), clock(540, true)),
            (cameras[1].clone(), clock(540, false)),
        ],
        "each camera's setting comes back"
    );
    app.cancel_import_dialog();
    assert_eq!(app.input_blocker(), None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_measurement_far_from_the_setting_is_used_and_flagged() {
    let (mut app, dir, paths) = folder_app("geotag-flag", 2);
    // The camera ran at +10:00 (DST on) while the user says +09:00.
    std::fs::write(
        &paths[0],
        jpeg_with(&[taken_at("2026:11:08 19:03:00"), model("ILCE-7M2")]),
    )
    .unwrap();
    // Four days later, past any file boundary.
    std::fs::write(
        &paths[1],
        jpeg_with(&[taken_at("2026:11:12 19:03:00"), model("ILCE-7M2")]),
    )
    .unwrap();
    put(
        &dir.join("t/2026-11-08.gpx"),
        &day_gpx(8, (9, 0), (9, 5), 1.0, Some(10 * 3600)),
    );
    put(
        &dir.join("t/2026-11-12.gpx"),
        &day_gpx(12, (9, 0), (9, 5), 2.0, None),
    );
    app.open_import_dialog(vec![dir.join("t")]);
    run_to_end(&mut app);
    let rows = app.import_dialog().unwrap().cameras.unwrap();
    assert!(
        rows[0].1.paired,
        "the only camera is taken as LightRemote's"
    );
    let paired = |minutes, dst| CameraClock {
        zone: CameraZone::new(minutes, dst).unwrap(),
        paired: true,
    };
    app.set_camera_clock(0, paired(540, false));
    app.commit_import_dialog();
    run_to_end(&mut app);
    for p in &paths {
        assert_eq!(
            clock_of(&app, p),
            Some((10 * 3600, Some(ClockSource::Measured)))
        );
    }
    assert_eq!(
        location_file(&app, &paths[1]).as_deref(),
        Some("2026-11-12.gpx")
    );
    let flag = (crate::i18n::t().geotag_measured_off)("ILCE-7M2", 1, 0);
    let status = app.status_text().unwrap_or_default().to_string();
    assert!(status.contains(&flag), "{status}");

    // Within half an hour of the setting, nothing is flagged.
    app.run(Command::ClearGeotags).unwrap();
    app.open_import_dialog(vec![dir.join("t")]);
    run_to_end(&mut app);
    app.set_camera_clock(0, paired(540, true));
    app.commit_import_dialog();
    run_to_end(&mut app);
    let status = app.status_text().unwrap_or_default().to_string();
    assert!(!status.contains("LightRemote"), "{status}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_folder_with_an_icloud_placeholder_names_the_file_that_never_came() {
    let (mut app, dir, paths) = folder_app("geotag-icloud", 2);
    app.geotag.download_timeout = Duration::from_millis(300);
    std::fs::write(&paths[0], jpeg_with(&[taken_at("2026:11:08 23:30:00")])).unwrap();
    std::fs::write(&paths[1], jpeg_with(&[taken_at("2026:11:10 12:00:00")])).unwrap();
    let tracks = dir.join("GPX");
    put(
        &tracks.join("2026/2026-11-08.gpx"),
        &day_gpx(8, (14, 0), (14, 55), 35.0, None),
    );
    put(&tracks.join("2026/.2026-11-10.gpx.icloud"), "placeholder");
    app.run(import(&[&tracks], Some(9 * 3600))).unwrap();
    run_to_end(&mut app);
    assert_eq!(
        location_file(&app, &paths[0]).as_deref(),
        Some("2026-11-08.gpx")
    );
    assert_eq!(app.stored_location(&paths[1]), None);
    let status = app.status_text().unwrap_or_default().to_string();
    let late = (crate::i18n::t().geotag_files_not_downloaded)(1);
    assert!(status.contains(&late), "{status}");

    // With nothing downloaded, the failure still says why.
    std::fs::remove_file(tracks.join("2026/2026-11-08.gpx")).unwrap();
    app.run(import(&[&tracks], Some(9 * 3600))).unwrap();
    run_to_end(&mut app);
    let status = app.status_text().unwrap_or_default().to_string();
    assert!(status.contains(&late), "{status}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn only_the_camera_paired_with_lightremote_takes_its_measurements() {
    let (mut app, dir, paths) = folder_app("geotag-paired", 2);
    // LightRemote measured the A7II at +10:00. The X100V is set to +09:00.
    std::fs::write(
        &paths[0],
        jpeg_with(&[taken_at("2026:11:08 19:03:00"), model("ILCE-7M2")]),
    )
    .unwrap();
    std::fs::write(
        &paths[1],
        jpeg_with(&[taken_at("2026:11:08 18:03:00"), model("X100V")]),
    )
    .unwrap();
    put(
        &dir.join("t/day.gpx"),
        &day_gpx(8, (9, 0), (9, 5), 1.0, Some(10 * 3600)),
    );
    app.open_import_dialog(vec![dir.join("t")]);
    run_to_end(&mut app);
    let tokyo = CameraZone::new(540, false).unwrap();
    app.set_camera_clock(
        0,
        CameraClock {
            zone: tokyo,
            paired: true,
        },
    );
    app.set_camera_clock(
        1,
        CameraClock {
            zone: tokyo,
            paired: false,
        },
    );
    app.commit_import_dialog();
    run_to_end(&mut app);
    assert_eq!(
        clock_of(&app, &paths[0]),
        Some((10 * 3600, Some(ClockSource::Measured)))
    );
    assert_eq!(
        clock_of(&app, &paths[1]),
        Some((9 * 3600, Some(ClockSource::Setting)))
    );
    let status = app.status_text().unwrap_or_default().to_string();
    assert!(!status.contains("X100V"), "{status}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_run_matches_on_what_the_dialog_read() {
    let (mut app, dir, paths) = folder_app("geotag-reuse", 1);
    std::fs::write(&paths[0], jpeg_with(&[taken_at("2026:11:08 18:05:00")])).unwrap();
    let gpx = dir.join("t/day.gpx");
    put(&gpx, GPX);
    app.open_import_dialog(vec![gpx]);
    run_to_end(&mut app);
    // Rewritten off the track after the dialog read it.
    std::fs::write(&paths[0], jpeg_with(&[taken_at("2026:11:09 18:05:00")])).unwrap();
    app.set_camera_clock(
        0,
        CameraClock {
            zone: CameraZone::new(540, false).unwrap(),
            paired: false,
        },
    );
    app.commit_import_dialog();
    run_to_end(&mut app);
    assert_eq!(location_file(&app, &paths[0]).as_deref(), Some("day.gpx"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_run_and_a_clear_reach_every_subfolder_but_hidden_and_linked_ones() {
    let root = crate::app::test_support::temp_folder("geotag-tree");
    let outside = crate::app::test_support::temp_folder("geotag-tree-outside");
    let photo = jpeg_with(&[taken_at("2026:11:08 18:05:00")]);
    let day1 = root.join("day1/a.jpg");
    let day2 = root.join("day2/deep/b.jpg");
    let hidden = root.join(".hidden/c.jpg");
    let linked = outside.join("d.jpg");
    for p in [&day1, &day2, &hidden, &linked] {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, &photo).unwrap();
    }
    std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
    let gpx = outside.join("day.gpx");
    std::fs::write(&gpx, GPX).unwrap();

    let mut app = App::new(None);
    crate::app::test_support::load_folder(&mut app, &root);
    let out = app.run(import(&[&gpx], Some(9 * 3600))).unwrap();
    assert_eq!(out.photos, 2, "day1 and day2/deep, not .hidden or link");
    run_to_end(&mut app);
    app.folder_store.flush_blocking(Duration::from_secs(10));
    let on_disk = |p: &Path| crate::sidecar::read_record(p).unwrap().location;
    assert!(on_disk(&day1).is_some() && on_disk(&day2).is_some());
    assert!(on_disk(&hidden).is_none() && on_disk(&linked).is_none());

    assert_eq!(app.run(Command::ClearGeotags).unwrap().photos, 2);
    app.folder_store.flush_blocking(Duration::from_secs(10));
    assert!(on_disk(&day1).is_none() && on_disk(&day2).is_none());
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&outside);
}
