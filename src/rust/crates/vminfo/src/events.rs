// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! `vminfo events [-jfr]`: subscribe to `/events` and trace records,
//! byte-matched to node `vminfo.js` `do_events`.

use std::io::{BufRead, BufReader};
use std::process::ExitCode;

use serde_json::Value;

use crate::client::Client;
use crate::fail;

/// Stream `/events` and print each record. `-j` dumps raw JSON per line;
/// otherwise it renders node's `[time] zonename alias type` trace. `-r`
/// includes the `ready` frame; `-f` uses full (non-truncated) fields.
pub fn do_events(client: &Client, full: bool, json: bool, ready: bool) -> ExitCode {
    let stream = match client.get_stream("/events") {
        Ok(s) => s,
        Err(e) => return fail(&e),
    };

    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::FAILURE;
            }
        };
        if line.is_empty() {
            continue;
        }
        let ev: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                // node: a JSON parse failure is fatal to the client.
                eprintln!("{e}");
                return ExitCode::FAILURE;
            }
        };

        let kind = ev.get("type").and_then(Value::as_str).unwrap_or("");
        if kind == "ready" {
            if ready {
                print_ready(&ev, full, json);
            }
            continue;
        }
        print_event(&ev, full, json);
    }

    ExitCode::SUCCESS
}

fn print_ready(ev: &Value, full: bool, json: bool) {
    if json {
        println!("{}", serde_json::to_string(ev).unwrap_or_default());
        return;
    }
    let date = format_date(ev.get("date").and_then(Value::as_str).unwrap_or(""), full);
    if full {
        let uuid = ev.get("uuid").and_then(Value::as_str).unwrap_or("");
        println!("[{date}] ready (uuid {uuid})");
    } else {
        println!("[{date}] ready");
    }
}

fn print_event(ev: &Value, full: bool, json: bool) {
    if json {
        println!("{}", serde_json::to_string(ev).unwrap_or_default());
        return;
    }

    let kind = ev.get("type").and_then(Value::as_str).unwrap_or("");
    let zonename = ev.get("zonename").and_then(Value::as_str).unwrap_or("");
    let zn = format_zonename(zonename, full);
    let date = format_date(ev.get("date").and_then(Value::as_str).unwrap_or(""), full);

    let mut alias = ev
        .get("vm")
        .and_then(|vm| vm.get("alias"))
        .and_then(Value::as_str)
        .unwrap_or("-")
        .to_string();
    if alias.chars().count() > 30 {
        let truncated: String = alias.chars().take(27).collect();
        alias = format!("{truncated}...");
    }

    let base = format!("[{date}] {zn} {alias} {kind}");

    if let Some(changes) = ev.get("changes").and_then(Value::as_array) {
        for change in changes {
            let pretty_path = change.get("prettyPath").and_then(Value::as_str).unwrap_or("");
            let action = change.get("action").and_then(Value::as_str).unwrap_or("");
            let old = change.get("oldValue").unwrap_or(&Value::Null);
            let new = change.get("newValue").unwrap_or(&Value::Null);
            // node `%j` is JSON.stringify of the value.
            println!(
                "{base}: {pretty_path} {action} :: {} -> {}",
                serde_json::to_string(old).unwrap_or_else(|_| "null".to_string()),
                serde_json::to_string(new).unwrap_or_else(|_| "null".to_string())
            );
        }
    } else {
        println!("{base}");
    }
}

/// node `formatDate`: full -> full ISO; else the time-of-day after `T`.
fn format_date(date: &str, full: bool) -> String {
    if full {
        return date.to_string();
    }
    date.split_once('T').map_or_else(|| date.to_string(), |(_, t)| t.to_string())
}

/// node `formatZonename`: full -> the whole name; else the first dash group.
fn format_zonename(zonename: &str, full: bool) -> String {
    if full {
        return zonename.to_string();
    }
    zonename.split('-').next().unwrap_or(zonename).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn format_date_short_takes_time() {
        assert_eq!(format_date("2026-06-24T00:30:36.089Z", false), "00:30:36.089Z");
        assert_eq!(format_date("2026-06-24T00:30:36.089Z", true), "2026-06-24T00:30:36.089Z");
    }

    #[test]
    fn format_zonename_short_takes_first_group() {
        assert_eq!(
            format_zonename("4256eeee-c8b0-4fed-9e0a-975a31e14571", false),
            "4256eeee"
        );
        assert_eq!(
            format_zonename("4256eeee-c8b0-4fed-9e0a-975a31e14571", true),
            "4256eeee-c8b0-4fed-9e0a-975a31e14571"
        );
    }

    #[test]
    fn change_serializes_with_j() {
        // Verify %j-style values: strings get quoted, numbers bare.
        assert_eq!(serde_json::to_string(&json!("stopped")).unwrap(), "\"stopped\"");
        assert_eq!(serde_json::to_string(&json!(1500)).unwrap(), "1500");
    }
}
