// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! VM source: load VM objects (the node `VM.lookup({}, {fields})` equivalent)
//! from the vminfod daemon (plain HTTP on 127.0.0.1:9090), then project them
//! into the [`Vm`](crate::vm::Vm) model used by the compiler.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use serde_json::Value;

use crate::error::FwError;
use crate::vm::Vm;

const HOST: &str = "127.0.0.1";
const PORT: u16 = 9090;

/// Fetch all VMs from vminfod and project into the compiler model.
pub fn load_vms() -> Result<Vec<Vm>, FwError> {
    let body = http_get("/vms")?;
    let v: Value = serde_json::from_str(&body)?;
    let arr = v
        .as_array()
        .ok_or_else(|| FwError::generic("vminfod /vms: expected array"))?;
    Ok(arr.iter().filter_map(Vm::from_json).collect())
}

/// Fetch a single VM by uuid; returns None if not found.
pub fn load_vm(uuid: &str) -> Result<Option<Vm>, FwError> {
    let vms = load_vms()?;
    Ok(vms.into_iter().find(|v| v.uuid == uuid))
}

fn http_get(path: &str) -> Result<String, FwError> {
    let addr = format!("{HOST}:{PORT}");
    let mut stream = TcpStream::connect(&addr).map_err(|e| {
        FwError::generic(format!("connect vminfod {addr}: {e}"))
    })?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| FwError::generic(e.to_string()))?;

    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {HOST}\r\nUser-Agent: fwadm\r\n\
         Accept: application/json\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| FwError::generic(e.to_string()))?;

    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|e| FwError::generic(e.to_string()))?;

    let text = String::from_utf8_lossy(&raw);
    let idx = text
        .find("\r\n\r\n")
        .ok_or_else(|| FwError::generic("vminfod: malformed response"))?;
    let headers = &text[..idx];
    let status_ok = headers
        .lines()
        .next()
        .map(|l| l.contains(" 200 "))
        .unwrap_or(false);
    if !status_ok {
        return Err(FwError::generic(format!(
            "vminfod {path}: non-200 response"
        )));
    }
    let body = &text[idx + 4..];
    Ok(decode_body(headers, body))
}

/// Decode the body, handling `Transfer-Encoding: chunked`.
fn decode_body(headers: &str, body: &str) -> String {
    let chunked = headers
        .lines()
        .any(|l| l.to_ascii_lowercase().starts_with("transfer-encoding:")
            && l.to_ascii_lowercase().contains("chunked"));
    if !chunked {
        return body.to_string();
    }
    let mut out = String::new();
    let mut rest = body;
    loop {
        let Some(nl) = rest.find("\r\n") else { break };
        let size_str = &rest[..nl];
        let size = usize::from_str_radix(size_str.trim(), 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        let start = nl + 2;
        if start + size > rest.len() {
            out.push_str(&rest[start..]);
            break;
        }
        out.push_str(&rest[start..start + size]);
        rest = &rest[start + size..];
        if let Some(stripped) = rest.strip_prefix("\r\n") {
            rest = stripped;
        }
    }
    out
}
