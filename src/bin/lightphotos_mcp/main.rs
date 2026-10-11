// SPDX-License-Identifier: MIT OR Apache-2.0
// Non-test code never panics, and every `unsafe` block says why it is sound.
// See CLAUDE.md → Rust rules.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable,
    clippy::undocumented_unsafe_blocks
)]

//! `lightphotos-mcp`: an MCP server over stdio that lets an agent cull, edit
//! and look at photos. Two modes:
//!
//! ```sh
//! lightphotos-mcp --folder ~/Pictures/shoot   # headless: edits sidecars, renders through the lib
//! lightphotos-mcp --attach 7777               # drives `lightphotos --control 7777`
//! ```
//!
//! Attached, the token comes from `LIGHTPHOTOS_CONTROL_TOKEN` or `--token`.
//! The tools are a small fixed set; `list_commands` describes every command
//! `run_command` takes. stdout carries the protocol only, so every log line
//! goes to stderr. Desktop only.

#[cfg(not(target_arch = "wasm32"))]
mod attached;
#[cfg(not(target_arch = "wasm32"))]
mod headless;
#[cfg(not(target_arch = "wasm32"))]
mod rpc;
#[cfg(not(target_arch = "wasm32"))]
mod tools;

#[cfg(target_arch = "wasm32")]
fn main() {}

#[cfg(not(target_arch = "wasm32"))]
fn main() -> std::process::ExitCode {
    let mode = match tools::Mode::from_args(std::env::args().skip(1)) {
        Ok(mode) => mode,
        Err(e) => {
            eprintln!("lightphotos-mcp: {e}");
            eprintln!("usage: lightphotos-mcp --folder <dir>");
            eprintln!("       lightphotos-mcp --attach <port> [--token <token>]");
            return std::process::ExitCode::from(2);
        }
    };
    match rpc::serve(std::io::stdin().lock(), std::io::stdout().lock(), &mode) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("lightphotos-mcp: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
