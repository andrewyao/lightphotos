// SPDX-License-Identifier: MIT OR Apache-2.0

use super::*;

const T0: i64 = 1_762_588_800; // 2025-11-08T08:00:00Z

fn iso(t: i64) -> String {
    chrono::DateTime::from_timestamp(t, 0)
        .unwrap()
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

fn wall(t: i64) -> String {
    chrono::DateTime::from_timestamp(t, 0)
        .unwrap()
        .format("%Y:%m:%d %H:%M:%S")
        .to_string()
}

/// A GPX file in LightRemote's layout (`GPX.render` in DayLog.swift).
fn gpx(
    points: &[(i64, f64, f64, Option<f64>)],
    clock: &[(&str, i64)],
    zones: &[(&str, i64)],
) -> String {
    let mut x = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<gpx version=\"1.1\" creator=\"LightRemote\" \
         xmlns=\"http://www.topografix.com/GPX/1/1\" xmlns:lr=\"{LR_NS}\">\n<metadata><name>day</name><extensions>\n"
    );
    for (from, minutes) in zones {
        x += &format!("<lr:utcOffset from=\"{from}\" minutes=\"{minutes}\" zone=\"Z\"/>\n");
    }
    for (at, s) in clock {
        x += &format!("<lr:cameraClock measuredAt=\"{at}\" cameraMinusUtcSeconds=\"{s}\"/>\n");
    }
    x += "</extensions></metadata><trk><name>day</name><trkseg>\n";
    for (t, lat, lon, ele) in points {
        x += &format!("<trkpt lat=\"{lat:.7}\" lon=\"{lon:.7}\">");
        if let Some(e) = ele {
            x += &format!("<ele>{e:.1}</ele>");
        }
        x += &format!("<time>{}</time><hdop>1.0</hdop></trkpt>\n", iso(*t));
    }
    x + "</trkseg></trk></gpx>\n"
}

fn track(points: &[(i64, f64, f64, Option<f64>)]) -> Track {
    Track::parse("day.gpx", gpx(points, &[], &[]).as_bytes()).unwrap()
}

fn at_utc(track: &Track, utc: i64) -> MatchOutcome {
    let clock = ClockModel::Manual {
        camera_minus_utc_s: 0,
    };
    locate(track, &clock, false, Some(&wall(utc)))
}

fn located(o: MatchOutcome) -> GeoTag {
    match o {
        MatchOutcome::Located(tag) => tag,
        other => panic!("not located: {other:?}"),
    }
}

/// LightRemote's shared contract (`LightRemoteTests/Fixtures/clock_matching.json`),
/// which `MacHelper/test_geotag.py` checks against `geotag.py` too: every case
/// picks the same camera offset, or none.
#[test]
fn the_clock_matching_contract_holds_case_by_case() {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Case {
        name: String,
        points: Vec<String>,
        clock: Vec<(String, i64)>,
        offsets: Vec<(String, i64)>,
        wall: String,
        expected_offset: Option<i64>,
    }
    let cases: Vec<Case> =
        serde_json::from_str(include_str!("../../tests/fixtures/clock_matching.json")).unwrap();
    assert_eq!(cases.len(), 7);
    for case in cases {
        let points: Vec<_> = case
            .points
            .iter()
            .map(|p| {
                let t = chrono::DateTime::parse_from_rfc3339(p).unwrap().timestamp();
                (t, 35.0, 139.0, None)
            })
            .collect();
        let clock: Vec<_> = case.clock.iter().map(|(t, s)| (t.as_str(), *s)).collect();
        let zones: Vec<_> = case.offsets.iter().map(|(t, m)| (t.as_str(), *m)).collect();
        let track = Track::parse("day.gpx", gpx(&points, &clock, &zones).as_bytes()).unwrap();
        assert_eq!(track.camera_clock.len(), case.clock.len(), "{}", case.name);
        let model = ClockModel::select(&track, None).unwrap();
        let stamp = case.wall.replace('-', ":").replace('T', " ");
        let outcome = locate(&track, &model, false, Some(&stamp));
        let offset = match &outcome {
            MatchOutcome::Located(GeoTag {
                source:
                    GeoSource::Gpx {
                        camera_minus_utc_s, ..
                    },
                ..
            }) => Some(*camera_minus_utc_s),
            _ => None,
        };
        assert_eq!(offset, case.expected_offset, "{}: {outcome:?}", case.name);
        if case.name.contains("repeated") {
            assert_eq!(outcome, MatchOutcome::Ambiguous, "{}", case.name);
        }
    }
}

#[test]
fn a_photo_between_close_fixes_lies_on_the_line_between_them() {
    let t = track(&[
        (T0, 10.0, 20.0, Some(0.0)),
        (T0 + 600, 11.0, 22.0, Some(100.0)),
    ]);
    let tag = located(at_utc(&t, T0 + 150));
    assert!((tag.lat - 10.25).abs() < 1e-9, "{tag:?}");
    assert!((tag.lon - 20.5).abs() < 1e-9, "{tag:?}");
    assert!((tag.alt.unwrap() - 25.0).abs() < 1e-9, "{tag:?}");
    let exact = located(at_utc(&t, T0 + 600));
    assert_eq!((exact.lat, exact.lon), (11.0, 22.0));
}

#[test]
fn across_a_wide_gap_a_photo_snaps_to_a_fix_within_five_minutes_or_gets_none() {
    let t = track(&[(T0, 10.0, 20.0, None), (T0 + 700, 11.0, 22.0, None)]);
    let near_first = located(at_utc(&t, T0 + 300));
    assert_eq!((near_first.lat, near_first.lon), (10.0, 20.0));
    let near_second = located(at_utc(&t, T0 + 400));
    assert_eq!((near_second.lat, near_second.lon), (11.0, 22.0));
    assert_eq!(at_utc(&t, T0 + 350), MatchOutcome::OutsideTrack);
}

#[test]
fn before_and_after_the_track_a_photo_snaps_within_five_minutes() {
    let t = track(&[(T0, 10.0, 20.0, None), (T0 + 60, 11.0, 22.0, None)]);
    assert_eq!(located(at_utc(&t, T0 - 300)).lat, 10.0);
    assert_eq!(at_utc(&t, T0 - 301), MatchOutcome::OutsideTrack);
    assert_eq!(located(at_utc(&t, T0 + 360)).lat, 11.0);
    assert_eq!(at_utc(&t, T0 + 361), MatchOutcome::OutsideTrack);
}

#[test]
fn interpolation_takes_the_short_way_across_the_antimeridian() {
    let t = track(&[(T0, 0.0, 179.0, None), (T0 + 100, 0.0, -179.0, None)]);
    let lon = located(at_utc(&t, T0 + 75)).lon;
    assert!((lon - -179.5).abs() < 1e-9, "{lon}");
}

#[test]
fn a_manual_offset_shifts_the_wall_time_to_utc() {
    let t = track(&[(T0, 10.0, 20.0, None)]);
    let tokyo = ClockModel::select(&t, Some(9 * 3600)).unwrap();
    let tag = located(locate(&t, &tokyo, false, Some(&wall(T0 + 9 * 3600))));
    assert_eq!(
        tag.source,
        GeoSource::Gpx {
            file_name: "day.gpx".into(),
            camera_minus_utc_s: 9 * 3600
        }
    );
    let utc = ClockModel::select(&t, Some(0)).unwrap();
    assert_eq!(
        locate(&t, &utc, false, Some(&wall(T0 + 9 * 3600))),
        MatchOutcome::OutsideTrack
    );
}

#[test]
fn the_clock_model_prefers_measurement_then_the_phone_zone_then_the_user() {
    let p = [(T0, 1.0, 2.0, None)];
    let at = iso(T0);
    let both = Track::parse("day.gpx", gpx(&p, &[(&at, 5)], &[(&at, 60)]).as_bytes()).unwrap();
    assert!(both.knows_the_clock());
    assert!(matches!(
        ClockModel::select(&both, Some(0)),
        Some(ClockModel::Measured(_))
    ));
    let zone = Track::parse("day.gpx", gpx(&p, &[], &[(&at, 60)]).as_bytes()).unwrap();
    assert_eq!(
        ClockModel::select(&zone, None),
        Some(ClockModel::PhoneZone(vec![ClockPeriod {
            from: T0,
            camera_minus_utc_s: 3600
        }]))
    );
    let bare = track(&p);
    assert!(!bare.knows_the_clock());
    assert_eq!(ClockModel::select(&bare, None), None);
    assert_eq!(ClockModel::select(&bare, Some(i64::MAX)), None);
}

#[test]
fn file_gps_wins_and_a_missing_or_garbled_capture_time_is_reported() {
    let t = track(&[(T0, 10.0, 20.0, None)]);
    let clock = ClockModel::Manual {
        camera_minus_utc_s: 0,
    };
    assert_eq!(
        locate(&t, &clock, true, Some(&wall(T0))),
        MatchOutcome::AlreadyHasGps
    );
    for bad in [
        None,
        Some(""),
        Some("2025:11:08"),
        Some("garbage at all here!"),
        Some("2025:11:08 08:00:0é"),
    ] {
        assert_eq!(
            locate(&t, &clock, false, bad),
            MatchOutcome::NoCaptureTime,
            "{bad:?}"
        );
    }
    // Hostile extremes never overflow.
    let far = ClockModel::Manual {
        camera_minus_utc_s: MAX_CLOCK_ERROR_S,
    };
    assert_eq!(
        locate(&t, &far, false, Some("0001:01:01 00:00:00")),
        MatchOutcome::OutsideTrack
    );
}

#[test]
fn malformed_gpx_is_an_error_not_a_panic() {
    let at = iso(T0);
    let ok_point = format!("<trkpt lat=\"1\" lon=\"2\"><time>{at}</time></trkpt>");
    let wrap = |body: &str| {
        format!(
            "<gpx xmlns=\"http://www.topografix.com/GPX/1/1\" xmlns:lr=\"{LR_NS}\">{body}</gpx>"
        )
    };
    type Expect = fn(&GpxError) -> bool;
    let cases: Vec<(String, Expect)> = vec![
        (String::new(), |e| matches!(e, GpxError::NotXml(_))),
        ("<gpx".into(), |e| matches!(e, GpxError::NotXml(_))),
        (wrap(&ok_point)[..40].to_string(), |e| matches!(e, GpxError::NotXml(_))),
        ("<kml/>".into(), |e| *e == GpxError::NotGpx),
        ("<gpx/>".into(), |e| *e == GpxError::NotGpx),
        (wrap(""), |e| *e == GpxError::NoPoints),
        (
            wrap("<trkpt lat=\"NaN\" lon=\"2\"><time>2025-11-08T08:00:00Z</time></trkpt><trkpt lat=\"91\" lon=\"2\"><time>2025-11-08T08:00:00Z</time></trkpt><trkpt lat=\"1\" lon=\"2\"/>"),
            |e| *e == GpxError::NoPoints,
        ),
        (
            wrap("<trkpt lat=\"1\" lon=\"2\"><time>2025-11-08T08:00:00</time></trkpt>"),
            |e| matches!(e, GpxError::Malformed(_)),
        ),
        (
            wrap(&format!("<lr:cameraClock measuredAt=\"{at}\"/>{ok_point}")),
            |e| matches!(e, GpxError::Malformed(_)),
        ),
        (
            wrap(&format!("<lr:cameraClock measuredAt=\"{at}\" cameraMinusUtcSeconds=\"9223372036854775807\"/>{ok_point}")),
            |e| matches!(e, GpxError::Malformed(_)),
        ),
        (
            wrap(&format!("<lr:utcOffset from=\"{at}\" minutes=\"x\"/>{ok_point}")),
            |e| matches!(e, GpxError::Malformed(_)),
        ),
        (
            wrap(&format!("<lr:utcOffset from=\"{at}\" minutes=\"99999\"/>{ok_point}")),
            |e| matches!(e, GpxError::Malformed(_)),
        ),
        (
            "<!DOCTYPE gpx [<!ENTITY a \"aaaaaaaaaa\"><!ENTITY b \"&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;\"><!ENTITY c \"&b;&b;&b;&b;&b;&b;&b;&b;&b;&b;\"><!ENTITY d \"&c;&c;&c;&c;&c;&c;&c;&c;&c;&c;\"><!ENTITY e \"&d;&d;&d;&d;&d;&d;&d;&d;&d;&d;\"><!ENTITY f \"&e;&e;&e;&e;&e;&e;&e;&e;&e;&e;\"><!ENTITY g \"&f;&f;&f;&f;&f;&f;&f;&f;&f;&f;\">]><gpx>&g;</gpx>".into(),
            |e| matches!(e, GpxError::NotXml(_) | GpxError::NotGpx),
        ),
    ];
    for (text, expect) in cases {
        let err = Track::parse("day.gpx", text.as_bytes()).unwrap_err();
        assert!(expect(&err), "{text:?} gave {err:?}");
    }
    assert!(matches!(
        Track::parse("day.gpx", &[0xff, 0xfe]),
        Err(GpxError::NotXml(_))
    ));
    let huge = vec![b' '; MAX_GPX_BYTES + 1];
    assert!(matches!(
        Track::parse("day.gpx", &huge),
        Err(GpxError::TooLarge { .. })
    ));
}

#[test]
fn points_are_sorted_and_a_repeated_second_keeps_the_first() {
    let t = track(&[
        (T0 + 60, 2.0, 2.0, None),
        (T0, 1.0, 1.0, None),
        (T0 + 60, 3.0, 3.0, None),
    ]);
    assert_eq!(
        t.points.iter().map(|p| p.point.utc).collect::<Vec<_>>(),
        [T0, T0 + 60]
    );
    assert_eq!(t.points[1].point.lat, 2.0);
}

#[test]
fn a_geotag_serializes_with_its_source_tagged_by_kind() {
    let tag = GeoTag {
        lat: 35.5,
        lon: 139.25,
        alt: None,
        source: GeoSource::Gpx {
            file_name: "2026-11-08.gpx".into(),
            camera_minus_utc_s: 32400,
        },
    };
    let json = serde_json::to_value(&tag).unwrap();
    assert_eq!(
        json,
        serde_json::json!({"lat":35.5,"lon":139.25,"source":{"kind":"gpx","file_name":"2026-11-08.gpx","camera_minus_utc_s":32400}})
    );
    assert_eq!(serde_json::from_value::<GeoTag>(json).unwrap(), tag);
    let off_globe = GeoTag { lat: 91.0, ..tag };
    assert_eq!(off_globe.gps(), None);
}

#[test]
fn a_utc_offset_reads_the_ways_people_and_exif_write_it() {
    for (text, seconds) in [
        ("+09:00", 32400),
        ("-05:30", -19800),
        (" +0530 ", 19800),
        ("9", 32400),
        ("-8", -28800),
        ("\u{2212}3:00", -10800),
        ("+00:00", 0),
    ] {
        assert_eq!(parse_utc_offset(text), Some(seconds), "{text}");
    }
    for bad in [
        "",
        "+",
        "bogus",
        "+9:5",
        "+25:00",
        "+09:60",
        "+123",
        "+09:00:00",
        "++9",
        "\u{0669}",
    ] {
        assert_eq!(parse_utc_offset(bad), None, "{bad}");
    }
    assert_eq!(format_utc_offset(-19800), "-05:30");
    assert_eq!(format_utc_offset(32400), "+09:00");
    assert_eq!(parse_utc_offset(&format_utc_offset(-12600)), Some(-12600));
}

#[test]
fn two_days_merge_so_a_photo_at_midnight_lies_between_their_fixes() {
    // 2026-11-08T14:55:00Z, five minutes before midnight at +09:00.
    let t = 1_794_149_700;
    let at = iso(t - 3600);
    let day = |name: &str, points: &[(i64, f64, f64, Option<f64>)]| {
        Track::parse(name, gpx(points, &[], &[(&at, 540)]).as_bytes()).unwrap()
    };
    let before = day(
        "2026-11-08.gpx",
        &[(t - 60, 35.0, 139.0, None), (t, 35.0, 139.0, None)],
    );
    let after = day(
        "2026-11-09.gpx",
        &[(t + 600, 35.6, 139.6, None), (t + 660, 35.6, 139.6, None)],
    );
    // 00:00:10 on the 9th by the camera: 310 s past the last fix of the 8th,
    // 290 s before the first of the 9th.
    let photo = "2026:11:09 00:00:10";
    let alone = ClockModel::select(&before, None).unwrap();
    assert_eq!(
        locate(&before, &alone, false, Some(photo)),
        MatchOutcome::OutsideTrack,
        "the 8th alone is too far"
    );

    let both = Track::merge([before, after]).unwrap();
    assert_eq!(both.file_names(), ["2026-11-08.gpx", "2026-11-09.gpx"]);
    let clock = ClockModel::select(&both, None).unwrap();
    let tag = located(locate(&both, &clock, false, Some(photo)));
    assert!(
        (tag.lat - 35.31).abs() < 1e-9,
        "on the line between days: {tag:?}"
    );
    assert_eq!(
        tag.source,
        GeoSource::Gpx {
            file_name: "2026-11-09.gpx".into(),
            camera_minus_utc_s: 9 * 3600
        },
        "named for the nearer fix"
    );
    let early = located(locate(&both, &clock, false, Some("2026:11:08 23:59:30")));
    assert!(
        matches!(&early.source, GeoSource::Gpx { file_name, .. } if file_name == "2026-11-08.gpx"),
        "{early:?}"
    );
}

#[test]
fn merging_keeps_the_first_file_at_a_shared_second_and_needs_a_track() {
    let a = Track::parse("a.gpx", gpx(&[(T0, 1.0, 1.0, None)], &[], &[]).as_bytes()).unwrap();
    let b = Track::parse(
        "b.gpx",
        gpx(&[(T0, 2.0, 2.0, None), (T0 + 1, 2.0, 2.0, None)], &[], &[]).as_bytes(),
    )
    .unwrap();
    let both = Track::merge([a, b]).unwrap();
    assert_eq!(
        both.points
            .iter()
            .map(|f| (f.point.utc, f.point.lat, f.file))
            .collect::<Vec<_>>(),
        [(T0, 1.0, 0), (T0 + 1, 2.0, 1)]
    );
    assert_eq!(Track::merge(Vec::new()), None);
}
