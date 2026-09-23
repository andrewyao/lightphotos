// SPDX-License-Identifier: GPL-3.0-or-later

//! Where the Immich API key is kept. macOS files it in the login Keychain
//! under one account per server. Elsewhere it goes in a file under the config
//! directory that only the user can read, and the export form says so.

#[cfg(target_os = "macos")]
const SERVICE: &str = "app.lightphotos.immich";

#[cfg(target_os = "macos")]
pub fn load_api_key(server: &str) -> Option<String> {
    let bytes = security_framework::passwords::get_generic_password(SERVICE, server).ok()?;
    String::from_utf8(bytes).ok()
}

#[cfg(target_os = "macos")]
pub fn save_api_key(server: &str, key: &str) -> Result<(), String> {
    security_framework::passwords::set_generic_password(SERVICE, server, key.as_bytes())
        .map_err(|e| format!("Keychain: {e}"))
}

#[cfg(target_os = "macos")]
pub fn delete_api_key(server: &str) {
    let _ = security_framework::passwords::delete_generic_password(SERVICE, server);
}

#[cfg(not(target_os = "macos"))]
const KEY_FILE: &str = "immich_api_key";

/// One key at a time off macOS, so `server` only matters for the Keychain.
#[cfg(not(target_os = "macos"))]
pub fn load_api_key(_server: &str) -> Option<String> {
    crate::prefs::load(KEY_FILE).filter(|k| !k.is_empty())
}

#[cfg(not(target_os = "macos"))]
pub fn save_api_key(_server: &str, key: &str) -> Result<(), String> {
    use std::io::Write;
    let dir = crate::prefs::config_dir().ok_or_else(|| "no config directory".to_string())?;
    let path = dir.join(KEY_FILE);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    // Created private rather than chmod'ed after, so the key is never readable.
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    std::fs::create_dir_all(&dir)
        .and_then(|()| opts.open(&path))
        .and_then(|mut f| f.write_all(key.as_bytes()))
        .map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(not(target_os = "macos"))]
pub fn delete_api_key(_server: &str) {
    if let Some(dir) = crate::prefs::config_dir() {
        let _ = std::fs::remove_file(dir.join(KEY_FILE));
    }
}
