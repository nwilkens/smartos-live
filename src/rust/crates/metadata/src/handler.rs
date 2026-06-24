// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The shared connection handler: line buffering + request dispatch.
//!
//! Both the OS-zone unix server and the HVM serial client run the same line
//! protocol over a byte stream. This module owns the per-connection buffer
//! (split on `\n`, retain the trailing partial line) and turns each complete
//! line into a response by consulting the [`crate::resolver`]. Writes
//! (PUT/DELETE) are delegated to an injected [`Writer`] so the I/O layer stays
//! decoupled from the vmadm update path.

use serde_json::Value;

use crate::protocol::{self, Outcome, Request};
use crate::resolver::{self, Resolved, WriteOp};

/// Applies validated metadata writes (PUT/DELETE). Implemented over the vmadm
/// update path in the daemon; a no-op/in-memory impl is used in tests.
pub trait MetadataWriter {
    /// Apply a write, returning Ok on success or an error message on failure.
    fn apply(&self, uuid: &str, op: &WriteOp) -> Result<(), String>;
}

/// A per-connection line buffer.
#[derive(Default)]
pub struct LineBuffer {
    buf: Vec<u8>,
}

impl LineBuffer {
    #[must_use]
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Feed received bytes; return the complete lines (without `\n`), retaining
    /// any trailing partial line for the next call.
    pub fn push(&mut self, data: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(data);
        let mut lines = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            // Drop the trailing '\n'; keep the rest as a (lossy) string.
            let s = String::from_utf8_lossy(&line[..line.len().saturating_sub(1)]).into_owned();
            lines.push(s);
        }
        lines
    }
}

/// Handle one complete request line against `vm`, returning the bytes to write
/// back (empty for a silent drop). `uuid` and `writer` support PUT/DELETE.
#[must_use]
pub fn handle_line<W: MetadataWriter>(
    line: &str,
    vm: &Value,
    uuid: &str,
    writer: &W,
) -> Vec<u8> {
    let (req, ctx) = protocol::parse_line(line);
    match req {
        Request::Invalid => protocol::invalid_command(),
        Request::SilentDrop => Vec::new(),
        Request::Negotiate { ver } => protocol::format_negotiate(&ver),
        Request::Get { key, is_v2 } => {
            let outcome = resolver::resolve_get(vm, &key);
            frame(is_v2, &ctx.reqid, &outcome)
        }
        Request::Keys { is_v2 } => {
            let outcome = resolver::resolve_keys(vm);
            frame(is_v2, &ctx.reqid, &outcome)
        }
        Request::Unknown { cmd, is_v2 } => {
            let outcome = Outcome::Failure {
                message: format!("Unknown command {cmd}"),
            };
            frame(is_v2, &ctx.reqid, &outcome)
        }
        // PUT/DELETE are V2-only.
        Request::Put { key, value } => {
            let outcome = apply_write(resolver::resolve_put(vm, &key, &value), uuid, writer);
            frame(true, &ctx.reqid, &outcome)
        }
        Request::Delete { key } => {
            let outcome = apply_write(resolver::resolve_delete(vm, &key), uuid, writer);
            frame(true, &ctx.reqid, &outcome)
        }
    }
}

fn apply_write<W: MetadataWriter>(resolved: Resolved, uuid: &str, writer: &W) -> Outcome {
    match resolved {
        Resolved::WriteError(message) => Outcome::Failure { message },
        Resolved::Write(op) => match writer.apply(uuid, &op) {
            Ok(()) => Outcome::Success("OK".to_string()),
            Err(message) => Outcome::Failure { message },
        },
        // resolve_put/resolve_delete never return Read.
        Resolved::Read(o) => o,
    }
}

fn frame(is_v2: bool, reqid: &str, outcome: &Outcome) -> Vec<u8> {
    if is_v2 {
        protocol::format_v2(reqid, outcome)
    } else {
        protocol::format_v1(outcome)
    }
}

/// A writer that refuses all writes; used where the update path is not wired.
pub struct ReadOnlyWriter;

impl MetadataWriter for ReadOnlyWriter {
    fn apply(&self, _uuid: &str, _op: &WriteOp) -> Result<(), String> {
        Err("metadata writes are not supported".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD as B64;
    use crate::crc32::crc32_calc;
    use serde_json::json;

    fn vm() -> Value {
        json!({
            "uuid": "u",
            "hostname": "img2",
            "resolvers": ["10.199.199.14"],
            "customer_metadata": {},
            "internal_metadata": {"root_pw": "secret"},
            "tags": {},
            "routes": {}
        })
    }

    fn make_v2(reqid: &str, cmd: &str, b64arg: &str) -> String {
        let body = if b64arg.is_empty() {
            cmd.to_string()
        } else {
            format!("{cmd} {b64arg}")
        };
        let inner = format!("{reqid} {body}");
        format!("V2 {} {} {}", inner.len(), crc32_calc(inner.as_bytes()), inner)
    }

    #[test]
    fn line_buffer_splits_and_retains_partial() {
        let mut lb = LineBuffer::new();
        let got = lb.push(b"GET sdc:hostname\nKE");
        assert_eq!(got, vec!["GET sdc:hostname".to_string()]);
        let got2 = lb.push(b"YS\n");
        assert_eq!(got2, vec!["KEYS".to_string()]);
    }

    #[test]
    fn v1_get_hostname() {
        let out = handle_line("GET sdc:hostname", &vm(), "u", &ReadOnlyWriter);
        assert_eq!(out, b"SUCCESS\nimg2\n.\n");
    }

    #[test]
    fn v2_get_hostname_matches_live_shape() {
        let arg = B64.encode(b"sdc:hostname");
        let line = make_v2("000001", "GET", &arg);
        let out = handle_line(&line, &vm(), "u", &ReadOnlyWriter);
        assert_eq!(out, b"V2 23 0cb469ba 000001 SUCCESS aW1nMg==\n");
    }

    #[test]
    fn v2_keys() {
        let line = make_v2("000003", "KEYS", "");
        let out = handle_line(&line, &vm(), "u", &ReadOnlyWriter);
        assert_eq!(out, b"V2 27 40a81145 000003 SUCCESS cm9vdF9wdw==\n");
    }

    #[test]
    fn v2_notfound() {
        let arg = B64.encode(b"sdc:nonexistent-key-xyz");
        let line = make_v2("000002", "GET", &arg);
        let out = handle_line(&line, &vm(), "u", &ReadOnlyWriter);
        assert_eq!(out, b"V2 15 1ddcd766 000002 NOTFOUND\n");
    }

    #[test]
    fn negotiate() {
        let out = handle_line("NEGOTIATE V2", &vm(), "u", &ReadOnlyWriter);
        assert_eq!(out, b"V2_OK\n");
    }

    #[test]
    fn silent_drop_writes_nothing() {
        let inner = "1 GET c2RjOmhvc3RuYW1l";
        let line = format!("V2 {} deadbeef {}", inner.len(), inner);
        let out = handle_line(&line, &vm(), "u", &ReadOnlyWriter);
        assert!(out.is_empty());
    }

    struct CapturingWriter {
        applied: std::cell::RefCell<Vec<(String, WriteOp)>>,
    }
    impl MetadataWriter for CapturingWriter {
        fn apply(&self, uuid: &str, op: &WriteOp) -> Result<(), String> {
            self.applied.borrow_mut().push((uuid.to_string(), op.clone()));
            Ok(())
        }
    }

    #[test]
    fn v2_put_ok() {
        let w = CapturingWriter {
            applied: std::cell::RefCell::new(Vec::new()),
        };
        let k = B64.encode(b"mykey");
        let v = B64.encode(b"myval");
        let line = make_v2("9", "PUT", &format!("{k} {v}"));
        let out = handle_line(&line, &vm(), "u", &w);
        // body 'OK' -> base64 'T0s='.
        let s = String::from_utf8(out).expect("utf8");
        assert!(s.contains("SUCCESS T0s="), "got {s}");
        assert_eq!(w.applied.borrow().len(), 1);
    }
}
