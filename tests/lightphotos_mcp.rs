// SPDX-License-Identifier: MIT OR Apache-2.0

//! Scripted stdio sessions against the built `lightphotos-mcp` binary.

#![cfg(not(target_arch = "wasm32"))]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use lightphotos::decode::image_decode;
use lightphotos::decode::image_encode::{self, Encoding, Pixels};
use lightphotos::sidecar::{self, Flag};
use serde_json::{json, Value};

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

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
    let jpeg = image_encode::encode(
        Encoding::Jpeg(Default::default()),
        300,
        200,
        Pixels::Rgba8(&rgba),
    )
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
        ["list_commands", "run_command", "get_state", "render"]
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

/// A stand-in for `lightphotos --control`: answers each connection's one
/// request as the app would, and keeps what it was sent.
fn fake_app(png: Vec<u8>) -> (u16, Arc<Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            let req: Value = serde_json::from_str(&line).unwrap();
            log.lock().unwrap().push(req.clone());
            let reply = match (req["token"].as_str(), req["op"].as_str()) {
                (Some(t), _) if t != TOKEN => {
                    json!({ "ok": false, "error": { "error": "bad_token" }, "message": "wrong or missing `token`" })
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
                (_, Some("screenshot")) => json!({
                    "ok": true,
                    "result": { "png_base64": lightphotos::base64::encode(&png), "width": 2, "height": 1 },
                }),
                _ => json!({ "ok": false, "error": { "error": "unknown_op" }, "message": "no op" }),
            };
            writeln!(stream, "{reply}").unwrap();
        }
    });
    (port, seen)
}

#[test]
fn an_attached_session_runs_commands_and_returns_screenshots_through_the_control_channel() {
    let png = image_encode::encode(
        Encoding::Png,
        2,
        1,
        Pixels::Rgba8(&[255, 0, 0, 255, 0, 0, 255, 255]),
    )
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
        ["list_commands", "run_command", "get_state", "screenshot"]
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

    let seen = seen.lock().unwrap();
    let ops: Vec<&str> = seen.iter().map(|r| r["op"].as_str().unwrap()).collect();
    assert_eq!(
        ops,
        ["run", "wait", "screenshot", "run"],
        "the screenshot waits for the app first"
    );
    assert!(seen.iter().all(|r| r["token"] == TOKEN));
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
