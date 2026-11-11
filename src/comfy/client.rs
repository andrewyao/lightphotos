// SPDX-License-Identifier: MIT OR Apache-2.0

//! A blocking client for the part of ComfyUI's HTTP API that LightPhotos uses:
//! check the server, check it has a workflow's models, upload an image, queue a
//! workflow, wait for it, fetch the result, and cancel. Fill workers call it
//! from their own threads, so blocking keeps an async runtime out of the
//! crate, as in `export/immich.rs`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::{Map, Value};

use super::workflow::{self, ModelRef};

/// ComfyUI Desktop's port, then the port a source install listens on.
pub const DEFAULT_URLS: [&str; 2] = ["http://127.0.0.1:8000", "http://127.0.0.1:8188"];

const SMALL_REPLY: u64 = 1024 * 1024;
const OBJECT_INFO_REPLY: u64 = 16 * 1024 * 1024;
const HISTORY_REPLY: u64 = 8 * 1024 * 1024;
const IMAGE_REPLY: u64 = 64 * 1024 * 1024;
const POLL: Duration = Duration::from_millis(500);
/// What [`ComfyServer::wait`] answers when its caller stopped it.
pub const STOPPED: &str = "the job was cancelled";
/// What a request answers when the server didn't: nothing is listening, the
/// connection dropped or the reply never came. The caller words it for the
/// user, with the address and what to do next; the transport's own text
/// goes to the log.
pub const LOST: &str = "ComfyUI stopped responding";

/// [`LOST`], with why in the log.
fn lost(url: &str, e: &ureq::Error) -> String {
    eprintln!("[comfy] no answer from {url}: {e}");
    LOST.into()
}

pub struct ComfyServer {
    url: String,
    agent: ureq::Agent,
}

#[derive(Debug, PartialEq)]
pub struct ServerInfo {
    pub version: Option<String>,
}

#[derive(Debug, PartialEq)]
pub struct MissingModel {
    pub loader: &'static str,
    pub folder: &'static str,
    pub file: String,
}

impl MissingModel {
    pub fn message(&self) -> String {
        format!(
            "ComfyUI is missing {}. Download it into models/{}.",
            self.file, self.folder
        )
    }
}

#[derive(Debug, PartialEq)]
pub struct Uploaded {
    pub name: String,
    pub subfolder: String,
}

impl Uploaded {
    /// The value a `LoadImage` node's `image` input takes.
    pub fn image_ref(&self) -> String {
        match self.subfolder.is_empty() {
            true => self.name.clone(),
            false => format!("{}/{}", self.subfolder, self.name),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct OutputImage {
    pub filename: String,
    #[serde(default)]
    pub subfolder: String,
    #[serde(rename = "type", default = "output_kind")]
    pub kind: String,
}

fn output_kind() -> String {
    "output".into()
}

/// `127.0.0.1:8188/` and `http://127.0.0.1:8188` mean the same server. A bare
/// host gets `http://`, since ComfyUI serves plain HTTP on localhost.
pub fn normalize_url(url: &str) -> Result<String, String> {
    let url = url.trim().trim_end_matches('/');
    if url.is_empty() {
        return Err("a ComfyUI server URL is required".into());
    }
    match url.split_once("://") {
        None => Ok(format!("http://{url}")),
        Some((scheme, _))
            if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") =>
        {
            Ok(url.to_string())
        }
        Some((scheme, _)) => Err(format!(
            "a ComfyUI URL starts with http://, not {scheme}://"
        )),
    }
}

impl ComfyServer {
    pub fn new(url: &str) -> Result<ComfyServer, String> {
        // No redirects: a local server has no reason to send us elsewhere.
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_connect(Some(Duration::from_secs(2)))
            .timeout_send_body(Some(Duration::from_secs(30)))
            .timeout_recv_response(Some(Duration::from_secs(60)))
            .timeout_recv_body(Some(Duration::from_secs(60)))
            .build()
            .into();
        Ok(ComfyServer {
            url: normalize_url(url)?,
            agent,
        })
    }

    /// Whether a ComfyUI server answers at `url`, within about a second.
    pub fn probe(url: &str) -> Result<ServerInfo, String> {
        let url = normalize_url(url)?;
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_connect(Some(Duration::from_secs(1)))
            .timeout_recv_response(Some(Duration::from_secs(1)))
            .timeout_recv_body(Some(Duration::from_secs(1)))
            .build()
            .into();
        let resp = agent
            .get(format!("{url}/system_stats"))
            .call()
            .map_err(|e| format!("ComfyUI isn't running at {url} ({e})"))?;
        let body = checked(resp, SMALL_REPLY).map_err(|e| format!("{url}: {e}"))?;
        parse_system_stats(&body).ok_or(format!("{url} answered, but it isn't ComfyUI"))
    }

    /// The first of [`DEFAULT_URLS`] where ComfyUI answers.
    pub fn find_default() -> Option<String> {
        DEFAULT_URLS
            .iter()
            .find(|url| ComfyServer::probe(url).is_ok())
            .map(|url| url.to_string())
    }

    /// The model files `workflow` loads that ComfyUI doesn't list.
    pub fn missing_models(
        &self,
        workflow: &Map<String, Value>,
    ) -> Result<Vec<MissingModel>, String> {
        let wanted = workflow::model_files(workflow);
        let mut info = Map::new();
        for class in workflow::loader_classes() {
            if !wanted.iter().any(|m| m.loader == class) {
                continue;
            }
            let resp = self
                .agent
                .get(format!("{}/object_info/{class}", self.url))
                .call()
                .map_err(|e| lost(&self.url, &e))?;
            let body = checked(resp, OBJECT_INFO_REPLY)?;
            if let Ok(Value::Object(one)) = serde_json::from_str::<Value>(&body) {
                info.extend(one);
            }
        }
        missing_in(&Value::Object(info), &wanted)
    }

    /// Uploads `png` to ComfyUI's input folder as `name`, replacing a file of
    /// that name.
    pub fn upload_png(&self, png: &[u8], name: &str) -> Result<Uploaded, String> {
        let boundary = format!(
            "lightphotos-{:x}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let body = multipart_body(&boundary, png, name);
        let resp = self
            .agent
            .post(format!("{}/upload/image", self.url))
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .send(&body[..])
            .map_err(|e| lost(&self.url, &e))?;
        parse_upload(&checked(resp, SMALL_REPLY)?)
    }

    /// Queues `prompt`, an injected API-format workflow. Returns its id.
    pub fn queue(&self, prompt: &Value, client_id: &str) -> Result<String, String> {
        let resp = self
            .agent
            .post(format!("{}/prompt", self.url))
            .header("content-type", "application/json")
            .send(serde_json::json!({ "prompt": prompt, "client_id": client_id }).to_string())
            .map_err(|e| lost(&self.url, &e))?;
        let status = resp.status().as_u16();
        let body = read_capped(resp, OBJECT_INFO_REPLY)?;
        parse_queue(status, &String::from_utf8_lossy(&body))
    }

    /// Polls until `prompt_id` finishes, `deadline` passes or `stop` is set.
    /// Returns the images `output_node` saved.
    pub fn wait(
        &self,
        prompt_id: &str,
        output_node: &str,
        deadline: Instant,
        stop: &AtomicBool,
    ) -> Result<Vec<OutputImage>, String> {
        loop {
            if stop.load(Ordering::Relaxed) {
                return Err(STOPPED.into());
            }
            let resp = self
                .agent
                .get(format!("{}/history/{prompt_id}", self.url))
                .call()
                .map_err(|e| lost(&self.url, &e))?;
            let body = checked(resp, HISTORY_REPLY)?;
            if let History::Done(images) = parse_history(&body, prompt_id, output_node)? {
                return Ok(images);
            }
            if Instant::now() + POLL > deadline {
                return Err("ComfyUI took too long to finish".into());
            }
            std::thread::sleep(POLL);
        }
    }

    pub fn fetch(&self, image: &OutputImage) -> Result<Vec<u8>, String> {
        let resp = self
            .agent
            .get(format!("{}/view", self.url))
            .query("filename", &image.filename)
            .query("subfolder", &image.subfolder)
            .query("type", &image.kind)
            .call()
            .map_err(|e| lost(&self.url, &e))?;
        let status = resp.status().as_u16();
        let bytes = read_capped(resp, IMAGE_REPLY)?;
        match status {
            200 => Ok(bytes),
            _ => Err(format!(
                "ComfyUI answered HTTP {status} for {}",
                image.filename
            )),
        }
    }

    /// Takes `prompt_id` off the queue if it hasn't started, and stops it if
    /// it has. An older ComfyUI ignores the id and stops whatever is running.
    pub fn cancel(&self, prompt_id: &str) -> Result<(), String> {
        let send = |path: &str, body: Value| -> Result<(), String> {
            let resp = self
                .agent
                .post(format!("{}{path}", self.url))
                .header("content-type", "application/json")
                .send(body.to_string())
                .map_err(|e| lost(&self.url, &e))?;
            checked(resp, SMALL_REPLY).map(|_| ())
        };
        send("/queue", serde_json::json!({ "delete": [prompt_id] }))?;
        send("/interrupt", serde_json::json!({ "prompt_id": prompt_id }))
    }
}

fn read_capped(mut resp: ureq::http::Response<ureq::Body>, cap: u64) -> Result<Vec<u8>, String> {
    resp.body_mut()
        .with_config()
        .limit(cap)
        .read_to_vec()
        .map_err(|e| match e {
            ureq::Error::BodyExceedsLimit(_) => "ComfyUI's reply was too large to read".into(),
            e => lost("the server", &e),
        })
}

/// The body of a 2xx reply, or an error naming the status.
fn checked(resp: ureq::http::Response<ureq::Body>, cap: u64) -> Result<String, String> {
    let status = resp.status().as_u16();
    let body = String::from_utf8_lossy(&read_capped(resp, cap)?).into_owned();
    match status {
        200..=299 => Ok(body),
        _ => Err(format!(
            "ComfyUI answered HTTP {status}: {}",
            clip(&body, 300)
        )),
    }
}

/// At most `max` chars of `text`.
fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((end, _)) => format!("{}…", text.get(..end).unwrap_or_default()),
        None => text.to_string(),
    }
}

fn parse_system_stats(body: &str) -> Option<ServerInfo> {
    let stats: Value = serde_json::from_str(body).ok()?;
    let system = stats.get("system")?.as_object()?;
    Some(ServerInfo {
        version: system
            .get("comfyui_version")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// The options a COMBO input lists, in either schema: `[[a, b], {..}]` or, in
/// newer ComfyUI, `["COMBO", {"options": [a, b]}]`.
fn combo_options<'a>(info: &'a Value, class: &str, input: &str) -> Option<Vec<&'a str>> {
    let inputs = info.get(class)?.get("input")?;
    let spec = ["required", "optional"]
        .iter()
        .find_map(|group| inputs.get(group)?.get(input))?;
    let first = spec.get(0)?;
    let options = match first.as_str() {
        Some("COMBO") => spec.get(1)?.get("options")?,
        _ => first,
    };
    Some(
        options
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .collect(),
    )
}

fn missing_in(info: &Value, wanted: &[ModelRef]) -> Result<Vec<MissingModel>, String> {
    let mut missing = Vec::new();
    for model in wanted {
        let options = combo_options(info, model.loader, model.input).ok_or(format!(
            "ComfyUI has no {} node. Update ComfyUI to use this feature.",
            model.loader
        ))?;
        if !options.contains(&model.file.as_str()) {
            missing.push(MissingModel {
                loader: model.loader,
                folder: model.folder,
                file: model.file.clone(),
            });
        }
    }
    Ok(missing)
}

fn multipart_body(boundary: &str, png: &[u8], name: &str) -> Vec<u8> {
    let mut body = Vec::with_capacity(png.len() + 512);
    for (field, value) in [("overwrite", "true"), ("type", "input")] {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    let quoted = name.replace(['"', '\r', '\n', '/', '\\'], "_");
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"image\"; filename=\"{quoted}\"\r\nContent-Type: image/png\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(png);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

fn parse_upload(body: &str) -> Result<Uploaded, String> {
    #[derive(Deserialize)]
    struct Reply {
        name: String,
        #[serde(default)]
        subfolder: String,
    }
    let reply: Reply = serde_json::from_str(body)
        .map_err(|e| format!("unexpected reply from ComfyUI's upload: {e}"))?;
    Ok(Uploaded {
        name: reply.name,
        subfolder: reply.subfolder,
    })
}

/// A queued prompt's id, or what ComfyUI said is wrong with the workflow.
fn parse_queue(status: u16, body: &str) -> Result<String, String> {
    let reply: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    if (200..300).contains(&status) {
        return reply
            .get("prompt_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("unexpected reply from ComfyUI's queue: {}", clip(body, 200)));
    }
    let mut parts = Vec::new();
    if let Some(error) = reply.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| error.as_str());
        let details = error
            .get("details")
            .and_then(Value::as_str)
            .filter(|d| !d.is_empty());
        match (message, details) {
            (Some(m), Some(d)) => parts.push(format!("{m}: {d}")),
            (Some(m), None) => parts.push(m.to_string()),
            _ => {}
        }
    }
    if let Some(Value::Object(nodes)) = reply.get("node_errors") {
        let mut ids: Vec<&String> = nodes.keys().collect();
        ids.sort();
        for id in ids {
            let Some(node) = nodes.get(id) else { continue };
            let class = node
                .get("class_type")
                .and_then(Value::as_str)
                .unwrap_or("?");
            let errors = node.get("errors").and_then(Value::as_array);
            for e in errors.into_iter().flatten() {
                let message = e.get("message").and_then(Value::as_str).unwrap_or("error");
                let details = e.get("details").and_then(Value::as_str).unwrap_or("");
                parts.push(match details.is_empty() {
                    true => format!("node {id} ({class}): {message}"),
                    false => format!("node {id} ({class}): {message}: {details}"),
                });
            }
        }
    }
    let why = match parts.is_empty() {
        true => format!("HTTP {status}"),
        false => parts.join("; "),
    };
    Err(clip(&format!("ComfyUI rejected the workflow: {why}"), 600))
}

#[derive(Debug, PartialEq)]
enum History {
    Pending,
    Done(Vec<OutputImage>),
}

/// ComfyUI adds a prompt to `/history` once it has run, so no entry means it
/// is still queued or running.
fn parse_history(body: &str, prompt_id: &str, output_node: &str) -> Result<History, String> {
    let unexpected = || format!("unexpected history reply from ComfyUI: {}", clip(body, 200));
    let Ok(Value::Object(history)) = serde_json::from_str::<Value>(body) else {
        return Err(unexpected());
    };
    let Some(entry) = history.get(prompt_id) else {
        return Ok(History::Pending);
    };
    if !entry.is_object() {
        return Err(unexpected());
    }
    let status = entry.get("status");
    if status
        .and_then(|s| s.get("status_str"))
        .and_then(Value::as_str)
        == Some("error")
    {
        return Err(execution_error(status));
    }
    let images: Vec<OutputImage> = entry
        .get("outputs")
        .and_then(|o| o.get(output_node))
        .and_then(|n| n.get("images"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|i| serde_json::from_value(i.clone()).ok())
        .collect();
    match images.is_empty() {
        true => Err("ComfyUI finished without saving an image".into()),
        false => Ok(History::Done(images)),
    }
}

/// The reason in a failed prompt's `status.messages`, a list of
/// `[event, data]` pairs.
fn execution_error(status: Option<&Value>) -> String {
    let messages = status
        .and_then(|s| s.get("messages"))
        .and_then(Value::as_array);
    for pair in messages.into_iter().flatten() {
        let event = pair.get(0).and_then(Value::as_str);
        let data = pair.get(1);
        match event {
            Some("execution_interrupted") => return "ComfyUI stopped the job".into(),
            Some("execution_error") => {
                let node = data
                    .and_then(|d| d.get("node_type"))
                    .and_then(Value::as_str)
                    .unwrap_or("a node");
                let message = data
                    .and_then(|d| d.get("exception_message"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                return clip(
                    &format!("ComfyUI failed in {node}: {}", message.trim()),
                    600,
                );
            }
            _ => {}
        }
    }
    "ComfyUI failed to run the workflow".into()
}

#[cfg(test)]
mod tests {
    use super::super::test_server::{serve, Reply};
    use super::*;

    #[test]
    fn urls_normalize() {
        assert_eq!(
            normalize_url(" 127.0.0.1:8188/ ").unwrap(),
            "http://127.0.0.1:8188"
        );
        assert_eq!(
            normalize_url("https://box:8000").unwrap(),
            "https://box:8000"
        );
        assert!(normalize_url("").is_err());
        assert!(normalize_url("ftp://box").is_err());
    }

    #[test]
    fn probe_needs_comfyui_not_just_any_server() {
        let comfy = serve(|r| match r.path.as_str() {
            "/system_stats" => Reply::new(
                200,
                r#"{"system": {"os": "posix", "comfyui_version": "0.3.60"}, "devices": []}"#,
            ),
            _ => Reply::new(404, ""),
        });
        assert_eq!(
            ComfyServer::probe(&comfy).unwrap().version.as_deref(),
            Some("0.3.60")
        );
        let other = serve(|_| Reply::new(200, "<html>dev server</html>"));
        let err = ComfyServer::probe(&other).unwrap_err();
        assert!(err.contains("isn't ComfyUI"), "{err}");
        let gone = serve(|_| Reply::new(500, "boom"));
        assert!(ComfyServer::probe(&gone).is_err());
    }

    #[test]
    fn probe_of_a_closed_port_fails_fast() {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let start = Instant::now();
        let err = ComfyServer::probe(&format!("127.0.0.1:{port}")).unwrap_err();
        assert!(err.contains("isn't running"), "{err}");
        assert!(start.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn a_server_that_went_away_is_lost_with_no_transport_text() {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let server = ComfyServer::new(&format!("127.0.0.1:{port}")).unwrap();
        let never = Instant::now() + Duration::from_secs(60);
        let stop = AtomicBool::new(false);
        assert_eq!(server.wait("id", "10", never, &stop).unwrap_err(), LOST);
        assert_eq!(server.queue(&Value::Null, "c").unwrap_err(), LOST);
        assert_eq!(server.upload_png(&[], "a.png").unwrap_err(), LOST);
        assert_eq!(server.cancel("id").unwrap_err(), LOST);
    }

    fn refs() -> Vec<ModelRef> {
        workflow::model_files(
            &workflow::parse(
                r#"{"1": {"class_type": "UNETLoader", "inputs": {"unet_name": "u.safetensors"}},
                    "2": {"class_type": "DualCLIPLoader",
                          "inputs": {"clip_name1": "l.safetensors", "clip_name2": "g.safetensors"}},
                    "3": {"class_type": "VAELoader", "inputs": {"vae_name": "v.safetensors"}}}"#,
            )
            .unwrap(),
        )
    }

    #[test]
    fn missing_models_reads_both_combo_schemas() {
        let info: Value = serde_json::from_str(
            r#"{
              "UNETLoader": {"input": {"required": {
                  "unet_name": [["u.safetensors", "other.safetensors"], {}],
                  "weight_dtype": [["default"]]}}},
              "DualCLIPLoader": {"input": {"required": {
                  "clip_name1": ["COMBO", {"options": ["l.safetensors"]}],
                  "clip_name2": ["COMBO", {"options": ["l.safetensors"]}],
                  "type": [["sdxl"]]}}},
              "VAELoader": {"input": {"required": {"vae_name": [["pixel_space"]]}}}
            }"#,
        )
        .unwrap();
        let missing = missing_in(&info, &refs()).unwrap();
        let files: Vec<&str> = missing.iter().map(|m| m.file.as_str()).collect();
        assert_eq!(files, ["g.safetensors", "v.safetensors"]);
        assert_eq!(
            missing[1].message(),
            "ComfyUI is missing v.safetensors. Download it into models/vae."
        );
    }

    #[test]
    fn missing_loader_node_is_an_error() {
        for info in [r#"{}"#, r#"{"UNETLoader": {"input": 5}}"#, r#"[1]"#] {
            let info: Value = serde_json::from_str(info).unwrap();
            let err = missing_in(&info, &refs()).unwrap_err();
            assert!(err.contains("UNETLoader"), "{err}");
        }
    }

    #[test]
    fn missing_models_asks_only_for_the_loaders_used() {
        let url = serve(|r| match r.path.as_str() {
            "/object_info/UNETLoader" => Reply::new(
                200,
                r#"{"UNETLoader": {"input": {"required": {"unet_name": [["u.safetensors"]]}}}}"#,
            ),
            _ => Reply::new(404, "{}"),
        });
        let server = ComfyServer::new(&url).unwrap();
        let wf = workflow::parse(
            r#"{"1": {"class_type": "UNETLoader", "inputs": {"unet_name": "u.safetensors"}}}"#,
        )
        .unwrap();
        assert_eq!(server.missing_models(&wf).unwrap(), []);
    }

    #[test]
    fn upload_sends_multipart_and_reads_the_stored_name() {
        let url = serve(|r| {
            let body = String::from_utf8_lossy(&r.body);
            let ok = r.method == "POST"
                && r.path == "/upload/image"
                && body.contains("name=\"image\"; filename=\"fill.png\"")
                && body.contains("name=\"overwrite\"\r\n\r\ntrue")
                && body.contains("PNGDATA");
            match ok {
                true => Reply::new(
                    200,
                    r#"{"name": "fill.png", "subfolder": "lp", "type": "input"}"#,
                ),
                false => Reply::new(400, "bad"),
            }
        });
        let up = ComfyServer::new(&url)
            .unwrap()
            .upload_png(b"PNGDATA", "fill.png")
            .unwrap();
        assert_eq!(up.image_ref(), "lp/fill.png");
    }

    #[test]
    fn queue_returns_the_prompt_id() {
        assert_eq!(
            parse_queue(
                200,
                r#"{"prompt_id": "abc", "number": 3, "node_errors": {}}"#
            )
            .unwrap(),
            "abc"
        );
        assert!(parse_queue(200, "{}").is_err());
    }

    #[test]
    fn queue_errors_name_the_node_and_reason() {
        let body = r#"{
          "error": {"type": "prompt_outputs_failed_validation",
                    "message": "Prompt outputs failed validation", "details": "", "extra_info": {}},
          "node_errors": {
            "3": {"errors": [{"type": "value_not_in_list",
                              "message": "Value not in list",
                              "details": "unet_name: 'x.safetensors' not in []",
                              "extra_info": {}}],
                  "dependent_outputs": ["10"], "class_type": "UNETLoader"}
          }
        }"#;
        let err = parse_queue(400, body).unwrap_err();
        assert_eq!(
            err,
            "ComfyUI rejected the workflow: Prompt outputs failed validation; \
             node 3 (UNETLoader): Value not in list: unet_name: 'x.safetensors' not in []"
        );
        assert_eq!(
            parse_queue(500, "<html>").unwrap_err(),
            "ComfyUI rejected the workflow: HTTP 500"
        );
        assert!(parse_queue(400, r#"{"node_errors": [1], "error": 7}"#).is_err());
    }

    const DONE: &str = r#"{"p1": {
        "prompt": [0, "p1", {}, {}, ["10"]],
        "outputs": {
          "12": {"images": [{"filename": "preview.png", "subfolder": "", "type": "temp"}]},
          "10": {"images": [{"filename": "fill_00001_.png", "subfolder": "LightPhotos", "type": "output"}]}
        },
        "status": {"status_str": "success", "completed": true, "messages": []}
    }}"#;

    #[test]
    fn history_success_gives_the_output_node_images() {
        assert_eq!(
            parse_history(DONE, "p1", "10").unwrap(),
            History::Done(vec![OutputImage {
                filename: "fill_00001_.png".into(),
                subfolder: "LightPhotos".into(),
                kind: "output".into(),
            }])
        );
        assert_eq!(parse_history("{}", "p1", "10").unwrap(), History::Pending);
        let err = parse_history(DONE, "p1", "99").unwrap_err();
        assert!(err.contains("without saving"), "{err}");
    }

    #[test]
    fn history_status_error_is_readable() {
        let body = r#"{"p1": {"outputs": {}, "status": {"status_str": "error", "completed": false,
          "messages": [["execution_start", {"prompt_id": "p1"}],
                       ["execution_error", {"prompt_id": "p1", "node_id": "8", "node_type": "KSampler",
                         "exception_message": "MPS backend out of memory\n", "exception_type": "RuntimeError"}]]}}}"#;
        assert_eq!(
            parse_history(body, "p1", "10").unwrap_err(),
            "ComfyUI failed in KSampler: MPS backend out of memory"
        );
        let stopped = r#"{"p1": {"status": {"status_str": "error",
          "messages": [["execution_interrupted", {}]]}}}"#;
        assert_eq!(
            parse_history(stopped, "p1", "10").unwrap_err(),
            "ComfyUI stopped the job"
        );
    }

    #[test]
    fn malformed_history_is_an_error_not_a_panic() {
        for body in [
            "",
            "[]",
            "null",
            r#"{"p1": 5}"#,
            "{\"p1\": {\"status\": {\"status_str\": \"error\", \"messages\": 3}}}",
        ] {
            assert!(parse_history(body, "p1", "10").is_err(), "{body:?}");
        }
        let odd = r#"{"p1": {"outputs": {"10": {"images": [{"filename": 3}, "x", {"filename": "ok.png"}]}}}}"#;
        let History::Done(images) = parse_history(odd, "p1", "10").unwrap() else {
            panic!("expected images");
        };
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].kind, "output");
    }

    #[test]
    fn fetch_caps_the_reply() {
        let url = serve(|r| {
            match r
                .path
                .starts_with("/view?filename=a.png&subfolder=s&type=output")
            {
                true => Reply::new(200, "PNG"),
                false => Reply::new(404, ""),
            }
        });
        let server = ComfyServer::new(&url).unwrap();
        let image = OutputImage {
            filename: "a.png".into(),
            subfolder: "s".into(),
            kind: "output".into(),
        };
        assert_eq!(server.fetch(&image).unwrap(), b"PNG");
        let huge =
            serve(|_| Reply::new(200, "").header("content-length", (IMAGE_REPLY + 1).to_string()));
        assert!(ComfyServer::new(&huge).unwrap().fetch(&image).is_err());
    }

    #[test]
    fn clip_cuts_at_a_char_boundary() {
        assert_eq!(clip("ééé", 2), "éé…");
        assert_eq!(clip("ab", 5), "ab");
    }

    /// Runs Generative Fill against a live server. Run it by hand with
    /// `COMFY_URL=http://127.0.0.1:8000 cargo test live_fill -- --ignored`.
    #[test]
    #[ignore]
    fn live_fill() {
        let Ok(url) = std::env::var("COMFY_URL") else {
            eprintln!("COMFY_URL not set; skipping");
            return;
        };
        let feature = super::super::features::FEATURES
            .iter()
            .find(|f| f.id == "generative_fill")
            .unwrap();
        let server = ComfyServer::new(&url).unwrap();
        ComfyServer::probe(&url).unwrap();
        let wf = workflow::parse(feature.workflow).unwrap();
        let missing = server.missing_models(&wf).unwrap();
        assert!(missing.is_empty(), "{:?}", missing);

        let (w, h) = (512u32, 512u32);
        let mut rgba = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let hole = (192..320).contains(&x) && (192..320).contains(&y);
                rgba.extend([
                    (x / 2) as u8,
                    (y / 2) as u8,
                    128,
                    if hole { 0 } else { 255 },
                ]);
            }
        }
        let mut png = Vec::new();
        let mut encoder = png::Encoder::new(&mut png, w, h);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&rgba).unwrap();
        drop(writer);

        let up = server
            .upload_png(&png, "lightphotos_live_test.png")
            .unwrap();
        let injected = workflow::inject(&wf, &up.image_ref(), "a red flower", 1234).unwrap();
        let id = server.queue(&injected.prompt, "lightphotos-test").unwrap();
        let images = server
            .wait(
                &id,
                &injected.output_node,
                Instant::now() + Duration::from_secs(900),
                &AtomicBool::new(false),
            )
            .unwrap();
        let bytes = server.fetch(&images[0]).unwrap();
        assert!(bytes.starts_with(b"\x89PNG"), "got {} bytes", bytes.len());
    }
}
