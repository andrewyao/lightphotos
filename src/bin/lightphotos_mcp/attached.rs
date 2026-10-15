//! Attached mode: each tool call is one request on a fresh connection to
//! the app's `--control` channel (`src/shell/control/tcp.rs`).

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

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

    /// Sends one request on a new connection and returns the reply's
    /// `result`.
    fn ask(&self, mut request: Value) -> Result<Value, Failure> {
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
        request["token"] = json!(token);
        let patience = match request.get("op").and_then(Value::as_str) {
            Some("wait") => REPLY_TIMEOUT + Duration::from_millis(WAIT_MS),
            _ => REPLY_TIMEOUT,
        };
        let line = self
            .exchange(&request.to_string(), patience)
            .map_err(|e| fail(self.explain(&e, patience)))?;
        let reply: Value = serde_json::from_slice(&line)
            .map_err(|e| fail(format!("the app sent a reply that is not JSON: {e}")))?;
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

    /// Sends `request` and reads the reply line, all within `patience`.
    fn exchange(&self, request: &str, patience: Duration) -> std::io::Result<Vec<u8>> {
        let until = Instant::now() + patience;
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, self.port));
        let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
        stream.set_write_timeout(Some(CONNECT_TIMEOUT))?;
        stream.write_all(format!("{request}\n").as_bytes())?;
        let mut line = Vec::new();
        BufReader::new(Deadline { stream, until })
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
        let result = App::new(port, None).exchange("{}", Duration::from_millis(400));
        let took = start.elapsed();
        peer.join().unwrap();

        let kind = result.as_ref().map_err(std::io::Error::kind).err();
        assert!(
            matches!(kind, Some(ErrorKind::TimedOut | ErrorKind::WouldBlock)),
            "{result:?}"
        );
        assert!(took < Duration::from_millis(1500), "took {took:?}");
    }
}
