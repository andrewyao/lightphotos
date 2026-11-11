// SPDX-License-Identifier: MIT OR Apache-2.0

//! What the app knows about the user's ComfyUI: the Settings ▸ ComfyUI
//! choices, whether its server answers, which model files are in place, and
//! the model downloads. Everything that touches the network or the models
//! folder runs on its own thread and is polled from the frame loop. Nothing
//! here runs until ComfyUI is enabled in Settings, and the server is only
//! asked while a tool that needs it is open.
//!
//! Native only: the browser build can't reach a local server.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::comfy::client::{ComfyServer, DEFAULT_URLS, LOST};
use crate::comfy::download;
use crate::comfy::features::{self, ComfyFeature, ModelFile, FEATURES};
use crate::comfy::workflow;

const ENABLED_PREF: &str = "comfy_enabled";
const SERVER_PREF: &str = "comfy_server";
const MODELS_PREF: &str = "comfy_models_dir";
const FEATURES_PREF: &str = "comfy_features";

/// How often a server that didn't answer is asked again.
const RETRY_DOWN: Duration = Duration::from_secs(5);
/// How often a server that answered is checked to still be there.
const RECHECK_UP: Duration = Duration::from_secs(30);

/// Whether the ComfyUI server answers, as of the last probe.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ComfyStatus {
    /// Not asked yet, or a setting changed since.
    Unknown,
    /// The first probe is out.
    Checking,
    /// It answers and lists every model the enabled features load.
    Up { url: String },
    /// It doesn't answer, or can't run a feature; `why` says what to do.
    /// `url` is empty when no server was found at the default ports.
    Down { url: String, why: String },
}

/// Whether a feature's controls can be used.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Gate {
    /// ComfyUI is off in Settings: the controls don't show.
    Hidden,
    /// The controls show disabled; the text is the first thing to fix.
    Blocked(String),
    /// The server hasn't answered yet: asked, or not asked because no tool
    /// that needs it is open.
    Checking,
    /// Ready, at this server.
    Ready(String),
}

/// How one model file stands, for its row in Settings.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ModelState {
    Installed,
    Missing,
    Queued,
    Downloading { done: u64, total: u64 },
    Failed(String),
}

/// What a probe found.
struct Probed {
    /// The server asked, or the default that answered; empty when none did.
    url: String,
    /// True when `url` came from the default ports, to be remembered.
    found: bool,
    outcome: Result<(), ProbeFail>,
}

enum ProbeFail {
    /// Nothing answered at these addresses.
    NotRunning(Vec<String>),
    /// The server answered and said why it can't run a feature.
    Other(String),
}

struct RunningDownload {
    model: &'static ModelFile,
    cancel: Arc<AtomicBool>,
    done: Arc<AtomicU64>,
    result: Receiver<Result<(), String>>,
}

pub(crate) struct Comfy {
    enabled: bool,
    /// The server URL from Settings; empty looks at the default ports and
    /// remembers the one that answers.
    server: String,
    /// The Server field's text while it is being typed.
    server_draft: Option<String>,
    models_dir: Option<PathBuf>,
    /// Ids of the features turned on.
    features: BTreeSet<&'static str>,
    status: ComfyStatus,
    probe: Option<Receiver<Probed>>,
    probed_at: Option<Instant>,
    /// The model files found in place by the last scan or download.
    installed: HashSet<&'static str>,
    scan: Option<Receiver<Option<Scanned>>>,
    queue: VecDeque<&'static ModelFile>,
    running: Option<RunningDownload>,
    failed: HashMap<&'static str, String>,
}

/// `host:port` for a message, without the scheme.
pub(super) fn bare(url: &str) -> &str {
    url.split_once("://").map_or(url, |(_, rest)| rest)
}

/// A setting saved to the prefs file. A test must never write the
/// developer's own settings.
fn save_pref(key: &str, value: &str) {
    if cfg!(test) {
        return;
    }
    if let Err(e) = crate::persist::prefs::save(key, value) {
        eprintln!("[comfy] could not save {key}: {e}");
    }
}

fn load_pref(key: &str) -> Option<String> {
    if cfg!(test) {
        return None;
    }
    crate::persist::prefs::load(key)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Where the picker starts: ComfyUI Desktop's models folder when one is
/// found, else `~/Documents`.
pub(crate) fn suggested_models_dir() -> Option<PathBuf> {
    detect_models_dir().or_else(|| {
        let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
        Some(PathBuf::from(home).join("Documents"))
    })
}

/// ComfyUI Desktop's models folder, from its own settings. `None` when no
/// install is found. Reads small files, so it runs on the scan thread.
fn detect_models_dir() -> Option<PathBuf> {
    // The platform's app-support folder, the parent of our own.
    let support = crate::persist::prefs::config_dir()?.parent()?.to_path_buf();
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    detect_models_dir_in(&support, home.as_deref().map(Path::new))
}

/// The most of a ComfyUI settings file read. They are a few KB.
const MAX_COMFY_SETTINGS_BYTES: usize = 1 << 20;

/// Looks, in order, at the current Comfy Desktop's `settings.json`
/// (`modelsDirs`), the older ComfyUI Desktop's `config.json` (`basePath`,
/// whose `models` folder it used) and `~/Documents/ComfyUI/models`. The first
/// that names an existing folder wins.
fn detect_models_dir_in(support: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let json = |file: PathBuf| -> Option<serde_json::Value> {
        let bytes = crate::paths::read_capped(&file, MAX_COMFY_SETTINGS_BYTES).ok()??;
        serde_json::from_slice(&bytes).ok()
    };
    let desktop = json(support.join("Comfy Desktop").join("settings.json")).and_then(|s| {
        let first = s.get("modelsDirs")?.as_array()?.first()?.as_str()?;
        Some(PathBuf::from(first))
    });
    let legacy = || {
        json(support.join("ComfyUI").join("config.json")).and_then(|c| {
            let base = c.get("basePath")?.as_str()?;
            Some(PathBuf::from(base).join("models"))
        })
    };
    let documents = || home.map(|h| h.join("Documents").join("ComfyUI").join("models"));
    [desktop, legacy(), documents()]
        .into_iter()
        .flatten()
        .find(|dir| dir.is_absolute() && dir.is_dir())
}

/// A finished scan of the models folder.
struct Scanned {
    dir: PathBuf,
    /// The folder came from [`detect_models_dir`], not the user's choice.
    detected: bool,
    found: Vec<&'static str>,
}

fn probe(server: Option<String>, wanted: Vec<&'static ComfyFeature>) -> Probed {
    let (url, found) = match server {
        Some(url) => (url, false),
        None => match ComfyServer::find_default() {
            Some(url) => (url, true),
            None => {
                let looked = DEFAULT_URLS.iter().map(|u| u.to_string()).collect();
                return Probed {
                    url: String::new(),
                    found: false,
                    outcome: Err(ProbeFail::NotRunning(looked)),
                };
            }
        },
    };
    let outcome = (|| {
        if !found {
            ComfyServer::probe(&url).map_err(|_| ProbeFail::NotRunning(vec![url.clone()]))?;
        }
        let server = ComfyServer::new(&url).map_err(ProbeFail::Other)?;
        for feature in wanted {
            let nodes = workflow::parse(feature.workflow).map_err(ProbeFail::Other)?;
            let missing = server.missing_models(&nodes).map_err(|why| match why {
                why if why == LOST => ProbeFail::NotRunning(vec![url.clone()]),
                why => ProbeFail::Other(why),
            })?;
            if let Some(first) = missing.first() {
                return Err(ProbeFail::Other(first.message()));
            }
        }
        Ok(())
    })();
    Probed {
        url,
        found,
        outcome,
    }
}

impl Comfy {
    /// The saved settings. Reads the prefs file and nothing else: no probe
    /// and no look at the models folder happens at startup.
    pub(crate) fn load() -> Comfy {
        let features = load_pref(FEATURES_PREF)
            .map(|saved| {
                FEATURES
                    .iter()
                    .filter(|f| saved.split(',').any(|id| id.trim() == f.id))
                    .map(|f| f.id)
                    .collect()
            })
            .unwrap_or_default();
        Comfy {
            enabled: load_pref(ENABLED_PREF).is_some_and(|v| v == "on"),
            server: load_pref(SERVER_PREF).unwrap_or_default(),
            server_draft: None,
            models_dir: load_pref(MODELS_PREF).map(PathBuf::from),
            features,
            status: ComfyStatus::Unknown,
            probe: None,
            probed_at: None,
            installed: HashSet::new(),
            scan: None,
            queue: VecDeque::new(),
            running: None,
            failed: HashMap::new(),
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    /// What the Server field shows: the text being typed, else the URL in
    /// use.
    pub(crate) fn server_text(&self) -> &str {
        self.server_draft.as_deref().unwrap_or(&self.server)
    }

    pub(crate) fn edit_server(&mut self, text: String) {
        self.server_draft = Some(text);
    }

    /// The Server field lost focus: its text becomes the server, unless it
    /// isn't an http URL, which is reported and dropped.
    pub(crate) fn commit_server(&mut self) -> Result<(), String> {
        match self.server_draft.take() {
            Some(text) => self.set_server(&text),
            None => Ok(()),
        }
    }

    pub(crate) fn models_dir(&self) -> Option<&Path> {
        self.models_dir.as_deref()
    }

    pub(crate) fn feature_on(&self, id: &str) -> bool {
        self.features.contains(id)
    }

    #[cfg(test)]
    pub(crate) fn status(&self) -> &ComfyStatus {
        &self.status
    }

    /// A setting the status depends on changed: ask again from scratch.
    fn forget_status(&mut self) {
        self.status = ComfyStatus::Unknown;
        self.probe = None;
        self.probed_at = None;
    }

    pub(crate) fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
        save_pref(ENABLED_PREF, if on { "on" } else { "off" });
        self.forget_status();
        if on {
            self.scan_models();
        } else {
            self.cancel_all_downloads();
        }
    }

    /// The server to use; empty goes back to looking at the default ports.
    /// `Err` for text that isn't an http URL, which is not saved.
    pub(crate) fn set_server(&mut self, url: &str) -> Result<(), String> {
        let url = match url.trim() {
            "" => String::new(),
            typed => crate::comfy::client::normalize_url(typed)?,
        };
        if url != self.server {
            save_pref(SERVER_PREF, &url);
            self.server = url;
            self.forget_status();
        }
        Ok(())
    }

    pub(crate) fn set_models_dir(&mut self, dir: PathBuf) {
        save_pref(MODELS_PREF, &dir.to_string_lossy());
        self.cancel_all_downloads();
        self.failed.clear();
        self.models_dir = Some(dir);
        self.scan_models();
    }

    pub(crate) fn set_feature(&mut self, id: &str, on: bool) {
        let Some(feature) = features::feature(id) else {
            return;
        };
        if on {
            self.features.insert(feature.id);
        } else {
            self.features.remove(feature.id);
        }
        let saved: Vec<&str> = self.features.iter().copied().collect();
        save_pref(FEATURES_PREF, &saved.join(","));
        self.forget_status();
    }

    /// Whether `feature`'s controls can be used, and if not, the first thing
    /// in the way.
    pub(crate) fn gate(&self, feature: &ComfyFeature) -> Gate {
        let t = crate::i18n::t();
        if !self.enabled {
            return Gate::Hidden;
        }
        if !self.features.contains(feature.id) {
            return Gate::Blocked(t.comfy_blocked_feature.replace("{feature}", feature.name));
        }
        if self.models_dir.is_none() {
            return Gate::Blocked(t.comfy_blocked_folder.replace("{feature}", feature.name));
        }
        if let Some(model) = feature
            .models
            .iter()
            .find(|m| !self.installed.contains(m.file))
        {
            if self.scan.is_some() {
                return Gate::Checking;
            }
            return Gate::Blocked(
                t.comfy_blocked_model
                    .replace("{feature}", feature.name)
                    .replace("{file}", model.file),
            );
        }
        match &self.status {
            ComfyStatus::Unknown | ComfyStatus::Checking => Gate::Checking,
            ComfyStatus::Down { why, .. } => Gate::Blocked(why.clone()),
            ComfyStatus::Up { url } => Gate::Ready(url.clone()),
        }
    }

    /// Asks the server now, as entering a tool that needs it does, and
    /// looks at the models folder again.
    pub(crate) fn check_now(&mut self) {
        if !self.enabled {
            return;
        }
        self.probed_at = None;
        self.scan_models();
        self.start_probe();
    }

    /// When the next probe is due while a tool that needs the server is
    /// open; `None` while one is out or ComfyUI is off.
    pub(crate) fn probe_due(&self) -> Option<Instant> {
        if !self.enabled || self.probe.is_some() {
            return None;
        }
        let every = match self.status {
            ComfyStatus::Up { .. } => RECHECK_UP,
            _ => RETRY_DOWN,
        };
        Some(self.probed_at.map_or_else(Instant::now, |at| at + every))
    }

    /// Starts a probe if one is due.
    pub(crate) fn probe_if_due(&mut self) {
        if self.probe_due().is_some_and(|at| at <= Instant::now()) {
            self.start_probe();
        }
    }

    fn start_probe(&mut self) {
        if !self.enabled || self.probe.is_some() {
            return;
        }
        let server = Some(self.server.clone()).filter(|s| !s.is_empty());
        let wanted: Vec<&'static ComfyFeature> = FEATURES
            .iter()
            .filter(|f| self.features.contains(f.id))
            .collect();
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("comfy-probe".into())
            .spawn(move || {
                let _ = tx.send(probe(server, wanted));
            });
        match spawned {
            Ok(_) => {
                self.probe = Some(rx);
                if self.status == ComfyStatus::Unknown {
                    self.status = ComfyStatus::Checking;
                }
            }
            Err(e) => self.land_probe(Probed {
                url: self.server.clone(),
                found: false,
                outcome: Err(ProbeFail::Other(format!("could not check ComfyUI: {e}"))),
            }),
        }
    }

    fn land_probe(&mut self, probed: Probed) {
        self.probe = None;
        self.probed_at = Some(Instant::now());
        if probed.found && self.server.is_empty() {
            save_pref(SERVER_PREF, &probed.url);
            self.server = probed.url.clone();
        }
        self.status = match probed.outcome {
            Ok(()) => ComfyStatus::Up { url: probed.url },
            Err(ProbeFail::Other(why)) => ComfyStatus::Down {
                url: probed.url,
                why,
            },
            Err(ProbeFail::NotRunning(looked)) => {
                let t = crate::i18n::t();
                let looked: Vec<&str> = looked.iter().map(|u| bare(u)).collect();
                ComfyStatus::Down {
                    url: probed.url,
                    why: t
                        .comfy_not_running
                        .replace("{where}", &looked.join(t.comfy_and)),
                }
            }
        };
    }

    /// Looks in the models folder, off the frame, for every feature's files.
    pub(crate) fn scan_models(&mut self) {
        if !self.enabled {
            self.installed.clear();
            return;
        }
        let chosen = self.models_dir.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("comfy-models".into())
            .spawn(move || {
                let (dir, detected) = match chosen {
                    Some(dir) => (dir, false),
                    None => match detect_models_dir() {
                        Some(dir) => (dir, true),
                        None => {
                            let _ = tx.send(None);
                            return;
                        }
                    },
                };
                let found = FEATURES
                    .iter()
                    .flat_map(|f| f.models)
                    .filter(|m| download::installed(m, &dir))
                    .map(|m| m.file)
                    .collect();
                let _ = tx.send(Some(Scanned {
                    dir,
                    detected,
                    found,
                }));
            });
        match spawned {
            Ok(_) => self.scan = Some(rx),
            Err(e) => eprintln!("[comfy] could not look in the models folder: {e}"),
        }
    }

    pub(crate) fn model_state(&self, model: &ModelFile) -> ModelState {
        if let Some(run) = self.running.as_ref().filter(|r| r.model.file == model.file) {
            return ModelState::Downloading {
                done: run.done.load(Ordering::Relaxed),
                total: model.bytes,
            };
        }
        if self.queue.iter().any(|m| m.file == model.file) {
            return ModelState::Queued;
        }
        if self.installed.contains(model.file) {
            return ModelState::Installed;
        }
        match self.failed.get(model.file) {
            Some(why) => ModelState::Failed(why.clone()),
            None => ModelState::Missing,
        }
    }

    /// Queues the model file named `file` for download. One runs at a time.
    pub(crate) fn download(&mut self, file: &str) {
        let Some(model) = FEATURES
            .iter()
            .flat_map(|f| f.models)
            .find(|m| m.file == file)
        else {
            return;
        };
        let startable = matches!(
            self.model_state(model),
            ModelState::Missing | ModelState::Failed(_)
        );
        if !self.enabled || !startable {
            return;
        }
        self.failed.remove(model.file);
        self.queue.push_back(model);
        self.start_next_download();
    }

    /// Stops the download of `file`, running or queued. A running one keeps
    /// its `.part`, so a later download resumes.
    pub(crate) fn cancel_download(&mut self, file: &str) {
        self.queue.retain(|m| m.file != file);
        if let Some(run) = self.running.as_ref().filter(|r| r.model.file == file) {
            run.cancel.store(true, Ordering::Relaxed);
        }
    }

    fn cancel_all_downloads(&mut self) {
        self.queue.clear();
        if let Some(run) = &self.running {
            run.cancel.store(true, Ordering::Relaxed);
        }
    }

    fn start_next_download(&mut self) {
        if self.running.is_some() {
            return;
        }
        let Some(dir) = self.models_dir.clone() else {
            self.queue.clear();
            return;
        };
        let Some(model) = self.queue.pop_front() else {
            return;
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicU64::new(0));
        let (tx, rx) = std::sync::mpsc::channel();
        let (stop, progress) = (Arc::clone(&cancel), Arc::clone(&done));
        let spawned = std::thread::Builder::new()
            .name("comfy-download".into())
            .spawn(move || {
                let report = |have: u64, _total: u64| progress.store(have, Ordering::Relaxed);
                let _ = tx.send(download::download(model, &dir, report, &stop));
            });
        match spawned {
            Ok(_) => {
                self.running = Some(RunningDownload {
                    model,
                    cancel,
                    done,
                    result: rx,
                })
            }
            Err(e) => {
                self.failed
                    .insert(model.file, format!("could not start the download: {e}"));
            }
        }
    }

    /// Each frame: lands a finished probe, scan or download. True while any
    /// is still out, so the frame loop keeps polling.
    pub(crate) fn poll(&mut self) -> bool {
        if let Some(rx) = &self.probe {
            match rx.try_recv() {
                Ok(probed) => self.land_probe(probed),
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => self.land_probe(Probed {
                    url: self.server.clone(),
                    found: false,
                    outcome: Err(ProbeFail::Other("the ComfyUI check stopped".into())),
                }),
            }
        }
        if let Some(rx) = &self.scan {
            match rx.try_recv() {
                Ok(scanned) => {
                    self.scan = None;
                    match scanned {
                        // A choice made while the scan ran wins over a found folder.
                        Some(s) if s.detected && self.models_dir.is_some() => self.scan_models(),
                        Some(s) => {
                            if s.detected {
                                save_pref(MODELS_PREF, &s.dir.to_string_lossy());
                                self.models_dir = Some(s.dir);
                            }
                            self.installed = s.found.into_iter().collect();
                        }
                        None => self.installed.clear(),
                    }
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => self.scan = None,
            }
        }
        if let Some(run) = &self.running {
            let landed = match run.result.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err("the download stopped".into())),
            };
            if let Some(result) = landed {
                let file = run.model.file;
                let cancelled = run.cancel.load(Ordering::Relaxed);
                self.running = None;
                match result {
                    Ok(()) => {
                        self.installed.insert(file);
                        // ComfyUI lists a new file only when asked again.
                        self.probed_at = None;
                    }
                    Err(_) if cancelled => {}
                    Err(why) => {
                        self.failed.insert(file, why);
                    }
                }
                self.start_next_download();
            }
        }
        self.probe.is_some() || self.scan.is_some() || self.running.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comfy::features::GENERATIVE_FILL;
    use crate::comfy::test_server::{serve, Reply};

    fn fill() -> &'static ComfyFeature {
        features::feature(GENERATIVE_FILL).unwrap()
    }

    fn fresh_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("lp-comfy-detect-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn finds_the_models_folder_comfy_desktop_is_set_to() {
        let root = fresh_dir("desktop");
        let models = root.join("ComfyUI-Shared").join("models");
        std::fs::create_dir_all(&models).unwrap();
        let support = root.join("support");
        std::fs::create_dir_all(support.join("Comfy Desktop")).unwrap();
        let settings = serde_json::json!({ "modelsDirs": [models] });
        std::fs::write(
            support.join("Comfy Desktop").join("settings.json"),
            settings.to_string(),
        )
        .unwrap();
        // The Documents folder exists too, and loses to the app's own setting.
        std::fs::create_dir_all(root.join("Documents/ComfyUI/models")).unwrap();
        assert_eq!(detect_models_dir_in(&support, Some(&root)), Some(models));
    }

    #[test]
    fn falls_back_past_missing_or_hostile_settings() {
        let root = fresh_dir("fallback");
        let support = root.join("support");
        std::fs::create_dir_all(support.join("Comfy Desktop")).unwrap();
        std::fs::create_dir_all(support.join("ComfyUI")).unwrap();
        // A folder that doesn't exist, a relative path and garbage are all skipped.
        std::fs::write(
            support.join("Comfy Desktop").join("settings.json"),
            r#"{"modelsDirs":["/no/such/lightphotos/models"]}"#,
        )
        .unwrap();
        std::fs::write(support.join("ComfyUI").join("config.json"), "{not json").unwrap();
        assert_eq!(detect_models_dir_in(&support, Some(&root)), None);

        std::fs::write(
            support.join("Comfy Desktop").join("settings.json"),
            r#"{"modelsDirs":["relative/models"]}"#,
        )
        .unwrap();
        let legacy = root.join("legacy");
        std::fs::create_dir_all(legacy.join("models")).unwrap();
        let config = serde_json::json!({ "basePath": legacy });
        std::fs::write(
            support.join("ComfyUI").join("config.json"),
            config.to_string(),
        )
        .unwrap();
        assert_eq!(
            detect_models_dir_in(&support, Some(&root)),
            Some(legacy.join("models"))
        );

        std::fs::write(
            support.join("ComfyUI").join("config.json"),
            r#"{"basePath":7}"#,
        )
        .unwrap();
        let documents = root.join("Documents/ComfyUI/models");
        std::fs::create_dir_all(&documents).unwrap();
        assert_eq!(detect_models_dir_in(&support, Some(&root)), Some(documents));
        assert_eq!(detect_models_dir_in(&support, None), None);
    }

    fn settle(comfy: &mut Comfy) {
        let start = Instant::now();
        while comfy.poll() {
            assert!(start.elapsed().as_secs() < 20, "never settled");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// A models folder holding files of the right sizes, sparse so they
    /// cost no disk.
    fn models_dir(tag: &str, features: &[&ComfyFeature]) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("lightphotos-comfy-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for m in features.iter().flat_map(|f| f.models) {
            std::fs::create_dir_all(dir.join(m.folder)).unwrap();
            let file = std::fs::File::create(dir.join(m.folder).join(m.file)).unwrap();
            file.set_len(m.bytes).unwrap();
        }
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A server that answers `/system_stats` and lists `models` in every
    /// loader's options.
    fn comfy_server(models: &'static [&'static str]) -> String {
        serve(move |r| {
            if r.path == "/system_stats" {
                return Reply::new(200, r#"{"system":{"comfyui_version":"0.3"}}"#);
            }
            let Some(class) = r.path.strip_prefix("/object_info/") else {
                return Reply::new(404, "");
            };
            let inputs: serde_json::Map<String, serde_json::Value> =
                ["unet_name", "clip_name1", "clip_name2", "vae_name"]
                    .iter()
                    .map(|i| (i.to_string(), serde_json::json!([models, {}])))
                    .collect();
            Reply::new(
                200,
                serde_json::json!({ class: { "input": { "required": inputs } } }).to_string(),
            )
        })
    }

    const ALL: &[&str] = &[
        "sdxl_inpainting_0.1_unet.fp16.safetensors",
        "sdxl_inpainting_0.1_clip_l.fp16.safetensors",
        "sdxl_inpainting_0.1_clip_g.fp16.safetensors",
        "sdxl_inpainting_0.1_vae.fp16.safetensors",
    ];

    #[test]
    fn the_gate_names_the_first_thing_in_the_way_in_order() {
        let mut comfy = Comfy::load();
        assert_eq!(comfy.gate(fill()), Gate::Hidden, "off by default");
        assert!(comfy.probe_due().is_none(), "and never probed while off");
        comfy.check_now();
        assert!(!comfy.poll(), "check_now does nothing while off");

        let blocked = |comfy: &Comfy| match comfy.gate(fill()) {
            Gate::Blocked(why) => why,
            other => panic!("expected blocked, got {other:?}"),
        };
        comfy.set_enabled(true);
        assert!(blocked(&comfy).contains("Settings"), "{}", blocked(&comfy));
        assert!(blocked(&comfy).contains("turn on"), "the feature is off");

        comfy.set_feature(GENERATIVE_FILL, true);
        assert!(
            blocked(&comfy).contains("models folder"),
            "{}",
            blocked(&comfy)
        );

        let empty = models_dir("gate-empty", &[]);
        comfy.set_models_dir(empty.clone());
        settle(&mut comfy);
        let why = blocked(&comfy);
        assert!(why.contains(fill().models[0].file), "{why}");

        let full = models_dir("gate-full", &[fill()]);
        comfy.set_models_dir(full.clone());
        settle(&mut comfy);
        assert_eq!(comfy.gate(fill()), Gate::Checking, "not asked yet");

        // Nothing listens on a port just closed.
        let closed = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", l.local_addr().unwrap())
        };
        comfy.set_server(&closed).unwrap();
        comfy.check_now();
        settle(&mut comfy);
        let why = blocked(&comfy);
        assert!(why.starts_with("Start ComfyUI"), "{why}");
        assert!(why.contains(bare(&closed)), "{why}");
        assert!(matches!(comfy.status(), ComfyStatus::Down { .. }));

        comfy.set_server(&comfy_server(&ALL[1..])).unwrap();
        comfy.check_now();
        settle(&mut comfy);
        let why = blocked(&comfy);
        assert!(
            why.contains("ComfyUI is missing") && why.contains(ALL[0]),
            "{why}"
        );

        let up = comfy_server(ALL);
        comfy.set_server(&up).unwrap();
        comfy.check_now();
        settle(&mut comfy);
        assert_eq!(comfy.gate(fill()), Gate::Ready(up));
        assert!(
            comfy.probe_due().unwrap() > Instant::now() + Duration::from_secs(20),
            "a server that is up is checked again in 30 s, not 5"
        );

        assert!(comfy.set_server("ftp://x").is_err());
        comfy.set_enabled(false);
        assert_eq!(comfy.gate(fill()), Gate::Hidden);
        for dir in [empty, full] {
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn a_server_that_is_down_is_asked_again_within_five_seconds() {
        let mut comfy = Comfy::load();
        comfy.set_enabled(true);
        comfy.set_server("http://127.0.0.1:9").unwrap();
        comfy.check_now();
        assert_eq!(*comfy.status(), ComfyStatus::Checking);
        assert!(comfy.probe_due().is_none(), "one probe at a time");
        settle(&mut comfy);
        let due = comfy.probe_due().unwrap();
        assert!(due > Instant::now() && due <= Instant::now() + RETRY_DOWN);
    }

    #[test]
    fn one_download_runs_at_a_time_and_cancel_clears_the_rest() {
        let mut comfy = Comfy::load();
        comfy.set_enabled(true);
        let dir = models_dir("downloads", &[]);
        comfy.set_models_dir(dir.clone());
        settle(&mut comfy);
        let [a, b, ..] = fill().models else {
            panic!("the feature has several models");
        };
        // Queued by hand, so no request leaves the machine.
        comfy.queue.push_back(a);
        comfy.queue.push_back(b);
        assert_eq!(comfy.model_state(a), ModelState::Queued);
        comfy.cancel_download(a.file);
        assert_eq!(comfy.model_state(a), ModelState::Missing);
        assert_eq!(comfy.model_state(b), ModelState::Queued);
        comfy.set_enabled(false);
        assert_eq!(comfy.model_state(b), ModelState::Missing);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
