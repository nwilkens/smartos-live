// Input transformation: plain parse, grouping, and merge, mirroring
// json.js parseInput. Regex rewrites are reproduced with manual scanning to
// avoid a regex dependency for the few fixed patterns used.

use serde_json::Value;

pub enum Merge {
    Shallow,
    Deep,
}

// Rewrite "}...{"  / "}\n{" object boundaries into ",\n" joins. Mirrors the
// two replace() passes used for grouping/merging of adjacent objects.
fn join_object_boundaries(buf: &str) -> String {
    // Pattern 1: '}' WS*\n WS* '{'  -> '},\n{'
    let mut s = replace_ws_newline(buf, '}', '{');
    // Pattern 2: '}{"' -> '},\n{"'
    s = s.replace("}{\"", "},\n{\"");
    s
}

// Replace occurrences of `open` <ws>*\n<ws>* `close` with `open`,\n`close`.
fn replace_ws_newline(buf: &str, open: char, close: char) -> String {
    let bytes: Vec<char> = buf.chars().collect();
    let mut out = String::with_capacity(buf.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == open {
            // look ahead: ws* then '\n' then ws* then close
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j] == ' ' || bytes[j] == '\t' || bytes[j] == '\r') {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == '\n' {
                let mut k = j + 1;
                while k < bytes.len() && (bytes[k] == ' ' || bytes[k] == '\t' || bytes[k] == '\r') {
                    k += 1;
                }
                if k < bytes.len() && bytes[k] == close {
                    out.push(open);
                    out.push(',');
                    out.push('\n');
                    out.push(close);
                    i = k + 1;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

fn join_array_boundaries(buf: &str) -> String {
    // '] ...\n... [' -> ',\n' and '][' -> ',\n'
    let s = replace_ws_newline_arr(buf);
    s.replace("][", ",\n")
}

fn replace_ws_newline_arr(buf: &str) -> String {
    let bytes: Vec<char> = buf.chars().collect();
    let mut out = String::with_capacity(buf.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == ']' {
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j] == ' ' || bytes[j] == '\t' || bytes[j] == '\r') {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == '\n' {
                let mut k = j + 1;
                while k < bytes.len() && (bytes[k] == ' ' || bytes[k] == '\t' || bytes[k] == '\r')
                {
                    k += 1;
                }
                if k < bytes.len() && bytes[k] == '[' {
                    out.push(',');
                    out.push('\n');
                    i = k + 1;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

pub fn parse_plain(buf: &str) -> Result<Value, serde_json::Error> {
    serde_json::from_str(buf)
}

pub fn parse_group(buf: &str) -> Result<Value, serde_json::Error> {
    let mut nb = join_object_boundaries(buf);
    nb = join_array_boundaries(&nb);
    let mut nb = nb.trim().to_string();
    if !nb.starts_with('[') {
        nb = format!("[\n{nb}");
    }
    if !nb.ends_with(']') {
        nb = format!("{nb}\n]\n");
    }
    serde_json::from_str(&nb)
}

pub fn parse_merge(buf: &str, merge: &Merge) -> Result<Value, serde_json::Error> {
    let nb = join_object_boundaries(buf);
    let wrapped = format!("[\n{nb}\n]\n");
    let objs: Vec<Value> = serde_json::from_str(&wrapped)?;
    let mut iter = objs.into_iter();
    let mut merged = iter.next().unwrap_or(Value::Null);
    for obj in iter {
        match merge {
            Merge::Shallow => shallow_extend(&mut merged, obj),
            Merge::Deep => deep_extend(&mut merged, obj),
        }
    }
    Ok(merged)
}

fn shallow_extend(a: &mut Value, b: Value) {
    if let (Some(am), Value::Object(bm)) = (a.as_object_mut(), b) {
        for (k, v) in bm {
            am.insert(k, v);
        }
    }
}

fn deep_extend(a: &mut Value, b: Value) {
    let (Some(am), Value::Object(bm)) = (a.as_object_mut(), b) else {
        return;
    };
    for (k, v) in bm {
        match (am.get_mut(&k), &v) {
            (Some(av), Value::Object(_)) if av.is_object() => deep_extend(av, v),
            _ => {
                am.insert(k, v);
            }
        }
    }
}

// Strip a leading HTTP header block. Returns (header_to_passthrough, body).
// Mirrors the while-loop in main() including the 100-Continue retry.
pub fn strip_http_header(buf: &str, drop_headers: bool) -> (String, String) {
    let mut passthrough = String::new();
    let mut s = buf.to_string();
    loop {
        if !s.starts_with("HTTP/") {
            break;
        }
        let (idx, seplen) = match s.find("\r\n\r\n") {
            Some(i) => (i, 4),
            None => match s.find("\n\n") {
                Some(i) => (i, 2),
                None => break,
            },
        };
        if !drop_headers {
            passthrough.push_str(&s[..idx + seplen]);
        }
        let is_100 = s.starts_with("HTTP/1.1 100 Continue");
        s = s[idx + seplen..].to_string();
        if is_100 {
            continue;
        }
        break;
    }
    (passthrough, s)
}
