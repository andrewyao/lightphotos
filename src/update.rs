// SPDX-License-Identifier: MIT OR Apache-2.0

//! The "a newer version is available" cue. A release build is stamped with its
//! version (`LIGHTPHOTOS_VERSION`, set by `release.yml` from the tag). At most
//! once a day it asks GitHub for the latest release, and when that is newer
//! the header and Settings link to the downloads page. A build without a
//! stamp (every dev build), the browser and the App Store build never check
//! and show nothing.

use std::sync::mpsc::{Receiver, TryRecvError};

use crate::i18n::Lang;
use crate::persist::prefs;

/// Whether this build may check for updates and show the cue at all.
pub(crate) const ENABLED: bool = cfg!(all(not(target_arch = "wasm32"), not(feature = "app-store")));

const CHECK_PREF: &str = "update_check";
const LAST_CHECKED_PREF: &str = "update_last_checked";
const LATEST_PREF: &str = "update_latest";
const CHECK_EVERY_SECS: u64 = 24 * 60 * 60;
/// Pretends GitHub's latest release is this version and skips the request, so
/// `--drive` and a manual check can see the cue without a network.
const LATEST_OVERRIDE_ENV: &str = "LIGHTPHOTOS_UPDATE_LATEST";
#[cfg(not(target_arch = "wasm32"))]
const RELEASES_URL: &str = "https://api.github.com/repos/andrewyao/lightphotos/releases/latest";
/// A release's JSON is a few KB; this only stops a hostile reply from filling memory.
#[cfg(not(target_arch = "wasm32"))]
const MAX_REPLY_BYTES: u64 = 1024 * 1024;

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Version(u32, u32, u32);

impl Version {
    /// `1.2.3` or `v1.2.3`. Anything else (a pre-release suffix, a missing
    /// part, a sign, a number past `u32`) is `None`, so a malformed tag never
    /// reads as newer.
    pub(crate) fn parse(text: &str) -> Option<Version> {
        let text = text.trim();
        let text = text.strip_prefix('v').unwrap_or(text);
        let mut parts = text.split('.');
        let mut next = || {
            let part = parts.next()?;
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            part.parse::<u32>().ok()
        };
        let version = Version(next()?, next()?, next()?);
        parts.next().is_none().then_some(version)
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// The downloads page in `lang`. The site's folder names are its own
/// (`zh-cn`, `zh-tw`, and none for English), not the app's `Lang::code`.
pub(crate) fn downloads_url(lang: Lang) -> String {
    let folder = match lang {
        Lang::En => "",
        Lang::ZhHans => "zh-cn/",
        Lang::ZhHant => "zh-tw/",
        Lang::Ja => "ja/",
        Lang::Ko => "ko/",
        Lang::De => "de/",
        Lang::Fr => "fr/",
    };
    format!("https://lightphotos.app/{folder}downloads")
}

#[cfg(not(target_arch = "wasm32"))]
fn parse_release(body: &str) -> Result<Version, String> {
    #[derive(serde::Deserialize)]
    struct Release {
        tag_name: String,
    }
    let release: Release = serde_json::from_str(body).map_err(|e| format!("reply: {e}"))?;
    Version::parse(&release.tag_name)
        .ok_or_else(|| format!("unexpected tag {:?}", release.tag_name))
}

/// True when `last_checked` (unix seconds) is missing, a day old, or in the
/// future, which a clock set back would otherwise freeze for good.
fn is_due(last_checked: Option<u64>, now: u64) -> bool {
    last_checked.is_none_or(|last| last > now || now - last >= CHECK_EVERY_SECS)
}

fn now_secs() -> u64 {
    web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(not(target_arch = "wasm32"))]
fn fetch_latest(current: Version) -> Result<Version, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(10)))
        .user_agent(format!("LightPhotos/{current}"))
        .build()
        .into();
    let mut reply = agent
        .get(RELEASES_URL)
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| e.to_string())?;
    let body = reply
        .body_mut()
        .with_config()
        .limit(MAX_REPLY_BYTES)
        .read_to_string()
        .map_err(|e| format!("read reply: {e}"))?;
    parse_release(&body)
}

pub(crate) struct UpdateState {
    current: Option<Version>,
    latest: Option<Version>,
    auto_check: bool,
    pending: Option<Receiver<Result<Version, String>>>,
}

impl UpdateState {
    /// Reads the saved settings and cached answer. Tests start inert: they
    /// must not read the developer's own settings.
    pub(crate) fn new() -> Self {
        let current = if ENABLED && !cfg!(test) {
            option_env!("LIGHTPHOTOS_VERSION").and_then(Version::parse)
        } else {
            None
        };
        let mut state = UpdateState {
            current,
            latest: None,
            auto_check: true,
            pending: None,
        };
        if current.is_some() {
            state.auto_check = prefs::load(CHECK_PREF).is_none_or(|v| v.trim() != "off");
            state.latest = prefs::load(LATEST_PREF).and_then(|v| Version::parse(&v));
        }
        state
    }

    #[cfg(test)]
    pub(crate) fn with(current: Option<Version>, latest: Option<Version>) -> Self {
        UpdateState {
            current,
            latest,
            auto_check: true,
            pending: None,
        }
    }

    /// The running version, when this build is stamped and may show it.
    pub(crate) fn current(&self) -> Option<Version> {
        self.current
    }

    pub(crate) fn auto_check(&self) -> bool {
        self.auto_check
    }

    /// The newer version to point the user at, if one is known.
    pub(crate) fn available(&self) -> Option<Version> {
        if !self.auto_check {
            return None;
        }
        let current = self.current?;
        self.latest.filter(|&latest| latest > current)
    }

    pub(crate) fn set_auto_check(&mut self, on: bool) {
        self.auto_check = on;
        if !cfg!(test) {
            if let Err(e) = prefs::save(CHECK_PREF, if on { "on" } else { "off" }) {
                eprintln!("[update] could not save the setting: {e}");
            }
        }
        self.start_if_due();
    }

    /// Ask GitHub, unless the last answer is under a day old. Call at launch
    /// and when the checkbox goes on.
    pub(crate) fn start_if_due(&mut self) {
        let Some(current) = self.current else { return };
        if !self.auto_check || self.pending.is_some() {
            return;
        }
        if let Some(latest) = std::env::var(LATEST_OVERRIDE_ENV)
            .ok()
            .and_then(|v| Version::parse(&v))
        {
            self.latest = Some(latest);
            return;
        }
        if cfg!(test) {
            return;
        }
        let last_checked = prefs::load(LAST_CHECKED_PREF).and_then(|v| v.trim().parse().ok());
        if !is_due(last_checked, now_secs()) {
            return;
        }
        self.spawn_check(current);
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn spawn_check(&mut self, current: Version) {
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("update-check".into())
            .spawn(move || {
                let _ = tx.send(fetch_latest(current));
            });
        match spawned {
            Ok(_) => self.pending = Some(rx),
            Err(e) => eprintln!("[update] could not start the check: {e}"),
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn spawn_check(&mut self, _current: Version) {}

    /// Take a finished check. Returns `(running, changed)`: `running` keeps the
    /// frame loop polling, `changed` asks for a redraw.
    pub(crate) fn poll(&mut self) -> (bool, bool) {
        let Some(rx) = &self.pending else {
            return (false, false);
        };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return (true, false),
            Err(TryRecvError::Disconnected) => Err("the check stopped".into()),
        };
        self.pending = None;
        match result {
            Ok(latest) => {
                let changed = self.latest != Some(latest);
                self.latest = Some(latest);
                if !cfg!(test) {
                    if let Err(e) = prefs::save(LATEST_PREF, &latest.to_string())
                        .and_then(|()| prefs::save(LAST_CHECKED_PREF, &now_secs().to_string()))
                    {
                        eprintln!("[update] could not save the answer: {e}");
                    }
                }
                (false, changed)
            }
            Err(e) => {
                eprintln!("[update] check failed: {e}");
                (false, false)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn v(text: &str) -> Version {
        Version::parse(text).expect("a valid version")
    }

    #[test]
    fn parses_plain_and_v_prefixed_versions() {
        assert_eq!(Version::parse("0.1.37"), Some(Version(0, 1, 37)));
        assert_eq!(Version::parse("v1.2.3"), Some(Version(1, 2, 3)));
        assert_eq!(Version::parse(" v1.2.3\n"), Some(Version(1, 2, 3)));
    }

    #[test]
    fn rejects_anything_that_is_not_three_plain_numbers() {
        for bad in [
            "",
            "v",
            "1",
            "1.2",
            "v1.0",
            "1.2.3.4",
            "1.2.3-rc1",
            "1.2.x",
            "+1.2.3",
            "-1.2.3",
            "1..3",
            "1.2.",
            "99999999999.0.0",
            "latest",
            "vv1.2.3",
            "1.2.3 beta",
        ] {
            assert_eq!(Version::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn orders_numerically_not_as_text() {
        assert!(v("0.1.38") > v("0.1.37"));
        assert!(v("0.1.100") > v("0.1.99"));
        assert!(v("0.2.0") > v("0.1.99"));
        assert!(v("1.0.0") > v("0.99.99"));
        assert!(v("0.1.37") == v("v0.1.37"));
    }

    #[test]
    fn downloads_url_uses_the_sites_folder_for_every_language() {
        let urls: Vec<_> = Lang::ALL.map(downloads_url).into();
        assert_eq!(
            urls,
            [
                "https://lightphotos.app/downloads",
                "https://lightphotos.app/zh-cn/downloads",
                "https://lightphotos.app/zh-tw/downloads",
                "https://lightphotos.app/ja/downloads",
                "https://lightphotos.app/ko/downloads",
                "https://lightphotos.app/de/downloads",
                "https://lightphotos.app/fr/downloads",
            ]
        );
    }

    #[test]
    fn reads_the_tag_from_a_release_and_survives_bad_replies() {
        assert_eq!(
            parse_release(r#"{"tag_name":"v0.1.38","name":"x","assets":[]}"#),
            Ok(v("0.1.38"))
        );
        for bad in [
            "",
            "{",
            "[]",
            "{}",
            r#"{"tag_name":null}"#,
            r#"{"tag_name":"nightly"}"#,
            r#"{"tag_name":"v1.2.3-rc1"}"#,
            r#"{"tag_name":"v1.2.3"#,
        ] {
            assert!(parse_release(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_check_is_due_after_a_day_or_when_the_clock_went_back() {
        let day = CHECK_EVERY_SECS;
        assert!(is_due(None, 1_000_000));
        assert!(!is_due(Some(1_000_000), 1_000_000));
        assert!(!is_due(Some(1_000_000), 1_000_000 + day - 1));
        assert!(is_due(Some(1_000_000), 1_000_000 + day));
        assert!(is_due(Some(2_000_000), 1_000_000));
    }

    #[test]
    fn only_a_newer_release_is_available() {
        let current = Some(v("0.1.37"));
        assert_eq!(
            UpdateState::with(current, Some(v("0.1.38"))).available(),
            Some(v("0.1.38"))
        );
        assert_eq!(
            UpdateState::with(current, Some(v("0.1.37"))).available(),
            None
        );
        assert_eq!(
            UpdateState::with(current, Some(v("0.1.36"))).available(),
            None
        );
        assert_eq!(UpdateState::with(current, None).available(), None);
        assert_eq!(UpdateState::with(None, Some(v("9.9.9"))).available(), None);
    }

    #[test]
    fn turning_checks_off_hides_a_known_update() {
        let mut state = UpdateState::with(Some(v("0.1.37")), Some(v("0.1.38")));
        state.set_auto_check(false);
        assert_eq!(state.available(), None);
        state.set_auto_check(true);
        assert_eq!(state.available(), Some(v("0.1.38")));
    }

    #[test]
    fn a_finished_check_replaces_the_known_version() {
        let mut state = UpdateState::with(Some(v("0.1.37")), None);
        let (tx, rx) = mpsc::channel();
        state.pending = Some(rx);
        assert_eq!(state.poll(), (true, false));
        tx.send(Ok(v("0.1.38"))).expect("the receiver is alive");
        assert_eq!(state.poll(), (false, true));
        assert_eq!(state.available(), Some(v("0.1.38")));
    }

    #[test]
    fn a_failed_check_changes_nothing() {
        let mut state = UpdateState::with(Some(v("0.1.37")), Some(v("0.1.38")));
        let (tx, rx) = mpsc::channel();
        state.pending = Some(rx);
        tx.send(Err("offline".into()))
            .expect("the receiver is alive");
        assert_eq!(state.poll(), (false, false));
        assert_eq!(state.available(), Some(v("0.1.38")));
        let (tx, rx) = mpsc::channel::<Result<Version, String>>();
        state.pending = Some(rx);
        drop(tx);
        assert_eq!(state.poll(), (false, false));
    }
}
