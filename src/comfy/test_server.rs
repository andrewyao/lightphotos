// SPDX-License-Identifier: MIT OR Apache-2.0

//! A one-request-per-connection HTTP/1.1 server on 127.0.0.1 for tests.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;

pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

pub struct Reply {
    pub status: u16,
    /// A `content-length` here replaces the true one.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Sends no `content-length`, so closing the connection ends the body.
    pub no_length: bool,
}

impl Reply {
    pub fn new(status: u16, body: impl Into<Vec<u8>>) -> Reply {
        Reply {
            status,
            headers: Vec::new(),
            body: body.into(),
            no_length: false,
        }
    }

    pub fn header(mut self, name: &str, value: impl Into<String>) -> Reply {
        self.headers.push((name.into(), value.into()));
        self
    }
}

/// Serves `handler` until the test process ends. Returns `http://127.0.0.1:<port>`.
pub fn serve(handler: impl Fn(&Request) -> Reply + Send + 'static) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let Some(request) = read_request(&mut stream) else {
                continue;
            };
            let reply = handler(&request);
            let mut head = format!("HTTP/1.1 {} X\r\nconnection: close\r\n", reply.status);
            if !reply.no_length
                && !reply
                    .headers
                    .iter()
                    .any(|(k, _)| k.eq_ignore_ascii_case("content-length"))
            {
                head.push_str(&format!("content-length: {}\r\n", reply.body.len()));
            }
            for (k, v) in &reply.headers {
                head.push_str(&format!("{k}: {v}\r\n"));
            }
            head.push_str("\r\n");
            // The client may hang up early, as a cancelled download does.
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&reply.body);
        }
    });
    url
}

fn read_request(stream: &mut std::net::TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut headers = Vec::new();
    loop {
        line.clear();
        reader.read_line(&mut line).ok()?;
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        let (k, v) = trimmed.split_once(':')?;
        headers.push((k.trim().to_string(), v.trim().to_string()));
    }
    let len: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0; len];
    reader.read_exact(&mut body).ok()?;
    Some(Request {
        method,
        path,
        headers,
        body,
    })
}
