// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The `imageInfo` value and its table/JSON rendering.
//!
//! Node imgadm treats an image manifest as opaque JSON: it parses
//! the IMGAPI/db payload with `JSON.parse` and echoes it back with
//! `JSON.stringify(..., 2)` without ever forcing it through a typed
//! schema. We mirror that exactly with `serde_json::Value`, so that
//! sub-second `published_at` precision, non-string `tags` values,
//! and field ordering all round-trip byte-for-byte. The typed
//! `imgapi_manifest::Manifest` is used only for *validation* on the
//! import path, never for display or persistence.

use serde_json::{Map, Value};

use crate::error::{Error, Result};

/// One installed/available image record. Serializes to the exact
/// shape node emits for `list -j` / `get`:
/// `{manifest, [source], [zpool], [children], [cloneNames], [clones]}`.
///
/// `base` carries the original on-disk db object's key order
/// (`manifest`, and whichever of `source`/`zpool` it stored, in the
/// order the file wrote them) so `to_value` reproduces node's
/// `JSON.parse`-preserved ordering byte-for-byte. Computed fields
/// (`children`/`cloneNames`/`clones`) are appended after, exactly as
/// node's `_loadImages` mutates the parsed object.
#[derive(Debug, Clone)]
pub struct ImageInfo {
    pub manifest: Value,
    pub zpool: Option<String>,
    pub source: Option<String>,
    pub clones: Option<u64>,
    pub clone_names: Option<Vec<String>>,
    pub children: Option<Value>,
    /// The ordered db object keys (sans the computed fields), if this
    /// info was loaded from a db file. None for remote (avail) images.
    pub base: Option<Map<String, Value>>,
}

impl ImageInfo {
    /// Read a string field off the manifest (`manifest.<key>`).
    pub fn mstr(&self, key: &str) -> Option<&str> {
        self.manifest.get(key).and_then(Value::as_str)
    }

    pub fn uuid(&self) -> Option<&str> {
        self.mstr("uuid")
    }

    pub fn origin(&self) -> Option<&str> {
        self.mstr("origin")
    }

    /// `files[0].size`, if present.
    pub fn file_size(&self) -> Option<u64> {
        self.manifest
            .get("files")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|f| f.get("size"))
            .and_then(Value::as_u64)
    }

    /// Serialize to the node-compatible imageInfo object. The base
    /// object's key order (from the db file, or `manifest`+`source`
    /// for a remote image) is preserved, then the computed fields are
    /// appended: `children?`, `cloneNames?`, `clones?`.
    pub fn to_value(&self) -> Value {
        let mut m = match &self.base {
            Some(b) => b.clone(),
            None => {
                // Remote (avail) image: node pushes `{manifest, source}`.
                let mut m = Map::new();
                m.insert("manifest".to_string(), self.manifest.clone());
                if let Some(s) = &self.source {
                    m.insert("source".to_string(), Value::String(s.clone()));
                }
                m
            }
        };
        if let Some(c) = &self.children {
            m.insert("children".to_string(), c.clone());
        }
        if let Some(cn) = &self.clone_names {
            m.insert(
                "cloneNames".to_string(),
                Value::Array(cn.iter().cloned().map(Value::String).collect()),
            );
        }
        if let Some(c) = self.clones {
            m.insert("clones".to_string(), Value::Number(c.into()));
        }
        Value::Object(m)
    }
}

/// A "row": the manifest fields plus the computed convenience
/// fields node adds in `rowFromImageInfo`. Stored as a flat
/// `serde_json::Map` so column lookups (incl. dotted `a.b`) and
/// filters work against any manifest field uniformly.
pub fn row_from_image_info(info: &ImageInfo) -> Map<String, Value> {
    let mut row: Map<String, Value> = match &info.manifest {
        Value::Object(m) => m.clone(),
        _ => Map::new(),
    };

    if let Some(pa) = row.get("published_at").and_then(Value::as_str).map(str::to_string) {
        // published_date / pub: just the date (first 10 chars).
        let date: String = pa.chars().take(10).collect();
        row.insert("published_date".to_string(), Value::String(date.clone()));
        row.insert("pub".to_string(), Value::String(date));
        // published: drop trailing ".<millis>Z" -> "Z".
        let published = strip_millis(&pa);
        row.insert("published".to_string(), Value::String(published));
    }

    if let Some(s) = &info.source {
        row.insert("source".to_string(), Value::String(s.clone()));
    }
    if let Some(c) = info.clones {
        row.insert("clones".to_string(), Value::Number(c.into()));
    }
    if let Some(z) = &info.zpool {
        row.insert("zpool".to_string(), Value::String(z.clone()));
    }
    if let Some(size) = info.file_size() {
        row.insert("size".to_string(), Value::Number(size.into()));
    }

    row
}

/// Replace a trailing `.<digits>Z` with `Z`, matching node's
/// `replace(/\.\d+Z$/, 'Z')`.
fn strip_millis(s: &str) -> String {
    if let Some(stripped) = s.strip_suffix('Z')
        && let Some(dot) = stripped.rfind('.')
        && dot + 1 < stripped.len()
        && stripped[dot + 1..].chars().all(|c| c.is_ascii_digit())
    {
        return format!("{}Z", &stripped[..dot]);
    }
    s.to_string()
}

/// Look up a (possibly dotted) field on a row, returning the value
/// as a display string (node coerces with `String(actual)`).
fn lookup_display(row: &Map<String, Value>, field: &str) -> Option<String> {
    lookup_value(row, field).map(value_to_display)
}

fn value_to_display(v: &Value) -> String {
    match v {
        Value::Null => "-".to_string(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// Render rows as a tabula-style table to stdout.
///
/// Columns are left-aligned and padded with spaces to the widest
/// cell in the column; cells are separated by two spaces; the final
/// column is not right-padded (no trailing whitespace). Header cells
/// are the uppercased lookup name. Missing cells render as `-`.
pub fn print_table(
    rows: &[Map<String, Value>],
    columns: &[String],
    sort: &[String],
    skip_header: bool,
) {
    let mut idx: Vec<usize> = (0..rows.len()).collect();
    if !sort.is_empty() {
        idx.sort_by(|&a, &b| {
            for key in sort {
                let (key, desc) = if let Some(k) = key.strip_prefix('-') {
                    (k, true)
                } else {
                    (key.as_str(), false)
                };
                let va = lookup_display(&rows[a], key).unwrap_or_default();
                let vb = lookup_display(&rows[b], key).unwrap_or_default();
                let ord = va.cmp(&vb);
                let ord = if desc { ord.reverse() } else { ord };
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
            std::cmp::Ordering::Equal
        });
    }

    // Build the cell matrix (header + body).
    let headers: Vec<String> = columns.iter().map(|c| c.to_uppercase()).collect();
    let mut matrix: Vec<Vec<String>> = Vec::with_capacity(idx.len() + 1);
    if !skip_header {
        matrix.push(headers.clone());
    }
    for &i in &idx {
        let row = &rows[i];
        let cells: Vec<String> = columns
            .iter()
            .map(|c| lookup_display(row, c).unwrap_or_else(|| "-".to_string()))
            .collect();
        matrix.push(cells);
    }

    // Column widths = max cell length per column.
    let ncols = columns.len();
    let mut widths = vec![0usize; ncols];
    for line in &matrix {
        for (c, cell) in line.iter().enumerate() {
            if c < ncols {
                widths[c] = widths[c].max(cell.chars().count());
            }
        }
    }

    for line in &matrix {
        let mut out = String::new();
        for (c, cell) in line.iter().enumerate() {
            if c > 0 {
                out.push_str("  ");
            }
            if c + 1 == line.len() {
                // Last column: no padding.
                out.push_str(cell);
            } else {
                let pad = widths[c].saturating_sub(cell.chars().count());
                out.push_str(cell);
                for _ in 0..pad {
                    out.push(' ');
                }
            }
        }
        println!("{out}");
    }
}

/// Parse `FIELD=VALUE` filter args (node `filtersFromArgs`).
pub fn filters_from_args(args: &[String]) -> Result<Vec<(String, FilterVal)>> {
    let mut out = Vec::new();
    for arg in args {
        let Some(idx) = arg.find('=') else {
            return Err(Error::Usage(format!(
                "invalid filter: \"{arg}\" (must be of the form \"field=value\")"
            )));
        };
        let field = arg[..idx].to_string();
        let val = &arg[idx + 1..];
        let fv = match val {
            "true" => FilterVal::Bool(true),
            "false" => FilterVal::Bool(false),
            s if s.starts_with('~') => FilterVal::Substr(s[1..].to_string()),
            s => FilterVal::Exact(s.to_string()),
        };
        out.push((field, fv));
    }
    Ok(out)
}

#[derive(Debug, Clone)]
pub enum FilterVal {
    Bool(bool),
    Substr(String),
    Exact(String),
}

/// Apply filters (node `filterImagesInfo`). Returns true if kept.
pub fn matches_filters(info: &ImageInfo, filters: &[(String, FilterVal)]) -> bool {
    if filters.is_empty() {
        return true;
    }
    let row = row_from_image_info(info);
    for (field, val) in filters {
        let actual = lookup_value(&row, field);
        let Some(actual) = actual else {
            return false;
        };
        match val {
            FilterVal::Bool(b) => {
                if actual.as_bool() != Some(*b) {
                    return false;
                }
            }
            FilterVal::Substr(sub) => match actual.as_str() {
                Some(s) if s.contains(sub.as_str()) => {}
                _ => return false,
            },
            FilterVal::Exact(want) => {
                if &value_to_display(actual) != want {
                    return false;
                }
            }
        }
    }
    true
}

/// Look up a possibly-dotted field, returning the raw value.
fn lookup_value<'a>(row: &'a Map<String, Value>, field: &str) -> Option<&'a Value> {
    let mut cur: &Value = row.get(field.split('.').next()?)?;
    for part in field.split('.').skip(1) {
        cur = cur.as_object()?.get(part)?;
    }
    Some(cur)
}
