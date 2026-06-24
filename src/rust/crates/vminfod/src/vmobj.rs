// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Reorder a [`tritond_vmadm::VmGetJson`] into vminfod's exact key
//! insertion order.
//!
//! vminfod delegates VM-object construction to vmload and emits the keys in
//! a fixed insertion order (the order node's vmload builder assigns them).
//! `VmGetJson`'s serde field order differs, so we re-key its serialized
//! `Value` into the documented order. Conditional keys (`image_uuid`,
//! `datasets`, `zfs_data_*`) follow the same present-when-set rule the live
//! daemon uses, because we only copy keys that exist in the source value.
//!
//! The canonical order was captured byte-for-byte from the live daemon on a
//! production CN (`GET /vms/<uuid>` for a joyent-minimal OS zone): 49 keys
//! with no delegated dataset, 51-52 with one. See `KEY_ORDER` below.

use serde_json::{Map, Value};
use tritond_vmadm::VmGetJson;

/// vminfod's OS-zone key order, as emitted by the live daemon. Conditional
/// keys are listed at the position the live daemon inserts them; a key not
/// present in the source object is skipped (so `image_uuid` drops out for an
/// image-less zone, `datasets`/`zfs_data_*` drop out without a delegated
/// dataset).
const KEY_ORDER: &[&str] = &[
    "zonename",
    "autoboot",
    "brand",
    "limit_priv",
    "v",
    "create_timestamp",
    "image_uuid",
    "cpu_shares",
    "max_lwps",
    "max_msg_ids",
    "max_sem_ids",
    "max_shm_ids",
    "max_shm_memory",
    "zfs_io_priority",
    "max_physical_memory",
    "max_locked_memory",
    "max_swap",
    "cpu_cap",
    "billing_id",
    "owner_uuid",
    "tmpfs",
    "hostname",
    "dns_domain",
    "resolvers",
    "alias",
    "nics",
    "datasets",
    "uuid",
    "zone_state",
    "zonepath",
    "hvm",
    "zoneid",
    "zonedid",
    "last_modified",
    // exit_status/exit_timestamp are inserted by vmload right after
    // last_modified (index.js:822/838), only for non-running zones.
    "exit_status",
    "exit_timestamp",
    "firewall_enabled",
    "server_uuid",
    "platform_buildstamp",
    "state",
    "boot_timestamp",
    "init_restarts",
    "pid",
    "customer_metadata",
    "internal_metadata",
    "routes",
    "tags",
    "quota",
    "zfs_root_recsize",
    "zfs_filesystem",
    "zpool",
    "zfs_data_compression",
    "zfs_data_recsize",
    "snapshots",
];

/// Per-NIC key order, captured from the live daemon. Each key is emitted
/// only when present in the source NIC object.
const NIC_KEY_ORDER: &[&str] = &[
    "interface", "mac", "vlan_id", "nic_tag", "gateway", "gateways", "netmask", "ip", "ips",
    "model", "mtu", "primary",
];

/// Serialize a `VmGetJson` to its source map (pre-reorder). An absent
/// optional key here means node omits it too (serde `skip_serializing_if`).
#[must_use]
pub fn to_source_map(vm: &VmGetJson) -> Map<String, Value> {
    match serde_json::to_value(vm) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    }
}

/// Reorder an (optionally enriched) source map into vminfod's key order.
#[must_use]
pub fn from_source_map(src: Map<String, Value>) -> Value {
    reorder(src)
}

/// Build the ordered vminfod VM object as a `serde_json::Value::Object`,
/// preserving insertion order (the workspace enables serde_json's
/// `preserve_order`). Unknown extra keys the source carries but the order
/// table omits are appended at the end so no data is silently dropped.
#[must_use]
pub fn to_ordered_value(vm: &VmGetJson) -> Value {
    reorder(to_source_map(vm))
}

fn reorder(mut src: Map<String, Value>) -> Value {
    // node's vmload truncates `last_modified` to whole seconds (the live
    // daemon always emits `...:SS.000Z`); the underlying mtime carries
    // milliseconds. Truncate here so the field matches byte-for-byte.
    if let Some(Value::String(ts)) = src.get_mut("last_modified") {
        *ts = truncate_to_seconds(ts);
    }
    let mut out = Map::with_capacity(src.len());
    for key in KEY_ORDER {
        if let Some(mut v) = src.remove(*key) {
            if *key == "nics" {
                v = reorder_nics(v);
            }
            out.insert((*key).to_string(), v);
        }
    }
    // Preserve any field the source had that the order table doesn't list
    // (forward-compat: never silently drop a key). Appended after the known
    // ordering, sorted for determinism.
    let mut extra: Vec<(String, Value)> = src.into_iter().collect();
    extra.sort_by(|a, b| a.0.cmp(&b.0));
    for (k, v) in extra {
        out.insert(k, v);
    }
    Value::Object(out)
}

/// Truncate an ISO8601 `...:SS.mmmZ` timestamp to whole seconds
/// (`...:SS.000Z`), matching node's vmload `last_modified`.
fn truncate_to_seconds(ts: &str) -> String {
    match ts.split_once('.') {
        Some((head, _frac)) => format!("{head}.000Z"),
        None => ts.to_string(),
    }
}

fn reorder_nics(nics: Value) -> Value {
    let Value::Array(arr) = nics else {
        return nics;
    };
    Value::Array(
        arr.into_iter()
            .map(|n| match n {
                Value::Object(m) => reorder_one_nic(m),
                other => other,
            })
            .collect(),
    )
}

fn reorder_one_nic(mut src: Map<String, Value>) -> Value {
    let mut out = Map::with_capacity(src.len());
    for key in NIC_KEY_ORDER {
        if let Some(v) = src.remove(*key) {
            out.insert((*key).to_string(), v);
        }
    }
    let mut extra: Vec<(String, Value)> = src.into_iter().collect();
    extra.sort_by(|a, b| a.0.cmp(&b.0));
    for (k, v) in extra {
        out.insert(k, v);
    }
    Value::Object(out)
}

/// Serialize an ordered VM object to its compact JSON string (the cached
/// per-VM form vminfod reuses for `/vms`, `/vms/:uuid`, and event payloads).
#[must_use]
pub fn to_ordered_string(vm: &VmGetJson) -> String {
    serde_json::to_string(&to_ordered_value(vm)).unwrap_or_else(|_| "{}".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tritond_vmadm::VmGetJson;
    use uuid::Uuid;

    fn os_zone() -> VmGetJson {
        let u: Uuid = "4256eeee-c8b0-4fed-9e0a-975a31e14571".parse().expect("uuid");
        let mut vm = VmGetJson::skeleton(u);
        vm.brand = "joyent-minimal".to_string();
        vm.state = "running".to_string();
        vm.zone_state = "running".to_string();
        vm.zonepath = format!("/zones/{u}");
        vm.autoboot = true;
        vm.hvm = false;
        vm
    }

    #[test]
    fn ordered_keys_follow_node_order() {
        let v = to_ordered_value(&os_zone());
        let obj = v.as_object().expect("object");
        let keys: Vec<&String> = obj.keys().collect();
        // zonename first, autoboot second, brand third (node order), not
        // the serde struct order (which starts uuid).
        assert_eq!(keys[0], "zonename");
        assert_eq!(keys[1], "autoboot");
        assert_eq!(keys[2], "brand");
        // uuid lands after nics, not first.
        let i_uuid = keys.iter().position(|k| *k == "uuid").expect("uuid");
        let i_nics = keys.iter().position(|k| *k == "nics").expect("nics");
        assert!(i_nics < i_uuid, "nics before uuid");
        // snapshots last.
        assert_eq!(keys.last().map(|s| s.as_str()), Some("snapshots"));
    }

    #[test]
    fn absent_image_uuid_is_omitted() {
        let v = to_ordered_value(&os_zone());
        assert!(v.get("image_uuid").is_none());
    }

    #[test]
    fn present_optionals_are_ordered() {
        let mut vm = os_zone();
        vm.image_uuid = Some("b933df4b-b8f4-4bef-ad1e-236a20304496".parse().expect("uuid"));
        vm.v = Some(1);
        let v = to_ordered_value(&vm);
        let keys: Vec<&String> = v.as_object().expect("obj").keys().collect();
        let i_v = keys.iter().position(|k| *k == "v").expect("v");
        let i_img = keys.iter().position(|k| *k == "image_uuid").expect("img");
        let i_create = keys.iter().position(|k| *k == "create_timestamp");
        // node order: v(5), create_timestamp(6), image_uuid(7).
        assert!(i_v < i_img);
        if let Some(ic) = i_create {
            assert!(ic < i_img);
        }
    }

    #[test]
    fn nic_keys_ordered() {
        let mut vm = os_zone();
        let mut nic = tritond_vmadm::model::get_json::NicJson::default();
        nic.interface = "net0".to_string();
        nic.mac = "02:d3:c4:c5:52:13".to_string();
        nic.vlan_id = Some(0);
        nic.nic_tag = Some("admin".to_string());
        nic.netmask = Some("255.255.255.0".to_string());
        nic.ip = Some("10.10.1.3".to_string());
        nic.ips = vec!["10.10.1.3/24".to_string()];
        nic.mtu = Some(1500);
        nic.primary = true;
        vm.nics = vec![nic];

        let v = to_ordered_value(&vm);
        let n = &v["nics"][0];
        let keys: Vec<&String> = n.as_object().expect("nic obj").keys().collect();
        assert_eq!(
            keys,
            vec!["interface", "mac", "vlan_id", "nic_tag", "netmask", "ip", "ips", "mtu", "primary"]
        );
    }
}
