// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! VM model + lookup table for firewall target resolution. Ports the relevant
//! parts of `src/fw/lib/fw.js` (createVMlookup / mergeIntoLookup) and
//! `src/fw/lib/util/vm.js` (ipsFromNICs).

use std::collections::BTreeMap;

use serde_json::Value;

/// A VM as needed by the firewall compiler (the subset of VM_FIELDS).
#[derive(Debug, Clone)]
pub struct Vm {
    /// VM UUID.
    pub uuid: String,
    /// firewall_enabled (default false).
    pub enabled: bool,
    /// Owner UUID.
    pub owner_uuid: Option<String>,
    /// Zone state.
    pub state: Option<String>,
    /// Zonepath (config dir lives under here).
    pub zonepath: String,
    /// Tags (name -> value as string).
    pub tags: BTreeMap<String, String>,
    /// Sorted, deduped IPs from NICs (no /cidr, no dhcp/addrconf).
    pub ips: Vec<String>,
}

/// Extract IPs from a NICs array exactly like `ipsFromNICs`: for each nic, use
/// `ips[]` (stripping `/cidr`) or `ip`, then drop "dhcp"/"addrconf".
pub fn ips_from_nics(nics: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(arr) = nics.as_array() {
        for nic in arr {
            if let Some(ips) = nic.get("ips").and_then(|v| v.as_array()) {
                for ip in ips {
                    if let Some(s) = ip.as_str() {
                        let head = s.split('/').next().unwrap_or(s);
                        push_ip(&mut out, head);
                    }
                }
            } else if let Some(ip) = nic.get("ip").and_then(|v| v.as_str()) {
                push_ip(&mut out, ip);
            }
        }
    }
    out
}

fn push_ip(out: &mut Vec<String>, ip: &str) {
    if ip != "dhcp" && ip != "addrconf" {
        out.push(ip.to_string());
    }
}

/// Address comparator matching `mod_addr.compare` (numeric IP ordering). v4
/// sorts before v6; within a family, by numeric value.
pub fn addr_compare(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let pa = parse_addr(a);
    let pb = parse_addr(b);
    match (pa, pb) {
        (Some(AddrKind::V4(x)), Some(AddrKind::V4(y))) => x.cmp(&y),
        (Some(AddrKind::V6(x)), Some(AddrKind::V6(y))) => x.cmp(&y),
        (Some(AddrKind::V4(_)), Some(AddrKind::V6(_))) => Ordering::Less,
        (Some(AddrKind::V6(_)), Some(AddrKind::V4(_))) => Ordering::Greater,
        _ => a.cmp(b),
    }
}

enum AddrKind {
    V4(u32),
    V6(u128),
}

fn parse_addr(s: &str) -> Option<AddrKind> {
    let head = s.split('/').next().unwrap_or(s);
    if let Ok(v4) = head.parse::<std::net::Ipv4Addr>() {
        Some(AddrKind::V4(u32::from(v4)))
    } else {
        head.parse::<std::net::Ipv6Addr>()
            .ok()
            .map(|v6| AddrKind::V6(u128::from(v6)))
    }
}

impl Vm {
    /// Build a Vm from a VM.js-shaped JSON object.
    pub fn from_json(obj: &Value) -> Option<Vm> {
        let uuid = obj.get("uuid")?.as_str()?.to_string();
        let zonepath =
            obj.get("zonepath").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let enabled = obj
            .get("firewall_enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let owner_uuid = obj
            .get("owner_uuid")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let state = obj
            .get("state")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let mut tags = BTreeMap::new();
        if let Some(t) = obj.get("tags").and_then(|v| v.as_object()) {
            for (k, v) in t {
                let val = match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                tags.insert(k.clone(), val);
            }
        }

        let mut ips = ips_from_nics(obj.get("nics").unwrap_or(&Value::Null));
        ips.sort_by(|a, b| addr_compare(a, b));

        Some(Vm {
            uuid,
            enabled,
            owner_uuid,
            state,
            zonepath,
            tags,
            ips,
        })
    }
}

/// A lookup table over VMs, keyed by the dimensions the compiler resolves
/// against: all, vms (by uuid), tags, tagValues, ips, wildcards.all == all.
#[derive(Debug, Default)]
pub struct VmLookup {
    /// All VMs by uuid.
    pub all: BTreeMap<String, Vm>,
    /// tag name -> { vm uuid -> Vm }.
    pub tags: BTreeMap<String, BTreeMap<String, Vm>>,
    /// tag name -> value -> { vm uuid -> Vm }.
    pub tag_values: BTreeMap<String, BTreeMap<String, BTreeMap<String, Vm>>>,
}

impl VmLookup {
    /// Build a lookup from a list of VMs (createVMlookup).
    pub fn build(vms: Vec<Vm>) -> VmLookup {
        let mut lk = VmLookup::default();
        for vm in vms {
            for (tag, val) in &vm.tags {
                lk.tags
                    .entry(tag.clone())
                    .or_default()
                    .insert(vm.uuid.clone(), vm.clone());
                lk.tag_values
                    .entry(tag.clone())
                    .or_default()
                    .entry(val.clone())
                    .or_default()
                    .insert(vm.uuid.clone(), vm.clone());
            }
            lk.all.insert(vm.uuid.clone(), vm);
        }
        lk
    }
}
