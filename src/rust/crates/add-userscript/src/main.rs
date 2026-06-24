// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! `add-userscript` — native Rust drop-in for the SmartOS add-userscript tool.
//!
//! Takes a script filename, reads an optional VM payload on stdin, and injects
//! the script as the `user-script` metadata value. If the input payload has a
//! `brand` or `customer_metadata` key it is treated as a create payload (set
//! under `customer_metadata`); otherwise it is an update payload (set under
//! `set_customer_metadata`). Output is the payload re-serialized with 2-space
//! indent.

use std::io::{IsTerminal as _, Read as _};
use std::process::ExitCode;

use clap::Parser;
use serde_json::{Map, Value};

#[derive(Parser)]
#[command(name = "add-userscript", disable_help_flag = true)]
struct Cli {
    /// Path to the user-script file.
    filename: String,
}

fn main() -> ExitCode {
    // Match node usage: a single positional arg; anything else => usage/exit 1.
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(_) => {
            let prog = std::env::args().next().unwrap_or_else(|| "add-userscript".into());
            println!("Usage: {prog} <filename>");
            return ExitCode::from(1);
        }
    };

    let mut payload = read_payload();

    let action_create = match &payload {
        Value::Object(map) => map.contains_key("brand") || map.contains_key("customer_metadata"),
        // Non-object inputs never own those keys => update path (matches JS).
        _ => false,
    };

    let script = match std::fs::read(&cli.filename) {
        Ok(bytes) => bytes,
        Err(e) => {
            // Match node's fs error wording for the common ENOENT case.
            eprintln!("{}", node_fs_error(&cli.filename, &e));
            return ExitCode::from(1);
        }
    };
    // node uses Buffer.toString() => UTF-8 (lossy for invalid bytes).
    let script = String::from_utf8_lossy(&script).into_owned();

    if let Value::Object(map) = &mut payload {
        let target_key = if action_create {
            "customer_metadata"
        } else {
            "set_customer_metadata"
        };
        if !map.contains_key(target_key) {
            map.insert(target_key.to_string(), Value::Object(Map::new()));
        }
        if let Some(Value::Object(meta)) = map.get_mut(target_key) {
            meta.insert("user-script".to_string(), Value::String(script));
        }
    }

    match serde_json::to_string_pretty(&payload) {
        Ok(s) => {
            println!("{s}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(1)
        }
    }
}

/// Read stdin (unless a tty) and parse as JSON. Empty input or other parse
/// errors fall back to an empty object, mirroring the node tool.
fn read_payload() -> Value {
    let empty = Value::Object(Map::new());

    if std::io::stdin().is_terminal() {
        return empty;
    }

    let mut buf = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
        eprintln!("{e}");
        return empty;
    }

    if buf.trim().is_empty() {
        // node: JSON.parse('') throws "Unexpected end of input", silently => {}.
        return empty;
    }

    match serde_json::from_str::<Value>(&buf) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Warning(JSON.parse): {e}");
            empty
        }
    }
}

/// Approximate node's `fs.readFile` error message for a missing/unreadable file.
fn node_fs_error(path: &str, e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound => {
            format!("ENOENT, open '{path}'")
        }
        _ => e.to_string(),
    }
}
