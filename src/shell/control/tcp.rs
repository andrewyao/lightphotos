// SPDX-License-Identifier: MIT OR Apache-2.0

//! `--control <port>`: the loopback TCP transport. Off unless the flag is
//! given, and bound to 127.0.0.1 only.
//!
//! Each TCP connection carries one JSON request per line and gets one JSON
//! reply per line. A connection signs in first, and each side proves it
//! holds the token without sending it (`lightphotos::control_auth`):
//!
//! ```text
//! {"op": "hello", "nonce": "<client nonce>"}  → {"ok": true, "result": {"nonce": "<server nonce>", "proof": "…"}}
//! {"op": "auth", "mac": "…"}                  → {"ok": true, "result": {"signed_in": true}}
//! {"op": "run", "command": {"id": "photo.rate", "params": {"stars": 3}}}
//! {"op": "commands" | "state" | "list" | "screenshot"}
//! {"op": "wait", "timeout_ms": 10000}
//!
//! {"ok": true, "result": …}
//! {"ok": false, "error": {"error": "auth_required", …}, "message": "…"}
//! ```
//!
//! A request that carries a `token`, as clients before the sign-in sent,
//! gets a `raw_token_refused` error that says to update the client.
//!
//! A connection thread parses and checks each line, then hands the request
//! to the main thread through the event loop (`UserEvent::Control`) and waits
//! for the answer on a channel.

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use winit::event_loop::EventLoopProxy;

use super::{Answer, ChannelError, Failure, Op, Payload, Reply, Request, DEFAULT_WAIT};
use crate::command::Command;
use crate::shell::macos_delegate::UserEvent;
use lightphotos::control_auth::{self, Side};

/// The longest request line. A longer one ends the connection.
const MAX_LINE: usize = 1 << 20;
const MAX_CONNECTIONS: usize = 4;
const MIN_TOKEN_LEN: usize = 16;
const TOKEN_ENV: &str = "LIGHTPHOTOS_CONTROL_TOKEN";
/// How long a connection may go without sending a line before it closes,
/// so idle clients can't hold every slot.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_WAIT: Duration = Duration::from_secs(60);

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
/// the port, and the token when the app made it, to stderr.
pub(crate) fn start(port: u16, proxy: EventLoopProxy<UserEvent>) -> Result<(), String> {
    let (token, generated) = token_from_env()?;
    let port = listen(port, token.clone(), IDLE_TIMEOUT, move |request, reply| {
        proxy.send_event(UserEvent::Control(request, reply)).is_ok()
    })?;
    eprintln!("{}", listening_line(port, &token, generated));
    Ok(())
}

/// The token, and whether the app made it rather than reading it from
/// [`TOKEN_ENV`].
fn token_from_env() -> Result<(String, bool), String> {
    match std::env::var(TOKEN_ENV) {
        Ok(token) => Ok((check_token(token)?, false)),
        Err(std::env::VarError::NotPresent) => Ok((new_token()?, true)),
        Err(std::env::VarError::NotUnicode(_)) => Err(format!("{TOKEN_ENV} is not valid text")),
    }
}

/// The startup line. A token the user chose is a secret they already hold,
/// so it stays out of stderr and whatever logs it.
fn listening_line(port: u16, token: &str, generated: bool) -> String {
    if generated {
        format!("[control] listening on 127.0.0.1:{port} token {token}")
    } else {
        format!("[control] listening on 127.0.0.1:{port} with the token from {TOKEN_ENV}")
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
    control_auth::random_hex::<16>().map_err(|e| format!("can't make a token: {e}"))
}

/// One connection's sign-in: the nonces of its last `hello`, and whether
/// it has answered them.
struct Session {
    token: Arc<str>,
    /// The client's nonce, then the app's.
    nonces: Option<(String, String)>,
    signed_in: bool,
    /// A wrong `mac` ends the connection, so each guess costs a new one.
    closing: bool,
}

impl Session {
    fn new(token: Arc<str>) -> Session {
        Session {
            token,
            nonces: None,
            signed_in: false,
            closing: false,
        }
    }

    /// Answers one line: a sign-in step here, a request on the main thread.
    fn answer(&mut self, line: &[u8], app: &impl Fn(Request, Reply) -> bool) -> Answer {
        match parse_request(line)? {
            Asked::Hello(client) => {
                let server = control_auth::new_nonce().map_err(|reason| ChannelError::Failed {
                    reason: format!("can't make a sign-in nonce: {reason}"),
                })?;
                let proof = control_auth::mac_hex(&self.token, Side::Server, &client, &server);
                self.nonces = Some((client, server.clone()));
                Ok(Payload::Json(json!({ "nonce": server, "proof": proof })))
            }
            Asked::Auth(mac) => {
                // The nonces answer once, right or wrong.
                let (client, server) = self.nonces.take().ok_or(ChannelError::AuthRequired)?;
                self.signed_in =
                    control_auth::verify(&self.token, Side::Client, &client, &server, &mac);
                if !self.signed_in {
                    self.closing = true;
                    return Err(ChannelError::BadToken.into());
                }
                Ok(Payload::Json(json!({ "signed_in": true })))
            }
            Asked::Op(op) if self.signed_in => ask(app, op),
            Asked::Op(_) => Err(ChannelError::AuthRequired.into()),
        }
    }
}

/// Binds 127.0.0.1:`port` and serves each connection on a thread of its
/// own, closing one that sends nothing for `idle`. Returns the bound port.
/// `app` hands a request to the main thread and returns false once the app
/// is gone.
fn listen<A>(port: u16, token: String, idle: Duration, app: A) -> Result<u16, String>
where
    A: Fn(Request, Reply) -> bool + Clone + Send + 'static,
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
    A: Fn(Request, Reply) -> bool + Send + 'static,
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
            serve(reader, stream, &mut Session::new(token), &app);
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
    session: &mut Session,
    app: &impl Fn(Request, Reply) -> bool,
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
        let answer = session.answer(&line, app);
        if write_reply(&mut writer, answer).is_err() || session.closing {
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

/// What one line asks for.
#[derive(Debug)]
enum Asked {
    /// The client's nonce.
    Hello(String),
    /// The client's MAC of the nonce, as hex.
    Auth(String),
    Op(Op),
}

/// Parses one request line.
fn parse_request(line: &[u8]) -> Result<Asked, Failure> {
    let bad = |reason: &str| ChannelError::BadJson {
        reason: reason.into(),
    };
    let value: Value = serde_json::from_slice(line).map_err(|e| ChannelError::BadJson {
        reason: e.to_string(),
    })?;
    if !value.is_object() {
        return Err(bad("not an object").into());
    }
    if value.get("token").is_some() {
        return Err(ChannelError::RawTokenRefused.into());
    }
    let op = value
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("missing a string `op`"))?;
    match op {
        "hello" => {
            let nonce = value
                .get("nonce")
                .and_then(Value::as_str)
                .filter(|n| control_auth::is_nonce(n))
                .ok_or_else(|| bad("`hello` takes a fresh hex `nonce` of your own"))?;
            return Ok(Asked::Hello(nonce.into()));
        }
        "auth" => {
            let mac = value
                .get("mac")
                .and_then(Value::as_str)
                .ok_or_else(|| bad("`auth` takes the nonce's MAC as a hex string `mac`"))?;
            return Ok(Asked::Auth(mac.into()));
        }
        _ => {}
    }
    Ok(Asked::Op(match op {
        "run" => {
            let command = value.get("command").unwrap_or(&Value::Null);
            Op::Run(Box::new(Command::from_json(command)?))
        }
        "commands" => Op::Commands,
        "state" => Op::State,
        "list" => Op::List,
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
    }))
}

/// Hands `op` to the main thread and waits for its answer.
fn ask(app: &impl Fn(Request, Reply) -> bool, op: Op) -> Answer {
    let request = Request::new(op);
    let expires = request.expires;
    let (reply, answer) = mpsc::channel();
    if !app(request, reply) {
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
    use crate::command::CommandError;
    use crate::renderer::RgbFrame;
    use std::io::Cursor;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn request(op: &str) -> String {
        format!(r#"{{"op":"{op}"}}"#)
    }

    fn token() -> Arc<str> {
        TOKEN.into()
    }

    fn parse_op(line: &str) -> Result<Op, Failure> {
        match parse_request(line.as_bytes())? {
            Asked::Op(op) => Ok(op),
            other => panic!("not an op: {other:?}"),
        }
    }

    /// A connection that has already signed in.
    fn signed_in() -> Session {
        Session {
            signed_in: true,
            ..Session::new(token())
        }
    }

    fn json_answer(answer: Answer) -> Result<Value, String> {
        match answer {
            Ok(Payload::Json(v)) => Ok(v),
            Ok(Payload::Frame(_)) => Err("a frame".into()),
            Err(f) => Err(error_tag(&f).to_string()),
        }
    }

    fn error_tag(failure: &Failure) -> &str {
        failure.error["error"].as_str().unwrap()
    }

    #[test]
    fn only_a_generated_token_is_printed() {
        assert!(listening_line(80, TOKEN, true).contains(TOKEN));
        assert!(!listening_line(80, TOKEN, false).contains(TOKEN));
    }

    #[test]
    fn requests_parse_into_ops() {
        let parse = parse_op;
        assert!(matches!(parse(&request("state")), Ok(Op::State)));
        assert!(matches!(parse(&request("commands")), Ok(Op::Commands)));
        assert!(matches!(parse(&request("list")), Ok(Op::List)));
        assert!(matches!(parse(&request("screenshot")), Ok(Op::Screenshot)));
        assert!(matches!(parse(&request("wait")), Ok(Op::Wait(t)) if t == DEFAULT_WAIT));
        let wait = |ms: &str| parse(&format!(r#"{{"op":"wait","timeout_ms":{ms}}}"#));
        assert!(matches!(wait("250"), Ok(Op::Wait(t)) if t == Duration::from_millis(250)));
        assert!(matches!(wait("999999999"), Ok(Op::Wait(t)) if t == MAX_WAIT));
        assert_eq!(error_tag(&wait("-1").unwrap_err()), "bad_json");

        let run = parse(r#"{"op":"run","command":{"id":"photo.rate","params":{"stars":3}}}"#);
        let Ok(Op::Run(cmd)) = run else {
            panic!("not a run: {run:?}");
        };
        assert_eq!(cmd.id(), "photo.rate");
    }

    #[test]
    fn bad_requests_get_the_matching_error() {
        let tag = |line: &str| error_tag(&parse_op(line).unwrap_err()).to_string();
        assert_eq!(tag("{not json"), "bad_json");
        assert_eq!(tag("[1, 2]"), "bad_json");
        assert_eq!(tag("{}"), "bad_json");
        assert_eq!(tag(&request("dance")), "unknown_op");
        assert_eq!(
            tag(r#"{"op":"run","command":{"id":"photo.zap"}}"#),
            "unknown_command"
        );
        assert_eq!(
            tag(r#"{"op":"run","command":{"id":"photo.rate","params":{"stars":9}}}"#),
            "bad_params"
        );
    }

    #[test]
    fn a_request_with_a_raw_token_is_refused_with_a_way_forward() {
        for line in [
            format!(r#"{{"token":"{TOKEN}","op":"state"}}"#),
            r#"{"token":42,"op":"dance"}"#.to_string(),
        ] {
            let failure = parse_request(line.as_bytes()).unwrap_err();
            assert_eq!(error_tag(&failure), "raw_token_refused", "{line}");
            assert!(
                failure.message.contains("Update lightphotos-mcp"),
                "{}",
                failure.message
            );
        }
    }

    const CLIENT: &str = "00ff";

    fn hello(client: &str) -> String {
        format!(r#"{{"op":"hello","nonce":"{client}"}}"#)
    }

    #[test]
    fn each_side_proves_the_token_and_a_connection_signs_in_before_any_request() {
        let mut session = Session::new(token());
        let mut say = |line: &str| json_answer(session.answer(line.as_bytes(), &fake_app));
        let auth = |mac: &str| format!(r#"{{"op":"auth","mac":"{mac}"}}"#);

        assert_eq!(say(&request("state")), Err("auth_required".into()));
        assert_eq!(
            say(&auth("00")),
            Err("auth_required".into()),
            "no hello yet"
        );
        assert_eq!(
            say(&request("hello")),
            Err("bad_json".into()),
            "no client nonce"
        );

        let first = say(&hello(CLIENT)).unwrap();
        let server = first["nonce"].as_str().unwrap().to_string();
        assert!(
            control_auth::is_nonce(&server) && server.len() == 64,
            "{server}"
        );
        let proof = first["proof"].as_str().unwrap();
        assert!(control_auth::verify(
            TOKEN,
            Side::Server,
            CLIENT,
            &server,
            proof
        ));
        assert!(
            !control_auth::verify(TOKEN, Side::Client, CLIENT, &server, proof),
            "the app's proof is no client MAC"
        );
        assert_eq!(
            say(&auth(proof)),
            Err("bad_token".into()),
            "echoing the proof back fails"
        );
        let right = control_auth::mac_hex(TOKEN, Side::Client, CLIENT, &server);
        assert_eq!(
            say(&auth(&right)),
            Err("auth_required".into()),
            "the nonces answer once"
        );

        let fresh = say(&hello(CLIENT)).unwrap()["nonce"]
            .as_str()
            .unwrap()
            .to_string();
        assert_ne!(fresh, server);
        let mac = control_auth::mac_hex(TOKEN, Side::Client, CLIENT, &fresh);
        assert_eq!(say(&auth(&mac)).unwrap()["signed_in"], true);
        assert_eq!(say(&request("state")).unwrap()["mode"], "grid");
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
    fn fake_app(request: Request, reply: Reply) -> bool {
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
            &mut signed_in(),
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
        let input = format!("{{oops\n\n{}\n{}\n", r#"{"op":"dance"}"#, request("state"));
        let replies = replies(&input);
        assert_eq!(replies.len(), 3, "{replies:?}");
        assert_eq!(replies[0]["ok"], false);
        assert_eq!(replies[0]["error"]["error"], "bad_json");
        assert_eq!(replies[1]["error"]["error"], "unknown_op");
        assert_eq!(
            replies[2],
            json!({ "ok": true, "result": { "mode": "grid" } })
        );
    }

    #[test]
    fn a_command_error_comes_back_with_its_message() {
        let reply = &replies(r#"{"op":"run","command":{"id":"edit.undo"}}"#)[0];
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["error"]["error"], "failed");
        assert_eq!(reply["message"], "edit.undo refused");
    }

    #[test]
    fn a_wrong_mac_closes_the_connection() {
        let input = format!(
            "{}\n{}\n{}\n",
            hello(CLIENT),
            r#"{"op":"auth","mac":"00"}"#,
            hello(CLIENT)
        );
        let mut out = Vec::new();
        serve(
            Cursor::new(input.into_bytes()),
            &mut out,
            &mut Session::new(token()),
            &fake_app,
        );
        let replies: Vec<Value> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(replies.len(), 2, "nothing after the wrong mac: {replies:?}");
        assert_eq!(replies[1]["error"]["error"], "bad_token");
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
        let gone = |_: Request, _: Reply| false;
        serve(
            Cursor::new(request("state").into_bytes()),
            &mut out,
            &mut signed_in(),
            &gone,
        );
        let reply: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(reply["error"]["error"], "app_closed");
    }

    /// Sends `line` and reads the reply line.
    fn ask_line(stream: &mut TcpStream, line: &str) -> Value {
        stream.write_all(format!("{line}\n").as_bytes()).unwrap();
        let mut reply = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut reply)
            .unwrap();
        serde_json::from_str(&reply).unwrap()
    }

    /// Signs `stream` in as a client does: `hello`, then the MAC of both nonces.
    fn sign_in(stream: &mut TcpStream) {
        let reply = ask_line(stream, &hello(CLIENT));
        let server = reply["result"]["nonce"].as_str().unwrap();
        let mac = control_auth::mac_hex(TOKEN, Side::Client, CLIENT, server);
        let auth = ask_line(stream, &format!(r#"{{"op":"auth","mac":"{mac}"}}"#));
        assert_eq!(auth["ok"], true, "{auth}");
    }

    /// A slot frees just after its socket closes, so retry for a while.
    fn assert_a_slot_frees(port: u16) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            // A refused connection may close before the request is written.
            let mut again = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
            let _ = again.write_all(format!("{}\n", hello(CLIENT)).as_bytes());
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

        let mut first = connect();
        sign_in(&mut first);
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
            sign_in(stream);
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
