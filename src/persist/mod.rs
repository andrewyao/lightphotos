// SPDX-License-Identifier: MIT OR Apache-2.0
//! What outlives the session: the per-folder catalog and its sidecar writes,
//! photo groups, cached signals, preferences, presets, and the Immich key.

pub(crate) mod catalog;
pub(crate) mod groups;
// The importer is native-only, so the browser build compiles the parser with
// no caller until a wasm file picker exists.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub(crate) mod lr_preset;
pub(crate) mod prefs;
pub(crate) mod presets;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod secret;
pub(crate) mod signalcache;
