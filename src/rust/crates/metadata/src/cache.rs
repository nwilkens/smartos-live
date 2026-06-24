// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The in-memory VM cache, keyed by zonename (== uuid for zones).
//!
//! The metadata agent serves every request from the live VM object. It seeds
//! the cache from `GET /vms`, then keeps it current from the `/events` stream
//! (create/modify/delete). Each request handler clones the current object for
//! the zone, so resolution never blocks the watcher.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use serde_json::Value;

/// A thread-safe map of zonename -> VM object (as vminfod serializes it).
#[derive(Clone, Default)]
pub struct VmCache {
    inner: Arc<RwLock<HashMap<String, Value>>>,
}

impl VmCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the whole cache from a `/vms` array. Non-object/anonymous
    /// entries are skipped. Returns the number of VMs loaded.
    pub fn replace_from_vms(&self, vms: &Value) -> usize {
        let mut map = HashMap::new();
        if let Some(arr) = vms.as_array() {
            for vm in arr {
                if let Some(zn) = vm.get("zonename").and_then(Value::as_str) {
                    map.insert(zn.to_string(), vm.clone());
                }
            }
        }
        let n = map.len();
        if let Ok(mut guard) = self.inner.write() {
            *guard = map;
        }
        n
    }

    /// Insert/replace one VM object (create or modify).
    pub fn upsert(&self, vm: &Value) {
        if let Some(zn) = vm.get("zonename").and_then(Value::as_str)
            && let Ok(mut guard) = self.inner.write() {
                guard.insert(zn.to_string(), vm.clone());
            }
    }

    /// Remove a VM by zonename (delete).
    pub fn remove(&self, zonename: &str) {
        if let Ok(mut guard) = self.inner.write() {
            guard.remove(zonename);
        }
    }

    /// A clone of the current VM object for `zonename`, if present.
    #[must_use]
    pub fn get(&self, zonename: &str) -> Option<Value> {
        self.inner.read().ok().and_then(|g| g.get(zonename).cloned())
    }

    /// The number of cached VMs.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.read().map(|g| g.len()).unwrap_or(0)
    }

    /// Whether the cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// All cached zonenames (a snapshot).
    #[must_use]
    pub fn zonenames(&self) -> Vec<String> {
        self.inner
            .read()
            .map(|g| g.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// The brand of a cached zone, if known.
    #[must_use]
    pub fn brand(&self, zonename: &str) -> Option<String> {
        self.inner
            .read()
            .ok()
            .and_then(|g| g.get(zonename).and_then(|v| v.get("brand")).and_then(Value::as_str).map(str::to_string))
    }

    /// The zonepath of a cached zone, if known.
    #[must_use]
    pub fn zonepath(&self, zonename: &str) -> Option<String> {
        self.inner
            .read()
            .ok()
            .and_then(|g| g.get(zonename).and_then(|v| v.get("zonepath")).and_then(Value::as_str).map(str::to_string))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn vm(zn: &str, brand: &str) -> Value {
        json!({"zonename": zn, "uuid": zn, "brand": brand, "zonepath": format!("/zones/{zn}")})
    }

    #[test]
    fn replace_and_get() {
        let c = VmCache::new();
        let n = c.replace_from_vms(&json!([vm("a", "joyent-minimal"), vm("b", "bhyve")]));
        assert_eq!(n, 2);
        assert!(c.get("a").is_some());
        assert_eq!(c.brand("b").as_deref(), Some("bhyve"));
        assert_eq!(c.zonepath("a").as_deref(), Some("/zones/a"));
    }

    #[test]
    fn upsert_and_remove() {
        let c = VmCache::new();
        c.upsert(&vm("a", "joyent-minimal"));
        assert_eq!(c.len(), 1);
        c.remove("a");
        assert!(c.is_empty());
    }
}
