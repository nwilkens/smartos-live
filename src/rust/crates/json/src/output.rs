// Output rendering for the four output modes, mirroring json.js printDatum.

use serde_json::Value;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Jsony,
    Json,
    Inspect,
    Compact,
}

// Render a value to its output string, or None when nothing should be emitted
// (mirrors json.js where `undefined` produces no output).
pub fn render(datum: Option<&Value>, mode: OutputMode, indent: usize) -> Option<String> {
    match mode {
        OutputMode::Inspect => Some(inspect(datum?)),
        OutputMode::Json => datum.map(|d| stringify(d, indent)),
        OutputMode::Compact => datum.map(compact),
        OutputMode::Jsony => match datum {
            Some(Value::String(s)) => Some(s.clone()),
            Some(d) => Some(stringify(d, indent)),
            None => None,
        },
    }
}

// JSON.stringify(datum, null, indent) semantics.
pub fn stringify(v: &Value, indent: usize) -> String {
    if indent == 0 {
        return serde_json::to_string(v).unwrap_or_default();
    }
    let pad = " ".repeat(indent);
    let mut out = String::new();
    write_pretty(v, indent, 0, &pad, &mut out);
    out
}

fn write_pretty(v: &Value, _indent: usize, depth: usize, pad: &str, out: &mut String) {
    match v {
        Value::Array(a) => {
            if a.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push_str("[\n");
            for (i, item) in a.iter().enumerate() {
                out.push_str(&pad.repeat(depth + 1));
                write_pretty(item, _indent, depth + 1, pad, out);
                if i + 1 < a.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&pad.repeat(depth));
            out.push(']');
        }
        Value::Object(o) => {
            if o.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push_str("{\n");
            let len = o.len();
            for (i, (k, val)) in o.iter().enumerate() {
                out.push_str(&pad.repeat(depth + 1));
                out.push_str(&serde_json::to_string(&Value::String(k.clone())).unwrap_or_default());
                out.push_str(": ");
                write_pretty(val, _indent, depth + 1, pad, out);
                if i + 1 < len {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&pad.repeat(depth));
            out.push('}');
        }
        _ => out.push_str(&serde_json::to_string(v).unwrap_or_default()),
    }
}

// OM_COMPACT: arrays one element per line, each element single-line JSON with
// ", " between members; objects/scalars are single-line.
fn compact(v: &Value) -> String {
    match v {
        Value::Array(a) => {
            let mut s = String::from("[\n");
            for item in a {
                s.push_str("  ");
                s.push_str(&compact_inline(item));
                s.push_str(",\n");
            }
            if a.is_empty() {
                s.push('\n');
            } else {
                s.truncate(s.len() - 2);
                s.push('\n');
            }
            s.push(']');
            s
        }
        // Non-array: plain single-line JSON (no spacing transform).
        _ => serde_json::to_string(v).unwrap_or_default(),
    }
}

// Array element: JSON.stringify(d,null,0) with the json.js ',"' -> ', "'
// regex applied (only between members, not in ",:" sequences).
fn compact_inline(v: &Value) -> String {
    let base = serde_json::to_string(v).unwrap_or_default();
    base.replace(",\"", ", \"")
}

// ---- Node legacy util.inspect (v0.10-era) reproduction ----
//
// Matches node for scalars, strings, arrays, and objects whose entries each
// render on a single line (the common interactive case). Known gap: when a
// nested value itself spans multiple lines, node 0.10 breaks the key onto its
// own line and reindents the child by the key column; we keep the child inline
// under the key. No SmartOS consumer uses inspect mode in scripts.

pub fn inspect(v: &Value) -> String {
    inspect_value(v)
}

fn inspect_value(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => inspect_string(s),
        Value::Array(a) => inspect_array(a),
        Value::Object(o) => inspect_object(o),
    }
}

// Single-quoted with JS-style escaping, switching quotes like node does only
// for embedded single quotes via backslash.
fn inspect_string(s: &str) -> String {
    let mut out = String::from("'");
    for c in s.chars() {
        match c {
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('\'');
    out
}

// Object key: bareword if a valid JS identifier, else single-quoted.
fn inspect_key(k: &str) -> String {
    let is_ident = !k.is_empty()
        && k.chars().enumerate().all(|(i, c)| {
            if i == 0 {
                c == '_' || c == '$' || c.is_ascii_alphabetic()
            } else {
                c == '_' || c == '$' || c.is_ascii_alphanumeric()
            }
        });
    if is_ident {
        k.to_string()
    } else {
        inspect_string(k)
    }
}

fn inspect_array(a: &[Value]) -> String {
    if a.is_empty() {
        return "[]".into();
    }
    let parts: Vec<String> = a.iter().map(inspect_value).collect();
    assemble(&parts, "[", "]", false)
}

fn inspect_object(o: &serde_json::Map<String, Value>) -> String {
    if o.is_empty() {
        return "{}".into();
    }
    let parts: Vec<String> = o
        .iter()
        .map(|(k, v)| format!("{}: {}", inspect_key(k), inspect_value(v)))
        .collect();
    assemble(&parts, "{", "}", true)
}

// Join entries inline if total length stays under node's ~72 budget and none
// span multiple lines; otherwise one entry per line with 2-space indent.
fn assemble(parts: &[String], open: &str, close: &str, braces: bool) -> String {
    let total: usize = parts.iter().map(|p| p.len()).sum::<usize>() + (parts.len() - 1) * 2;
    let multiline = parts.iter().any(|p| p.contains('\n'));
    if !multiline && total + open.len() + close.len() + 2 <= 72 {
        if braces {
            return format!("{} {} {}", open, parts.join(", "), close);
        }
        return format!("{} {} {}", open, parts.join(", "), close);
    }
    let indented: Vec<String> = parts
        .iter()
        .map(|p| {
            p.split('\n')
                .enumerate()
                .map(|(i, l)| if i == 0 { l.to_string() } else { format!("  {l}") })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect();
    format!("{} {} {}", open, indented.join(",\n  "), close)
}
