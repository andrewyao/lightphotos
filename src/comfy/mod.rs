// SPDX-License-Identifier: MIT OR Apache-2.0

//! LightPhotos' side of a local ComfyUI server: a blocking HTTP client, the
//! code that fills our inputs into a workflow, the table of AI features and
//! the model files each needs, and the downloader for those files. Native
//! only; the browser build renders finished fills but can't make them.

pub mod client;
pub mod download;
pub mod features;
pub mod workflow;

#[cfg(test)]
pub(crate) mod test_server;
