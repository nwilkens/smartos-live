// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The cache loader/reconciler.
//!
//! vminfod delegates VM-object construction entirely to vmload; our loader
//! delegates to `tritond_vmadm::{get, list}` (the same vmload-equivalent
//! reconstruction) and reorders the result into vminfod's key order. A full
//! refresh lists every zone, diffs the new set against the cache, and emits
//! create / modify / delete events for the differences.

use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;
use tritond_vmadm::backend::HostBackend;

use crate::cache::CachedVm;
use crate::datasets;
use crate::diff::diff;
use crate::events::{create_record, delete_record, modify_record};
use crate::hrtime::HrTime;
use crate::state::{Daemon, RefreshLogEntry};
use crate::vmobj::{from_source_map, to_source_map};
use tritond_vmadm::VmGetJson;

/// Load every configured zone and reconcile the cache, emitting events for
/// the diffs. This is both the initial seed (no events listeners yet, so
/// emits are no-ops) and the periodic full refresh.
pub async fn full_refresh(daemon: &Arc<Daemon>) {
    let started = Instant::now();
    daemon.set_substatus("fetching vm cache data");

    let host = HostBackend::new();
    let listed = match tritond_vmadm::lifecycle::list(&host).await {
        Ok(v) => v,
        Err(e) => {
            daemon.record_refresh_error(e.to_string());
            daemon.set_status("running", "working");
            return;
        }
    };

    daemon.set_substatus("comparing vmobj sets");

    // Build the new ordered set keyed by zonename.
    let mut new_set: Vec<(String, Value)> = Vec::with_capacity(listed.len());
    for vm in &listed {
        let value = build_value(&host, vm).await;
        let zonename = value
            .get("zonename")
            .and_then(Value::as_str)
            .unwrap_or(&vm.zonename)
            .to_string();
        new_set.push((zonename, value));
    }

    daemon.set_substatus("synchronizing data");

    let mut cache_changes: Vec<Value> = Vec::new();
    let mut vm_changes: Vec<Value> = Vec::new();

    // Snapshot the current zonenames so we can detect deletions.
    let existing = daemon.with_inner(|i| i.cache.zonenames());

    // creates + modifies.
    for (zonename, value) in &new_set {
        reconcile_one(daemon, zonename, value, &mut cache_changes, &mut vm_changes);
    }

    // deletions: zones in the cache but not in the new set.
    let new_names: std::collections::BTreeSet<&String> =
        new_set.iter().map(|(z, _)| z).collect();
    for zonename in &existing {
        if !new_names.contains(zonename) {
            emit_delete(daemon, zonename, &mut cache_changes, &mut vm_changes);
        }
    }

    let ended = Instant::now();
    let pretty_delta = HrTime::delta(started, ended).to_pretty();
    daemon.record_refresh_log(RefreshLogEntry {
        cache_changes,
        vm_changes,
        started,
        ended,
        pretty_delta,
    });
    daemon.set_status("running", "working");
}

/// Build the ordered, dataset-enriched VM value for one reconstructed VM.
/// The base fields come from `tritond_vmadm` (vmload-equivalent); the
/// delegated-dataset keys are added here ([`datasets::enrich`]) because the
/// engine does not model them.
async fn build_value(host: &HostBackend, vm: &VmGetJson) -> Value {
    let mut src = to_source_map(vm);
    datasets::enrich(host, vm.uuid, &mut src).await;
    from_source_map(src)
}

/// Reconcile a single zone into the cache, emitting create or modify.
fn reconcile_one(
    daemon: &Arc<Daemon>,
    zonename: &str,
    value: &Value,
    cache_changes: &mut Vec<Value>,
    vm_changes: &mut Vec<Value>,
) {
    let uuid = value
        .get("uuid")
        .and_then(Value::as_str)
        .unwrap_or(zonename)
        .to_string();

    let prev = daemon.with_inner(|i| i.cache.get(zonename).map(|c| c.value.clone()));

    match prev {
        None => {
            // create.
            daemon.with_inner(|i| {
                i.cache.insert(zonename.to_string(), CachedVm::new(value.clone()));
            });
            cache_changes.push(serde_json::json!({"action": "added", "zonename": zonename}));
            let rec = create_record(zonename, &uuid, value);
            emit(daemon, rec);
        }
        Some(old) => {
            let changes = diff(&old, value);
            if changes.is_empty() {
                return;
            }
            daemon.with_inner(|i| {
                i.cache.insert(zonename.to_string(), CachedVm::new(value.clone()));
            });
            vm_changes.push(serde_json::json!({"zonename": zonename, "nchanges": changes.len()}));
            let rec = modify_record(zonename, &uuid, value, &changes);
            emit(daemon, rec);
        }
    }
}

/// Emit a delete for a zone that vanished from the cache.
fn emit_delete(
    daemon: &Arc<Daemon>,
    zonename: &str,
    cache_changes: &mut Vec<Value>,
    _vm_changes: &mut [Value],
) {
    let removed = daemon.with_inner(|i| i.cache.remove(zonename));
    if removed.is_some() {
        cache_changes.push(serde_json::json!({"action": "removed", "zonename": zonename}));
        let rec = delete_record(zonename, zonename);
        emit(daemon, rec);
    }
}

/// Recompute a single zone (after a targeted watcher event) and emit the
/// diff. Returns true if the zone is gone (caller may stop per-zone watches).
pub async fn refresh_one(daemon: &Arc<Daemon>, zonename: &str) -> bool {
    let host = HostBackend::new();
    let parsed = match zonename.parse() {
        Ok(u) => u,
        Err(_) => return false,
    };

    match tritond_vmadm::lifecycle::get(&host, parsed).await {
        Ok(Some(vm)) => {
            let value = build_value(&host, &vm).await;
            let mut cc = Vec::new();
            let mut vc = Vec::new();
            reconcile_one(daemon, zonename, &value, &mut cc, &mut vc);
            false
        }
        Ok(None) => {
            // Zone gone: emit delete if we had it cached.
            let mut cc = Vec::new();
            let mut vc: Vec<Value> = Vec::new();
            emit_delete(daemon, zonename, &mut cc, &mut vc);
            true
        }
        Err(e) => {
            daemon.record_refresh_error(format!("{zonename}: {e}"));
            false
        }
    }
}

/// Serialize an event once and fan it out (node emits one JSON.stringify per
/// event to all listeners).
fn emit(daemon: &Arc<Daemon>, record: Value) {
    if let Ok(line) = serde_json::to_string(&record) {
        daemon.hub.emit(line);
    }
}
