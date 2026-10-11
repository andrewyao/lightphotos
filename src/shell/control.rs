// SPDX-License-Identifier: MIT OR Apache-2.0

//! `--control <port>`: a loopback channel that lets another local process
//! drive the running app. Off unless the flag is given, and bound to
//! 127.0.0.1 only.
//!
//! Each TCP connection carries one JSON request per line and gets one JSON
//! reply per line:
//!
//! ```text
//! {"token": "…", "op": "run", "command": {"id": "photo.rate", "params": {"stars": 3}}}
//! {"token": "…", "op": "commands" | "state" | "screenshot"}
//! {"token": "…", "op": "wait", "timeout_ms": 10000}
//!
//! {"ok": true, "result": …}
//! {"ok": false, "error": {"error": "bad_token", …}, "message": "…"}
//! ```
//!
//! A connection thread parses and checks each line, then hands the request
//! to the main thread through the event loop (`UserEvent::Control`) and waits
//! for the answer on a channel. The app holds `wait` and `screenshot` until
//! its frame loop can answer them (`app/control.rs`).

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};
use winit::event_loop::EventLoopProxy;

use crate::command::{Command, CommandError};
use crate::renderer::RgbFrame;
use crate::shell::macos_delegate::UserEvent;

/// The longest request line. A longer one ends the connection.
const MAX_LINE: usize = 1 << 20;
const MAX_CONNECTIONS: usize = 4;
const MIN_TOKEN_LEN: usize = 16;
const TOKEN_ENV: &str = "LIGHTPHOTOS_CONTROL_TOKEN";
/// How long a connection waits for the main thread to answer.
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a connection may go without sending a line before it closes,
/// so idle clients can't hold every slot.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const DEFAULT_WAIT: Duration = Duration::from_secs(10);
const MAX_WAIT: Duration = Duration::from_secs(60);

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

/// Splits `--control <port>` out of the arguments. Returns the port, if the
/// flag is there, and the other arguments in order.
pub(crate) fn split_args(
    args: impl IntoIterator<Item = String>,
) -> Result<(Option<u16>, Vec<String>), String> {
    let mut port = None;
    let mut rest = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--control" {
            let value = args.next().unwrap_or_default();
            let parsed = value
                .parse()
                .map_err(|_| format!("--control takes a port from 0 to 65535, not `{value}`"))?;
            port = Some(parsed);
        } else {
            rest.push(arg);
        }
    }
    Ok((port, rest))
}

/// Starts the channel on 127.0.0.1:`port` (0 picks a free one) and prints
/// the port and token to stderr.
pub(crate) fn start(port: u16, proxy: EventLoopProxy<UserEvent>) -> Result<(), String> {
    let token = token_from_env()?;
    let port = listen(port, token.clone(), IDLE_TIMEOUT, move |request, reply| {
        proxy.send_event(UserEvent::Control(request, reply)).is_ok()
    })?;
    eprintln!("[control] listening on 127.0.0.1:{port} token {token}");
    Ok(())
}

fn token_from_env() -> Result<String, String> {
    match std::env::var(TOKEN_ENV) {
        Ok(token) => check_token(token),
        Err(std::env::VarError::NotPresent) => new_token(),
        Err(std::env::VarError::NotUnicode(_)) => Err(format!("{TOKEN_ENV} is not valid text")),
    }
}

fn check_token(token: String) -> Result<String, String> {
    if token.chars().count() < MIN_TOKEN_LEN {
        return Err(format!(
            "{TOKEN_ENV} must be at least {MIN_TOKEN_LEN} characters"
        ));
    }
    Ok(token)
}

/// 32 hex characters from the OS random source.
fn new_token() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| format!("can't make a token: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Compares in time that does not depend on where the tokens differ.
fn same_token(given: &[u8], token: &[u8]) -> bool {
    given.len() == token.len() && given.iter().zip(token).fold(0, |acc, (a, b)| acc | (a ^ b)) == 0
}

/// Binds 127.0.0.1:`port` and serves each connection on a thread of its
/// own, closing one that sends nothing for `idle`. Returns the bound port.
/// `app` hands a request to the main thread and returns false once the app
/// is gone.
fn listen<A>(port: u16, token: String, idle: Duration, app: A) -> Result<u16, String>
where
    A: Fn(Request, Sender<Answer>) -> bool + Clone + Send + 'static,
{
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
        .map_err(|e| format!("can't listen on 127.0.0.1:{port}: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let token: Arc<str> = token.into();
    let open = Arc::new(AtomicUsize::new(0));
    std::thread::Builder::new()
        .name("control-listen".into())
        .spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => accept(stream, &token, &open, idle, app.clone()),
                    // Out of file descriptors, say. Don't spin on it.
                    Err(_) => std::thread::sleep(Duration::from_millis(50)),
                }
            }
        })
        .map_err(|e| format!("can't start the control thread: {e}"))?;
    Ok(port)
}

fn accept<A>(
    mut stream: TcpStream,
    token: &Arc<str>,
    open: &Arc<AtomicUsize>,
    idle: Duration,
    app: A,
) where
    A: Fn(Request, Sender<Answer>) -> bool + Send + 'static,
{
    let Some(slot) = Slot::take(open) else {
        let refused = ChannelError::TooManyConnections {
            limit: MAX_CONNECTIONS,
        };
        let _ = write_reply(&mut stream, Err(refused.into()));
        return;
    };
    let _ = stream.set_nodelay(true);
    if stream.set_read_timeout(Some(idle)).is_err() {
        return;
    }
    let Ok(reader) = stream.try_clone().map(BufReader::new) else {
        return;
    };
    let token = Arc::clone(token);
    // A failed spawn drops the closure, and with it the slot.
    let _ = std::thread::Builder::new()
        .name("control-conn".into())
        .spawn(move || {
            let _slot = slot;
            serve(reader, stream, &token, &app);
        });
}

/// One open connection, counted against `MAX_CONNECTIONS` until dropped.
struct Slot(Arc<AtomicUsize>);

impl Slot {
    fn take(open: &Arc<AtomicUsize>) -> Option<Slot> {
        let slot = Slot(Arc::clone(open));
        // Over the cap, dropping the slot gives the count back.
        (open.fetch_add(1, Ordering::AcqRel) < MAX_CONNECTIONS).then_some(slot)
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Answers one connection's requests until it closes, goes idle past the
/// read timeout, or sends a line over `MAX_LINE`. A bad line gets an error
/// reply and the next line is read.
fn serve(
    mut reader: impl BufRead,
    mut writer: impl Write,
    token: &str,
    app: &impl Fn(Request, Sender<Answer>) -> bool,
) {
    loop {
        let line = match read_line(&mut reader) {
            Ok(Line::Text(line)) => line,
            Ok(Line::TooLong) => {
                let too_long = ChannelError::TooLong { limit: MAX_LINE };
                let _ = write_reply(&mut writer, Err(too_long.into()));
                return;
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                let _ = write_reply(&mut writer, Err(ChannelError::IdleTimeout.into()));
                return;
            }
            Ok(Line::End) | Err(_) => return,
        };
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let answer = parse_request(&line, token).and_then(|op| ask(app, op));
        if write_reply(&mut writer, answer).is_err() {
            return;
        }
    }
}

enum Line {
    Text(Vec<u8>),
    TooLong,
    End,
}

/// Reads one line of at most `MAX_LINE` bytes, never buffering more.
fn read_line(reader: &mut impl BufRead) -> std::io::Result<Line> {
    let mut line = Vec::new();
    let read = reader
        .take(MAX_LINE as u64 + 1)
        .read_until(b'\n', &mut line)?;
    Ok(if read == 0 {
        Line::End
    } else if line.len() > MAX_LINE && line.last() != Some(&b'\n') {
        Line::TooLong
    } else {
        Line::Text(line)
    })
}

/// Parses one request line and checks its token.
fn parse_request(line: &[u8], token: &str) -> Result<Op, Failure> {
    let bad = |reason: &str| ChannelError::BadJson {
        reason: reason.into(),
    };
    let value: Value = serde_json::from_slice(line).map_err(|e| ChannelError::BadJson {
        reason: e.to_string(),
    })?;
    if !value.is_object() {
        return Err(bad("not an object").into());
    }
    let given = value.get("token").and_then(Value::as_str).unwrap_or("");
    if !same_token(given.as_bytes(), token.as_bytes()) {
        return Err(ChannelError::BadToken.into());
    }
    let op = value
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("missing a string `op`"))?;
    Ok(match op {
        "run" => {
            let command = value.get("command").unwrap_or(&Value::Null);
            Op::Run(Box::new(Command::from_json(command)?))
        }
        "commands" => Op::Commands,
        "state" => Op::State,
        "screenshot" => Op::Screenshot,
        "wait" => Op::Wait(match value.get("timeout_ms") {
            None => DEFAULT_WAIT,
            Some(ms) => {
                let ms = ms
                    .as_u64()
                    .ok_or_else(|| bad("`timeout_ms` must be a whole number of milliseconds"))?;
                Duration::from_millis(ms).min(MAX_WAIT)
            }
        }),
        _ => return Err(ChannelError::UnknownOp { op: op.into() }.into()),
    })
}

/// Hands `op` to the main thread and waits for its answer.
fn ask(app: &impl Fn(Request, Sender<Answer>) -> bool, op: Op) -> Answer {
    let patience = match &op {
        Op::Wait(timeout) => *timeout + REPLY_TIMEOUT,
        _ => REPLY_TIMEOUT,
    };
    let expires = Instant::now() + patience;
    let (reply, answer) = mpsc::channel();
    if !app(Request { op, expires }, reply) {
        return Err(ChannelError::AppClosed.into());
    }
    match answer.recv_timeout(expires.saturating_duration_since(Instant::now())) {
        Ok(answer) => answer,
        Err(RecvTimeoutError::Timeout) => Err(ChannelError::Timeout.into()),
        Err(RecvTimeoutError::Disconnected) => Err(ChannelError::AppClosed.into()),
    }
}

fn write_reply(writer: &mut impl Write, answer: Answer) -> std::io::Result<()> {
    let mut line = reply_json(answer).to_string();
    line.push('\n');
    writer.write_all(line.as_bytes())?;
    writer.flush()
}

fn reply_json(answer: Answer) -> Value {
    let failed = |f: Failure| json!({ "ok": false, "error": f.error, "message": f.message });
    match answer {
        Ok(Payload::Json(result)) => json!({ "ok": true, "result": result }),
        Ok(Payload::Frame(frame)) => match frame.to_png() {
            Ok(png) => json!({
                "ok": true,
                "result": {
                    "png_base64": lightphotos::base64::encode(&png),
                    "width": frame.width,
                    "height": frame.height,
                },
            }),
            Err(e) => failed(
                ChannelError::Failed {
                    reason: format!("no screenshot: {e}"),
                }
                .into(),
            ),
        },
        Err(failure) => failed(failure),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn request(op: &str) -> String {
        format!(r#"{{"token":"{TOKEN}","op":"{op}"}}"#)
    }

    fn error_tag(failure: &Failure) -> &str {
        failure.error["error"].as_str().unwrap()
    }

    #[test]
    fn requests_parse_into_ops() {
        let parse = |line: &str| parse_request(line.as_bytes(), TOKEN);
        assert!(matches!(parse(&request("state")), Ok(Op::State)));
        assert!(matches!(parse(&request("commands")), Ok(Op::Commands)));
        assert!(matches!(parse(&request("screenshot")), Ok(Op::Screenshot)));
        assert!(matches!(parse(&request("wait")), Ok(Op::Wait(t)) if t == DEFAULT_WAIT));
        let wait = |ms: &str| {
            parse(&format!(
                r#"{{"token":"{TOKEN}","op":"wait","timeout_ms":{ms}}}"#
            ))
        };
        assert!(matches!(wait("250"), Ok(Op::Wait(t)) if t == Duration::from_millis(250)));
        assert!(matches!(wait("999999999"), Ok(Op::Wait(t)) if t == MAX_WAIT));
        assert_eq!(error_tag(&wait("-1").unwrap_err()), "bad_json");

        let run = parse(&format!(
            r#"{{"token":"{TOKEN}","op":"run","command":{{"id":"photo.rate","params":{{"stars":3}}}}}}"#
        ));
        let Ok(Op::Run(cmd)) = run else {
            panic!("not a run: {run:?}");
        };
        assert_eq!(cmd.id(), "photo.rate");
    }

    #[test]
    fn bad_requests_get_the_matching_error() {
        let tag =
            |line: &str| error_tag(&parse_request(line.as_bytes(), TOKEN).unwrap_err()).to_string();
        assert_eq!(tag("{not json"), "bad_json");
        assert_eq!(tag("[1, 2]"), "bad_json");
        assert_eq!(tag(&format!(r#"{{"token":"{TOKEN}"}}"#)), "bad_json");
        assert_eq!(tag(&request("dance")), "unknown_op");
        assert_eq!(
            tag(&format!(
                r#"{{"token":"{TOKEN}","op":"run","command":{{"id":"photo.zap"}}}}"#
            )),
            "unknown_command"
        );
        assert_eq!(
            tag(&format!(
                r#"{{"token":"{TOKEN}","op":"run","command":{{"id":"photo.rate","params":{{"stars":9}}}}}}"#
            )),
            "bad_params"
        );
    }

    #[test]
    fn a_wrong_missing_or_partial_token_is_refused_before_the_op() {
        for line in [
            r#"{"op":"state"}"#.to_string(),
            r#"{"token":42,"op":"state"}"#.to_string(),
            format!(r#"{{"token":"{}","op":"state"}}"#, &TOKEN[..31]),
            format!(r#"{{"token":"{TOKEN}0","op":"state"}}"#),
            format!(r#"{{"token":"{}","op":"dance"}}"#, TOKEN.to_uppercase()),
        ] {
            let failure = parse_request(line.as_bytes(), TOKEN).unwrap_err();
            assert_eq!(error_tag(&failure), "bad_token", "{line}");
        }
    }

    #[test]
    fn tokens_are_long_enough_and_random() {
        assert!(check_token("short".into()).is_err());
        assert!(check_token("x".repeat(15)).is_err());
        assert!(check_token("x".repeat(16)).is_ok());
        let (a, b) = (new_token().unwrap(), new_token().unwrap());
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn the_control_flag_is_split_from_the_path() {
        let split = |args: &[&str]| split_args(args.iter().map(|s| s.to_string()));
        assert_eq!(
            split(&["photo.jpg"]).unwrap(),
            (None, vec!["photo.jpg".into()])
        );
        assert_eq!(
            split(&["--control", "0", "dir"]).unwrap(),
            (Some(0), vec!["dir".into()])
        );
        assert_eq!(split(&["dir", "--control", "4100"]).unwrap().0, Some(4100));
        assert!(split(&["--control"]).is_err());
        assert!(split(&["--control", "70000"]).is_err());
    }

    #[test]
    fn a_line_over_the_cap_is_too_long_and_one_at_the_cap_is_not() {
        let mut at_cap = vec![b' '; MAX_LINE];
        at_cap.push(b'\n');
        at_cap.extend_from_slice(&vec![b'x'; MAX_LINE + 1]);
        let mut reader = Cursor::new(at_cap);
        assert!(
            matches!(read_line(&mut reader).unwrap(), Line::Text(l) if l.len() == MAX_LINE + 1)
        );
        assert!(matches!(read_line(&mut reader).unwrap(), Line::TooLong));

        let mut short = Cursor::new(b"{}".to_vec());
        assert!(matches!(read_line(&mut short).unwrap(), Line::Text(l) if l == b"{}"));
        assert!(matches!(read_line(&mut short).unwrap(), Line::End));
    }

    /// Stands in for the main thread: answers each op on a thread of its own.
    fn fake_app(request: Request, reply: Sender<Answer>) -> bool {
        std::thread::spawn(move || {
            let answer = match request.op {
                Op::State => Ok(Payload::Json(json!({ "mode": "grid" }))),
                Op::Screenshot => Ok(Payload::Frame(RgbFrame {
                    width: 2,
                    height: 1,
                    rgb: vec![255, 0, 0, 0, 0, 255],
                })),
                Op::Run(cmd) => Err(CommandError::Failed {
                    reason: format!("{} refused", cmd.id()),
                }
                .into()),
                _ => Err(ChannelError::Timeout.into()),
            };
            let _ = reply.send(answer);
        });
        true
    }

    fn replies(input: &str) -> Vec<Value> {
        let mut out = Vec::new();
        serve(
            Cursor::new(input.as_bytes().to_vec()),
            &mut out,
            TOKEN,
            &fake_app,
        );
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn a_bad_line_gets_an_error_and_the_next_line_still_works() {
        let input = format!(
            "{{oops\n\n{}\n{}\n",
            r#"{"token":"nope-nope-nope-nope","op":"state"}"#,
            request("state")
        );
        let replies = replies(&input);
        assert_eq!(replies.len(), 3, "{replies:?}");
        assert_eq!(replies[0]["ok"], false);
        assert_eq!(replies[0]["error"]["error"], "bad_json");
        assert_eq!(replies[1]["error"]["error"], "bad_token");
        assert_eq!(
            replies[2],
            json!({ "ok": true, "result": { "mode": "grid" } })
        );
    }

    #[test]
    fn a_command_error_comes_back_with_its_message() {
        let line = format!(r#"{{"token":"{TOKEN}","op":"run","command":{{"id":"edit.undo"}}}}"#);
        let reply = &replies(&line)[0];
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["error"]["error"], "failed");
        assert_eq!(reply["message"], "edit.undo refused");
    }

    #[test]
    fn a_too_long_line_is_refused_and_the_connection_closes() {
        let input = format!("{}\n{}\n", "x".repeat(MAX_LINE + 10), request("state"));
        let replies = replies(&input);
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0]["error"]["error"], "too_long");
    }

    #[test]
    fn a_closed_app_is_an_error_not_a_hang() {
        let mut out = Vec::new();
        let gone = |_: Request, _: Sender<Answer>| false;
        serve(
            Cursor::new(request("state").into_bytes()),
            &mut out,
            TOKEN,
            &gone,
        );
        let reply: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(reply["error"]["error"], "app_closed");
    }

    /// A slot frees just after its socket closes, so retry for a while.
    fn assert_a_slot_frees(port: u16) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            // A refused connection may close before the request is written.
            let mut again = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
            let _ = again.write_all(format!("{}\n", request("state")).as_bytes());
            let mut reply = String::new();
            let _ = BufReader::new(again).read_line(&mut reply);
            if reply.contains(r#""ok":true"#) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "slots never freed: {reply}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn idle_connections_get_one_error_line_and_free_their_slots() {
        let idle = Duration::from_millis(100);
        let port = listen(0, TOKEN.into(), idle, fake_app).unwrap();
        let connect = || TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        let held: Vec<TcpStream> = (0..MAX_CONNECTIONS).map(|_| connect()).collect();
        for mut stream in held {
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut said = String::new();
            stream.read_to_string(&mut said).unwrap();
            let lines: Vec<Value> = said
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect();
            assert_eq!(lines.len(), 1, "{said}");
            assert_eq!(lines[0]["error"]["error"], "idle_timeout");
        }
        assert_a_slot_frees(port);
    }

    #[test]
    fn over_loopback_a_screenshot_decodes_and_extra_connections_are_refused() {
        let port = listen(0, TOKEN.into(), IDLE_TIMEOUT, fake_app).unwrap();
        let connect = || TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        let ask_line = |stream: &mut TcpStream, line: &str| -> Value {
            stream.write_all(format!("{line}\n").as_bytes()).unwrap();
            let mut reply = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut reply)
                .unwrap();
            serde_json::from_str(&reply).unwrap()
        };

        let mut first = connect();
        let shot = ask_line(&mut first, &request("screenshot"));
        assert_eq!(shot["ok"], true, "{shot}");
        assert_eq!(
            (
                shot["result"]["width"].clone(),
                shot["result"]["height"].clone()
            ),
            (json!(2), json!(1))
        );
        let b64 = shot["result"]["png_base64"].as_str().unwrap();
        assert!(b64.starts_with("iVBORw0KGgo"), "a PNG signature: {b64}");

        let mut open: Vec<TcpStream> = (1..MAX_CONNECTIONS).map(|_| connect()).collect();
        for stream in &mut open {
            assert_eq!(ask_line(stream, &request("state"))["ok"], true);
        }
        let mut extra = connect();
        let mut refused = String::new();
        extra.read_to_string(&mut refused).unwrap();
        let refused: Value = serde_json::from_str(refused.trim()).unwrap();
        assert_eq!(refused["error"]["error"], "too_many_connections");

        drop(open);
        drop(first);
        assert_a_slot_frees(port);
    }
}
