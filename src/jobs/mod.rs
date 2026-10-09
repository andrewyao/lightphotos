// SPDX-License-Identifier: MIT OR Apache-2.0
//! Work off the UI thread: the decode loader and its caches, the on-disk
//! thumbnail cache, and the scoring pool.

pub(crate) mod cache_limits;
pub(crate) mod loader;
pub(crate) mod score;
pub(crate) mod thumbnail;
