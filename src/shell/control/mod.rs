// SPDX-License-Identifier: MIT OR Apache-2.0

//! The control protocol: requests that another process or an in-browser
//! agent sends to drive the running app, and the app's answers. A transport
//! (`tcp` natively, `web::web_mcp` in the browser) checks each request, then
//! hands it to the main thread through the event loop (`UserEvent::Control`)
//! with a [`Reply`] to answer on. The app holds `wait` and `screenshot` until
//! its frame loop can answer them (`app/control.rs`).

use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use web_time::Instant;

use crate::command::{Command, CommandError};
#[cfg(not(target_arch = "wasm32"))]
use crate::renderer::RgbFrame;

#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod tcp;

/// How long a transport waits for the main thread to answer.
pub(crate) const REPLY_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const DEFAULT_WAIT: Duration = Duration::from_secs(10);

/// Where the main thread sends its [`Answer`]. Dropping it unanswered tells
/// the transport nothing ran.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) type Reply = std::sync::mpsc::Sender<Answer>;
#[cfg(target_arch = "wasm32")]
pub(crate) use crate::web::web_mcp::Reply;

/// A checked request for the main thread.
#[derive(Debug, Clone)]
pub(crate) enum Op {
    Run(Box<Command>),
    Commands,
    State,
    /// Every photo the Grid shows, in its order.
    List,
    #[cfg(not(target_arch = "wasm32"))]
    Screenshot,
    /// Answer once the app settles, or with `timeout` after this long.
    Wait(Duration),
}

/// An `Op` on its way to the main thread.
#[derive(Debug, Clone)]
pub(crate) struct Request {
    pub(crate) op: Op,
    /// When the transport stops waiting. The app drops a request it reaches
    /// later without running it, so a request the client gave up on can't
    /// run behind its retry.
    pub(crate) expires: Instant,
}

impl Request {
    /// `op`, expiring once a transport has waited `REPLY_TIMEOUT` for the
    /// answer, plus a `wait`'s own timeout.
    pub(crate) fn new(op: Op) -> Request {
        let patience = match &op {
            Op::Wait(timeout) => *timeout + REPLY_TIMEOUT,
            _ => REPLY_TIMEOUT,
        };
        let expires = Instant::now() + patience;
        Request { op, expires }
    }
}

/// The main thread's answer to one `Op`.
pub(crate) type Answer = Result<Payload, Failure>;

pub(crate) enum Payload {
    Json(Value),
    /// Encoded on the connection thread, so the main thread never waits on it.
    #[cfg(not(target_arch = "wasm32"))]
    Frame(RgbFrame),
}

/// An error reply. `error` is a [`CommandError`] or a [`ChannelError`], both
/// tagged by their `error` field.
#[derive(Debug)]
pub(crate) struct Failure {
    // A WebMCP agent sees only a rejected Error's message.
    #[cfg_attr(
        target_arch = "wasm32",
        allow(dead_code, reason = "a WebMCP agent sees only the message")
    )]
    error: Value,
    pub(crate) message: String,
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
// Most variants are about TCP lines and connections, which the browser has none of.
#[cfg_attr(
    target_arch = "wasm32",
    allow(dead_code, reason = "the browser has no TCP lines or connections")
)]
pub(crate) enum ChannelError {
    BadJson {
        reason: String,
    },
    /// A wrong `mac` in `auth`. The connection closes after it.
    BadToken,
    /// A request before the connection signed in.
    AuthRequired,
    /// A request with a `token`, as clients before the sign-in sent.
    RawTokenRefused,
    UnknownOp {
        op: String,
    },
    TooLong {
        limit: usize,
    },
    TooManyConnections {
        limit: usize,
    },
    IdleTimeout,
    /// A connection that did not sign in within the time allowed.
    SignInTimeout,
    Timeout,
    AppClosed,
    Failed {
        reason: String,
    },
}

impl std::fmt::Display for ChannelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChannelError::BadJson { reason } => write!(
                f,
                "not a request: {reason}; send one JSON object per line with an `op`"
            ),
            ChannelError::BadToken => f.write_str(
                "wrong `mac`: use the token the app printed at startup. The connection \
                 closes; open a new one and send `hello` again",
            ),
            ChannelError::AuthRequired => f.write_str(
                "not signed in: send {\"op\":\"hello\",\"nonce\":\"…\"}, then \
                 {\"op\":\"auth\",\"mac\":\"…\"}",
            ),
            ChannelError::RawTokenRefused => f.write_str(
                "this app no longer takes the token in a request, so nothing on the port can \
                 learn it. Update lightphotos-mcp (or your client) to this app's version: it \
                 signs in with `hello` and `auth`",
            ),
            ChannelError::UnknownOp { op } => write!(
                f,
                "no op `{op}`; use hello, auth, run, commands, state, list, screenshot or wait"
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
            ChannelError::SignInTimeout => f.write_str(
                "no sign-in in time; closing the connection, open a new one and send `hello` \
                 at once",
            ),
            ChannelError::Timeout => f.write_str("timed out: the app is still busy"),
            ChannelError::AppClosed => f.write_str("the app is closing"),
            ChannelError::Failed { reason } => f.write_str(reason),
        }
    }
}
