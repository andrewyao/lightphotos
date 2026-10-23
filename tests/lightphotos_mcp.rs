// SPDX-License-Identifier: MIT OR Apache-2.0

//! Scripted stdio sessions against the built `lightphotos-mcp` binary.

#![cfg(not(target_arch = "wasm32"))]
#![allow(
    clippy::disallowed_methods,
    reason = "tests write their fixtures directly; clippy.toml says why the app may not"
)]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use lightphotos::control_auth::{self, Side};
use lightphotos::decode::image_decode;
use lightphotos::decode::image_encode::{Embedded, Encoding, Pixels, Prepared};
use lightphotos::scoring::quality::{Basis, QualityScore};
use lightphotos::sidecar::{self, Flag};
use serde_json::{json, Value};

const TOKEN: &str = "0123456789abcdef0123456789abcdef";
const NONCE: &str = "00112233445566778899aabbccddeeff";

/// Runs the server with `args`, sends `lines`, closes stdin, and returns
/// each reply keyed by its position, after checking the server exited
/// cleanly on EOF.
fn session(args: &[&str], env_token: Option<&str>, lines: &[String]) -> Vec<Value> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_lightphotos-mcp"));
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env_remove("LIGHTPHOTOS_CONTROL_TOKEN");
    if let Some(token) = env_token {
        cmd.env("LIGHTPHOTOS_CONTROL_TOKEN", token);
    }
    let mut child = cmd.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    for line in lines {
        writeln!(stdin, "{line}").unwrap();
    }
    drop(stdin);
    let replies: Vec<Value> = BufReader::new(child.stdout.take().unwrap())
        .lines()
        .map(|l| serde_json::from_str(&l.unwrap()).expect("stdout carries only JSON-RPC"))
        .collect();
    assert!(child.wait().unwrap().success(), "exits cleanly on EOF");
    replies
}

fn request(id: u64, method: &str, params: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string()
}

fn call(id: u64, tool: &str, arguments: Value) -> String {
    request(
        id,
        "tools/call",
        json!({ "name": tool, "arguments": arguments }),
    )
}

fn reply(replies: &[Value], id: u64) -> &Value {
    replies
        .iter()
        .find(|r| r["id"] == json!(id))
        .unwrap_or_else(|| panic!("no reply {id} in {replies:?}"))
}

fn content_text(result: &Value) -> String {
    result["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lightphotos-mcp-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A 300x200 mid-gray JPEG.
fn write_jpeg(path: &Path) {
    let rgba: Vec<u8> = [110, 110, 110, 255].repeat(300 * 200);
    let jpeg = Prepared::new(300, 200, Pixels::Rgba8(&rgba))
        .and_then(|p| p.encode(Encoding::Jpeg(Default::default()), Embedded::NONE))
        .unwrap();
    std::fs::write(path, jpeg).unwrap();
}

fn decode_base64(text: &str) -> Vec<u8> {
    let value = |c: u8| match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => panic!("not base64: {c}"),
    };
    let mut out = Vec::new();
    for chunk in text.trim_end_matches('=').as_bytes().chunks(4) {
        let mut group = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            group |= u32::from(value(c)) << (18 - 6 * i);
        }
        out.extend_from_slice(&group.to_be_bytes()[1..chunk.len()]);
    }
    out
}

/// Decodes a returned image and gives its size and mean red value.
fn decode_image(data: &str, dir: &Path, name: &str) -> (u32, u32, f64) {
    let path = dir.join(name);
    std::fs::write(&path, decode_base64(data)).unwrap();
    let img = image_decode::decode(&path, u32::MAX).unwrap();
    let px = img.rgba.len() / 4;
    let red: f64 = img.rgba.chunks(4).map(|p| f64::from(p[0])).sum::<f64>() / px as f64;
    std::fs::remove_file(&path).unwrap();
    (img.width, img.height, red)
}

#[test]
fn a_headless_session_rates_renders_and_refuses_what_it_cannot_do() {
    let dir = temp_dir("headless");
    write_jpeg(&dir.join("a.jpg"));
    write_jpeg(&dir.join("b.jpg"));
    let out = temp_dir("headless-out");
    let folder = dir.to_str().unwrap();

    let render = |id, size| call(id, "render", json!({ "photo": "a.jpg", "max_size": size }));
    let lines = vec![
        request(1, "initialize", json!({ "protocolVersion": "2024-11-05" })),
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }).to_string(),
        request(2, "tools/list", json!({})),
        call(
            3,
            "run_command",
            json!({ "id": "photo.rate", "params": { "stars": 4, "photos": ["a.jpg"] } }),
        ),
        render(4, 100),
        call(
            5,
            "run_command",
            json!({ "id": "develop.set", "params": { "controls": { "exposure": 2 }, "photos": ["a.jpg"] } }),
        ),
        render(6, 100),
        "{not json".to_string(),
        request(7, "resources/list", json!({})),
        call(
            8,
            "run_command",
            json!({ "id": "export", "params": { "photos": ["a.jpg"] } }),
        ),
        call(
            9,
            "run_command",
            json!({ "id": "photo.flag", "params": { "flag": "pick", "photos": ["a.jpg", "../b.jpg"] } }),
        ),
        call(10, "get_state", json!({})),
        call(11, "screenshot", json!({})),
        call(
            12,
            "run_command",
            json!({ "id": "photo.shine", "params": {} }),
        ),
        request(13, "tools/call", json!({ "name": "nope" })),
        request(14, "ping", json!({})),
        "x".repeat(5 << 20),
        request(15, "ping", json!({})),
    ];
    let replies = session(&["--folder", folder], None, &lines);
    assert_eq!(
        replies.len(),
        17,
        "one reply per request and bad line, none for the notification"
    );

    let init = &reply(&replies, 1)["result"];
    assert_eq!(init["protocolVersion"], "2024-11-05");
    assert_eq!(init["serverInfo"]["name"], "lightphotos");
    assert!(init["instructions"]
        .as_str()
        .unwrap()
        .contains("list_commands"));

    let tools: Vec<&str> = reply(&replies, 2)["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        tools,
        [
            "list_commands",
            "run_command",
            "get_state",
            "list_photos",
            "render"
        ]
    );

    assert_eq!(reply(&replies, 3)["result"]["isError"], false);
    let saved = sidecar::read_record(&dir.join("a.jpg")).unwrap();
    assert_eq!(saved.rating, Some(4));
    assert!(
        saved.adjustments.exposure > 1.9,
        "the exposure edit was saved"
    );

    let image_of = |id| {
        let result = &reply(&replies, id)["result"];
        assert_eq!(result["isError"], false, "{result}");
        let image = &result["content"][0];
        assert_eq!(image["mimeType"], "image/jpeg");
        decode_image(image["data"].as_str().unwrap(), &out, &format!("{id}.jpg"))
    };
    let (w, h, plain) = image_of(4);
    assert_eq!((w, h), (100, 67), "scaled to max_size on the long edge");
    let (_, _, brighter) = image_of(6);
    assert!(
        brighter > plain + 30.0,
        "render applies the saved exposure: {plain} -> {brighter}"
    );

    let bad_line = replies
        .iter()
        .find(|r| r["error"]["code"] == -32700)
        .unwrap();
    assert_eq!(bad_line["id"], Value::Null);
    assert_eq!(reply(&replies, 7)["error"]["code"], -32601);

    let needs_app = &reply(&replies, 8)["result"];
    assert_eq!(needs_app["isError"], true);
    assert!(content_text(needs_app).contains("--attach"), "{needs_app}");

    let outside = &reply(&replies, 9)["result"];
    assert_eq!(outside["isError"], true);
    assert_eq!(
        sidecar::read_record(&dir.join("a.jpg")).unwrap().flag,
        None,
        "a refused photo in the list writes none of them"
    );

    let state: Value = serde_json::from_str(&content_text(&reply(&replies, 10)["result"])).unwrap();
    assert_eq!(state["total"], 2);
    assert_eq!(state["photos"][0]["name"], "a.jpg");
    assert_eq!(state["photos"][0]["rating"], 4);
    assert_eq!(state["photos"][0]["edited"], true);
    assert_eq!(state["photos"][1]["rating"], Value::Null);

    assert_eq!(reply(&replies, 11)["result"]["isError"], true);
    let unknown = &reply(&replies, 12)["result"];
    assert!(content_text(unknown).contains("list_commands"), "{unknown}");
    assert_eq!(reply(&replies, 13)["error"]["code"], -32602);
    assert_eq!(reply(&replies, 14)["result"], json!({}));
    let too_long = replies
        .iter()
        .find(|r| r["error"]["code"] == -32600)
        .unwrap();
    assert!(too_long["error"]["message"]
        .as_str()
        .unwrap()
        .contains("longer"));
    assert_eq!(
        reply(&replies, 15)["result"],
        json!({}),
        "the server reads on after a long line"
    );

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&out);
}

#[test]
fn a_headless_toggle_settles_once_for_the_set() {
    let dir = temp_dir("toggle");
    write_jpeg(&dir.join("a.jpg"));
    write_jpeg(&dir.join("b.jpg"));
    let picked = sidecar::ImageRecord {
        flag: Some(Flag::Pick),
        ..Default::default()
    };
    sidecar::write_record(&dir.join("a.jpg"), &picked).unwrap();
    let toggle = json!({ "id": "photo.flag", "params": { "flag": "toggle_pick", "photos": ["a.jpg", "b.jpg"] } });
    let replies = session(
        &["--folder", dir.to_str().unwrap()],
        None,
        &[call(1, "run_command", toggle)],
    );
    assert_eq!(reply(&replies, 1)["result"]["isError"], false);
    for name in ["a.jpg", "b.jpg"] {
        assert_eq!(
            sidecar::read_record(&dir.join(name)).unwrap().flag,
            Some(Flag::Pick),
            "{name}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_headless_score_saves_what_the_listing_then_reports() {
    let dir = temp_dir("score-run");
    write_jpeg(&dir.join("a.jpg"));
    let score = json!({ "id": "photo.score", "params": { "photos": ["a.jpg"] } });
    let replies = session(
        &["--folder", dir.to_str().unwrap()],
        None,
        &[
            call(1, "list_commands", json!({})),
            call(2, "run_command", score),
            call(3, "list_photos", json!({})),
        ],
    );
    let listing: Value =
        serde_json::from_str(&content_text(&reply(&replies, 1)["result"])).unwrap();
    let listed = listing["commands"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "photo.score")
        .cloned();
    assert_eq!(listed.map(|c| c["headless"].clone()), Some(json!(true)));
    assert_eq!(reply(&replies, 2)["result"]["isError"], false);
    let photos: Value = serde_json::from_str(&content_text(&reply(&replies, 3)["result"])).unwrap();
    let saved = sidecar::read_record(&dir.join("a.jpg")).unwrap();
    if cfg!(target_os = "macos") {
        let stored = saved.score.expect("scored");
        assert_eq!(
            photos["photos"][0]["score"],
            json!({ "value": stored.score.value, "stale": false })
        );
    } else {
        assert!(
            saved.score.is_none(),
            "scoring needs macOS; nothing changes"
        );
        assert!(photos["photos"][0].get("score").is_none());
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_headless_listing_reports_the_score_the_app_stored() {
    let dir = temp_dir("score");
    for name in ["scored.jpg", "edited.jpg", "unscored.jpg"] {
        write_jpeg(&dir.join(name));
    }
    let score_for = |rec: &sidecar::ImageRecord| sidecar::StoredScore {
        score: QualityScore {
            value: 72,
            basis: Basis::WithAesthetics,
            base: Some(80),
            deductions: vec![],
        },
        edits: rec.edit_signature(),
    };
    let mut scored = sidecar::ImageRecord::default();
    scored.score = Some(score_for(&scored));
    sidecar::write_record(&dir.join("scored.jpg"), &scored).unwrap();
    let mut edited = sidecar::ImageRecord::default();
    edited.score = Some(score_for(&edited));
    edited.adjustments.exposure = 1.0;
    sidecar::write_record(&dir.join("edited.jpg"), &edited).unwrap();

    let replies = session(
        &["--folder", dir.to_str().unwrap()],
        None,
        &[call(1, "list_photos", json!({}))],
    );
    let listed: Value = serde_json::from_str(&content_text(&reply(&replies, 1)["result"])).unwrap();
    let score_of = |name: &str| {
        listed["photos"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == name)
            .map(|p| p.get("score").cloned())
            .unwrap()
    };
    assert_eq!(
        score_of("scored.jpg"),
        Some(json!({ "value": 72, "stale": false }))
    );
    assert_eq!(
        score_of("edited.jpg"),
        Some(json!({ "value": 72, "stale": true }))
    );
    assert_eq!(score_of("unscored.jpg"), None);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A stand-in for `lightphotos --control`: signs each connection in, answers
/// its requests as the app would, and keeps every request after the sign-in.
fn fake_app(png: Vec<u8>) -> (u16, Arc<Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut signed_in = false;
            let mut client = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                let req: Value = serde_json::from_str(&line).unwrap();
                if signed_in {
                    log.lock().unwrap().push(req.clone());
                }
                let reply = match (signed_in, req["op"].as_str()) {
                    (_, Some("hello")) => {
                        client = req["nonce"].as_str().unwrap_or("").to_string();
                        let proof = control_auth::mac_hex(TOKEN, Side::Server, &client, NONCE);
                        json!({ "ok": true, "result": { "nonce": NONCE, "proof": proof } })
                    }
                    (_, Some("auth")) => {
                        let mac = req["mac"].as_str().unwrap_or("");
                        signed_in = control_auth::verify(TOKEN, Side::Client, &client, NONCE, mac);
                        match signed_in {
                            true => json!({ "ok": true, "result": { "signed_in": true } }),
                            false => {
                                json!({ "ok": false, "error": { "error": "bad_token" }, "message": "wrong `mac`" })
                            }
                        }
                    }
                    (false, _) => {
                        json!({ "ok": false, "error": { "error": "auth_required" }, "message": "not signed in" })
                    }
                    (_, Some("run")) if req["command"]["id"] == "photo.rate" => {
                        json!({ "ok": true, "result": { "photos": 1, "pending": false } })
                    }
                    (_, Some("run")) => json!({
                        "ok": false,
                        "error": { "error": "unknown_command", "id": req["command"]["id"] },
                        "message": "no command; list the commands to see every id",
                    }),
                    (_, Some("wait")) => json!({ "ok": true, "result": { "view": "grid" } }),
                    (_, Some("list")) => json!({
                        "ok": true,
                        "result": { "photos": [{ "name": "a.jpg", "rating": 3 }], "total": 1, "truncated": false },
                    }),
                    (_, Some("screenshot")) => json!({
                        "ok": true,
                        "result": { "png_base64": lightphotos::base64::encode(&png), "width": 2, "height": 1 },
                    }),
                    _ => {
                        json!({ "ok": false, "error": { "error": "unknown_op" }, "message": "no op" })
                    }
                };
                writeln!(stream, "{reply}").unwrap();
            }
        }
    });
    (port, seen)
}

#[test]
fn an_attached_session_runs_commands_and_returns_screenshots_through_the_control_channel() {
    let png = Prepared::new(2, 1, Pixels::Rgba8(&[255, 0, 0, 255, 0, 0, 255, 255]))
        .and_then(|p| p.encode(Encoding::Png, Embedded::NONE))
        .unwrap();
    let (port, seen) = fake_app(png.clone());
    let port_arg = port.to_string();
    let lines = vec![
        request(1, "tools/list", json!({})),
        call(
            2,
            "run_command",
            json!({ "id": "photo.rate", "params": { "stars": 4 } }),
        ),
        call(3, "screenshot", json!({})),
        call(4, "run_command", json!({ "id": "photo.shine" })),
        call(5, "render", json!({ "photo": "a.jpg" })),
        call(6, "list_photos", json!({})),
    ];
    let replies = session(&["--attach", &port_arg], Some(TOKEN), &lines);

    let tools: Vec<&str> = reply(&replies, 1)["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        tools,
        [
            "list_commands",
            "run_command",
            "get_state",
            "list_photos",
            "screenshot"
        ]
    );

    let rated = &reply(&replies, 2)["result"];
    assert_eq!(rated["isError"], false);
    assert_eq!(
        serde_json::from_str::<Value>(&content_text(rated)).unwrap()["photos"],
        1
    );

    let shot = &reply(&replies, 3)["result"];
    assert_eq!(shot["content"][0]["type"], "image");
    assert_eq!(shot["content"][0]["mimeType"], "image/png");
    assert_eq!(
        decode_base64(shot["content"][0]["data"].as_str().unwrap()),
        png
    );

    let unknown = &reply(&replies, 4)["result"];
    assert_eq!(unknown["isError"], true);
    assert!(content_text(unknown).contains("list_commands"), "{unknown}");
    assert_eq!(reply(&replies, 5)["result"]["isError"], true);
    let listed = &reply(&replies, 6)["result"];
    assert_eq!(
        serde_json::from_str::<Value>(&content_text(listed)).unwrap()["photos"][0]["rating"],
        3
    );

    let seen = seen.lock().unwrap();
    let ops: Vec<&str> = seen.iter().map(|r| r["op"].as_str().unwrap()).collect();
    assert_eq!(
        ops,
        ["run", "wait", "screenshot", "run", "list"],
        "the screenshot waits for the app first"
    );
    assert!(
        seen.iter().all(|r| r.get("token").is_none()),
        "the token never crosses the socket"
    );
    assert_eq!(
        seen[0]["command"],
        json!({ "id": "photo.rate", "params": { "stars": 4 } })
    );
}

#[test]
fn an_attached_call_with_no_app_or_no_token_says_how_to_start_it() {
    let closed = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
        .to_string();
    let replies = session(
        &["--attach", &closed],
        Some(TOKEN),
        &[
            call(1, "get_state", json!({})),
            "\n".into(),
            call(2, "get_state", json!({})),
        ],
    );
    let refused = &reply(&replies, 1)["result"];
    assert_eq!(refused["isError"], true);
    assert!(content_text(refused).contains("--control"), "{refused}");
    assert_eq!(replies.len(), 2, "a blank line gets no reply");

    let replies = session(
        &["--attach", &closed],
        None,
        &[call(1, "get_state", json!({}))],
    );
    let tokenless = &reply(&replies, 1)["result"];
    assert!(
        content_text(tokenless).contains("LIGHTPHOTOS_CONTROL_TOKEN"),
        "{tokenless}"
    );
}

/// One writer per folder. While LightPhotos has the folder open (here this
/// test holds the lock as the app does), a headless write is refused with
/// the `--attach` hint and changes nothing, and reads still work. Once the
/// app lets go, the same write goes through. A crashed app's lock file,
/// its line still naming it, blocks no one.
#[test]
fn a_headless_write_waits_its_turn_behind_the_app() {
    let dir = temp_dir("one-writer");
    let photo = dir.join("a.jpg");
    write_jpeg(&photo);
    let folder = dir.to_str().unwrap();
    let rate = || {
        call(
            2,
            "run_command",
            json!({ "id": "photo.rate", "params": { "stars": 4, "photos": ["a.jpg"] } }),
        )
    };

    let app = sidecar::FolderLock::acquire(&dir, sidecar::Writer::App, sidecar::LOCK_WAIT).unwrap();
    let refused = session(
        &["--folder", folder],
        None,
        &[call(1, "get_state", json!({})), rate()],
    );
    let untouched = sidecar::read_record(&photo).unwrap();
    drop(app);
    let allowed = session(&["--folder", folder], None, &[rate()]);
    let rated = sidecar::read_record(&photo).unwrap();

    // The app crashes: its line stays in the file, its OS lock goes.
    sidecar::write_record(&photo, &sidecar::ImageRecord::default()).unwrap();
    let who = dir.join(sidecar::SIDECAR_DIR).join(sidecar::WHO_FILE);
    std::fs::write(&who, "4000000000 app\n").unwrap();
    let after_crash = session(&["--folder", folder], None, &[rate()]);
    let rated_after_crash = sidecar::read_record(&photo).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();

    let read = &reply(&refused, 1)["result"];
    assert_eq!(read["isError"], false, "reads work");
    let state: Value = serde_json::from_str(&content_text(read)).unwrap();
    let read_only = state["read_only"].as_str().unwrap_or_default();
    assert!(
        read_only.contains("has this folder open") && read_only.contains("--attach"),
        "{state}"
    );
    let no = &reply(&refused, 2)["result"];
    assert_eq!(no["isError"], true, "{no}");
    let why = content_text(no);
    assert!(why.contains("--attach"), "{why}");
    assert!(why.contains(&std::process::id().to_string()), "{why}");
    assert!(why.contains("nothing was changed"), "{why}");
    assert_eq!(untouched.rating, None, "the sidecar is unchanged");

    assert_eq!(reply(&allowed, 2)["result"]["isError"], false);
    assert_eq!(rated.rating, Some(4));
    assert_eq!(reply(&after_crash, 2)["result"]["isError"], false);
    assert_eq!(rated_after_crash.rating, Some(4));
}
