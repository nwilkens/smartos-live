// Lookup-string parsing and application, mirroring json.js parseLookup/lookupDatum.

use serde_json::Value;

// Parse "a.b.c" -> ["a","b","c"], "b['a']" -> ["b","['a']"], honoring an
// alternate delimiter. State machine ported from json.js parseLookup.
pub fn parse_lookup(lookup: &str, delim: char) -> Vec<String> {
    let mut bits: Vec<String> = Vec::new();
    let mut bit = String::new();
    let mut states: Vec<Option<char>> = vec![None];
    let mut escaped = false;
    let mut prev: Option<char> = None;

    for ch in lookup.chars() {
        let now_escaped = !escaped && prev == Some('\\');
        escaped = now_escaped;
        prev = Some(ch);

        if escaped {
            bit.push(ch);
            continue;
        }

        match states.last().copied().flatten() {
            None => match ch {
                '"' | '\'' => {
                    states.push(Some(ch));
                    bit.push(ch);
                }
                '[' => {
                    states.push(Some('['));
                    if !bit.is_empty() {
                        bits.push(std::mem::take(&mut bit));
                    }
                    bit.push(ch);
                }
                c if c == delim => {
                    if !bit.is_empty() {
                        bits.push(std::mem::take(&mut bit));
                    }
                }
                _ => bit.push(ch),
            },
            Some('[') => {
                bit.push(ch);
                match ch {
                    '"' | '\'' | '[' => states.push(Some(ch)),
                    ']' => {
                        states.pop();
                        if states.last().copied().flatten().is_none() {
                            bits.push(std::mem::take(&mut bit));
                        }
                    }
                    _ => {}
                }
            }
            Some('"') => {
                bit.push(ch);
                if ch == '"' {
                    states.pop();
                    if states.last().copied().flatten().is_none() {
                        bits.push(std::mem::take(&mut bit));
                    }
                }
            }
            Some('\'') => {
                bit.push(ch);
                if ch == '\'' {
                    states.pop();
                    if states.last().copied().flatten().is_none() {
                        bits.push(std::mem::take(&mut bit));
                    }
                }
            }
            Some(_) => bit.push(ch),
        }
    }

    if !bit.is_empty() {
        bits.push(bit);
    }
    bits
}

// Resolve a single bit (already a parsed lookup element) against a value.
// Returns None for any miss (mirrors json.js returning `undefined`).
fn step<'a>(datum: &'a Value, bit: &str) -> Option<&'a Value> {
    // Bracket form: ['key'] | ["key"] | [N]
    if let Some(inner) = bit.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        let inner = inner.trim();
        if (inner.starts_with('\'') && inner.ends_with('\''))
            || (inner.starts_with('"') && inner.ends_with('"'))
        {
            let key = &inner[1..inner.len() - 1];
            return datum.get(key);
        }
        if let Ok(idx) = inner.parse::<i64>() {
            return index_array(datum, idx);
        }
        // Bareword inside brackets (e.g. a[b]) is undefined in our model.
        return None;
    }

    // Negative array indexing (Python-style), per lookupDatum.
    if let Ok(n) = bit.parse::<i64>()
        && n < 0
    {
        return index_array(datum, n);
    }

    // Plain identifier or token: object key, else numeric array index.
    if let Some(v) = datum.get(bit) {
        return Some(v);
    }
    if let Ok(idx) = bit.parse::<i64>()
        && idx >= 0
    {
        return index_array(datum, idx);
    }
    None
}

fn index_array(datum: &Value, idx: i64) -> Option<&Value> {
    let arr = datum.as_array()?;
    let len = arr.len() as i64;
    let real = if idx < 0 { len + idx } else { idx };
    if real < 0 || real >= len {
        return None;
    }
    arr.get(real as usize)
}

// Apply a full parsed lookup. Returns owned clone (callers reshape into rows).
pub fn lookup_datum(datum: &Value, lookup: &[String]) -> Option<Value> {
    let mut cur = datum;
    for bit in lookup {
        cur = step(cur, bit)?;
    }
    Some(cur.clone())
}

// The "joined" lookup string used as a row key (lookup.join('.')).
pub fn join_lookup(lookup: &[String]) -> String {
    lookup.join(".")
}
