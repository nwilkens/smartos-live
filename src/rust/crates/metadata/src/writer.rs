// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The on-disk metadata writer for PUT/DELETE.
//!
//! Reproduces node `VM.js` `updateMetadata`/`writeAllMetadata`: a write merges
//! the requested key into the zone's live `customer_metadata`/`internal_metadata`
//! and persists the pair to `<zonepath>/config/metadata.json`, written
//! atomically (temp file + rename) so a reader never observes a partial file.
//! vminfod observes the rename and emits a `modify` event, which refreshes the
//! agent's cache; the agent itself does not mutate the cache here, mirroring the
//! node agent's reliance on the vmadm/vminfod path.
//!
//! The persisted object is `{"internal_metadata": {...}, "customer_metadata":
//! {...}}` (internal first, both always present), serialized with a two-space
//! indent to match `JSON.stringify(d, null, 2)`.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::cache::VmCache;
use crate::handler::MetadataWriter;
use crate::resolver::WriteOp;

/// Persists PUT/DELETE to `<zonepath>/config/metadata.json`, merging against the
/// zone's current metadata from the cache.
pub struct FileWriter {
    cache: VmCache,
}

impl FileWriter {
    #[must_use]
    pub fn new(cache: VmCache) -> Self {
        Self { cache }
    }

    /// The current metadata map (`customer_metadata` or `internal_metadata`)
    /// from the cached VM object, or an empty map if absent. Preserves vminfod's
    /// key order via serde_json's `preserve_order`.
    fn current_map(vm: &Value, field: &str) -> Map<String, Value> {
        vm.get(field)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default()
    }

    /// The zonepath for `uuid`: the cached object's `zonepath`, else the
    /// conventional `/zones/<uuid>`.
    fn zonepath(&self, uuid: &str, vm: &Value) -> PathBuf {
        let zp = vm
            .get("zonepath")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("/zones/{uuid}"));
        PathBuf::from(zp)
    }
}

impl MetadataWriter for FileWriter {
    fn apply(&self, uuid: &str, op: &WriteOp) -> Result<(), String> {
        let Some(vm) = self.cache.get(uuid) else {
            return Err(format!("unknown zone {uuid}"));
        };

        let mut customer = Self::current_map(&vm, "customer_metadata");
        let mut internal = Self::current_map(&vm, "internal_metadata");

        match op {
            WriteOp::Set {
                key,
                value,
                internal: is_internal,
            } => {
                let target = if *is_internal { &mut internal } else { &mut customer };
                target.insert(key.clone(), Value::String(value.clone()));
            }
            WriteOp::Remove {
                key,
                internal: is_internal,
            } => {
                let target = if *is_internal { &mut internal } else { &mut customer };
                target.remove(key);
            }
        }

        // node writes `d` with internal_metadata first, then customer_metadata,
        // and always persists both fields together.
        let mut doc = Map::new();
        doc.insert("internal_metadata".to_string(), Value::Object(internal));
        doc.insert("customer_metadata".to_string(), Value::Object(customer));

        let body = serde_json::to_string_pretty(&Value::Object(doc))
            .map_err(|e| format!("serialize metadata: {e}"))?;

        let zonepath = self.zonepath(uuid, &vm);
        let dest = zonepath.join("config").join("metadata.json");
        write_atomic(&dest, body.as_bytes())
            .map_err(|e| format!("write {}: {e}", dest.display()))
    }
}

/// Write `data` to `dest` atomically: write a sibling `<dest>.new`, fsync, then
/// rename over `dest`. Matches node `writeAndRename` (temp suffix `.new`).
fn write_atomic(dest: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let mut tmp = dest.as_os_str().to_owned();
    tmp.push(".new");
    let tmp = PathBuf::from(tmp);

    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }

    if let Err(e) = std::fs::rename(&tmp, dest) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cache_with(vm: Value) -> VmCache {
        let c = VmCache::new();
        c.upsert(&vm);
        c
    }

    fn read(dest: &Path) -> String {
        std::fs::read_to_string(dest).expect("read metadata.json")
    }

    #[test]
    fn put_customer_key_persists_both_namespaces() {
        let dir = tempfile::tempdir().expect("tempdir");
        let zp = dir.path().to_string_lossy().to_string();
        std::fs::create_dir_all(dir.path().join("config")).expect("mkdir config");

        let vm = json!({
            "zonename": "u", "uuid": "u", "zonepath": zp,
            "customer_metadata": {"existing": "v"},
            "internal_metadata": {"root_pw": "secret"}
        });
        let w = FileWriter::new(cache_with(vm));
        w.apply(
            "u",
            &WriteOp::Set {
                key: "mykey".to_string(),
                value: "myval".to_string(),
                internal: false,
            },
        )
        .expect("apply");

        let dest = dir.path().join("config").join("metadata.json");
        let parsed: Value = serde_json::from_str(&read(&dest)).expect("json");
        assert_eq!(parsed["customer_metadata"]["mykey"], json!("myval"));
        assert_eq!(parsed["customer_metadata"]["existing"], json!("v"));
        assert_eq!(parsed["internal_metadata"]["root_pw"], json!("secret"));

        // internal_metadata is serialized before customer_metadata, two-space
        // indent, no trailing newline (matches JSON.stringify(d, null, 2)).
        let raw = read(&dest);
        let i_pos = raw.find("internal_metadata").expect("internal present");
        let c_pos = raw.find("customer_metadata").expect("customer present");
        assert!(i_pos < c_pos, "internal must precede customer");
        assert!(raw.contains("  \"internal_metadata\""), "two-space indent");
        assert!(!raw.ends_with('\n'), "no trailing newline");
    }

    #[test]
    fn put_pw_key_routes_internal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let zp = dir.path().to_string_lossy().to_string();
        std::fs::create_dir_all(dir.path().join("config")).expect("mkdir config");
        let vm = json!({
            "zonename": "u", "uuid": "u", "zonepath": zp,
            "customer_metadata": {}, "internal_metadata": {}
        });
        let w = FileWriter::new(cache_with(vm));
        w.apply(
            "u",
            &WriteOp::Set {
                key: "admin_pw".to_string(),
                value: "hunter2".to_string(),
                internal: true,
            },
        )
        .expect("apply");
        let dest = dir.path().join("config").join("metadata.json");
        let parsed: Value = serde_json::from_str(&read(&dest)).expect("json");
        assert_eq!(parsed["internal_metadata"]["admin_pw"], json!("hunter2"));
        assert!(!parsed["customer_metadata"].as_object().unwrap().contains_key("admin_pw"));
    }

    #[test]
    fn delete_removes_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let zp = dir.path().to_string_lossy().to_string();
        std::fs::create_dir_all(dir.path().join("config")).expect("mkdir config");
        let vm = json!({
            "zonename": "u", "uuid": "u", "zonepath": zp,
            "customer_metadata": {"gone": "x", "kept": "y"},
            "internal_metadata": {}
        });
        let w = FileWriter::new(cache_with(vm));
        w.apply(
            "u",
            &WriteOp::Remove {
                key: "gone".to_string(),
                internal: false,
            },
        )
        .expect("apply");
        let dest = dir.path().join("config").join("metadata.json");
        let parsed: Value = serde_json::from_str(&read(&dest)).expect("json");
        let cm = parsed["customer_metadata"].as_object().unwrap();
        assert!(!cm.contains_key("gone"));
        assert!(cm.contains_key("kept"));
    }

    #[test]
    fn unknown_zone_errors() {
        let w = FileWriter::new(VmCache::new());
        let r = w.apply(
            "missing",
            &WriteOp::Remove {
                key: "k".to_string(),
                internal: false,
            },
        );
        assert!(r.is_err());
    }

    #[test]
    fn no_temp_file_left_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let zp = dir.path().to_string_lossy().to_string();
        std::fs::create_dir_all(dir.path().join("config")).expect("mkdir config");
        let vm = json!({
            "zonename": "u", "uuid": "u", "zonepath": zp,
            "customer_metadata": {}, "internal_metadata": {}
        });
        let w = FileWriter::new(cache_with(vm));
        w.apply(
            "u",
            &WriteOp::Set {
                key: "k".to_string(),
                value: "v".to_string(),
                internal: false,
            },
        )
        .expect("apply");
        let tmp = dir.path().join("config").join("metadata.json.new");
        assert!(!tmp.exists(), "temp file should be renamed away");
    }
}
