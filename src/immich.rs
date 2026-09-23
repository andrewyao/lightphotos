// SPDX-License-Identifier: GPL-3.0-or-later

//! A blocking client for the part of the Immich API that export uses: check a
//! key, upload a JPEG, set its rating. Any server that speaks the Immich v2/v3
//! API works, including Gumnut's compatibility layer at
//! `https://immich.gumnut.ai`. Export workers call it from their own threads,
//! so blocking costs nothing and keeps an async runtime out of the crate.
//!
//! No client-side dedupe: the server answers a repeat upload of the same bytes
//! with `status: "duplicate"` and the existing asset's id, so a re-run batch
//! converges on its own.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

pub struct ImmichServer {
    /// What the user connected to, such as `https://immich.gumnut.ai`. Also the
    /// Keychain account the API key is filed under.
    origin: String,
    /// Where the API lives, found through `/.well-known/immich`.
    api: String,
    key: String,
    agent: ureq::Agent,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Account {
    pub name: String,
    pub email: String,
}

#[derive(Debug, PartialEq)]
pub struct UploadedAsset {
    pub id: String,
    pub duplicate: bool,
}

impl ImmichServer {
    /// Find the server's API and check `key` against it. The check is
    /// `users/me` rather than `server/about` because Gumnut answers
    /// `server/about` for any key, and a wrong key should fail here, not on the
    /// first upload.
    pub fn connect(url: &str, key: &str) -> Result<(ImmichServer, Account), String> {
        let origin = normalize_origin(url)?;
        let key = key.trim().to_string();
        if key.is_empty() {
            return Err("an API key is required".into());
        }
        // No overall limit: sending a photo can take minutes on a slow uplink.
        // `upload` sets a sending budget sized to the photo instead.
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_send_body(Some(Duration::from_secs(30)))
            .timeout_recv_response(Some(Duration::from_secs(120)))
            .timeout_recv_body(Some(Duration::from_secs(60)))
            .build()
            .into();

        let well_known = agent
            .get(format!("{origin}/.well-known/immich"))
            .call()
            .map_err(|e| {
                // A home server is often plain HTTP, and a bare host defaults
                // to HTTPS. Say so rather than upgrade to sending the key in
                // the clear on our own.
                let hint = match url.contains("://") {
                    true => "",
                    false => " (for a server without HTTPS, start the URL with http://)",
                };
                format!("could not reach {origin}: {e}{hint}")
            })?;
        let well_known = match well_known.status().as_u16() {
            200 => read_body(well_known).ok(),
            _ => None,
        };
        let api = api_endpoint(&origin, well_known.as_deref());

        let server = ImmichServer {
            origin,
            api,
            key,
            agent,
        };
        let me = server
            .agent
            .get(format!("{}/users/me", server.api))
            .header("x-api-key", &server.key)
            .header("accept", "application/json")
            .call()
            .map_err(|e| e.to_string())?;
        let account = serde_json::from_str(&checked(me)?)
            .map_err(|e| format!("unexpected users/me reply: {e}"))?;
        Ok((server, account))
    }

    /// The form of `url` a server is saved under.
    pub fn normalized(url: &str) -> Result<String, String> {
        normalize_origin(url)
    }

    pub fn origin(&self) -> &str {
        &self.origin
    }

    pub fn upload(
        &self,
        jpeg: &[u8],
        filename: &str,
        taken: SystemTime,
    ) -> Result<UploadedAsset, String> {
        let boundary = format!(
            "lightphotos-{:x}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let body = multipart_body(&boundary, jpeg, filename, &iso8601(taken));
        let resp = self
            .agent
            .post(format!("{}/assets", self.api))
            .header("x-api-key", &self.key)
            .header("accept", "application/json")
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .config()
            .timeout_send_body(Some(send_budget(body.len())))
            .build()
            .send(&body[..])
            .map_err(|e| format!("upload {filename}: {e}"))?;
        parse_upload(&checked(resp)?)
    }

    /// `stars` must be 1-5. Immich v3 rejects 0, and unrated is the default.
    pub fn set_rating(&self, id: &str, stars: u8) -> Result<(), String> {
        let resp = self
            .agent
            .put(format!("{}/assets/{id}", self.api))
            .header("x-api-key", &self.key)
            .header("content-type", "application/json")
            .send(format!(r#"{{"rating":{stars}}}"#))
            .map_err(|e| format!("set rating: {e}"))?;
        checked(resp).map(|_| ())
    }
}

/// How long sending `bytes` may take. ureq has no idle timeout, only a budget
/// for the whole body, so the budget assumes a floor rate: 16 KiB/s is a
/// 1 Mbit/s uplink shared by eight export workers. A 25 MB photo gets about 27
/// minutes; a connection that stalls outright still ends.
fn send_budget(bytes: usize) -> Duration {
    const FLOOR_BYTES_PER_SEC: u64 = 16 * 1024;
    Duration::from_secs(60 + bytes as u64 / FLOOR_BYTES_PER_SEC)
}

fn read_body(mut resp: ureq::http::Response<ureq::Body>) -> Result<String, String> {
    resp.body_mut()
        .read_to_string()
        .map_err(|e| format!("read reply: {e}"))
}

/// The body of a 2xx reply, or an error that says what the server said.
fn checked(resp: ureq::http::Response<ureq::Body>) -> Result<String, String> {
    let status = resp.status().as_u16();
    let body = read_body(resp)?;
    match status {
        200..=299 => Ok(body),
        401 => Err("the server rejected the API key".into()),
        403 => Err(format!(
            "the API key lacks a permission this needs ({})",
            error_message(&body).unwrap_or_default()
        )),
        _ => Err(match error_message(&body) {
            Some(msg) => format!("HTTP {status}: {msg}"),
            None => format!("HTTP {status}"),
        }),
    }
}

/// Immich's error replies are `{"message": ..., "statusCode": ...}`.
fn error_message(body: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Err {
        message: serde_json::Value,
    }
    let msg = serde_json::from_str::<Err>(body).ok()?.message;
    Some(match msg {
        serde_json::Value::String(s) => s,
        // Validation failures list one message per field.
        other => other.to_string(),
    })
}

/// `immich.example.com/`, `https://immich.example.com/api` and
/// `https://immich.example.com` all mean the same server.
fn normalize_origin(url: &str) -> Result<String, String> {
    let url = url.trim().trim_end_matches('/');
    if url.is_empty() {
        return Err("a server URL is required".into());
    }
    let url = if url.contains("://") {
        url.to_string()
    } else {
        format!("https://{url}")
    };
    Ok(url.strip_suffix("/api").unwrap_or(&url).to_string())
}

/// The API base from a `/.well-known/immich` body, which is how Immich's own
/// apps accept a bare server URL. Falls back to `<origin>/api`.
fn api_endpoint(origin: &str, well_known: Option<&str>) -> String {
    #[derive(Deserialize)]
    struct WellKnown {
        api: Api,
    }
    #[derive(Deserialize)]
    struct Api {
        endpoint: String,
    }
    let endpoint = well_known
        .and_then(|b| serde_json::from_str::<WellKnown>(b).ok())
        .map(|w| w.api.endpoint)
        .unwrap_or_else(|| "/api".into());
    let endpoint = endpoint.trim_end_matches('/');
    if endpoint.contains("://") {
        endpoint.to_string()
    } else {
        format!("{origin}/{}", endpoint.trim_start_matches('/'))
    }
}

fn multipart_body(boundary: &str, jpeg: &[u8], filename: &str, date: &str) -> Vec<u8> {
    let mut body = Vec::with_capacity(jpeg.len() + 1024);
    for (name, value) in [
        ("fileCreatedAt", date),
        ("fileModifiedAt", date),
        ("filename", filename),
    ] {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    let quoted = filename.replace(['"', '\r', '\n'], "_");
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"assetData\"; filename=\"{quoted}\"\r\nContent-Type: image/jpeg\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(jpeg);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

fn parse_upload(body: &str) -> Result<UploadedAsset, String> {
    #[derive(Deserialize)]
    struct Reply {
        id: String,
        status: String,
    }
    let reply: Reply =
        serde_json::from_str(body).map_err(|e| format!("unexpected upload reply: {e}"))?;
    Ok(UploadedAsset {
        duplicate: reply.status == "duplicate",
        id: reply.id,
    })
}

/// UTC ISO 8601 with milliseconds, e.g. `2024-02-29T13:05:09.250Z`.
fn iso8601(t: SystemTime) -> String {
    let since = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs();
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    // Howard Hinnant's days-to-civil.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem / 60 % 60,
        rem % 60,
        since.subsec_millis()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_urls_normalize_to_one_origin() {
        for url in [
            "immich.gumnut.ai",
            "https://immich.gumnut.ai",
            "https://immich.gumnut.ai/",
            " https://immich.gumnut.ai/api/ ",
        ] {
            assert_eq!(
                normalize_origin(url).unwrap(),
                "https://immich.gumnut.ai",
                "{url:?}"
            );
        }
        assert_eq!(
            normalize_origin("http://nas.local:2283").unwrap(),
            "http://nas.local:2283"
        );
        assert!(normalize_origin("  ").is_err());
    }

    #[test]
    fn the_api_endpoint_comes_from_well_known_or_defaults_to_api() {
        let origin = "https://photos.example.com";
        assert_eq!(
            api_endpoint(origin, Some(r#"{"api":{"endpoint":"/api"}}"#)),
            "https://photos.example.com/api"
        );
        assert_eq!(
            api_endpoint(origin, Some(r#"{"api":{"endpoint":"/immich/api/"}}"#)),
            "https://photos.example.com/immich/api"
        );
        assert_eq!(
            api_endpoint(
                origin,
                Some(r#"{"api":{"endpoint":"https://api.example.com/v1"}}"#)
            ),
            "https://api.example.com/v1"
        );
        assert_eq!(api_endpoint(origin, None), "https://photos.example.com/api");
        assert_eq!(
            api_endpoint(origin, Some("<html>not found</html>")),
            "https://photos.example.com/api"
        );
    }

    #[test]
    fn multipart_body_carries_the_fields_and_the_jpeg_intact() {
        let jpeg = [0xFF, 0xD8, 0x00, 0x0D, 0x0A, 0xFF, 0xD9];
        let body = multipart_body("B", &jpeg, "IMG_1.jpg", "2024-01-02T03:04:05.000Z");
        let mut want = b"--B\r\nContent-Disposition: form-data; name=\"fileCreatedAt\"\r\n\r\n2024-01-02T03:04:05.000Z\r\n\
--B\r\nContent-Disposition: form-data; name=\"fileModifiedAt\"\r\n\r\n2024-01-02T03:04:05.000Z\r\n\
--B\r\nContent-Disposition: form-data; name=\"filename\"\r\n\r\nIMG_1.jpg\r\n\
--B\r\nContent-Disposition: form-data; name=\"assetData\"; filename=\"IMG_1.jpg\"\r\nContent-Type: image/jpeg\r\n\r\n"
            .to_vec();
        want.extend_from_slice(&jpeg);
        want.extend_from_slice(b"\r\n--B--\r\n");
        assert_eq!(body, want);
    }

    #[test]
    fn upload_replies_parse_created_and_duplicate() {
        assert_eq!(
            parse_upload(r#"{"id":"a1","status":"created"}"#).unwrap(),
            UploadedAsset {
                id: "a1".into(),
                duplicate: false
            }
        );
        assert_eq!(
            parse_upload(r#"{"id":"b2","status":"duplicate"}"#).unwrap(),
            UploadedAsset {
                id: "b2".into(),
                duplicate: true
            }
        );
        assert!(parse_upload("{}").is_err());
    }

    #[test]
    fn error_replies_surface_the_server_message() {
        assert_eq!(
            error_message(r#"{"message":"Not found","statusCode":404}"#).as_deref(),
            Some("Not found")
        );
        assert_eq!(
            error_message(r#"{"message":["rating must be ≥ 1"]}"#).as_deref(),
            Some(r#"["rating must be ≥ 1"]"#)
        );
        assert_eq!(error_message("gateway timeout"), None);
    }

    /// The case the review raised: eight 25 MB uploads sharing a 5 Mbit/s
    /// uplink take about 320 s each, which a fixed five-minute limit failed.
    #[test]
    fn the_send_budget_grows_with_the_photo() {
        let shared_uplink_secs = 8 * 25_000_000 * 8 / 5_000_000;
        assert!(send_budget(25_000_000) > Duration::from_secs(shared_uplink_secs));
        assert_eq!(send_budget(0), Duration::from_secs(60));
    }

    #[test]
    fn iso8601_formats_utc_with_millis() {
        let t = |secs: u64, ms: u64| UNIX_EPOCH + Duration::from_millis(secs * 1000 + ms);
        assert_eq!(iso8601(t(0, 0)), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso8601(t(1_709_211_909, 250)), "2024-02-29T13:05:09.250Z");
        assert_eq!(iso8601(t(951_782_400, 0)), "2000-02-29T00:00:00.000Z");
    }
}
