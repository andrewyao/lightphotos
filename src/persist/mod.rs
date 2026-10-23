// SPDX-License-Identifier: MIT OR Apache-2.0
//! What outlives the session: the per-folder store and its sidecar writes,
//! photo groups, cached signals, preferences, presets, and the Immich key.

pub(crate) mod catalog;
pub(crate) mod folder_store;
pub(crate) mod groups;
pub(crate) mod lr_preset;
pub(crate) mod prefs;
pub(crate) mod preset_folders;
pub(crate) mod presets;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod secret;
pub(crate) mod signalcache;
