// JSON.stringify(v, null, 2) reproduction for the --json / --nictag-rules
// modes. serde_json's pretty printer differs from node for empty containers
// and key ordering, so we render explicitly to match node byte-for-byte.

use serde_json::Value;

/// JSON.stringify(v, null, indent).
pub fn pretty(v: &Value, indent: usize) -> String {
    if indent == 0 {
        return serde_json::to_string(v).unwrap_or_default();
    }
    let pad = " ".repeat(indent);
    let mut out = String::new();
    write(v, 0, &pad, &mut out);
    out
}

fn write(v: &Value, depth: usize, pad: &str, out: &mut String) {
    match v {
        Value::Array(a) => {
            if a.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push_str("[\n");
            for (i, item) in a.iter().enumerate() {
                out.push_str(&pad.repeat(depth + 1));
                write(item, depth + 1, pad, out);
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
                write(val, depth + 1, pad, out);
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
