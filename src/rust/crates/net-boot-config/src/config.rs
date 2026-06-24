// Boot-time networking config loading + the KEY=VALUE projection, ported
// from node_modules/net-boot-config.js. Reproduces the node tool's behavior
// (file precedence, default properties, and configValues mapping) so the
// stdout of the dump/--routes/--nictag-rules/--json modes is byte-identical.

use std::io::ErrorKind;
use std::path::PathBuf;

use serde_json::{Map, Value};

const ARRAY_PROPS: [&str; 5] =
    ["aggregations", "etherstubs", "nictags", "resolvers", "vnics"];
const OBJ_PROPS: [&str; 2] = ["nictag_rules", "routes"];

const DEFAULT_CONFIG: &str = "/system/boot/networking.json";
const HN_CONFIG: &str = "/usbkey/boot/networking.json";

fn config_path() -> PathBuf {
    match std::env::var_os("NET_BOOT_CONFIG_FILE") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => PathBuf::from(DEFAULT_CONFIG),
    }
}

// Mirror addDefaultProperties: array props default to [], object props to {}.
fn add_default_properties(conf: &mut Map<String, Value>) {
    for prop in ARRAY_PROPS {
        conf.entry(prop).or_insert_with(|| Value::Array(Vec::new()));
    }
    for prop in OBJ_PROPS {
        conf.entry(prop)
            .or_insert_with(|| Value::Object(Map::new()));
    }
}

// Parse a single config file into an object, applying defaults. None means the
// file did not exist (ENOENT); any other failure is an error.
fn load_conf_file(path: &std::path::Path) -> anyhow::Result<Option<Map<String, Value>>> {
    let raw = match std::fs::read(path) {
        Ok(r) => r,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let parsed: Value = serde_json::from_slice(&raw)?;
    let mut conf = match parsed {
        Value::Object(m) => m,
        // The node tool would carry a non-object through addDefaultProperties,
        // which only adds keys; emulate by starting from an empty object so the
        // defaults still apply. Real config files are always objects.
        _ => Map::new(),
    };
    add_default_properties(&mut conf);
    Ok(Some(conf))
}

/// Load the config from the primary path, falling back to the headnode path.
/// When neither exists, warn (as the node tool does) and return all-defaults.
pub fn load_config() -> anyhow::Result<Map<String, Value>> {
    if let Some(conf) = load_conf_file(&config_path())? {
        return Ok(conf);
    }
    if let Some(conf) = load_conf_file(std::path::Path::new(HN_CONFIG))? {
        return Ok(conf);
    }
    eprintln!("WARNING: No config file found");
    let mut conf = Map::new();
    add_default_properties(&mut conf);
    Ok(conf)
}

/// True if boot-file networking is enabled (either config file exists).
pub fn enabled() -> bool {
    config_path().exists() || std::path::Path::new(HN_CONFIG).exists()
}

fn arr<'a>(conf: &'a Map<String, Value>, key: &str) -> &'a [Value] {
    match conf.get(key) {
        Some(Value::Array(a)) => a.as_slice(),
        _ => &[],
    }
}

fn str_field(obj: &Value, key: &str) -> Option<String> {
    obj.get(key).and_then(value_to_string)
}

// Render a JSON scalar the way node's string concatenation (`vals[x] = y`)
// would when later emitted via util.format("%s=%s"). Strings pass through;
// numbers/bools stringify; null/objects/arrays are treated as absent.
fn value_to_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

// Truthiness check matching the node `if (conf.x)` guards: present, non-empty
// string, non-zero number, true.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

// Join a JSON array of scalars with commas (Array.prototype.join), matching
// node's coercion of each element to a string.
fn join_array(v: &Value) -> Option<String> {
    let a = v.as_array()?;
    let parts: Vec<String> = a
        .iter()
        .map(|e| match e {
            Value::Null => String::new(),
            other => value_to_string(other).unwrap_or_default(),
        })
        .collect();
    Some(parts.join(","))
}

/// Reproduce configValues: ordered list of (key, value) pairs that the node
/// tool prints as `key=value`. Insertion order is preserved exactly.
pub fn config_values(conf: &Map<String, Value>) -> Vec<(String, String)> {
    let mut vals: Vec<(String, String)> = Vec::new();
    let mut ip_keys: Vec<String> = Vec::new();
    let mut tag_nums: std::collections::HashMap<String, i64> =
        std::collections::HashMap::new();

    let push = |vals: &mut Vec<(String, String)>, k: String, v: String| {
        // Match JS object semantics: assigning an existing key overwrites in
        // place rather than appending a duplicate.
        if let Some(slot) = vals.iter_mut().find(|(ek, _)| *ek == k) {
            slot.1 = v;
        } else {
            vals.push((k, v));
        }
    };

    if let Some(v) = conf.get("dns_domain")
        && truthy(v)
        && let Some(s) = value_to_string(v)
    {
        push(&mut vals, "dns_domain".into(), s);
    }

    let etherstubs = arr(conf, "etherstubs");
    if !etherstubs.is_empty()
        && let Some(joined) = join_array(&Value::Array(etherstubs.to_vec()))
    {
        push(&mut vals, "etherstub".into(), joined);
    }

    if let Some(v) = conf.get("hostname")
        && truthy(v)
        && let Some(s) = value_to_string(v)
    {
        push(&mut vals, "hostname".into(), s);
    }

    let admin_tag = match conf.get("admin_tag") {
        Some(v) if truthy(v) => value_to_string(v).unwrap_or_else(|| "admin".into()),
        _ => "admin".into(),
    };
    push(&mut vals, "admin_tag".into(), admin_tag.clone());

    for tag in arr(conf, "nictags") {
        let name = match str_field(tag, "name") {
            Some(n) => n,
            None => continue,
        };
        // Skip the 'admin' nictag when admin_tag has been overridden.
        if name == "admin" && admin_tag != name {
            continue;
        }
        if let Some(v) = tag.get("mac")
            && truthy(v)
            && let Some(s) = value_to_string(v)
        {
            push(&mut vals, format!("{name}_nic"), s);
        }
        if let Some(v) = tag.get("mtu")
            && truthy(v)
            && let Some(s) = value_to_string(v)
        {
            push(&mut vals, format!("{name}_mtu"), s);
        }
    }

    let resolvers = arr(conf, "resolvers");
    if !resolvers.is_empty()
        && let Some(joined) = join_array(&Value::Array(resolvers.to_vec()))
    {
        push(&mut vals, "dns_resolvers".into(), joined);
    }

    for nic in arr(conf, "vnics") {
        let tag = match str_field(nic, "nic_tag") {
            Some(t) if !t.is_empty() => t,
            _ => continue,
        };
        if tag == "admin" && admin_tag != tag {
            continue;
        }

        let entry = tag_nums.entry(tag.clone()).or_insert(0);
        let cur = *entry;
        let iface = if (tag != "admin" && tag != admin_tag && tag != "external") || cur != 0
        {
            format!("{tag}{cur}")
        } else {
            tag.clone()
        };
        *entry += 1;

        if let Some(v) = nic.get("ip")
            && truthy(v)
            && let Some(s) = value_to_string(v)
        {
            let key = format!("{iface}_ip");
            push(&mut vals, key.clone(), s);
            ip_keys.push(key);
        }
        if let Some(v) = nic.get("gateway")
            && truthy(v)
            && let Some(s) = value_to_string(v)
        {
            push(&mut vals, format!("{iface}_gateway"), s);
        }
        if let Some(v) = nic.get("mac")
            && truthy(v)
            && let Some(s) = value_to_string(v)
        {
            push(&mut vals, format!("{iface}_mac"), s);
        }
        if let Some(v) = nic.get("mtu")
            && truthy(v)
            && let Some(s) = value_to_string(v)
        {
            push(&mut vals, format!("{iface}_mtu"), s);
        }
        if let Some(v) = nic.get("netmask")
            && truthy(v)
            && let Some(s) = value_to_string(v)
        {
            push(&mut vals, format!("{iface}_netmask"), s);
        }
        if let Some(v) = nic.get("overlay_nic_tags_provided")
            && truthy(v)
            && let Some(joined) = join_array(v)
        {
            push(
                &mut vals,
                format!("{iface}_overlay_nic_tags_provided"),
                joined,
            );
        }
        if let Some(v) = nic.get("vlan_id")
            && truthy(v)
            && let Some(s) = value_to_string(v)
        {
            push(&mut vals, format!("{iface}_vlan_id"), s);
        }
    }

    if !ip_keys.is_empty() {
        push(&mut vals, "bootfile_ip_keys".into(), ip_keys.join(","));
    }

    for aggr in arr(conf, "aggregations") {
        let name = match str_field(aggr, "name") {
            Some(n) if !n.is_empty() => n,
            _ => continue,
        };
        if let Some(macs) = aggr.get("macs").and_then(|m| m.as_array())
            && !macs.is_empty()
            && let Some(joined) = join_array(&Value::Array(macs.clone()))
        {
            push(&mut vals, format!("{name}_aggr"), joined);
        }
        if let Some(v) = aggr.get("lacp_mode")
            && truthy(v)
            && let Some(s) = value_to_string(v)
        {
            push(&mut vals, format!("{name}_lacp_mode"), s);
        }
        if let Some(tags) = aggr.get("nic_tags_provided").and_then(|t| t.as_array()) {
            for t in tags {
                if let Some(ts) = value_to_string(t) {
                    push(&mut vals, format!("{ts}_nic"), name.clone());
                }
            }
        }
    }

    vals
}

/// Routes object as ordered (dst, gw) string pairs.
pub fn routes(conf: &Map<String, Value>) -> Vec<(String, String)> {
    match conf.get("routes") {
        Some(Value::Object(m)) => m
            .iter()
            .map(|(k, v)| (k.clone(), value_to_string(v).unwrap_or_default()))
            .collect(),
        _ => Vec::new(),
    }
}

/// The nictag_rules value (already defaulted to {} by load_config).
pub fn nictag_rules(conf: &Map<String, Value>) -> Value {
    conf.get("nictag_rules")
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        let mut m = match v {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        add_default_properties(&mut m);
        m
    }

    fn render(conf: &Map<String, Value>) -> String {
        config_values(conf)
            .into_iter()
            .map(|(k, v)| format!("{k}={v}\n"))
            .collect()
    }

    #[test]
    fn empty_config_just_admin_tag() {
        let conf = obj(json!({}));
        assert_eq!(render(&conf), "admin_tag=admin\n");
    }

    #[test]
    fn basic_vnic_and_nictag() {
        let conf = obj(json!({
            "hostname": "node0",
            "dns_domain": "example.com",
            "resolvers": ["8.8.8.8", "8.8.4.4"],
            "etherstubs": ["stub0", "stub1"],
            "nictags": [{ "name": "admin", "mac": "aa:bb:cc:dd:ee:ff", "mtu": 1500 }],
            "vnics": [{
                "nic_tag": "admin", "ip": "10.0.0.2", "netmask": "255.255.255.0",
                "gateway": "10.0.0.1", "mac": "aa:bb:cc:dd:ee:ff", "vlan_id": 0
            }]
        }));
        let out = render(&conf);
        assert_eq!(
            out,
            "dns_domain=example.com\n\
             etherstub=stub0,stub1\n\
             hostname=node0\n\
             admin_tag=admin\n\
             admin_nic=aa:bb:cc:dd:ee:ff\n\
             admin_mtu=1500\n\
             dns_resolvers=8.8.8.8,8.8.4.4\n\
             admin_ip=10.0.0.2\n\
             admin_gateway=10.0.0.1\n\
             admin_mac=aa:bb:cc:dd:ee:ff\n\
             admin_netmask=255.255.255.0\n\
             bootfile_ip_keys=admin_ip\n"
        );
    }

    #[test]
    fn vnic_iface_numbering() {
        // Non-special tag: first gets bare name only when tag==admin/external;
        // otherwise it gets the index suffix (matching node).
        let conf = obj(json!({
            "vnics": [
                { "nic_tag": "sdc_underlay", "ip": "1.1.1.1" },
                { "nic_tag": "sdc_underlay", "ip": "1.1.1.2" },
                { "nic_tag": "external", "ip": "2.2.2.1" },
                { "nic_tag": "external", "ip": "2.2.2.2" }
            ]
        }));
        let out = render(&conf);
        // Non-special tags always get the index suffix; 'external' keeps the
        // bare name only for the first instance, then external1, etc.
        assert!(out.contains("sdc_underlay0_ip=1.1.1.1\n"));
        assert!(out.contains("sdc_underlay1_ip=1.1.1.2\n"));
        assert!(out.contains("external_ip=2.2.2.1\n"));
        assert!(out.contains("external1_ip=2.2.2.2\n"));
        assert!(out.contains(
            "bootfile_ip_keys=sdc_underlay0_ip,sdc_underlay1_ip,external_ip,external1_ip\n"
        ));
    }

    #[test]
    fn admin_tag_override_skips_admin() {
        let conf = obj(json!({
            "admin_tag": "mgmt",
            "nictags": [
                { "name": "admin", "mac": "aa:aa:aa:aa:aa:aa" },
                { "name": "mgmt", "mac": "bb:bb:bb:bb:bb:bb" }
            ],
            "vnics": [
                { "nic_tag": "admin", "ip": "10.0.0.2" },
                { "nic_tag": "mgmt", "ip": "10.1.0.2" }
            ]
        }));
        let out = render(&conf);
        assert!(out.contains("admin_tag=mgmt\n"));
        assert!(!out.contains("admin_nic="));
        assert!(out.contains("mgmt_nic=bb:bb:bb:bb:bb:bb\n"));
        // admin vnic skipped; mgmt is the admin_tag so it keeps the bare name.
        assert!(!out.contains("admin_ip="));
        assert!(out.contains("mgmt_ip=10.1.0.2\n"));
    }

    #[test]
    fn aggregations() {
        let conf = obj(json!({
            "aggregations": [{
                "name": "aggr0",
                "macs": ["aa:aa:aa:aa:aa:aa", "bb:bb:bb:bb:bb:bb"],
                "lacp_mode": "active",
                "nic_tags_provided": ["admin", "external"]
            }]
        }));
        let out = render(&conf);
        assert!(out.contains("aggr0_aggr=aa:aa:aa:aa:aa:aa,bb:bb:bb:bb:bb:bb\n"));
        assert!(out.contains("aggr0_lacp_mode=active\n"));
        assert!(out.contains("admin_nic=aggr0\n"));
        assert!(out.contains("external_nic=aggr0\n"));
    }

    #[test]
    fn overlay_nic_tags_joined() {
        let conf = obj(json!({
            "vnics": [{
                "nic_tag": "sdc_overlay",
                "overlay_nic_tags_provided": ["t1", "t2"]
            }]
        }));
        let out = render(&conf);
        assert!(out.contains("sdc_overlay0_overlay_nic_tags_provided=t1,t2\n"));
    }

    #[test]
    fn routes_pairs() {
        let conf = obj(json!({ "routes": { "10.0.0.0/8": "10.99.99.7" } }));
        assert_eq!(
            routes(&conf),
            vec![("10.0.0.0/8".to_string(), "10.99.99.7".to_string())]
        );
    }
}
