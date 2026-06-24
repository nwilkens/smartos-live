// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! A minimal blocking HTTP/1.1 client for the vminfod daemon.
//!
//! vminfod is plain HTTP on localhost; a full HTTP stack is overkill and
//! would add dependencies. This client sends a `GET` and returns the body,
//! reproducing node `client.js`'s contract: a non-200 status is an error
//! whose message is `"bad statusCode: <code>"` (the vminfo CLI prints that to
//! stderr and exits 1).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// A client bound to a host:port (default 127.0.0.1:9090).
pub struct Client {
    host: String,
    port: u16,
    user_agent: String,
}

/// A request error: either a transport failure or a non-200 status whose
/// message matches node's `"bad statusCode: <code>"`.
#[derive(Debug)]
pub enum ClientError {
    Transport(String),
    Status(u16),
    Parse(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Transport(s) => write!(f, "{s}"),
            ClientError::Status(code) => write!(f, "bad statusCode: {code}"),
            ClientError::Parse(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for ClientError {}

impl Client {
    #[must_use]
    pub fn new(host: String, port: u16, user_agent: String) -> Self {
        Self {
            host,
            port,
            user_agent,
        }
    }

    /// `GET <path>` and return the response body, erroring on a non-200
    /// status (matching node's client). `path` includes any query string.
    pub fn get(&self, path: &str) -> Result<String, ClientError> {
        let (status, body) = self.request(path)?;
        if status != 200 {
            return Err(ClientError::Status(status));
        }
        Ok(body)
    }

    /// `GET <path>` and parse the body as JSON, erroring on non-200 or on a
    /// JSON parse failure (node's `JSON.parse` catch).
    pub fn get_json(&self, path: &str) -> Result<serde_json::Value, ClientError> {
        let body = self.get(path)?;
        serde_json::from_str(&body).map_err(|e| ClientError::Parse(e.to_string()))
    }

    /// Open a streaming GET for `/events`: return the connected stream after
    /// the headers (so the caller can read NDJSON lines). Errors on non-200.
    pub fn get_stream(&self, path: &str) -> Result<TcpStream, ClientError> {
        let mut stream = self.connect()?;
        self.send_request(&mut stream, path)?;
        // Read until end of headers, capturing the status and any body bytes
        // already buffered after the header terminator.
        let (status, leftover) = read_headers(&mut stream)?;
        if status != 200 {
            return Err(ClientError::Status(status));
        }
        // The caller needs the leftover body bytes too; prepend them via a
        // small wrapper is overkill, so we require callers to drain leftover
        // first. We return the leftover through a side channel: stash it by
        // returning a stream whose first reads are the leftover. Simplest:
        // require no leftover for /events (the ready frame arrives after
        // headers). If there is leftover, we cannot push it back onto the
        // TcpStream, so we error to avoid silently dropping data.
        if !leftover.is_empty() {
            return Err(ClientError::Transport(
                "unexpected buffered body before stream".to_string(),
            ));
        }
        Ok(stream)
    }

    fn connect(&self) -> Result<TcpStream, ClientError> {
        let addr = format!("{}:{}", self.host, self.port);
        let stream = TcpStream::connect(&addr)
            .map_err(|e| ClientError::Transport(format!("connect {addr}: {e}")))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        Ok(stream)
    }

    fn send_request(&self, stream: &mut TcpStream, path: &str) -> Result<(), ClientError> {
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: {}:{}\r\nUser-Agent: {}\r\nAccept: */*\r\nConnection: close\r\n\r\n",
            self.host, self.port, self.user_agent
        );
        stream
            .write_all(req.as_bytes())
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        stream
            .flush()
            .map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Send a request and read the full (non-streaming) response body,
    /// handling both `Content-Length` and `Transfer-Encoding: chunked` and a
    /// `Connection: close` body.
    fn request(&self, path: &str) -> Result<(u16, String), ClientError> {
        let mut stream = self.connect()?;
        self.send_request(&mut stream, path)?;

        let mut raw = Vec::new();
        stream
            .read_to_end(&mut raw)
            .map_err(|e| ClientError::Transport(e.to_string()))?;

        parse_response(&raw)
    }
}

/// Parse a full HTTP response (headers + body) into `(status, body)`. Handles
/// chunked transfer encoding and plain bodies.
fn parse_response(raw: &[u8]) -> Result<(u16, String), ClientError> {
    let sep = find_subslice(raw, b"\r\n\r\n")
        .ok_or_else(|| ClientError::Transport("no header terminator".to_string()))?;
    let header_bytes = &raw[..sep];
    let body_bytes = &raw[sep + 4..];

    let header_text = String::from_utf8_lossy(header_bytes);
    let mut lines = header_text.split("\r\n");
    let status = parse_status_line(lines.next().unwrap_or(""))?;

    let chunked = lines.any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    });

    let body = if chunked {
        dechunk(body_bytes)?
    } else {
        String::from_utf8_lossy(body_bytes).to_string()
    };
    Ok((status, body))
}

/// Read just the headers from a live stream, returning `(status, leftover)`
/// where leftover is any body bytes already read past the header terminator.
fn read_headers(stream: &mut TcpStream) -> Result<(u16, Vec<u8>), ClientError> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = stream
            .read(&mut byte)
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        if n == 0 {
            return Err(ClientError::Transport("connection closed before headers".to_string()));
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
        if buf.len() > 64 * 1024 {
            return Err(ClientError::Transport("headers too large".to_string()));
        }
    }
    let header_text = String::from_utf8_lossy(&buf);
    let status = parse_status_line(header_text.split("\r\n").next().unwrap_or(""))?;
    Ok((status, Vec::new()))
}

fn parse_status_line(line: &str) -> Result<u16, ClientError> {
    // "HTTP/1.1 200 OK"
    line.split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| ClientError::Transport(format!("bad status line: {line}")))
}

/// Decode a chunked-transfer body into its concatenated payload.
fn dechunk(mut data: &[u8]) -> Result<String, ClientError> {
    let mut out = Vec::new();
    loop {
        let crlf = find_subslice(data, b"\r\n")
            .ok_or_else(|| ClientError::Transport("malformed chunk size".to_string()))?;
        let size_line = String::from_utf8_lossy(&data[..crlf]);
        let size_hex = size_line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16)
            .map_err(|e| ClientError::Transport(format!("chunk size: {e}")))?;
        data = &data[crlf + 2..];
        if size == 0 {
            break;
        }
        if data.len() < size {
            return Err(ClientError::Transport("truncated chunk".to_string()));
        }
        out.extend_from_slice(&data[..size]);
        // Skip the trailing CRLF after the chunk data.
        data = &data[size..];
        if data.starts_with(b"\r\n") {
            data = &data[2..];
        }
    }
    Ok(String::from_utf8_lossy(&out).to_string())
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_plain_response() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 15\r\n\r\n{\"ping\":\"pong\"}";
        let (status, body) = parse_response(raw).expect("parse");
        assert_eq!(status, 200);
        assert_eq!(body, "{\"ping\":\"pong\"}");
    }

    #[test]
    fn parse_chunked_response() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n[]\r\n0\r\n\r\n";
        let (status, body) = parse_response(raw).expect("parse");
        assert_eq!(status, 200);
        assert_eq!(body, "[]");
    }

    #[test]
    fn parse_404() {
        let raw = b"HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\n\r\nVM Not Found: x";
        let (status, body) = parse_response(raw).expect("parse");
        assert_eq!(status, 404);
        assert_eq!(body, "VM Not Found: x");
    }

    #[test]
    fn status_error_message_matches_node() {
        let e = ClientError::Status(404);
        assert_eq!(e.to_string(), "bad statusCode: 404");
    }
}
