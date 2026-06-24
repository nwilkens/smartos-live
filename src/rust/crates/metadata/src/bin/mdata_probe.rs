// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! `mdata-probe` — a tiny mdata protocol client for differential testing.
//!
//! Connects to a unix metadata socket, issues a fixed sequence of V1 and V2
//! requests, and prints each response as both a Rust byte-literal (for exact
//! wire diffs) and a decoded value. Used to compare a TEST metadata daemon's
//! socket against the live socket byte-for-byte. Read-only: it never issues
//! PUT/DELETE.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use metadata::crc32::crc32_calc;

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: mdata-probe <socket-path> [key ...]");
        eprintln!("  with no keys, runs the standard differential suite");
        return std::process::ExitCode::FAILURE;
    }
    let sock = &args[1];

    let keys: Vec<String> = if args.len() > 2 {
        args[2..].to_vec()
    } else {
        vec![
            "sdc:hostname".into(),
            "sdc:uuid".into(),
            "sdc:resolvers".into(),
            "sdc:nics".into(),
            "root_pw".into(),
            "sdc:nonexistent-key-xyz".into(),
        ]
    };

    match run(sock, &keys) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mdata-probe: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(sock: &str, keys: &[String]) -> std::io::Result<()> {
    // One connection for the whole suite, mirroring a real client.
    let mut stream = UnixStream::connect(sock)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;

    // V1 NEGOTIATE then a V1 GET.
    let resp = exchange(&mut stream, b"NEGOTIATE V2\n")?;
    report("NEGOTIATE V2", &resp);

    // V1 GETs.
    for (i, key) in keys.iter().enumerate() {
        let _ = i;
        let req = format!("GET {key}\n");
        let resp = exchange(&mut stream, req.as_bytes())?;
        report(&format!("V1 GET {key}"), &resp);
    }
    let resp = exchange(&mut stream, b"KEYS\n")?;
    report("V1 KEYS", &resp);

    // V2 frames.
    let mut reqid = 1u32;
    for key in keys {
        let frame = v2_get(reqid, key);
        let resp = exchange(&mut stream, frame.as_bytes())?;
        report(&format!("V2 GET {key} (reqid {reqid})"), &resp);
        reqid += 1;
    }
    let frame = v2_keys(reqid);
    let resp = exchange(&mut stream, frame.as_bytes())?;
    report(&format!("V2 KEYS (reqid {reqid})"), &resp);

    Ok(())
}

/// Build a V2 GET frame, base64-encoding the key.
fn v2_get(reqid: u32, key: &str) -> String {
    let arg = B64.encode(key.as_bytes());
    let inner = format!("{reqid} GET {arg}");
    format!("V2 {} {} {}\n", inner.len(), crc32_calc(inner.as_bytes()), inner)
}

fn v2_keys(reqid: u32) -> String {
    let inner = format!("{reqid} KEYS");
    format!("V2 {} {} {}\n", inner.len(), crc32_calc(inner.as_bytes()), inner)
}

/// Send a request and read one response (until a recognizable terminator).
fn exchange(stream: &mut UnixStream, req: &[u8]) -> std::io::Result<Vec<u8>> {
    stream.write_all(req)?;
    stream.flush()?;

    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                if response_complete(&out) {
                    break;
                }
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                break;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

/// Heuristic: a response is complete at a V1 terminator or a full V2 line.
fn response_complete(out: &[u8]) -> bool {
    if out.ends_with(b"V2_OK\n") {
        return true;
    }
    if out.starts_with(b"V2 ") && out.ends_with(b"\n") {
        return true;
    }
    if out.ends_with(b"NOTFOUND\n") || out.ends_with(b"FAILURE\n") || out.ends_with(b".\n") {
        return true;
    }
    if out == b"invalid command\n" {
        return true;
    }
    false
}

/// Print the response both as an exact byte-literal and a decoded value.
fn report(label: &str, resp: &[u8]) {
    println!("=== {label} ===");
    println!("RAW {resp:?}");
    println!("HEX {}", hex(resp));
    if let Some(decoded) = decode_value(resp) {
        println!("VAL {decoded}");
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Decode the value carried by a response (V1 body, or V2 base64 body).
fn decode_value(resp: &[u8]) -> Option<String> {
    let s = String::from_utf8_lossy(resp);
    if let Some(rest) = s.strip_prefix("V2 ") {
        // V2 <len> <crc> <reqid> <CODE> [<b64body>]
        let inner: Vec<&str> = rest.trim_end().splitn(5, ' ').collect();
        if inner.len() >= 4 {
            let code = inner[3];
            if inner.len() == 5
                && let Ok(b) = B64.decode(inner[4])
            {
                return Some(format!("{code} {}", String::from_utf8_lossy(&b)));
            }
            return Some(code.to_string());
        }
        return None;
    }
    if let Some(rest) = s.strip_prefix("SUCCESS\n") {
        // V1: body up to "\n.\n", un-dot-stuffed.
        let body = rest.strip_suffix(".\n").unwrap_or(rest);
        let body = body.strip_suffix('\n').unwrap_or(body);
        return Some(un_dot_stuff(body));
    }
    Some(s.trim_end().to_string())
}

fn un_dot_stuff(s: &str) -> String {
    s.lines()
        .map(|l| l.strip_prefix("..").map_or(l, |_| &l[1..]))
        .collect::<Vec<_>>()
        .join("\n")
}
