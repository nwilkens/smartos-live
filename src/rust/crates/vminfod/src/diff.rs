// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! A faithful port of node `diff.js`, the change-record generator vminfod
//! emits in a `modify` event's `changes` array.
//!
//! Consumers match on the record shape `{path, prettyPath, action,
//! oldValue, newValue[, ident]}` (the metadata agent's `VminfodWatcher` and
//! vmadmd's `ZoneEvent` inspect `changes` to react to state transitions), so
//! the algorithm is reproduced exactly, including the `DIFF_MAP` keying that
//! treats `nics` array elements by `mac` and `disks` elements by `path`.

use serde_json::{Map, Value};

/// `DIFF_MAP` from vminfod.js: array elements under these keys are matched by
/// the named identifier instead of by position, so an in-place element edit
/// reports a nested `changed` rather than a remove+add pair.
fn diff_map(key: &str) -> Option<&'static str> {
    match key {
        "disks" => Some("path"),
        "nics" => Some("mac"),
        _ => None,
    }
}

/// One change record, serialized exactly as node's diff.js emits it.
/// `ident` is present only for map-keyed array element changes.
#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct Change {
    #[serde(rename = "prettyPath")]
    pub pretty_path: String,
    pub path: Vec<Value>,
    pub action: &'static str,
    #[serde(rename = "oldValue", skip_serializing_if = "Option::is_none")]
    pub old_value: Option<Value>,
    #[serde(rename = "newValue", skip_serializing_if = "Option::is_none")]
    pub new_value: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ident: Option<Value>,
}

/// Compute the change list between two VM objects, applying `DIFF_MAP` at the
/// top level (matching vminfod's `diff(old, new, {map: DIFF_MAP})`).
#[must_use]
pub fn diff(a: &Value, b: &Value) -> Vec<Change> {
    diff_inner(a, b, &[], None, true)
}

/// Mirror of node `diff()`: dispatch to array or object diff. `key` is the
/// parent object key (used for `DIFF_MAP`); `apply_map` is true only at the
/// top level (node applies the map only when `path.length === 2`).
fn diff_inner(a: &Value, b: &Value, prefix: &[Value], key: Option<&str>, apply_map: bool) -> Vec<Change> {
    if same_value(a, b) {
        return Vec::new();
    }
    match (a, b) {
        (Value::Array(_), Value::Array(_)) => array_diff(a, b, prefix, key, apply_map),
        _ => obj_diff(a, b, prefix),
    }
}

fn obj_diff(a: &Value, b: &Value, prefix: &[Value]) -> Vec<Change> {
    let empty = Map::new();
    let am = a.as_object().unwrap_or(&empty);
    let bm = b.as_object().unwrap_or(&empty);
    let mut changes = Vec::new();

    for (key, av) in am {
        let mut path = prefix.to_vec();
        path.push(Value::String(key.clone()));
        let pretty = to_dot_notation(&path);

        let Some(bv) = bm.get(key) else {
            changes.push(Change {
                pretty_path: pretty,
                path,
                action: "removed",
                old_value: Some(av.clone()),
                new_value: None,
                ident: None,
            });
            continue;
        };

        if same_value(av, bv) {
            continue;
        }

        // If either side is not an object/array, the value changed.
        if !is_container(av) || !is_container(bv) {
            changes.push(Change {
                pretty_path: pretty,
                path,
                action: "changed",
                old_value: Some(av.clone()),
                new_value: Some(bv.clone()),
                ident: None,
            });
            continue;
        }

        // Both are containers: recurse. node passes `key` as the 4th arg so a
        // nested array can consult DIFF_MAP, but only when path.length === 2.
        let apply = path.len() == 1;
        changes.extend(diff_inner(av, bv, &path, Some(key), apply));
    }

    for (key, bv) in bm {
        if am.contains_key(key) {
            continue;
        }
        let mut path = prefix.to_vec();
        path.push(Value::String(key.clone()));
        let pretty = to_dot_notation(&path);
        changes.push(Change {
            pretty_path: pretty,
            path,
            action: "added",
            old_value: None,
            new_value: Some(bv.clone()),
            ident: None,
        });
    }

    changes
}

fn array_diff(a: &Value, b: &Value, prefix: &[Value], key: Option<&str>, apply_map: bool) -> Vec<Change> {
    let av = a.as_array().cloned().unwrap_or_default();
    let bv = b.as_array().cloned().unwrap_or_default();

    let mut path = prefix.to_vec();
    path.push(Value::Null);
    let pretty = to_dot_notation(&path);

    // Identifier key from DIFF_MAP: node uses it when path.length === 2 and
    // the key is in the map. path here is prefix + [null]; node's check is on
    // the pre-null path length === 2, i.e. prefix.len() == 1.
    let ident_key = if apply_map && prefix.len() == 1 {
        key.and_then(diff_map)
    } else {
        None
    };

    let a_ser: Vec<String> = av.iter().map(|o| serde_json::to_string(o).unwrap_or_default()).collect();
    let b_ser: Vec<String> = bv.iter().map(|o| serde_json::to_string(o).unwrap_or_default()).collect();

    let mut possibly_removed: Vec<usize> =
        (0..av.len()).filter(|&i| !b_ser.contains(&a_ser[i])).collect();
    let mut possibly_added: Vec<usize> =
        (0..bv.len()).filter(|&j| !a_ser.contains(&b_ser[j])).collect();

    // (a_obj, b_obj, ident) tuples to recurse into.
    let mut modified: Vec<(Value, Value, Value)> = Vec::new();

    if let Some(idk) = ident_key {
        possibly_removed.retain(|&i| {
            let oi = &av[i];
            let mut found = false;
            possibly_added.retain(|&j| {
                if found {
                    return true;
                }
                let oj = &bv[j];
                if oi.get(idk) == oj.get(idk) {
                    found = true;
                    modified.push((oi.clone(), oj.clone(), oi.get(idk).cloned().unwrap_or(Value::Null)));
                    return false; // consume this added candidate
                }
                true
            });
            !found
        });
    }

    let mut changes = Vec::new();

    for &i in &possibly_removed {
        changes.push(Change {
            pretty_path: pretty.clone(),
            path: path.clone(),
            action: "removed",
            old_value: Some(av[i].clone()),
            new_value: None,
            ident: None,
        });
    }

    for (a_obj, b_obj, ident) in &modified {
        // node: objDiff(mod.a, mod.b, {prefix: path}) then tags each with ident.
        for mut ch in obj_diff(a_obj, b_obj, &path) {
            ch.ident = Some(ident.clone());
            changes.push(ch);
        }
    }

    for &j in &possibly_added {
        changes.push(Change {
            pretty_path: pretty.clone(),
            path: path.clone(),
            action: "added",
            old_value: None,
            new_value: Some(bv[j].clone()),
            ident: None,
        });
    }

    changes
}

/// node `theSameValue`: strict equality (numbers/strings/bools/null) plus
/// Date-by-time, which for our JSON values (dates are ISO strings) reduces to
/// `==`.
fn same_value(a: &Value, b: &Value) -> bool {
    a == b
}

/// node treats both objects and arrays as "object" for the recursion branch
/// (`typeof x === 'object'`); `null` is also `typeof 'object'` in JS but node
/// guards with `theSameValue` first and then recurses, where a null vs object
/// produces a change. We mirror by treating only object/array as containers
/// (null/primitive => value-changed), which yields the same records.
fn is_container(v: &Value) -> bool {
    matches!(v, Value::Object(_) | Value::Array(_))
}

/// node `toDotNotation`: join path elements with `.`, render `null` as `*`,
/// and bracket-escape any element containing `.`/`[`/`]`.
fn to_dot_notation(path: &[Value]) -> String {
    let mut s = String::new();
    for elem in path {
        let raw = match elem {
            Value::Null => "*".to_string(),
            Value::String(st) => st.clone(),
            other => other.to_string(),
        };
        if raw.contains(['.', '[', ']']) {
            let escaped = raw.replace('[', "\\[").replace(']', "\\]");
            s.push('[');
            s.push_str(&escaped);
            s.push(']');
            continue;
        }
        if !s.is_empty() {
            s.push('.');
        }
        s.push_str(&raw);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn primitive_changed() {
        let a = json!({"num": 1, "keep": true});
        let b = json!({"num": 2, "keep": true});
        let c = diff(&a, &b);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].action, "changed");
        assert_eq!(c[0].pretty_path, "num");
        assert_eq!(c[0].old_value, Some(json!(1)));
        assert_eq!(c[0].new_value, Some(json!(2)));
    }

    #[test]
    fn added_and_removed() {
        let a = json!({"foo": true});
        let b = json!({"bar": true});
        let c = diff(&a, &b);
        // foo removed, bar added.
        let removed = c.iter().find(|x| x.action == "removed").expect("removed");
        let added = c.iter().find(|x| x.action == "added").expect("added");
        assert_eq!(removed.pretty_path, "foo");
        assert_eq!(added.pretty_path, "bar");
    }

    #[test]
    fn nested_object_change() {
        let a = json!({"config": {"a": 1}});
        let b = json!({"config": {"a": 2}});
        let c = diff(&a, &b);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].pretty_path, "config.a");
        assert_eq!(c[0].action, "changed");
    }

    #[test]
    fn nics_keyed_by_mac() {
        // Same nic (by mac), only mtu changed -> a nested `changed` with ident.
        let a = json!({"nics": [{"mac": "02:00:00:00:00:01", "mtu": 1500}]});
        let b = json!({"nics": [{"mac": "02:00:00:00:00:01", "mtu": 9000}]});
        let c = diff(&a, &b);
        assert_eq!(c.len(), 1, "expected one nested change, got {c:?}");
        assert_eq!(c[0].action, "changed");
        assert_eq!(c[0].pretty_path, "nics.*.mtu");
        assert_eq!(c[0].ident, Some(json!("02:00:00:00:00:01")));
        assert_eq!(c[0].old_value, Some(json!(1500)));
        assert_eq!(c[0].new_value, Some(json!(9000)));
    }

    #[test]
    fn nics_added_removed_when_mac_differs() {
        let a = json!({"nics": [{"mac": "02:00:00:00:00:01", "mtu": 1500}]});
        let b = json!({"nics": [{"mac": "02:00:00:00:00:02", "mtu": 1500}]});
        let c = diff(&a, &b);
        // Different mac -> the old element removed, new element added.
        assert!(c.iter().any(|x| x.action == "removed"));
        assert!(c.iter().any(|x| x.action == "added"));
        assert!(c.iter().all(|x| x.pretty_path == "nics.*"));
    }

    #[test]
    fn disks_keyed_by_path() {
        let a = json!({"disks": [{"path": "/dev/zvol/dsk/zones/x/disk0", "size": 10}]});
        let b = json!({"disks": [{"path": "/dev/zvol/dsk/zones/x/disk0", "size": 20}]});
        let c = diff(&a, &b);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].pretty_path, "disks.*.size");
        assert_eq!(c[0].ident, Some(json!("/dev/zvol/dsk/zones/x/disk0")));
    }

    #[test]
    fn no_change_empty() {
        let a = json!({"a": 1, "b": [1, 2, 3]});
        assert!(diff(&a, &a).is_empty());
    }

    #[test]
    fn change_serializes_without_null_values() {
        let c = Change {
            pretty_path: "foo".to_string(),
            path: vec![json!("foo")],
            action: "added",
            old_value: None,
            new_value: Some(json!(true)),
            ident: None,
        };
        let v = serde_json::to_value(&c).expect("serialize");
        assert!(v.get("oldValue").is_none());
        assert!(v.get("ident").is_none());
        assert_eq!(v["newValue"], json!(true));
        assert_eq!(v["action"], json!("added"));
    }
}
