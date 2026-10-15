// SPDX-License-Identifier: MIT OR Apache-2.0

//! The `--control` protocol: requests another process sends to drive the
//! running app, and the app's answers. A transport (`tcp`) parses and checks
//! each request, then hands it to the main thread through the event loop
//! (`UserEvent::Control`) with a [`Reply`] to answer on. The app holds
//! `wait` and `screenshot` until its frame loop can answer them
//! (`app/control.rs`).

use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

use crate::command::{Command, CommandError};
use crate::renderer::RgbFrame;

pub(crate) mod tcp;

/// How long a transport waits for the main thread to answer.
pub(crate) const REPLY_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const DEFAULT_WAIT: Duration = Duration::from_secs(10);
pub(crate) const MAX_WAIT: Duration = Duration::from_secs(60);

/// Where the main thread sends its [`Answer`]. Dropping it unanswered tells
/// the transport nothing ran.
pub(crate) type Reply = std::sync::mpsc::Sender<Answer>;

/// A checked request for the main thread.
#[derive(Debug, Clone)]
pub(crate) enum Op {
    Run(Box<Command>),
    Commands,
    State,
    Screenshot,
    /// Answer once the app settles, or with `timeout` after this long.
    Wait(Duration),
}

/// An `Op` on its way to the main thread.
#[derive(Debug, Clone)]
pub(crate) struct Request {
    pub(crate) op: Op,
    /// When the connection stops waiting. The app drops a request it reaches
    /// later without running it, so a request the client gave up on can't
    /// run behind its retry.
    pub(crate) expires: Instant,
}

/// The main thread's answer to one `Op`.
pub(crate) type Answer = Result<Payload, Failure>;

pub(crate) enum Payload {
    Json(Value),
    /// Encoded on the connection thread, so the main thread never waits on it.
    Frame(RgbFrame),
}

/// An error reply. `error` is a [`CommandError`] or a [`ChannelError`], both
/// tagged by their `error` field.
#[derive(Debug)]
pub(crate) struct Failure {
    error: Value,
    message: String,
}

impl From<CommandError> for Failure {
    fn from(e: CommandError) -> Failure {
        Failure {
            error: serde_json::to_value(&e).unwrap_or_default(),
            message: e.to_string(),
        }
    }
}

impl From<ChannelError> for Failure {
    fn from(e: ChannelError) -> Failure {
        Failure {
            error: serde_json::to_value(&e).unwrap_or_default(),
            message: e.to_string(),
        }
    }
}

/// What went wrong with a request before or after its command ran.
#[derive(Serialize, Debug)]
#[serde(tag = "error", rename_all = "snake_case")]
pub(crate) enum ChannelError {
    BadJson { reason: String },
    BadToken,
    UnknownOp { op: String },
    TooLong { limit: usize },
    TooManyConnections { limit: usize },
    IdleTimeout,
    Timeout,
    AppClosed,
    Failed { reason: String },
}

impl std::fmt::Display for ChannelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChannelError::BadJson { reason } => write!(
                f,
                "not a request: {reason}; send one JSON object per line with `token` and `op`"
            ),
            ChannelError::BadToken => {
                f.write_str("wrong or missing `token`; use the one the app printed at startup")
            }
            ChannelError::UnknownOp { op } => write!(
                f,
                "no op `{op}`; use run, commands, state, screenshot or wait"
            ),
            ChannelError::TooLong { limit } => {
                write!(
                    f,
                    "request longer than {limit} bytes; closing the connection"
                )
            }
            ChannelError::TooManyConnections { limit } => write!(
                f,
                "{limit} connections are open already; close one and retry"
            ),
            ChannelError::IdleTimeout => f.write_str(
                "no request for a while; closing the connection, open a new one to go on",
            ),
            ChannelError::Timeout => f.write_str("timed out: the app is still busy"),
            ChannelError::AppClosed => f.write_str("the app is closing"),
            ChannelError::Failed { reason } => f.write_str(reason),
        }
    }
}
