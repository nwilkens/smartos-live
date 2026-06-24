// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! A `JSON.stringify(value, null, 2)`-compatible pretty printer.
//!
//! The node vminfo CLI prints `status -j`, `vms`, and `vm <uuid>` via
//! `JSON.stringify(msg, null, 2)`. serde_json's `to_string_pretty` is close
//! but differs on empty containers (it prints `{}`/`[]` with no inner
//! newline, which actually matches node) and key/value spacing. To guarantee
//! byte-for-byte parity we render explicitly to node's rules:
//!
//!   - 2-space indent per depth level
//!   - `"key": value` (a single space after the colon)
//!   - empty object -> `{}`, empty array -> `[]` (no inner whitespace)
//!   - non-empty containers put each element on its own indented line
//!   - strings escaped as node escapes them (control chars + `"` + `\`)
//!
//! Because the daemon already serializes VM objects in node key order and the
//! workspace's serde_json preserves insertion order, the input `Value`'s key
//! order is preserved here too.

use serde_json::Value;

/// Render `value` as `JSON.stringify(value, null, 2)`.
#[must_use]
pub fn stringify_pretty(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value, 0);
    out
}

fn indent(out: &mut String, depth: usize) {
    for _ in 0..depth * 2 {
        out.push(' ');
    }
}

fn write_value(out: &mut String, value: &Value, depth: usize) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => write_json_string(out, s),
        Value::Array(arr) => write_array(out, arr, depth),
        Value::Object(obj) => write_object(out, obj, depth),
    }
}

fn write_array(out: &mut String, arr: &[Value], depth: usize) {
    if arr.is_empty() {
        out.push_str("[]");
        return;
    }
    out.push_str("[\n");
    for (i, v) in arr.iter().enumerate() {
        indent(out, depth + 1);
        write_value(out, v, depth + 1);
        if i + 1 < arr.len() {
            out.push(',');
        }
        out.push('\n');
    }
    indent(out, depth);
    out.push(']');
}

fn write_object(out: &mut String, obj: &serde_json::Map<String, Value>, depth: usize) {
    if obj.is_empty() {
        out.push_str("{}");
        return;
    }
    out.push_str("{\n");
    let len = obj.len();
    for (i, (k, v)) in obj.iter().enumerate() {
        indent(out, depth + 1);
        write_json_string(out, k);
        out.push_str(": ");
        write_value(out, v, depth + 1);
        if i + 1 < len {
            out.push(',');
        }
        out.push('\n');
    }
    indent(out, depth);
    out.push('}');
}

/// Escape a string as JSON (node's `JSON.stringify` string rules): `"` and
/// `\` are backslash-escaped, the named control escapes are used where they
/// exist, and other control chars below 0x20 become `\u00XX`.
fn write_json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_containers() {
        assert_eq!(stringify_pretty(&json!({})), "{}");
        assert_eq!(stringify_pretty(&json!([])), "[]");
    }

    #[test]
    fn nested_pretty_matches_node() {
        let v = json!({"a": 1, "b": {"c": [1, 2]}, "d": []});
        let expected = "{\n  \"a\": 1,\n  \"b\": {\n    \"c\": [\n      1,\n      2\n    ]\n  },\n  \"d\": []\n}";
        assert_eq!(stringify_pretty(&v), expected);
    }

    #[test]
    fn preserves_key_order() {
        // With preserve_order, insertion order is kept.
        let mut m = serde_json::Map::new();
        m.insert("zonename".to_string(), json!("z"));
        m.insert("autoboot".to_string(), json!(true));
        m.insert("brand".to_string(), json!("joyent-minimal"));
        let v = Value::Object(m);
        let s = stringify_pretty(&v);
        let i_zone = s.find("zonename").expect("zonename");
        let i_auto = s.find("autoboot").expect("autoboot");
        assert!(i_zone < i_auto);
    }

    #[test]
    fn escapes_quotes_and_newlines() {
        let v = json!({"k": "a\"b\nc"});
        assert_eq!(stringify_pretty(&v), "{\n  \"k\": \"a\\\"b\\nc\"\n}");
    }

    #[test]
    fn string_value() {
        assert_eq!(stringify_pretty(&json!("pong")), "\"pong\"");
    }
}
