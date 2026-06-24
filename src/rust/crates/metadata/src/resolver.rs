// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Key resolution over a VM object, reproducing `agent.js` `_metadataHandler`
//! plus `VM.flatten`.
//!
//! The VM object is a [`serde_json::Value`] exactly as vminfod serializes it
//! (`GET /vms/:uuid`), so the JSON we re-stringify for `sdc:nics` /
//! `sdc:resolvers` / `sdc:routes` preserves the daemon's field order and is
//! byte-identical to the node agent's output. Resolution is pure: GET/KEYS
//! never mutate; PUT/DELETE are surfaced as validated [`WriteOp`]s for the
//! caller to apply via the vmadm update path.

use serde_json::Value;

use crate::protocol::Outcome;

/// Hash-valued top-level keys `VM.flatten` will index with `a.b`.
const FLATTENABLE_HASH_KEYS: &[&str] = &[
    "customer_metadata",
    "internal_metadata",
    "tags",
    "routes",
];

/// Array-valued top-level keys `VM.flatten` will index with `a.<n>`.
const FLATTENABLE_ARRAYS: &[&str] = &["nics", "disks", "filesystems", "resolvers"];

/// A validated write request the daemon must apply (PUT/DELETE). The resolver
/// performs all namespace/validation checks; the caller only applies it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOp {
    /// Set `key`=`value`. `internal` routes to internal_metadata vs customer.
    Set {
        key: String,
        value: String,
        internal: bool,
    },
    /// Remove `key`. `internal` selects the namespace.
    Remove { key: String, internal: bool },
}

/// The outcome of resolving a GET/KEYS, or validating a PUT/DELETE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// A read result, ready to frame.
    Read(Outcome),
    /// A validated write to apply; on success the daemon returns `OK`.
    Write(WriteOp),
    /// A validation failure for a write (e.g. namespace rejection).
    WriteError(String),
}

/// Resolve a `GET <key>` against `vm`.
#[must_use]
pub fn resolve_get(vm: &Value, key: &str) -> Outcome {
    if key.is_empty() {
        return Outcome::Failure {
            message: "Invalid GET Request".to_string(),
        };
    }

    if let Some(rest) = key.strip_prefix("sdc:") {
        return resolve_sdc(vm, rest);
    }

    // customer vs internal metadata passthrough.
    let which = metadata_namespace(vm, key);
    let Some(map) = vm.get(which) else {
        return Outcome::Failure {
            message: format!("Zone did not contain {which}"),
        };
    };
    value_to_outcome(map.get(key))
}

/// Resolve a `KEYS` (mdata-list) against `vm`.
#[must_use]
pub fn resolve_keys(vm: &Value) -> Outcome {
    let mut keys: Vec<String> = Vec::new();

    if let Some(cm) = vm.get("customer_metadata").and_then(Value::as_object) {
        for k in cm.keys() {
            if !ends_with_pw(k) && internal_namespace(vm, k).is_none() {
                keys.push(k.clone());
            }
        }
    }
    if let Some(im) = vm.get("internal_metadata").and_then(Value::as_object) {
        for k in im.keys() {
            if ends_with_pw(k) || internal_namespace(vm, k).is_some() {
                keys.push(k.clone());
            }
        }
    }

    // Always SUCCESS (an empty list is an empty-string body).
    Outcome::Success(keys.join("\n"))
}

/// Validate a `PUT <key> <value>` against `vm`, returning a [`WriteOp`] or a
/// validation error. An empty `key` is the "Invalid PUT Request" sentinel the
/// parser produces.
#[must_use]
pub fn resolve_put(vm: &Value, key: &str, value: &str) -> Resolved {
    if key.is_empty() {
        return Resolved::WriteError("Invalid PUT Request".to_string());
    }
    if let Some(err) = reject_protected_namespace(vm, key) {
        return Resolved::WriteError(err);
    }
    Resolved::Write(WriteOp::Set {
        key: key.to_string(),
        value: value.to_string(),
        internal: is_internal_target(key),
    })
}

/// Validate a `DELETE <key>` against `vm`.
#[must_use]
pub fn resolve_delete(vm: &Value, key: &str) -> Resolved {
    if key.is_empty() {
        return Resolved::WriteError("Invalid DELETE Request".to_string());
    }
    if let Some(err) = reject_protected_namespace(vm, key) {
        return Resolved::WriteError(err);
    }
    Resolved::Write(WriteOp::Remove {
        key: key.to_string(),
        internal: is_internal_target(key),
    })
}

/// node setMetadata: `_pw$` keys and `operator-script` go to internal_metadata.
fn is_internal_target(key: &str) -> bool {
    ends_with_pw(key) || key == "operator-script"
}

/// Reject `sdc:` and registered internal namespaces for writes.
fn reject_protected_namespace(vm: &Value, key: &str) -> Option<String> {
    if key.starts_with("sdc:") {
        return Some("Cannot update the \"sdc\" Namespace.".to_string());
    }
    internal_namespace(vm, key).map(|ns| format!("Cannot update the \"{ns}\" Namespace."))
}

/// Resolve `sdc:<rest>` (the prefix already stripped).
fn resolve_sdc(vm: &Value, rest: &str) -> Outcome {
    match rest {
        "nics" if vm.get("nics").is_some() => json_outcome(vm.get("nics")),
        "resolvers" if vm.get("resolvers").is_some() => json_outcome(vm.get("resolvers")),
        "tmpfs" if vm.get("tmpfs").is_some() => json_outcome(vm.get("tmpfs")),
        "routes" if vm.get("routes").is_some() => Outcome::Success(build_routes(vm)),
        "operator-script" => {
            value_to_outcome(vm.get("internal_metadata").and_then(|m| m.get("operator-script")))
        }
        "volumes" => {
            value_to_outcome(vm.get("internal_metadata").and_then(|m| m.get("sdc:volumes")))
        }
        _ if rest.starts_with("tags.") => {
            let name = &rest["tags.".len()..];
            value_to_outcome(vm.get("tags").and_then(|t| t.get(name)))
        }
        _ => value_to_outcome(flatten(vm, rest).as_ref()),
    }
}

/// node `JSON.stringify(vmobj.<field>)` for the special sdc array/number keys.
/// Serializing the already-ordered Value reproduces the daemon's byte order.
fn json_outcome(v: Option<&Value>) -> Outcome {
    match v {
        Some(val) => Outcome::Success(serde_json::to_string(val).unwrap_or_default()),
        None => Outcome::NotFound,
    }
}

/// Build `sdc:routes` per agent.js: an array of {linklocal,dst,gateway},
/// resolving `nics[N]` / `macs[MAC]` link-local gateways to the local nic IP.
fn build_routes(vm: &Value) -> String {
    let mut out: Vec<Value> = Vec::new();
    let Some(routes) = vm.get("routes").and_then(Value::as_object) else {
        return "[]".to_string();
    };
    let nics = vm.get("nics").and_then(Value::as_array);

    for (dst, gw_val) in routes {
        let Some(gw) = gw_val.as_str() else { continue };

        if let Some(idx) = parse_nics_index(gw) {
            if let Some(ip) = nic_ip_by_index(nics, idx) {
                out.push(route_obj(true, dst, &ip));
            }
        } else if let Some(mac) = parse_macs(gw) {
            if let Some(ip) = nic_ip_by_mac(nics, &mac) {
                out.push(route_obj(true, dst, &ip));
            }
        } else {
            // Plain gateway: all info present.
            out.push(route_obj(false, dst, gw));
        }
    }
    serde_json::to_string(&Value::Array(out)).unwrap_or_else(|_| "[]".to_string())
}

fn route_obj(linklocal: bool, dst: &str, gateway: &str) -> Value {
    // Field order matches agent.js literal {linklocal, dst, gateway}.
    let mut m = serde_json::Map::new();
    m.insert("linklocal".to_string(), Value::Bool(linklocal));
    m.insert("dst".to_string(), Value::String(dst.to_string()));
    m.insert("gateway".to_string(), Value::String(gateway.to_string()));
    Value::Object(m)
}

fn parse_nics_index(s: &str) -> Option<usize> {
    let inner = s.strip_prefix("nics[")?.strip_suffix(']')?;
    inner.parse().ok()
}

fn parse_macs(s: &str) -> Option<String> {
    s.strip_prefix("macs[")?.strip_suffix(']').map(normalize_mac)
}

/// Normalize a MAC to colon-separated lowercase with no leading zeros dropped,
/// for comparison (node uses macaddr.parse().compare()).
fn normalize_mac(s: &str) -> String {
    s.split(':')
        .map(|oct| {
            let n = u8::from_str_radix(oct, 16).unwrap_or(0);
            format!("{n:02x}")
        })
        .collect::<Vec<_>>()
        .join(":")
}

fn nic_ip_by_index(nics: Option<&Vec<Value>>, idx: usize) -> Option<String> {
    let nic = nics?.get(idx)?;
    let ip = nic.get("ip").and_then(Value::as_str)?;
    if ip == "dhcp" {
        return None;
    }
    Some(ip.to_string())
}

fn nic_ip_by_mac(nics: Option<&Vec<Value>>, want: &str) -> Option<String> {
    for nic in nics? {
        let mac = nic.get("mac").and_then(Value::as_str)?;
        if normalize_mac(mac) == *want {
            let ip = nic.get("ip").and_then(Value::as_str)?;
            if ip == "dhcp" {
                return None;
            }
            return Some(ip.to_string());
        }
    }
    None
}

/// `VM.flatten(vmobj, key)`: dotted-path lookup with the flattenable classes,
/// else `vmobj[key]`. Returns the raw Value (caller maps to an Outcome).
fn flatten(vm: &Value, key: &str) -> Option<Value> {
    let tokens: Vec<&str> = key.split('.').collect();

    // a.b.c over array-of-hash keys (node FLATTENABLE_ARRAY_HASH_KEYS). The
    // generated set is brand-specific; nics/disks are the practical members.
    if tokens.len() == 3 && is_array_hash_key(tokens[0]) {
        let arr = vm.get(tokens[0])?.as_array()?;
        let idx: usize = tokens[1].parse().ok()?;
        return arr.get(idx)?.get(tokens[2]).cloned();
    }

    // a.b over hash keys.
    if tokens.len() == 2 && FLATTENABLE_HASH_KEYS.contains(&tokens[0]) {
        return vm.get(tokens[0])?.get(tokens[1]).cloned();
    }

    // a.<n> over array keys.
    if tokens.len() == 2 && FLATTENABLE_ARRAYS.contains(&tokens[0]) {
        let arr = vm.get(tokens[0])?.as_array()?;
        let idx: usize = tokens[1].parse().ok()?;
        return arr.get(idx).cloned();
    }

    vm.get(key).cloned()
}

fn is_array_hash_key(k: &str) -> bool {
    matches!(k, "nics" | "disks" | "filesystems")
}

/// Map a looked-up Value to an Outcome with node's `returnit` semantics:
/// string -> SUCCESS; number -> SUCCESS (stringified); other truthy ->
/// FAILURE; null/absent/false/empty -> NOTFOUND.
fn value_to_outcome(v: Option<&Value>) -> Outcome {
    match v {
        None | Some(Value::Null) => Outcome::NotFound,
        Some(Value::String(s)) => {
            // node: isString -> SUCCESS even for empty string.
            Outcome::Success(s.clone())
        }
        Some(Value::Number(n)) => Outcome::Success(n.to_string()),
        Some(Value::Bool(false)) => Outcome::NotFound, // falsy -> NOTFOUND
        Some(Value::Bool(true)) => Outcome::Failure {
            message: String::new(),
        },
        // Arrays/objects are truthy non-string non-number -> FAILURE.
        Some(_) => Outcome::Failure {
            message: String::new(),
        },
    }
}

/// Which metadata map a non-sdc GET key reads from (node: `_pw$` or a
/// registered internal namespace -> internal_metadata, else customer).
fn metadata_namespace(vm: &Value, key: &str) -> &'static str {
    if ends_with_pw(key) || internal_namespace(vm, key).is_some() {
        "internal_metadata"
    } else {
        "customer_metadata"
    }
}

/// node internalNamespace: if `key` has a `ns:` prefix listed in
/// `internal_metadata_namespaces`, return that namespace.
fn internal_namespace(vm: &Value, key: &str) -> Option<String> {
    if !key.contains(':') {
        return None;
    }
    let prefix = key.split(':').next()?;
    let nss = vm.get("internal_metadata_namespaces")?.as_array()?;
    for ns in nss {
        if ns.as_str() == Some(prefix) {
            return Some(prefix.to_string());
        }
    }
    None
}

fn ends_with_pw(key: &str) -> bool {
    key.ends_with("_pw")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn img2() -> Value {
        json!({
            "uuid": "4256eeee-c8b0-4fed-9e0a-975a31e14571",
            "zonename": "4256eeee-c8b0-4fed-9e0a-975a31e14571",
            "hostname": "img2",
            "alias": "img2",
            "dns_domain": "local",
            "max_physical_memory": 1024,
            "tmpfs": 512,
            "image_uuid": "b933df4b-b8f4-4bef-ad1e-236a20304496",
            "server_uuid": "041d4a3f-6bee-111f-6dc9-48210b330ae7",
            "owner_uuid": "00000000-0000-0000-0000-000000000000",
            "resolvers": ["10.199.199.14"],
            "nics": [{"interface":"net0","mac":"02:d3:c4:c5:52:13","vlan_id":0,"nic_tag":"admin","netmask":"255.255.255.0","ip":"10.10.1.3","ips":["10.10.1.3/24"],"mtu":1500,"primary":true}],
            "routes": {},
            "customer_metadata": {},
            "internal_metadata": {"root_pw": "secret"},
            "tags": {}
        })
    }

    #[test]
    fn sdc_hostname() {
        assert_eq!(resolve_get(&img2(), "sdc:hostname"), Outcome::Success("img2".into()));
    }

    #[test]
    fn sdc_uuid() {
        assert_eq!(
            resolve_get(&img2(), "sdc:uuid"),
            Outcome::Success("4256eeee-c8b0-4fed-9e0a-975a31e14571".into())
        );
    }

    #[test]
    fn sdc_max_physical_memory_is_number_success() {
        assert_eq!(resolve_get(&img2(), "sdc:max_physical_memory"), Outcome::Success("1024".into()));
    }

    #[test]
    fn sdc_resolvers_json() {
        assert_eq!(
            resolve_get(&img2(), "sdc:resolvers"),
            Outcome::Success("[\"10.199.199.14\"]".into())
        );
    }

    #[test]
    fn sdc_nics_byte_exact_order() {
        let out = resolve_get(&img2(), "sdc:nics");
        assert_eq!(
            out,
            Outcome::Success(
                "[{\"interface\":\"net0\",\"mac\":\"02:d3:c4:c5:52:13\",\"vlan_id\":0,\"nic_tag\":\"admin\",\"netmask\":\"255.255.255.0\",\"ip\":\"10.10.1.3\",\"ips\":[\"10.10.1.3/24\"],\"mtu\":1500,\"primary\":true}]"
                    .into()
            )
        );
    }

    #[test]
    fn sdc_routes_empty() {
        assert_eq!(resolve_get(&img2(), "sdc:routes"), Outcome::Success("[]".into()));
    }

    #[test]
    fn sdc_missing_is_notfound() {
        assert_eq!(resolve_get(&img2(), "sdc:nonexistent-key-xyz"), Outcome::NotFound);
        assert_eq!(resolve_get(&img2(), "sdc:datacenter_name"), Outcome::NotFound);
        assert_eq!(resolve_get(&img2(), "sdc:operator-script"), Outcome::NotFound);
    }

    #[test]
    fn root_pw_from_internal() {
        assert_eq!(resolve_get(&img2(), "root_pw"), Outcome::Success("secret".into()));
    }

    #[test]
    fn keys_lists_pw_from_internal() {
        assert_eq!(resolve_keys(&img2()), Outcome::Success("root_pw".into()));
    }

    #[test]
    fn keys_lists_customer_without_namespace() {
        let mut vm = img2();
        vm["customer_metadata"] = json!({
            "triton:fdb_cluster_peers": "a",
            "triton:fdb_cluster_secret": "b",
            "triton:fdb_public_ip": "c",
            "user-script": "echo"
        });
        vm["internal_metadata"] = json!({});
        // No internal_metadata_namespaces -> triton: keys are customer keys.
        assert_eq!(
            resolve_keys(&vm),
            Outcome::Success(
                "triton:fdb_cluster_peers\ntriton:fdb_cluster_secret\ntriton:fdb_public_ip\nuser-script".into()
            )
        );
    }

    #[test]
    fn namespace_routes_internal() {
        let mut vm = img2();
        vm["internal_metadata_namespaces"] = json!(["triton"]);
        vm["internal_metadata"] = json!({"triton:secret": "hush", "root_pw": "x"});
        // GET triton:secret reads internal_metadata.
        assert_eq!(resolve_get(&vm, "triton:secret"), Outcome::Success("hush".into()));
        // and is listed by KEYS.
        let Outcome::Success(list) = resolve_keys(&vm) else { panic!("expected success") };
        assert!(list.contains("triton:secret"));
        assert!(list.contains("root_pw"));
    }

    #[test]
    fn put_rejects_sdc_namespace() {
        assert_eq!(
            resolve_put(&img2(), "sdc:foo", "bar"),
            Resolved::WriteError("Cannot update the \"sdc\" Namespace.".into())
        );
    }

    #[test]
    fn put_routes_pw_internal() {
        assert_eq!(
            resolve_put(&img2(), "admin_pw", "x"),
            Resolved::Write(WriteOp::Set {
                key: "admin_pw".into(),
                value: "x".into(),
                internal: true
            })
        );
        assert_eq!(
            resolve_put(&img2(), "foo", "bar"),
            Resolved::Write(WriteOp::Set {
                key: "foo".into(),
                value: "bar".into(),
                internal: false
            })
        );
    }

    #[test]
    fn put_empty_key_invalid() {
        assert_eq!(
            resolve_put(&img2(), "", ""),
            Resolved::WriteError("Invalid PUT Request".into())
        );
    }

    #[test]
    fn delete_rejects_namespace_and_removes() {
        let mut vm = img2();
        vm["internal_metadata_namespaces"] = json!(["triton"]);
        assert_eq!(
            resolve_delete(&vm, "triton:x"),
            Resolved::WriteError("Cannot update the \"triton\" Namespace.".into())
        );
        assert_eq!(
            resolve_delete(&vm, "foo"),
            Resolved::Write(WriteOp::Remove {
                key: "foo".into(),
                internal: false
            })
        );
    }

    #[test]
    fn routes_linklocal_nics_index() {
        let mut vm = img2();
        vm["routes"] = json!({"10.99.99.0/24": "nics[0]"});
        // nic[0].ip is 10.10.1.3 -> gateway resolves.
        assert_eq!(
            resolve_get(&vm, "sdc:routes"),
            Outcome::Success(
                "[{\"linklocal\":true,\"dst\":\"10.99.99.0/24\",\"gateway\":\"10.10.1.3\"}]".into()
            )
        );
    }

    #[test]
    fn routes_plain_gateway() {
        let mut vm = img2();
        vm["routes"] = json!({"10.99.99.0/24": "10.10.1.1"});
        assert_eq!(
            resolve_get(&vm, "sdc:routes"),
            Outcome::Success(
                "[{\"linklocal\":false,\"dst\":\"10.99.99.0/24\",\"gateway\":\"10.10.1.1\"}]".into()
            )
        );
    }

    #[test]
    fn flatten_dotted_customer_metadata() {
        let mut vm = img2();
        vm["customer_metadata"] = json!({"foo": "barval"});
        // sdc:customer_metadata.foo flattens.
        assert_eq!(
            resolve_get(&vm, "sdc:customer_metadata.foo"),
            Outcome::Success("barval".into())
        );
    }

    #[test]
    fn empty_get_request_error() {
        assert_eq!(
            resolve_get(&img2(), ""),
            Outcome::Failure {
                message: "Invalid GET Request".into()
            }
        );
    }
}
