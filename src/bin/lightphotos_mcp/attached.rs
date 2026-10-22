//! Attached mode: each tool call signs in on a fresh connection to the
//! app's `--control` channel (`src/shell/control/tcp.rs`) and sends one
//! request.

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use lightphotos::control_auth::{self, Side};

use crate::tools::{command_request, image, text, Tool, ToolResult};

pub const TOKEN_ENV: &str = "LIGHTPHOTOS_CONTROL_TOKEN";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// The app answers within 30 s or replies `timeout` itself.
const REPLY_TIMEOUT: Duration = Duration::from_secs(35);
const WAIT_MS: u64 = 10_000;
/// A screenshot of a large window is a few MB of base64.
const MAX_REPLY: u64 = 64 << 20;

pub struct App {
    port: u16,
    token: Option<String>,
}

/// A failed request: the app's error code, when it answered, and what to
/// tell the agent.
struct Failure {
    code: Option<String>,
    message: String,
}

impl From<Failure> for String {
    fn from(f: Failure) -> String {
        f.message
    }
}

impl App {
    pub fn new(port: u16, token: Option<String>) -> App {
        App { port, token }
    }

    pub fn call(&self, tool: Tool, args: &Map<String, Value>) -> ToolResult {
        let wait = |default| args.get("wait").and_then(Value::as_bool).unwrap_or(default);
        match tool {
            Tool::ListCommands => Ok(vec![text(
                self.ask(json!({ "op": "commands" }))?.to_string(),
            )]),
            Tool::RunCommand => {
                let command = command_request(args)?;
                let result = self.ask(json!({ "op": "run", "command": command }))?;
                Ok(vec![text(result.to_string())])
            }
            Tool::GetState if wait(false) => Ok(vec![text(self.settle()?.to_string())]),
            Tool::GetState => Ok(vec![text(self.ask(json!({ "op": "state" }))?.to_string())]),
            Tool::ListPhotos => Ok(vec![text(self.ask(json!({ "op": "list" }))?.to_string())]),
            Tool::Screenshot => self.screenshot(wait(true)),
            Tool::Render => Err(
                "render works headless: run lightphotos-mcp with --folder <dir>. \
                                 Attached, call screenshot to see the app."
                    .into(),
            ),
        }
    }

    fn screenshot(&self, wait: bool) -> ToolResult {
        let mut note = String::new();
        if wait {
            match self.settle() {
                Ok(_) => {}
                Err(f) if f.code.as_deref() == Some("timeout") => {
                    note = " The app was still busy after 10 s, so this may show it mid-change."
                        .into();
                }
                Err(f) => return Err(f.into()),
            }
        }
        let shot = self.ask(json!({ "op": "screenshot" }))?;
        let png = shot
            .get("png_base64")
            .and_then(Value::as_str)
            .ok_or("the app's screenshot reply had no `png_base64`")?;
        let size = |key| shot.get(key).and_then(Value::as_u64).unwrap_or(0);
        Ok(vec![
            image(png.to_string(), "image/png"),
            text(format!(
                "The app window, {}x{} px.{note}",
                size("width"),
                size("height")
            )),
        ])
    }

    /// The state once the app settles, or a `timeout` failure.
    fn settle(&self) -> Result<Value, Failure> {
        self.ask(json!({ "op": "wait", "timeout_ms": WAIT_MS }))
    }

    /// Signs in on a new connection, sends one request, and returns the
    /// reply's `result`. The token never leaves this process: the app gets
    /// an HMAC of its nonce (`lightphotos::control_auth`).
    fn ask(&self, request: Value) -> Result<Value, Failure> {
        let fail = |message: String| Failure {
            code: None,
            message,
        };
        let token = self.token.as_deref().ok_or_else(|| {
            fail(format!(
                "no control token. Set {TOKEN_ENV} for lightphotos-mcp to \
                 the token the app printed at startup, or start the app with the same \
                 {TOKEN_ENV}."
            ))
        })?;
        let patience = match request.get("op").and_then(Value::as_str) {
            Some("wait") => REPLY_TIMEOUT + Duration::from_millis(WAIT_MS),
            _ => REPLY_TIMEOUT,
        };
        let io = |e: std::io::Error| fail(self.explain(&e, patience));
        let mut conn = Conn::open(self.port, Instant::now() + patience).map_err(io)?;

        let client = control_auth::new_nonce().map_err(fail)?;
        let hello = conn
            .line(&json!({ "op": "hello", "nonce": client }).to_string())
            .map_err(io)?;
        let hello = reply_result(&hello).map_err(|f| match f.code.as_deref() {
            // An app from before the sign-in reads `hello` as a request
            // with no token.
            Some("bad_token") => fail(format!(
                "the app on 127.0.0.1:{} is older than this lightphotos-mcp and does not \
                 know its sign-in. Update the app to the same version, then retry.",
                self.port
            )),
            _ => f,
        })?;
        // Nothing that depends on the token goes out until the other end
        // has shown it holds the token too.
        let server = hello
            .get("nonce")
            .and_then(Value::as_str)
            .filter(|n| control_auth::is_nonce(n));
        let proof = hello.get("proof").and_then(Value::as_str).unwrap_or("");
        let Some(server) = server
            .filter(|server| control_auth::verify(token, Side::Server, &client, server, proof))
        else {
            return Err(fail(format!(
                "the program on 127.0.0.1:{} could not prove it holds the token, so nothing \
                 was sent to it. Either {TOKEN_ENV} for lightphotos-mcp differs from the \
                 app's, or something other than LightPhotos is on the port.",
                self.port
            )));
        };
        let mac = control_auth::mac_hex(token, Side::Client, &client, server);
        let auth = json!({ "op": "auth", "mac": mac });
        reply_result(&conn.line(&auth.to_string()).map_err(io)?)?;

        reply_result(&conn.line(&request.to_string()).map_err(io)?)
    }

    fn explain(&self, e: &std::io::Error, patience: Duration) -> String {
        let port = self.port;
        match e.kind() {
            ErrorKind::ConnectionRefused => format!(
                "nothing is listening on 127.0.0.1:{port}. Start the app with \
                 `{TOKEN_ENV}=<token> lightphotos --control {port} <folder>`, set the same \
                 {TOKEN_ENV} for lightphotos-mcp, and retry."
            ),
            ErrorKind::WouldBlock | ErrorKind::TimedOut => format!(
                "the app on 127.0.0.1:{port} did not answer within {} s; it may be busy. Retry, \
                 or call get_state with wait: true.",
                patience.as_secs()
            ),
            ErrorKind::UnexpectedEof => format!(
                "the app on 127.0.0.1:{port} {e}; it may have quit. Start it with \
                 `lightphotos --control {port}` and retry."
            ),
            _ => format!("cannot reach the app on 127.0.0.1:{port}: {e}"),
        }
    }
}

/// One reply line's `result`, or the app's error with a hint for the agent.
fn reply_result(line: &[u8]) -> Result<Value, Failure> {
    let reply: Value = serde_json::from_slice(line).map_err(|e| Failure {
        code: None,
        message: format!("the app sent a reply that is not JSON: {e}"),
    })?;
    if reply.get("ok").and_then(Value::as_bool) == Some(true) {
        return Ok(reply.get("result").cloned().unwrap_or(Value::Null));
    }
    let code = reply
        .pointer("/error/error")
        .and_then(Value::as_str)
        .map(str::to_string);
    let said = match (
        code.as_deref(),
        reply.pointer("/error/id").and_then(Value::as_str),
    ) {
        // The app's own text says to list the commands, which the hint
        // below names as the tool.
        (Some("unknown_command"), Some(id)) => format!("no command `{id}`"),
        _ => reply
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("the app refused the request")
            .to_string(),
    };
    let hint = match code.as_deref() {
        Some("unknown_command") => " Call list_commands to see every id.",
        Some("bad_token") => {
            " Set LIGHTPHOTOS_CONTROL_TOKEN for lightphotos-mcp to the token the app printed."
        }
        Some("too_many_connections") => " Retry in a moment.",
        Some("timeout") => " Retry, or call get_state with wait: true.",
        _ => "",
    };
    Err(Failure {
        code,
        message: format!("{}.{hint}", said.trim_end_matches('.')),
    })
}

/// One connection to the app, whose reads all end by one deadline.
struct Conn {
    writer: TcpStream,
    reader: BufReader<Deadline>,
}

impl Conn {
    fn open(port: u16, until: Instant) -> std::io::Result<Conn> {
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let writer = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
        writer.set_write_timeout(Some(CONNECT_TIMEOUT))?;
        let stream = writer.try_clone()?;
        Ok(Conn {
            writer,
            reader: BufReader::new(Deadline { stream, until }),
        })
    }

    /// Sends `request` and reads one reply line.
    fn line(&mut self, request: &str) -> std::io::Result<Vec<u8>> {
        self.writer.write_all(format!("{request}\n").as_bytes())?;
        let mut line = Vec::new();
        (&mut self.reader)
            .take(MAX_REPLY)
            .read_until(b'\n', &mut line)?;
        if line.last() != Some(&b'\n') {
            let why = if line.is_empty() {
                "closed the connection without answering"
            } else {
                "sent a reply that was cut off"
            };
            return Err(std::io::Error::new(ErrorKind::UnexpectedEof, why));
        }
        Ok(line)
    }
}

/// A stream whose reads all end by `until`, so a peer that sends a byte at
/// a time cannot stretch one request past its patience.
struct Deadline {
    stream: TcpStream,
    until: Instant,
}

impl Read for Deadline {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let left = self.until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(std::io::Error::new(
                ErrorKind::TimedOut,
                "the reply did not arrive in time",
            ));
        }
        self.stream.set_read_timeout(Some(left))?;
        self.stream.read(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn a_peer_that_drips_its_reply_is_cut_off_at_the_deadline() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(conn.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            for _ in 0..30 {
                if conn.write_all(b" ").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });

        let start = Instant::now();
        let until = start + Duration::from_millis(400);
        let result = Conn::open(port, until).and_then(|mut c| c.line("{}"));
        let took = start.elapsed();
        peer.join().unwrap();

        let kind = result.as_ref().map_err(std::io::Error::kind).err();
        assert!(
            matches!(kind, Some(ErrorKind::TimedOut | ErrorKind::WouldBlock)),
            "{result:?}"
        );
        assert!(took < Duration::from_millis(1500), "took {took:?}");
    }

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    /// Answers one connection with `answer`, called with each request
    /// line, and returns every byte the client sent.
    fn peer(
        answer: impl Fn(&Value) -> Value + Send + 'static,
    ) -> (u16, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let heard = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut heard = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    return heard;
                }
                heard.push_str(&line);
                let reply = answer(&serde_json::from_str(&line).unwrap());
                conn.write_all(format!("{reply}\n").as_bytes()).unwrap();
            }
        });
        (port, heard)
    }

    #[test]
    fn the_client_signs_in_without_sending_the_token() {
        const NONCE: &str = "00112233445566778899aabbccddeeff";
        let client = std::sync::Mutex::new(String::new());
        let (port, heard) = peer(move |request| match request["op"].as_str() {
            Some("hello") => {
                let theirs = request["nonce"].as_str().unwrap().to_string();
                let proof = control_auth::mac_hex(TOKEN, Side::Server, &theirs, NONCE);
                *client.lock().unwrap() = theirs;
                json!({ "ok": true, "result": { "nonce": NONCE, "proof": proof } })
            }
            Some("auth") => {
                let mac = request["mac"].as_str().unwrap_or("");
                let theirs = client.lock().unwrap().clone();
                assert!(
                    control_auth::verify(TOKEN, Side::Client, &theirs, NONCE, mac),
                    "{request}"
                );
                json!({ "ok": true, "result": { "signed_in": true } })
            }
            _ => json!({ "ok": true, "result": { "mode": "grid" } }),
        });
        let state = App::new(port, Some(TOKEN.into())).ask(json!({ "op": "state" }));
        assert_eq!(state.map_err(String::from).unwrap()["mode"], "grid");
        let heard = heard.join().unwrap();
        assert_eq!(heard.lines().count(), 3, "{heard}");
        assert!(
            !heard.contains(TOKEN),
            "the token crossed the socket: {heard}"
        );
    }

    #[test]
    fn a_squatter_that_cannot_prove_the_token_gets_no_mac() {
        let (port, heard) = peer(|request| match request["op"].as_str() {
            Some("hello") => json!({ "ok": true, "result": { "nonce": "00ff", "proof": "00" } }),
            _ => json!({ "ok": true, "result": { "signed_in": true } }),
        });
        let err = App::new(port, Some(TOKEN.into()))
            .ask(json!({ "op": "state" }))
            .map_err(String::from)
            .unwrap_err();
        assert!(err.contains("could not prove"), "{err}");
        let heard = heard.join().unwrap();
        assert_eq!(heard.lines().count(), 1, "only the hello: {heard}");
        assert!(!heard.contains("mac"), "{heard}");
    }

    #[test]
    fn an_app_from_before_the_sign_in_gets_a_clear_error() {
        let (port, heard) = peer(
            |_| json!({ "ok": false, "error": { "error": "bad_token" }, "message": "wrong or missing `token`" }),
        );
        let err = App::new(port, Some(TOKEN.into()))
            .ask(json!({ "op": "state" }))
            .map_err(String::from)
            .unwrap_err();
        assert!(err.contains("older than this lightphotos-mcp"), "{err}");
        assert!(!heard.join().unwrap().contains(TOKEN));
    }
}
