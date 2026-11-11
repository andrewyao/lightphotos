// SPDX-License-Identifier: MIT OR Apache-2.0

//! Downloads a feature's model file into ComfyUI's models folder.
//!
//! A model is gigabytes, so it streams to `<file>.part` beside its final name
//! instead of going through `paths::write_atomic`, which holds the whole file
//! in memory. The `.part` is renamed into place only after its size and
//! SHA-256 match, and a rename within one directory is atomic, so ComfyUI never
//! sees a half-written model. A `.part` left by a crash, a cancel or a dropped
//! connection is resumed with an HTTP Range request.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use lightphotos::control_auth::hex;
use ring::digest::{Context, SHA256};

use super::features::ModelFile;

const CHUNK: usize = 256 * 1024;

/// Why a download stopped, and whether its `.part` is worth resuming.
enum Stop {
    /// The bytes so far are good: a cancel or a dropped connection.
    Keep(String),
    /// The bytes are wrong or untrustworthy.
    Discard(String),
}

/// Where `model` goes under `models_dir`. Refuses a name that could reach
/// outside its folder.
pub fn final_path(model: &ModelFile, models_dir: &Path) -> Result<PathBuf, String> {
    for name in [model.folder, model.file] {
        let bad = name.is_empty()
            || name.contains("..")
            || name.contains(['/', '\\', ':', '\0'])
            || name == ".";
        if bad {
            return Err(format!("refusing to save a model as {name:?}"));
        }
    }
    Ok(models_dir.join(model.folder).join(model.file))
}

/// Whether `model` is in place with the right size. The hash is checked once,
/// at download time, since rehashing gigabytes on every look is too slow.
pub fn installed(model: &ModelFile, models_dir: &Path) -> bool {
    final_path(model, models_dir)
        .ok()
        .and_then(|p| fs::metadata(p).ok())
        .is_some_and(|m| m.is_file() && m.len() == model.bytes)
}

/// Downloads `model` into `models_dir`, calling `progress(done, total)` as
/// bytes arrive. Setting `cancel` stops it and keeps the `.part` to resume.
pub fn download(
    model: &ModelFile,
    models_dir: &Path,
    progress: impl Fn(u64, u64),
    cancel: &AtomicBool,
) -> Result<(), String> {
    let dest = final_path(model, models_dir)?;
    if installed(model, models_dir) {
        progress(model.bytes, model.bytes);
        return Ok(());
    }
    let dir = models_dir.join(model.folder);
    fs::create_dir_all(&dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let part = dir.join(format!("{}.part", model.file));
    match fetch_into(model, &part, &progress, cancel) {
        Ok(()) => fs::rename(&part, &dest)
            .map_err(|e| format!("could not move {} into place: {e}", model.file)),
        Err(Stop::Keep(why)) => Err(why),
        Err(Stop::Discard(why)) => {
            let _ = fs::remove_file(&part);
            Err(why)
        }
    }
}

fn agent(remaining: u64) -> ureq::Agent {
    // ureq has no idle timeout, only a budget for the whole body, so the
    // budget assumes a floor rate of 256 KiB/s. A stalled download ends, and
    // its `.part` resumes.
    let body_budget = Duration::from_secs(120 + remaining / (256 * 1024));
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        // Hugging Face redirects every file to its CDN. Nothing secret is sent.
        .max_redirects(5)
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(60)))
        .timeout_recv_body(Some(body_budget))
        .build()
        .into()
}

/// Hashes the `.part` already on disk. `None` if there is none, it is longer
/// than the model, or it can't be read.
fn resume_point(model: &ModelFile, part: &Path) -> Option<(u64, Context)> {
    let mut file = File::open(part).ok()?;
    let len = file.metadata().ok()?.len();
    if len == 0 || len > model.bytes {
        return None;
    }
    let mut hasher = Context::new(&SHA256);
    let mut buf = vec![0; CHUNK];
    let mut read = 0u64;
    loop {
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(buf.get(..n)?);
        read = read.saturating_add(n as u64);
    }
    (read == len).then_some((len, hasher))
}

fn fetch_into(
    model: &ModelFile,
    part: &Path,
    progress: &impl Fn(u64, u64),
    cancel: &AtomicBool,
) -> Result<(), Stop> {
    let io = |what: &str, e: std::io::Error| Stop::Keep(format!("{what} {}: {e}", model.file));
    let (mut have, mut hasher) = resume_point(model, part).unwrap_or((0, Context::new(&SHA256)));

    if have < model.bytes {
        let mut request = agent(model.bytes - have).get(model.url);
        if have > 0 {
            request = request.header("range", format!("bytes={have}-"));
        }
        let mut resp = request
            .call()
            .map_err(|e| Stop::Keep(format!("could not download {}: {e}", model.file)))?;
        let length = resp.body().content_length();
        let mut file = match resp.status().as_u16() {
            206 if have > 0 => {
                let range = resp
                    .headers()
                    .get("content-range")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                let resumes_here = range
                    .strip_prefix("bytes ")
                    .and_then(|r| r.split_once('-'))
                    .is_some_and(|(start, _)| start.trim() == have.to_string());
                if !resumes_here || length.is_some_and(|l| l != model.bytes - have) {
                    return Err(Stop::Discard(format!(
                        "the server resumed {} at the wrong place; try again",
                        model.file
                    )));
                }
                OpenOptions::new()
                    .append(true)
                    .open(part)
                    .map_err(|e| io("could not open", e))?
            }
            200 => {
                // A fresh download, or a server that ignored the Range.
                if length.is_some_and(|l| l != model.bytes) {
                    return Err(Stop::Discard(format!(
                        "the server's {} is not the expected size; it may have changed",
                        model.file
                    )));
                }
                have = 0;
                hasher = Context::new(&SHA256);
                File::create(part).map_err(|e| io("could not create", e))?
            }
            // A `.part` the server can't resume from, perhaps a different file.
            416 => {
                return Err(Stop::Discard(format!(
                    "could not resume {}; try again to restart it",
                    model.file
                )));
            }
            status => {
                return Err(Stop::Keep(format!(
                    "the server answered HTTP {status} for {}",
                    model.file
                )));
            }
        };

        let mut reader = resp.body_mut().as_reader();
        let mut buf = vec![0; CHUNK];
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Stop::Keep("download cancelled".into()));
            }
            let n = reader
                .read(&mut buf)
                .map_err(|e| Stop::Keep(format!("the download of {} stopped: {e}", model.file)))?;
            if n == 0 {
                break;
            }
            let chunk = buf.get(..n).unwrap_or_default();
            have = have.saturating_add(n as u64);
            if have > model.bytes {
                return Err(Stop::Discard(format!(
                    "the server sent more of {} than expected",
                    model.file
                )));
            }
            file.write_all(chunk)
                .map_err(|e| io("could not write", e))?;
            hasher.update(chunk);
            progress(have, model.bytes);
        }
        file.sync_all().map_err(|e| io("could not save", e))?;
        if have < model.bytes {
            return Err(Stop::Keep(format!(
                "the download of {} stopped early; try again to resume it",
                model.file
            )));
        }
    }

    match hex(hasher.finish().as_ref()) == model.sha256 {
        true => Ok(()),
        false => Err(Stop::Discard(format!(
            "{} downloaded with the wrong checksum and was deleted; try again",
            model.file
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_server::{serve, Reply};
    use super::*;
    use std::sync::{Arc, Mutex};

    fn payload(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 + i / 251) as u8).collect()
    }

    fn sha(bytes: &[u8]) -> &'static str {
        Box::leak(hex(ring::digest::digest(&SHA256, bytes).as_ref()).into_boxed_str())
    }

    fn model(url: &str, body: &[u8]) -> ModelFile {
        ModelFile {
            folder: "vae",
            file: "m.safetensors",
            url: Box::leak(format!("{url}/m").into_boxed_str()),
            bytes: body.len() as u64,
            sha256: sha(body),
        }
    }

    /// A server for `body` that honors Range when `ranges` is set, and logs
    /// each request's Range header.
    fn file_server(body: Vec<u8>, ranges: bool) -> (String, Arc<Mutex<Vec<Option<String>>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let seen = log.clone();
        let url = serve(move |r| {
            let range = r.header("range").map(str::to_string);
            seen.lock().unwrap().push(range.clone());
            let start = range.filter(|_| ranges).and_then(|r| {
                r.strip_prefix("bytes=")?
                    .strip_suffix('-')?
                    .parse::<usize>()
                    .ok()
            });
            match start {
                Some(s) if s >= body.len() => Reply::new(416, ""),
                Some(s) => Reply::new(206, body[s..].to_vec()).header(
                    "content-range",
                    format!("bytes {s}-{}/{}", body.len() - 1, body.len()),
                ),
                None => Reply::new(200, body.clone()),
            }
        });
        (url, log)
    }

    fn dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "lp-comfy-dl-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn part_of(dir: &Path) -> PathBuf {
        dir.join("vae/m.safetensors.part")
    }

    #[test]
    fn full_download_lands_in_place_and_reruns_skip_the_network() {
        let body = payload(700_000);
        let (url, log) = file_server(body.clone(), true);
        let (m, d) = (model(&url, &body), dir());
        let last = Mutex::new((0, 0));
        download(
            &m,
            &d,
            |a, b| *last.lock().unwrap() = (a, b),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(fs::read(d.join("vae/m.safetensors")).unwrap(), body);
        assert!(!part_of(&d).exists());
        assert_eq!(*last.lock().unwrap(), (700_000, 700_000));
        assert!(installed(&m, &d));
        download(&m, &d, |_, _| {}, &AtomicBool::new(false)).unwrap();
        assert_eq!(
            log.lock().unwrap().len(),
            1,
            "the second call made no request"
        );
    }

    #[test]
    fn resumes_a_partial_part_with_range() {
        let body = payload(600_000);
        let (url, log) = file_server(body.clone(), true);
        let (m, d) = (model(&url, &body), dir());
        fs::create_dir_all(d.join("vae")).unwrap();
        fs::write(part_of(&d), &body[..250_000]).unwrap();
        download(&m, &d, |_, _| {}, &AtomicBool::new(false)).unwrap();
        assert_eq!(fs::read(d.join("vae/m.safetensors")).unwrap(), body);
        assert_eq!(*log.lock().unwrap(), [Some("bytes=250000-".to_string())]);
    }

    #[test]
    fn a_server_ignoring_range_restarts_from_zero() {
        let body = payload(400_000);
        let (url, log) = file_server(body.clone(), false);
        let (m, d) = (model(&url, &body), dir());
        fs::create_dir_all(d.join("vae")).unwrap();
        fs::write(part_of(&d), &body[..100_000]).unwrap();
        download(&m, &d, |_, _| {}, &AtomicBool::new(false)).unwrap();
        assert_eq!(fs::read(d.join("vae/m.safetensors")).unwrap(), body);
        assert_eq!(log.lock().unwrap().len(), 1);
    }

    #[test]
    fn hash_mismatch_deletes_the_part() {
        let body = payload(300_000);
        let mut wrong = body.clone();
        wrong[1234] ^= 1;
        let (url, _) = file_server(wrong, true);
        let (m, d) = (model(&url, &body), dir());
        let err = download(&m, &d, |_, _| {}, &AtomicBool::new(false)).unwrap_err();
        assert!(err.contains("checksum"), "{err}");
        assert!(!part_of(&d).exists());
        assert!(!d.join("vae/m.safetensors").exists());
    }

    #[test]
    fn a_corrupt_part_is_caught_by_the_hash_after_resume() {
        let body = payload(300_000);
        let (url, _) = file_server(body.clone(), true);
        let (m, d) = (model(&url, &body), dir());
        fs::create_dir_all(d.join("vae")).unwrap();
        fs::write(part_of(&d), vec![0u8; 100_000]).unwrap();
        assert!(download(&m, &d, |_, _| {}, &AtomicBool::new(false)).is_err());
        assert!(!part_of(&d).exists(), "the bad part is gone");
        download(&m, &d, |_, _| {}, &AtomicBool::new(false)).unwrap();
        assert_eq!(fs::read(d.join("vae/m.safetensors")).unwrap(), body);
    }

    #[test]
    fn cancel_mid_stream_keeps_the_part_to_resume() {
        let body = payload(4_000_000);
        let (url, _) = file_server(body.clone(), true);
        let (m, d) = (model(&url, &body), dir());
        let cancel = AtomicBool::new(false);
        let err = download(
            &m,
            &d,
            |_, _| cancel.store(true, Ordering::Relaxed),
            &cancel,
        )
        .unwrap_err();
        assert!(err.contains("cancelled"), "{err}");
        let kept = fs::metadata(part_of(&d)).unwrap().len();
        assert!(kept > 0 && kept < body.len() as u64, "kept {kept}");
        assert!(!d.join("vae/m.safetensors").exists());
        download(&m, &d, |_, _| {}, &AtomicBool::new(false)).unwrap();
        assert_eq!(fs::read(d.join("vae/m.safetensors")).unwrap(), body);
    }

    #[test]
    fn hostile_content_length_is_refused() {
        let body = payload(50_000);
        let url =
            serve(|_| Reply::new(200, payload(50_000)).header("content-length", "999999999999999"));
        let (m, d) = (model(&url, &body), dir());
        let err = download(&m, &d, |_, _| {}, &AtomicBool::new(false)).unwrap_err();
        assert!(err.contains("size"), "{err}");
        assert!(!part_of(&d).exists());
        assert!(!d.join("vae/m.safetensors").exists());
    }

    #[test]
    fn a_body_longer_than_the_model_is_refused_while_streaming() {
        let body = payload(50_000);
        let url = serve(|_| Reply {
            no_length: true,
            ..Reply::new(200, payload(80_000))
        });
        let (m, d) = (model(&url, &body), dir());
        let err = download(&m, &d, |_, _| {}, &AtomicBool::new(false)).unwrap_err();
        assert!(err.contains("more"), "{err}");
        assert!(!part_of(&d).exists());
    }

    #[test]
    fn names_that_escape_the_folder_are_refused() {
        let d = dir();
        for (folder, file) in [
            ("../x", "m"),
            ("vae", "../../m"),
            ("vae", "a/b"),
            ("vae", "a\\b"),
            ("", "m"),
            ("vae", ".."),
        ] {
            let m = ModelFile {
                folder,
                file,
                url: "http://127.0.0.1:1/",
                bytes: 1,
                sha256: "00",
            };
            assert!(
                download(&m, &d, |_, _| {}, &AtomicBool::new(false)).is_err(),
                "{folder}/{file}"
            );
            assert!(!installed(&m, &d));
        }
    }
}
