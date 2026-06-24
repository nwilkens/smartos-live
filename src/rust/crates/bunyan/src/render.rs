// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Long/short human renderers, ported field-for-field from node-bunyan's
//! `emitRecord`. Mutates a clone of the record map (deleting consumed keys and
//! hoisting leftover `req.*`/`res.*`/`err.*` fields) to match node's ordering.

use serde_json::{Map, Value};

use crate::level;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Long,
    Short,
    Json,
    Bunyan,
}

pub struct RenderOpts {
    pub mode: OutputMode,
    pub color: bool,
    pub json_indent: usize,
}

/// Render a valid record to its output string (no trailing newline handling
/// here beyond what node emits inline). Returns the full block.
pub fn emit_record(rec: &Map<String, Value>, opts: &RenderOpts) -> String {
    match opts.mode {
        OutputMode::Json => to_pretty(&Value::Object(rec.clone()), opts.json_indent),
        OutputMode::Bunyan => serde_json::to_string(&Value::Object(rec.clone()))
            .unwrap_or_default(),
        OutputMode::Long | OutputMode::Short => long_or_short(rec, opts),
    }
}

fn indent(s: &str) -> String {
    let mut out = String::from("    ");
    out.push_str(&s.split('\n').collect::<Vec<_>>().join("\n    "));
    out
}

fn stylize(color: bool, s: &str, c: &str) -> String {
    if !color {
        return s.to_string();
    }
    level::stylize_color(s, c)
}

/// node's `JSON.stringify(v, null, n)` produces `null` for n==0 path elsewhere,
/// but for leftover fields it always uses indent 2. This honors a configurable
/// indent for the `-o json[-N]` mode.
fn to_pretty(v: &Value, n: usize) -> String {
    if n == 0 {
        return serde_json::to_string(v).unwrap_or_default();
    }
    let indent_bytes = vec![b' '; n];
    let fmt = serde_json::ser::PrettyFormatter::with_indent(&indent_bytes);
    let mut buf = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, fmt);
    if serde::Serialize::serialize(v, &mut ser).is_err() {
        return String::new();
    }
    String::from_utf8(buf).unwrap_or_default()
}

/// Stringify a JSON value as a header/leftover value with 2-space indent,
/// matching node `JSON.stringify(value, null, 2)`.
fn stringify2(v: &Value) -> String {
    to_pretty(v, 2)
}

fn trim_right(s: &str) -> String {
    s.trim_end().to_string()
}

fn long_or_short(orig: &Map<String, Value>, opts: &RenderOpts) -> String {
    let short = opts.mode == OutputMode::Short;
    let mut rec = orig.clone();

    rec.shift_remove("v");

    // time
    let time_raw = rec
        .get("time")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let time = if short && time_raw.as_bytes().get(10) == Some(&b'T') {
        stylize(opts.color, &time_raw[11..], "XXX")
    } else {
        stylize(opts.color, &format!("[{time_raw}]"), "XXX")
    };
    rec.shift_remove("time");

    // name[/component][/pid]
    let mut name_str = rec
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    rec.shift_remove("name");
    if let Some(comp) = rec.get("component").and_then(|v| v.as_str()) {
        name_str.push('/');
        name_str.push_str(comp);
    }
    rec.shift_remove("component");
    if !short {
        name_str.push('/');
        name_str.push_str(&value_to_plain(rec.get("pid")));
    }
    rec.shift_remove("pid");

    // level
    let lvl = rec.get("level").and_then(Value::as_i64).unwrap_or(0);
    let mut level_str = level::upper_padded(lvl);
    if opts.color
        && let Some(c) = level::color_from_level(lvl)
    {
        level_str = level::stylize_color(&level_str, c);
    }
    rec.shift_remove("level");

    // src
    let mut src = String::new();
    if let Some(s) = rec.get("src").and_then(Value::as_object)
        && let Some(file) = s.get("file").and_then(|v| v.as_str())
    {
        let line = value_to_plain(s.get("line"));
        src = match s.get("func").and_then(|v| v.as_str()) {
            Some(func) => format!(" ({file}:{line} in {func})"),
            None => format!(" ({file}:{line})"),
        };
        src = stylize(opts.color, &src, "green");
    }
    rec.shift_remove("src");

    let hostname = rec
        .get("hostname")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    rec.shift_remove("hostname");

    let mut extras: Vec<String> = Vec::new();
    let mut details: Vec<String> = Vec::new();

    if let Some(req_id) = rec.get("req_id") {
        extras.push(format!("req_id={}", value_to_plain(Some(req_id))));
    }
    rec.shift_remove("req_id");

    // msg
    let msg = rec.get("msg").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let oneline_msg = if msg.contains('\n') {
        details.push(indent(&stylize(opts.color, &msg, "cyan")));
        String::new()
    } else {
        format!(" {}", stylize(opts.color, &msg, "cyan"))
    };
    rec.shift_remove("msg");

    handle_req(&mut rec, &mut details);
    handle_client_req(&mut rec, &mut details);

    if let Some(res) = rec.get("res").and_then(Value::as_object).cloned() {
        emit_res(&res, &mut rec, &mut details);
        rec.shift_remove("res");
    }
    if let Some(cres) = rec.get("client_res").and_then(Value::as_object).cloned() {
        emit_res(&cres, &mut rec, &mut details);
        rec.shift_remove("res");
    }

    handle_err(&mut rec, &mut details);

    // leftover fields
    let leftover: Vec<String> = rec.keys().cloned().collect();
    for key in leftover {
        let value = rec.get(&key).cloned().unwrap_or(Value::Null);
        let (rendered, stringified) = match &value {
            Value::String(s) => (s.clone(), false),
            other => (stringify2(other), true),
        };
        if rendered.contains('\n') || rendered.len() > 50 {
            details.push(indent(&format!("{key}: {rendered}")));
        } else if !stringified && (rendered.contains(' ') || rendered.is_empty()) {
            extras.push(format!("{key}={}", json_quote(&rendered)));
        } else {
            extras.push(format!("{key}={rendered}"));
        }
    }

    let extras_str = stylize(
        opts.color,
        &(if extras.is_empty() {
            String::new()
        } else {
            format!(" ({})", extras.join(", "))
        }),
        "XXX",
    );
    let details_str = stylize(
        opts.color,
        &(if details.is_empty() {
            String::new()
        } else {
            format!("{}\n", details.join("\n    --\n"))
        }),
        "XXX",
    );

    if !short {
        format!(
            "{} {}: {} on {}{}:{}{}\n{}",
            time,
            level_str,
            name_str,
            hostname.unwrap_or_else(|| "<no-hostname>".to_string()),
            src,
            oneline_msg,
            extras_str,
            details_str
        )
    } else {
        format!(
            "{} {} {}:{}{}\n{}",
            time, level_str, name_str, oneline_msg, extras_str, details_str
        )
    }
}

fn handle_req(rec: &mut Map<String, Value>, details: &mut Vec<String>) {
    let Some(mut req) = rec.get("req").and_then(Value::as_object).cloned() else {
        return;
    };
    rec.shift_remove("req");

    let headers = match req.get("headers") {
        None => String::new(),
        Some(Value::String(s)) => format!("\n{s}"),
        Some(Value::Object(map)) => {
            let lines: Vec<String> =
                map.iter().map(|(h, v)| format!("{h}: {}", value_to_plain(Some(v)))).collect();
            format!("\n{}", lines.join("\n"))
        }
        Some(_) => String::new(),
    };

    let method = req.get("method").map(|v| value_to_plain(Some(v))).unwrap_or_default();
    let url = req.get("url").map(|v| value_to_plain(Some(v))).unwrap_or_default();
    let http_ver = req
        .get("httpVersion")
        .map(|v| value_to_plain(Some(v)))
        .unwrap_or_else(|| "1.1".to_string());
    let mut s = format!("{method} {url} HTTP/{http_ver}{headers}");
    req.shift_remove("url");
    req.shift_remove("method");
    req.shift_remove("httpVersion");
    req.shift_remove("headers");

    if let Some(body) = req.get("body") {
        let b = match body {
            Value::Object(_) | Value::Array(_) => stringify2(body),
            other => value_to_plain(Some(other)),
        };
        s.push_str(&format!("\n\n{b}"));
        req.shift_remove("body");
    }
    // node's trailers check is `Object.keys > 0` (a buggy length-vs-number
    // compare that is effectively never true); preserve that behavior.
    req.shift_remove("trailers");

    details.push(indent(&s));
    for (k, v) in &req {
        rec.insert(format!("req.{k}"), v.clone());
    }
}

fn handle_client_req(rec: &mut Map<String, Value>, details: &mut Vec<String>) {
    let Some(mut creq) = rec.get("client_req").and_then(Value::as_object).cloned() else {
        return;
    };
    rec.shift_remove("client_req");

    let headers = creq.get("headers").cloned();
    let mut host_line = String::new();
    if let Some(addr) = creq.get("address").map(|v| value_to_plain(Some(v))) {
        host_line = format!("Host: {addr}");
        if let Some(port) = creq.get("port") {
            host_line.push_str(&format!(":{}", value_to_plain(Some(port))));
        }
        host_line.push('\n');
    }
    creq.shift_remove("headers");
    creq.shift_remove("address");
    creq.shift_remove("port");

    let method = creq.get("method").map(|v| value_to_plain(Some(v))).unwrap_or_default();
    let url = creq.get("url").map(|v| value_to_plain(Some(v))).unwrap_or_default();
    let http_ver = creq
        .get("httpVersion")
        .map(|v| value_to_plain(Some(v)))
        .unwrap_or_else(|| "1.1".to_string());
    let hdr_str = match headers {
        Some(Value::Object(map)) => map
            .iter()
            .map(|(h, v)| format!("{h}: {}", value_to_plain(Some(v))))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    };
    let mut s = format!("{method} {url} HTTP/{http_ver}\n{host_line}{hdr_str}");
    creq.shift_remove("method");
    creq.shift_remove("url");
    creq.shift_remove("httpVersion");

    if let Some(body) = creq.get("body") {
        let b = match body {
            Value::Object(_) | Value::Array(_) => stringify2(body),
            other => value_to_plain(Some(other)),
        };
        s.push_str(&format!("\n\n{b}"));
        creq.shift_remove("body");
    }
    for (k, v) in &creq {
        rec.insert(format!("client_req.{k}"), v.clone());
    }
    details.push(indent(&s));
}

fn emit_res(res_in: &Map<String, Value>, rec: &mut Map<String, Value>, details: &mut Vec<String>) {
    let mut res = res_in.clone();
    let mut s = String::new();
    if let Some(sc) = res.get("statusCode") {
        let code = value_to_plain(Some(sc));
        let reason = status_reason(sc.as_i64().unwrap_or(-1));
        s.push_str(&format!("HTTP/1.1 {code} {reason}\n"));
        res.shift_remove("statusCode");
    }

    let headers = if res.contains_key("header") {
        let h = res.get("header").cloned();
        res.shift_remove("header");
        h
    } else if res.contains_key("headers") {
        let h = res.get("headers").cloned();
        res.shift_remove("headers");
        h
    } else {
        None
    };

    match &headers {
        None | Some(Value::Null) => {}
        Some(Value::String(hs)) => s.push_str(&trim_right(hs)),
        Some(Value::Object(map)) => {
            let joined = map
                .iter()
                .map(|(h, v)| format!("{h}: {}", value_to_plain(Some(v))))
                .collect::<Vec<_>>()
                .join("\n");
            s.push_str(&joined);
        }
        Some(_) => {}
    }

    if let Some(body) = res.get("body") {
        let b = match body {
            Value::Object(_) | Value::Array(_) => stringify2(body),
            other => value_to_plain(Some(other)),
        };
        s.push_str(&format!("\n\n{b}"));
        res.shift_remove("body");
    } else {
        s = trim_right(&s);
    }

    if let Some(tr) = res.get("trailer") {
        s.push_str(&format!("\n{}", value_to_plain(Some(tr))));
    }
    res.shift_remove("trailer");

    if !s.is_empty() {
        details.push(indent(&s));
    }
    for (k, v) in &res {
        rec.insert(format!("res.{k}"), v.clone());
    }
}

fn handle_err(rec: &mut Map<String, Value>, details: &mut Vec<String>) {
    let has_stack = rec
        .get("err")
        .and_then(Value::as_object)
        .and_then(|e| e.get("stack"))
        .and_then(|v| v.as_str())
        .is_some();
    if !has_stack {
        return;
    }
    let Some(mut err) = rec.get("err").and_then(Value::as_object).cloned() else {
        return;
    };
    if let Some(stack) = err.get("stack").and_then(|v| v.as_str()) {
        details.push(indent(stack));
    }
    err.shift_remove("message");
    err.shift_remove("name");
    err.shift_remove("stack");
    for (k, v) in &err {
        rec.insert(format!("err.{k}"), v.clone());
    }
    rec.shift_remove("err");
}

/// Plain string form of a JSON scalar as node's string concatenation produces
/// (numbers/bools without quotes, strings as-is).
fn value_to_plain(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => n.to_string(),
        Some(other) => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// node's `JSON.stringify(str)` for a plain string value.
fn json_quote(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

include!("status.rs");
