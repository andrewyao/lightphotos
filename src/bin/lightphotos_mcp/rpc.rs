//! JSON-RPC 2.0 over stdio, one message per line, answered in order.

use std::io::{BufRead, Read, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};

use serde_json::{json, Map, Value};

use crate::tools::{text, Mode, Tool};

/// The longest message line. A longer one is skipped with an error.
const MAX_LINE: usize = 4 << 20;

/// Newest first; `initialize` falls back to the first.
const PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// Answers each request line on `input` until it ends. Only a failed write
/// to `output` stops it early.
pub fn serve(mut input: impl BufRead, mut output: impl Write, mode: &Mode) -> std::io::Result<()> {
    loop {
        let reply = match read_line(&mut input)? {
            Line::End => return Ok(()),
            Line::TooLong => Some(error(
                Value::Null,
                INVALID_REQUEST,
                &format!("message longer than {MAX_LINE} bytes"),
            )),
            Line::Text(line) if line.iter().all(u8::is_ascii_whitespace) => None,
            Line::Text(line) => handle(&line, mode),
        };
        if let Some(reply) = reply {
            writeln!(output, "{reply}")?;
            output.flush()?;
        }
    }
}

enum Line {
    Text(Vec<u8>),
    TooLong,
    End,
}

/// Reads one line of at most `MAX_LINE` bytes. A longer one is read to its
/// end and dropped, never buffered whole.
fn read_line(input: &mut impl BufRead) -> std::io::Result<Line> {
    let mut line = Vec::new();
    let read = input
        .take(MAX_LINE as u64 + 1)
        .read_until(b'\n', &mut line)?;
    if read == 0 {
        return Ok(Line::End);
    }
    if line.len() <= MAX_LINE || line.last() == Some(&b'\n') {
        return Ok(Line::Text(line));
    }
    loop {
        let buf = input.fill_buf()?;
        if buf.is_empty() {
            break;
        }
        match buf.iter().position(|&b| b == b'\n') {
            Some(i) => {
                input.consume(i + 1);
                break;
            }
            None => {
                let n = buf.len();
                input.consume(n);
            }
        }
    }
    Ok(Line::TooLong)
}

/// The reply to one message, or `None` for a notification or a response.
fn handle(line: &[u8], mode: &Mode) -> Option<Value> {
    let message: Value = match serde_json::from_slice(line) {
        Ok(v) => v,
        Err(e) => return Some(error(Value::Null, PARSE_ERROR, &format!("not JSON: {e}"))),
    };
    let Value::Object(message) = message else {
        return Some(error(
            Value::Null,
            INVALID_REQUEST,
            "send one JSON-RPC object per line; batches are not supported",
        ));
    };
    let id = message.get("id").cloned();
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        // A response to a request we never send, or junk without a method.
        let is_response = message.contains_key("result") || message.contains_key("error");
        return match id {
            Some(id) if !is_response => {
                Some(error(id, INVALID_REQUEST, "missing a string `method`"))
            }
            _ => None,
        };
    };
    // A notification (no id) gets no reply, whatever its method.
    let id = id?;
    let params = match message.get("params") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(p)) => p.clone(),
        Some(_) => return Some(error(id, INVALID_PARAMS, "`params` must be an object")),
    };
    Some(match method {
        "initialize" => success(id, initialize(&params, mode)),
        "ping" => success(id, json!({})),
        "tools/list" => success(id, json!({ "tools": mode.tools() })),
        "tools/call" => match call_tool(&params, mode) {
            Ok(result) => success(id, result),
            Err(message) => error(id, INVALID_PARAMS, &message),
        },
        _ => error(id, METHOD_NOT_FOUND, &format!("no method `{method}`")),
    })
}

fn initialize(params: &Map<String, Value>, mode: &Mode) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str);
    let version = PROTOCOL_VERSIONS
        .into_iter()
        .find(|v| Some(*v) == asked)
        .unwrap_or(PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "lightphotos", "version": env!("CARGO_PKG_VERSION") },
        "instructions": mode.instructions(),
    })
}

/// A `tools/call` result, or the message of a JSON-RPC error for a call that
/// names no tool. A tool that fails answers with `isError`.
fn call_tool(params: &Map<String, Value>, mode: &Mode) -> Result<Value, String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or("missing a string `name`")?;
    let tool = Tool::parse(name).ok_or_else(|| format!("no tool `{name}`; call tools/list"))?;
    let args = match params.get("arguments") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(a)) => a.clone(),
        Some(_) => return Err("`arguments` must be an object".into()),
    };
    // A decoder that panics on a malformed file fails this call, not the server.
    let result = catch_unwind(AssertUnwindSafe(|| mode.call(tool, &args)))
        .unwrap_or_else(|_| Err(format!("{name} failed unexpectedly; see the server's log")));
    Ok(match result {
        Ok(content) => json!({ "content": content, "isError": false }),
        Err(message) => json!({ "content": [text(message)], "isError": true }),
    })
}

fn success(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}
