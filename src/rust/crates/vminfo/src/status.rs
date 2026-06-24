// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! `vminfo status [-jf]` formatting, byte-matched to node `vminfo.js`
//! `do_status`.

use std::process::ExitCode;

use serde_json::Value;

use crate::client::Client;
use crate::pretty::stringify_pretty;
use crate::{fail, usage};

/// `vminfo status`: `-j` dumps the raw status as pretty JSON; otherwise it
/// renders node's formatted block.
pub fn do_status(client: &Client, full: bool, json: bool) -> ExitCode {
    let path = if full { "/status?full=true" } else { "/status" };
    let msg = match client.get_json(path) {
        Ok(v) => v,
        Err(e) => return fail(&e),
    };

    if !msg.is_object() {
        usage(None);
        return ExitCode::FAILURE;
    }

    if json {
        println!("{}", stringify_pretty(&msg));
        return ExitCode::SUCCESS;
    }

    println!("{}", format_status(&msg));
    ExitCode::SUCCESS
}

/// Reproduce node's `output.push(...)` sequence exactly.
fn format_status(msg: &Value) -> String {
    let mut out: Vec<String> = Vec::new();

    let s = |v: &Value| v.as_str().unwrap_or("").to_string();
    let n = |v: &Value| {
        // node `%d` prints integers; numbers come back as JSON numbers.
        v.as_i64()
            .map(|x| x.to_string())
            .or_else(|| v.as_u64().map(|x| x.to_string()))
            .or_else(|| v.as_f64().map(|x| (x as i64).to_string()))
            .unwrap_or_default()
    };

    out.push(format!("state: {} ({})", s(&msg["state"]), s(&msg["status"])));
    out.push(format!("pid: {}", n(&msg["pid"])));
    out.push(format!("uptime: {}", s(&msg["uptime"])));

    let rss = msg["memory"]["rss"].as_f64().unwrap_or(0.0);
    out.push(format!("rss: {:.2}mb", rss / 1024.0 / 1024.0));
    out.push(format!("numVms: {}", n(&msg["numVms"])));

    out.push("queue".to_string());
    out.push(format!("  paused: {}", bool_str(&msg["queue"]["paused"])));
    out.push(format!("  idle: {}", bool_str(&msg["queue"]["idle"])));
    out.push(format!(
        "  npending: {}",
        n(&msg["queue"]["vasync_queue"]["npending"])
    ));
    out.push(format!(
        "  nqueued: {}",
        n(&msg["queue"]["vasync_queue"]["nqueued"])
    ));

    out.push("fullRefresh".to_string());
    // node `%s` of a null prints "null".
    out.push(format!("  lastRefresh: {}", scalar_str(&msg["lastRefresh"])));

    let refresh_errors = msg["refreshErrors"].as_array().cloned().unwrap_or_default();
    if !refresh_errors.is_empty() {
        // node has a format bug here (the count arg is misplaced), but the
        // literal text printed is "  refreshErrors: (%d items)" with no
        // substitution applied; reproduce that text verbatim.
        out.push("  refreshErrors: (%d items)".to_string());
        for o in &refresh_errors {
            out.push(format!("    - {} ({})", s(&o["err"]), s(&o["ago"])));
        }
    }

    // included with full=true
    if let Some(refresh_log) = msg.get("refreshLog").and_then(Value::as_array)
        && !refresh_log.is_empty()
    {
        out.push(format!("  refreshLog: ({} items)", refresh_log.len()));
        for o in refresh_log {
            let cc = o["cacheChanges"].as_array().map_or(0, Vec::len);
            let vc = o["vmChanges"].as_array().map_or(0, Vec::len);
            out.push(format!(
                "    - {} cacheChanges / {} vmChanges - {} ago",
                cc,
                vc,
                s(&o["endedAgo"])
            ));
        }
    }

    // included with full=true
    if let Some(fsw) = msg.get("fswatcher").filter(|v| v.is_object()) {
        out.push("fswatcher".to_string());
        out.push(format!("  running: {}", bool_str(&fsw["running"])));
        out.push(format!("  pid: {}", n(&fsw["watcher_pid"])));
        out.push(format!(
            "  watching: {}",
            fsw["watching"].as_array().map_or(0, Vec::len)
        ));
        out.push(format!(
            "  tryingToWatch: {}",
            fsw["not_yet_watching"].as_array().map_or(0, Vec::len)
        ));
        out.push(format!(
            "  pendingActions: {}",
            fsw["pending_actions"].as_object().map_or(0, serde_json::Map::len)
        ));
    }

    let listeners = msg["eventsListeners"].as_object().cloned().unwrap_or_default();
    out.push(format!("eventsListeners: ({} listeners)", listeners.len()));
    for (uuid, el) in &listeners {
        out.push(format!("  - {}", safe_user_agent(&el["userAgent"])));
        out.push(format!("    {} created {} ago", uuid, s(&el["createdAgo"])));
        out.push(String::new());
    }

    out.join("\n")
}

/// node `%s` of a bool prints "true"/"false".
fn bool_str(v: &Value) -> String {
    match v.as_bool() {
        Some(b) => b.to_string(),
        None => "undefined".to_string(),
    }
}

/// node `%s` of a string/null/number scalar.
fn scalar_str(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// node `safeUserAgent`: JSON.stringify the value, strip the surrounding
/// quotes, and un-escape embedded quotes.
fn safe_user_agent(v: &Value) -> String {
    let s = match v {
        Value::String(s) => s.clone(),
        Value::Null => return "undefined".to_string(),
        other => other.to_string(),
    };
    // JSON.stringify(s) then strip leading/trailing quote, un-escape \".
    let stringified = serde_json::to_string(&s).unwrap_or_else(|_| format!("\"{s}\""));
    let trimmed = stringified
        .strip_prefix('"')
        .and_then(|x| x.strip_suffix('"'))
        .unwrap_or(&stringified);
    trimmed.replace("\\\"", "\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> Value {
        json!({
            "pid": 46798,
            "uptime": "2758.097877148s (45.97m)",
            "memory": {"rss": 36941824, "heapTotal": 0, "heapUsed": 0},
            "state": "running",
            "status": "working",
            "numVms": 8,
            "queue": {
                "paused": false,
                "idle": true,
                "vasync_queue": {"npending": 0, "nqueued": 0}
            },
            "lastRefresh": "57.33s (57.33s)",
            "refreshErrors": [],
            "eventsListeners": {}
        })
    }

    #[test]
    fn formats_core_block() {
        let s = format_status(&sample());
        assert!(s.contains("state: running (working)"));
        assert!(s.contains("pid: 46798"));
        assert!(s.contains("uptime: 2758.097877148s (45.97m)"));
        assert!(s.contains("rss: 35.23mb"));
        assert!(s.contains("numVms: 8"));
        assert!(s.contains("  paused: false"));
        assert!(s.contains("  idle: true"));
        assert!(s.contains("  npending: 0"));
        assert!(s.contains("  nqueued: 0"));
        assert!(s.contains("  lastRefresh: 57.33s (57.33s)"));
        assert!(s.contains("eventsListeners: (0 listeners)"));
    }

    #[test]
    fn null_last_refresh_prints_null() {
        let mut v = sample();
        v["lastRefresh"] = Value::Null;
        let s = format_status(&v);
        assert!(s.contains("  lastRefresh: null"));
    }

    #[test]
    fn listeners_render() {
        let mut v = sample();
        v["eventsListeners"] = json!({
            "uuid-1": {"userAgent": "curl/8.20.0", "createdAgo": "10.85m"}
        });
        let s = format_status(&v);
        assert!(s.contains("eventsListeners: (1 listeners)"));
        assert!(s.contains("  - curl/8.20.0"));
        assert!(s.contains("    uuid-1 created 10.85m ago"));
    }

    #[test]
    fn rss_rounds_to_two_decimals() {
        // node: (rss / 1024 / 1024).toFixed(2)
        let mut v = sample();
        v["memory"]["rss"] = json!(36941824u64);
        let s = format_status(&v);
        assert!(s.contains("rss: 35.23mb"), "{s}");
    }
}
