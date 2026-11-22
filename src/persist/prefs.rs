// SPDX-License-Identifier: MIT OR Apache-2.0

//! Global app settings, the state that belongs to the install rather than to
//! a photo folder. Native keeps one file per key under the OS's config
//! directory, the browser one `localStorage` entry per key.
//!
//! Values are raw strings, not typed values, so a caller can tell a value it
//! cannot parse from one that is missing or unreadable. `presets.rs` needs
//! that distinction to avoid overwriting a corrupt but recoverable library.

#[cfg(not(target_arch = "wasm32"))]
use std::ffi::OsStr;
#[cfg(not(target_arch = "wasm32"))]
use std::path::{Path, PathBuf};

/// Which platform's config-directory convention to follow. A parameter of
/// [`config_dir_from`] rather than a `cfg!` inside it, so every platform's
/// path is testable from one host.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Layout {
    Mac,
    Windows,
    /// The XDG base-directory spec, used everywhere else.
    Xdg,
}

#[cfg(not(target_arch = "wasm32"))]
impl Layout {
    const fn host() -> Layout {
        if cfg!(target_os = "macos") {
            Layout::Mac
        } else if cfg!(windows) {
            Layout::Windows
        } else {
            Layout::Xdg
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
static CONFIG_DIR_OVERRIDE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn override_config_dir(dir: PathBuf) {
    let _ = CONFIG_DIR_OVERRIDE.set(dir);
}

/// The directory holding this install's settings files.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = CONFIG_DIR_OVERRIDE.get() {
        return Some(dir.clone());
    }
    config_dir_from(
        Layout::host(),
        std::env::var_os("HOME").as_deref(),
        std::env::var_os("APPDATA").as_deref(),
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
    )
}

/// The path derivation, with the three environment variables injected, so a
/// test never reads the developer's own environment.
#[cfg(not(target_arch = "wasm32"))]
fn config_dir_from(
    layout: Layout,
    home: Option<&OsStr>,
    appdata: Option<&OsStr>,
    xdg: Option<&OsStr>,
) -> Option<PathBuf> {
    Some(match layout {
        Layout::Mac => PathBuf::from(home?).join("Library/Application Support/LightPhotos"),
        Layout::Windows => PathBuf::from(appdata?).join("LightPhotos"),
        Layout::Xdg => xdg
            .map(PathBuf::from)
            .or_else(|| home.map(|h| PathBuf::from(h).join(".config")))?
            .join("lightphotos"),
    })
}

#[cfg(not(target_arch = "wasm32"))]
pub fn load(key: &str) -> Option<String> {
    load_at(&config_dir()?.join(key))
}

#[cfg(not(target_arch = "wasm32"))]
pub fn save(key: &str, value: &str) -> Result<(), String> {
    let dir = config_dir().ok_or_else(|| "no config directory".to_string())?;
    save_at(&dir.join(key), value, false)
}

/// [`save`] that flushes to disk before returning, for user-authored data
/// with no second copy (the presets library). Not for the per-frame session
/// file: a disk barrier at key-repeat rate would stutter the UI.
#[cfg(not(target_arch = "wasm32"))]
pub fn save_durable(key: &str, value: &str) -> Result<(), String> {
    let dir = config_dir().ok_or_else(|| "no config directory".to_string())?;
    save_at(&dir.join(key), value, true)
}

/// Read a setting, telling "not there yet" (`Ok(None)`) from "there but
/// unreadable" (`Err`): a permission error, I/O error, a directory in its
/// place, or bytes that are not UTF-8. A caller that would overwrite the
/// file on a miss needs the distinction, so it does not mistake a damaged
/// file for an empty one and replace it. See [`load`] for the lossy form.
#[cfg(not(target_arch = "wasm32"))]
pub fn load_checked(key: &str) -> Result<Option<String>, String> {
    let dir = config_dir().ok_or_else(|| "no config directory".to_string())?;
    load_at_checked(&dir.join(key))
}

#[cfg(not(target_arch = "wasm32"))]
fn load_at(path: &Path) -> Option<String> {
    load_at_checked(path).ok().flatten()
}

#[cfg(not(target_arch = "wasm32"))]
fn load_at_checked(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read(path) {
        Ok(bytes) => String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| format!("{}: not valid UTF-8", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn save_at(path: &Path, value: &str, durable: bool) -> Result<(), String> {
    let write = if durable {
        crate::paths::write_atomic_in_dir_durable
    } else {
        crate::paths::write_atomic_in_dir
    };
    write(path, value.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(target_arch = "wasm32")]
fn storage_key(key: &str) -> String {
    format!("lightphotos.{key}")
}

#[cfg(target_arch = "wasm32")]
fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}

#[cfg(target_arch = "wasm32")]
pub fn load(key: &str) -> Option<String> {
    load_checked(key).ok().flatten()
}

/// Read a setting, telling an absent key (`Ok(None)`) from `localStorage`
/// being unavailable or throwing (`Err`), so a caller that would overwrite on
/// a miss does not replace a value it merely failed to read.
#[cfg(target_arch = "wasm32")]
pub fn load_checked(key: &str) -> Result<Option<String>, String> {
    let storage = storage().ok_or_else(|| "no localStorage".to_string())?;
    storage
        .get_item(&storage_key(key))
        .map_err(|e| format!("localStorage: {e:?}"))
}

/// `localStorage` has no separate durability step; this is [`save`].
#[cfg(target_arch = "wasm32")]
pub fn save_durable(key: &str, value: &str) -> Result<(), String> {
    save(key, value)
}

/// `set_item` throws `QuotaExceededError` rather than truncating, so the
/// failure has to reach the caller.
#[cfg(target_arch = "wasm32")]
pub fn save(key: &str, value: &str) -> Result<(), String> {
    let storage = storage().ok_or_else(|| "no localStorage".to_string())?;
    storage
        .set_item(&storage_key(key), value)
        .map_err(|e| format!("localStorage: {e:?}"))
}

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod tests {
    use super::*;

    #[test]
    fn macos_uses_application_support_under_home() {
        assert_eq!(
            config_dir_from(
                Layout::Mac,
                Some(OsStr::new("/Users/x")),
                None,
                Some(OsStr::new("/ignored"))
            ),
            Some(PathBuf::from(
                "/Users/x/Library/Application Support/LightPhotos"
            ))
        );
        assert_eq!(config_dir_from(Layout::Mac, None, None, None), None);
    }

    #[test]
    fn windows_uses_appdata() {
        assert_eq!(
            config_dir_from(
                Layout::Windows,
                Some(OsStr::new("/home/x")),
                Some(OsStr::new("C:\\Users\\x\\AppData\\Roaming")),
                None
            ),
            Some(PathBuf::from("C:\\Users\\x\\AppData\\Roaming").join("LightPhotos"))
        );
        assert_eq!(config_dir_from(Layout::Windows, None, None, None), None);
    }

    #[test]
    fn xdg_config_home_wins_over_the_home_fallback() {
        assert_eq!(
            config_dir_from(
                Layout::Xdg,
                Some(OsStr::new("/home/x")),
                None,
                Some(OsStr::new("/home/x/.conf"))
            ),
            Some(PathBuf::from("/home/x/.conf/lightphotos"))
        );
    }

    #[test]
    fn xdg_falls_back_to_dot_config_under_home() {
        assert_eq!(
            config_dir_from(Layout::Xdg, Some(OsStr::new("/home/x")), None, None),
            Some(PathBuf::from("/home/x/.config/lightphotos"))
        );
        assert_eq!(config_dir_from(Layout::Xdg, None, None, None), None);
    }

    #[test]
    fn save_at_creates_the_directory_and_load_at_reads_it_back() {
        let dir = std::env::temp_dir().join(format!("lp-prefs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("language");

        assert_eq!(load_at(&path), None, "nothing stored yet");
        save_at(&path, "zh", false).unwrap();
        assert_eq!(load_at(&path), Some("zh".to_string()));
        save_at(&path, "en", false).unwrap();
        assert_eq!(load_at(&path), Some("en".to_string()), "a write replaces");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_at_leaves_no_temp_file_behind() {
        let dir = std::env::temp_dir().join(format!("lp-prefs-tmp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("presets");

        save_at(&path, "first", false).unwrap();
        save_at(&path, "second", false).unwrap();

        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, [OsStr::new("presets")], "only the setting remains");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_at_checked_tells_missing_from_unreadable() {
        let dir = std::env::temp_dir().join(format!("lp-prefs-chk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let missing = dir.join("absent");
        assert_eq!(
            load_at_checked(&missing),
            Ok(None),
            "a missing file is None"
        );

        let good = dir.join("good");
        save_at(&good, "zh", false).unwrap();
        assert_eq!(load_at_checked(&good), Ok(Some("zh".to_string())));

        // Invalid UTF-8 is an error, not an empty miss. The lossy `load_at`
        // used to turn this into `None`, which let a caller overwrite it.
        let latin1 = dir.join("latin1");
        std::fs::write(&latin1, [0xff, 0xfe, 0x00]).unwrap();
        assert!(load_at_checked(&latin1).is_err(), "non-UTF-8 is an error");
        assert_eq!(load_at(&latin1), None, "the lossy form still hides it");

        // A directory where a file is expected is unreadable, not missing.
        let as_dir = dir.join("adir");
        std::fs::create_dir_all(&as_dir).unwrap();
        assert!(load_at_checked(&as_dir).is_err(), "a directory is an error");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_at_reports_a_path_it_cannot_write() {
        let dir = std::env::temp_dir().join(format!("lp-prefs-err-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();

        let err = save_at(&blocker.join("language"), "en", false).unwrap_err();
        assert!(err.contains("blocker"), "the error names the path: {err}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
