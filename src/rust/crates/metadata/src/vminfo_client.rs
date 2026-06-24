// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! A minimal async HTTP/1.1 client for the vminfod daemon.
//!
//! vminfod is plain HTTP on localhost. The metadata agent needs two access
//! patterns: a one-shot `GET /vms` (and `/vms/:uuid`) returning the body, and
//! a long-lived `GET /events` whose NDJSON lines drive zone lifecycle. The
//! crates/vminfo `Client` is blocking and rejects buffered body bytes after
//! the headers, which a long-running streaming reader cannot tolerate; this
//! async client keeps the leftover bytes so the `ready` frame is never lost.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

/// A vminfod endpoint (default 127.0.0.1:9090).
#[derive(Clone)]
pub struct VminfoClient {
    host: String,
    port: u16,
    user_agent: String,
}

/// A request error.
#[derive(Debug)]
pub enum VminfoError {
    Transport(String),
    Status(u16),
    Parse(String),
}

impl std::fmt::Display for VminfoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VminfoError::Transport(s) => write!(f, "{s}"),
            VminfoError::Status(c) => write!(f, "bad statusCode: {c}"),
            VminfoError::Parse(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for VminfoError {}

impl VminfoClient {
    #[must_use]
    pub fn new(host: String, port: u16, user_agent: String) -> Self {
        Self {
            host,
            port,
            user_agent,
        }
    }

    /// `GET <path>` and parse the (non-streaming) body as JSON.
    pub async fn get_json(&self, path: &str) -> Result<serde_json::Value, VminfoError> {
        let body = self.get(path).await?;
        serde_json::from_str(&body).map_err(|e| VminfoError::Parse(e.to_string()))
    }

    /// `GET <path>` and return the response body string. Uses
    /// `Connection: close` so a read-to-end delimits the body.
    pub async fn get(&self, path: &str) -> Result<String, VminfoError> {
        let mut stream = self.connect().await?;
        self.send_request(&mut stream, path, "close").await?;
        let mut raw = Vec::new();
        stream
            .read_to_end(&mut raw)
            .await
            .map_err(|e| VminfoError::Transport(e.to_string()))?;
        let (status, body) = parse_response(&raw)?;
        if status != 200 {
            return Err(VminfoError::Status(status));
        }
        Ok(body)
    }

    /// Open `GET /events` and return a buffered reader positioned right after
    /// the response headers. Reading the headers *through* the `BufReader`
    /// leaves any body bytes that shared the header's final TCP segment (the
    /// `ready` frame) buffered for the caller's NDJSON line reader.
    pub async fn open_events(&self, path: &str) -> Result<BufReader<TcpStream>, VminfoError> {
        let mut stream = self.connect().await?;
        // keep-alive so the daemon keeps the long-lived stream open.
        self.send_request(&mut stream, path, "keep-alive").await?;
        let mut reader = BufReader::new(stream);
        let status = read_headers(&mut reader).await?;
        if status != 200 {
            return Err(VminfoError::Status(status));
        }
        Ok(reader)
    }

    async fn connect(&self) -> Result<TcpStream, VminfoError> {
        let addr = format!("{}:{}", self.host, self.port);
        let stream = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(&addr))
            .await
            .map_err(|_| VminfoError::Transport(format!("connect {addr}: timeout")))?
            .map_err(|e| VminfoError::Transport(format!("connect {addr}: {e}")))?;
        Ok(stream)
    }

    async fn send_request(
        &self,
        stream: &mut TcpStream,
        path: &str,
        conn: &str,
    ) -> Result<(), VminfoError> {
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: {}:{}\r\nUser-Agent: {}\r\nAccept: */*\r\nConnection: {conn}\r\n\r\n",
            self.host, self.port, self.user_agent
        );
        stream
            .write_all(req.as_bytes())
            .await
            .map_err(|e| VminfoError::Transport(e.to_string()))?;
        stream
            .flush()
            .await
            .map_err(|e| VminfoError::Transport(e.to_string()))
    }
}

/// Read just the HTTP response headers from a buffered `reader`, returning the
/// status code. Body bytes that follow the header terminator stay buffered in
/// the `BufReader` for the caller's subsequent line reads.
async fn read_headers(reader: &mut BufReader<TcpStream>) -> Result<u16, VminfoError> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = reader
            .read(&mut byte)
            .await
            .map_err(|e| VminfoError::Transport(e.to_string()))?;
        if n == 0 {
            return Err(VminfoError::Transport(
                "connection closed before headers".to_string(),
            ));
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
        if buf.len() > 64 * 1024 {
            return Err(VminfoError::Transport("headers too large".to_string()));
        }
    }
    let header_text = String::from_utf8_lossy(&buf);
    parse_status_line(header_text.split("\r\n").next().unwrap_or(""))
}

fn parse_response(raw: &[u8]) -> Result<(u16, String), VminfoError> {
    let sep = find_subslice(raw, b"\r\n\r\n")
        .ok_or_else(|| VminfoError::Transport("no header terminator".to_string()))?;
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

fn parse_status_line(line: &str) -> Result<u16, VminfoError> {
    line.split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| VminfoError::Transport(format!("bad status line: {line}")))
}

fn dechunk(mut data: &[u8]) -> Result<String, VminfoError> {
    let mut out = Vec::new();
    loop {
        let crlf = find_subslice(data, b"\r\n")
            .ok_or_else(|| VminfoError::Transport("malformed chunk size".to_string()))?;
        let size_line = String::from_utf8_lossy(&data[..crlf]);
        let size_hex = size_line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16)
            .map_err(|e| VminfoError::Transport(format!("chunk size: {e}")))?;
        data = &data[crlf + 2..];
        if size == 0 {
            break;
        }
        if data.len() < size {
            return Err(VminfoError::Transport("truncated chunk".to_string()));
        }
        out.extend_from_slice(&data[..size]);
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
    fn parse_plain_ok() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n[]";
        let (status, body) = parse_response(raw).expect("parse");
        assert_eq!(status, 200);
        assert_eq!(body, "[]");
    }

    #[test]
    fn parse_chunked() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n[]\r\n0\r\n\r\n";
        let (status, body) = parse_response(raw).expect("parse");
        assert_eq!(status, 200);
        assert_eq!(body, "[]");
    }

    #[test]
    fn status_message_matches_node() {
        assert_eq!(VminfoError::Status(404).to_string(), "bad statusCode: 404");
    }
}
