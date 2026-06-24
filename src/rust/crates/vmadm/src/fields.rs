// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! node-`vmadm`-compatible `list`/`lookup` field handling: the
//! `LIST_FIELDS` header/width table, field accessors over [`VmGetJson`],
//! `field=value` filtering, multi-key sort, and table/parsable rendering.

use tritond_vmadm::VmGetJson;

/// Default `list` column order and sort (node `DEFAULT_ORDER`/`DEFAULT_SORT`).
pub const DEFAULT_ORDER: &str = "uuid,type,ram,state,alias";
pub const DEFAULT_SORT: &str = "ram,uuid";

/// `(header, width)` for a known field, mirroring node's `LIST_FIELDS`.
/// Unknown fields render with an uppercased header and width 20.
fn field_meta(field: &str) -> (String, usize) {
    let known: &[(&str, &str, usize)] = &[
        ("alias", "ALIAS", 10),
        ("autoboot", "AUTOBOOT", 8),
        ("brand", "BRAND", 14),
        ("create_timestamp", "CREATE_TIMESTAMP", 24),
        ("do_not_inventory", "DNI", 5),
        ("flexible_disk_size", "FLEX_DISK_SIZE", 14),
        ("image_uuid", "IMAGE_UUID", 36),
        ("ram", "RAM", 7),
        ("max_physical_memory", "MAX_PHYS", 8),
        ("quota", "QUOTA", 5),
        ("zone_state", "ZONE_STATE", 10),
        ("state", "STATE", 16),
        ("type", "TYPE", 4),
        ("uuid", "UUID", 36),
        ("vcpus", "VCPUS", 5),
        ("zonename", "ZONENAME", 12),
        ("zonepath", "ZONEPATH", 40),
        ("zoneid", "ZONEID", 6),
    ];
    for (name, header, width) in known {
        if *name == field {
            return ((*header).to_string(), *width);
        }
    }
    (field.to_uppercase(), 20)
}

/// node's TYPE column: HVM brands show their name, everything else `OS`.
fn vm_type(brand: &str) -> &'static str {
    match brand {
        "bhyve" => "BHYV",
        "kvm" => "KVM",
        _ => "OS",
    }
}

/// The RAM column: a bhyve guest's `ram`, else `max_physical_memory`.
fn vm_ram(vm: &VmGetJson) -> u64 {
    vm.ram.unwrap_or(vm.max_physical_memory)
}

/// Resolve a field's value for a VM. Supports top-level scalars plus the
/// nested `nics.<n>.<prop>` / `disks.<n>.<prop>` /
/// `{tags,customer_metadata,internal_metadata,routes}.<key>` forms node
/// allows in `-o`/filters. Missing values render as the empty string.
pub fn field_value(vm: &VmGetJson, field: &str) -> String {
    if let Some((collection, rest)) = field.split_once('.') {
        return nested_value(vm, collection, rest);
    }
    match field {
        "uuid" => vm.uuid.to_string(),
        "type" => vm_type(&vm.brand).to_string(),
        "brand" => vm.brand.clone(),
        "ram" => vm_ram(vm).to_string(),
        "state" => vm.state.clone(),
        "zone_state" => vm.zone_state.clone(),
        "alias" => vm.alias.clone().unwrap_or_default(),
        "quota" => vm.quota.to_string(),
        "flexible_disk_size" => vm
            .flexible_disk_size
            .map(|s| s.to_string())
            .unwrap_or_default(),
        "max_physical_memory" => vm.max_physical_memory.to_string(),
        "vcpus" => vm.vcpus.map(|v| v.to_string()).unwrap_or_default(),
        "zonename" => vm.zonename.clone(),
        "zonepath" => vm.zonepath.clone(),
        "zoneid" => vm.zoneid.map(|z| z.to_string()).unwrap_or_default(),
        "autoboot" => vm.autoboot.to_string(),
        "do_not_inventory" => vm.do_not_inventory.unwrap_or(false).to_string(),
        "create_timestamp" => vm
            .create_timestamp
            .map(|t| t.to_rfc3339())
            .unwrap_or_default(),
        // node's image_uuid column: the OS-zone top-level image_uuid, else
        // the (boot) disk image for an HVM zone.
        "image_uuid" => vm
            .image_uuid
            .or_else(|| {
                vm.disks.as_ref().and_then(|disks| {
                    disks
                        .iter()
                        .find(|d| d.boot)
                        .or_else(|| disks.first())
                        .and_then(|d| d.image_uuid)
                })
            })
            .map(|u| u.to_string())
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn nested_value(vm: &VmGetJson, collection: &str, rest: &str) -> String {
    let map_get = |m: &tritond_vmadm::model::JsonMap| {
        m.get(rest)
            .map(|v| match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .unwrap_or_default()
    };
    match collection {
        "tags" => map_get(&vm.tags),
        "customer_metadata" => map_get(&vm.customer_metadata),
        "internal_metadata" => map_get(&vm.internal_metadata),
        "routes" => map_get(&vm.routes),
        "nics" => nic_field(vm, rest),
        "disks" => disk_field(vm, rest),
        _ => String::new(),
    }
}

fn nic_field(vm: &VmGetJson, rest: &str) -> String {
    let (idx, prop) = match rest.split_once('.') {
        Some((i, p)) => (i, p),
        None => return String::new(),
    };
    let Some(nic) = idx.parse::<usize>().ok().and_then(|i| vm.nics.get(i)) else {
        return String::new();
    };
    match prop {
        "ip" => nic.ip.clone().unwrap_or_default(),
        "mac" => nic.mac.clone(),
        "nic_tag" => nic.nic_tag.clone().unwrap_or_default(),
        "netmask" => nic.netmask.clone().unwrap_or_default(),
        "gateway" => nic.gateway.clone().unwrap_or_default(),
        "interface" => nic.interface.clone(),
        "vlan_id" => nic.vlan_id.map(|v| v.to_string()).unwrap_or_default(),
        _ => String::new(),
    }
}

fn disk_field(vm: &VmGetJson, rest: &str) -> String {
    let (idx, prop) = match rest.split_once('.') {
        Some((i, p)) => (i, p),
        None => return String::new(),
    };
    let Some(disk) = idx
        .parse::<usize>()
        .ok()
        .and_then(|i| vm.disks.as_ref().and_then(|d| d.get(i)))
    else {
        return String::new();
    };
    match prop {
        "path" => disk.path.clone(),
        "boot" => disk.boot.to_string(),
        "size" => disk.size.to_string(),
        "image_uuid" => disk.image_uuid.map(|u| u.to_string()).unwrap_or_default(),
        "model" => disk.model.clone().unwrap_or_default(),
        _ => String::new(),
    }
}

/// Whether a VM matches every `field=value` filter (exact match on the
/// field's rendered value).
pub fn matches_filters(vm: &VmGetJson, filters: &[String]) -> bool {
    filters.iter().all(|f| match f.split_once('=') {
        Some((field, want)) => field_value(vm, field) == want,
        // A bare token that isn't `field=value` matches nothing sensibly;
        // treat it as a no-match so a typo doesn't silently list everything.
        None => false,
    })
}

/// Sort VMs by the comma-separated `sortby` fields (numeric where the
/// values parse as numbers, else lexicographically). A `-`-prefixed field
/// sorts descending.
pub fn sort_vms(vms: &mut [VmGetJson], sortby: &str) {
    let keys: Vec<(&str, bool)> = sortby
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|k| k.strip_prefix('-').map_or((k, false), |rest| (rest, true)))
        .collect();
    vms.sort_by(|a, b| {
        for (field, desc) in &keys {
            let (va, vb) = (field_value(a, field), field_value(b, field));
            let ord = match (va.parse::<u64>(), vb.parse::<u64>()) {
                (Ok(na), Ok(nb)) => na.cmp(&nb),
                _ => va.cmp(&vb),
            };
            let ord = if *desc { ord.reverse() } else { ord };
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
        }
        std::cmp::Ordering::Equal
    });
}

/// Render the VMs as node's `list` output: a padded table, or (when
/// `parsable`) `:`-separated values. `header` adds the column header row.
pub fn format_list(vms: &[VmGetJson], order: &str, header: bool, parsable: bool) -> String {
    let cols: Vec<&str> = order.split(',').filter(|s| !s.is_empty()).collect();
    let mut out = String::new();

    if parsable {
        // node's parsable (`-p`) output never emits a header row; the
        // header belongs only to the formatted table below.
        let _ = header;
        for vm in vms {
            let row: Vec<String> = cols
                .iter()
                // Escape any literal ':' in a value so the delimiter is
                // unambiguous (node escapes with a backslash).
                .map(|c| field_value(vm, c).replace(':', "\\:"))
                .collect();
            out.push_str(&row.join(":"));
            out.push('\n');
        }
        return out;
    }

    if header {
        let hdr: Vec<String> = cols
            .iter()
            .map(|c| {
                let (h, w) = field_meta(c);
                format!("{h:<w$}")
            })
            .collect();
        out.push_str(hdr.join("  ").trim_end());
        out.push('\n');
    }
    for vm in vms {
        let row: Vec<String> = cols
            .iter()
            .map(|c| {
                let (_, w) = field_meta(c);
                let v = field_value(vm, c);
                let v = if v.is_empty() { "-".to_string() } else { v };
                format!("{v:<w$}")
            })
            .collect();
        out.push_str(row.join("  ").trim_end());
        out.push('\n');
    }
    out
}

/// `lookup` output: matching uuids one per line (or, when `unique`, error
/// unless exactly one). The JSON / `-o` forms are handled by the caller.
pub fn format_lookup_uuids(vms: &[VmGetJson]) -> String {
    vms.iter()
        .map(|vm| vm.uuid.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}
