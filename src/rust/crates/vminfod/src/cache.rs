// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The in-memory VM cache, keyed by zonename (== uuid for OS/HVM zones).
//!
//! vminfod caches both the ordered VM `Value` (for diffing) and its
//! pre-serialized JSON string (reused for `/vms`, `/vms/:uuid`, and event
//! `vm` payloads), so a serialization happens once per VM change rather than
//! once per request. We keep both: the `Value` to diff old vs new, the
//! string to serve.

use std::collections::BTreeMap;

use serde_json::Value;

/// One cached VM: the ordered object and its serialized form.
#[derive(Debug, Clone)]
pub struct CachedVm {
    /// The ordered VM object (node key order), used for diffing.
    pub value: Value,
    /// The pre-serialized compact JSON of `value`.
    pub json: String,
}

impl CachedVm {
    #[must_use]
    pub fn new(value: Value) -> Self {
        let json = serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string());
        Self { value, json }
    }
}

/// The cache: zonename -> cached VM, with an explicit insertion-order index.
///
/// node iterates `Object.keys(self.vmobjs)` (insertion order) for
/// `serializeVms()`, and on a fresh seed that order is `zoneadm list -cp`
/// order (the order `tritond_vmadm::list` returns). We reproduce that by
/// keeping an `order` vector of zonenames in first-seen order alongside the
/// map, so `/vms` is byte-identical to the live daemon. A re-inserted
/// (modified) zone keeps its original position; a deleted zone is removed
/// from the order.
#[derive(Debug, Default)]
pub struct VmCache {
    vms: BTreeMap<String, CachedVm>,
    order: Vec<String>,
}

impl VmCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.vms.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.vms.is_empty()
    }

    #[must_use]
    pub fn contains(&self, zonename: &str) -> bool {
        self.vms.contains_key(zonename)
    }

    #[must_use]
    pub fn get(&self, zonename: &str) -> Option<&CachedVm> {
        self.vms.get(zonename)
    }

    /// Insert or update a VM. A new zonename is appended to the order; an
    /// existing one keeps its position (node's `Object.keys` preserves the
    /// original insertion slot when a key's value is replaced).
    pub fn insert(&mut self, zonename: String, vm: CachedVm) {
        if !self.vms.contains_key(&zonename) {
            self.order.push(zonename.clone());
        }
        self.vms.insert(zonename, vm);
    }

    pub fn remove(&mut self, zonename: &str) -> Option<CachedVm> {
        let removed = self.vms.remove(zonename);
        if removed.is_some() {
            self.order.retain(|z| z != zonename);
        }
        removed
    }

    /// Zonenames in insertion (first-seen) order.
    #[must_use]
    pub fn zonenames(&self) -> Vec<String> {
        self.order.clone()
    }

    /// Serialize the whole cache as node's `serializeVms()` does: `'[' +
    /// per-vm cached JSON joined by ',' + ']'`, in insertion order. Empty
    /// cache -> `'[]'`.
    #[must_use]
    pub fn serialize_vms(&self) -> String {
        let mut out = String::from("[");
        let mut first = true;
        for zonename in &self.order {
            if let Some(vm) = self.vms.get(zonename) {
                if !first {
                    out.push(',');
                }
                out.push_str(&vm.json);
                first = false;
            }
        }
        out.push(']');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_serializes_to_empty_array() {
        let c = VmCache::new();
        assert_eq!(c.serialize_vms(), "[]");
        assert_eq!(c.len(), 0);
    }

    #[test]
    fn serialize_joins_cached_json() {
        let mut c = VmCache::new();
        c.insert("a".to_string(), CachedVm::new(json!({"zonename": "a"})));
        c.insert("b".to_string(), CachedVm::new(json!({"zonename": "b"})));
        let s = c.serialize_vms();
        assert!(s.starts_with('['));
        assert!(s.ends_with(']'));
        assert!(s.contains("\"zonename\":\"a\""));
        assert!(s.contains("},{"));
    }

    #[test]
    fn get_and_remove() {
        let mut c = VmCache::new();
        c.insert("z".to_string(), CachedVm::new(json!({"x": 1})));
        assert!(c.contains("z"));
        assert!(c.get("z").is_some());
        assert!(c.remove("z").is_some());
        assert!(!c.contains("z"));
    }

    #[test]
    fn preserves_insertion_order_not_sorted() {
        let mut c = VmCache::new();
        // Insert out of sorted order; serialize must keep insertion order.
        c.insert("zzz".to_string(), CachedVm::new(json!({"zonename": "zzz"})));
        c.insert("aaa".to_string(), CachedVm::new(json!({"zonename": "aaa"})));
        c.insert("mmm".to_string(), CachedVm::new(json!({"zonename": "mmm"})));
        assert_eq!(c.zonenames(), vec!["zzz", "aaa", "mmm"]);
        let s = c.serialize_vms();
        let i_z = s.find("zzz").expect("zzz");
        let i_a = s.find("aaa").expect("aaa");
        let i_m = s.find("mmm").expect("mmm");
        assert!(i_z < i_a && i_a < i_m, "insertion order kept: {s}");
    }

    #[test]
    fn update_keeps_position() {
        let mut c = VmCache::new();
        c.insert("a".to_string(), CachedVm::new(json!({"v": 1})));
        c.insert("b".to_string(), CachedVm::new(json!({"v": 1})));
        // Re-insert "a" (a modify): it must keep its leading slot.
        c.insert("a".to_string(), CachedVm::new(json!({"v": 2})));
        assert_eq!(c.zonenames(), vec!["a", "b"]);
    }

    #[test]
    fn remove_drops_from_order() {
        let mut c = VmCache::new();
        c.insert("a".to_string(), CachedVm::new(json!({})));
        c.insert("b".to_string(), CachedVm::new(json!({})));
        c.remove("a");
        assert_eq!(c.zonenames(), vec!["b"]);
    }
}
